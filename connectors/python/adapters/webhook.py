import hmac
import json
import os
import time
import urllib.parse
import uuid
from typing import Any, Dict

from common import (
    DEFAULT_MAX_FETCH_BYTES,
    ExternalConnectorApp,
    classify_delivery_http_error,
    json_env,
    route_decode,
    route_encode,
    safe_public_http_request,
)


class WebhookConnector(ExternalConnectorApp):
    platform = "webhook"
    attachments_in = False
    attachments_out = False

    def __init__(self) -> None:
        super().__init__()
        self._routes = json_env("WEBHOOK_ROUTES_JSON", {})
        self._session_mode = (
            os.environ.get("WEBHOOK_SESSION_MODE", "per_delivery").strip().lower()
        )

    def start_background(self) -> None:
        if self._routes or self.enable_test_api or self.capture_path:
            self.mark_ready()
        else:
            self.mark_degraded("WEBHOOK_ROUTES_JSON is not configured")

    def _render_prompt(
        self, template: str, route_name: str, event_type: str, payload: Dict[str, Any]
    ) -> str:
        if not template:
            return json.dumps(payload, indent=2, sort_keys=True)
        safe_payload = {
            "route_name": route_name,
            "event_type": event_type,
            "payload_json": json.dumps(payload, indent=2, sort_keys=True),
        }
        try:
            return template.format(**safe_payload)
        except Exception:
            return f"{template}\n\n{safe_payload['payload_json']}"

    def _validate_route_signature(
        self, route: Dict[str, Any], headers: Any, body: bytes
    ) -> bool:
        secret = str(route.get("secret") or "").strip()
        if not secret or secret == "INSECURE_NO_AUTH":
            return True
        header_name = route.get("signature_header") or "X-Hub-Signature-256"
        signature = headers.get(header_name, "")
        if not signature:
            return False
        if signature.startswith("sha256="):
            expected = (
                "sha256=" + hmac.new(secret.encode("utf-8"), body, "sha256").hexdigest()
            )
            return hmac.compare_digest(expected, signature)
        expected = hmac.new(secret.encode("utf-8"), body, "sha256").hexdigest()
        return hmac.compare_digest(expected, signature)

    def handle_platform_request(
        self, method: str, path: str, query: str, headers: Any, body: bytes
    ):
        if method != "POST" or not path.startswith("/webhooks/"):
            return None
        route_name = path.rsplit("/", 1)[-1]
        route = self._routes.get(route_name)
        if not isinstance(route, dict):
            return (
                404,
                "application/json",
                json.dumps({"error": "unknown route"}).encode("utf-8"),
            )
        if not self._validate_route_signature(route, headers, body):
            return (
                401,
                "application/json",
                json.dumps({"error": "invalid signature"}).encode("utf-8"),
            )
        try:
            payload = json.loads(body.decode("utf-8"))
        except json.JSONDecodeError:
            payload = dict(
                urllib.parse.parse_qsl(body.decode("utf-8"), keep_blank_values=True)
            )
        event_header = route.get("event_header") or "X-GitHub-Event"
        event_type = (
            headers.get(event_header, "") or payload.get("event_type") or "unknown"
        )
        allowed_events = route.get("allowed_events") or []
        if allowed_events and event_type not in allowed_events:
            return (
                200,
                "application/json",
                json.dumps({"status": "ignored"}).encode("utf-8"),
            )
        delivery_id = (
            headers.get("X-GitHub-Delivery")
            or headers.get("X-Request-Id")
            or headers.get("X-Request-ID")
            or f"delivery-{uuid.uuid4().hex[:12]}"
        )
        prompt = self._render_prompt(
            str(route.get("prompt_template") or ""),
            route_name,
            str(event_type),
            payload if isinstance(payload, dict) else {"payload": payload},
        )
        callback_url = route.get("callback_url")
        callback_url_field = route.get("callback_url_field")
        if not callback_url and callback_url_field and isinstance(payload, dict):
            callback_url = payload.get(callback_url_field)
        thread_path = [route_name]
        if route.get("session_mode", self._session_mode) == "per_delivery":
            thread_path.append(str(delivery_id))
        self._daemon.submit_event(
            {
                "event_id": f"webhook-{delivery_id}",
                "fingerprint": str(delivery_id),
                "occurred_at_ms": int(time.time() * 1000),
                "actor_id": f"webhook:{route_name}",
                "source_kind": "webhook",
                "intent": "domain_event",
                "thread_path": thread_path,
                "content": prompt,
                "reply_route": route_encode(
                    {
                        "callback_url": callback_url,
                        "allowed_callback_hosts": route.get("allowed_callback_hosts")
                        or [],
                    }
                    if callback_url
                    else {}
                ),
                "metadata": {"route": route_name, "event_type": event_type},
            }
        )
        return (
            200,
            "application/json",
            json.dumps({"status": "accepted", "delivery_id": delivery_id}).encode(
                "utf-8"
            ),
        )

    def deliver(self, payload: Dict[str, Any]) -> None:
        route = route_decode(payload.get("reply_route"))
        callback_url = route.get("callback_url")
        if not callback_url:
            return
        status, _headers, response_body = safe_public_http_request(
            "POST",
            callback_url,
            body=json.dumps(
                {
                    "content": payload.get("content", ""),
                    "metadata": payload.get("metadata", {}),
                }
            ).encode("utf-8"),
            headers={"Content-Type": "application/json"},
            timeout=15.0,
            max_bytes=DEFAULT_MAX_FETCH_BYTES,
            allowed_hosts=route.get("allowed_callback_hosts"),
        )
        classify_delivery_http_error(
            status,
            f"webhook callback returned HTTP {status}: {response_body.decode('utf-8', errors='replace')}",
            retryable_statuses={429},
        )
