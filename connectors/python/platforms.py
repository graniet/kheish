"""Backward-compatible imports for the original flat connector module."""

from common import classify_delivery_http_error
from registry import (
    CONNECTOR_FACTORIES,
    DiscordConnector,
    EmailConnector,
    MatrixConnector,
    SignalConnector,
    SmsConnector,
    WebhookConnector,
    WhatsAppConnector,
)

__all__ = [
    "CONNECTOR_FACTORIES",
    "DiscordConnector",
    "EmailConnector",
    "MatrixConnector",
    "SignalConnector",
    "SmsConnector",
    "WebhookConnector",
    "WhatsAppConnector",
    "classify_delivery_http_error",
]
