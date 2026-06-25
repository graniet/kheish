//! Daemon-managed ingress and egress connectors.

mod config;
mod external_protocol;
mod ingress;
mod output;
mod routes;
mod runtime;

pub use config::{
    ConnectorKind, ConnectorRegistry, ConnectorSessionPolicy, ConnectorSettings,
    ExternalChildProcessConfig, ExternalConnectorConfig, ExternalConnectorMode, ExternalReplyRoute,
    ExternalThreadRef, HttpInputConnectorConfig, HttpReplyRoute, SlackConnectorConfig,
    SlackReplyRoute, SlackTeamBotTokenConfig, TelegramConnectorConfig, TelegramIngressMode,
    TelegramReplyRoute, decode_external_reply_route, decode_http_reply_route,
    decode_slack_reply_route, decode_telegram_reply_route,
    default_slack_ingress_events_per_second as slack_default_ingress_events_per_second,
    default_telegram_ingress_events_per_second, encode_external_reply_route,
    encode_http_reply_route, encode_slack_reply_route, encode_telegram_reply_route,
    http_default_ingress_events_per_second, http_default_signature_max_age_secs,
    reply_target_references_connector,
};
#[allow(unused_imports)]
pub use external_protocol::{
    EXTERNAL_CONNECTOR_INGRESS_PROTOCOL_VERSION, EXTERNAL_CONNECTOR_PROTOCOL_VERSION,
    ExternalConnectorAssetDescriptor, ExternalConnectorCapabilities,
    ExternalConnectorDeliveryRequest, ExternalConnectorDeliveryResponse,
    ExternalConnectorDeliveryStatus, ExternalConnectorHealth, ExternalConnectorHealthStatus,
    ExternalConnectorIngressBatchItemResponse, ExternalConnectorIngressBatchItemStatus,
    ExternalConnectorIngressBatchRequest, ExternalConnectorIngressBatchResponse,
    ExternalConnectorIngressEvent, ExternalConnectorIngressEventRelation,
    ExternalConnectorIngressRequest, ExternalConnectorIngressResponse,
    ExternalConnectorIngressStatus, ExternalConnectorManifest, ExternalConnectorOutputPart,
    is_supported_external_connector_ingress_protocol_version,
    is_supported_external_connector_protocol_version,
};
pub(crate) use ingress::spawn_ingress_tasks;
pub use output::{build_delivery_dispatcher, register_output_plugins};
pub(crate) use routes::build_router;
pub(crate) use runtime::ExternalConnectorRuntimeService;
