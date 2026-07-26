from common import ExternalConnectorApp
from adapters.discord import DiscordConnector
from adapters.email import EmailConnector
from adapters.matrix import MatrixConnector
from adapters.signal import SignalConnector
from adapters.sms import SmsConnector
from adapters.webhook import WebhookConnector
from adapters.whatsapp import WhatsAppConnector

CONNECTOR_FACTORIES: dict[str, type[ExternalConnectorApp]] = {
    "discord": DiscordConnector,
    "matrix": MatrixConnector,
    "email": EmailConnector,
    "sms": SmsConnector,
    "signal": SignalConnector,
    "whatsapp": WhatsAppConnector,
    "webhook": WebhookConnector,
}

__all__ = [
    "CONNECTOR_FACTORIES",
    "DiscordConnector",
    "EmailConnector",
    "MatrixConnector",
    "SignalConnector",
    "SmsConnector",
    "WebhookConnector",
    "WhatsAppConnector",
]
