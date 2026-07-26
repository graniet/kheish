import asyncio
import sys
import unittest
from datetime import datetime, timezone
from email.message import EmailMessage
from pathlib import Path
from types import SimpleNamespace
from typing import Any


CONNECTOR_ROOT = Path(__file__).resolve().parents[1]
if str(CONNECTOR_ROOT) not in sys.path:
    sys.path.insert(0, str(CONNECTOR_ROOT))

from adapters.discord import DiscordConnector  # noqa: E402
from adapters.email import _email_thread_anchor, _extract_email_text  # noqa: E402
from adapters.matrix import MatrixConnector  # noqa: E402
from adapters.signal import SignalConnector  # noqa: E402
from adapters.whatsapp import WhatsAppConnector  # noqa: E402
from common import route_decode  # noqa: E402


class RecordingDaemon:
    def __init__(self) -> None:
        self.events: list[dict[str, Any]] = []

    def submit_event(self, payload: dict[str, Any]) -> dict[str, str]:
        self.events.append(payload)
        return {"status": "accepted"}


class AdapterEventTests(unittest.TestCase):
    def test_discord_dm_message_is_normalized(self) -> None:
        class FakeDMChannel:
            id = 42

        class FakeThread:
            pass

        author = SimpleNamespace(id=7, bot=False)
        bot_user = SimpleNamespace(id=99)
        message = SimpleNamespace(
            id=123,
            author=author,
            channel=FakeDMChannel(),
            guild=None,
            mentions=[],
            content="hello discord",
            attachments=[],
            reference=None,
            created_at=datetime(2025, 1, 1, tzinfo=timezone.utc),
        )
        daemon = RecordingDaemon()
        connector = object.__new__(DiscordConnector)
        connector._client = SimpleNamespace(user=bot_user)
        connector._discord = SimpleNamespace(
            DMChannel=FakeDMChannel,
            Thread=FakeThread,
        )
        connector._daemon = daemon

        asyncio.run(connector._handle_message(message))

        self.assertEqual(len(daemon.events), 1)
        event = daemon.events[0]
        self.assertEqual(event["event_id"], "discord-123")
        self.assertEqual(event["thread_path"], ["dm", "42"])
        self.assertEqual(event["content"], "hello discord")
        self.assertEqual(
            route_decode(event["reply_route"]),
            {"channel_id": "42", "reply_to_message_id": "123"},
        )

    def test_matrix_reply_is_normalized(self) -> None:
        daemon = RecordingDaemon()
        connector = object.__new__(MatrixConnector)
        connector._user_id = "@kheish:example.org"
        connector._daemon = daemon

        connector._handle_event(
            "!room:example.org",
            {
                "type": "m.room.message",
                "event_id": "$message",
                "sender": "@alice:example.org",
                "content": {
                    "msgtype": "m.text",
                    "body": "hello matrix",
                    "m.relates_to": {
                        "m.in_reply_to": {"event_id": "$parent"},
                    },
                },
            },
        )

        self.assertEqual(len(daemon.events), 1)
        event = daemon.events[0]
        self.assertEqual(event["event_id"], "matrix-$message")
        self.assertEqual(event["thread_path"], ["!room:example.org"])
        self.assertEqual(
            event["relation"],
            {"kind": "reply_to", "target_event_id": "$parent"},
        )

    def test_signal_group_message_is_normalized(self) -> None:
        daemon = RecordingDaemon()
        connector = object.__new__(SignalConnector)
        connector._account = "+15550000000"
        connector._daemon = daemon

        connector._handle_signal_event(
            """{
                "envelope": {
                    "timestamp": 1700000000000,
                    "sourceNumber": "+15551234567",
                    "dataMessage": {
                        "message": "hello signal",
                        "groupInfo": {"groupId": "group-1"},
                        "attachments": []
                    }
                }
            }"""
        )

        self.assertEqual(len(daemon.events), 1)
        event = daemon.events[0]
        self.assertEqual(event["thread_path"], ["group:group-1"])
        self.assertEqual(event["content"], "hello signal")
        self.assertEqual(
            route_decode(event["reply_route"]),
            {"chat_id": "group:group-1"},
        )

    def test_whatsapp_message_is_normalized(self) -> None:
        daemon = RecordingDaemon()
        connector = object.__new__(WhatsAppConnector)
        connector._daemon = daemon
        connector._allowed_media_hosts = set()
        connector._media_max_bytes = 1024

        connector._handle_bridge_message(
            {
                "messageId": "wa-1",
                "chatId": "chat-42",
                "senderId": "alice",
                "body": "hello whatsapp",
                "isGroup": True,
                "mediaUrls": [],
            }
        )

        self.assertEqual(len(daemon.events), 1)
        event = daemon.events[0]
        self.assertEqual(event["event_id"], "whatsapp-wa-1")
        self.assertEqual(event["thread_path"], ["chat-42"])
        self.assertEqual(event["metadata"], {"is_group": True})

    def test_email_helpers_preserve_body_and_thread_identity(self) -> None:
        message = EmailMessage()
        message["References"] = "<first@example.org> <latest@example.org>"
        message.set_content("<p>Hello</p><p>World</p>", subtype="html")

        self.assertEqual(_extract_email_text(message), "Hello\nWorld")
        self.assertEqual(_email_thread_anchor(message), "<latest@example.org>")


if __name__ == "__main__":
    unittest.main()
