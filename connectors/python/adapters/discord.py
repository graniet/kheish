import asyncio
import io
import os
import re
import threading
from typing import Any, Dict

from common import (
    ExternalConnectorApp,
    LOG,
    RetryableDeliveryError,
    TerminalDeliveryError,
    bool_env,
    inline_asset_from_bytes,
    iter_delivery_assets,
    route_decode,
    route_encode,
)


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
        max_bytes = int(
            os.environ.get("DISCORD_ATTACHMENT_MAX_BYTES", str(5 * 1024 * 1024))
        )
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
                    filename=asset.get("file_name")
                    or asset.get("id")
                    or "attachment.bin",
                )
            )
        if files:
            kwargs["files"] = files
        await channel.send(**kwargs)

    def deliver(self, payload: Dict[str, Any]) -> None:
        if not self._loop:
            raise TerminalDeliveryError("discord event loop is not running")
        future = asyncio.run_coroutine_threadsafe(
            self._deliver_async(payload), self._loop
        )
        future.result(timeout=60)
