import os
import threading
import time
import uuid
from typing import Any, Dict, Optional

from common import (
    DEFAULT_MAX_FETCH_BYTES,
    ExternalConnectorApp,
    LOG,
    RetryableDeliveryError,
    TerminalDeliveryError,
    guess_media_type,
    inline_asset_from_bytes,
    iter_delivery_assets,
    json_request,
    route_decode,
    route_encode,
    safe_public_http_request,
)


class WhatsAppConnector(ExternalConnectorApp):
    platform = "whatsapp"
    experimental = True

    def __init__(self) -> None:
        super().__init__()
        self._bridge_url = os.environ.get("WHATSAPP_BRIDGE_URL", "").rstrip("/")
        self._allowed_media_hosts = {
            value.strip().lower()
            for value in os.environ.get("WHATSAPP_ALLOWED_MEDIA_HOSTS", "").split(",")
            if value.strip()
        }
        self._media_max_bytes = int(
            os.environ.get("WHATSAPP_MEDIA_MAX_BYTES", str(DEFAULT_MAX_FETCH_BYTES))
        )

    def start_background(self) -> None:
        if not self._bridge_url:
            super().start_background()
            return
        try:
            status, decoded = json_request(
                "GET", f"{self._bridge_url}/health", timeout=10.0
            )
            if status >= 400:
                self.mark_degraded(f"whatsapp bridge health returned HTTP {status}")
                return
            self.mark_ready(decoded.get("status"))
        except Exception as exc:
            self.mark_degraded(str(exc))
            return
        threading.Thread(target=self._poll_loop, daemon=True).start()

    def _bridge_json(
        self, method: str, path: str, payload: Optional[Dict[str, Any]] = None
    ) -> Dict[str, Any]:
        status, decoded = json_request(
            method, f"{self._bridge_url}{path}", payload, timeout=120.0
        )
        if status >= 500:
            raise RetryableDeliveryError(f"whatsapp bridge returned HTTP {status}")
        if status >= 400:
            raise TerminalDeliveryError(
                f"whatsapp bridge returned HTTP {status}: {decoded}"
            )
        return decoded

    def _poll_loop(self) -> None:
        while True:
            try:
                messages = self._bridge_json("GET", "/messages")
                if isinstance(messages, list):
                    for message in messages:
                        self._handle_bridge_message(message)
            except Exception as exc:
                LOG.exception("whatsapp bridge poll failed")
                self.mark_degraded(str(exc))
                time.sleep(2)
            time.sleep(1)

    def _handle_bridge_message(self, payload: Dict[str, Any]) -> None:
        chat_id = str(payload.get("chatId") or "").strip()
        if not chat_id:
            return
        attachments = []
        for index, media_url in enumerate(payload.get("mediaUrls") or []):
            try:
                if os.path.isabs(media_url):
                    raise RuntimeError(
                        "whatsapp mediaUrls may not reference local absolute paths"
                    )
                status, _headers, body = safe_public_http_request(
                    "GET",
                    media_url,
                    timeout=15.0,
                    max_bytes=self._media_max_bytes,
                    allowed_hosts=self._allowed_media_hosts,
                )
                if status >= 400:
                    continue
                file_name = f"whatsapp-{index}"
                attachments.append(
                    inline_asset_from_bytes(
                        file_name,
                        body,
                        (payload.get("mediaType") or None),
                    )
                )
            except Exception:
                LOG.exception("failed to import WhatsApp media")
        text = str(payload.get("body") or "").strip()
        self._daemon.submit_event(
            {
                "event_id": f"whatsapp-{payload.get('messageId') or uuid.uuid4().hex}",
                "fingerprint": str(payload.get("messageId") or chat_id),
                "occurred_at_ms": int(time.time() * 1000),
                "actor_id": payload.get("senderId") or chat_id,
                "source_kind": "whatsapp",
                "intent": "message",
                "thread_path": [chat_id],
                "content": text,
                "attachments": attachments,
                "reply_route": route_encode(
                    {
                        "chat_id": chat_id,
                        "reply_to": payload.get("messageId"),
                    }
                ),
                "metadata": {"is_group": bool(payload.get("isGroup"))},
            }
        )

    def deliver(self, payload: Dict[str, Any]) -> None:
        route = route_decode(payload.get("reply_route"))
        chat_id = route.get("chat_id")
        if not chat_id:
            raise TerminalDeliveryError("whatsapp reply route is missing chat_id")
        text = str(payload.get("content") or "").strip()
        reply_to = route.get("reply_to")
        if text:
            request = {"chatId": chat_id, "message": text}
            if reply_to:
                request["replyTo"] = reply_to
            self._bridge_json("POST", "/send", request)
        for asset in iter_delivery_assets(payload):
            temp_path = self._daemon.download_asset_to_tempfile(asset)
            try:
                media_type = guess_media_type(
                    asset.get("file_name") or "", asset.get("media_type")
                )
                if media_type.startswith("image/"):
                    kind = "image"
                elif media_type.startswith("video/"):
                    kind = "video"
                elif media_type.startswith("audio/"):
                    kind = "audio"
                else:
                    kind = "document"
                self._bridge_json(
                    "POST",
                    "/send-media",
                    {
                        "chatId": chat_id,
                        "filePath": temp_path,
                        "mediaType": kind,
                        "fileName": asset.get("file_name"),
                    },
                )
            finally:
                try:
                    os.unlink(temp_path)
                except OSError:
                    pass
