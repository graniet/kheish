import base64
import hmac
import os
import time
import urllib.parse
import uuid
from typing import Any, Dict

from common import (
    DEFAULT_MAX_FETCH_BYTES,
    ExternalConnectorApp,
    LOG,
    TerminalDeliveryError,
    bool_env,
    classify_delivery_http_error,
    http_request,
    inline_asset_from_bytes,
    iter_delivery_assets,
    route_decode,
    route_encode,
    safe_public_http_request,
)


class SmsConnector(ExternalConnectorApp):
    platform = "sms"
    attachments_out = False

    def __init__(self) -> None:
        super().__init__()
        self._sid = os.environ.get("TWILIO_ACCOUNT_SID", "").strip()
        self._auth_token = os.environ.get("TWILIO_AUTH_TOKEN", "").strip()
        self._from_number = os.environ.get("TWILIO_PHONE_NUMBER", "").strip()
        self._webhook_url = os.environ.get("SMS_WEBHOOK_URL", "").strip()
        self._allowed_media_hosts = {
            value.strip().lower()
            for value in os.environ.get(
                "TWILIO_ALLOWED_MEDIA_HOSTS",
                "twilio.com,twiliocdn.com",
            ).split(",")
            if value.strip()
        }

    def start_background(self) -> None:
        if all([self._sid, self._auth_token, self._from_number]):
            self.mark_ready()
        else:
            super().start_background()

    def _twilio_auth_header(self) -> str:
        raw = f"{self._sid}:{self._auth_token}".encode("utf-8")
        return "Basic " + base64.b64encode(raw).decode("ascii")

    def _validate_signature(self, post_params: Dict[str, str], signature: str) -> bool:
        if not self._webhook_url:
            return True
        data_to_sign = self._webhook_url
        for key in sorted(post_params):
            data_to_sign += key + post_params[key]
        digest = hmac.new(
            self._auth_token.encode("utf-8"),
            data_to_sign.encode("utf-8"),
            "sha1",
        ).digest()
        expected = base64.b64encode(digest).decode("utf-8")
        return hmac.compare_digest(expected, signature)

    def handle_platform_request(
        self, method: str, path: str, query: str, headers: Any, body: bytes
    ):
        if method == "POST" and path == "/twilio/inbound":
            form = urllib.parse.parse_qs(body.decode("utf-8"), keep_blank_values=True)
            flat = {key: values[0] for key, values in form.items() if values}
            if self._webhook_url and not bool_env("SMS_INSECURE_NO_SIGNATURE", False):
                signature = headers.get("X-Twilio-Signature", "")
                if not signature or not self._validate_signature(flat, signature):
                    return (
                        403,
                        "application/xml",
                        b'<?xml version="1.0" encoding="UTF-8"?><Response></Response>',
                    )
            from_number = flat.get("From", "").strip()
            message_sid = flat.get("MessageSid", "").strip()
            text = flat.get("Body", "").strip()
            attachments = []
            media_count = int(flat.get("NumMedia", "0") or "0")
            for index in range(media_count):
                media_url = flat.get(f"MediaUrl{index}", "").strip()
                media_type = flat.get(f"MediaContentType{index}", "").strip() or None
                if not media_url:
                    continue
                try:
                    status, _resp_headers, media_body = safe_public_http_request(
                        "GET",
                        media_url,
                        headers={"Authorization": self._twilio_auth_header()},
                        timeout=15.0,
                        max_bytes=DEFAULT_MAX_FETCH_BYTES,
                        allowed_hosts=self._allowed_media_hosts,
                    )
                    if status < 400:
                        attachments.append(
                            inline_asset_from_bytes(
                                f"twilio-{index}",
                                media_body,
                                media_type,
                            )
                        )
                except Exception:
                    LOG.exception("failed to import Twilio media")
            if from_number and (text or attachments):
                self._daemon.submit_event(
                    {
                        "event_id": f"sms-{message_sid or uuid.uuid4().hex}",
                        "fingerprint": message_sid or from_number,
                        "occurred_at_ms": int(time.time() * 1000),
                        "actor_id": from_number,
                        "source_kind": "sms",
                        "intent": "message",
                        "thread_path": [from_number],
                        "content": text,
                        "attachments": attachments,
                        "reply_route": route_encode({"to": from_number}),
                        "metadata": {"from": from_number},
                    }
                )
            return (
                200,
                "application/xml",
                b'<?xml version="1.0" encoding="UTF-8"?><Response></Response>',
            )
        return None

    def deliver(self, payload: Dict[str, Any]) -> None:
        if iter_delivery_assets(payload):
            raise TerminalDeliveryError(
                "Twilio SMS delivery does not support daemon attachments"
            )
        route = route_decode(payload.get("reply_route"))
        to_number = route.get("to")
        if not to_number:
            raise TerminalDeliveryError("sms reply route is missing destination number")
        form = urllib.parse.urlencode(
            {
                "From": self._from_number,
                "To": to_number,
                "Body": str(payload.get("content") or ""),
            }
        ).encode("utf-8")
        status, _headers, response_body = http_request(
            "POST",
            f"https://api.twilio.com/2010-04-01/Accounts/{self._sid}/Messages.json",
            body=form,
            headers={
                "Authorization": self._twilio_auth_header(),
                "Content-Type": "application/x-www-form-urlencoded",
            },
            timeout=30.0,
        )
        classify_delivery_http_error(
            status,
            f"Twilio returned HTTP {status}: {response_body.decode('utf-8', errors='replace')}",
            retryable_statuses={429},
        )
