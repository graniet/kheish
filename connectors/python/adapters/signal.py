import base64
import json
import os
import threading
import time
import urllib.parse
import urllib.request
import uuid
from typing import Any, Dict, Optional

from common import (
    ExternalConnectorApp,
    LOG,
    RetryableDeliveryError,
    TerminalDeliveryError,
    classify_delivery_http_error,
    http_request,
    inline_asset_from_bytes,
    iter_delivery_assets,
    json_request,
    route_decode,
    route_encode,
)


class SignalConnector(ExternalConnectorApp):
    platform = "signal"
    experimental = True

    def __init__(self) -> None:
        super().__init__()
        self._http_url = os.environ.get("SIGNAL_HTTP_URL", "").rstrip("/")
        self._account = os.environ.get("SIGNAL_ACCOUNT", "").strip()

    def start_background(self) -> None:
        if not self._http_url or not self._account:
            super().start_background()
            return
        try:
            status, _headers, _body = http_request(
                "GET",
                f"{self._http_url}/api/v1/check",
                timeout=10.0,
            )
            if status >= 400:
                self.mark_degraded(f"signal health check returned HTTP {status}")
                return
            self.mark_ready()
        except Exception as exc:
            self.mark_degraded(str(exc))
            return
        threading.Thread(target=self._sse_loop, daemon=True).start()

    def _rpc(self, method: str, params: Dict[str, Any]) -> Any:
        status, decoded = json_request(
            "POST",
            f"{self._http_url}/api/v1/rpc",
            {
                "jsonrpc": "2.0",
                "id": f"{method}-{uuid.uuid4().hex[:8]}",
                "method": method,
                "params": params,
            },
            timeout=60.0,
        )
        classify_delivery_http_error(
            status,
            f"signal RPC {method} returned HTTP {status}",
            retryable_statuses={429},
        )
        if "error" in decoded:
            raise RetryableDeliveryError(
                f"signal RPC {method} failed: {decoded['error']}"
            )
        return decoded.get("result")

    def _sse_loop(self) -> None:
        url = f"{self._http_url}/api/v1/events?account={urllib.parse.quote(self._account, safe='')}"
        while True:
            try:
                request = urllib.request.Request(
                    url, headers={"Accept": "text/event-stream"}
                )
                with urllib.request.urlopen(request, timeout=60) as response:
                    buffer = []
                    for raw_line in response:
                        line = raw_line.decode("utf-8", errors="replace").strip()
                        if not line:
                            if buffer:
                                payload = "".join(buffer)
                                buffer = []
                                self._handle_signal_event(payload)
                            continue
                        if line.startswith(":"):
                            continue
                        if line.startswith("data:"):
                            buffer.append(line[5:].lstrip())
            except Exception as exc:
                LOG.exception("signal SSE failed")
                self.mark_degraded(str(exc))
                time.sleep(2)

    def _attachment_asset(self, attachment_id: str) -> Optional[Dict[str, Any]]:
        result = self._rpc(
            "getAttachment", {"account": self._account, "id": attachment_id}
        )
        data = result.get("data") if isinstance(result, dict) else result
        if not data:
            return None
        raw = base64.b64decode(data)
        return inline_asset_from_bytes(f"signal-{attachment_id}", raw)

    def _handle_signal_event(self, data: str) -> None:
        envelope = json.loads(data)
        envelope = envelope.get("envelope", envelope)
        data_message = envelope.get("dataMessage") or (
            envelope.get("editMessage") or {}
        ).get("dataMessage")
        if not data_message:
            return
        sender = (
            envelope.get("sourceNumber")
            or envelope.get("sourceUuid")
            or envelope.get("source")
        )
        if not sender or sender == self._account:
            return
        group_info = data_message.get("groupInfo") or {}
        group_id = group_info.get("groupId")
        chat_id = f"group:{group_id}" if group_id else sender
        attachments = []
        for attachment in data_message.get("attachments") or []:
            attachment_id = attachment.get("id")
            if not attachment_id:
                continue
            try:
                asset = self._attachment_asset(str(attachment_id))
                if asset:
                    attachments.append(asset)
            except Exception:
                LOG.exception("failed to fetch Signal attachment")
        self._daemon.submit_event(
            {
                "event_id": f"signal-{envelope.get('timestamp') or uuid.uuid4().hex}",
                "fingerprint": str(envelope.get("timestamp") or chat_id),
                "occurred_at_ms": envelope.get("timestamp") or int(time.time() * 1000),
                "actor_id": sender,
                "source_kind": "signal",
                "intent": "message",
                "thread_path": [chat_id],
                "content": data_message.get("message") or "",
                "attachments": attachments,
                "reply_route": route_encode({"chat_id": chat_id}),
                "metadata": {"group_id": group_id} if group_id else {},
            }
        )

    def deliver(self, payload: Dict[str, Any]) -> None:
        route = route_decode(payload.get("reply_route"))
        chat_id = route.get("chat_id")
        if not chat_id:
            raise TerminalDeliveryError("signal reply route is missing chat_id")
        params: Dict[str, Any] = {
            "account": self._account,
            "message": str(payload.get("content") or ""),
        }
        if chat_id.startswith("group:"):
            params["groupId"] = chat_id[6:]
        else:
            params["recipient"] = [chat_id]
        temp_paths = []
        try:
            for asset in iter_delivery_assets(payload):
                temp_path = self._daemon.download_asset_to_tempfile(asset)
                temp_paths.append(temp_path)
            if temp_paths:
                params["attachments"] = temp_paths
            self._rpc("send", params)
        finally:
            for path in temp_paths:
                try:
                    os.unlink(path)
                except OSError:
                    pass
