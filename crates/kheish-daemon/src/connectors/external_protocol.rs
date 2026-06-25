use kheish_types::ConversationKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{InputAttachmentRequest, SubmitInputItemRequest};

use super::config::ExternalThreadRef;

pub const EXTERNAL_CONNECTOR_PROTOCOL_VERSION: u32 = 1;
pub const EXTERNAL_CONNECTOR_INGRESS_PROTOCOL_VERSION: u32 = 2;
pub const MIN_EXTERNAL_CONNECTOR_INGRESS_PROTOCOL_VERSION: u32 = 1;

pub fn is_supported_external_connector_protocol_version(version: u32) -> bool {
    version == EXTERNAL_CONNECTOR_PROTOCOL_VERSION
}

pub fn is_supported_external_connector_ingress_protocol_version(version: u32) -> bool {
    (MIN_EXTERNAL_CONNECTOR_INGRESS_PROTOCOL_VERSION..=EXTERNAL_CONNECTOR_INGRESS_PROTOCOL_VERSION)
        .contains(&version)
}

/// One additive manifest summary describing sidecar surface area.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalConnectorCapabilities {
    #[serde(default = "default_true")]
    pub attachments_in: bool,
    #[serde(default = "default_true")]
    pub attachments_out: bool,
    #[serde(default)]
    pub threads: bool,
}

impl Default for ExternalConnectorCapabilities {
    fn default() -> Self {
        Self {
            attachments_in: true,
            attachments_out: true,
            threads: false,
        }
    }
}

/// Sidecar manifest returned by `GET /manifest`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalConnectorManifest {
    pub protocol_version: u32,
    pub instance_id: String,
    #[serde(default)]
    pub capabilities: ExternalConnectorCapabilities,
    #[serde(default)]
    pub experimental: bool,
}

/// One sidecar health snapshot returned by `GET /health`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalConnectorHealth {
    pub protocol_version: u32,
    pub instance_id: String,
    pub status: ExternalConnectorHealthStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// High-level lifecycle states exposed by one sidecar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalConnectorHealthStatus {
    #[default]
    Starting,
    Ready,
    Degraded,
    Draining,
}

/// One inbound external event submitted into the daemon-owned connector pipeline.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExternalConnectorIngressEventRelation {
    pub kind: String,
    pub target_event_id: String,
}

/// One inbound external event shared by the single-event and batch ingress routes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExternalConnectorIngressEvent {
    pub instance_id: String,
    pub event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<ExternalConnectorIngressEventRelation>,
    #[serde(default)]
    pub thread: ExternalThreadRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_key: Option<String>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub input_items: Vec<SubmitInputItemRequest>,
    #[serde(default)]
    pub attachments: Vec<InputAttachmentRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// One inbound external event submitted into the daemon-owned connector pipeline.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExternalConnectorIngressRequest {
    #[serde(default = "default_ingress_protocol_version")]
    pub protocol_version: u32,
    #[serde(flatten)]
    pub event: ExternalConnectorIngressEvent,
}

/// One bounded batch of inbound external events.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExternalConnectorIngressBatchRequest {
    #[serde(default = "default_ingress_protocol_version")]
    pub protocol_version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<ExternalConnectorIngressEvent>,
}

/// The result of one inbound external event submission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalConnectorIngressResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    pub status: ExternalConnectorIngressStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One batch ingress response preserving request order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalConnectorIngressBatchItemStatus {
    Accepted,
    Duplicate,
    Rejected,
    RateLimited,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalConnectorIngressBatchItemResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    pub status: ExternalConnectorIngressBatchItemStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalConnectorIngressBatchResponse {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<ExternalConnectorIngressBatchItemResponse>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalConnectorIngressStatus {
    Accepted,
    Duplicate,
    Rejected,
}

/// One daemon-owned attachment projected through the sidecar delivery contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalConnectorAssetDescriptor {
    pub id: String,
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_length: Option<u64>,
    pub download_path: String,
}

/// One externally consumable output part sent to a sidecar.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExternalConnectorOutputPart {
    Text {
        text: String,
    },
    Attachment {
        attachment: ExternalConnectorAssetDescriptor,
    },
}

/// One daemon-to-sidecar delivery attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExternalConnectorDeliveryRequest {
    #[serde(default = "default_protocol_version")]
    pub protocol_version: u32,
    pub delivery_id: String,
    pub attempt: u32,
    pub reply_route: String,
    pub conversation: ConversationKey,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<ExternalConnectorOutputPart>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ExternalConnectorAssetDescriptor>,
    pub metadata: Value,
}

/// One sidecar delivery acknowledgment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalConnectorDeliveryResponse {
    pub status: ExternalConnectorDeliveryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalConnectorDeliveryStatus {
    Committed,
    RetryableError,
    TerminalError,
}

pub fn default_protocol_version() -> u32 {
    EXTERNAL_CONNECTOR_PROTOCOL_VERSION
}

pub fn default_ingress_protocol_version() -> u32 {
    EXTERNAL_CONNECTOR_INGRESS_PROTOCOL_VERSION
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn external_protocol_version_policy_is_explicit() {
        assert!(is_supported_external_connector_protocol_version(1));
        assert!(!is_supported_external_connector_protocol_version(2));
        assert!(is_supported_external_connector_ingress_protocol_version(1));
        assert!(is_supported_external_connector_ingress_protocol_version(2));
        assert!(!is_supported_external_connector_ingress_protocol_version(3));
    }

    #[test]
    fn external_ingress_v2_golden_shape_is_stable() {
        let request = ExternalConnectorIngressRequest {
            protocol_version: 2,
            event: ExternalConnectorIngressEvent {
                instance_id: "discord-main".to_string(),
                event_id: "evt-1".to_string(),
                fingerprint: Some("upstream-fp-1".to_string()),
                occurred_at_ms: Some(1_730_000_000_000),
                actor_id: Some("user-42".to_string()),
                source_kind: Some("discord".to_string()),
                intent: Some("message".to_string()),
                relation: Some(ExternalConnectorIngressEventRelation {
                    kind: "reply_to".to_string(),
                    target_event_id: "evt-0".to_string(),
                }),
                thread: ExternalThreadRef {
                    path: vec!["guild-1".to_string(), "channel-2".to_string()],
                },
                routing_key: Some("ignored-when-thread-present".to_string()),
                content: "hello".to_string(),
                input_items: Vec::new(),
                attachments: Vec::new(),
                reply_route: Some("{\"channel_id\":\"2\"}".to_string()),
                metadata: Some(json!({ "message_id": "123" })),
            },
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "protocol_version": 2,
                "instance_id": "discord-main",
                "event_id": "evt-1",
                "fingerprint": "upstream-fp-1",
                "occurred_at_ms": 1730000000000_u64,
                "actor_id": "user-42",
                "source_kind": "discord",
                "intent": "message",
                "relation": {
                    "kind": "reply_to",
                    "target_event_id": "evt-0"
                },
                "thread": {
                    "path": ["guild-1", "channel-2"]
                },
                "routing_key": "ignored-when-thread-present",
                "content": "hello",
                "input_items": [],
                "attachments": [],
                "reply_route": "{\"channel_id\":\"2\"}",
                "metadata": {
                    "message_id": "123"
                }
            })
        );
    }

    #[test]
    fn external_manifest_capabilities_default_to_legacy_compatible() {
        let manifest = serde_json::from_value::<ExternalConnectorManifest>(json!({
            "protocol_version": 1,
            "instance_id": "sidecar-1"
        }))
        .unwrap();
        assert!(manifest.capabilities.attachments_in);
        assert!(manifest.capabilities.attachments_out);
        assert!(!manifest.capabilities.threads);
        assert!(!manifest.experimental);
    }
}
