use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use kheish_agent::MailboxMessage;
use kheish_session::{
    append_json_line_sync, append_json_lines_sync, decode_safe_storage_name,
    prepare_storage_path_for_write, resolve_storage_path_for_read, write_json_pretty_atomically,
};
use kheish_types::{
    ApprovalRequest, ApprovalResolution, AttachmentRef, ReplyHandle, UserQuestionRequest,
    UserQuestionResolution,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state_files::read_json_or_quarantine;
use crate::{
    DaemonOutputRecord, DeliveryView, ObservationMaterializationRequest, PendingQuestionView,
    ResolveApprovalsRequest, ResolveUserQuestionRequest, SubmitInputItemRequest,
    SubmitInputRequest,
};

/// The durable kind of work represented by one daemon run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonRunKind {
    /// One user or API input processed by an agent session.
    Input,
    /// One daemon-owned continuation submitted to advance an active session goal.
    GoalContinuation,
    /// One scheduler-owned input submission dispatched later.
    ScheduledInput,
    /// One observation materialization submission executed against a session.
    ObservationMaterialization,
    /// One scheduler-owned observation materialization dispatched later.
    ScheduledObservationMaterialization,
    /// One agent mailbox delivery processed in the background.
    MailboxDelivery,
    /// One public channel turn delivered to one session-backed member.
    ChannelDelivery,
    /// One child clarification surfaced to the parent user channel.
    ParentClarification,
    /// A suspended run resumed after approval resolution.
    ApprovalResume,
    /// A suspended run resumed after structured user input.
    UserQuestionResume,
}

/// One durable reference back to the scheduled dispatch that owns a resumed run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledRunOrigin {
    /// The owning schedule identifier.
    pub schedule_id: String,
    /// The claimed fire timestamp for this execution.
    pub fire_at_ms: u64,
}

/// One typed clarification request escalated from a child agent to its parent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParentClarificationRunRequest {
    /// The child agent awaiting the clarification result.
    pub requester_agent_id: String,
    /// The child session that will receive the answer mailbox message.
    pub requester_session_id: String,
    /// The child run that emitted the clarification tool call, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_run_id: Option<String>,
    /// The child tool call that emitted the clarification request, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_tool_call_id: Option<String>,
    /// Projects containing the requester child session when the question was raised.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requester_project_ids: Vec<String>,
    /// Public channels containing the requester child session when the question was raised.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requester_channel_ids: Vec<String>,
    /// Projects containing the parent session where the question was surfaced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_project_ids: Vec<String>,
    /// Public channels containing the parent session where the question was surfaced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_channel_ids: Vec<String>,
    /// The structured user-question request to surface to the parent session.
    pub request: UserQuestionRequest,
}

/// One public channel turn delivered to a session-backed member.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelDeliveryRunRequest {
    /// The owning channel identifier.
    pub channel_id: String,
    /// The thread root that this turn should read and possibly answer.
    pub thread_root_message_id: String,
    /// The public message that caused this turn to be scheduled.
    pub origin_message_id: String,
    /// The latest human-authored message that still anchors this social turn.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub human_origin_message_id: String,
    /// The durable turn identifier currently holding the public speaker lease.
    pub turn_id: String,
    /// The ordered addressed members explicitly targeted by the current human turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addressed_member_ids: Vec<String>,
    /// The resolved provider route pinned when the turn was queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The resolved model pinned when the turn was queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Explains why one parent clarification was completed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParentClarificationCompletionReason {
    Answered,
    Declined,
    Cancelled,
    Interrupted,
    Expired { expires_at_ms: u64 },
}

/// Tracks which durable parent-clarification side effects already completed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentClarificationCompletionState {
    /// The accepted user resolution when the clarification was first claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<UserQuestionResolution>,
    /// Structured completion reason used for idempotent replay and typed late-answer errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ParentClarificationCompletionReason>,
    /// Whether the reply mailbox message already reached the child mailbox.
    #[serde(default)]
    pub mailbox_posted: bool,
    /// Whether the parent-facing note already reached daemon outputs.
    #[serde(default)]
    pub output_emitted: bool,
    /// Whether the elicitation-result hook already ran for this completion.
    #[serde(default)]
    pub hook_dispatched: bool,
    /// Whether the structured resolution event has already been durably recorded.
    #[serde(default)]
    pub resolution_recorded: bool,
    /// Whether the completion event has already been durably recorded.
    #[serde(default)]
    pub completion_recorded: bool,
}

impl ParentClarificationCompletionState {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// The externally visible lifecycle of one daemon run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonRunStatus {
    /// The run is accepted and queued behind another active run for the same session.
    Queued,
    /// The run is actively executing in the session actor.
    Running,
    /// The run is paused until approval resolutions are provided.
    WaitingForApproval,
    /// The run is paused until a structured user-question request is answered.
    WaitingForUserQuestion,
    /// The run completed successfully.
    Completed,
    /// The run failed with an unrecoverable error.
    Failed,
    /// The run was interrupted while it was active.
    Interrupted,
    /// The run was cancelled before completion.
    Cancelled,
}

impl DaemonRunStatus {
    /// Returns true when the run no longer has background work pending.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Interrupted | Self::Cancelled
        )
    }

    /// Returns true when one persisted run may move from this lifecycle state to `next`.
    pub fn allows_transition_to(&self, next: &Self) -> bool {
        if self == next {
            return true;
        }
        match (self, next) {
            (Self::Queued, Self::Running | Self::Failed | Self::Cancelled) => true,
            (
                Self::Running,
                Self::WaitingForApproval
                | Self::WaitingForUserQuestion
                | Self::Completed
                | Self::Failed
                | Self::Interrupted
                | Self::Cancelled,
            ) => true,
            (
                Self::WaitingForApproval,
                Self::Running | Self::Failed | Self::Interrupted | Self::Cancelled,
            ) => true,
            (
                Self::WaitingForUserQuestion,
                Self::Running
                | Self::Completed
                | Self::Failed
                | Self::Interrupted
                | Self::Cancelled,
            ) => true,
            (Self::Completed | Self::Failed | Self::Interrupted | Self::Cancelled, _) => false,
            _ => false,
        }
    }
}

/// A compact human-readable summary of a run request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRequestSummary {
    /// The request source plugin.
    pub source_plugin: String,
    /// The request source kind.
    pub source_kind: String,
    /// The actor identifier associated with the request.
    pub actor_id: String,
    /// The short text preview for input runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_preview: Option<String>,
    /// The explicitly or implicitly resolved provider route when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The effective primary model when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The approval request count for approval-resume runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_count: Option<usize>,
    /// The user-question count for question-resume runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_count: Option<usize>,
}

/// Durable idempotency metadata for a direct input run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunInputIdempotency {
    /// SHA-256 hash of the caller-provided idempotency key.
    pub key_hash: String,
    /// Versioned SHA-256 fingerprint of the behaviorally relevant request payload.
    pub request_fingerprint: String,
}

/// The durable request payload that can be replayed by the daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunRequestPayload {
    /// A detached input submission.
    Input {
        request: SubmitInputRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idempotency: Option<RunInputIdempotency>,
    },
    /// One scheduler-owned input submission dispatched at a later timestamp.
    ScheduledInput {
        schedule_id: String,
        fire_at_ms: u64,
        request: SubmitInputRequest,
    },
    /// One detached observation materialization submission.
    ObservationMaterialization {
        request: ObservationMaterializationRequest,
    },
    /// One scheduler-owned observation materialization submission.
    ScheduledObservationMaterialization {
        schedule_id: String,
        fire_at_ms: u64,
        request: ObservationMaterializationRequest,
    },
    /// One daemon-owned public channel-delivery submission.
    ChannelDelivery { request: ChannelDeliveryRunRequest },
    /// One detached mailbox-delivery submission.
    MailboxDelivery {
        agent_id: String,
        messages: Vec<MailboxMessage>,
    },
    /// One child clarification request surfaced to the parent session.
    ParentClarification {
        request: ParentClarificationRunRequest,
        #[serde(
            default,
            skip_serializing_if = "ParentClarificationCompletionState::is_empty"
        )]
        completion: ParentClarificationCompletionState,
    },
    /// A detached approval-resume submission.
    ApprovalResume {
        request: ResolveApprovalsRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        original_request: Option<SubmitInputRequest>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scheduled_origin: Option<ScheduledRunOrigin>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel_delivery: Option<ChannelDeliveryRunRequest>,
    },
    /// A detached user-question resume submission.
    UserQuestionResume {
        request: ResolveUserQuestionRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        original_request: Option<SubmitInputRequest>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scheduled_origin: Option<ScheduledRunOrigin>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel_delivery: Option<ChannelDeliveryRunRequest>,
    },
}

impl RunRequestPayload {
    pub(crate) fn channel_delivery_request(&self) -> Option<&ChannelDeliveryRunRequest> {
        match self {
            RunRequestPayload::ChannelDelivery { request } => Some(request),
            RunRequestPayload::ApprovalResume {
                channel_delivery, ..
            }
            | RunRequestPayload::UserQuestionResume {
                channel_delivery, ..
            } => channel_delivery.as_ref(),
            RunRequestPayload::Input { .. }
            | RunRequestPayload::ScheduledInput { .. }
            | RunRequestPayload::ObservationMaterialization { .. }
            | RunRequestPayload::ScheduledObservationMaterialization { .. }
            | RunRequestPayload::MailboxDelivery { .. }
            | RunRequestPayload::ParentClarification { .. } => None,
        }
    }
}

/// One externally visible daemon run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunView {
    /// The stable run identifier.
    pub run_id: String,
    /// The owning session identifier.
    pub session_id: String,
    /// The owning agent identifier.
    pub agent_id: String,
    /// The run kind.
    pub kind: DaemonRunKind,
    /// The current lifecycle status.
    pub status: DaemonRunStatus,
    /// The run submission timestamp in milliseconds since the Unix epoch.
    pub submitted_at_ms: u64,
    /// The last update timestamp in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
    /// The actual execution start timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    /// The terminal timestamp when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    /// The current queue position within the session, if queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_position: Option<usize>,
    /// The summarized request.
    pub request: RunRequestSummary,
    /// The normalized daemon-owned attachments that formed the input request when applicable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_attachments: Vec<AttachmentRef>,
    /// Arbitrary caller metadata attached to the input request when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_metadata: Option<Value>,
    /// The pending approval identifiers currently associated with this run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_approval_ids: Vec<String>,
    /// The pending approval payloads currently associated with this run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_approvals: Vec<ApprovalRequest>,
    /// The pending structured user-question identifiers currently associated with this run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_question_ids: Vec<String>,
    /// The pending structured user-question payloads currently associated with this run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_questions: Vec<UserQuestionRequest>,
    /// The captured run-local output records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<DaemonOutputRecord>,
    /// Redacted outbound delivery state associated with this run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliveries: Vec<DeliveryView>,
    /// The terminal or transient error description, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Rebuilds the in-memory pending question index from persisted runs.
pub(crate) fn rebuild_pending_question_index(
    runs: &BTreeMap<String, RunRecord>,
) -> BTreeMap<String, PendingQuestionView> {
    let mut pending = BTreeMap::new();
    for record in runs.values() {
        if record.view.status != DaemonRunStatus::WaitingForUserQuestion {
            continue;
        }
        for request in &record.view.pending_questions {
            pending.insert(
                pending_question_index_key(&record.view.run_id, &request.id),
                pending_question_view_for_record(record, request),
            );
        }
    }
    pending
}

/// Backfills pending approval payloads for pre-patch waiting runs from durable event logs.
pub(crate) fn repair_pending_approval_payloads_from_events(
    run_store: &FileRunStore,
    runs: &mut BTreeMap<String, RunRecord>,
) -> Result<usize> {
    let mut repaired = 0;
    for record in runs.values_mut() {
        if record.view.status != DaemonRunStatus::WaitingForApproval
            || !record.view.pending_approvals.is_empty()
            || record.view.pending_approval_ids.is_empty()
        {
            continue;
        }
        let Some(requests) = run_store
            .load_events(&record.view.run_id)?
            .into_iter()
            .rev()
            .find_map(|entry| match entry.event {
                RunEvent::WaitingForApproval {
                    request_ids,
                    requests,
                } if request_ids == record.view.pending_approval_ids && !requests.is_empty() => {
                    Some(requests)
                }
                _ => None,
            })
        else {
            continue;
        };
        record.view.pending_approvals = requests;
        run_store.save_run(record)?;
        repaired += 1;
    }
    Ok(repaired)
}

/// Builds one pending-question projection from the durable run record that owns it.
pub(crate) fn pending_question_view_for_record(
    record: &RunRecord,
    request: &UserQuestionRequest,
) -> PendingQuestionView {
    let mut view = PendingQuestionView {
        session_id: record.view.session_id.clone(),
        agent_id: record.view.agent_id.clone(),
        run_id: Some(record.view.run_id.clone()),
        run_kind: Some(record.view.kind.clone()),
        requester_agent_id: None,
        requester_session_id: None,
        requester_run_id: None,
        requester_tool_call_id: None,
        requester_project_ids: Vec::new(),
        requester_channel_ids: Vec::new(),
        parent_project_ids: Vec::new(),
        parent_channel_ids: Vec::new(),
        request: request.clone(),
    };
    if let RunRequestPayload::ParentClarification {
        request: clarification,
        ..
    } = &record.payload
    {
        view.requester_agent_id = Some(clarification.requester_agent_id.clone());
        view.requester_session_id = Some(clarification.requester_session_id.clone());
        view.requester_run_id = clarification.requester_run_id.clone();
        view.requester_tool_call_id = clarification.requester_tool_call_id.clone();
        view.requester_project_ids = clarification.requester_project_ids.clone();
        view.requester_channel_ids = clarification.requester_channel_ids.clone();
        view.parent_project_ids = clarification.parent_project_ids.clone();
        view.parent_channel_ids = clarification.parent_channel_ids.clone();
    }
    view
}

/// Builds the in-memory pending-question index key.
pub(crate) fn pending_question_index_key(run_id: &str, request_id: &str) -> String {
    format!("{run_id}\u{1f}{request_id}")
}

/// One persisted run record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    /// The externally visible run view.
    pub view: RunView,
    /// The preferred external reply targets captured when the run was created.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reply_targets: Vec<ReplyHandle>,
    /// The replayable request payload.
    pub payload: RunRequestPayload,
}

impl RunRecord {
    pub(crate) fn is_channel_delivery_lineage(&self) -> bool {
        self.view.kind == DaemonRunKind::ChannelDelivery
            || self.payload.channel_delivery_request().is_some()
    }
}

/// One persisted run event entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunEventEntry {
    /// The event timestamp in milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// The affected run identifier.
    pub run_id: String,
    /// The session identifier.
    pub session_id: String,
    /// The agent identifier.
    pub agent_id: String,
    /// The event payload.
    pub event: RunEvent,
}

/// One durable event in the daemon run lifecycle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    Accepted,
    Queued {
        position: usize,
    },
    Started,
    WaitingForApproval {
        request_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        requests: Vec<ApprovalRequest>,
    },
    ApprovalResolved {
        resolutions: Vec<ApprovalResolution>,
    },
    WaitingForUserQuestion {
        request_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        requests: Vec<UserQuestionRequest>,
    },
    UserQuestionResolved {
        resolution: UserQuestionResolution,
    },
    ParentClarificationResolved {
        requester_agent_id: String,
        requester_session_id: String,
        request_id: String,
        declined: bool,
        resolution: UserQuestionResolution,
    },
    Output {
        output: DaemonOutputRecord,
    },
    Completed,
    Failed {
        error: String,
    },
    Interrupted,
    Cancelled,
}

/// In-memory session queue state derived from persisted runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionRunState {
    /// The currently active run for the session.
    pub active_run_id: Option<String>,
    /// The queued run identifiers for the session.
    pub queued_run_ids: VecDeque<String>,
}

/// Filesystem-backed persistence for daemon run records and events.
#[derive(Clone, Debug)]
pub struct FileRunStore {
    root: PathBuf,
}

impl FileRunStore {
    /// Creates a new run store rooted at the provided directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the filesystem path for one run record.
    pub fn run_path(&self, run_id: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.root.join("runs"), run_id, "json")
    }

    /// Returns the filesystem path for one run-event log.
    pub fn events_path(&self, run_id: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.root.join("run-events"), run_id, "jsonl")
    }

    /// Saves or replaces one run record.
    pub fn save_run(&self, record: &RunRecord) -> Result<()> {
        let path =
            prepare_storage_path_for_write(&self.root.join("runs"), &record.view.run_id, "json")?;
        write_json_pretty_atomically(&path, record)
            .with_context(|| format!("failed to write {}", path.display()))
    }

    /// Loads one run record when present.
    pub fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>> {
        let path = self.run_path(run_id);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&fs::read(&path)?)?))
    }

    /// Lists all persisted run records keyed by run identifier.
    pub fn load_runs(&self) -> Result<BTreeMap<String, RunRecord>> {
        let root = self.root.join("runs");
        if !root.exists() {
            return Ok(BTreeMap::new());
        }
        let mut runs = BTreeMap::new();
        for path in self.run_record_paths(&root)? {
            let Some(record) = read_json_or_quarantine::<RunRecord>(&path, "run record")? else {
                continue;
            };
            runs.insert(record.view.run_id.clone(), record);
        }
        Ok(runs)
    }

    /// Appends one run-event entry.
    pub fn append_event(&self, entry: &RunEventEntry) -> Result<()> {
        let path =
            prepare_storage_path_for_write(&self.root.join("run-events"), &entry.run_id, "jsonl")?;
        append_json_line_sync(&path, entry)
            .with_context(|| format!("failed to append {}", path.display()))
    }

    /// Appends multiple run-event entries for one run in a single synced write.
    pub fn append_events(&self, entries: &[RunEventEntry]) -> Result<()> {
        let Some(first) = entries.first() else {
            return Ok(());
        };
        if entries.iter().any(|entry| entry.run_id != first.run_id) {
            bail!("cannot append run events for multiple runs in one batch");
        }
        let path =
            prepare_storage_path_for_write(&self.root.join("run-events"), &first.run_id, "jsonl")?;
        append_json_lines_sync(&path, entries)
            .with_context(|| format!("failed to append {}", path.display()))
    }

    /// Loads the full persisted event log for one run.
    pub fn load_events(&self, run_id: &str) -> Result<Vec<RunEventEntry>> {
        let path = self.events_path(run_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        raw.lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Returns the next numeric seed inferred from persisted run identifiers.
    pub fn next_seed(&self) -> u64 {
        [
            self.root.join("runs"),
            self.root.join("run-events"),
            self.root.join("run-memories"),
        ]
        .into_iter()
        .flat_map(|root| self.storage_file_paths(&root).unwrap_or_default())
        .filter_map(|path| self.persisted_storage_id(&path))
        .filter_map(|run_id| run_id.strip_prefix("run-").and_then(|raw| raw.parse().ok()))
        .max()
        .unwrap_or(0u64)
    }

    fn persisted_storage_id(&self, path: &std::path::Path) -> Option<String> {
        let file_name = path.file_name()?.to_str()?;
        let base_name = file_name.split(".corrupt-").next().unwrap_or(file_name);
        let stem = base_name
            .strip_suffix(".json")
            .or_else(|| base_name.strip_suffix(".jsonl"))?;
        decode_safe_storage_name(stem).or_else(|| Some(stem.to_string()))
    }

    fn storage_file_paths(&self, root: &std::path::Path) -> Result<Vec<PathBuf>> {
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        for entry in
            fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                for nested in fs::read_dir(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?
                {
                    let nested = nested?;
                    let nested_path = nested.path();
                    if nested_path.is_file() {
                        paths.push(nested_path);
                    }
                }
                continue;
            }
            if path.is_file() {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    fn run_record_paths(&self, root: &std::path::Path) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for entry in
            fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                for nested in fs::read_dir(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?
                {
                    let nested = nested?;
                    let nested_path = nested.path();
                    if nested_path.extension().and_then(|ext| ext.to_str()) == Some("json") {
                        paths.push(nested_path);
                    }
                }
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
                paths.push(path);
            }
        }
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use kheish_session::{legacy_storage_path, safe_storage_name};
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn sample_run_record(run_id: &str) -> RunRecord {
        RunRecord {
            view: RunView {
                run_id: run_id.to_string(),
                session_id: "session-a".to_string(),
                agent_id: "agent-1".to_string(),
                kind: DaemonRunKind::Input,
                status: DaemonRunStatus::Completed,
                submitted_at_ms: 1,
                updated_at_ms: 1,
                started_at_ms: Some(1),
                finished_at_ms: Some(1),
                queued_position: None,
                request: RunRequestSummary {
                    source_plugin: "daemon".to_string(),
                    source_kind: "api".to_string(),
                    actor_id: "tester".to_string(),
                    text_preview: Some("hello".to_string()),
                    provider: Some("openai".to_string()),
                    model: Some("gpt-5.4".to_string()),
                    approval_count: None,
                    question_count: None,
                },
                input_attachments: Vec::new(),
                input_metadata: None,
                pending_approval_ids: Vec::new(),
                pending_approvals: Vec::new(),
                pending_question_ids: Vec::new(),
                pending_questions: Vec::new(),
                outputs: Vec::new(),
                deliveries: Vec::new(),
                error: None,
            },
            reply_targets: Vec::new(),
            payload: RunRequestPayload::Input {
                request: SubmitInputRequest {
                    provider: None,
                    source_plugin: Some("daemon".to_string()),
                    source_kind: Some("api".to_string()),
                    actor_id: Some("tester".to_string()),
                    content: "hello".to_string(),
                    input_items: Vec::new(),
                    attachments: Vec::new(),
                    generation: None,
                    completion_requirements: None,
                    metadata: None,
                    binding_keys: Vec::new(),
                    reply_targets: Vec::new(),
                    reply_plugin: None,
                    reply_address: None,
                },
                idempotency: None,
            },
        }
    }

    fn sample_approval_request() -> ApprovalRequest {
        ApprovalRequest {
            id: "approval-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            input: json!({"command": "printf ok"}),
            scope: "session".to_string(),
            reason: "shell command requires approval".to_string(),
        }
    }

    #[test]
    fn repair_pending_approval_payloads_backfills_legacy_waiting_runs() -> Result<()> {
        let temp = tempdir()?;
        let store = FileRunStore::new(temp.path());
        let approval = sample_approval_request();
        let mut record = sample_run_record("run-approval");
        record.view.status = DaemonRunStatus::WaitingForApproval;
        record.view.finished_at_ms = None;
        record.view.pending_approval_ids = vec![approval.id.clone()];
        store.save_run(&record)?;
        store.append_event(&RunEventEntry {
            timestamp_ms: 1,
            run_id: record.view.run_id.clone(),
            session_id: record.view.session_id.clone(),
            agent_id: record.view.agent_id.clone(),
            event: RunEvent::WaitingForApproval {
                request_ids: record.view.pending_approval_ids.clone(),
                requests: vec![approval.clone()],
            },
        })?;

        let mut runs = BTreeMap::from([(record.view.run_id.clone(), record)]);
        let repaired = repair_pending_approval_payloads_from_events(&store, &mut runs)?;

        assert_eq!(repaired, 1);
        assert_eq!(
            runs["run-approval"].view.pending_approvals,
            vec![approval.clone()]
        );
        assert_eq!(
            store
                .load_run("run-approval")?
                .expect("persisted repaired run")
                .view
                .pending_approvals,
            vec![approval]
        );
        assert_eq!(
            repair_pending_approval_payloads_from_events(&store, &mut runs)?,
            0
        );
        Ok(())
    }

    #[test]
    fn summarize_input_request_uses_ordered_input_items_for_preview() {
        let summary = summarize_input_request(&SubmitInputRequest {
            provider: None,
            source_plugin: Some("sdk".to_string()),
            source_kind: Some("test".to_string()),
            actor_id: Some("tester".to_string()),
            content: String::new(),
            input_items: vec![
                SubmitInputItemRequest::Text {
                    text: "Compare the board and asset.".to_string(),
                },
                SubmitInputItemRequest::BoardReference {
                    board_id: "board-1".to_string(),
                    revision_id: Some("rev-1".to_string()),
                },
                SubmitInputItemRequest::AssetReference {
                    asset_id: "asset-1".to_string(),
                },
            ],
            attachments: Vec::new(),
            generation: None,
            completion_requirements: None,
            metadata: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
            reply_address: None,
        });

        assert_eq!(
            summary.text_preview.as_deref(),
            Some("Compare the board and asset. [board:board-1@rev-1] [asset:asset-1]")
        );
    }

    #[test]
    fn run_status_transition_matrix_is_explicit() {
        use DaemonRunStatus::*;

        let statuses = [
            Queued,
            Running,
            WaitingForApproval,
            WaitingForUserQuestion,
            Completed,
            Failed,
            Interrupted,
            Cancelled,
        ];
        let allowed = [
            (Queued, Running),
            (Queued, Failed),
            (Queued, Cancelled),
            (Running, WaitingForApproval),
            (Running, WaitingForUserQuestion),
            (Running, Completed),
            (Running, Failed),
            (Running, Interrupted),
            (Running, Cancelled),
            (WaitingForApproval, Running),
            (WaitingForApproval, Failed),
            (WaitingForApproval, Interrupted),
            (WaitingForApproval, Cancelled),
            (WaitingForUserQuestion, Running),
            (WaitingForUserQuestion, Completed),
            (WaitingForUserQuestion, Failed),
            (WaitingForUserQuestion, Interrupted),
            (WaitingForUserQuestion, Cancelled),
        ];

        for from in &statuses {
            for to in &statuses {
                let expected = from == to || allowed.contains(&(from.clone(), to.clone()));
                assert_eq!(
                    from.allows_transition_to(to),
                    expected,
                    "unexpected transition rule for {from:?} -> {to:?}"
                );
            }
        }
    }

    #[test]
    fn rebuild_session_run_state_uses_deterministic_queue_order() {
        let mut first_waiting = sample_run_record("run-2");
        first_waiting.view.status = DaemonRunStatus::WaitingForApproval;
        first_waiting.view.submitted_at_ms = 10;
        first_waiting.view.queued_position = None;

        let mut second_waiting = sample_run_record("run-1");
        second_waiting.view.status = DaemonRunStatus::WaitingForUserQuestion;
        second_waiting.view.submitted_at_ms = 10;
        second_waiting.view.queued_position = None;

        let mut queued = sample_run_record("run-3");
        queued.view.status = DaemonRunStatus::Queued;
        queued.view.submitted_at_ms = 10;
        queued.view.queued_position = Some(2);

        let state = rebuild_session_run_state(&BTreeMap::from([
            ("run-2".to_string(), first_waiting),
            ("run-1".to_string(), second_waiting),
            ("run-3".to_string(), queued),
        ]));

        assert_eq!(
            state.get("session-a"),
            Some(&SessionRunState {
                active_run_id: Some("run-1".to_string()),
                queued_run_ids: VecDeque::from(["run-2".to_string(), "run-3".to_string()]),
            })
        );
    }

    #[test]
    fn rebuild_session_run_state_prefers_persisted_queued_positions() {
        let mut active = sample_run_record("run-active");
        active.view.status = DaemonRunStatus::Running;
        active.view.submitted_at_ms = 100;
        active.view.queued_position = None;

        let mut submitted_first = sample_run_record("run-submitted-first");
        submitted_first.view.status = DaemonRunStatus::Queued;
        submitted_first.view.submitted_at_ms = 10;
        submitted_first.view.queued_position = Some(2);

        let mut queued_first = sample_run_record("run-queued-first");
        queued_first.view.status = DaemonRunStatus::Queued;
        queued_first.view.submitted_at_ms = 20;
        queued_first.view.queued_position = Some(1);

        let state = rebuild_session_run_state(&BTreeMap::from([
            ("run-active".to_string(), active),
            ("run-submitted-first".to_string(), submitted_first),
            ("run-queued-first".to_string(), queued_first),
        ]));

        assert_eq!(
            state.get("session-a"),
            Some(&SessionRunState {
                active_run_id: Some("run-active".to_string()),
                queued_run_ids: VecDeque::from([
                    "run-queued-first".to_string(),
                    "run-submitted-first".to_string()
                ]),
            })
        );
    }

    #[test]
    fn run_store_uses_safe_filenames_for_hostile_run_ids() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileRunStore::new(root.path());
        let run_id = "../../../tmp/evil-run";
        let record = sample_run_record(run_id);
        store.save_run(&record)?;
        store.append_event(&RunEventEntry {
            run_id: run_id.to_string(),
            session_id: "session-a".to_string(),
            agent_id: "agent-1".to_string(),
            timestamp_ms: 1,
            event: RunEvent::Completed,
        })?;

        let expected_run = root
            .path()
            .join("runs")
            .join("__safe")
            .join(format!("{}.json", safe_storage_name(run_id)));
        let expected_events = root
            .path()
            .join("run-events")
            .join("__safe")
            .join(format!("{}.jsonl", safe_storage_name(run_id)));

        assert!(expected_run.exists());
        assert!(expected_events.exists());
        assert!(!root.path().join("runs/../../../tmp/evil-run.json").exists());
        assert_eq!(
            store.load_run(run_id)?.map(|value| value.view.run_id),
            Some(run_id.to_string())
        );
        assert_eq!(store.load_events(run_id)?.len(), 1);
        Ok(())
    }

    #[test]
    fn run_store_migrates_legacy_safe_paths() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileRunStore::new(root.path());
        let run_id = "run-legacy";
        let legacy_run =
            legacy_storage_path(&root.path().join("runs"), run_id, "json").expect("legacy run");
        let legacy_events = legacy_storage_path(&root.path().join("run-events"), run_id, "jsonl")
            .expect("legacy events");
        fs::create_dir_all(legacy_run.parent().expect("run parent"))?;
        fs::create_dir_all(legacy_events.parent().expect("events parent"))?;
        fs::write(
            &legacy_run,
            serde_json::to_vec_pretty(&sample_run_record(run_id))?,
        )?;
        fs::write(
            &legacy_events,
            format!(
                "{}\n",
                serde_json::to_string(&RunEventEntry {
                    run_id: run_id.to_string(),
                    session_id: "session-a".to_string(),
                    agent_id: "agent-1".to_string(),
                    timestamp_ms: 1,
                    event: RunEvent::Completed,
                })?
            ),
        )?;

        store.save_run(&sample_run_record(run_id))?;
        store.append_event(&RunEventEntry {
            run_id: run_id.to_string(),
            session_id: "session-a".to_string(),
            agent_id: "agent-1".to_string(),
            timestamp_ms: 2,
            event: RunEvent::Completed,
        })?;

        assert!(!legacy_run.exists());
        assert!(!legacy_events.exists());
        assert!(
            root.path()
                .join("runs")
                .join("__safe")
                .join(format!("{}.json", safe_storage_name(run_id)))
                .exists()
        );
        assert!(
            root.path()
                .join("run-events")
                .join("__safe")
                .join(format!("{}.jsonl", safe_storage_name(run_id)))
                .exists()
        );
        Ok(())
    }

    #[test]
    fn run_store_next_seed_preserves_quarantined_highest_run_id() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileRunStore::new(root.path());
        store.save_run(&sample_run_record("run-1"))?;
        store.save_run(&sample_run_record("run-2"))?;

        let run_two = store.run_path("run-2");
        let quarantined = run_two.with_file_name(format!(
            "{}.corrupt-test",
            run_two.file_name().expect("run-2 file").to_string_lossy()
        ));
        fs::rename(&run_two, &quarantined)?;

        assert_eq!(store.next_seed(), 2);
        Ok(())
    }
}

/// Returns the current timestamp in milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis() as u64
}

fn input_request_text_preview(request: &SubmitInputRequest) -> String {
    if !request.input_items.is_empty() {
        return request
            .input_items
            .iter()
            .filter_map(input_item_preview)
            .collect::<Vec<_>>()
            .join(" ");
    }
    if request.attachments.is_empty() {
        request.content.clone()
    } else {
        format!(
            "{} [{} attachment(s)]",
            request.content,
            request.attachments.len()
        )
    }
}

fn input_item_preview(item: &SubmitInputItemRequest) -> Option<String> {
    match item {
        SubmitInputItemRequest::Text { text } => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        SubmitInputItemRequest::AssetReference { asset_id } => Some(format!("[asset:{asset_id}]")),
        SubmitInputItemRequest::BoardReference {
            board_id,
            revision_id,
        } => {
            let suffix = revision_id
                .as_deref()
                .map(|revision_id| format!("@{revision_id}"))
                .unwrap_or_default();
            Some(format!("[board:{board_id}{suffix}]"))
        }
        SubmitInputItemRequest::InlineAsset(upload) => {
            let media_type = upload.media_type.as_deref().unwrap_or("unknown");
            Some(format!("[inline_asset:{} {media_type}]", upload.file_name))
        }
    }
}

/// Builds one compact request summary from a detached input request.
pub fn summarize_input_request(request: &SubmitInputRequest) -> RunRequestSummary {
    let text_preview = truncate_preview(&input_request_text_preview(request));
    RunRequestSummary {
        source_plugin: request
            .source_plugin
            .clone()
            .unwrap_or_else(|| "daemon".to_string()),
        source_kind: request
            .source_kind
            .clone()
            .unwrap_or_else(|| "api".to_string()),
        actor_id: request
            .actor_id
            .clone()
            .unwrap_or_else(|| "api-user".to_string()),
        text_preview: Some(text_preview),
        provider: request.provider.clone(),
        model: request
            .generation
            .as_ref()
            .and_then(|generation| generation.model.clone()),
        approval_count: None,
        question_count: None,
    }
}

/// Builds one compact request summary from a mailbox-delivery run.
pub fn summarize_mailbox_request(
    agent_id: &str,
    message_count: usize,
    subject_preview: Option<&str>,
) -> RunRequestSummary {
    let text_preview = if let Some(subject) = subject_preview.filter(|subject| !subject.is_empty())
    {
        Some(truncate_preview(&format!(
            "{message_count} mailbox message(s): {subject}"
        )))
    } else {
        Some(format!("{message_count} mailbox message(s)"))
    };
    RunRequestSummary {
        source_plugin: "daemon".to_string(),
        source_kind: "mailbox".to_string(),
        actor_id: agent_id.to_string(),
        text_preview,
        provider: None,
        model: None,
        approval_count: None,
        question_count: None,
    }
}

/// Builds one compact request summary from a channel-delivery run.
pub fn summarize_channel_delivery_request(
    _request: &ChannelDeliveryRunRequest,
    actor_id: &str,
    preview: Option<&str>,
) -> RunRequestSummary {
    RunRequestSummary {
        source_plugin: "daemon".to_string(),
        source_kind: "channel".to_string(),
        actor_id: actor_id.to_string(),
        text_preview: preview.map(truncate_preview),
        provider: None,
        model: None,
        approval_count: None,
        question_count: None,
    }
}

/// Builds one compact request summary from a child clarification run.
pub fn summarize_parent_clarification_request(
    request: &ParentClarificationRunRequest,
) -> RunRequestSummary {
    let text_preview = request
        .request
        .questions
        .first()
        .map(|question| question.question.clone());
    RunRequestSummary {
        source_plugin: "daemon".to_string(),
        source_kind: "parent_clarification".to_string(),
        actor_id: request.requester_agent_id.clone(),
        text_preview,
        provider: None,
        model: None,
        approval_count: None,
        question_count: Some(request.request.questions.len()),
    }
}

/// Builds one compact request summary from an approval-resume request.
pub fn summarize_approval_request(request: &ResolveApprovalsRequest) -> RunRequestSummary {
    RunRequestSummary {
        source_plugin: "daemon".to_string(),
        source_kind: "approval".to_string(),
        actor_id: "operator".to_string(),
        text_preview: None,
        provider: None,
        model: None,
        approval_count: Some(request.resolutions.len()),
        question_count: None,
    }
}

/// Builds one compact request summary from a user-question resume request.
pub fn summarize_user_question_request(request: &ResolveUserQuestionRequest) -> RunRequestSummary {
    RunRequestSummary {
        source_plugin: "daemon".to_string(),
        source_kind: "user_question".to_string(),
        actor_id: "operator".to_string(),
        text_preview: None,
        provider: None,
        model: None,
        approval_count: None,
        question_count: Some(request.resolution.answers.len()),
    }
}

/// Rebuilds one session-queue view from persisted runs.
pub fn rebuild_session_run_state(
    runs: &BTreeMap<String, RunRecord>,
) -> BTreeMap<String, SessionRunState> {
    let mut grouped = BTreeMap::<String, Vec<&RunRecord>>::new();
    for record in runs.values() {
        grouped
            .entry(record.view.session_id.clone())
            .or_default()
            .push(record);
    }

    let mut sessions = BTreeMap::new();
    for (session_id, mut records) in grouped {
        records
            .sort_by(|left, right| run_queue_rebuild_key(left).cmp(&run_queue_rebuild_key(right)));
        let mut state = SessionRunState::default();
        for record in records {
            match record.view.status {
                DaemonRunStatus::Queued => {
                    state.queued_run_ids.push_back(record.view.run_id.clone());
                }
                DaemonRunStatus::Running
                | DaemonRunStatus::WaitingForApproval
                | DaemonRunStatus::WaitingForUserQuestion => {
                    if state.active_run_id.is_none() {
                        state.active_run_id = Some(record.view.run_id.clone());
                    } else {
                        state.queued_run_ids.push_back(record.view.run_id.clone());
                    }
                }
                DaemonRunStatus::Completed
                | DaemonRunStatus::Failed
                | DaemonRunStatus::Interrupted
                | DaemonRunStatus::Cancelled => {}
            }
        }
        if state.active_run_id.is_some() || !state.queued_run_ids.is_empty() {
            sessions.insert(session_id, state);
        }
    }
    sessions
}

fn run_queue_rebuild_key(record: &RunRecord) -> (u8, usize, u64, &str) {
    match record.view.status {
        DaemonRunStatus::Running
        | DaemonRunStatus::WaitingForApproval
        | DaemonRunStatus::WaitingForUserQuestion => (
            0,
            record.view.queued_position.unwrap_or(0),
            record.view.submitted_at_ms,
            record.view.run_id.as_str(),
        ),
        DaemonRunStatus::Queued => (
            1,
            record.view.queued_position.unwrap_or(usize::MAX),
            record.view.submitted_at_ms,
            record.view.run_id.as_str(),
        ),
        DaemonRunStatus::Completed
        | DaemonRunStatus::Failed
        | DaemonRunStatus::Interrupted
        | DaemonRunStatus::Cancelled => (
            2,
            record.view.queued_position.unwrap_or(usize::MAX),
            record.view.submitted_at_ms,
            record.view.run_id.as_str(),
        ),
    }
}

pub(crate) fn truncate_preview(content: &str) -> String {
    const MAX_CHARS: usize = 160;
    let mut truncated = content.trim().chars().take(MAX_CHARS).collect::<String>();
    if content.trim().chars().count() > MAX_CHARS {
        truncated.push('…');
    }
    truncated
}
