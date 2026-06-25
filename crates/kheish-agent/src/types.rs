use std::collections::BTreeMap;

use kheish_types::{
    ApprovalRequest, ConversationKey, InputEnvelope, ModelGenerationConfig, ToolSurfaceFilter,
    UserQuestionRequest,
};
use serde::{Deserialize, Serialize};

/// The stable identifier for a spawned sub-agent.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AgentId(pub String);

/// The lifecycle state of a sub-agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// The agent is idle and ready to receive work.
    Idle,
    /// The agent is actively processing work.
    Running,
    /// The agent is waiting for one or more approval decisions.
    WaitingForApproval,
    /// The agent is waiting for structured user input.
    WaitingForUserInput,
    /// The agent failed and requires operator intervention.
    Failed,
    /// The agent completed its assigned work.
    Completed,
}

/// The retention strategy applied to a child agent after it settles.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildRetentionPolicy {
    /// Keep the child runtime alive after each completed run.
    #[default]
    Retain,
    /// Close the child runtime once it reaches a stable terminal state.
    CloseOnSettle,
}

/// Durable delivery state for one mailbox message.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxMessageState {
    /// The message is queued in the recipient mailbox.
    #[default]
    Pending,
    /// The message has been captured by a durable delivery run.
    Delivering,
    /// The message has been moved to the dead-letter queue.
    DeadLetter,
}

fn default_mailbox_schema_version() -> u32 {
    MailboxMessage::SCHEMA_VERSION
}

/// A mailbox message exchanged between agents.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MailboxMessage {
    /// Version of the mailbox message envelope contract.
    #[serde(default = "default_mailbox_schema_version")]
    pub schema_version: u32,
    /// Stable message identifier used for ack/retry/dedupe.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Current durable delivery state.
    #[serde(default)]
    pub state: MailboxMessageState,
    /// Creation timestamp in milliseconds since the Unix epoch.
    #[serde(default)]
    pub created_at_ms: u64,
    /// Optional expiration timestamp in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// Number of delivery runs that have captured this message.
    #[serde(default)]
    pub delivery_attempts: u32,
    /// Last delivery or dead-letter reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The sender agent identifier.
    pub from: AgentId,
    /// The destination agent identifier.
    pub to: AgentId,
    /// The subject line.
    pub subject: String,
    /// The JSON payload.
    pub payload: serde_json::Value,
}

impl MailboxMessage {
    /// Current mailbox envelope version.
    pub const SCHEMA_VERSION: u32 = 1;

    /// Builds one pending mailbox message.
    pub fn new(
        id: String,
        from: AgentId,
        to: AgentId,
        subject: String,
        payload: serde_json::Value,
        created_at_ms: u64,
        expires_at_ms: Option<u64>,
    ) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            id,
            state: MailboxMessageState::Pending,
            created_at_ms,
            expires_at_ms,
            delivery_attempts: 0,
            last_error: None,
            from,
            to,
            subject,
            payload,
        }
    }

    /// Returns whether the message has expired at `now_ms`.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms
            .is_some_and(|expires_at_ms| expires_at_ms <= now_ms)
    }

    /// Returns a copy marked as captured by one delivery run.
    pub fn delivering_for_run(&self) -> Self {
        let mut message = self.clone();
        message.state = MailboxMessageState::Delivering;
        message.delivery_attempts = message.delivery_attempts.saturating_add(1);
        message
    }

    /// Returns a copy requeued after a failed delivery attempt.
    pub fn pending_after_delivery_error(&self, reason: impl Into<String>) -> Self {
        let mut message = self.clone();
        message.state = MailboxMessageState::Pending;
        message.last_error = Some(reason.into());
        message
    }

    /// Returns a copy marked as dead-lettered.
    pub fn dead_lettered(&self, reason: impl Into<String>) -> Self {
        let mut message = self.clone();
        message.state = MailboxMessageState::DeadLetter;
        message.last_error = Some(reason.into());
        message
    }

    /// Semantic equality for legacy payload-based dedupe when ids are unavailable.
    pub fn matches_delivery_payload(&self, other: &Self) -> bool {
        if !self.id.is_empty() && !other.id.is_empty() {
            return self.id == other.id;
        }
        self.from == other.from
            && self.to == other.to
            && self.subject == other.subject
            && self.payload == other.payload
    }
}

/// A subtask assigned to a spawned agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubtaskSpec {
    /// The subtask name.
    pub name: String,
    /// The subtask description.
    pub description: String,
    /// The normalized input envelope for the subtask.
    pub input: InputEnvelope,
}

/// The persisted context used when forking a sub-agent from an existing turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ForkContext {
    /// The exact parent assistant message carried into the fork.
    pub parent_assistant_message: String,
    /// Synthetic placeholder tool-result identifiers created for inherited tool uses.
    pub inherited_tool_call_ids: Vec<String>,
    /// Optional team label associated with the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_name: Option<String>,
    /// Optional isolation mode associated with the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<String>,
    /// The exact system prompt bytes reused for the fork.
    pub system_prompt: String,
    /// The prompt merge mode applied to the fork-specific system prompt.
    #[serde(default)]
    pub prompt_merge_mode: kheish_runtime::PromptMergeMode,
    /// The default provider pinned to the forked agent when one was resolved at spawn time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The default generation overrides applied to the child runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<ModelGenerationConfig>,
    /// The tool filter applied to the child runtime.
    #[serde(default)]
    pub tool_surface: ToolSurfaceFilter,
    /// The optional worktree path associated with the forked agent.
    pub worktree_path: Option<String>,
}

/// The persisted state of a single agent entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentRecord {
    /// The agent identifier.
    pub id: AgentId,
    /// The optional parent agent.
    pub parent: Option<AgentId>,
    /// The stable machine-friendly name allocated for the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The stable hierarchy path allocated for the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The optional human-readable nickname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    /// The agent conversation key.
    pub conversation: ConversationKey,
    /// The current agent status.
    pub status: AgentStatus,
    /// The post-settlement retention strategy.
    #[serde(default)]
    pub retention: ChildRetentionPolicy,
    /// The originating daemon run when the agent was spawned from a tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_by_run_id: Option<String>,
    /// The idempotency key attached to the spawn request when one was provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_request_id: Option<String>,
    /// The creation timestamp in milliseconds since the Unix epoch.
    #[serde(default)]
    pub spawned_at_ms: u64,
    /// The timestamp when the agent last reached a stable terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at_ms: Option<u64>,
    /// The timestamp when the runtime handle was closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at_ms: Option<u64>,
    /// The assigned subtasks.
    pub subtasks: Vec<SubtaskSpec>,
    /// The optional sidechain session identifier.
    pub sidechain_session_id: Option<String>,
    /// The optional fork context reused during resume.
    pub fork_context: Option<ForkContext>,
}

/// A serializable snapshot of supervisor state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentSupervisorSnapshot {
    /// The next numeric identifier to allocate.
    pub next_id: u64,
    /// The next monotone audit identifier to allocate.
    #[serde(default)]
    pub next_audit_id: u64,
    /// The tracked agent records keyed by identifier.
    pub agents: BTreeMap<AgentId, AgentRecord>,
    /// The latest terminal snapshots for closed agents.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub terminal_snapshots: BTreeMap<AgentId, ManagedAgentSnapshot>,
    /// The pending mailbox messages keyed by destination agent.
    pub mailboxes: BTreeMap<AgentId, Vec<MailboxMessage>>,
    /// Dead-lettered mailbox messages keyed by destination agent.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mailbox_dead_letters: BTreeMap<AgentId, Vec<MailboxMessage>>,
    /// Bounded supervisor lifecycle audit records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audit_log: Vec<AgentSupervisorAuditEntry>,
}

/// One durable supervisor lifecycle audit record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSupervisorAuditEntry {
    /// Monotone identifier within the supervisor snapshot.
    pub audit_id: u64,
    /// Timestamp in milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// Stable lifecycle event name.
    pub event: String,
    /// Primary agent affected by the event.
    pub agent_id: AgentId,
    /// Parent agent, when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<AgentId>,
    /// Session associated with the primary agent, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Previous lifecycle status, when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_status: Option<AgentStatus>,
    /// New lifecycle status, when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<AgentStatus>,
    /// Human-readable reason for operator-visible lifecycle actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Cheap aggregate counts for one supervisor state snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSupervisorStatusSnapshot {
    /// Number of agent records known by the supervisor.
    pub total: usize,
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
    /// Number of lifecycle audit entries that could not be appended to the durable sink.
    #[serde(default)]
    pub audit_sink_error_count: u64,
    /// Last durable audit sink error observed by the supervisor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_audit_sink_error: Option<String>,
}

/// A summarized runtime view for one supervised agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManagedAgentSnapshot {
    /// The static agent record.
    pub agent: AgentRecord,
    /// Pending approval requests emitted by the runtime.
    pub pending_approvals: Vec<ApprovalRequest>,
    /// Pending structured user-question requests emitted by the runtime.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_questions: Vec<UserQuestionRequest>,
    /// The latest assistant message content, when available.
    pub last_assistant_message: Option<String>,
    /// The number of journal entries currently loaded in memory.
    pub journal_len: usize,
    /// The number of compaction checkpoints currently loaded in memory.
    pub checkpoint_len: usize,
    /// The last runtime error, if the session command failed.
    pub last_error: Option<String>,
}

/// The result of interrupting an in-flight session run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InterruptResult {
    /// Whether a running command was actually interrupted.
    pub interrupted: bool,
    /// The latest managed snapshot after the interrupt request.
    pub snapshot: ManagedAgentSnapshot,
}
