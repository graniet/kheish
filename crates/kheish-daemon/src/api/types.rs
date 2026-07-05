//! Request and response DTOs for the daemon HTTP API.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use kheish_agent::{
    AgentRecord, AgentStatus, ChildRetentionPolicy, ForkContext, ManagedAgentSnapshot,
};
use kheish_mcp::McpRuntimeSnapshot;
use kheish_runtime::{
    DebugCaptureLevel, ModelGenerationConfig, PermissionMode, SystemPromptSettings,
    ToolRuntimeLimits,
};
use kheish_session::StoredSession;
use kheish_skills::{SkillDefinition, SkillRuntimeConfig, SkillScope, SkillSummary};
use kheish_types::{
    ApprovalResolution, CapabilityScope, CompletionRequirement, CredentialScope, HookEventName,
    HookFailureMode, HookSettings, LearnedContextBundle, LearningEvidenceRef, LearningKind,
    LearningPolicyDecision, LearningPublishTier, LearningScope, LearningScopeKind,
    LearningSensitivity, LearningSourceRef, LearningStatus, LearningVerificationStatus,
    PersonaSkillAssignment, RecoveredMemoryBundle, ReplyHandle, SessionGoal, SessionGoalStatus,
    SessionOperatorConfig, SessionPersonaBinding, SessionRoutePolicy, SkillExecutionContext,
    TaskStatus, ToolSurfaceFilter, UserQuestionRequest, UserQuestionResolution,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::LearningAutomationPolicyConfig;
use crate::assets::StoredAssetRecord;
use crate::connectors::{
    ExternalReplyRoute, HttpReplyRoute, SlackReplyRoute, TelegramReplyRoute,
    encode_external_reply_route, encode_http_reply_route, encode_slack_reply_route,
    encode_telegram_reply_route,
};
use crate::derivations::DerivationCreateRequest;
use crate::personas::{PersonaIndexEntry, PersonaRecord};
use crate::services::{ConnectorConfigRecord, ConnectorConfigSource};
use crate::{ConnectorSessionPolicy, DaemonOutputRecord, LearningCandidateState};

fn default_learning_sensitivity() -> LearningSensitivity {
    LearningSensitivity::Scoped
}

fn serialize_u64_as_decimal_string<S>(
    value: &u64,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

fn serialize_optional_u64_as_decimal_string<S>(
    value: &Option<u64>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        Some(value) => serializer.serialize_some(&value.to_string()),
        None => serializer.serialize_none(),
    }
}

fn deserialize_u64_from_decimal_string_or_number<'de, D>(
    deserializer: D,
) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    u64_from_decimal_string_or_number(&value).map_err(serde::de::Error::custom)
}

fn deserialize_optional_u64_from_decimal_string_or_number<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(None);
    }
    u64_from_decimal_string_or_number(&value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

fn u64_from_decimal_string_or_number(value: &Value) -> std::result::Result<u64, String> {
    if let Some(number) = value.as_u64() {
        return Ok(number);
    }
    if let Some(string) = value.as_str() {
        return string
            .parse::<u64>()
            .map_err(|_| "event id cursor must be a decimal u64 string".to_string());
    }
    Err("event id cursor must be a decimal u64 string or number".to_string())
}

fn default_learning_confidence() -> u8 {
    80
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_json_object() -> Value {
    Value::Object(Default::default())
}
use crate::{ScheduleCreateRequest, ScheduleView};

/// Request body for invoking one daemon-loaded MCP tool through the operator API.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpToolCallRequest {
    /// JSON object passed to the MCP tool as `arguments`.
    #[serde(default = "default_json_object")]
    pub input: Value,
}

impl Default for McpToolCallRequest {
    fn default() -> Self {
        Self {
            input: default_json_object(),
        }
    }
}

/// Response body for one daemon-loaded MCP tool invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpToolCallResponse {
    /// Qualified MCP tool name, for example `mcp__github__get_me`.
    pub tool_name: String,
    /// Sanitized MCP tool output in the same shape returned to model tools.
    pub output: kheish_runtime::ToolExecutionOutput,
}

/// RFC 7807-style API error response used by the daemon control plane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProblemDetails {
    /// Stable machine-readable problem type.
    #[serde(rename = "type")]
    pub problem_type: String,
    /// Short human-readable title for the error class.
    pub title: String,
    /// HTTP status code.
    pub status: u16,
    /// Human-readable detail for this occurrence.
    pub detail: String,
    /// Stable daemon error code.
    pub code: String,
    /// Optional feature domain for stable daemon problem codes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

/// Common KheishStack request body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackManifestRequest {
    /// Raw YAML or JSON KheishStack manifest.
    pub manifest: String,
    /// Deprecated compatibility field. Stack APIs require a self-contained manifest and reject file references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_root: Option<String>,
    /// Deprecated compatibility field. KheishStack v1alpha1 always enforces strict scope validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict_scopes: Option<bool>,
}

/// KheishStack plan request body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackPlanRequest {
    #[serde(flatten)]
    pub stack: StackManifestRequest,
    /// When true, omits no-op and verification-only actions from the response.
    #[serde(default)]
    pub only_changes: bool,
    /// Allows value_env secrets to be read from the daemon environment for fingerprint planning.
    #[serde(default)]
    pub allow_secret_env: bool,
}

/// KheishStack apply request body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackApplyRequest {
    #[serde(flatten)]
    pub stack: StackManifestRequest,
    /// Returns the plan-shaped apply report without mutating daemon resources.
    #[serde(default)]
    pub dry_run: bool,
    /// Allows value_env secrets to be read from the daemon environment.
    #[serde(default)]
    pub allow_secret_env: bool,
    /// Deprecated compatibility field. Startup-only drift remains blocked by the daemon Stack API.
    #[serde(default)]
    pub force_restart: bool,
    /// Prunes ledger-owned resources omitted from the desired manifest when supported.
    #[serde(default)]
    pub prune: bool,
}

/// KheishStack import request body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackImportRequest {
    #[serde(flatten)]
    pub stack: StackManifestRequest,
    /// Optional explicit resource keys such as `persona/demo` or `connector/http/inbox`.
    #[serde(default)]
    pub resources: Vec<String>,
    /// Records value_env fingerprints for imported secrets. The caller attests the live secret matches the environment value.
    #[serde(default)]
    pub allow_secret_env: bool,
}

/// KheishStack down request body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackDownRequest {
    #[serde(flatten)]
    pub stack: StackManifestRequest,
    /// Executes supported destructive operations. When false, returns the plan only.
    #[serde(default)]
    pub yes: bool,
}

impl ProblemDetails {
    /// Builds one daemon problem document.
    pub fn new(status: u16, code: impl Into<String>, detail: impl Into<String>) -> Self {
        let code = code.into();
        Self {
            problem_type: format!("urn:kheish:problem:{code}"),
            title: problem_title(status),
            status,
            detail: detail.into(),
            code,
            domain: None,
        }
    }

    /// Attaches a stable feature domain to this daemon problem document.
    #[must_use]
    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }
}

fn problem_title(status: u16) -> String {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        500 => "Internal Server Error",
        _ => "Daemon Error",
    }
    .to_string()
}

/// Asset list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetListQuery {
    /// Filters assets by identifier, file name, MIME type, or digest substring.
    pub query: Option<String>,
    pub page: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

impl AssetListQuery {
    pub fn page_query(&self) -> ListPageQuery {
        list_page_query(self.page, self.cursor.clone())
    }
}

/// Asset delete query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetDeleteQuery {
    /// When true, returns the deletion plan without removing files.
    #[serde(default)]
    pub dry_run: Option<bool>,
}

/// Asset garbage-collection request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetGcRequest {
    /// Defaults to true so GC is inspect-only unless callers explicitly execute it.
    #[serde(default)]
    pub dry_run: Option<bool>,
}

/// Board list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardListQuery {
    /// Filters boards by identifier or display name substring.
    pub query: Option<String>,
    /// Restricts results to one owning session identifier.
    pub owner_session_id: Option<String>,
}

/// Channel list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelListQuery {
    /// Filters channels by identifier, title, description, or purpose substring.
    pub query: Option<String>,
}

/// Project list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectListQuery {
    /// Filters projects by identifier, display name, or description substring.
    pub query: Option<String>,
    /// Restricts results to one linked member session identifier.
    pub member_session_id: Option<String>,
    /// Restricts results to one linked channel identifier.
    pub channel_id: Option<String>,
    /// Restricts results to one project lifecycle state.
    pub status: Option<crate::ProjectStatus>,
}

/// Channel message list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelMessageListQuery {
    /// Restricts results to one thread root identifier.
    pub thread_root_message_id: Option<String>,
    /// Filters messages by sender, content, or attachment metadata substring.
    pub query: Option<String>,
    /// Limits the number of returned messages after filtering.
    pub limit: Option<usize>,
}

/// Channel stimulus list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelStimulusListQuery {
    /// Restricts results to one canonical thread root identifier.
    pub thread_root_message_id: Option<String>,
    /// Restricts results to one durable stimulus state.
    pub state: Option<crate::ChannelStimulusState>,
    /// Limits the number of returned stimuli after filtering.
    pub limit: Option<usize>,
}

/// Channel thread-work list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelThreadWorkListQuery {
    /// Restricts results to one canonical thread root identifier.
    pub thread_root_message_id: Option<String>,
}

/// Project-task list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectTaskListQuery {
    /// Filters tasks by identifier, title, description, or output substring.
    pub query: Option<String>,
    /// Restricts results to one task lifecycle state.
    pub status: Option<TaskStatus>,
    /// Restricts results to one assigned project member identifier.
    pub assignee_member_id: Option<String>,
}

/// Derivation list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivationListQuery {
    /// Filters derivations by identifier, profile, subject, or result asset substring.
    pub query: Option<String>,
}

/// Derivation create query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivationCreateQuery {
    /// Recompute even when the cache already contains a terminal derivation.
    pub force_refresh: Option<bool>,
    /// Recompute only when the cache currently points at a failed derivation.
    pub retry_failed: Option<bool>,
}

/// Learning-candidate list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningCandidateListQuery {
    /// Filters candidates by identifier, scope, content, or source substring.
    pub query: Option<String>,
    /// Restricts results to one learning scope kind.
    pub scope_kind: Option<LearningScopeKind>,
    /// Restricts results to one scope identifier.
    pub scope_id: Option<String>,
    /// Restricts results to one learning candidate kind.
    pub kind: Option<LearningKind>,
    /// Restricts results to one review state.
    pub state: Option<LearningCandidateState>,
}

/// Published-learning list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningListQuery {
    /// Filters learnings by identifier, scope, content, or source substring.
    pub query: Option<String>,
    /// Restricts results to one learning scope kind.
    pub scope_kind: Option<LearningScopeKind>,
    /// Restricts results to one scope identifier.
    pub scope_id: Option<String>,
    /// Restricts results to one published learning kind.
    pub kind: Option<LearningKind>,
    /// Restricts results to one publication status.
    pub status: Option<LearningStatus>,
    /// Restricts results to one automation decision class.
    pub policy_decision: Option<LearningPolicyDecision>,
    /// Restricts results to one policy actor.
    pub policy_actor: Option<String>,
    /// Restricts results to learnings published from candidates that matched one named rule.
    pub matched_rule_name: Option<String>,
}

/// Promoted learning-skill list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningSkillsListQuery {
    /// Restricts results to one source learning identifier.
    pub source_learning_id: Option<String>,
    /// Restricts results to one promoted-skill status.
    pub status: Option<crate::LearningSkillStatus>,
}

/// Observation-source list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationSourceListQuery {
    /// Filters sources by identifier or display name substring.
    pub query: Option<String>,
}

/// Observation list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationListQuery {
    /// Restricts results to one source identifier.
    pub source_id: Option<String>,
    /// Restricts results to one source stream identifier.
    pub stream_id: Option<String>,
    /// Restricts results to observations captured at or after this timestamp.
    pub after_ms: Option<u64>,
    /// Restricts results to observations captured at or before this timestamp.
    pub before_ms: Option<u64>,
    /// Includes logically purged observations when true.
    #[serde(default)]
    pub include_purged: bool,
}

/// Observation transcript job list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationTranscriptListQuery {
    pub page: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    /// Restricts results to one capture group.
    pub capture_group_id: Option<String>,
    /// Restricts results to one Aurora recording identifier.
    pub recording_id: Option<String>,
    /// Restricts results to one durable transcript status.
    pub status: Option<crate::ObservationTranscriptStatus>,
}

impl ObservationTranscriptListQuery {
    pub fn page_query(&self) -> ListPageQuery {
        list_page_query(self.page, self.cursor.clone())
    }
}

/// Observation transcript segment list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationTranscriptSegmentListQuery {
    pub page: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

impl ObservationTranscriptSegmentListQuery {
    pub fn page_query(&self) -> ListPageQuery {
        list_page_query(self.page, self.cursor.clone())
    }
}

/// Observation audit list query parameters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationAuditListQuery {
    /// Restricts results to one source identifier.
    pub source_id: Option<String>,
    /// Restricts results to one audit event kind.
    pub event: Option<String>,
    /// Maximum number of newest records returned.
    #[serde(default = "default_observation_audit_limit")]
    pub limit: usize,
}

fn default_observation_audit_limit() -> usize {
    100
}

impl Default for ObservationAuditListQuery {
    fn default() -> Self {
        Self {
            source_id: None,
            event: None,
            limit: default_observation_audit_limit(),
        }
    }
}

/// One inline asset payload uploaded through the control plane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineAssetUpload {
    /// The caller-provided file name used for display and MIME inference.
    pub file_name: String,
    /// The optional caller-declared MIME type for the uploaded payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// The base64-encoded raw payload.
    pub content_base64: String,
}

/// One attachment reference or inline upload embedded in a session input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputAttachmentRequest {
    /// References one asset that already exists in the daemon-owned store.
    AssetReference { asset_id: String },
    /// Uploads one new asset inline as part of the request payload.
    InlineAsset(InlineAssetUpload),
}

/// One ordered input item accepted by the session input API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SubmitInputItemRequest {
    /// Contributes one ordered plain-text fragment to the input.
    Text { text: String },
    /// Inserts one existing daemon-owned asset at the current input position.
    AssetReference { asset_id: String },
    /// Inserts one board revision render asset at the current input position.
    BoardReference {
        /// The stable daemon-owned board identifier.
        board_id: String,
        /// The optional immutable revision identifier. When omitted, execution resolves the latest
        /// available board revision.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision_id: Option<String>,
    },
    /// Uploads and inserts one new daemon-owned asset at the current input position.
    InlineAsset(InlineAssetUpload),
}

/// Validates one inline upload or asset reference used in legacy attachment arrays.
pub(crate) fn validate_input_attachment_requests(
    attachments: &[InputAttachmentRequest],
) -> Result<()> {
    for attachment in attachments {
        match attachment {
            InputAttachmentRequest::AssetReference { asset_id } if asset_id.trim().is_empty() => {
                bail!("asset_id is required");
            }
            InputAttachmentRequest::AssetReference { .. } => {}
            InputAttachmentRequest::InlineAsset(upload) if upload.file_name.trim().is_empty() => {
                bail!("attachment file_name is required");
            }
            InputAttachmentRequest::InlineAsset(upload)
                if upload.content_base64.trim().is_empty() =>
            {
                bail!("attachment content_base64 is required");
            }
            InputAttachmentRequest::InlineAsset(_) => {}
        }
    }
    Ok(())
}

/// Validates one ordered multimodal input list.
pub(crate) fn validate_submit_input_items(items: &[SubmitInputItemRequest]) -> Result<()> {
    for item in items {
        match item {
            SubmitInputItemRequest::Text { .. } => {}
            SubmitInputItemRequest::AssetReference { asset_id } if asset_id.trim().is_empty() => {
                bail!("asset_id is required");
            }
            SubmitInputItemRequest::AssetReference { .. } => {}
            SubmitInputItemRequest::BoardReference {
                board_id,
                revision_id,
            } if board_id.trim().is_empty() => {
                bail!("board_id is required");
            }
            SubmitInputItemRequest::BoardReference {
                revision_id: Some(revision_id),
                ..
            } if revision_id.trim().is_empty() => {
                bail!("revision_id is required");
            }
            SubmitInputItemRequest::BoardReference { .. } => {}
            SubmitInputItemRequest::InlineAsset(upload) if upload.file_name.trim().is_empty() => {
                bail!("attachment file_name is required");
            }
            SubmitInputItemRequest::InlineAsset(upload)
                if upload.content_base64.trim().is_empty() =>
            {
                bail!("attachment content_base64 is required");
            }
            SubmitInputItemRequest::InlineAsset(_) => {}
        }
    }
    Ok(())
}

/// Asset import request payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateAssetRequest {
    /// The inline upload that should be persisted as a daemon-owned asset.
    #[serde(flatten)]
    pub upload: InlineAssetUpload,
}

/// Derivation creation request payload.
pub type CreateDerivationRequest = DerivationCreateRequest;

/// Learning-candidate creation request payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateLearningCandidateRequest {
    /// The durable scope that should own the candidate.
    pub scope: LearningScope,
    /// The durable candidate kind.
    pub kind: LearningKind,
    /// The visibility class that should apply after publication.
    #[serde(default = "default_learning_sensitivity")]
    pub sensitivity: LearningSensitivity,
    /// The compact candidate content retained for review.
    pub content: String,
    /// The coarse confidence score in the inclusive range `[0, 100]`.
    #[serde(default = "default_learning_confidence")]
    pub confidence: u8,
    /// Optional provenance pointers retained with the candidate.
    ///
    /// Daemon automation only treats these pointers as trusted when the candidate itself was
    /// created by a daemon-owned workflow.
    #[serde(default)]
    pub source: LearningSourceRef,
    /// Immutable evidence references retained with the candidate.
    ///
    /// Daemon automation only treats these references as trusted when the candidate itself was
    /// created by a daemon-owned workflow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<LearningEvidenceRef>,
    /// The optional expiration timestamp in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
}

/// Learning-candidate publication request payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishLearningCandidateRequest {
    /// Optional replacement scope used for the published learning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<LearningScope>,
    /// Optional replacement kind used for the published learning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<LearningKind>,
    /// Optional replacement sensitivity used for the published learning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitivity: Option<LearningSensitivity>,
    /// Optional replacement content used for the published learning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Optional replacement confidence used for the published learning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<u8>,
    /// Optional replacement expiration timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// Optional publication tier for the durable learning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_tier: Option<LearningPublishTier>,
    /// Optional immutable evidence references retained with the durable learning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<LearningEvidenceRef>,
    /// Optional older learning identifier superseded by the published record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
}

/// Published-learning supersession request payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupersedeLearningRequest {
    /// Optional replacement scope used for the new record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<LearningScope>,
    /// Optional replacement kind used for the new record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<LearningKind>,
    /// Optional replacement sensitivity used for the new record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitivity: Option<LearningSensitivity>,
    /// The compact replacement content retained durably.
    pub content: String,
    /// Optional replacement confidence used for the new record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<u8>,
    /// Optional replacement expiration timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
}

/// Learning revocation request payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeLearningRequest {
    /// Optional human-readable revocation reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Bulk learning revocation request payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeMatchingLearningsRequest {
    /// Filters learnings by identifier, scope, content, or source substring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Restricts revocation to one learning scope kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_kind: Option<LearningScopeKind>,
    /// Restricts revocation to one scope identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,
    /// Restricts revocation to one published learning kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<LearningKind>,
    /// Restricts revocation to one publication status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<LearningStatus>,
    /// Restricts revocation to one automation decision class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_decision: Option<LearningPolicyDecision>,
    /// Restricts revocation to one policy actor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_actor: Option<String>,
    /// Restricts revocation to learnings published from candidates that matched one named rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_rule_name: Option<String>,
    /// Optional human-readable revocation reason applied to every matching learning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn default_learning_skill_context() -> SkillExecutionContext {
    SkillExecutionContext::Fork
}

/// Promoted-learning skill creation request payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateLearningSkillRequest {
    /// The stable skill name exposed in the catalog.
    pub skill_name: String,
    /// Optional description override. Defaults to a generic non-sensitive child-agent summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional when-to-use guidance stored in the skill frontmatter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    /// Optional version string stored in the skill frontmatter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The rendered instructions pinned into the daemon-owned skill file.
    pub instructions: String,
    /// Preferred tools declared by the promoted skill.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
    /// Tools the promoted skill should avoid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_tools: Vec<String>,
    /// The execution context used when the skill is activated. Promoted procedure skills must fork.
    #[serde(default = "default_learning_skill_context")]
    pub context: SkillExecutionContext,
    /// Optional child-agent profile used for forked skill execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_profile: Option<String>,
    /// Optional provider override used for forked skill execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Optional primary model override used for forked skill execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Optional fallback model override used for forked skill execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    /// Optional initial promotion state. New promoted skills default to `draft`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<crate::LearningSkillStatus>,
}

/// Promoted-learning skill rollout result payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningSkillRolloutResultRequest {
    /// The rollout gate that this daemon run should satisfy.
    pub kind: crate::LearningSkillRolloutKind,
    /// The completed daemon run used as rollout evidence.
    pub run_id: String,
    /// Required marker that must appear in the run's latest daemon output for success.
    pub expected_output_contains: String,
    /// Optional current promoted-skill definition fingerprint guard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_fingerprint: Option<String>,
}

/// Promoted-learning skill revocation request payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeLearningSkillRequest {
    /// Optional human-readable revocation reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Promoted-learning skill rollback request payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackLearningSkillRequest {
    /// Optional human-readable rollback reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Board creation request payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateBoardRequest {
    /// Optional caller-supplied stable board identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_id: Option<String>,
    /// The user-visible board name.
    pub display_name: String,
    /// The owning session when the board should follow one agent conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    /// Optional caller-supplied board metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// Board update request payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateBoardRequest {
    /// Optional replacement user-visible board name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Optional replacement board metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// Board-revision creation request payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateBoardRevisionRequest {
    /// The parent revision expected by the caller for linear history updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_revision_id: Option<String>,
    /// Optional caller-scoped idempotency key for this board revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_revision_id: Option<String>,
    /// The daemon-owned rendered asset identifier used for multimodal inputs.
    pub render_asset_id: String,
    /// The optional daemon-owned structured state asset identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_asset_id: Option<String>,
    /// Optional user-visible note associated with the revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The originating session when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_id: Option<String>,
    /// The originating run when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_run_id: Option<String>,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// Channel-member upsert payload used by create/update APIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelMemberRequest {
    /// The stable member identifier inside the channel namespace.
    pub member_id: String,
    /// The member kind stored in the channel roster.
    pub member_kind: crate::ChannelMemberKind,
    /// Optional display-name synchronization mode for session-backed members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name_mode: Option<crate::ChannelMemberDisplayNameMode>,
    /// The human-readable member display name.
    pub display_name: String,
    /// The bound daemon session identifier when the member is session-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The bound daemon actor identifier when the member is human-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    /// Optional role label used by channel arbitration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Optional expertise tags used by channel arbitration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expertise_tags: Vec<String>,
    /// The durable participation mode for the member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub participation_mode: Option<crate::ChannelParticipationMode>,
    /// Whether autonomous speaking is muted for this member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
}

/// Project-member upsert payload used by create and update APIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMemberRequest {
    /// The stable member identifier inside the project namespace.
    pub member_id: String,
    /// The durable member kind stored in the project roster.
    pub member_kind: crate::ChannelMemberKind,
    /// The human-readable member display name.
    pub display_name: String,
    /// The bound daemon session identifier when the member is session-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The live daemon agent identifier used to resolve a session-backed member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// The bound daemon actor identifier when the member is human-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    /// Optional role label used for assignment and routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Optional expertise tags used for filtering and automation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expertise_tags: Vec<String>,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// Channel creation request payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreateChannelRequest {
    /// Optional caller-supplied stable channel identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    /// The user-visible channel title.
    pub title: String,
    /// Optional short description shown in channel lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional longer purpose text shown in channel details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// The daemon-owned pinned asset identifiers attached to the channel header.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_asset_ids: Vec<String>,
    /// Optional stable creator actor identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    /// The initial channel members.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ChannelMemberRequest>,
    /// Optional replacement autonomy policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autonomy_policy: Option<crate::ChannelAutonomyPolicy>,
    /// The default participation mode for new channel members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_participation_mode: Option<crate::ChannelParticipationMode>,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// Channel stimulus creation payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreateChannelStimulusRequest {
    /// The durable scope that constrains where the daemon may surface the stimulus.
    #[serde(default)]
    pub scope: crate::ChannelStimulusScope,
    /// The canonical thread targeted by the stimulus when it is thread-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_root_message_id: Option<String>,
    /// The semantic kind used for dedupe, policy, and rendering.
    pub kind: crate::ChannelStimulusKind,
    /// The preferred public presentation surface for the canonical marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility_hint: Option<crate::ChannelStimulusVisibilityHint>,
    /// The public text that should be materialized before channel arbitration continues.
    pub content: String,
    /// The members explicitly preferred when the stimulus opens a new social turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addressed_member_ids: Vec<String>,
    /// The optional session that requested the stimulus and should appear as the sender.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_session_id: Option<String>,
    /// The optional non-session actor identifier used for the sender.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_actor_id: Option<String>,
    /// The optional non-session sender display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_display_name: Option<String>,
    /// The caller-reported source kind such as `schedule`, `agent_idea`, or `review`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    /// The stable source reference such as a schedule id or review run id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    /// One optional dedupe key used to coalesce equivalent queued stimuli.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    /// One optional progress key used to supersede stale progress updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_key: Option<String>,
    /// The earliest timestamp when the worker may process the stimulus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_at_ms: Option<u64>,
    /// The expiration timestamp after which the stimulus should be canceled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// Optional caller-supplied metadata stored with the stimulus.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// Channel update request payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UpdateChannelRequest {
    /// Optional replacement channel title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional replacement short description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional replacement longer purpose text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// Optional full replacement pinned asset list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_asset_ids: Option<Vec<String>>,
    /// Optional replacement autonomy policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autonomy_policy: Option<crate::ChannelAutonomyPolicy>,
    /// Optional replacement default participation mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_participation_mode: Option<crate::ChannelParticipationMode>,
    /// Whether autonomous speaking is currently paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<bool>,
    /// Optional replacement metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// Project-channel link creation or update payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectChannelLinkRequest {
    /// The daemon-owned channel identifier.
    pub channel_id: String,
    /// Optional semantic role of the channel inside the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Whether this channel should become the default discussion target for new tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_for_new_tasks: Option<bool>,
    /// Whether operators want project members mirrored into this channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_members: Option<bool>,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// Project creation request payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreateProjectRequest {
    /// Optional caller-supplied stable project identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// The user-visible project name.
    pub display_name: String,
    /// Optional short project description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional replacement initial lifecycle state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<crate::ProjectStatus>,
    /// The initial project members.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ProjectMemberRequest>,
    /// The initial linked channels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channel_links: Vec<ProjectChannelLinkRequest>,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// Project update request payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UpdateProjectRequest {
    /// Optional replacement project display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Optional replacement project description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional replacement project lifecycle state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<crate::ProjectStatus>,
    /// Optional replacement metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// Project-task assignment payload accepted by create and update APIs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectTaskAssignmentRequest {
    /// Optional direct project member identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee_member_id: Option<String>,
    /// Optional project-member session identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee_session_id: Option<String>,
    /// Optional live daemon agent identifier resolved to a project member session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee_agent_id: Option<String>,
}

/// Project-task creation request payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreateProjectTaskRequest {
    /// Optional caller-supplied stable project-task identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_task_id: Option<String>,
    /// The user-visible project-task title.
    pub title: String,
    /// The longer operator-facing project-task description.
    #[serde(default)]
    pub description: String,
    /// Optional replacement initial lifecycle state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    /// Optional durable assignment payload.
    #[serde(default)]
    pub assignment: ProjectTaskAssignmentRequest,
    /// The linked project-task discussion channel when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discussion_channel_id: Option<String>,
    /// The linked project-task discussion thread root when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discussion_thread_root_message_id: Option<String>,
    /// The task identifiers that currently block this project task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
    /// Optional latest run identifier associated with this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_run_id: Option<String>,
    /// Optional output or conclusion captured at creation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// Project-task update request payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UpdateProjectTaskRequest {
    /// Optional replacement project-task title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional replacement project-task description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional replacement lifecycle state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    /// Optional replacement assignment payload.
    #[serde(default)]
    pub assignment: ProjectTaskAssignmentRequest,
    /// Whether the current assignment should be cleared.
    #[serde(default)]
    pub clear_assignment: bool,
    /// Optional replacement linked discussion channel identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discussion_channel_id: Option<String>,
    /// Optional replacement linked discussion thread root identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discussion_thread_root_message_id: Option<String>,
    /// Whether the current linked discussion should be cleared.
    #[serde(default)]
    pub clear_discussion: bool,
    /// Optional full replacement dependency list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<Vec<String>>,
    /// Optional replacement latest run identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_run_id: Option<String>,
    /// Optional replacement output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Whether the current output should be cleared.
    #[serde(default)]
    pub clear_output: bool,
    /// Optional replacement metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// Project-task start request payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StartProjectTaskRequest {
    /// Optional provider override for the spawned run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Optional model override for the spawned run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Optional replacement public kickoff message used when the daemon creates a new discussion thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kickoff_message: Option<String>,
    /// Optional caller-supplied metadata attached to the spawned run request.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// Channel message creation request payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PostChannelMessageRequest {
    /// The stable sender actor identifier.
    pub sender_actor_id: String,
    /// Optional human-readable sender display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_display_name: Option<String>,
    /// The sender session identifier when the sender is session-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_session_id: Option<String>,
    /// The thread root identifier when posting inside one thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_root_message_id: Option<String>,
    /// The direct parent message identifier when posting one reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
    /// Optional ordered addressed members that should receive the first public turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addressed_member_ids: Vec<String>,
    /// Ordered multimodal public content parts for the message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_items: Vec<SubmitInputItemRequest>,
    /// Optional plain-text fallback used when no input items are supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// Channel reaction creation or replacement request payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetChannelReactionRequest {
    /// The stable actor identifier applying the reaction.
    pub actor_id: String,
    /// The emoji or reaction token to apply.
    pub emoji: String,
}

/// Compact asset summary returned by asset listing endpoints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetSummaryView {
    /// The stable daemon-owned asset identifier.
    pub asset_id: String,
    /// The normalized MIME type stored for the asset.
    pub media_type: String,
    /// The original file name recorded for the asset.
    pub file_name: String,
    /// The normalized SHA-256 digest of the stored raw payload.
    pub sha256: String,
    /// The stored raw payload size in bytes.
    pub byte_length: u64,
    /// The asset creation timestamp in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
}

/// Full asset view returned by asset detail endpoints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetView {
    /// The stable daemon-owned asset identifier.
    pub asset_id: String,
    /// The normalized MIME type stored for the asset.
    pub media_type: String,
    /// The original file name recorded for the asset.
    pub file_name: String,
    /// The normalized SHA-256 digest of the stored raw payload.
    pub sha256: String,
    /// The stored raw payload size in bytes.
    pub byte_length: u64,
    /// The asset creation timestamp in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// The opaque daemon storage URI for the raw payload.
    pub uri: String,
    /// The opaque daemon storage URI for derived plain text when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_uri: Option<String>,
    /// The SHA-256 digest of derived plain text when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_sha256: Option<String>,
    /// The byte length of derived plain text when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_byte_length: Option<u64>,
    /// The opaque daemon storage URI for a derived visual preview when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_image_uri: Option<String>,
    /// The MIME type for the derived visual preview when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_image_media_type: Option<String>,
    /// The SHA-256 digest of the derived visual preview when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_image_sha256: Option<String>,
    /// The byte length of the derived visual preview when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_image_byte_length: Option<u64>,
    /// Derivations whose result points at this asset.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derivation_ids: Vec<String>,
    /// Durable provenance records for daemon-produced assets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<AssetProvenanceView>,
}

/// One source asset referenced by asset provenance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetProvenanceSourceView {
    /// The source daemon asset identifier.
    pub asset_id: String,
    /// The source asset media type at dispatch time.
    pub media_type: String,
    /// The source asset raw SHA-256 at dispatch time.
    pub sha256: String,
}

/// One durable provenance record exposed by asset detail endpoints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetProvenanceView {
    /// Provider-neutral producer kind.
    pub kind: String,
    /// Tool name that produced the asset.
    pub tool_name: String,
    /// Session that requested the asset when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Run that requested the asset when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Model tool-call id that requested the asset when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Selected daemon media route id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
    /// Backend provider that produced the bytes.
    pub provider: String,
    /// Concrete backend model that produced the bytes.
    pub model: String,
    /// SHA-256 of the prompt/instruction text.
    pub prompt_sha256: String,
    /// Ordered source assets used by edit-style producers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_assets: Vec<AssetProvenanceSourceView>,
    /// 1-based index of this output within the provider batch.
    pub output_index: u32,
    /// Total number of outputs in the provider batch.
    pub output_count: u32,
}

/// One durable place that currently references a daemon-owned asset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetReferenceView {
    /// The owning subsystem, such as `runs`, `observations`, `boards`, or `channels`.
    pub domain: String,
    /// The stable owner record identifier in that subsystem.
    pub owner_id: String,
    /// The reference role inside the owner record.
    pub role: String,
    /// Whether this reference must block physical asset deletion.
    pub hard: bool,
    /// Optional parent scope such as a session, board, channel, or source identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Optional extra identifier such as a run output index or channel message id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_id: Option<String>,
}

/// Full reference report for one daemon-owned asset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetReferencesView {
    /// The inspected asset identifier.
    pub asset_id: String,
    /// The number of hard references that should block physical deletion.
    pub hard_reference_count: usize,
    /// The number of soft references retained only for lineage/audit.
    pub soft_reference_count: usize,
    /// All discovered references sorted by subsystem and owner.
    pub references: Vec<AssetReferenceView>,
}

/// One file included in an asset deletion or GC plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetDeletionFileView {
    /// Logical file role, such as `raw`, `derived_text`, `preview`, or `metadata`.
    pub kind: String,
    /// Opaque daemon storage URI for payload files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// Current on-disk byte length.
    pub byte_length: u64,
    /// Whether the file exists at planning time.
    pub exists: bool,
}

/// Result of planning or executing deletion for one asset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetDeletionPlanView {
    /// The inspected asset identifier.
    pub asset_id: String,
    /// Whether the endpoint executed removal or only planned it.
    pub dry_run: bool,
    /// Whether hard references prevented physical deletion.
    pub blocked: bool,
    /// Whether this call physically deleted the asset metadata/payload files.
    pub deleted: bool,
    /// Sum of existing daemon-owned files that are removable for this asset.
    pub reclaimable_bytes: u64,
    /// Number of hard references that block deletion.
    pub hard_reference_count: usize,
    /// Number of soft lineage references that do not block deletion.
    pub soft_reference_count: usize,
    /// Files that would be or were removed.
    pub files: Vec<AssetDeletionFileView>,
    /// References discovered during planning.
    pub references: Vec<AssetReferenceView>,
}

/// Result of planning or executing asset garbage collection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetGcPlanView {
    /// Whether this was inspect-only.
    pub dry_run: bool,
    /// Number of assets inspected.
    pub inspected_count: usize,
    /// Number of assets without hard references.
    pub candidate_count: usize,
    /// Number of assets blocked by hard references.
    pub blocked_count: usize,
    /// Number of assets physically deleted by this call.
    pub deleted_count: usize,
    /// Number of orphan payload files found outside the loaded asset catalog.
    #[serde(default)]
    pub orphan_file_count: usize,
    /// Number of orphan payload files physically deleted by this call.
    #[serde(default)]
    pub orphan_deleted_count: usize,
    /// Sum of reclaimable bytes across catalog candidates and orphan files.
    pub reclaimable_bytes: u64,
    /// Sum of reclaimable bytes from orphan payload files only.
    #[serde(default)]
    pub orphan_reclaimable_bytes: u64,
    /// Per-asset plans sorted by asset id.
    pub plans: Vec<AssetDeletionPlanView>,
    /// Orphan payload files sorted by storage kind and URI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orphan_files: Vec<AssetDeletionFileView>,
}

/// Loaded skill registry summary exposed through runtime settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSkillsView {
    pub loaded_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Skill list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillListQuery {
    /// Filters skills by name, description, or path substring.
    pub query: Option<String>,
}

/// Runtime configuration exposed for a skill.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRuntimeView {
    /// Tools that the skill explicitly allows in addition to the ambient surface.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
    /// Tools that the skill explicitly suppresses while it is active.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_tools: Vec<String>,
    /// The execution context requested by the skill.
    #[serde(default)]
    pub context: SkillExecutionContext,
    /// The optional agent profile override requested by the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_profile: Option<String>,
    /// The optional primary provider override requested by the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The optional primary model override requested by the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The optional fallback model override requested by the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
}

/// Compact skill summary returned by listing endpoints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummaryView {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub skill_path: String,
    pub skill_root: String,
    pub scope: SkillScope,
    pub digest: String,
    pub runtime: SkillRuntimeView,
}

/// Full skill view returned by detail endpoints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillView {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub skill_path: String,
    pub skill_root: String,
    pub scope: SkillScope,
    pub digest: String,
    pub runtime: SkillRuntimeView,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instructions: String,
}

/// Runtime skill creation payload written to the daemon-managed skill root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRuntimeSkillRequest {
    /// The directory-safe skill name (lowercase ascii alphanumerics or `-`).
    pub name: String,
    /// The one-line skill description shown in catalogs.
    pub description: String,
    /// The markdown instructions injected when the skill activates.
    pub instructions: String,
    /// The optional one-line activation hint shown in catalogs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    /// The optional skill version label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Session creation request payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    /// The optional caller-selected session identifier.
    pub session_id: Option<String>,
    /// The optional provider-side thread identifier to bind to the session.
    pub thread_id: Option<String>,
    /// The optional persona identifier bound to the session at creation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona_id: Option<String>,
    /// The optional capability scope snapshot persisted with the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_scope: Option<CapabilityScope>,
    /// The optional credential scope snapshot persisted with the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_scope: Option<CredentialScope>,
}

/// Session reply-target mutation payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SetSessionReplyTargetsRequest {
    #[serde(default)]
    pub reply_targets: Vec<SessionReplyTargetRequest>,
}

/// Explicit session reply-target defaults projected through the control plane.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReplyTargetsView {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reply_targets: Vec<ReplyHandle>,
}

/// Session operator-contact mutation payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSessionOperatorConfigRequest {
    #[serde(flatten)]
    pub operator: SessionOperatorConfig,
}

/// Session tool-overrides update payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSessionToolOverridesRequest {
    #[serde(flatten)]
    pub tool_overrides: kheish_types::SessionToolOverrides,
}

/// Session structured-output-contract update payload. The schema is standard
/// JSON Schema restricted to the enforceable subset; unsupported keywords are
/// rejected with their paths rather than silently dropped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSessionOutputContractRequest {
    /// The JSON Schema the session's final answers must match.
    pub schema: Value,
    /// Optional corrective-turn budget (clamped by the daemon).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_repair_attempts: Option<u8>,
}

/// Session input-contract update payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSessionInputContractRequest {
    /// The JSON Schema every submitted payload must match.
    pub schema: Value,
}

/// Structured input contract projected through the control plane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredInputContractView {
    /// Canonical JSON Schema rendering of the enforced schema.
    pub schema: Value,
}

impl From<&kheish_types::StructuredInputContract> for StructuredInputContractView {
    fn from(contract: &kheish_types::StructuredInputContract) -> Self {
        Self {
            schema: contract.schema.to_json_schema(),
        }
    }
}

/// Structured output contract projected through the control plane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredOutputContractView {
    /// Canonical JSON Schema rendering of the enforced schema.
    pub schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_repair_attempts: Option<u8>,
}

impl From<&kheish_types::StructuredOutputContract> for StructuredOutputContractView {
    fn from(contract: &kheish_types::StructuredOutputContract) -> Self {
        Self {
            schema: contract.schema.to_json_schema(),
            max_repair_attempts: contract.max_repair_attempts,
        }
    }
}

/// Session operator-contact policy projected through the control plane.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOperatorConfigView {
    #[serde(flatten)]
    pub operator: SessionOperatorConfig,
}

/// One session reply target declared through the control plane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionReplyTargetRequest {
    Raw {
        plugin: String,
        address: String,
    },
    External {
        connector: String,
        route: String,
    },
    Telegram {
        connector: String,
        chat_id: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_thread_id: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to_message_id: Option<i64>,
    },
    Slack {
        connector: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enterprise_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        team_id: Option<String>,
        channel_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_ts: Option<String>,
    },
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
    },
}

impl SessionReplyTargetRequest {
    pub(crate) fn into_reply_handle(self) -> ReplyHandle {
        match self {
            SessionReplyTargetRequest::Raw { plugin, address } => ReplyHandle { plugin, address },
            SessionReplyTargetRequest::External { connector, route } => ReplyHandle {
                plugin: "external".to_string(),
                address: encode_external_reply_route(&ExternalReplyRoute { connector, route }),
            },
            SessionReplyTargetRequest::Telegram {
                connector,
                chat_id,
                message_thread_id,
                reply_to_message_id,
            } => ReplyHandle {
                plugin: "telegram".to_string(),
                address: encode_telegram_reply_route(&TelegramReplyRoute {
                    connector,
                    chat_id,
                    message_thread_id,
                    reply_to_message_id,
                }),
            },
            SessionReplyTargetRequest::Slack {
                connector,
                enterprise_id,
                team_id,
                channel_id,
                thread_ts,
            } => ReplyHandle {
                plugin: "slack".to_string(),
                address: encode_slack_reply_route(&SlackReplyRoute {
                    connector,
                    enterprise_id,
                    team_id,
                    channel_id,
                    thread_ts,
                }),
            },
            SessionReplyTargetRequest::Http { url, headers } => ReplyHandle {
                plugin: "http".to_string(),
                address: encode_http_reply_route(&HttpReplyRoute {
                    url,
                    allow_private_network: false,
                    headers,
                }),
            },
        }
    }
}

fn deserialize_reply_targets<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<ReplyHandle>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<Value>::deserialize(deserializer)?;
    values
        .into_iter()
        .map(|value| {
            if value.get("type").is_some() {
                return serde_json::from_value::<SessionReplyTargetRequest>(value)
                    .map(SessionReplyTargetRequest::into_reply_handle)
                    .map_err(serde::de::Error::custom);
            }
            serde_json::from_value::<ReplyHandle>(value).map_err(serde::de::Error::custom)
        })
        .collect()
}

/// One write-only connector secret binding accepted by create/update APIs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorSecretInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
}

/// One per-team Slack bot token binding accepted by create/update APIs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutSlackTeamBotTokenRequest {
    pub team_id: String,
    pub bot_token: ConnectorSecretInput,
}

/// One redacted per-team Slack bot token projected through the control plane.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackTeamBotTokenView {
    pub team_id: String,
    pub bot_token: ConnectorSecretView,
}

/// One redacted connector secret binding projected through the control plane.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorSecretView {
    pub configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
}

/// The source that owns one connector definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorSourceView {
    File,
    Daemon,
}

/// Telegram connector create/update payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PutExternalConnectorRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<crate::ExternalConnectorMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_private_network: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_token: Option<ConnectorSecretInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_unauthenticated_ingress: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_self_output: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_reply_targets: Option<Vec<ReplyHandle>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_binding_keys: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_policy: Option<ConnectorSessionPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_events_per_second: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_process: Option<crate::ExternalChildProcessConfig>,
}

/// Telegram connector create/update payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PutTelegramConnectorRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<ConnectorSecretInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_token: Option<ConnectorSecretInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_unauthenticated_ingress: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    #[serde(default)]
    pub ingress_mode: crate::TelegramIngressMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polling_timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_events_per_second: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_chat_ids: Option<Vec<i64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_self_output: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_reply_targets: Option<Vec<ReplyHandle>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_binding_keys: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_policy: Option<ConnectorSessionPolicy>,
}

/// Slack connector create/update payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PutSlackConnectorRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<ConnectorSecretInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_secret: Option<ConnectorSecretInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_unauthenticated_ingress: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_self_output: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_reply_targets: Option<Vec<ReplyHandle>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_binding_keys: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_policy: Option<ConnectorSessionPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_events_per_second: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_api_app_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_enterprise_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_team_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_channel_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_file_hosts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_bot_tokens: Option<Vec<PutSlackTeamBotTokenRequest>>,
}

/// HTTP connector create/update payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PutHttpConnectorRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<ConnectorSecretInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hmac_secret: Option<ConnectorSecretInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_unauthenticated_ingress: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_hmac_signature: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_max_age_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_idempotency_key: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_events_per_second: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_payload_reply_targets: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reply_targets: Option<Vec<ReplyHandle>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_binding_keys: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_policy: Option<ConnectorSessionPolicy>,
}

/// One projected Telegram connector.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExternalConnectorView {
    pub source: ConnectorSourceView,
    pub name: String,
    pub platform: String,
    pub mode: crate::ExternalConnectorMode,
    pub base_url: String,
    pub allow_private_network: bool,
    pub shared_token: ConnectorSecretView,
    pub allow_unauthenticated_ingress: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_session_id: Option<String>,
    pub include_self_output: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_reply_targets: Vec<ReplyHandle>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_binding_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "ConnectorSessionPolicy::is_empty")]
    pub session_policy: ConnectorSessionPolicy,
    pub ingress_events_per_second: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_process: Option<ExternalChildProcessView>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExternalChildProcessView {
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_env_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

/// One projected Telegram connector.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TelegramConnectorView {
    pub source: ConnectorSourceView,
    pub name: String,
    pub bot_token: ConnectorSecretView,
    pub secret_token: ConnectorSecretView,
    pub allow_unauthenticated_ingress: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    pub ingress_mode: crate::TelegramIngressMode,
    pub polling_timeout_seconds: u64,
    pub ingress_events_per_second: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_chat_ids: Vec<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_session_id: Option<String>,
    pub include_self_output: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_reply_targets: Vec<ReplyHandle>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_binding_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "ConnectorSessionPolicy::is_empty")]
    pub session_policy: ConnectorSessionPolicy,
}

/// One projected Slack connector.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlackConnectorView {
    pub source: ConnectorSourceView,
    pub name: String,
    pub bot_token: ConnectorSecretView,
    pub signing_secret: ConnectorSecretView,
    pub allow_unauthenticated_ingress: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_session_id: Option<String>,
    pub include_self_output: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_reply_targets: Vec<ReplyHandle>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_binding_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "ConnectorSessionPolicy::is_empty")]
    pub session_policy: ConnectorSessionPolicy,
    pub ingress_events_per_second: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_api_app_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_enterprise_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_team_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_channel_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_file_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub team_bot_tokens: Vec<SlackTeamBotTokenView>,
}

/// One projected HTTP connector.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HttpConnectorView {
    pub source: ConnectorSourceView,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    pub bearer_token: ConnectorSecretView,
    pub hmac_secret: ConnectorSecretView,
    pub allow_unauthenticated_ingress: bool,
    pub require_hmac_signature: bool,
    pub signature_max_age_secs: u64,
    pub require_idempotency_key: bool,
    pub ingress_events_per_second: u32,
    pub allow_payload_reply_targets: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_reply_targets: Vec<ReplyHandle>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_binding_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "ConnectorSessionPolicy::is_empty")]
    pub session_policy: ConnectorSessionPolicy,
}

/// One connector projected through the runtime control plane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConnectorView {
    External(ExternalConnectorView),
    Telegram(TelegramConnectorView),
    Slack(SlackConnectorView),
    Http(HttpConnectorView),
}

/// Persona list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaListQuery {
    /// Filters personas by identifier or display name substring.
    pub query: Option<String>,
    pub page: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

impl PersonaListQuery {
    pub fn page_query(&self) -> ListPageQuery {
        list_page_query(self.page, self.cursor.clone())
    }
}

/// Persona creation request payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreatePersonaRequest {
    /// The optional caller-selected persona identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona_id: Option<String>,
    /// The user-visible persona name.
    pub display_name: String,
    /// The exact persona instructions captured for future sessions.
    pub soul: String,
    /// Optional caller-defined metadata stored with the persona.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// The optional capability baseline inherited by future bound sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_scope: Option<CapabilityScope>,
    /// Default inline skills resolved and frozen into future session bindings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_skills: Option<Vec<PersonaSkillAssignment>>,
}

/// Persona update request payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UpdatePersonaRequest {
    /// The updated user-visible persona name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The updated persona instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soul: Option<String>,
    /// Optional caller-defined metadata replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// Optional capability baseline replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_scope: Option<CapabilityScope>,
    /// Optional default inline skill replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_skills: Option<Vec<PersonaSkillAssignment>>,
}

/// Session persona mutation payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetSessionPersonaRequest {
    /// The persona identifier that should be bound to the session.
    pub persona_id: String,
}

/// Session capability-scope mutation payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSessionCapabilityScopeRequest {
    /// Replaces the persisted session capability scope. `null` clears the scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_scope: Option<CapabilityScope>,
}

/// Session credential-scope mutation payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSessionCredentialScopeRequest {
    /// Replaces the persisted session credential scope. `null` clears the scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_scope: Option<CredentialScope>,
}

/// Compact persona summary returned by listing endpoints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaSummaryView {
    pub persona_id: String,
    pub display_name: String,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Full persona view returned by detail endpoints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaView {
    pub persona_id: String,
    pub display_name: String,
    pub soul: String,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "CapabilityScope::is_empty")]
    pub capability_scope: CapabilityScope,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_skills: Vec<PersonaSkillAssignment>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// Compact session persona summary projected from one bound persona snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPersonaSummaryView {
    pub persona_id: String,
    pub persona_version: u64,
    pub display_name: String,
    pub bound_at_ms: u64,
}

/// Session route policy mutation payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SetSessionRoutePolicyRequest {
    /// Replaces the persisted session route policy. `null` clears the policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_policy: Option<SessionRoutePolicy>,
}

/// Session input submission payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubmitInputRequest {
    /// Optional route override for this run. `provider` is kept as the legacy wire name.
    #[serde(default, alias = "route_id", skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The ingress plugin identifier that produced the input.
    pub source_plugin: Option<String>,
    /// The ingress source kind associated with the input.
    pub source_kind: Option<String>,
    /// The actor identifier recorded for the input.
    pub actor_id: Option<String>,
    /// The legacy free-form text body used when `input_items` is empty.
    #[serde(default)]
    pub content: String,
    /// The ordered multimodal input sequence. This must not be combined with `content` or `attachments`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_items: Vec<SubmitInputItemRequest>,
    /// The legacy attachment list appended after `content` when `input_items` is empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<InputAttachmentRequest>,
    /// The run-scoped generation overrides.
    pub generation: Option<ModelGenerationConfig>,
    /// Optional deterministic completion requirements enforced by the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_requirements: Option<Vec<CompletionRequirement>>,
    /// Arbitrary caller metadata attached to the normalized input envelope.
    pub metadata: Option<Value>,
    /// Binding keys that should follow the session for later routing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub binding_keys: Vec<String>,
    /// Explicit output targets for this input.
    #[serde(
        default,
        deserialize_with = "deserialize_reply_targets",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub reply_targets: Vec<ReplyHandle>,
    /// Deprecated convenience output plugin selector kept for compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_plugin: Option<String>,
    /// Deprecated convenience output address kept for compatibility.
    pub reply_address: Option<String>,
}

/// Direct run submission payload.
///
/// This accepts the historical flat `SubmitInputRequest` shape and adds an
/// optional idempotency key scoped to `POST /v1/sessions/{session_id}/runs`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubmitRunRequest {
    /// Optional caller key used to deduplicate direct run submissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// The actual input request fields.
    #[serde(flatten)]
    pub request: SubmitInputRequest,
}

/// Approval resolution request payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolveApprovalsRequest {
    /// Optional caller key used to deduplicate approval resolution submissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub resolutions: Vec<ApprovalResolution>,
}

/// User question resolution request payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolveUserQuestionRequest {
    /// Optional caller key used to deduplicate user-question resolution submissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub resolution: UserQuestionResolution,
}

/// Request-scoped user-question cancellation payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CancelUserQuestionRequest {
    /// Optional caller key used to deduplicate request-scoped question cancellation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Optional operator note used when declining a parent clarification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
}

/// Session end request payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EndSessionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Sidechain subtask definition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SidechainSubtaskRequest {
    /// The short human-readable child task name.
    pub name: String,
    /// The detailed child task description.
    pub description: String,
    /// The legacy plain-text task body used when `input_items` is empty.
    #[serde(default)]
    pub content: String,
    /// The ordered multimodal child input sequence. This must not be combined with `content`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_items: Vec<SubmitInputItemRequest>,
    /// The legacy attachment list appended after `content` when `input_items` is empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<InputAttachmentRequest>,
}

/// Sidechain spawn request payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpawnSidechainRequest {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    /// Preferred child-session route policy. When present, this supersedes the legacy provider/model fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_policy: Option<SessionRoutePolicy>,
    #[serde(default, alias = "route_id", skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<ChildRetentionPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", skip_deserializing)]
    pub spawned_by_run_id: Option<String>,
    pub fork_context: ForkContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<ModelGenerationConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_surface: Option<ToolSurfaceFilter>,
    /// Optional child capability-scope restriction intersected with the parent session scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_scope: Option<CapabilityScope>,
    /// Optional child credential-scope restriction intersected with the parent session scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_scope: Option<CredentialScope>,
    pub subtask: Option<SidechainSubtaskRequest>,
}

/// Cross-agent mailbox post payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PostMailboxRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub from_agent_id: String,
    pub to_agent_id: String,
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    pub payload: Value,
}

/// Mailbox post acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostMailboxResponse {
    pub accepted: bool,
    pub message_id: String,
    #[serde(default)]
    pub duplicate: bool,
}

/// Mailbox message acknowledgement result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckMailboxResponse {
    pub acknowledged: bool,
    pub agent_id: String,
    pub message_id: String,
}

/// Agent nickname mutation payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetAgentNicknameRequest {
    pub nickname: String,
}

/// Lightweight agent inventory row for list-style control-plane views.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSummaryView {
    /// Stable agent identifier.
    pub agent_id: String,
    /// Optional parent agent identifier for sidechains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    /// Session identifier owned by the agent runtime.
    pub session_id: String,
    /// Optional thread identifier associated with the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Stable machine-friendly name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Stable hierarchy path rooted at the root agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Optional human-readable nickname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    /// Last persisted lifecycle state.
    pub status: AgentStatus,
    /// Post-settlement retention policy.
    pub retention: ChildRetentionPolicy,
    /// Originating parent run, when spawned from a tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_by_run_id: Option<String>,
    /// Spawn idempotency key, when supplied by the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_request_id: Option<String>,
    /// Creation timestamp in milliseconds since the Unix epoch.
    pub spawned_at_ms: u64,
    /// Settlement timestamp in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at_ms: Option<u64>,
    /// Runtime closure timestamp in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at_ms: Option<u64>,
    /// Sidechain session identifier for forked agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidechain_session_id: Option<String>,
    /// Number of subtasks attached to this agent record.
    pub subtask_count: usize,
    /// Number of pending mailbox messages addressed to this agent.
    pub mailbox_message_count: usize,
    /// Whether the daemon currently has a live runtime actor for this agent.
    pub has_runtime: bool,
    /// Active daemon run for this agent session, when one is currently claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run_id: Option<String>,
    /// Number of queued daemon runs behind the active run for this agent session.
    #[serde(default)]
    pub queued_run_count: usize,
    /// Pending approval decisions known from the active daemon run.
    #[serde(default)]
    pub pending_approval_count: usize,
    /// Pending structured user questions known from the active daemon run.
    #[serde(default)]
    pub pending_question_count: usize,
    /// Parent clarification runs currently waiting on this agent's behalf.
    #[serde(default)]
    pub pending_parent_clarification_count: usize,
    /// Bounded identifiers for parent clarification runs currently waiting on this agent's behalf.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_parent_clarification_run_ids: Vec<String>,
    /// Most recent run error for this agent session, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Bounded preview of the most recent daemon output for this agent session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_preview: Option<String>,
    /// Whether `last_output_preview` was truncated from a larger output.
    #[serde(default, skip_serializing_if = "is_false")]
    pub last_output_truncated: bool,
    /// Latest agent or run activity timestamp in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at_ms: Option<u64>,
}

/// Agent summary list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSummaryListQuery {
    #[serde(default, alias = "root")]
    pub root_agent_id: Option<String>,
    pub session_id: Option<String>,
    pub status: Option<AgentStatus>,
    pub has_runtime: Option<bool>,
    pub page: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

impl AgentSummaryListQuery {
    pub fn page_query(&self) -> ListPageQuery {
        list_page_query(self.page, self.cursor.clone())
    }

    #[must_use]
    pub fn matches_summary(&self, summary: &AgentSummaryView) -> bool {
        self.session_id
            .as_deref()
            .is_none_or(|session_id| summary.session_id == session_id)
            && self
                .status
                .as_ref()
                .is_none_or(|status| &summary.status == status)
            && self
                .has_runtime
                .is_none_or(|has_runtime| summary.has_runtime == has_runtime)
    }
}

/// Agent lifecycle audit query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAuditListQuery {
    pub agent_id: Option<String>,
}

/// Exact aggregate counts for one agent-summary list response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSummaryCountsView {
    /// Number of summaries in the selected root scope before non-root filters.
    pub total: usize,
    /// Number of summaries after all list filters and before cursor pagination.
    pub filtered: usize,
    pub idle: usize,
    pub running: usize,
    pub waiting_for_approval: usize,
    pub waiting_for_user_input: usize,
    pub failed: usize,
    pub completed: usize,
    pub with_runtime: usize,
    pub without_runtime: usize,
}

impl AgentSummaryCountsView {
    #[must_use]
    pub fn from_summaries(total: usize, summaries: &[AgentSummaryView]) -> Self {
        let mut counts = Self {
            total,
            filtered: summaries.len(),
            ..Default::default()
        };
        for summary in summaries {
            match summary.status {
                AgentStatus::Idle => counts.idle += 1,
                AgentStatus::Running => counts.running += 1,
                AgentStatus::WaitingForApproval => counts.waiting_for_approval += 1,
                AgentStatus::WaitingForUserInput => counts.waiting_for_user_input += 1,
                AgentStatus::Failed => counts.failed += 1,
                AgentStatus::Completed => counts.completed += 1,
            }
            if summary.has_runtime {
                counts.with_runtime += 1;
            } else {
                counts.without_runtime += 1;
            }
        }
        counts
    }
}

/// Cursor-paginated agent-summary response with exact filtered counts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSummaryListPage {
    pub items: Vec<AgentSummaryView>,
    pub pagination: ListPageMeta,
    pub counts: AgentSummaryCountsView,
}

impl AgentSummaryView {
    /// Builds a summary row from a stored agent record and cheap daemon overlays.
    pub fn from_record(
        record: &AgentRecord,
        mailbox_message_count: usize,
        has_runtime: bool,
    ) -> Self {
        Self {
            agent_id: record.id.0.clone(),
            parent_agent_id: record.parent.as_ref().map(|parent| parent.0.clone()),
            session_id: record.conversation.session_id.clone(),
            thread_id: record.conversation.thread_id.clone(),
            name: record.name.clone(),
            path: record.path.clone(),
            nickname: record.nickname.clone(),
            status: record.status.clone(),
            retention: record.retention.clone(),
            spawned_by_run_id: record.spawned_by_run_id.clone(),
            spawn_request_id: record.spawn_request_id.clone(),
            spawned_at_ms: record.spawned_at_ms,
            settled_at_ms: record.settled_at_ms,
            closed_at_ms: record.closed_at_ms,
            sidechain_session_id: record.sidechain_session_id.clone(),
            subtask_count: record.subtasks.len(),
            mailbox_message_count,
            has_runtime,
            active_run_id: None,
            queued_run_count: 0,
            pending_approval_count: 0,
            pending_question_count: 0,
            pending_parent_clarification_count: 0,
            pending_parent_clarification_run_ids: Vec::new(),
            last_error: None,
            last_output_preview: None,
            last_output_truncated: false,
            last_activity_at_ms: agent_record_activity_at_ms(record),
        }
    }
}

fn agent_record_activity_at_ms(record: &AgentRecord) -> Option<u64> {
    [
        Some(record.spawned_at_ms),
        record.settled_at_ms,
        record.closed_at_ms,
    ]
    .into_iter()
    .flatten()
    .filter(|timestamp| *timestamp > 0)
    .max()
}

/// Compact session summary returned by listing endpoints.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionViewSummary {
    pub session_id: String,
    pub agent_id: String,
    pub status: kheish_agent::AgentStatus,
    pub pending_approvals: usize,
    #[serde(default)]
    pub pending_questions: usize,
    #[serde(default, skip_serializing_if = "SessionRoutePolicy::is_empty")]
    pub route_policy: SessionRoutePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<SessionGoal>,
    /// Session-local capability-scope override persisted on the session itself.
    #[serde(default, skip_serializing_if = "CapabilityScope::is_empty")]
    pub capability_scope: CapabilityScope,
    /// Effective capability scope enforced at runtime after persona/session restriction.
    #[serde(default, skip_serializing_if = "CapabilityScope::is_empty")]
    pub effective_capability_scope: CapabilityScope,
    /// Session-local credential-scope override persisted on the session itself.
    #[serde(default, skip_serializing_if = "CredentialScope::is_empty")]
    pub credential_scope: CredentialScope,
    /// Effective credential scope enforced at runtime for auth-backed resources.
    #[serde(default, skip_serializing_if = "CredentialScope::is_empty")]
    pub effective_credential_scope: CredentialScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<SessionPersonaSummaryView>,
    #[serde(default, skip_serializing_if = "SessionOperatorConfig::is_inactive")]
    pub operator: SessionOperatorConfig,
    /// Native tool surface adjustments persisted on the session.
    #[serde(
        default,
        skip_serializing_if = "kheish_types::SessionToolOverrides::is_empty"
    )]
    pub tool_overrides: kheish_types::SessionToolOverrides,
    /// Structured input contract enforced on the session's submissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_contract: Option<StructuredInputContractView>,
    /// Structured output contract enforced on the session's runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_contract: Option<StructuredOutputContractView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reply_targets: Vec<ReplyHandle>,
}

/// Full session view returned by detail endpoints.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionView {
    pub session_id: String,
    pub agent_id: String,
    pub snapshot: ManagedAgentSnapshot,
    #[serde(default, skip_serializing_if = "SessionRoutePolicy::is_empty")]
    pub route_policy: SessionRoutePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<SessionGoal>,
    /// Session-local capability-scope override persisted on the session itself.
    #[serde(default, skip_serializing_if = "CapabilityScope::is_empty")]
    pub capability_scope: CapabilityScope,
    /// Effective capability scope enforced at runtime after persona/session restriction.
    #[serde(default, skip_serializing_if = "CapabilityScope::is_empty")]
    pub effective_capability_scope: CapabilityScope,
    /// Session-local credential-scope override persisted on the session itself.
    #[serde(default, skip_serializing_if = "CredentialScope::is_empty")]
    pub credential_scope: CredentialScope,
    /// Effective credential scope enforced at runtime for auth-backed resources.
    #[serde(default, skip_serializing_if = "CredentialScope::is_empty")]
    pub effective_credential_scope: CredentialScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<SessionPersonaSummaryView>,
    #[serde(default, skip_serializing_if = "SessionOperatorConfig::is_inactive")]
    pub operator: SessionOperatorConfig,
    /// Native tool surface adjustments persisted on the session.
    #[serde(
        default,
        skip_serializing_if = "kheish_types::SessionToolOverrides::is_empty"
    )]
    pub tool_overrides: kheish_types::SessionToolOverrides,
    /// Structured input contract enforced on the session's submissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_contract: Option<StructuredInputContractView>,
    /// Structured output contract enforced on the session's runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_contract: Option<StructuredOutputContractView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reply_targets: Vec<ReplyHandle>,
    pub outputs: Vec<DaemonOutputRecord>,
}

/// Full response for session goal endpoints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGoalResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<SessionGoal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_tokens: Option<u64>,
}

impl SessionGoalResponse {
    pub fn new(goal: Option<SessionGoal>) -> Self {
        let remaining_tokens = goal.as_ref().and_then(SessionGoal::remaining_tokens);
        Self {
            goal,
            remaining_tokens,
        }
    }
}

/// Replace/create one session goal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSessionGoalRequest {
    pub objective: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SessionGoalStatus>,
}

/// Patch one existing session goal.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchSessionGoalRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SessionGoalStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_token_budget: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_goal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_no_active_runs: Option<bool>,
}

/// Effective memory projection resolved for one session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionMemoryContextView {
    /// The session that owns this effective memory projection.
    pub session_id: String,
    /// Effective capability scope enforced for the session.
    #[serde(default, skip_serializing_if = "CapabilityScope::is_empty")]
    pub effective_capability_scope: CapabilityScope,
    /// Learning scopes visible to the session in retrieval order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub learning_scopes: Vec<LearningScope>,
    /// Prompt-eligible semantic memory currently projected for the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learned_context: Option<LearnedContextBundle>,
    /// Compact recovered episodic memory currently projected for the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovered_memory: Option<RecoveredMemoryBundle>,
    /// Skills currently visible to the session after capability and learning-scope filtering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visible_skills: Vec<SkillSummaryView>,
}

/// Query parameters accepted by the session memory-context endpoint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMemoryContextQuery {
    /// Optional lexical query used to preview prompt ordering for the next input.
    pub query: Option<String>,
}

/// Query parameters accepted by the session memory search endpoint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMemorySearchQuery {
    /// Optional lexical query. When omitted, the daemon returns a recent memory browse view.
    pub query: Option<String>,
    /// Optional result limit capped by the daemon.
    pub limit: Option<usize>,
}

/// The durable source class represented by one memory search result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMemorySearchResultKind {
    Learning,
    RecoveredRun,
    Skill,
}

/// One ranked memory search result resolved for a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMemorySearchResultView {
    /// Stable source class used to interpret the result.
    pub kind: SessionMemorySearchResultKind,
    /// Stable source identifier, such as a learning id, run id, or skill name.
    pub source_id: String,
    /// Operator-facing title summarizing the result.
    pub title: String,
    /// Compact excerpt used for browsing and debugging.
    pub excerpt: String,
    /// The lexical match score retained for deterministic ranking.
    pub score: u64,
    /// The primary timestamp retained for recency ordering.
    pub timestamp_ms: u64,
    /// Optional owning learning scope when the result is scope-bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<LearningScope>,
    /// Whether the result is currently prompt-eligible.
    #[serde(default)]
    pub prompt_eligible: bool,
    /// Optional durable publication status when the result comes from the learning plane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learning_status: Option<LearningStatus>,
    /// Optional publish tier when the result comes from the learning plane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_tier: Option<LearningPublishTier>,
    /// Optional verification status when the result comes from the learning plane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_status: Option<LearningVerificationStatus>,
    /// The record fields that matched the query when one was provided.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_fields: Vec<String>,
}

/// One bounded, query-aware memory search projection resolved for a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMemorySearchView {
    /// The session that owns the effective search scope.
    pub session_id: String,
    /// Effective capability scope enforced for the session.
    #[serde(default, skip_serializing_if = "CapabilityScope::is_empty")]
    pub effective_capability_scope: CapabilityScope,
    /// Learning scopes visible to the session in retrieval order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub learning_scopes: Vec<LearningScope>,
    /// The normalized query used to rank results, when one was provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Whether older results were omitted while trimming to the requested limit.
    #[serde(default)]
    pub truncated: bool,
    /// Ranked memory search results.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<SessionMemorySearchResultView>,
}

/// Pending user question projected from a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingQuestionView {
    pub session_id: String,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_kind: Option<crate::DaemonRunKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requester_project_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requester_channel_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_project_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_channel_ids: Vec<String>,
    pub request: UserQuestionRequest,
}

/// Static daemon capability advertisement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonCapabilities {
    pub control_plane_version: String,
    #[serde(default)]
    pub api_revision: u32,
    #[serde(default)]
    pub route_capability_matrix_version: u32,
    pub approvals: bool,
    pub sidechains: bool,
    pub mailboxes: bool,
    pub session_events: bool,
    pub restart_restore: bool,
    pub live_events: bool,
    #[serde(default)]
    pub session_run_idempotency: bool,
    #[serde(default)]
    pub playbooks: bool,
    #[serde(default)]
    pub flows: bool,
    #[serde(default)]
    pub problem_details: bool,
    #[serde(default)]
    pub openapi: bool,
    #[serde(default)]
    pub cursor_pagination: bool,
    #[serde(default)]
    pub paginated_lists: bool,
    #[serde(default)]
    pub domain_errors: bool,
    #[serde(default)]
    pub sse_replay: bool,
    #[serde(default)]
    pub typed_sse_heartbeat: bool,
    #[serde(default)]
    pub agent_supervisor_audit: bool,
    #[serde(default)]
    pub spawn_policies: bool,
}

/// Query parameters shared by SSE stream endpoints.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventStreamQuery {
    /// Optional event id cursor. Only events with larger ids are replayed.
    /// Encoded as a string because daemon SSE ids can exceed JavaScript's safe integer range.
    pub cursor: Option<String>,
    /// Optional global stream session filter.
    pub session_id: Option<String>,
    /// Optional global stream run filter.
    pub run_id: Option<String>,
}

/// Coarse daemon readiness state exposed by `/v1/status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonReadinessState {
    /// The daemon is accepting normal control-plane traffic.
    Ready,
    /// The daemon is intentionally draining and should not receive new work.
    Draining,
}

/// Cheap session counters for operator status.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonSessionStatusSummaryView {
    /// Number of sessions known by the daemon session index.
    pub total: usize,
}

/// Cheap run counters for operator status.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRunStatusSummaryView {
    /// Number of persisted daemon runs.
    pub total: usize,
    /// Number of runs queued behind active session work.
    pub queued: usize,
    /// Number of runs currently executing.
    pub running: usize,
    /// Number of runs paused on approval decisions.
    pub waiting_for_approval: usize,
    /// Number of runs paused on structured user questions.
    pub waiting_for_user_question: usize,
    /// Number of runs that completed successfully.
    pub completed: usize,
    /// Number of runs that failed.
    pub failed: usize,
    /// Number of runs interrupted before completion.
    pub interrupted: usize,
    /// Number of runs cancelled before completion.
    pub cancelled: usize,
    /// Total pending approval request identifiers attached to current runs.
    pub pending_approval_count: usize,
    /// Total pending user-question identifiers attached to current runs.
    pub pending_question_count: usize,
    /// Largest queued-run depth currently observed for a single session.
    pub max_session_queue_depth: usize,
    /// Identifier for the oldest queued run, when any exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_queued_run_id: Option<String>,
    /// Age in milliseconds for the oldest queued run, when any exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_queued_run_age_ms: Option<u64>,
    /// Age threshold used to warn about queued-run lag.
    #[serde(default)]
    pub queued_run_lag_threshold_ms: u64,
    /// Identifier for the oldest non-terminal run, when any exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_non_terminal_run_id: Option<String>,
    /// Age in milliseconds for the oldest non-terminal run, when any exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_non_terminal_run_age_ms: Option<u64>,
    /// Identifier for the non-terminal run with the oldest observed activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_idle_non_terminal_run_id: Option<String>,
    /// Milliseconds since the last observed activity for the idlest non-terminal run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_non_terminal_run_idle_ms: Option<u64>,
    /// Age threshold used to classify non-terminal runs as stale.
    #[serde(default)]
    pub stale_non_terminal_run_threshold_ms: u64,
    /// Number of non-terminal runs idle longer than `stale_non_terminal_run_threshold_ms`.
    #[serde(default)]
    pub stale_non_terminal_run_count: usize,
    /// Bounded sample of stale non-terminal run identifiers ordered by idle time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_non_terminal_run_ids: Vec<String>,
}

/// Cheap schedule counters for operator status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonScheduleStatusSummaryView {
    /// Whether the background worker that dispatches due schedules is running.
    #[serde(default = "default_true")]
    pub dispatch_worker_enabled: bool,
    /// Number of persisted schedules.
    pub total: usize,
    /// Number of active schedules.
    pub active: usize,
    /// Number of paused schedules.
    pub paused: usize,
    /// Number of completed schedules.
    pub completed: usize,
    /// Number of cancelled schedules.
    pub canceled: usize,
    /// Number of active schedules due at the snapshot timestamp.
    pub due_count: usize,
    /// Number of schedules currently deferred by retry backoff.
    pub backoff_count: usize,
    /// Number of schedules with an in-flight run marker.
    pub in_flight_count: usize,
    /// Number of schedules with a queued follow-up fire.
    pub queued_fire_count: usize,
    /// Earliest scheduler wake-up boundary after backoff is applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_due_at_ms: Option<u64>,
    /// Identifier for the active schedule whose due time is furthest in the past.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_due_schedule_id: Option<String>,
    /// Lag in milliseconds for `oldest_due_schedule_id`, when any schedule is overdue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_due_schedule_lag_ms: Option<u64>,
}

impl Default for DaemonScheduleStatusSummaryView {
    fn default() -> Self {
        Self {
            dispatch_worker_enabled: true,
            total: 0,
            active: 0,
            paused: 0,
            completed: 0,
            canceled: 0,
            due_count: 0,
            backoff_count: 0,
            in_flight_count: 0,
            queued_fire_count: 0,
            next_due_at_ms: None,
            oldest_due_schedule_id: None,
            oldest_due_schedule_lag_ms: None,
        }
    }
}

/// Cheap agent counters for operator status.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonAgentStatusSummaryView {
    /// Number of agent records known by the supervisor.
    pub total: usize,
    /// Number of live runtime handles currently registered.
    pub live_runtime_count: usize,
    /// Number of child agent records.
    pub sidechain_count: usize,
    /// Number of agent records whose runtime has been closed.
    pub closed_count: usize,
    /// Number of cached terminal snapshots for closed agents.
    pub terminal_snapshot_count: usize,
    /// Total pending mailbox messages across all agents.
    pub mailbox_message_count: usize,
    /// Number of idle agents.
    pub idle: usize,
    /// Number of running agents.
    pub running: usize,
    /// Number of agents waiting for approval decisions.
    pub waiting_for_approval: usize,
    /// Number of agents waiting for structured user input.
    pub waiting_for_user_input: usize,
    /// Number of failed agents.
    pub failed: usize,
    /// Number of completed agents.
    pub completed: usize,
    /// Number of lifecycle audit entries that failed durable append.
    #[serde(default)]
    pub audit_sink_error_count: u64,
    /// Last durable audit sink error observed by the supervisor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_audit_sink_error: Option<String>,
    /// Current subagent spawn-policy quota and reservation snapshot.
    #[serde(default)]
    pub spawn_policy: crate::SubagentPolicyStatusView,
}

/// Cheap task counters for operator status.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonTaskStatusSummaryView {
    /// Number of live daemon-managed background shell tasks.
    pub live_background_shell_task_count: usize,
    /// Number of durable session task records.
    #[serde(default)]
    pub total: usize,
    /// Number of pending task records.
    #[serde(default)]
    pub pending: usize,
    /// Number of in-progress task records.
    #[serde(default)]
    pub in_progress: usize,
    /// Number of blocked task records.
    #[serde(default)]
    pub blocked: usize,
    /// Number of completed task records.
    #[serde(default)]
    pub completed: usize,
    /// Number of failed task records.
    #[serde(default)]
    pub failed: usize,
    /// Number of cancelled task records.
    #[serde(default)]
    pub cancelled: usize,
    /// Number of sessions whose task state could not be read while building status.
    #[serde(default)]
    pub unreadable_session_count: usize,
    /// Number of indexed sessions that do not yet have a durable task summary.
    #[serde(default)]
    pub unindexed_session_count: usize,
}

/// Coarse health state for cheap status probes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonStatusProbeState {
    #[default]
    Ok,
    Warning,
    Error,
}

/// One filesystem write probe result exposed through daemon status.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStorageProbeView {
    /// Stable name for the probed storage root.
    pub name: String,
    /// Filesystem path that was checked.
    pub path: String,
    /// Coarse probe outcome.
    pub state: DaemonStatusProbeState,
    /// True when the daemon successfully created, wrote, synced, and removed a probe file.
    pub writable: bool,
    /// Probe latency in milliseconds.
    #[serde(default)]
    pub latency_ms: u64,
    /// Machine-readable result code.
    pub code: String,
    /// Operator-facing result message with no secret material.
    pub message: String,
    /// Suggested operator action for non-OK states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// Explicit storage health exposed through `/v1/status`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStorageStatusView {
    /// Snapshot timestamp for the storage probes.
    #[serde(default)]
    pub checked_at_ms: u64,
    /// True when every probed storage root accepted a write probe.
    #[serde(default)]
    pub ok: bool,
    /// Number of write probes that failed.
    #[serde(default)]
    pub write_error_count: usize,
    /// Bounded probe results for daemon-owned storage roots.
    #[serde(default)]
    pub probes: Vec<DaemonStorageProbeView>,
    /// State-root lock status for the daemon process that produced this snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_root_lock: Option<DaemonStateRootLockStatusView>,
    /// Bounded asset startup repair summary captured when the asset store was loaded.
    #[serde(default)]
    pub asset_repair: AssetStartupRepairStatusView,
    /// Aggregated session storage footprint, absent when measuring failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_storage: Option<DaemonSessionStorageStatusView>,
}

/// Aggregated on-disk session storage footprint exposed through `/v1/status`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonSessionStorageStatusView {
    /// Number of sessions measured.
    #[serde(default)]
    pub session_count: usize,
    /// Total bytes across journals, metadata sidecars, and task archives.
    #[serde(default)]
    pub total_bytes: u64,
    /// Identifier of the largest session on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub largest_session_id: Option<String>,
    /// Footprint of the largest session on disk.
    #[serde(default)]
    pub largest_session_bytes: u64,
    /// Per-session threshold above which a session is reported oversized.
    #[serde(default)]
    pub oversized_threshold_bytes: u64,
    /// Number of sessions above the threshold; compact them offline with
    /// `sessions vacuum`.
    #[serde(default)]
    pub oversized_session_count: usize,
    /// Bounded sample of oversized session identifiers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub oversized_session_ids: Vec<String>,
}

/// Bounded startup repair summary for daemon-owned assets.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetStartupRepairStatusView {
    /// Number of asset repair actions performed at startup.
    #[serde(default)]
    pub repaired_count: usize,
    /// Number of asset metadata records skipped because their raw payload could not be trusted.
    #[serde(default)]
    pub skipped_asset_count: usize,
    /// Number of skipped assets whose raw payload was missing.
    #[serde(default)]
    pub skipped_raw_missing_count: usize,
    /// Number of skipped assets whose raw payload failed stored integrity checks.
    #[serde(default)]
    pub skipped_raw_integrity_mismatch_count: usize,
    /// Number of tombstone files ignored because they were corrupt or did not match their filename.
    #[serde(default)]
    pub invalid_tombstone_count: usize,
    /// Number of partial tombstone delete windows completed.
    #[serde(default)]
    pub completed_tombstone_delete_count: usize,
    /// Number of derived text payloads restored from valid raw payloads.
    #[serde(default)]
    pub restored_derived_text_count: usize,
    /// Number of derived preview payloads restored from valid raw payloads.
    #[serde(default)]
    pub restored_derived_preview_count: usize,
    /// Number of legacy derived payloads whose checksum/size metadata was backfilled.
    #[serde(default)]
    pub integrity_backfilled_count: usize,
    /// Bounded repair diagnostics, in startup scan order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<AssetStartupRepairDiagnosticView>,
}

/// One bounded asset startup repair diagnostic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetStartupRepairDiagnosticView {
    /// Asset id when the diagnostic can be attributed to one asset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_id: Option<String>,
    /// Asset component involved in the repair, such as `raw`, `derived_text`, or `tombstone`.
    pub kind: String,
    /// Startup action, such as `restore`, `skip_asset`, or `complete_delete`.
    pub action: String,
    /// Machine-readable reason for the startup action.
    pub reason: String,
    /// Opaque asset URI when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

/// State-root lock posture exposed through `/v1/status`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStateRootLockStatusView {
    /// Path to the daemon lock file.
    pub path: String,
    /// True when this daemon process owns the state-root lock.
    #[serde(default)]
    pub held: bool,
    /// Locking mechanism used on this platform.
    pub mechanism: String,
}

/// One model-provider route readiness probe exposed through daemon status.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonProviderRouteReadinessView {
    pub route_id: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub capabilities: crate::RouteCapabilities,
    #[serde(default)]
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_ref: Option<String>,
    pub state: DaemonStatusProbeState,
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<kheish_auth::AuthMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_updated_at_ms: Option<u64>,
}

/// Model-provider/account readiness exposed through `/v1/status`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonProviderReadinessView {
    /// Number of configured model routes.
    #[serde(default)]
    pub route_count: usize,
    /// Number of routes whose cheap readiness probe is OK.
    #[serde(default)]
    pub ready_route_count: usize,
    /// Number of routes with non-fatal readiness warnings.
    #[serde(default)]
    pub warning_route_count: usize,
    /// Number of routes with blocking readiness errors.
    #[serde(default)]
    pub error_route_count: usize,
    /// True when the currently active/default route has no readiness error.
    #[serde(default)]
    pub active_route_ready: bool,
    /// Per-route readiness details.
    #[serde(default)]
    pub routes: Vec<DaemonProviderRouteReadinessView>,
}

/// Control-plane auth/CORS posture exposed through daemon status.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonControlPlaneStatusView {
    /// Base URL the daemon uses for its own control-plane callbacks.
    pub base_url: String,
    /// Socket address the HTTP control plane is listening on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    /// Whether the HTTP bind address is loopback-only.
    #[serde(default)]
    pub bind_is_loopback: bool,
    /// Whether the HTTP bind address accepts traffic on an unspecified interface.
    #[serde(default)]
    pub bind_is_unspecified: bool,
    /// Whether bearer-token control-plane auth is enabled.
    pub auth_enabled: bool,
    /// Whether a read-only bearer token is configured.
    pub read_only_token_enabled: bool,
    /// Whether an effective full-access token is currently loadable.
    #[serde(default)]
    pub auth_effective_admin_token_available: bool,
    /// Whether an effective read-only token is currently loadable.
    #[serde(default)]
    pub auth_effective_read_only_token_available: bool,
    /// Number of configured token-file sources.
    #[serde(default)]
    pub auth_token_file_count: usize,
    /// Number of configured token-file sources that cannot currently load a token.
    #[serde(default)]
    pub auth_token_file_error_count: usize,
    /// True when the effective admin and read-only tokens are identical and therefore rejected.
    #[serde(default)]
    pub auth_duplicate_token: bool,
    /// Redacted status for configured token-file sources.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auth_token_files: Vec<DaemonControlPlaneAuthTokenFileStatusView>,
    /// Browser CORS policy in effect.
    pub cors_policy: DaemonControlPlaneCorsPolicy,
    /// Number of exact allowed browser origins when `cors_policy` is `exact`.
    pub cors_allowed_origin_count: usize,
    /// True when the control plane may be reachable off-loopback and bearer auth is disabled.
    pub externally_exposed_without_auth: bool,
}

/// Redacted control-plane auth token-file status.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonControlPlaneAuthTokenFileStatusView {
    /// Token role loaded from this file, such as `admin` or `read_only`.
    pub role: String,
    /// Token file path.
    pub path: String,
    /// Whether the file could be read.
    #[serde(default)]
    pub readable: bool,
    /// Whether a non-empty token was loaded from the file.
    #[serde(default)]
    pub token_loaded: bool,
    /// Redacted read/validation error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Browser CORS policy kind exposed through daemon status.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonControlPlaneCorsPolicy {
    #[default]
    Loopback,
    Exact,
}

/// Aggregated health state for cheap operator status.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonHealthView {
    /// Snapshot wall-clock timestamp.
    #[serde(default)]
    pub generated_at_ms: u64,
    /// Server-side time spent building this status snapshot.
    #[serde(default)]
    pub snapshot_duration_ms: u64,
    /// True when no warning or error was detected in the cheap status snapshot.
    #[serde(default)]
    pub ok: bool,
    /// Number of route diagnostics with `error` severity.
    #[serde(default)]
    pub route_error_count: usize,
    /// Number of route diagnostics with `warning` severity.
    #[serde(default)]
    pub route_warning_count: usize,
    /// Highest scheduler lag in milliseconds, when an active schedule is overdue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler_lag_ms: Option<u64>,
    /// Structured warnings and errors derived from existing daemon state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<DaemonHealthWarningView>,
}

/// One health warning emitted by `/v1/status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonHealthWarningView {
    pub severity: DaemonHealthSeverity,
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// Severity for daemon health warnings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonHealthSeverity {
    Info,
    Warning,
    Error,
}

/// Cheap hook subsystem status exposed through `/v1/status`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookStatusView {
    /// Number of hook definitions currently configured.
    #[serde(default)]
    pub configured_count: usize,
    /// Number of persisted hook dead-letter records.
    #[serde(default)]
    pub dead_lettered_count: usize,
    /// Number of hook dead-letter records that have not been operator-resolved.
    #[serde(default)]
    pub unresolved_dead_lettered_count: usize,
    /// Most recent hook dead-letter timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_dead_letter_at_ms: Option<u64>,
    /// Most recent hook name moved to dead-letter storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_dead_letter_hook: Option<String>,
    /// Most recent unresolved hook dead-letter timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_unresolved_dead_letter_at_ms: Option<u64>,
    /// Most recent unresolved hook name moved to dead-letter storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_unresolved_dead_letter_hook: Option<String>,
    /// Path to the hook dead-letter store, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead_letter_store_path: Option<String>,
    /// Redacted read error when the daemon cannot inspect hook dead letters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead_letter_read_error: Option<String>,
    /// Runtime counter for successful hook executor attempts.
    #[serde(default)]
    pub execution_count: u64,
    /// Runtime counter for hook executor failures.
    #[serde(default)]
    pub failure_count: u64,
    /// Runtime counter for hook retry attempts.
    #[serde(default)]
    pub retry_count: u64,
    /// Runtime counter for failed hook dead-letter persistence.
    #[serde(default)]
    pub dead_letter_persist_failure_count: u64,
}

/// Cheap event/SSE replay status exposed through `/v1/status`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonEventStatusView {
    /// Configured event history capacity.
    #[serde(default)]
    pub history_capacity: usize,
    /// Number of retained replayable events.
    #[serde(default)]
    pub retained_event_count: usize,
    /// Number of active broadcast subscribers.
    #[serde(default)]
    pub subscriber_count: usize,
    /// Oldest retained event id, serialized as a decimal string for JavaScript-safe clients.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_u64_as_decimal_string",
        deserialize_with = "deserialize_optional_u64_from_decimal_string_or_number"
    )]
    pub oldest_event_id: Option<u64>,
    /// Newest retained event id, serialized as a decimal string for JavaScript-safe clients.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_u64_as_decimal_string",
        deserialize_with = "deserialize_optional_u64_from_decimal_string_or_number"
    )]
    pub newest_event_id: Option<u64>,
    /// Next event id that will be assigned, serialized as a decimal string for JavaScript-safe clients.
    #[serde(
        default,
        serialize_with = "serialize_u64_as_decimal_string",
        deserialize_with = "deserialize_u64_from_decimal_string_or_number"
    )]
    pub next_event_id: u64,
    /// Safe decimal SSE cursor for connecting from the current stream tail without skipping
    /// events published after this status snapshot was read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_event_id_cursor: Option<String>,
    /// Replay buffer utilization as an integer percent.
    #[serde(default)]
    pub replay_buffer_utilization_percent: u8,
    /// Total events evicted from the replay buffer since daemon start.
    #[serde(default)]
    pub evicted_event_count: u64,
    /// Number of subscriptions that requested a cursor older than retained history.
    #[serde(default)]
    pub replay_gap_count: u64,
    /// Events skipped by live SSE consumers that lagged the broadcast channel.
    #[serde(default)]
    pub stream_lagged_event_count: u64,
    /// Oldest cursor for which per-session/per-run eviction metadata is still retained,
    /// serialized as a decimal string for JavaScript-safe clients.
    #[serde(
        default,
        serialize_with = "serialize_u64_as_decimal_string",
        deserialize_with = "deserialize_u64_from_decimal_string_or_number"
    )]
    pub scope_eviction_floor_id: u64,
    /// Number of session scopes with retained eviction metadata.
    #[serde(default)]
    pub evicted_session_scope_count: usize,
    /// Number of run scopes with retained eviction metadata.
    #[serde(default)]
    pub evicted_run_scope_count: usize,
}

/// Redacted operator view of one hook dead-letter record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDeadLetterView {
    pub id: String,
    pub at_ms: u64,
    pub hook_name: String,
    pub event: HookEventName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub target: String,
    pub attempt_count: u8,
    pub failure_mode: HookFailureMode,
    pub contract_version: u32,
    pub invocation_digest: String,
    pub definition_digest: String,
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_reason: Option<String>,
}

/// Request body used to mark a hook dead-letter record as operator-resolved.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveHookDeadLetterRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Server-side daemon status snapshot used by `/v1/status` and the operator CLI.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatusView {
    /// Snapshot wall-clock timestamp used for age calculations.
    #[serde(default)]
    pub snapshot_at_ms: u64,
    /// Process identifier for the daemon that produced this snapshot.
    #[serde(default)]
    pub process_id: u32,
    /// Human-readable readiness state preserved for compatibility.
    pub status: DaemonReadinessState,
    /// Boolean readiness flag preserved for compatibility.
    pub ready: bool,
    /// Static capability advertisement for this daemon build.
    pub capabilities: DaemonCapabilities,
    /// Current runtime settings, including route and debug configuration.
    pub runtime: RuntimeSettingsView,
    /// Control-plane auth/CORS posture.
    #[serde(default)]
    pub control_plane: DaemonControlPlaneStatusView,
    /// Explicit storage/write-health checks.
    #[serde(default)]
    pub storage: DaemonStorageStatusView,
    /// Cheap model-provider/account readiness checks.
    #[serde(default)]
    pub provider_readiness: DaemonProviderReadinessView,
    /// Cheap aggregated health warnings.
    #[serde(default)]
    pub health: DaemonHealthView,
    /// Cheap hook subsystem status.
    #[serde(default)]
    pub hooks: HookStatusView,
    /// Cheap event/SSE replay status.
    #[serde(default)]
    pub events: DaemonEventStatusView,
    /// Cheap session counters.
    pub sessions: DaemonSessionStatusSummaryView,
    /// Cheap run counters.
    pub runs: DaemonRunStatusSummaryView,
    /// Cheap recovered run-memory counters and effective policy.
    #[serde(default)]
    pub run_memory: crate::RunMemoryStatusView,
    /// Cheap prompt-visible session-memory counters.
    #[serde(default)]
    pub session_memory: crate::SessionMemoryStatusView,
    /// Cheap schedule counters.
    pub schedules: DaemonScheduleStatusSummaryView,
    /// Cheap output-delivery queue and worker counters.
    #[serde(default)]
    pub delivery: crate::DeliveryQueueStatusView,
    /// Cheap agent counters.
    pub agents: DaemonAgentStatusSummaryView,
    /// Cheap task counters.
    pub tasks: DaemonTaskStatusSummaryView,
}

/// Session log materialized from the daemon and session store.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionEventLogView {
    pub session: StoredSession,
    pub daemon_outputs: Vec<DaemonOutputRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub run_events: Vec<crate::RunEventEntry>,
}

/// Runtime configuration persistence metadata exposed with every runtime snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeConfigMetadataView {
    /// Current durable runtime-configuration revision. `0` means no daemon-owned mutation
    /// has been persisted yet.
    #[serde(default)]
    pub revision: u64,
    /// Last persisted mutation timestamp in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<u64>,
    /// Whether a daemon-owned runtime config document currently exists on disk.
    #[serde(default)]
    pub persisted: bool,
    /// Number of rollbackable historical revisions retained by the daemon.
    #[serde(default)]
    pub history_len: usize,
    /// Maximum number of historical revisions retained for rollback. Older revisions are pruned
    /// once this limit is exceeded.
    #[serde(default)]
    pub history_limit: usize,
    /// State-root-relative storage path used for durable runtime configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_path: Option<String>,
}

/// One durable runtime-configuration revision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuntimeConfigRevisionView {
    /// Monotonic durable revision assigned by the daemon.
    #[serde(default)]
    pub revision: u64,
    /// Revision creation timestamp in milliseconds since the Unix epoch.
    #[serde(default)]
    pub updated_at_ms: u64,
    /// Operator-facing source that initiated the change.
    pub source: String,
    /// Logical setting changed by this revision, or `rollback`.
    pub setting: String,
    /// Revision whose values were restored when `setting == "rollback"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_of_revision: Option<u64>,
    /// Active model route id, when model routing is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
    /// Active provider name, when model routing is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Active primary model, when model routing is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub permission_mode: PermissionMode,
    pub system_prompt: SystemPromptSettings,
    #[serde(default)]
    pub hooks: HookSettings,
    pub debug_level: DebugCaptureLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learning_policy: Option<LearningAutomationPolicyConfig>,
    #[serde(default)]
    pub run_memory_policy: crate::RunMemoryPolicyConfig,
    #[serde(default)]
    pub tool_runtime_limits: ToolRuntimeLimits,
}

/// Runtime configuration revision list response.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimeConfigRevisionListResponse {
    pub revisions: Vec<RuntimeConfigRevisionView>,
}

/// Runtime configuration rollback request payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRollbackRequest {
    /// Revision to restore. When omitted, the daemon rolls back to the previous revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_revision: Option<u64>,
    /// Optional compare-and-swap guard for callers that need explicit concurrency control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Skip `config_change` hooks for operator recovery from a bad runtime hook revision.
    #[serde(default, skip_serializing_if = "is_false")]
    pub skip_hooks: bool,
}

/// Runtime hook mutation payload.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SetHooksRequest {
    pub settings: HookSettings,
    /// Optional compare-and-swap guard for callers that need explicit concurrency control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Skip `config_change` hooks for operator recovery from a bad runtime hook revision.
    #[serde(default, skip_serializing_if = "is_false")]
    pub skip_hooks: bool,
}

impl<'de> Deserialize<'de> for SetHooksRequest {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let Some(object) = value.as_object() else {
            return Err(serde::de::Error::custom("hooks request must be an object"));
        };
        if object.contains_key("settings") {
            #[derive(Deserialize)]
            struct Wrapped {
                settings: HookSettings,
                #[serde(default)]
                expected_revision: Option<u64>,
                #[serde(default)]
                skip_hooks: bool,
            }
            let wrapped =
                serde_json::from_value::<Wrapped>(value).map_err(serde::de::Error::custom)?;
            return Ok(Self {
                settings: wrapped.settings,
                expected_revision: wrapped.expected_revision,
                skip_hooks: wrapped.skip_hooks,
            });
        }
        if !object.contains_key("hooks") {
            return Err(serde::de::Error::custom(
                "hooks request must include settings or legacy hooks",
            ));
        }
        #[derive(Deserialize)]
        struct LegacyFlat {
            #[serde(flatten)]
            settings: HookSettings,
            #[serde(default)]
            expected_revision: Option<u64>,
            #[serde(default)]
            skip_hooks: bool,
        }
        let legacy =
            serde_json::from_value::<LegacyFlat>(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            settings: legacy.settings,
            expected_revision: legacy.expected_revision,
            skip_hooks: legacy.skip_hooks,
        })
    }
}

/// Runtime tool-limit mutation payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetToolRuntimeLimitsRequest {
    pub limits: ToolRuntimeLimits,
    /// Optional compare-and-swap guard for callers that need explicit concurrency control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// Runtime run-memory policy mutation payload.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SetRunMemoryPolicyRequest {
    pub policy: crate::RunMemoryPolicyConfig,
    /// Optional compare-and-swap guard for callers that need explicit concurrency control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

impl<'de> Deserialize<'de> for SetRunMemoryPolicyRequest {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let Some(object) = value.as_object() else {
            return Err(serde::de::Error::custom(
                "run-memory-policy request must be an object",
            ));
        };
        if object.contains_key("policy") {
            #[derive(Deserialize)]
            struct Wrapped {
                policy: crate::RunMemoryPolicyConfig,
                #[serde(default)]
                expected_revision: Option<u64>,
            }
            let wrapped =
                serde_json::from_value::<Wrapped>(value).map_err(serde::de::Error::custom)?;
            return Ok(Self {
                policy: wrapped.policy,
                expected_revision: wrapped.expected_revision,
            });
        }
        if ![
            "enabled",
            "retention_ms",
            "max_tracked_per_session",
            "max_prompt_entries",
            "redact_pii",
            "search_visibility",
        ]
        .iter()
        .any(|key| object.contains_key(*key))
        {
            return Err(serde::de::Error::custom(
                "run-memory-policy request must include policy or legacy policy fields",
            ));
        }
        #[derive(Deserialize)]
        struct LegacyFlat {
            #[serde(flatten)]
            policy: crate::RunMemoryPolicyConfig,
            #[serde(default)]
            expected_revision: Option<u64>,
        }
        let legacy =
            serde_json::from_value::<LegacyFlat>(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            policy: legacy.policy,
            expected_revision: legacy.expected_revision,
        })
    }
}

/// Runtime learning automation policy mutation payload.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SetLearningPolicyRequest {
    pub policy: LearningAutomationPolicyConfig,
    /// Optional compare-and-swap guard for callers that need explicit concurrency control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

impl<'de> Deserialize<'de> for SetLearningPolicyRequest {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let Some(object) = value.as_object() else {
            return Err(serde::de::Error::custom(
                "learning-policy request must be an object",
            ));
        };
        if object.contains_key("policy") {
            #[derive(Deserialize)]
            struct Wrapped {
                policy: LearningAutomationPolicyConfig,
                #[serde(default)]
                expected_revision: Option<u64>,
            }
            let wrapped =
                serde_json::from_value::<Wrapped>(value).map_err(serde::de::Error::custom)?;
            return Ok(Self {
                policy: wrapped.policy,
                expected_revision: wrapped.expected_revision,
            });
        }
        if !["mode", "capture", "publication", "judge"]
            .iter()
            .any(|key| object.contains_key(*key))
        {
            return Err(serde::de::Error::custom(
                "learning-policy request must include policy or legacy policy fields",
            ));
        }
        #[derive(Deserialize)]
        struct LegacyFlat {
            #[serde(flatten)]
            policy: LearningAutomationPolicyConfig,
            #[serde(default)]
            expected_revision: Option<u64>,
        }
        let legacy =
            serde_json::from_value::<LegacyFlat>(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            policy: legacy.policy,
            expected_revision: legacy.expected_revision,
        })
    }
}

/// Runtime settings snapshot exposed by the daemon.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimeSettingsView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_route: Option<crate::ResolvedModelRoute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<crate::ResolvedModelRoute>,
    #[serde(default)]
    pub route_diagnostics: Vec<crate::RouteDiagnosticView>,
    pub permission_mode: PermissionMode,
    pub system_prompt: SystemPromptSettings,
    #[serde(default)]
    pub hooks: HookSettings,
    pub debug_level: DebugCaptureLevel,
    #[serde(default)]
    pub debug_capture: crate::DebugCapturePolicyView,
    #[serde(default)]
    pub mcp: McpRuntimeSnapshot,
    #[serde(default)]
    pub skills: RuntimeSkillsView,
    #[serde(default)]
    pub learning_policy: LearningAutomationPolicyConfig,
    #[serde(default)]
    pub run_memory_policy: crate::RunMemoryPolicyConfig,
    #[serde(default)]
    pub tool_runtime_limits: ToolRuntimeLimits,
    #[serde(default)]
    pub subagent_policy: crate::SubagentPolicyConfig,
    #[serde(default)]
    pub scheduler_policy: crate::SchedulerPolicyConfig,
    #[serde(default)]
    pub config: RuntimeConfigMetadataView,
}

/// Cursor-paginated list response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPage<T> {
    pub items: Vec<T>,
    pub pagination: ListPageMeta,
}

/// Metadata for one cursor-paginated list response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPageMeta {
    pub limit: usize,
    /// Number of matching items before applying the cursor and page size.
    #[serde(default)]
    pub total_count: usize,
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub order: String,
}

/// Opt-in cursor pagination query parameters shared by list endpoints.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPageQuery {
    pub page: Option<bool>,
    pub cursor: Option<String>,
}

impl ListPageQuery {
    /// Returns true when callers explicitly requested the page envelope.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.page.unwrap_or(false) || self.cursor.is_some()
    }
}

fn list_page_query(page: Option<bool>, cursor: Option<String>) -> ListPageQuery {
    ListPageQuery { page, cursor }
}

/// Run list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunListQuery {
    pub page: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    /// Page order: `asc` (default) or `desc` (newest first).
    pub order: Option<String>,
    pub priority_active: Option<bool>,
    pub session_id: Option<String>,
}

impl RunListQuery {
    pub fn page_query(&self) -> ListPageQuery {
        list_page_query(self.page, self.cursor.clone())
    }
}

/// Delivery queue list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryListQuery {
    pub page: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub plugin: Option<String>,
    pub status: Option<crate::DeliveryStatus>,
}

impl DeliveryListQuery {
    pub fn page_query(&self) -> ListPageQuery {
        list_page_query(self.page, self.cursor.clone())
    }
}

/// Test delivery creation payload routed through the durable delivery queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateDeliveryRequest {
    /// The session the delivery is attributed to.
    pub session_id: String,
    /// The reply plugin that transports the delivery.
    pub plugin: String,
    /// The plugin-specific reply address the delivery targets.
    pub target: String,
    /// The delivered message content.
    pub content: String,
}

/// Delivery replay query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryReplayQuery {
    /// When false, replay returns an existing replay for the same source delivery.
    #[serde(default)]
    pub force: bool,
}

/// Delivery bulk replay request body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryBulkReplayRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub force: bool,
    #[serde(default = "default_true")]
    pub unresolved_only: bool,
}

/// Delivery dead-letter resolution request body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryResolveRequest {
    /// Operator-visible reason. Secret-looking spans are redacted before persistence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Delivery target backpressure reset request body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryBackpressureResetRequest {
    /// Redacted target digest from `DeliveryView.target`, for example `http:address_sha256:...`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Optional plugin filter. When used without `target`, resets every persisted target for that plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// Return the matching redacted targets without mutating backpressure state.
    #[serde(default)]
    pub dry_run: bool,
}

fn default_true() -> bool {
    true
}

/// Explicit run-evidence retention prune request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRetentionPruneRequest {
    /// Only evidence for terminal runs older than this age is eligible.
    pub older_than_ms: u64,
    /// Optional session scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Optional maximum number of candidate debug bundles to prune or report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// When true, only reports candidates without deleting files.
    #[serde(default)]
    pub dry_run: bool,
}

/// Result of one explicit run-evidence retention prune.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRetentionPruneResponse {
    pub dry_run: bool,
    pub now_ms: u64,
    pub cutoff_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default)]
    pub matched_run_count: usize,
    pub candidate_run_ids: Vec<String>,
    #[serde(default)]
    pub candidate_debug_bytes: u64,
    pub pruned_debug_run_ids: Vec<String>,
    #[serde(default)]
    pub pruned_debug_bytes: u64,
}

/// Session list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListQuery {
    pub page: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    pub persona_id: Option<String>,
}

impl SessionListQuery {
    pub fn page_query(&self) -> ListPageQuery {
        list_page_query(self.page, self.cursor.clone())
    }
}

/// Pending question list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingQuestionListQuery {
    pub session_id: Option<String>,
    pub page: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

impl PendingQuestionListQuery {
    pub fn page_query(&self) -> ListPageQuery {
        list_page_query(self.page, self.cursor.clone())
    }
}

/// Task list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskListQuery {
    pub page: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    pub status: Option<kheish_types::TaskStatus>,
}

impl TaskListQuery {
    pub fn page_query(&self) -> ListPageQuery {
        list_page_query(self.page, self.cursor.clone())
    }
}

/// Task output query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOutputQuery {
    pub wait: Option<bool>,
    pub timeout_ms: Option<u64>,
    pub tail_bytes: Option<usize>,
    pub full: Option<bool>,
}

/// Task stop request payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopTaskRequest {
    pub reason: Option<String>,
}

/// Schedule list query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleListQuery {
    pub session_id: Option<String>,
    pub page: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

impl ScheduleListQuery {
    pub fn page_query(&self) -> ListPageQuery {
        list_page_query(self.page, self.cursor.clone())
    }
}

/// Schedule status mutation response payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScheduleMutationResponse {
    pub schedule: ScheduleView,
}

/// Schedule creation request payload.
pub type CreateScheduleRequest = ScheduleCreateRequest;

/// Runtime model swap request payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetModelRequest {
    #[serde(default, alias = "route_id", skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub model: String,
    /// Optional compare-and-swap guard for concurrent runtime mutations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// Runtime MCP server registration payload: one Codex-compatible
/// `[mcp_servers.<name>]` entry plus its name.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AddMcpServerRequest {
    pub name: String,
    #[serde(flatten)]
    pub server: kheish_mcp::CodexServerConfig,
}

/// Runtime model-route registration payload.
///
/// Exactly one credential source is required: an inline `api_key` (stored in the
/// encrypted secret store under `routes.<route_id>.api_key`) or an
/// `api_key_secret_ref` pointing at an existing secret slot.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AddModelRouteRequest {
    pub route_id: String,
    /// Provider driver: `anthropic`, `google`, `openai`, `openrouter`, or `xai`.
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_secret_ref: Option<String>,
}

/// Runtime permission mode request payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetPermissionModeRequest {
    pub mode: PermissionMode,
    /// Optional compare-and-swap guard for concurrent runtime mutations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// Runtime permission dry-run request payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckPermissionRequest {
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_override: Option<PermissionMode>,
    #[serde(default)]
    pub input: Value,
}

/// Runtime permission matrix dry-run request payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CheckPermissionMatrixRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Runtime permission matrix dry-run response payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PermissionMatrixModeView {
    pub mode: PermissionMode,
    pub tools: Vec<kheish_runtime::PermissionExplanation>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PermissionMatrixView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub modes: Vec<PermissionMatrixModeView>,
}

/// Session permission audit list response payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionPermissionAuditListView {
    pub session_id: String,
    pub audits: Vec<kheish_session::PermissionAuditRecord>,
}

/// Runtime system prompt update payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetSystemPromptRequest {
    pub settings: SystemPromptSettings,
    /// Optional compare-and-swap guard for concurrent runtime mutations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// Runtime debug capture level request payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetDebugLevelRequest {
    pub level: DebugCaptureLevel,
    /// Optional compare-and-swap guard for concurrent runtime mutations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// Session interrupt response payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InterruptSessionResponse {
    pub interrupted: bool,
    pub snapshot: ManagedAgentSnapshot,
}

impl From<&SkillRuntimeConfig> for SkillRuntimeView {
    fn from(value: &SkillRuntimeConfig) -> Self {
        Self {
            allowed_tools: value.allowed_tools.clone(),
            blocked_tools: value.blocked_tools.clone(),
            context: value.context,
            agent_profile: value.agent_profile.clone(),
            provider: value.provider.clone(),
            model: value.model.clone(),
            fallback_model: value.fallback_model.clone(),
        }
    }
}

impl From<SkillRuntimeConfig> for SkillRuntimeView {
    fn from(value: SkillRuntimeConfig) -> Self {
        Self::from(&value)
    }
}

impl From<&SkillSummary> for SkillSummaryView {
    fn from(value: &SkillSummary) -> Self {
        Self {
            name: value.name.clone(),
            description: value.description.clone(),
            when_to_use: value.when_to_use.clone(),
            version: value.version.clone(),
            skill_path: value.skill_path.display().to_string(),
            skill_root: value.skill_root.display().to_string(),
            scope: value.scope,
            digest: value.digest.clone(),
            runtime: SkillRuntimeView::from(&value.runtime),
        }
    }
}

impl From<SkillSummary> for SkillSummaryView {
    fn from(value: SkillSummary) -> Self {
        Self::from(&value)
    }
}

impl From<&SkillDefinition> for SkillView {
    fn from(value: &SkillDefinition) -> Self {
        Self {
            name: value.name.clone(),
            description: value.description.clone(),
            when_to_use: value.when_to_use.clone(),
            version: value.version.clone(),
            skill_path: value.skill_path.display().to_string(),
            skill_root: value.skill_root.display().to_string(),
            scope: value.scope,
            digest: value.digest.clone(),
            runtime: SkillRuntimeView::from(&value.runtime),
            instructions: value.instructions.clone(),
        }
    }
}

impl From<SkillDefinition> for SkillView {
    fn from(value: SkillDefinition) -> Self {
        Self::from(&value)
    }
}

impl From<&StoredAssetRecord> for AssetSummaryView {
    fn from(value: &StoredAssetRecord) -> Self {
        Self {
            asset_id: value.id.clone(),
            media_type: value.media_type.clone(),
            file_name: value.file_name.clone(),
            sha256: value.sha256.clone(),
            byte_length: value.byte_length,
            created_at_ms: value.created_at_ms,
        }
    }
}

impl From<StoredAssetRecord> for AssetSummaryView {
    fn from(value: StoredAssetRecord) -> Self {
        Self::from(&value)
    }
}

impl From<&StoredAssetRecord> for AssetView {
    fn from(value: &StoredAssetRecord) -> Self {
        Self {
            asset_id: value.id.clone(),
            media_type: value.media_type.clone(),
            file_name: value.file_name.clone(),
            sha256: value.sha256.clone(),
            byte_length: value.byte_length,
            created_at_ms: value.created_at_ms,
            uri: value.uri.clone(),
            text_uri: value.text_uri.clone(),
            text_sha256: value.text_sha256.clone(),
            text_byte_length: value.text_byte_length,
            preview_image_uri: value.preview_image_uri.clone(),
            preview_image_media_type: value.preview_image_media_type.clone(),
            preview_image_sha256: value.preview_image_sha256.clone(),
            preview_image_byte_length: value.preview_image_byte_length,
            derivation_ids: value.derivation_ids.clone(),
            provenance: value
                .provenance
                .iter()
                .map(|provenance| AssetProvenanceView {
                    kind: provenance.kind.clone(),
                    tool_name: provenance.tool_name.clone(),
                    session_id: provenance.session_id.clone(),
                    run_id: provenance.run_id.clone(),
                    tool_call_id: provenance.tool_call_id.clone(),
                    route_id: provenance.route_id.clone(),
                    provider: provenance.provider.clone(),
                    model: provenance.model.clone(),
                    prompt_sha256: provenance.prompt_sha256.clone(),
                    source_assets: provenance
                        .source_assets
                        .iter()
                        .map(|source| AssetProvenanceSourceView {
                            asset_id: source.asset_id.clone(),
                            media_type: source.media_type.clone(),
                            sha256: source.sha256.clone(),
                        })
                        .collect(),
                    output_index: provenance.output_index,
                    output_count: provenance.output_count,
                })
                .collect(),
        }
    }
}

impl From<StoredAssetRecord> for AssetView {
    fn from(value: StoredAssetRecord) -> Self {
        Self::from(&value)
    }
}

impl From<&PersonaIndexEntry> for PersonaSummaryView {
    fn from(value: &PersonaIndexEntry) -> Self {
        Self {
            persona_id: value.persona_id.clone(),
            display_name: value.display_name.clone(),
            version: value.version,
            created_at_ms: value.created_at_ms,
            updated_at_ms: value.updated_at_ms,
        }
    }
}

impl From<PersonaIndexEntry> for PersonaSummaryView {
    fn from(value: PersonaIndexEntry) -> Self {
        Self::from(&value)
    }
}

impl From<&PersonaRecord> for PersonaView {
    fn from(value: &PersonaRecord) -> Self {
        Self {
            persona_id: value.persona_id.clone(),
            display_name: value.display_name.clone(),
            soul: value.soul.clone(),
            version: value.version,
            created_at_ms: value.created_at_ms,
            updated_at_ms: value.updated_at_ms,
            capability_scope: value.capability_scope.clone(),
            default_skills: value.default_skills.clone(),
            metadata: value.metadata.clone(),
        }
    }
}

impl From<PersonaRecord> for PersonaView {
    fn from(value: PersonaRecord) -> Self {
        Self::from(&value)
    }
}

impl From<&SessionPersonaBinding> for SessionPersonaSummaryView {
    fn from(value: &SessionPersonaBinding) -> Self {
        Self {
            persona_id: value.persona_id.clone(),
            persona_version: value.persona_version,
            display_name: value.display_name.clone(),
            bound_at_ms: value.bound_at_ms,
        }
    }
}

impl From<SessionPersonaBinding> for SessionPersonaSummaryView {
    fn from(value: SessionPersonaBinding) -> Self {
        Self::from(&value)
    }
}

fn connector_source_view(source: ConnectorConfigSource) -> ConnectorSourceView {
    match source {
        ConnectorConfigSource::File => ConnectorSourceView::File,
        ConnectorConfigSource::Daemon => ConnectorSourceView::Daemon,
    }
}

fn connector_secret_view(
    inline: Option<&str>,
    env: Option<&str>,
    secret_ref: Option<&str>,
) -> ConnectorSecretView {
    if let Some(secret_ref) = secret_ref {
        return ConnectorSecretView {
            configured: true,
            source: Some("secret_ref".to_string()),
            secret_ref: Some(secret_ref.to_string()),
            env: None,
        };
    }
    if let Some(env) = env {
        return ConnectorSecretView {
            configured: true,
            source: Some("env".to_string()),
            secret_ref: None,
            env: Some(env.to_string()),
        };
    }
    if inline.is_some() {
        return ConnectorSecretView {
            configured: true,
            source: Some("inline".to_string()),
            secret_ref: None,
            env: None,
        };
    }
    ConnectorSecretView {
        configured: false,
        source: None,
        secret_ref: None,
        env: None,
    }
}

impl From<&ConnectorConfigRecord> for ConnectorView {
    fn from(value: &ConnectorConfigRecord) -> Self {
        match value {
            ConnectorConfigRecord::External { source, config } => {
                ConnectorView::External(ExternalConnectorView {
                    source: connector_source_view(*source),
                    name: config.name.clone(),
                    platform: config.platform.clone(),
                    mode: config.mode,
                    base_url: config.base_url.clone(),
                    allow_private_network: config.allow_private_network,
                    shared_token: connector_secret_view(
                        config.shared_token.as_deref(),
                        config.shared_token_env.as_deref(),
                        config.shared_token_secret_ref.as_deref(),
                    ),
                    allow_unauthenticated_ingress: config.allow_unauthenticated_ingress,
                    fixed_session_id: config.fixed_session_id.clone(),
                    include_self_output: config.include_self_output,
                    additional_reply_targets: config.additional_reply_targets.clone(),
                    additional_binding_keys: config.additional_binding_keys.clone(),
                    session_policy: config.session_policy.clone(),
                    ingress_events_per_second: config.ingress_events_per_second,
                    child_process: config.child_process.as_ref().map(|child| {
                        ExternalChildProcessView {
                            command: child.command.clone(),
                            args: child.args.clone(),
                            env_keys: child.env.keys().cloned().collect(),
                            credential_env_keys: child.credential_slots.keys().cloned().collect(),
                            working_dir: child.working_dir.clone(),
                        }
                    }),
                })
            }
            ConnectorConfigRecord::Telegram { source, config } => {
                ConnectorView::Telegram(TelegramConnectorView {
                    source: connector_source_view(*source),
                    name: config.name.clone(),
                    bot_token: connector_secret_view(
                        config.bot_token.as_deref(),
                        config.bot_token_env.as_deref(),
                        config.bot_token_secret_ref.as_deref(),
                    ),
                    secret_token: connector_secret_view(
                        config.secret_token.as_deref(),
                        config.secret_token_env.as_deref(),
                        config.secret_token_secret_ref.as_deref(),
                    ),
                    allow_unauthenticated_ingress: config.allow_unauthenticated_ingress,
                    api_base_url: config.api_base_url.clone(),
                    ingress_mode: config.ingress_mode,
                    polling_timeout_seconds: config.polling_timeout_seconds,
                    ingress_events_per_second: config.ingress_events_per_second,
                    allowed_chat_ids: config.allowed_chat_ids.clone(),
                    fixed_session_id: config.fixed_session_id.clone(),
                    include_self_output: config.include_self_output,
                    additional_reply_targets: config.additional_reply_targets.clone(),
                    additional_binding_keys: config.additional_binding_keys.clone(),
                    session_policy: config.session_policy.clone(),
                })
            }
            ConnectorConfigRecord::Slack { source, config } => {
                ConnectorView::Slack(SlackConnectorView {
                    source: connector_source_view(*source),
                    name: config.name.clone(),
                    bot_token: connector_secret_view(
                        config.bot_token.as_deref(),
                        config.bot_token_env.as_deref(),
                        config.bot_token_secret_ref.as_deref(),
                    ),
                    signing_secret: connector_secret_view(
                        config.signing_secret.as_deref(),
                        config.signing_secret_env.as_deref(),
                        config.signing_secret_secret_ref.as_deref(),
                    ),
                    allow_unauthenticated_ingress: config.allow_unauthenticated_ingress,
                    api_base_url: config.api_base_url.clone(),
                    fixed_session_id: config.fixed_session_id.clone(),
                    include_self_output: config.include_self_output,
                    additional_reply_targets: config.additional_reply_targets.clone(),
                    additional_binding_keys: config.additional_binding_keys.clone(),
                    session_policy: config.session_policy.clone(),
                    ingress_events_per_second: config.ingress_events_per_second,
                    allowed_api_app_ids: config.allowed_api_app_ids.clone(),
                    allowed_enterprise_ids: config.allowed_enterprise_ids.clone(),
                    allowed_team_ids: config.allowed_team_ids.clone(),
                    allowed_channel_ids: config.allowed_channel_ids.clone(),
                    allowed_file_hosts: config.allowed_file_hosts.clone(),
                    team_bot_tokens: config
                        .team_bot_tokens
                        .iter()
                        .map(|entry| SlackTeamBotTokenView {
                            team_id: entry.team_id.clone(),
                            bot_token: connector_secret_view(
                                entry.bot_token.as_deref(),
                                entry.bot_token_env.as_deref(),
                                entry.bot_token_secret_ref.as_deref(),
                            ),
                        })
                        .collect(),
                })
            }
            ConnectorConfigRecord::Http { source, config } => {
                ConnectorView::Http(HttpConnectorView {
                    source: connector_source_view(*source),
                    name: config.name.clone(),
                    fixed_session_id: config.fixed_session_id.clone(),
                    actor_id: config.actor_id.clone(),
                    bearer_token: connector_secret_view(
                        config.bearer_token.as_deref(),
                        config.bearer_token_env.as_deref(),
                        config.bearer_token_secret_ref.as_deref(),
                    ),
                    hmac_secret: connector_secret_view(
                        config.hmac_secret.as_deref(),
                        config.hmac_secret_env.as_deref(),
                        config.hmac_secret_secret_ref.as_deref(),
                    ),
                    allow_unauthenticated_ingress: config.allow_unauthenticated_ingress,
                    require_hmac_signature: config.require_hmac_signature,
                    signature_max_age_secs: config.signature_max_age_secs,
                    require_idempotency_key: config.require_idempotency_key,
                    ingress_events_per_second: config.ingress_events_per_second,
                    allow_payload_reply_targets: config.allow_payload_reply_targets,
                    default_reply_targets: config.default_reply_targets.clone(),
                    default_binding_keys: config.default_binding_keys.clone(),
                    session_policy: config.session_policy.clone(),
                })
            }
        }
    }
}

impl From<ConnectorConfigRecord> for ConnectorView {
    fn from(value: ConnectorConfigRecord) -> Self {
        Self::from(&value)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        DaemonStatusView, InlineAssetUpload, InputAttachmentRequest, ListPageQuery,
        McpToolCallRequest, ProblemDetails, ResolveApprovalsRequest, ResolveUserQuestionRequest,
        RuntimeSettingsView, SetHooksRequest, SetLearningPolicyRequest, SetRunMemoryPolicyRequest,
        SubmitInputItemRequest, SubmitInputRequest, SubmitRunRequest,
        validate_input_attachment_requests, validate_submit_input_items,
    };

    #[test]
    fn mcp_tool_call_request_defaults_missing_input_to_empty_object() {
        let request = serde_json::from_value::<McpToolCallRequest>(json!({}))
            .expect("request should deserialize");

        assert_eq!(request.input, json!({}));
        assert_eq!(McpToolCallRequest::default().input, json!({}));
    }

    #[test]
    fn submit_input_request_defaults_missing_content_for_ordered_items() {
        let request = serde_json::from_value::<SubmitInputRequest>(json!({
            "input_items": [
                {
                    "type": "text",
                    "text": "ordered prompt"
                }
            ]
        }))
        .expect("request should deserialize");

        assert_eq!(request.content, "");
        assert!(matches!(
            request.input_items.as_slice(),
            [SubmitInputItemRequest::Text { text }] if text == "ordered prompt"
        ));
    }

    #[test]
    fn submit_input_request_accepts_structured_and_raw_reply_targets() {
        let request = serde_json::from_value::<SubmitInputRequest>(json!({
            "content": "send this result",
            "reply_targets": [
                {
                    "type": "http",
                    "url": "https://example.com/hooks/kheish",
                    "headers": {
                        "Authorization": "Bearer test"
                    }
                },
                {
                    "plugin": "daemon",
                    "address": "session-1"
                },
                {
                    "type": "raw",
                    "plugin": "external",
                    "address": "{\"connector\":\"bridge\",\"route\":\"thread-1\"}"
                }
            ]
        }))
        .expect("request should deserialize");

        assert_eq!(request.reply_targets.len(), 3);
        assert_eq!(request.reply_targets[0].plugin, "http");
        assert!(request.reply_targets[0].address.contains("example.com"));
        assert_eq!(request.reply_targets[1].plugin, "daemon");
        assert_eq!(request.reply_targets[1].address, "session-1");
        assert_eq!(request.reply_targets[2].plugin, "external");
    }

    #[test]
    fn status_view_deserializes_pre_health_status_payloads() {
        let mut runtime =
            serde_json::to_value(RuntimeSettingsView::default()).expect("runtime should serialize");
        runtime
            .as_object_mut()
            .expect("runtime object")
            .remove("route_diagnostics");

        let status = serde_json::from_value::<DaemonStatusView>(json!({
            "status": "ready",
            "ready": true,
            "capabilities": {
                "control_plane_version": "test",
                "approvals": true,
                "sidechains": true,
                "mailboxes": true,
                "session_events": true,
                "restart_restore": true,
                "live_events": true
            },
            "runtime": runtime,
            "sessions": { "total": 1 },
            "runs": {
                "total": 1,
                "queued": 0,
                "running": 0,
                "waiting_for_approval": 0,
                "waiting_for_user_question": 0,
                "completed": 1,
                "failed": 0,
                "interrupted": 0,
                "cancelled": 0,
                "pending_approval_count": 0,
                "pending_question_count": 0,
                "max_session_queue_depth": 0
            },
            "schedules": {
                "total": 0,
                "active": 0,
                "paused": 0,
                "completed": 0,
                "canceled": 0,
                "due_count": 0,
                "backoff_count": 0,
                "in_flight_count": 0,
                "queued_fire_count": 0
            },
            "agents": {
                "total": 0,
                "live_runtime_count": 0,
                "sidechain_count": 0,
                "closed_count": 0,
                "terminal_snapshot_count": 0,
                "mailbox_message_count": 0,
                "idle": 0,
                "running": 0,
                "waiting_for_approval": 0,
                "waiting_for_user_input": 0,
                "failed": 0,
                "completed": 0
            },
            "tasks": {
                "live_background_shell_task_count": 0
            }
        }))
        .expect("old status payload should deserialize");

        assert_eq!(status.snapshot_at_ms, 0);
        assert_eq!(status.process_id, 0);
        assert_eq!(status.health.generated_at_ms, 0);
        assert!(!status.health.ok);
        assert_eq!(status.tasks.unindexed_session_count, 0);
        assert_eq!(status.runs.queued_run_lag_threshold_ms, 0);
        assert_eq!(status.runs.stale_non_terminal_run_threshold_ms, 0);
    }

    #[test]
    fn event_status_serializes_public_ids_as_strings_and_accepts_legacy_numbers() {
        let status = super::DaemonEventStatusView {
            oldest_event_id: Some(9_007_199_254_740_993),
            newest_event_id: Some(9_007_199_254_740_994),
            next_event_id: 9_007_199_254_740_995,
            scope_eviction_floor_id: 9_007_199_254_740_996,
            ..Default::default()
        };
        let value = serde_json::to_value(&status).expect("status should serialize");

        assert_eq!(value["oldest_event_id"], "9007199254740993");
        assert_eq!(value["newest_event_id"], "9007199254740994");
        assert_eq!(value["next_event_id"], "9007199254740995");
        assert_eq!(value["scope_eviction_floor_id"], "9007199254740996");

        let legacy = serde_json::from_value::<super::DaemonEventStatusView>(json!({
            "oldest_event_id": 41,
            "newest_event_id": "42",
            "next_event_id": 43,
            "scope_eviction_floor_id": "44"
        }))
        .expect("status should accept old numeric ids and new string ids");
        assert_eq!(legacy.oldest_event_id, Some(41));
        assert_eq!(legacy.newest_event_id, Some(42));
        assert_eq!(legacy.next_event_id, 43);
        assert_eq!(legacy.scope_eviction_floor_id, 44);
    }

    #[test]
    fn run_and_resolution_requests_keep_legacy_payloads_compatible() {
        let run = serde_json::from_value::<SubmitRunRequest>(json!({
            "content": "legacy flat input"
        }))
        .expect("legacy run request should deserialize");
        assert_eq!(run.idempotency_key, None);
        assert_eq!(run.request.content, "legacy flat input");

        let approvals = serde_json::from_value::<ResolveApprovalsRequest>(json!({
            "resolutions": []
        }))
        .expect("approval request without idempotency should deserialize");
        assert_eq!(approvals.idempotency_key, None);
        assert!(approvals.resolutions.is_empty());

        let question = serde_json::from_value::<ResolveUserQuestionRequest>(json!({
            "resolution": {
                "request_id": "question-1",
                "declined": true
            }
        }))
        .expect("question request without idempotency should deserialize");
        assert_eq!(question.idempotency_key, None);
        assert_eq!(question.resolution.request_id, "question-1");
        assert!(question.resolution.declined);
    }

    #[test]
    fn learning_policy_request_rejects_invalid_wrapped_policy() {
        let error = serde_json::from_value::<SetLearningPolicyRequest>(json!({
            "policy": null,
            "expected_revision": 7
        }))
        .expect_err("invalid wrapped policy should fail instead of falling back to defaults");

        assert!(
            error.to_string().contains("invalid type") || error.to_string().contains("expected")
        );
    }

    #[test]
    fn learning_policy_request_accepts_legacy_flat_expected_revision() {
        let request = serde_json::from_value::<SetLearningPolicyRequest>(json!({
            "mode": "manual_only",
            "expected_revision": 7
        }))
        .expect("legacy flat policy should deserialize");

        assert_eq!(
            request.policy.mode,
            crate::LearningAutomationMode::ManualOnly
        );
        assert_eq!(request.expected_revision, Some(7));
    }

    #[test]
    fn learning_policy_request_rejects_cas_only_payload() {
        let error = serde_json::from_value::<SetLearningPolicyRequest>(json!({
            "expected_revision": 7
        }))
        .expect_err("cas-only payload should not reset learning policy to defaults");

        assert!(error.to_string().contains("must include policy"));
    }

    #[test]
    fn hooks_request_rejects_cas_only_payload() {
        let error = serde_json::from_value::<SetHooksRequest>(json!({
            "expected_revision": 7,
            "skip_hooks": true
        }))
        .expect_err("cas-only payload should not reset hooks to defaults");

        assert!(error.to_string().contains("must include settings"));
    }

    #[test]
    fn run_memory_policy_request_accepts_legacy_flat_expected_revision() {
        let request = serde_json::from_value::<SetRunMemoryPolicyRequest>(json!({
            "enabled": false,
            "expected_revision": 7
        }))
        .expect("legacy flat run-memory policy should deserialize");

        assert!(!request.policy.enabled);
        assert_eq!(request.expected_revision, Some(7));
    }

    #[test]
    fn run_memory_policy_request_rejects_cas_only_payload() {
        let error = serde_json::from_value::<SetRunMemoryPolicyRequest>(json!({
            "expected_revision": 7
        }))
        .expect_err("cas-only payload should not reset run-memory policy to defaults");

        assert!(error.to_string().contains("must include policy"));
    }

    #[test]
    fn run_memory_policy_request_rejects_unknown_only_payload() {
        let error = serde_json::from_value::<SetRunMemoryPolicyRequest>(json!({
            "unexpected": "ignored"
        }))
        .expect_err("unknown-only payload should not reset run-memory policy to defaults");

        assert!(error.to_string().contains("must include policy"));
    }

    #[test]
    fn input_attachment_validation_rejects_empty_inline_asset_payload() {
        let error = validate_input_attachment_requests(&[InputAttachmentRequest::InlineAsset(
            InlineAssetUpload {
                file_name: "empty.txt".to_string(),
                media_type: Some("text/plain".to_string()),
                content_base64: String::new(),
            },
        )])
        .expect_err("empty inline attachment should fail");

        assert!(error.to_string().contains("content_base64 is required"));
    }

    #[test]
    fn ordered_input_validation_rejects_empty_inline_asset_payload() {
        let error = validate_submit_input_items(&[SubmitInputItemRequest::InlineAsset(
            InlineAssetUpload {
                file_name: "empty.txt".to_string(),
                media_type: Some("text/plain".to_string()),
                content_base64: String::new(),
            },
        )])
        .expect_err("empty ordered inline asset should fail");

        assert!(error.to_string().contains("content_base64 is required"));
    }

    #[test]
    fn list_page_query_requires_explicit_page_or_cursor() {
        assert!(
            !ListPageQuery {
                page: None,
                cursor: None,
            }
            .enabled()
        );
        assert!(
            ListPageQuery {
                page: Some(true),
                cursor: None,
            }
            .enabled()
        );
        assert!(
            ListPageQuery {
                page: None,
                cursor: Some("cursor".to_string()),
            }
            .enabled()
        );
    }

    #[test]
    fn problem_details_titles_cover_common_http_statuses() {
        assert_eq!(
            ProblemDetails::new(405, "method_not_allowed", "wrong method").title,
            "Method Not Allowed"
        );
        assert_eq!(
            ProblemDetails::new(422, "validation_failed", "blocked").title,
            "Unprocessable Entity"
        );
        assert_eq!(
            ProblemDetails::new(503, "service_unavailable", "draining").title,
            "Service Unavailable"
        );
    }
}
