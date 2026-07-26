import email
import os
import re
import smtplib
import ssl
import threading
import time
import uuid
from email import encoders
from email.header import decode_header
from email.mime.base import MIMEBase
from email.mime.multipart import MIMEMultipart
from email.mime.text import MIMEText
from email.utils import parseaddr
from typing import Any, Dict

from common import (
    ExternalConnectorApp,
    LOG,
    RetryableDeliveryError,
    TerminalDeliveryError,
    inline_asset_from_bytes,
    iter_delivery_assets,
    route_decode,
    route_encode,
)


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
                    file_name = _decode_header_value(
                        part.get_filename() or "attachment.bin"
                    )
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
                        "fingerprint": str(
                            msg.get("Message-ID") or uid.decode("utf-8")
                        ),
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
            raise TerminalDeliveryError(
                "email reply route is missing destination address"
            )
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
        msg["Message-ID"] = (
            f"<kheish-{uuid.uuid4().hex[:12]}@{self._address.split('@')[-1]}>"
        )
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
