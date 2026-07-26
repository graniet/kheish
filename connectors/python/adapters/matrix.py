import json
import os
import threading
import time
import urllib.parse
import uuid
from typing import Any, Dict, Optional

from common import (
    ExternalConnectorApp,
    LOG,
    TerminalDeliveryError,
    classify_delivery_http_error,
    guess_media_type,
    http_request,
    inline_asset_from_bytes,
    iter_delivery_assets,
    json_request,
    route_decode,
    route_encode,
)


class MatrixConnector(ExternalConnectorApp):
    platform = "matrix"
    threads = True

    def __init__(self) -> None:
        super().__init__()
        self._homeserver = os.environ.get("MATRIX_HOMESERVER", "").rstrip("/")
        self._token = os.environ.get("MATRIX_ACCESS_TOKEN", "").strip()
        self._user_id = os.environ.get("MATRIX_USER_ID", "").strip()
        self._since = None

    def start_background(self) -> None:
        if not self._homeserver or not self._token:
            super().start_background()
            return
        thread = threading.Thread(target=self._run_sync_loop, daemon=True)
        thread.start()

    def _matrix_headers(self, content_type: Optional[str] = None) -> Dict[str, str]:
        headers = {"Authorization": f"Bearer {self._token}"}
        if content_type:
            headers["Content-Type"] = content_type
        return headers

    def _matrix_json(
        self,
        method: str,
        path: str,
        payload: Optional[Dict[str, Any]] = None,
        params: Optional[Dict[str, str]] = None,
    ) -> Dict[str, Any]:
        url = f"{self._homeserver}{path}"
        if params:
            url = f"{url}?{urllib.parse.urlencode(params)}"
        status, decoded = json_request(
            method, url, payload, headers=self._matrix_headers()
        )
        if status >= 400:
            raise RuntimeError(f"matrix API returned HTTP {status}: {decoded}")
        return decoded

    def _run_sync_loop(self) -> None:
        try:
            whoami = self._matrix_json("GET", "/_matrix/client/v3/account/whoami")
            if not self._user_id:
                self._user_id = str(whoami.get("user_id") or "")
            self.mark_ready()
        except Exception as exc:
            self.mark_degraded(str(exc))
            return
        while True:
            try:
                response = self._matrix_json(
                    "GET",
                    "/_matrix/client/v3/sync",
                    params={
                        "timeout": "30000",
                        **({"since": self._since} if self._since else {}),
                    },
                )
                self._since = response.get("next_batch") or self._since
                joined = (response.get("rooms") or {}).get("join") or {}
                for room_id, room_payload in joined.items():
                    events = ((room_payload.get("timeline") or {}).get("events")) or []
                    for event in events:
                        self._handle_event(room_id, event)
            except Exception as exc:
                LOG.exception("matrix sync failed")
                self.mark_degraded(str(exc))
                time.sleep(2)

    def _download_mxc(self, mxc_url: str) -> bytes:
        if not mxc_url.startswith("mxc://"):
            raise RuntimeError(f"invalid Matrix media URL {mxc_url}")
        server_name, media_id = mxc_url[6:].split("/", 1)
        status, _headers, body = http_request(
            "GET",
            f"{self._homeserver}/_matrix/media/v3/download/{urllib.parse.quote(server_name)}/{urllib.parse.quote(media_id)}",
            headers=self._matrix_headers(),
            timeout=60.0,
        )
        if status >= 400:
            raise RuntimeError(f"matrix media download returned HTTP {status}")
        return body

    def _handle_event(self, room_id: str, event: Dict[str, Any]) -> None:
        if event.get("type") != "m.room.message":
            return
        if self._user_id and event.get("sender") == self._user_id:
            return
        content = event.get("content") or {}
        if content.get("msgtype") == "m.notice":
            return
        body = str(content.get("body") or "").strip()
        relates_to = content.get("m.relates_to") or {}
        thread_id = None
        if relates_to.get("rel_type") == "m.thread":
            thread_id = str(relates_to.get("event_id") or "")
        attachments = []
        media_url = content.get("url")
        if media_url:
            try:
                media_bytes = self._download_mxc(str(media_url))
                attachments.append(
                    inline_asset_from_bytes(
                        body or "attachment.bin",
                        media_bytes,
                        (((content.get("info") or {}).get("mimetype")) or None),
                    )
                )
            except Exception:
                LOG.exception("failed to import Matrix attachment")
        if not body and not attachments:
            return
        self._daemon.submit_event(
            {
                "event_id": f"matrix-{event.get('event_id')}",
                "fingerprint": str(event.get("event_id")),
                "occurred_at_ms": int(time.time() * 1000),
                "actor_id": event.get("sender"),
                "source_kind": "matrix",
                "intent": "message",
                "relation": (
                    {
                        "kind": "thread",
                        "target_event_id": str(relates_to.get("event_id")),
                    }
                    if relates_to.get("rel_type") == "m.thread"
                    and relates_to.get("event_id")
                    else (
                        {
                            "kind": "reply_to",
                            "target_event_id": str(
                                (relates_to.get("m.in_reply_to") or {}).get("event_id")
                            ),
                        }
                        if isinstance(relates_to.get("m.in_reply_to"), dict)
                        and (relates_to.get("m.in_reply_to") or {}).get("event_id")
                        else None
                    )
                ),
                "thread_path": [room_id] + ([thread_id] if thread_id else []),
                "content": body,
                "attachments": attachments,
                "reply_route": route_encode(
                    {
                        "room_id": room_id,
                        "thread_id": thread_id,
                        "event_id": event.get("event_id"),
                    }
                ),
                "metadata": {"room_id": room_id},
            }
        )

    def _upload_matrix_media(self, asset: Dict[str, Any]) -> str:
        body = self._daemon.download_asset(asset)
        file_name = asset.get("file_name") or "attachment.bin"
        status, _headers, response_body = http_request(
            "POST",
            f"{self._homeserver}/_matrix/media/v3/upload?filename={urllib.parse.quote(file_name)}",
            body=body,
            headers=self._matrix_headers(
                guess_media_type(file_name, asset.get("media_type"))
            ),
            timeout=120.0,
        )
        if status >= 400:
            classify_delivery_http_error(
                status,
                f"matrix upload failed with HTTP {status}",
                retryable_statuses={429},
            )
        decoded = json.loads(response_body.decode("utf-8"))
        return decoded["content_uri"]

    def _message_relates(
        self, reply_to: Optional[str], thread_id: Optional[str]
    ) -> Optional[Dict[str, Any]]:
        relates_to: Dict[str, Any] = {}
        if reply_to:
            relates_to["m.in_reply_to"] = {"event_id": reply_to}
        if thread_id:
            relates_to["rel_type"] = "m.thread"
            relates_to["event_id"] = thread_id
            relates_to["is_falling_back"] = True
        return relates_to or None

    def _send_matrix_message(self, room_id: str, content: Dict[str, Any]) -> None:
        transaction_id = uuid.uuid4().hex
        status, _decoded = json_request(
            "PUT",
            f"{self._homeserver}/_matrix/client/v3/rooms/{urllib.parse.quote(room_id, safe='')}/send/m.room.message/{transaction_id}",
            content,
            headers=self._matrix_headers(),
            timeout=60.0,
        )
        classify_delivery_http_error(
            status,
            f"matrix send failed with HTTP {status}",
            retryable_statuses={429},
        )

    def deliver(self, payload: Dict[str, Any]) -> None:
        route = route_decode(payload.get("reply_route"))
        room_id = route.get("room_id")
        if not room_id:
            raise TerminalDeliveryError("matrix reply route is missing room_id")
        thread_id = route.get("thread_id")
        reply_to = route.get("event_id")
        text = str(payload.get("content") or "").strip()
        if text:
            content = {"msgtype": "m.text", "body": text}
            relates_to = self._message_relates(reply_to, thread_id)
            if relates_to:
                content["m.relates_to"] = relates_to
            self._send_matrix_message(room_id, content)
        for asset in iter_delivery_assets(payload):
            mxc = self._upload_matrix_media(asset)
            media_type = guess_media_type(
                asset.get("file_name") or "", asset.get("media_type")
            )
            if media_type.startswith("image/"):
                msgtype = "m.image"
            elif media_type.startswith("audio/"):
                msgtype = "m.audio"
            elif media_type.startswith("video/"):
                msgtype = "m.video"
            else:
                msgtype = "m.file"
            content = {
                "msgtype": msgtype,
                "body": asset.get("file_name") or "attachment",
                "url": mxc,
                "info": {"mimetype": media_type},
            }
            relates_to = self._message_relates(reply_to, thread_id)
            if relates_to:
                content["m.relates_to"] = relates_to
            self._send_matrix_message(room_id, content)
