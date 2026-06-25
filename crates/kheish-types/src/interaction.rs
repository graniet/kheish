use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::ToolCallRecord;

/// Describes one approval request emitted by the permission layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// The stable approval request identifier.
    pub id: String,
    /// The affected tool call identifier.
    pub tool_call_id: String,
    /// The affected tool name.
    pub tool_name: String,
    /// The original tool input presented for review.
    pub input: Value,
    /// The scope that owns the matching permission rule.
    pub scope: String,
    /// The human-readable reason shown to the caller.
    pub reason: String,
}

/// Describes a caller-supplied resolution for a pending approval request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalResolution {
    /// The approval request identifier being resolved.
    pub request_id: String,
    /// The resolution behavior.
    pub behavior: ApprovalResolutionBehavior,
    /// Optional updated tool input supplied by the approver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<Value>,
    /// Optional justification attached to the resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
    /// Optional human-readable reason for a denial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Declares how a pending approval request was resolved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResolutionBehavior {
    /// Execute the tool call.
    Allow,
    /// Reject the tool call.
    Deny,
}

/// One selectable option displayed to the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestionOption {
    /// Stable option identifier within the request.
    pub id: String,
    /// Short user-facing label.
    pub label: String,
    /// Optional explanatory text shown alongside the label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional preview content rendered by capable clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// One structured question presented to the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestion {
    /// Stable question identifier within the request.
    pub id: String,
    /// Short header used by capable clients.
    pub header: String,
    /// The question text shown to the user.
    pub question: String,
    /// The mutually exclusive or multi-select options.
    pub options: Vec<UserQuestionOption>,
    /// Whether multiple options may be selected.
    #[serde(default)]
    pub multi_select: bool,
}

/// One pending structured user-question request emitted by the agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestionRequest {
    /// Stable request identifier.
    pub id: String,
    /// The tool call that created the request.
    pub tool_call_id: String,
    /// The structured questions presented to the user.
    pub questions: Vec<UserQuestion>,
    /// Creation timestamp in milliseconds since the Unix epoch.
    #[serde(default)]
    pub created_at_ms: u64,
    /// Optional expiration timestamp in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
}

/// One user-supplied answer for a structured question.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestionAnswer {
    /// The answered question identifier.
    pub question_id: String,
    /// The selected option identifiers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_option_ids: Vec<String>,
    /// Optional freeform answer when the user needs to elaborate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freeform_answer: Option<String>,
}

/// The caller-provided resolution for one pending user-question request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestionResolution {
    /// The pending request identifier being resolved.
    pub request_id: String,
    /// The structured answers supplied by the user.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<UserQuestionAnswer>,
    /// Whether the user declined to answer the clarification request.
    #[serde(default)]
    pub declined: bool,
    /// Optional operator note attached to the answer set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
}

/// Represents the result of a permission check for one tool call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    Deny { reason: String },
    Ask { request: ApprovalRequest },
}

/// Stores one permission decision that belongs to a pending tool batch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingToolDecision {
    /// The tool call waiting to be finalized.
    pub call: ToolCallRecord,
    /// The current permission decision for the call.
    pub decision: PermissionDecision,
    /// Additional context emitted by permission hooks before finalization.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hook_contexts: Vec<String>,
    /// Whether the engine should continue the turn after a denied tool result.
    #[serde(default)]
    pub retry: bool,
}

/// Stores one tool batch that was suspended while waiting for approvals.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingToolBatch {
    /// The turn that produced the batch.
    pub turn: usize,
    /// The assistant message that created the tool calls.
    pub assistant_message_id: String,
    /// The suspended tool calls together with their permission decisions.
    pub decisions: Vec<PendingToolDecision>,
}

/// Stores one structured user-question interaction suspended mid-run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingUserQuestion {
    /// The turn that produced the question request.
    pub turn: usize,
    /// The assistant message that triggered the interactive tool call.
    pub assistant_message_id: String,
    /// The suspended tool call.
    pub call: ToolCallRecord,
    /// The structured question request shown to the user.
    pub request: UserQuestionRequest,
}

/// Describes the terminal or suspended state of one agent run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunStatus {
    /// The run produced a final assistant answer.
    Completed,
    /// The run paused because one or more tool calls require approval.
    WaitingForApproval { requests: Vec<ApprovalRequest> },
    /// The run paused because the user must answer one or more structured questions.
    WaitingForUserQuestion { requests: Vec<UserQuestionRequest> },
}
