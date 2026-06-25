import asyncio
import base64
import email
import hmac
import io
import json
import logging
import os
import re
import smtplib
import ssl
import threading
import time
import urllib.parse
import urllib.request
import uuid
from datetime import datetime, timezone
from email import encoders
from email.header import decode_header
from email.mime.base import MIMEBase
from email.mime.multipart import MIMEMultipart
from email.mime.text import MIMEText
from email.utils import parseaddr
from pathlib import Path
from typing import Any, Dict, Optional

from common import (
    DEFAULT_MAX_FETCH_BYTES,
    ExternalConnectorApp,
    LOG,
    RetryableDeliveryError,
    TerminalDeliveryError,
    append_jsonl,
    bool_env,
    guess_media_type,
    http_request,
    inline_asset_from_bytes,
    iter_delivery_assets,
    json_env,
    json_request,
    normalize_path,
    route_decode,
    route_encode,
    safe_public_http_request,
)


def classify_delivery_http_error(
    status: int,
    detail: str,
    *,
    retryable_statuses: Optional[set[int]] = None,
) -> None:
    retryable_statuses = retryable_statuses or set()
    if status >= 500 or status in retryable_statuses:
        raise RetryableDeliveryError(detail)
    if status >= 400:
        raise TerminalDeliveryError(detail)


def _strip_html(value: str) -> str:
    text = re.sub(r"<br\s*/?>", "\n", value, flags=re.IGNORECASE)
    text = re.sub(r"</p>", "\n", text, flags=re.IGNORECASE)
    text = re.sub(r"<[^>]+>", "", text)
    return text.strip()


def _decode_header_value(raw: str) -> str:
    parts = decode_header(raw)
    decoded = []
    for part, charset in parts:
        if isinstance(part, bytes):
            decoded.append(part.decode(charset or "utf-8", errors="replace"))
        else:
            decoded.append(part)
    return "".join(decoded)


def _extract_email_text(msg: email.message.Message) -> str:
    if msg.is_multipart():
        for part in msg.walk():
            content_type = part.get_content_type()
            disposition = str(part.get("Content-Disposition", ""))
            if "attachment" in disposition:
                continue
            payload = part.get_payload(decode=True) or b""
            charset = part.get_content_charset() or "utf-8"
            if content_type == "text/plain":
                return payload.decode(charset, errors="replace").strip()
            if content_type == "text/html":
                return _strip_html(payload.decode(charset, errors="replace"))
        return ""
    payload = msg.get_payload(decode=True) or b""
    charset = msg.get_content_charset() or "utf-8"
    if msg.get_content_type() == "text/html":
        return _strip_html(payload.decode(charset, errors="replace"))
    return payload.decode(charset, errors="replace").strip()


def _email_thread_anchor(msg: email.message.Message) -> str:
    references = (msg.get("References") or "").strip().split()
    if references:
        return references[-1]
    in_reply_to = (msg.get("In-Reply-To") or "").strip()
    if in_reply_to:
        return in_reply_to
    message_id = (msg.get("Message-ID") or "").strip()
    if message_id:
        return message_id
    subject = _decode_header_value(msg.get("Subject", "") or "").strip()
    return subject or "email-thread"


class DiscordConnector(ExternalConnectorApp):
    platform = "discord"
    threads = True

    def __init__(self) -> None:
        super().__init__()
        self._discord = None
        self._client = None
        self._loop = None
        self._token = os.environ.get("DISCORD_TOKEN", "").strip()

    def start_background(self) -> None:
        if not self._token:
            super().start_background()
            return
        try:
            import discord  # type: ignore
        except ImportError:
            self.mark_degraded("discord.py is not installed")
            return
        self._discord = discord
        worker = threading.Thread(target=self._run_client, daemon=True)
        worker.start()

    def _run_client(self) -> None:
        discord = self._discord
        loop = asyncio.new_event_loop()
        asyncio.set_event_loop(loop)
        self._loop = loop
        intents = discord.Intents.default()
        intents.guilds = True
        intents.messages = True
        intents.message_content = True
        intents.dm_messages = True
        client = discord.Client(intents=intents)
        self._client = client

        @client.event
        async def on_ready():
            self.mark_ready()

        @client.event
        async def on_message(message):
            await self._handle_message(message)

        try:
            loop.run_until_complete(client.start(self._token))
        except Exception as exc:
            LOG.exception("discord connector stopped")
            self.mark_degraded(str(exc))

    async def _handle_message(self, message) -> None:
        if not self._client or message.author == self._client.user:
            return
        if getattr(message.author, "bot", False):
            return
        require_mention = bool_env("DISCORD_REQUIRE_MENTION", True)
        is_dm = isinstance(message.channel, self._discord.DMChannel)
        if not is_dm and require_mention and self._client.user not in message.mentions:
            return
        content = str(message.content or "").strip()
        if self._client.user in message.mentions:
            content = re.sub(rf"<@!?{self._client.user.id}>", "", content).strip()
        input_items = []
        attachments = []
        max_bytes = int(os.environ.get("DISCORD_ATTACHMENT_MAX_BYTES", str(5 * 1024 * 1024)))
        for attachment in list(getattr(message, "attachments", []))[:8]:
            try:
                if attachment.size > max_bytes:
                    continue
                data = await attachment.read(use_cached=True)
                attachments.append(
                    inline_asset_from_bytes(
                        attachment.filename,
                        data,
                        attachment.content_type,
                    )
                )
            except Exception:
                LOG.exception("failed to import discord attachment")
        if not content and not attachments:
            return
        if isinstance(message.channel, self._discord.Thread):
            thread_path = [
                str(message.guild.id) if message.guild else "dm",
                str(message.channel.parent_id or message.channel.id),
                str(message.channel.id),
            ]
            reply_route = route_encode(
                {
                    "channel_id": str(message.channel.parent_id or message.channel.id),
                    "thread_id": str(message.channel.id),
                    "reply_to_message_id": str(message.id),
                }
            )
        elif is_dm:
            thread_path = ["dm", str(message.channel.id)]
            reply_route = route_encode(
                {
                    "channel_id": str(message.channel.id),
                    "reply_to_message_id": str(message.id),
                }
            )
        else:
            thread_path = [str(message.guild.id), str(message.channel.id)]
            reply_route = route_encode(
                {
                    "channel_id": str(message.channel.id),
                    "reply_to_message_id": str(message.id),
                }
            )
        await asyncio.to_thread(
            self._daemon.submit_event,
            {
                "event_id": f"discord-{message.id}",
                "fingerprint": str(message.id),
                "occurred_at_ms": int(message.created_at.timestamp() * 1000),
                "actor_id": str(message.author.id),
                "source_kind": "discord",
                "intent": "message",
                "relation": (
                    {
                        "kind": "reply_to",
                        "target_event_id": f"discord-{message.reference.message_id}",
                    }
                    if getattr(message, "reference", None)
                    and getattr(message.reference, "message_id", None)
                    else None
                ),
                "thread_path": thread_path,
                "content": content,
                "input_items": input_items,
                "attachments": attachments,
                "reply_route": reply_route,
                "metadata": {
                    "message_id": str(message.id),
                    "channel_id": str(message.channel.id),
                },
            },
        )

    async def _deliver_async(self, payload: Dict[str, Any]) -> None:
        if not self._client:
            raise TerminalDeliveryError("discord client is not connected")
        route = route_decode(payload.get("reply_route"))
        target_id = route.get("thread_id") or route.get("channel_id")
        if not target_id:
            raise TerminalDeliveryError("discord reply route is missing channel_id")
        channel = self._client.get_channel(int(target_id))
        if channel is None:
            channel = await self._client.fetch_channel(int(target_id))
        if channel is None:
            raise RetryableDeliveryError(f"discord channel {target_id} was not found")
        kwargs: Dict[str, Any] = {
            "content": payload.get("content") or None,
            "allowed_mentions": self._discord.AllowedMentions(
                everyone=False,
                roles=False,
                users=True,
                replied_user=True,
            ),
        }
        reply_to_message_id = route.get("reply_to_message_id")
        if reply_to_message_id:
            try:
                reply_to = await channel.fetch_message(int(reply_to_message_id))
                kwargs["reference"] = reply_to.to_reference(fail_if_not_exists=False)
            except Exception:
                LOG.exception("failed to resolve discord reply target")
        files = []
        for asset in iter_delivery_assets(payload):
            data = self._daemon.download_asset(asset)
            files.append(
                self._discord.File(
                    io.BytesIO(data),
                    filename=asset.get("file_name") or asset.get("id") or "attachment.bin",
                )
            )
        if files:
            kwargs["files"] = files
        await channel.send(**kwargs)

    def deliver(self, payload: Dict[str, Any]) -> None:
        if not self._loop:
            raise TerminalDeliveryError("discord event loop is not running")
        future = asyncio.run_coroutine_threadsafe(self._deliver_async(payload), self._loop)
        future.result(timeout=60)


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

    def _matrix_json(self, method: str, path: str, payload: Optional[Dict[str, Any]] = None, params: Optional[Dict[str, str]] = None) -> Dict[str, Any]:
        url = f"{self._homeserver}{path}"
        if params:
            url = f"{url}?{urllib.parse.urlencode(params)}"
        status, decoded = json_request(method, url, payload, headers=self._matrix_headers())
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
                joined = ((response.get("rooms") or {}).get("join") or {})
                for room_id, room_payload in joined.items():
                    events = (((room_payload.get("timeline") or {}).get("events")) or [])
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
                    if relates_to.get("rel_type") == "m.thread" and relates_to.get("event_id")
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
            headers=self._matrix_headers(guess_media_type(file_name, asset.get("media_type"))),
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

    def _message_relates(self, reply_to: Optional[str], thread_id: Optional[str]) -> Optional[Dict[str, Any]]:
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
            media_type = guess_media_type(asset.get("file_name") or "", asset.get("media_type"))
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


class EmailConnector(ExternalConnectorApp):
    platform = "email"
    threads = True

    def __init__(self) -> None:
        super().__init__()
        self._imap_host = os.environ.get("EMAIL_IMAP_HOST", "").strip()
        self._imap_port = int(os.environ.get("EMAIL_IMAP_PORT", "993"))
        self._smtp_host = os.environ.get("EMAIL_SMTP_HOST", "").strip()
        self._smtp_port = int(os.environ.get("EMAIL_SMTP_PORT", "587"))
        self._address = os.environ.get("EMAIL_ADDRESS", "").strip()
        self._password = os.environ.get("EMAIL_PASSWORD", "").strip()
        self._poll_interval = int(os.environ.get("EMAIL_POLL_INTERVAL", "15"))
        self._allowed = {
            value.strip().lower()
            for value in os.environ.get("EMAIL_ALLOWED_USERS", "").split(",")
            if value.strip()
        }
        self._attachment_max_bytes = int(
            os.environ.get("EMAIL_ATTACHMENT_MAX_BYTES", str(5 * 1024 * 1024))
        )

    def start_background(self) -> None:
        if not all([self._imap_host, self._smtp_host, self._address, self._password]):
            super().start_background()
            return
        thread = threading.Thread(target=self._poll_loop, daemon=True)
        thread.start()

    def _poll_loop(self) -> None:
        first_ok = False
        while True:
            try:
                self._poll_once()
                if not first_ok:
                    self.mark_ready()
                    first_ok = True
            except Exception as exc:
                LOG.exception("email poll failed")
                self.mark_degraded(str(exc))
            time.sleep(self._poll_interval)

    def _poll_once(self) -> None:
        import imaplib

        mail = imaplib.IMAP4_SSL(self._imap_host, self._imap_port)
        try:
            mail.login(self._address, self._password)
            mail.select("INBOX")
            status, response = mail.search(None, "UNSEEN")
            if status != "OK":
                return
            for uid in response[0].split():
                fetch_status, data = mail.fetch(uid, "(BODY.PEEK[])")
                if fetch_status != "OK":
                    continue
                raw_bytes = None
                for part in data:
                    if isinstance(part, tuple):
                        raw_bytes = part[1]
                        break
                if not raw_bytes:
                    continue
                msg = email.message_from_bytes(raw_bytes)
                sender_name, sender_addr = parseaddr(msg.get("From", ""))
                sender_addr = sender_addr.lower().strip()
                if self._allowed and sender_addr not in self._allowed:
                    continue
                subject = _decode_header_value(msg.get("Subject", "") or "")
                body = _extract_email_text(msg)
                thread_anchor = _email_thread_anchor(msg)
                attachments = []
                for part in msg.walk():
                    disposition = str(part.get("Content-Disposition", "")).lower()
                    if "attachment" not in disposition and "inline" not in disposition:
                        continue
                    payload = part.get_payload(decode=True) or b""
                    if not payload or len(payload) > self._attachment_max_bytes:
                        continue
                    file_name = _decode_header_value(part.get_filename() or "attachment.bin")
                    attachments.append(
                        inline_asset_from_bytes(
                            file_name,
                            payload,
                            part.get_content_type(),
                        )
                    )
                response = self._daemon.submit_event(
                    {
                        "event_id": f"email-{thread_anchor}-{uid.decode('utf-8')}",
                        "fingerprint": str(msg.get("Message-ID") or uid.decode("utf-8")),
                        "occurred_at_ms": int(time.time() * 1000),
                        "actor_id": sender_addr,
                        "source_kind": "email",
                        "intent": "message",
                        "relation": (
                            {
                                "kind": "reply_to",
                                "target_event_id": str(msg.get("In-Reply-To")),
                            }
                            if msg.get("In-Reply-To")
                            else None
                        ),
                        "thread_path": [sender_addr, thread_anchor],
                        "content": body or subject or "(empty email)",
                        "attachments": attachments,
                        "reply_route": route_encode(
                            {
                                "to": sender_addr,
                                "subject": subject,
                                "in_reply_to": msg.get("Message-ID"),
                                "thread_anchor": thread_anchor,
                            }
                        ),
                        "metadata": {"subject": subject},
                    }
                )
                if response.get("status") in {"accepted", "duplicate"}:
                    mail.store(uid, "+FLAGS", "\\Seen")
        finally:
            try:
                mail.close()
            except Exception:
                pass
            try:
                mail.logout()
            except Exception:
                pass

    def deliver(self, payload: Dict[str, Any]) -> None:
        route = route_decode(payload.get("reply_route"))
        to_addr = route.get("to")
        if not to_addr:
            raise TerminalDeliveryError("email reply route is missing destination address")
        subject = route.get("subject") or "Kheish"
        if not subject.lower().startswith("re:"):
            subject = f"Re: {subject}"
        msg = MIMEMultipart()
        msg["From"] = self._address
        msg["To"] = to_addr
        msg["Subject"] = subject
        if route.get("in_reply_to"):
            msg["In-Reply-To"] = route["in_reply_to"]
            msg["References"] = route["in_reply_to"]
        msg["Message-ID"] = f"<kheish-{uuid.uuid4().hex[:12]}@{self._address.split('@')[-1]}>"
        text = str(payload.get("content") or "").strip()
        if text:
            msg.attach(MIMEText(text, "plain", "utf-8"))
        for asset in iter_delivery_assets(payload):
            body = self._daemon.download_asset(asset)
            part = MIMEBase("application", "octet-stream")
            part.set_payload(body)
            encoders.encode_base64(part)
            part.add_header(
                "Content-Disposition",
                f"attachment; filename={asset.get('file_name') or 'attachment.bin'}",
            )
            msg.attach(part)
        smtp = smtplib.SMTP(self._smtp_host, self._smtp_port, timeout=30)
        try:
            smtp.starttls(context=ssl.create_default_context())
            smtp.login(self._address, self._password)
            smtp.send_message(msg)
        except smtplib.SMTPResponseException as exc:
            if 400 <= exc.smtp_code < 500:
                raise RetryableDeliveryError(
                    f"SMTP returned transient error {exc.smtp_code}: {exc.smtp_error!r}"
                ) from exc
            raise TerminalDeliveryError(
                f"SMTP returned permanent error {exc.smtp_code}: {exc.smtp_error!r}"
            ) from exc
        except (smtplib.SMTPException, OSError) as exc:
            raise RetryableDeliveryError(f"SMTP delivery failed: {exc}") from exc
        finally:
            try:
                smtp.quit()
            except Exception:
                smtp.close()


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

    def handle_platform_request(self, method: str, path: str, query: str, headers: Any, body: bytes):
        if method == "POST" and path == "/twilio/inbound":
            form = urllib.parse.parse_qs(body.decode("utf-8"), keep_blank_values=True)
            flat = {key: values[0] for key, values in form.items() if values}
            if self._webhook_url and not bool_env("SMS_INSECURE_NO_SIGNATURE", False):
                signature = headers.get("X-Twilio-Signature", "")
                if not signature or not self._validate_signature(flat, signature):
                    return 403, "application/xml", b'<?xml version="1.0" encoding="UTF-8"?><Response></Response>'
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
            return 200, "application/xml", b'<?xml version="1.0" encoding="UTF-8"?><Response></Response>'
        return None

    def deliver(self, payload: Dict[str, Any]) -> None:
        if iter_delivery_assets(payload):
            raise TerminalDeliveryError("Twilio SMS delivery does not support daemon attachments")
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
            {"jsonrpc": "2.0", "id": f"{method}-{uuid.uuid4().hex[:8]}", "method": method, "params": params},
            timeout=60.0,
        )
        classify_delivery_http_error(
            status,
            f"signal RPC {method} returned HTTP {status}",
            retryable_statuses={429},
        )
        if "error" in decoded:
            raise RetryableDeliveryError(f"signal RPC {method} failed: {decoded['error']}")
        return decoded.get("result")

    def _sse_loop(self) -> None:
        url = f"{self._http_url}/api/v1/events?account={urllib.parse.quote(self._account, safe='')}"
        while True:
            try:
                request = urllib.request.Request(url, headers={"Accept": "text/event-stream"})
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
        result = self._rpc("getAttachment", {"account": self._account, "id": attachment_id})
        data = result.get("data") if isinstance(result, dict) else result
        if not data:
            return None
        raw = base64.b64decode(data)
        return inline_asset_from_bytes(f"signal-{attachment_id}", raw)

    def _handle_signal_event(self, data: str) -> None:
        envelope = json.loads(data)
        envelope = envelope.get("envelope", envelope)
        data_message = envelope.get("dataMessage") or (envelope.get("editMessage") or {}).get("dataMessage")
        if not data_message:
            return
        sender = envelope.get("sourceNumber") or envelope.get("sourceUuid") or envelope.get("source")
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
        params: Dict[str, Any] = {"account": self._account, "message": str(payload.get("content") or "")}
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
            status, decoded = json_request("GET", f"{self._bridge_url}/health", timeout=10.0)
            if status >= 400:
                self.mark_degraded(f"whatsapp bridge health returned HTTP {status}")
                return
            self.mark_ready(decoded.get("status"))
        except Exception as exc:
            self.mark_degraded(str(exc))
            return
        threading.Thread(target=self._poll_loop, daemon=True).start()

    def _bridge_json(self, method: str, path: str, payload: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
        status, decoded = json_request(method, f"{self._bridge_url}{path}", payload, timeout=120.0)
        if status >= 500:
            raise RetryableDeliveryError(f"whatsapp bridge returned HTTP {status}")
        if status >= 400:
            raise TerminalDeliveryError(f"whatsapp bridge returned HTTP {status}: {decoded}")
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
                    raise RuntimeError("whatsapp mediaUrls may not reference local absolute paths")
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
                media_type = guess_media_type(asset.get("file_name") or "", asset.get("media_type"))
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


class WebhookConnector(ExternalConnectorApp):
    platform = "webhook"
    attachments_in = False
    attachments_out = False

    def __init__(self) -> None:
        super().__init__()
        self._routes = json_env("WEBHOOK_ROUTES_JSON", {})
        self._session_mode = os.environ.get("WEBHOOK_SESSION_MODE", "per_delivery").strip().lower()

    def start_background(self) -> None:
        if self._routes or self.enable_test_api or self.capture_path:
            self.mark_ready()
        else:
            self.mark_degraded("WEBHOOK_ROUTES_JSON is not configured")

    def _render_prompt(self, template: str, route_name: str, event_type: str, payload: Dict[str, Any]) -> str:
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

    def _validate_route_signature(self, route: Dict[str, Any], headers: Any, body: bytes) -> bool:
        secret = str(route.get("secret") or "").strip()
        if not secret or secret == "INSECURE_NO_AUTH":
            return True
        header_name = route.get("signature_header") or "X-Hub-Signature-256"
        signature = headers.get(header_name, "")
        if not signature:
            return False
        if signature.startswith("sha256="):
            expected = "sha256=" + hmac.new(secret.encode("utf-8"), body, "sha256").hexdigest()
            return hmac.compare_digest(expected, signature)
        expected = hmac.new(secret.encode("utf-8"), body, "sha256").hexdigest()
        return hmac.compare_digest(expected, signature)

    def handle_platform_request(self, method: str, path: str, query: str, headers: Any, body: bytes):
        if method != "POST" or not path.startswith("/webhooks/"):
            return None
        route_name = path.rsplit("/", 1)[-1]
        route = self._routes.get(route_name)
        if not isinstance(route, dict):
            return 404, "application/json", json.dumps({"error": "unknown route"}).encode("utf-8")
        if not self._validate_route_signature(route, headers, body):
            return 401, "application/json", json.dumps({"error": "invalid signature"}).encode("utf-8")
        try:
            payload = json.loads(body.decode("utf-8"))
        except json.JSONDecodeError:
            payload = dict(urllib.parse.parse_qsl(body.decode("utf-8"), keep_blank_values=True))
        event_header = route.get("event_header") or "X-GitHub-Event"
        event_type = headers.get(event_header, "") or payload.get("event_type") or "unknown"
        allowed_events = route.get("allowed_events") or []
        if allowed_events and event_type not in allowed_events:
            return 200, "application/json", json.dumps({"status": "ignored"}).encode("utf-8")
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
                        "allowed_callback_hosts": route.get("allowed_callback_hosts") or [],
                    }
                    if callback_url
                    else {}
                ),
                "metadata": {"route": route_name, "event_type": event_type},
            }
        )
        return 200, "application/json", json.dumps({"status": "accepted", "delivery_id": delivery_id}).encode("utf-8")

    def deliver(self, payload: Dict[str, Any]) -> None:
        route = route_decode(payload.get("reply_route"))
        callback_url = route.get("callback_url")
        if not callback_url:
            return
        status, _headers, response_body = safe_public_http_request(
            "POST",
            callback_url,
            body=json.dumps({"content": payload.get("content", ""), "metadata": payload.get("metadata", {})}).encode("utf-8"),
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


CONNECTOR_FACTORIES = {
    "discord": DiscordConnector,
    "matrix": MatrixConnector,
    "email": EmailConnector,
    "sms": SmsConnector,
    "signal": SignalConnector,
    "whatsapp": WhatsAppConnector,
    "webhook": WebhookConnector,
}
