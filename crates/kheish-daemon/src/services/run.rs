use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::future::Future;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow};
use kheish_agent::{MailboxMessage, ManagedAgentSnapshot};
use serde_json::Value;
use tokio::sync::{Mutex, Notify};

use super::{ExternalActionAuditRecord, ExternalActionService};
use crate::debug::{DebugCapturePolicyView, FileDebugStore, RunDebugView};
use crate::events::DaemonEventBus;
use crate::memory::FileRunMemoryStore;
use crate::problems::DaemonProblem;
use crate::rebuild_session_run_state;
use crate::runs::{
    DaemonRunKind, DaemonRunStatus, FileRunStore, ParentClarificationCompletionReason,
    ParentClarificationCompletionState, ParentClarificationRunRequest, RunEvent, RunEventEntry,
    RunRecord, RunRequestPayload, RunView, ScheduledRunOrigin, SessionRunState, now_ms,
    pending_question_index_key, pending_question_view_for_record,
};
use crate::{
    DaemonEvent, DaemonOutputRecord, DaemonRunStatusSummaryView, PendingQuestionView,
    ResolveApprovalsRequest, ResolveUserQuestionRequest, RunRetentionPruneResponse,
    SubmitInputRequest,
};
use kheish_types::{ApprovalResolution, UserQuestionRequest, UserQuestionResolution};

const STALE_NON_TERMINAL_RUN_THRESHOLD_MS: u64 = 30 * 60 * 1000;
const QUEUED_RUN_LAG_WARNING_THRESHOLD_MS: u64 = 30 * 60 * 1000;
const STALE_NON_TERMINAL_RUN_THRESHOLD_MS_ENV: &str = "KHEISH_STALE_NON_TERMINAL_RUN_THRESHOLD_MS";
const QUEUED_RUN_LAG_WARNING_THRESHOLD_MS_ENV: &str = "KHEISH_QUEUED_RUN_LAG_WARNING_THRESHOLD_MS";
const MAX_STALE_RUN_STATUS_IDS: usize = 5;
const AGENT_SUMMARY_OUTPUT_PREVIEW_CHARS: usize = 240;
const AGENT_SUMMARY_PARENT_CLARIFICATION_RUN_ID_LIMIT: usize = 8;

/// The result of scheduling one daemon run into the session queue.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RunScheduleResult {
    /// The externally visible run view after queue insertion.
    pub(crate) view: RunView,
    /// Indicates whether the run started immediately.
    pub(crate) started_immediately: bool,
}

/// Run-derived overlay fields used to enrich agent summaries in one batch pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AgentRunSummaryOverlay {
    pub(crate) active_run_id: Option<String>,
    pub(crate) active_agent_id: Option<String>,
    pub(crate) active_run_kind: Option<DaemonRunKind>,
    pub(crate) active_run_status: Option<DaemonRunStatus>,
    pub(crate) queued_run_count: usize,
    pub(crate) pending_approval_count: usize,
    pub(crate) pending_question_count: usize,
    pub(crate) pending_parent_clarification_count: usize,
    pub(crate) pending_parent_clarification_run_ids: Vec<String>,
    pub(crate) last_error: Option<String>,
    pub(crate) last_output_preview: Option<String>,
    pub(crate) last_output_truncated: bool,
    pub(crate) last_activity_at_ms: Option<u64>,
    last_error_key: Option<(u64, String)>,
    last_output_key: Option<(u64, String)>,
}

/// Durable parent-clarification completion that needs side-effect replay.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ParentClarificationCompletionResume {
    pub(crate) run_id: String,
    pub(crate) session_id: String,
    pub(crate) agent_id: String,
    pub(crate) request: ParentClarificationRunRequest,
    pub(crate) resolution: UserQuestionResolution,
    pub(crate) reason: ParentClarificationCompletionReason,
}

/// The persisted run state produced after applying one orchestrator snapshot.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RunSnapshotUpdate {
    /// The updated persisted run record.
    pub(crate) record: RunRecord,
    /// The session whose active slot can now be advanced when the run completed.
    pub(crate) next_session_id: Option<String>,
}

/// The persisted run state produced after cancelling one daemon run.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RunCancellationResult {
    /// The updated persisted run record.
    pub(crate) record: RunRecord,
    /// Indicates whether the cancelled run previously owned the active slot.
    pub(crate) was_active: bool,
}

/// The queue transition produced after one active run releases its session slot.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RunQueueAdvanceResult {
    /// The queued run that became active, when one was available.
    pub(crate) started_run: Option<RunView>,
    /// Indicates whether this call actually released the session's active run slot.
    pub(crate) finished_run_was_active: bool,
    /// Indicates whether the session now has no active or queued runs.
    pub(crate) session_idle: bool,
}

pub(crate) struct ParentClarificationCompletionGuard<'a> {
    service: &'a RunService,
    run_id: String,
}

impl Drop for ParentClarificationCompletionGuard<'_> {
    fn drop(&mut self) {
        self.service
            .parent_clarification_inflight
            .lock()
            .expect("parent clarification completion mutex poisoned")
            .remove(&self.run_id);
        self.service.parent_clarification_notify.notify_waiters();
    }
}

/// One read-only snapshot of the non-terminal scheduled executions for a schedule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScheduledExecutionSnapshot {
    /// The in-flight run identifiers keyed by scheduled fire timestamp.
    pub(crate) run_ids_by_fire_at_ms: BTreeMap<u64, String>,
}

/// One read-only snapshot of pending user-question expiration state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PendingQuestionExpirationSnapshot {
    /// Waiting runs whose earliest pending question has expired.
    pub(crate) due_run_ids: BTreeSet<String>,
    /// Next pending question expiration timestamp, if any.
    pub(crate) next_expiry_ms: Option<u64>,
}

/// Owns durable run state, queue indexes, and run-scoped observability.
pub(crate) struct RunService {
    run_store: FileRunStore,
    run_memory_store: FileRunMemoryStore,
    debug_store: FileDebugStore,
    events: DaemonEventBus,
    runs: Mutex<BTreeMap<String, RunRecord>>,
    session_runs: Mutex<BTreeMap<String, SessionRunState>>,
    pending_goal_continuations: Mutex<BTreeMap<String, BTreeSet<String>>>,
    pending_idle_submissions: Mutex<BTreeMap<String, String>>,
    event_append_lock: StdMutex<()>,
    indexes: StdMutex<RunIndexes>,
    pending_questions: StdMutex<BTreeMap<String, PendingQuestionView>>,
    pending_question_notify: Notify,
    parent_clarification_inflight: StdMutex<BTreeSet<String>>,
    parent_clarification_notify: Notify,
    next_run_id: AtomicU64,
}

#[derive(Default)]
struct RunIndexes {
    scheduled_by_fire: BTreeMap<String, BTreeMap<u64, String>>,
    mailbox_by_session: BTreeMap<String, BTreeSet<String>>,
}

impl RunService {
    /// Creates a new run service backed by persisted daemon run stores.
    pub(crate) fn new(
        run_store: FileRunStore,
        run_memory_store: FileRunMemoryStore,
        debug_store: FileDebugStore,
        events: DaemonEventBus,
        runs: BTreeMap<String, RunRecord>,
        session_runs: BTreeMap<String, SessionRunState>,
        pending_questions: BTreeMap<String, PendingQuestionView>,
        next_run_id: AtomicU64,
    ) -> Self {
        let indexes = build_run_indexes(&runs);
        Self {
            run_store,
            run_memory_store,
            debug_store,
            events,
            runs: Mutex::new(runs),
            session_runs: Mutex::new(session_runs),
            pending_goal_continuations: Mutex::new(BTreeMap::new()),
            pending_idle_submissions: Mutex::new(BTreeMap::new()),
            event_append_lock: StdMutex::new(()),
            indexes: StdMutex::new(indexes),
            pending_questions: StdMutex::new(pending_questions),
            pending_question_notify: Notify::new(),
            parent_clarification_inflight: StdMutex::new(BTreeSet::new()),
            parent_clarification_notify: Notify::new(),
            next_run_id,
        }
    }

    /// Returns one fresh daemon-managed run identifier.
    pub(crate) fn next_run_id(&self) -> String {
        format!(
            "run-{}",
            self.next_run_id.fetch_add(1, Ordering::Relaxed) + 1
        )
    }

    /// Returns the reply targets stored for one run.
    pub(crate) async fn run_reply_targets(&self, run_id: &str) -> Vec<kheish_types::ReplyHandle> {
        self.runs
            .lock()
            .await
            .get(run_id)
            .map(|record| record.reply_targets.clone())
            .unwrap_or_default()
    }

    /// Returns the active run identifier for one session when present.
    pub(crate) async fn active_run_id(&self, session_id: &str) -> Option<String> {
        self.session_runs
            .lock()
            .await
            .get(session_id)
            .and_then(|state| state.active_run_id.clone())
    }

    /// Returns the active run identifier for one session or a descriptive error.
    pub(crate) async fn require_active_run_id(&self, session_id: &str) -> Result<String> {
        self.active_run_id(session_id).await.ok_or_else(|| {
            DaemonProblem::run_state_conflict(format!("session {session_id} has no active run"))
                .into()
        })
    }

    /// Returns the running run, session, and agent identifiers tracked by the service.
    pub(crate) async fn running_run_triplets(&self) -> Vec<(String, String, String)> {
        self.runs
            .lock()
            .await
            .values()
            .filter(|record| record.view.status == DaemonRunStatus::Running)
            .map(|record| {
                (
                    record.view.run_id.clone(),
                    record.view.session_id.clone(),
                    record.view.agent_id.clone(),
                )
            })
            .collect()
    }

    /// Returns whether one run record exists in the durable run map.
    pub(crate) async fn run_exists(&self, run_id: &str) -> bool {
        self.runs.lock().await.contains_key(run_id)
    }

    /// Returns terminal scheduled runs that may need schedule-side reconciliation.
    pub(crate) async fn terminal_scheduled_runs(&self) -> Vec<RunRecord> {
        self.runs
            .lock()
            .await
            .values()
            .filter(|record| {
                record.view.status.is_terminal() && scheduled_run_origin(&record.payload).is_some()
            })
            .cloned()
            .collect()
    }

    /// Returns the live scheduled executions for one schedule keyed by fire timestamp.
    pub(crate) fn scheduled_execution_snapshot(
        &self,
        schedule_id: &str,
    ) -> ScheduledExecutionSnapshot {
        let run_ids_by_fire_at_ms = self
            .indexes
            .lock()
            .expect("run indexes mutex poisoned")
            .scheduled_by_fire
            .get(schedule_id)
            .cloned()
            .unwrap_or_default();
        ScheduledExecutionSnapshot {
            run_ids_by_fire_at_ms,
        }
    }

    /// Returns whether the session already owns one non-terminal mailbox delivery run.
    pub(crate) fn has_pending_mailbox_delivery(&self, session_id: &str) -> bool {
        self.indexes
            .lock()
            .expect("run indexes mutex poisoned")
            .mailbox_by_session
            .get(session_id)
            .is_some_and(|run_ids| !run_ids.is_empty())
    }

    /// Returns one snapshot of the session queue state.
    pub(crate) async fn session_state(&self, session_id: &str) -> Option<SessionRunState> {
        self.session_runs.lock().await.get(session_id).cloned()
    }

    /// Runs one mutation while holding the session queue idle, preventing a run
    /// from becoming active between an idle precondition and the mutation.
    pub(crate) async fn with_session_idle_guard<F, Fut, T>(
        &self,
        session_id: &str,
        operation: F,
    ) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let session_runs = self.session_runs.lock().await;
        let has_active_or_queued = session_runs
            .get(session_id)
            .map(|state| state.active_run_id.is_some() || !state.queued_run_ids.is_empty())
            .unwrap_or(false);
        let has_pending_goal_continuation = self
            .pending_goal_continuations
            .lock()
            .await
            .get(session_id)
            .is_some_and(|run_ids| !run_ids.is_empty());
        let has_pending_idle_submission = self
            .pending_idle_submissions
            .lock()
            .await
            .contains_key(session_id);
        if has_active_or_queued || has_pending_goal_continuation || has_pending_idle_submission {
            anyhow::bail!("session {session_id} has active or queued runs");
        }
        operation().await
    }

    /// Reserves an idle session slot before request normalization performs
    /// durable side effects such as asset imports or binding-key writes.
    pub(crate) async fn reserve_idle_submission_slot(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<bool> {
        let session_runs = self.session_runs.lock().await;
        let has_active_or_queued = session_runs
            .get(session_id)
            .is_some_and(|state| state.active_run_id.is_some() || !state.queued_run_ids.is_empty());
        if has_active_or_queued {
            return Ok(false);
        }
        let has_pending_goal_continuation = self
            .pending_goal_continuations
            .lock()
            .await
            .get(session_id)
            .is_some_and(|run_ids| !run_ids.is_empty());
        if has_pending_goal_continuation {
            return Ok(false);
        }
        let mut pending = self.pending_idle_submissions.lock().await;
        if pending.contains_key(session_id) {
            return Ok(false);
        }
        pending.insert(session_id.to_string(), run_id.to_string());
        Ok(true)
    }

    /// Releases a pre-schedule idle reservation. Returns whether queued work
    /// should be promoted because it arrived behind a reservation that failed.
    pub(crate) async fn release_idle_submission_slot(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> bool {
        let session_runs = self.session_runs.lock().await;
        let mut pending = self.pending_idle_submissions.lock().await;
        if pending.get(session_id).map(String::as_str) != Some(run_id) {
            return false;
        }
        pending.remove(session_id);
        session_runs
            .get(session_id)
            .is_some_and(|state| state.active_run_id.is_none() && !state.queued_run_ids.is_empty())
    }

    /// Reserves a short-lived session run slot while a goal continuation is
    /// being converted into a durable run record.
    pub(crate) async fn reserve_goal_continuation_slot(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<bool> {
        let session_runs = self.session_runs.lock().await;
        let has_active_or_queued = session_runs
            .get(session_id)
            .map(|state| state.active_run_id.is_some() || !state.queued_run_ids.is_empty())
            .unwrap_or(false);
        if has_active_or_queued {
            return Ok(false);
        }
        let has_pending_idle_submission = self
            .pending_idle_submissions
            .lock()
            .await
            .contains_key(session_id);
        if has_pending_idle_submission {
            return Ok(false);
        }
        let mut pending = self.pending_goal_continuations.lock().await;
        let run_ids = pending.entry(session_id.to_string()).or_default();
        if !run_ids.is_empty() {
            return Ok(false);
        }
        run_ids.insert(run_id.to_string());
        Ok(true)
    }

    /// Releases a short-lived goal-continuation reservation.
    pub(crate) async fn release_goal_continuation_slot(&self, session_id: &str, run_id: &str) {
        let mut pending = self.pending_goal_continuations.lock().await;
        let should_remove = if let Some(run_ids) = pending.get_mut(session_id) {
            run_ids.remove(run_id);
            run_ids.is_empty()
        } else {
            false
        };
        if should_remove {
            pending.remove(session_id);
        }
    }

    /// Returns a cheap point-in-time status summary for all daemon runs.
    pub(crate) async fn status_snapshot(&self, now_ms: u64) -> DaemonRunStatusSummaryView {
        let runs = self.runs.lock().await;
        let session_runs = self.session_runs.lock().await;
        let queued_run_lag_threshold_ms = run_status_threshold_ms(
            QUEUED_RUN_LAG_WARNING_THRESHOLD_MS_ENV,
            QUEUED_RUN_LAG_WARNING_THRESHOLD_MS,
        );
        let stale_non_terminal_run_threshold_ms = run_status_threshold_ms(
            STALE_NON_TERMINAL_RUN_THRESHOLD_MS_ENV,
            STALE_NON_TERMINAL_RUN_THRESHOLD_MS,
        );
        let mut snapshot = DaemonRunStatusSummaryView {
            queued_run_lag_threshold_ms,
            stale_non_terminal_run_threshold_ms,
            ..Default::default()
        };
        let mut stale_run_ids_by_idle = Vec::new();

        for record in runs.values() {
            snapshot.total += 1;
            snapshot.pending_approval_count += record.view.pending_approval_ids.len();
            snapshot.pending_question_count += record.view.pending_question_ids.len();

            match record.view.status {
                DaemonRunStatus::Queued => {
                    snapshot.queued += 1;
                    let queued_age_ms = now_ms.saturating_sub(record.view.submitted_at_ms);
                    if snapshot
                        .oldest_queued_run_age_ms
                        .is_none_or(|current| queued_age_ms > current)
                    {
                        snapshot.oldest_queued_run_age_ms = Some(queued_age_ms);
                        snapshot.oldest_queued_run_id = Some(record.view.run_id.clone());
                    }
                }
                DaemonRunStatus::Running => snapshot.running += 1,
                DaemonRunStatus::WaitingForApproval => snapshot.waiting_for_approval += 1,
                DaemonRunStatus::WaitingForUserQuestion => {
                    snapshot.waiting_for_user_question += 1;
                }
                DaemonRunStatus::Completed => snapshot.completed += 1,
                DaemonRunStatus::Failed => snapshot.failed += 1,
                DaemonRunStatus::Interrupted => snapshot.interrupted += 1,
                DaemonRunStatus::Cancelled => snapshot.cancelled += 1,
            }

            if !record.view.status.is_terminal() {
                let age_ms = now_ms.saturating_sub(record.view.submitted_at_ms);
                if snapshot
                    .oldest_non_terminal_run_age_ms
                    .is_none_or(|current| age_ms > current)
                {
                    snapshot.oldest_non_terminal_run_age_ms = Some(age_ms);
                    snapshot.oldest_non_terminal_run_id = Some(record.view.run_id.clone());
                }
                let idle_ms = now_ms.saturating_sub(run_activity_at_ms(&record.view));
                if snapshot
                    .oldest_non_terminal_run_idle_ms
                    .is_none_or(|current| idle_ms > current)
                {
                    snapshot.oldest_non_terminal_run_idle_ms = Some(idle_ms);
                    snapshot.oldest_idle_non_terminal_run_id = Some(record.view.run_id.clone());
                }
                if idle_ms > stale_non_terminal_run_threshold_ms {
                    snapshot.stale_non_terminal_run_count += 1;
                    record_stale_run_sample(
                        &mut stale_run_ids_by_idle,
                        idle_ms,
                        record.view.run_id.clone(),
                    );
                }
            }
        }

        snapshot.stale_non_terminal_run_ids = stale_run_ids_by_idle
            .into_iter()
            .take(MAX_STALE_RUN_STATUS_IDS)
            .map(|(_, run_id)| run_id)
            .collect();

        snapshot.max_session_queue_depth = session_runs
            .values()
            .map(|state| state.queued_run_ids.len())
            .max()
            .unwrap_or_default();
        snapshot
    }

    /// Returns the session identifiers currently tracked by the queue index.
    pub(crate) async fn session_ids(&self) -> Vec<String> {
        self.session_runs.lock().await.keys().cloned().collect()
    }

    /// Returns the active and queued run identifiers for one session.
    pub(crate) async fn collect_session_run_ids(&self, session_id: &str) -> Vec<String> {
        let session_runs = self.session_runs.lock().await;
        let Some(state) = session_runs.get(session_id) else {
            return Vec::new();
        };
        state
            .active_run_id
            .iter()
            .chain(state.queued_run_ids.iter())
            .cloned()
            .collect()
    }

    /// Returns whether any persisted run currently belongs to one session.
    pub(crate) async fn has_session_runs(&self, session_id: &str) -> bool {
        self.runs
            .lock()
            .await
            .values()
            .any(|record| record.view.session_id == session_id)
    }

    /// Loads one run record from the in-memory index.
    pub(crate) async fn run_record(&self, run_id: &str) -> Result<RunRecord> {
        self.runs.lock().await.get(run_id).cloned().ok_or_else(|| {
            anyhow::Error::from(DaemonProblem::run_not_found(format!(
                "unknown run {run_id}"
            )))
        })
    }

    /// Returns one externally visible run view.
    pub(crate) async fn get_run(&self, run_id: &str) -> Result<RunView> {
        Ok(self.run_record(run_id).await?.view)
    }

    /// Returns the first run view that recorded the given connector ingress key in its metadata.
    pub(crate) async fn find_run_by_connector_ingress_key(
        &self,
        ingress_key: &str,
    ) -> Option<RunView> {
        self.runs.lock().await.values().find_map(|record| {
            record
                .view
                .input_metadata
                .as_ref()
                .and_then(|metadata| metadata.get("connector_ingress_key"))
                .and_then(Value::as_str)
                .filter(|value| *value == ingress_key)
                .map(|_| record.view.clone())
        })
    }

    /// Returns the first direct input run matching one session-scoped idempotency key.
    pub(crate) async fn find_input_run_by_idempotency(
        &self,
        session_id: &str,
        key_hash: &str,
        request_fingerprint: &str,
    ) -> Result<Option<RunView>> {
        let runs = self.runs.lock().await;
        let mut conflicting_run_id = None;
        for record in runs.values() {
            if record.view.session_id != session_id {
                continue;
            }
            let RunRequestPayload::Input {
                idempotency: Some(idempotency),
                ..
            } = &record.payload
            else {
                continue;
            };
            if idempotency.key_hash != key_hash {
                continue;
            }
            if idempotency.request_fingerprint == request_fingerprint {
                return Ok(Some(record.view.clone()));
            }
            conflicting_run_id = Some(record.view.run_id.clone());
        }
        if let Some(run_id) = conflicting_run_id {
            return Err(DaemonProblem::idempotency_conflict(format!(
                "session run idempotency key is already bound to run {run_id} with a different request payload"
            ))
            .into());
        }
        Ok(None)
    }

    /// Lists the visible runs, optionally scoped to one session identifier.
    pub(crate) async fn list_runs(&self, session_id: Option<&str>) -> Result<Vec<RunView>> {
        let mut runs = self
            .runs
            .lock()
            .await
            .values()
            .filter(|record| {
                session_id
                    .map(|session_id| record.view.session_id == session_id)
                    .unwrap_or(true)
            })
            .map(|record| record.view.clone())
            .collect::<Vec<_>>();
        runs.sort_by_key(|run| run.submitted_at_ms);
        Ok(runs)
    }

    /// Returns a point-in-time snapshot of all run records.
    pub(crate) async fn run_records_snapshot(&self) -> BTreeMap<String, RunRecord> {
        self.runs.lock().await.clone()
    }

    /// Returns a parent-clarification run previously created by the same child tool call.
    pub(crate) async fn find_parent_clarification_by_request(
        &self,
        requester_agent_id: &str,
        requester_session_id: &str,
        requester_run_id: Option<&str>,
        requester_tool_call_id: Option<&str>,
        request_id: &str,
    ) -> Option<RunRecord> {
        self.runs.lock().await.values().find_map(|record| {
            let RunRequestPayload::ParentClarification { request, .. } = &record.payload else {
                return None;
            };
            if request.requester_agent_id != requester_agent_id
                || request.requester_session_id != requester_session_id
                || request.request.id != request_id
            {
                return None;
            }
            if requester_run_id.is_some() && request.requester_run_id.as_deref() != requester_run_id
            {
                return None;
            }
            if requester_tool_call_id.is_some()
                && request.requester_tool_call_id.as_deref() != requester_tool_call_id
                && request.request.tool_call_id.as_str() != requester_tool_call_id.unwrap_or("")
            {
                return None;
            }
            Some(record.clone())
        })
    }

    /// Returns pending parent-clarification questions keyed by the child agent awaiting them.
    pub(crate) async fn pending_parent_clarifications_by_requester(
        &self,
    ) -> BTreeMap<String, Vec<PendingQuestionView>> {
        let runs = self.runs.lock().await;
        let mut pending = BTreeMap::<String, Vec<PendingQuestionView>>::new();
        for record in runs.values() {
            if record.view.status != DaemonRunStatus::WaitingForUserQuestion {
                continue;
            }
            let RunRequestPayload::ParentClarification { request, .. } = &record.payload else {
                continue;
            };
            for question in &record.view.pending_questions {
                pending
                    .entry(request.requester_agent_id.clone())
                    .or_default()
                    .push(pending_question_view_for_record(record, question));
            }
        }
        for questions in pending.values_mut() {
            questions.sort_by(|left, right| {
                left.run_id
                    .cmp(&right.run_id)
                    .then_with(|| left.request.id.cmp(&right.request.id))
            });
        }
        pending
    }

    /// Lists parent clarifications whose answer was claimed but whose side effects need replay.
    pub(crate) async fn incomplete_parent_clarification_completions(
        &self,
    ) -> Vec<ParentClarificationCompletionResume> {
        let runs = self.runs.lock().await;
        let mut resumable = Vec::new();
        for record in runs.values() {
            let RunRequestPayload::ParentClarification {
                request,
                completion,
            } = &record.payload
            else {
                continue;
            };
            let Some(resolution) = completion.resolution.clone() else {
                continue;
            };
            if completion.mailbox_posted
                && completion.output_emitted
                && completion.hook_dispatched
                && completion.resolution_recorded
                && completion.completion_recorded
            {
                continue;
            }
            resumable.push(ParentClarificationCompletionResume {
                run_id: record.view.run_id.clone(),
                session_id: record.view.session_id.clone(),
                agent_id: record.view.agent_id.clone(),
                request: request.clone(),
                reason: completion
                    .reason
                    .clone()
                    .unwrap_or_else(|| parent_clarification_reason_for_resolution(&resolution)),
                resolution,
            });
        }
        resumable.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        resumable
    }

    /// Returns all run events for one session, ordered for transcript display.
    pub(crate) async fn session_run_events(&self, session_id: &str) -> Result<Vec<RunEventEntry>> {
        let run_ids = self
            .runs
            .lock()
            .await
            .values()
            .filter(|record| record.view.session_id == session_id)
            .map(|record| record.view.run_id.clone())
            .collect::<Vec<_>>();
        let mut events = Vec::new();
        for run_id in run_ids {
            let mut monotonic_timestamp_ms = None::<u64>;
            for (index, entry) in self.run_events(&run_id)?.into_iter().enumerate() {
                let timestamp_ms = match monotonic_timestamp_ms {
                    Some(previous) => entry.timestamp_ms.max(previous.saturating_add(1)),
                    None => entry.timestamp_ms,
                };
                monotonic_timestamp_ms = Some(timestamp_ms);
                events.push((entry, timestamp_ms, index));
            }
        }
        events.sort_by(
            |(left, left_timestamp_ms, left_index), (right, right_timestamp_ms, right_index)| {
                left_timestamp_ms
                    .cmp(right_timestamp_ms)
                    .then_with(|| left.run_id.cmp(&right.run_id))
                    .then_with(|| left_index.cmp(right_index))
            },
        );
        Ok(events.into_iter().map(|(entry, _, _)| entry).collect())
    }

    /// Builds one run overlay per session for agent-summary list views.
    pub(crate) async fn agent_summary_overlays(&self) -> BTreeMap<String, AgentRunSummaryOverlay> {
        let runs = self.runs.lock().await;
        let session_runs = self.session_runs.lock().await;
        let mut overlays = BTreeMap::<String, AgentRunSummaryOverlay>::new();

        for record in runs.values() {
            let run = &record.view;
            let overlay = overlays.entry(run.session_id.clone()).or_default();
            let activity_at_ms = run_activity_at_ms(run);
            if overlay
                .last_activity_at_ms
                .is_none_or(|current| activity_at_ms > current)
            {
                overlay.last_activity_at_ms = Some(activity_at_ms);
            }

            if let Some(error) = run.error.as_ref() {
                let key = (activity_at_ms, run.run_id.clone());
                if overlay
                    .last_error_key
                    .as_ref()
                    .is_none_or(|current| &key > current)
                {
                    overlay.last_error_key = Some(key);
                    overlay.last_error = Some(error.clone());
                }
            }

            if let Some(output) = run.outputs.last() {
                let key = (activity_at_ms, run.run_id.clone());
                if overlay
                    .last_output_key
                    .as_ref()
                    .is_none_or(|current| &key > current)
                {
                    let (preview, truncated) =
                        output_preview(&output.content, AGENT_SUMMARY_OUTPUT_PREVIEW_CHARS);
                    overlay.last_output_key = Some(key);
                    overlay.last_output_preview = Some(preview);
                    overlay.last_output_truncated = truncated;
                }
            }

            if run.status == DaemonRunStatus::WaitingForUserQuestion
                && let RunRequestPayload::ParentClarification { request, .. } = &record.payload
            {
                let requester = overlays
                    .entry(request.requester_session_id.clone())
                    .or_default();
                requester.pending_parent_clarification_count += 1;
                if requester.pending_parent_clarification_run_ids.len()
                    < AGENT_SUMMARY_PARENT_CLARIFICATION_RUN_ID_LIMIT
                {
                    requester
                        .pending_parent_clarification_run_ids
                        .push(run.run_id.clone());
                }
                let activity_at_ms = run_activity_at_ms(run);
                if requester
                    .last_activity_at_ms
                    .is_none_or(|current| activity_at_ms > current)
                {
                    requester.last_activity_at_ms = Some(activity_at_ms);
                }
            }
        }

        for (session_id, session_state) in session_runs.iter() {
            let overlay = overlays.entry(session_id.clone()).or_default();
            overlay.active_run_id = session_state.active_run_id.clone();
            overlay.queued_run_count = session_state.queued_run_ids.len();
            let Some(active_run_id) = session_state.active_run_id.as_deref() else {
                continue;
            };
            let Some(active_record) = runs.get(active_run_id) else {
                continue;
            };
            overlay.active_agent_id = Some(active_record.view.agent_id.clone());
            overlay.active_run_kind = Some(active_record.view.kind.clone());
            overlay.active_run_status = Some(active_record.view.status.clone());
            overlay.pending_approval_count = active_record.view.pending_approval_ids.len();
            overlay.pending_question_count = active_record.view.pending_questions.len();
        }

        overlays
    }

    /// Deletes debug evidence for terminal runs older than the supplied age threshold.
    pub(crate) async fn prune_terminal_run_debug_evidence(
        &self,
        older_than_ms: u64,
        session_id: Option<&str>,
        limit: Option<usize>,
        dry_run: bool,
        now_ms: u64,
    ) -> Result<RunRetentionPruneResponse> {
        if older_than_ms == 0 {
            return Err(DaemonProblem::run_retention_invalid_request(
                "older_than_ms must be greater than zero",
            )
            .into());
        }
        if limit == Some(0) {
            return Err(DaemonProblem::run_retention_invalid_request(
                "limit must be greater than zero",
            )
            .into());
        }
        let cutoff_ms = now_ms.saturating_sub(older_than_ms);
        let mut candidates = {
            let runs = self.runs.lock().await;
            let mut candidates = runs
                .values()
                .filter(|record| {
                    record.view.status.is_terminal()
                        && session_id
                            .map(|session_id| record.view.session_id == session_id)
                            .unwrap_or(true)
                        && record
                            .view
                            .finished_at_ms
                            .unwrap_or(record.view.updated_at_ms)
                            <= cutoff_ms
                        && self.debug_store.has_run(&record.view.run_id)
                })
                .map(|record| {
                    (
                        record
                            .view
                            .finished_at_ms
                            .unwrap_or(record.view.updated_at_ms),
                        record.view.run_id.clone(),
                    )
                })
                .collect::<Vec<_>>();
            candidates
                .sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
            candidates
        };
        let matched_run_count = candidates.len();
        if let Some(limit) = limit
            && candidates.len() > limit
        {
            candidates.truncate(limit);
        }
        let candidate_run_ids = candidates
            .into_iter()
            .map(|(_, run_id)| run_id)
            .collect::<Vec<_>>();
        let candidate_debug_bytes = candidate_run_ids.iter().try_fold(0_u64, |total, run_id| {
            self.debug_store
                .run_bytes(run_id)
                .map(|bytes| total.saturating_add(bytes))
        })?;

        if dry_run || candidate_run_ids.is_empty() {
            return Ok(RunRetentionPruneResponse {
                dry_run,
                now_ms,
                cutoff_ms,
                limit,
                matched_run_count,
                candidate_run_ids,
                candidate_debug_bytes,
                pruned_debug_run_ids: Vec::new(),
                pruned_debug_bytes: 0,
            });
        }

        let mut pruned_debug_run_ids = Vec::new();
        let mut pruned_debug_bytes = 0_u64;
        for run_id in &candidate_run_ids {
            let debug_bytes = self.debug_store.run_bytes(run_id)?;
            if self.debug_store.delete_run(run_id)? {
                pruned_debug_bytes = pruned_debug_bytes.saturating_add(debug_bytes);
                pruned_debug_run_ids.push(run_id.clone());
            }
        }

        Ok(RunRetentionPruneResponse {
            dry_run,
            now_ms,
            cutoff_ms,
            limit,
            matched_run_count,
            candidate_run_ids,
            candidate_debug_bytes,
            pruned_debug_run_ids,
            pruned_debug_bytes,
        })
    }

    /// Deletes debug evidence for terminal runs older than the configured debug TTL.
    pub(crate) async fn prune_expired_debug_evidence(
        &self,
        now_ms: u64,
    ) -> Result<Option<RunRetentionPruneResponse>> {
        let mut response = None;
        let ttl_ms = self.debug_store.ttl_ms();
        if ttl_ms > 0 {
            let cutoff_ms = now_ms.saturating_sub(ttl_ms);
            let (protected_run_ids, expired_known_run_ids, retained_known_run_ids) =
                self.debug_retention_run_sets(cutoff_ms).await;
            let pruned = self.debug_store.prune_expired_bundles(
                &protected_run_ids,
                &expired_known_run_ids,
                &retained_known_run_ids,
                now_ms,
            )?;
            response = Some(RunRetentionPruneResponse {
                dry_run: false,
                now_ms,
                cutoff_ms,
                limit: None,
                matched_run_count: pruned.candidate_run_ids.len(),
                candidate_run_ids: pruned.candidate_run_ids,
                candidate_debug_bytes: pruned.candidate_debug_bytes,
                pruned_debug_run_ids: pruned.pruned_debug_run_ids,
                pruned_debug_bytes: pruned.pruned_debug_bytes,
            });
        }
        if let Some(budget_response) = self.prune_debug_store_over_budget(now_ms).await? {
            response = Some(match response {
                Some(existing) => combine_debug_prune_responses(existing, budget_response),
                None => budget_response,
            });
        }
        Ok(response)
    }

    /// Deletes old terminal/orphan debug evidence until the configured global debug cap is respected.
    pub(crate) async fn prune_debug_store_over_budget(
        &self,
        now_ms: u64,
    ) -> Result<Option<RunRetentionPruneResponse>> {
        let (protected_run_ids, known_terminal_retention_ms) =
            self.debug_store_budget_run_sets().await;
        let Some(pruned) = self
            .debug_store
            .prune_over_budget(&protected_run_ids, &known_terminal_retention_ms)?
        else {
            return Ok(None);
        };
        Ok(Some(RunRetentionPruneResponse {
            dry_run: false,
            now_ms,
            cutoff_ms: now_ms,
            limit: None,
            matched_run_count: pruned.candidate_run_ids.len(),
            candidate_run_ids: pruned.candidate_run_ids,
            candidate_debug_bytes: pruned.candidate_debug_bytes,
            pruned_debug_run_ids: pruned.pruned_debug_run_ids,
            pruned_debug_bytes: pruned.pruned_debug_bytes,
        }))
    }

    async fn debug_retention_run_sets(
        &self,
        cutoff_ms: u64,
    ) -> (BTreeSet<String>, BTreeSet<String>, BTreeSet<String>) {
        let runs = self.runs.lock().await;
        let mut protected = BTreeSet::new();
        let mut expired = BTreeSet::new();
        let mut retained = BTreeSet::new();
        for record in runs.values() {
            if !record.view.status.is_terminal() {
                protected.insert(record.view.run_id.clone());
                continue;
            }
            let finished_at_ms = record
                .view
                .finished_at_ms
                .unwrap_or(record.view.updated_at_ms);
            if finished_at_ms > cutoff_ms {
                retained.insert(record.view.run_id.clone());
            } else {
                expired.insert(record.view.run_id.clone());
            }
        }
        (protected, expired, retained)
    }

    async fn debug_store_budget_run_sets(&self) -> (BTreeSet<String>, BTreeMap<String, u64>) {
        let runs = self.runs.lock().await;
        let mut protected = BTreeSet::new();
        let mut known_terminal_retention_ms = BTreeMap::new();
        for record in runs.values() {
            if record.view.status.is_terminal() {
                known_terminal_retention_ms.insert(
                    record.view.run_id.clone(),
                    record
                        .view
                        .finished_at_ms
                        .unwrap_or(record.view.updated_at_ms),
                );
            } else {
                protected.insert(record.view.run_id.clone());
            }
        }
        (protected, known_terminal_retention_ms)
    }

    /// Returns the periodic debug retention worker interval.
    pub(crate) fn debug_retention_interval_ms(&self) -> u64 {
        self.debug_store.gc_interval_ms()
    }

    /// Returns an invalid debug encryption-key configuration error, when present.
    pub(crate) fn debug_store_encryption_key_error(&self) -> Option<&str> {
        self.debug_store.encryption_key_error()
    }

    /// Returns the effective debug capture storage/scrubber policy.
    pub(crate) fn debug_capture_policy_view(&self) -> DebugCapturePolicyView {
        self.debug_store.policy_view()
    }

    /// Waits until the provided run is no longer queued or actively running.
    pub(crate) async fn wait_for_run_settled(&self, run_id: &str) -> Result<RunView> {
        loop {
            let run = self.get_run(run_id).await?;
            match run.status {
                DaemonRunStatus::Queued | DaemonRunStatus::Running => {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                DaemonRunStatus::WaitingForApproval
                | DaemonRunStatus::WaitingForUserQuestion
                | DaemonRunStatus::Completed
                | DaemonRunStatus::Failed
                | DaemonRunStatus::Interrupted
                | DaemonRunStatus::Cancelled => return Ok(run),
            }
        }
    }

    /// Serializes parent-clarification completion for one run identifier.
    pub(crate) async fn acquire_parent_clarification_completion_slot(
        &self,
        run_id: &str,
    ) -> ParentClarificationCompletionGuard<'_> {
        loop {
            let notified = self.parent_clarification_notify.notified();
            let acquired = self
                .parent_clarification_inflight
                .lock()
                .expect("parent clarification completion mutex poisoned")
                .insert(run_id.to_string());
            if acquired {
                return ParentClarificationCompletionGuard {
                    service: self,
                    run_id: run_id.to_string(),
                };
            }
            notified.await;
        }
    }

    /// Returns whether one mailbox-delivery run already captured the provided message.
    pub(crate) async fn session_has_mailbox_delivery_message(
        &self,
        session_id: &str,
        target_agent_id: &str,
        from_agent_id: &str,
        subject: &str,
        payload: &Value,
    ) -> Result<bool> {
        Ok(self.run_store.load_runs()?.values().any(|record| {
            if record.view.session_id != session_id {
                return false;
            }
            if !matches!(
                record.view.status,
                DaemonRunStatus::Queued | DaemonRunStatus::Running | DaemonRunStatus::Completed
            ) {
                return false;
            }
            let RunRequestPayload::MailboxDelivery { agent_id, messages } = &record.payload else {
                return false;
            };
            agent_id == target_agent_id
                && messages.iter().any(|message| {
                    message.from.0 == from_agent_id
                        && message.to.0 == target_agent_id
                        && message.subject == subject
                        && message.payload == *payload
                })
        }))
    }

    /// Returns whether one mailbox-delivery run already captured the provided message id.
    pub(crate) async fn session_has_mailbox_delivery_message_id(
        &self,
        session_id: &str,
        target_agent_id: &str,
        message_id: &str,
    ) -> Result<bool> {
        if message_id.trim().is_empty() {
            return Ok(false);
        }
        Ok(self.run_store.load_runs()?.values().any(|record| {
            if record.view.session_id != session_id {
                return false;
            }
            if !matches!(
                record.view.status,
                DaemonRunStatus::Queued | DaemonRunStatus::Running | DaemonRunStatus::Completed
            ) {
                return false;
            }
            let RunRequestPayload::MailboxDelivery { agent_id, messages } = &record.payload else {
                return false;
            };
            agent_id == target_agent_id && messages.iter().any(|message| message.id == message_id)
        }))
    }

    /// Returns the longest pending mailbox prefix already captured by one durable delivery run.
    pub(crate) async fn session_mailbox_delivery_prefix_len(
        &self,
        session_id: &str,
        target_agent_id: &str,
        messages: &[MailboxMessage],
    ) -> Result<usize> {
        Ok(self
            .run_store
            .load_runs()?
            .values()
            .filter_map(|record| {
                if record.view.session_id != session_id {
                    return None;
                }
                if record.view.status != DaemonRunStatus::Completed {
                    return None;
                }
                let RunRequestPayload::MailboxDelivery {
                    agent_id,
                    messages: recorded_messages,
                } = &record.payload
                else {
                    return None;
                };
                if agent_id != target_agent_id
                    || recorded_messages.is_empty()
                    || recorded_messages.len() > messages.len()
                    || !recorded_messages
                        .iter()
                        .zip(messages.iter())
                        .all(|(recorded, pending)| pending.matches_delivery_payload(recorded))
                {
                    return None;
                }
                Some(recorded_messages.len())
            })
            .max()
            .unwrap_or(0))
    }

    /// Returns whether the durable run event log already contains a completed event.
    pub(crate) fn run_has_completed_event(&self, run_id: &str) -> Result<bool> {
        Ok(self
            .run_store
            .load_events(run_id)?
            .into_iter()
            .any(|entry| matches!(entry.event, RunEvent::Completed)))
    }

    /// Persists one run event entry.
    pub(crate) fn append_run_event(&self, view: &RunView, event: RunEvent) -> Result<()> {
        self.run_store.append_event(&run_event_entry(view, event))
    }

    /// Persists one run event only when the exact event is not already present.
    pub(crate) fn append_run_event_once(&self, view: &RunView, event: RunEvent) -> Result<()> {
        let _guard = self
            .event_append_lock
            .lock()
            .expect("run event append mutex poisoned");
        self.append_run_event_once_locked(view, event)
    }

    fn append_run_event_once_locked(&self, view: &RunView, event: RunEvent) -> Result<()> {
        if self
            .run_store
            .load_events(&view.run_id)?
            .iter()
            .any(|entry| run_event_matches(&entry.event, &event))
        {
            return Ok(());
        }
        self.run_store.append_event(&run_event_entry(view, event))
    }

    fn append_resume_events_locked(
        &self,
        view: &RunView,
        audit_event: Option<RunEvent>,
    ) -> Result<()> {
        let existing = self.run_store.load_events(&view.run_id)?;
        let mut entries = vec![run_event_entry(view, RunEvent::Started)];
        if let Some(event) = audit_event
            && !existing
                .iter()
                .any(|entry| run_event_matches(&entry.event, &event))
        {
            entries.push(run_event_entry(view, event));
        }
        self.run_store.append_events(&entries)
    }

    /// Publishes one run update and refreshes the pending-question projection.
    pub(crate) fn publish_run(&self, run: &RunView) {
        self.sync_pending_question_index(run);
        self.pending_question_notify.notify_waiters();
        self.events
            .publish(DaemonEvent::RunUpdated { run: run.clone() });
    }

    async fn update_run_record<T>(
        &self,
        run_id: &str,
        update: impl FnOnce(&mut RunRecord) -> Result<(T, bool)>,
    ) -> Result<T> {
        let mut runs = self.runs.lock().await;
        let record = runs
            .get_mut(run_id)
            .ok_or_else(|| DaemonProblem::run_not_found(format!("unknown run {run_id}")))?;
        let previous = record.clone();
        let (result, changed) = update(record)?;
        if changed {
            if let Err(error) = self.run_store.save_run(record) {
                *record = previous;
                return Err(error);
            }
            let updated = record.clone();
            self.refresh_record_indexes(Some(&previous), &updated);
        }
        Ok(result)
    }

    async fn update_parent_clarification_completion(
        &self,
        run_id: &str,
        update: impl FnOnce(&mut ParentClarificationCompletionState) -> Result<()>,
    ) -> Result<Option<ParentClarificationCompletionState>> {
        self.update_run_record(run_id, |record| {
            let completion = match &mut record.payload {
                RunRequestPayload::ParentClarification { completion, .. } => completion,
                _ => return Ok((None, false)),
            };
            let previous = completion.clone();
            update(completion)?;
            let changed = *completion != previous;
            if changed {
                record.view.updated_at_ms = now_ms();
            }
            Ok((Some(completion.clone()), changed))
        })
        .await
    }

    fn rebuild_indexes_from_runs(&self, runs: &BTreeMap<String, RunRecord>) {
        *self.indexes.lock().expect("run indexes mutex poisoned") = build_run_indexes(runs);
    }

    /// Lists the pending structured user questions, optionally scoped to one session.
    pub(crate) fn list_pending_questions(
        &self,
        session_id: Option<&str>,
    ) -> Vec<PendingQuestionView> {
        self.pending_questions
            .lock()
            .expect("pending question index mutex poisoned")
            .values()
            .filter(|question| {
                session_id
                    .map(|requested| requested == question.session_id.as_str())
                    .unwrap_or(true)
            })
            .cloned()
            .collect()
    }

    /// Returns a notification primitive for pending user-question state changes.
    pub(crate) fn pending_question_notify(&self) -> &Notify {
        &self.pending_question_notify
    }

    /// Returns the pending user-question runs due for expiration and the next expiry deadline.
    pub(crate) fn pending_question_expiration_snapshot(
        &self,
        now_ms: u64,
    ) -> PendingQuestionExpirationSnapshot {
        let pending_questions = self
            .pending_questions
            .lock()
            .expect("pending question index mutex poisoned");
        let mut snapshot = PendingQuestionExpirationSnapshot::default();
        for question in pending_questions.values() {
            let Some(expires_at_ms) = question.request.expires_at_ms else {
                continue;
            };
            if expires_at_ms <= now_ms {
                if let Some(run_id) = question.run_id.as_ref() {
                    snapshot.due_run_ids.insert(run_id.clone());
                }
            } else {
                snapshot.next_expiry_ms = Some(
                    snapshot
                        .next_expiry_ms
                        .map(|current| current.min(expires_at_ms))
                        .unwrap_or(expires_at_ms),
                );
            }
        }
        snapshot
    }

    /// Persists one run output and republishes the run when the run exists.
    pub(crate) async fn record_output_for_run(
        &self,
        run_id: &str,
        output: DaemonOutputRecord,
    ) -> Result<Option<RunView>> {
        let record = {
            let mut runs = self.runs.lock().await;
            let Some(record) = runs.get_mut(run_id) else {
                return Ok(None);
            };
            let previous = record.clone();
            record.view.outputs.push(output.clone());
            record.view.updated_at_ms = now_ms();
            if let Err(error) = self.run_store.save_run(record) {
                *record = previous;
                return Err(error);
            }
            record.clone()
        };
        self.append_run_event(
            &record.view,
            RunEvent::Output {
                output: output.clone(),
            },
        )?;
        self.publish_run(&record.view);
        Ok(Some(record.view))
    }

    /// Resumes one approval-blocked active run and republishes it as running.
    pub(crate) async fn resume_waiting_approval_run(
        &self,
        run_id: &str,
        request: ResolveApprovalsRequest,
    ) -> Result<RunView> {
        let resolutions = request.resolutions.clone();
        self.resume_waiting_run(
            run_id,
            DaemonRunStatus::WaitingForApproval,
            false,
            move |run| {
                if run.view.status != DaemonRunStatus::WaitingForApproval {
                    return Err(DaemonProblem::approval_state_conflict(format!(
                        "run {run_id} is not waiting for approval"
                    ))
                    .into());
                }
                validate_approval_resolution_ids(
                    &run.view.pending_approval_ids,
                    &request.resolutions,
                )?;
                let scheduled_origin = scheduled_run_origin(&run.payload);
                let channel_delivery = run.payload.channel_delivery_request().cloned();
                let next_kind = match run.view.kind {
                    DaemonRunKind::ScheduledInput
                    | DaemonRunKind::ScheduledObservationMaterialization => run.view.kind.clone(),
                    DaemonRunKind::ChannelDelivery => DaemonRunKind::ChannelDelivery,
                    _ if channel_delivery.is_some() => DaemonRunKind::ChannelDelivery,
                    _ => DaemonRunKind::ApprovalResume,
                };
                Ok((
                    next_kind,
                    RunRequestPayload::ApprovalResume {
                        request,
                        original_request: resume_original_request(&run.payload),
                        scheduled_origin,
                        channel_delivery,
                    },
                    Some(RunEvent::ApprovalResolved { resolutions }),
                ))
            },
        )
        .await
    }

    /// Resumes one user-question-blocked active run and republishes it as running.
    pub(crate) async fn resume_waiting_user_question_run(
        &self,
        run_id: &str,
        request: ResolveUserQuestionRequest,
    ) -> Result<RunView> {
        let resolution = request.resolution.clone();
        self.resume_waiting_run(
            run_id,
            DaemonRunStatus::WaitingForUserQuestion,
            true,
            move |run| {
                let scheduled_origin = scheduled_run_origin(&run.payload);
                let channel_delivery = run.payload.channel_delivery_request().cloned();
                let next_kind = match run.view.kind {
                    DaemonRunKind::ScheduledInput
                    | DaemonRunKind::ScheduledObservationMaterialization => run.view.kind.clone(),
                    DaemonRunKind::ChannelDelivery => DaemonRunKind::ChannelDelivery,
                    _ if channel_delivery.is_some() => DaemonRunKind::ChannelDelivery,
                    _ => DaemonRunKind::UserQuestionResume,
                };
                Ok((
                    next_kind,
                    RunRequestPayload::UserQuestionResume {
                        request,
                        original_request: resume_original_request(&run.payload),
                        scheduled_origin,
                        channel_delivery,
                    },
                    Some(RunEvent::UserQuestionResolved { resolution }),
                ))
            },
        )
        .await
    }

    /// Schedules one new run and persists the queue update.
    pub(crate) async fn schedule_run(&self, record: RunRecord) -> Result<RunScheduleResult> {
        self.schedule_run_with_idle_policy(record, false).await
    }

    /// Schedules one run only if the owning session has no active or queued runs.
    pub(crate) async fn schedule_run_requiring_idle(
        &self,
        record: RunRecord,
    ) -> Result<RunScheduleResult> {
        self.schedule_run_with_idle_policy(record, true).await
    }

    async fn schedule_run_with_idle_policy(
        &self,
        mut record: RunRecord,
        require_idle: bool,
    ) -> Result<RunScheduleResult> {
        let session_id = record.view.session_id.clone();
        let (view, started_immediately, queue_records) = {
            let mut runs = self.runs.lock().await;
            let mut session_runs = self.session_runs.lock().await;
            let mut pending_idle_submissions = self.pending_idle_submissions.lock().await;
            let reserved_run_id = pending_idle_submissions.get(&session_id).cloned();
            let pending_goal_continuations = self.pending_goal_continuations.lock().await;
            let has_pending_goal_continuation = pending_goal_continuations
                .get(&session_id)
                .is_some_and(|run_ids| !run_ids.is_empty());
            let previous_session_state = session_runs.get(&session_id).cloned();
            let state = session_runs.entry(session_id.clone()).or_default();
            let has_matching_idle_reservation =
                reserved_run_id.as_deref() == Some(record.view.run_id.as_str());
            let has_matching_goal_reservation = pending_goal_continuations
                .get(&session_id)
                .is_some_and(|run_ids| run_ids.contains(&record.view.run_id));
            if require_idle
                && !has_matching_idle_reservation
                && (reserved_run_id.is_some()
                    || state.active_run_id.is_some()
                    || !state.queued_run_ids.is_empty())
            {
                return Err(DaemonProblem::session_busy(format!(
                    "session {session_id} is already processing background work"
                ))
                .into());
            }
            if require_idle && has_matching_idle_reservation && state.active_run_id.is_some() {
                return Err(DaemonProblem::session_busy(format!(
                    "session {session_id} is already processing background work"
                ))
                .into());
            }
            let mut original_records = BTreeMap::from([(record.view.run_id.clone(), None)]);
            let can_start_now = state.active_run_id.is_none()
                && (has_matching_idle_reservation
                    || has_matching_goal_reservation
                    || (state.queued_run_ids.is_empty()
                        && reserved_run_id.is_none()
                        && !has_pending_goal_continuation));
            let started_immediately = if can_start_now {
                state.active_run_id = Some(record.view.run_id.clone());
                transition_run_status(&mut record.view, DaemonRunStatus::Running)?;
                record.view.started_at_ms = Some(now_ms());
                record.view.updated_at_ms = record
                    .view
                    .started_at_ms
                    .unwrap_or(record.view.updated_at_ms);
                true
            } else {
                state.queued_run_ids.push_back(record.view.run_id.clone());
                transition_run_status(&mut record.view, DaemonRunStatus::Queued)?;
                false
            };
            runs.insert(record.view.run_id.clone(), record.clone());
            for run_id in &state.queued_run_ids {
                if run_id == &record.view.run_id {
                    continue;
                }
                original_records
                    .entry(run_id.clone())
                    .or_insert_with(|| runs.get(run_id).cloned());
            }
            let queue_records = collect_queue_records(state, &mut runs)?;
            let persist_records = if started_immediately {
                vec![record.clone()]
            } else {
                queue_records.clone()
            };
            if let Err(error) =
                persist_run_batch_or_rollback(&self.run_store, &persist_records, &original_records)
            {
                restore_run_record_snapshots(&mut runs, &original_records);
                restore_session_run_state(&mut session_runs, &session_id, previous_session_state);
                return Err(error);
            }
            if has_matching_idle_reservation {
                pending_idle_submissions.remove(&session_id);
            }
            self.rebuild_indexes_from_runs(&runs);
            let view = runs
                .get(&record.view.run_id)
                .expect("run should exist after scheduling")
                .view
                .clone();
            (view, started_immediately, queue_records)
        };

        self.append_run_event(&view, RunEvent::Accepted)?;
        if started_immediately {
            self.append_run_event(&view, RunEvent::Started)?;
        } else {
            self.append_run_event(
                &view,
                RunEvent::Queued {
                    position: view.queued_position.unwrap_or(1),
                },
            )?;
        }
        self.publish_run(&view);
        for queued in queue_records {
            if queued.view.run_id != view.run_id {
                self.publish_run(&queued.view);
            }
        }
        Ok(RunScheduleResult {
            view,
            started_immediately,
        })
    }

    /// Advances the next queued run for one session into the active slot.
    pub(crate) async fn start_next_queued_run(&self, session_id: &str) -> Result<Option<RunView>> {
        let result = {
            let mut runs = self.runs.lock().await;
            let mut session_runs = self.session_runs.lock().await;
            let previous_session_state = session_runs.get(session_id).cloned();
            let mut original_records = BTreeMap::new();
            if let Some(state) = session_runs.get(session_id) {
                snapshot_session_run_records(&runs, state, &mut original_records);
            }
            let transition =
                advance_session_queue_locked(&mut runs, &mut session_runs, session_id, None)?;
            let mut persist_records = transition.dirty_queue_records.clone();
            if let Some(started) = transition.started_record.as_ref() {
                persist_records.push(started.clone());
            }
            if let Err(error) =
                persist_run_batch_or_rollback(&self.run_store, &persist_records, &original_records)
            {
                restore_run_record_snapshots(&mut runs, &original_records);
                restore_session_run_state(&mut session_runs, session_id, previous_session_state);
                return Err(error);
            }
            self.rebuild_indexes_from_runs(&runs);
            (
                transition
                    .dirty_queue_records
                    .into_iter()
                    .map(|record| record.view)
                    .collect::<Vec<_>>(),
                transition.started_record.map(|record| record.view),
                transition.session_idle,
            )
        };
        let (queue_updates, started_run, session_idle) = result;
        for view in queue_updates {
            self.publish_run(&view);
        }
        if let Some(view) = started_run.clone() {
            self.append_run_event(&view, RunEvent::Started)?;
            self.publish_run(&view);
        }
        let outcome = RunQueueAdvanceResult {
            started_run,
            finished_run_was_active: false,
            session_idle,
        };
        Ok(outcome.started_run)
    }

    /// Applies one settled orchestrator snapshot to the owned run state and persists the result.
    pub(crate) async fn apply_snapshot(
        &self,
        run_id: &str,
        snapshot: &ManagedAgentSnapshot,
    ) -> Result<Option<RunSnapshotUpdate>> {
        self.update_run_record(run_id, |record| {
            if record.view.status.is_terminal() {
                return Ok((None, false));
            }
            record.view.updated_at_ms = now_ms();
            let next_session_id = if !snapshot.pending_questions.is_empty() {
                transition_run_status(&mut record.view, DaemonRunStatus::WaitingForUserQuestion)?;
                record.view.pending_question_ids = snapshot
                    .pending_questions
                    .iter()
                    .map(|request| request.id.clone())
                    .collect();
                record.view.pending_questions = snapshot.pending_questions.clone();
                record.view.pending_approval_ids.clear();
                record.view.pending_approvals.clear();
                record.view.error = None;
                None
            } else if !snapshot.pending_approvals.is_empty() {
                transition_run_status(&mut record.view, DaemonRunStatus::WaitingForApproval)?;
                record.view.pending_approval_ids = snapshot
                    .pending_approvals
                    .iter()
                    .map(|request| request.id.clone())
                    .collect();
                record.view.pending_approvals = snapshot.pending_approvals.clone();
                clear_pending_questions(&mut record.view);
                record.view.error = None;
                None
            } else {
                transition_run_status(&mut record.view, DaemonRunStatus::Completed)?;
                record.view.finished_at_ms = Some(record.view.updated_at_ms);
                clear_pending_state(&mut record.view);
                record.view.error = None;
                Some(record.view.session_id.clone())
            };
            Ok((
                Some(RunSnapshotUpdate {
                    record: record.clone(),
                    next_session_id,
                }),
                true,
            ))
        })
        .await
    }

    /// Recovers non-terminal running runs after one daemon restart.
    pub(crate) async fn recover_running_runs(
        &self,
        snapshots: &BTreeMap<String, ManagedAgentSnapshot>,
    ) -> Result<()> {
        let recovered = {
            let mut runs = self.runs.lock().await;
            let mut session_runs = self.session_runs.lock().await;
            let previous_session_runs = session_runs.clone();
            let mut original_records = BTreeMap::new();
            let mut recovered_events = Vec::new();
            let mut recovered_views = BTreeMap::new();
            let mut persist_records = BTreeMap::new();
            for record in runs.values_mut() {
                if record.view.status != DaemonRunStatus::Running {
                    continue;
                }
                original_records.insert(record.view.run_id.clone(), Some(record.clone()));
                let event = recover_running_record(record, snapshots.get(&record.view.agent_id))?;
                persist_records.insert(record.view.run_id.clone(), record.clone());
                if let Some(event) = event {
                    recovered_events.push((record.clone(), event));
                }
                recovered_views.insert(record.view.run_id.clone(), record.view.clone());
            }
            *session_runs = rebuild_session_run_state(&runs);
            for (record, event) in normalize_rebuilt_session_queues(
                &mut runs,
                &mut session_runs,
                &mut original_records,
            )? {
                persist_records.insert(record.view.run_id.clone(), record.clone());
                if let Some(event) = event {
                    recovered_events.push((record.clone(), event));
                }
                recovered_views.insert(record.view.run_id.clone(), record.view.clone());
            }
            let persist_records = persist_records.into_values().collect::<Vec<_>>();
            if let Err(error) =
                persist_run_batch_or_rollback(&self.run_store, &persist_records, &original_records)
            {
                restore_run_record_snapshots(&mut runs, &original_records);
                *session_runs = previous_session_runs;
                return Err(error);
            }
            self.rebuild_indexes_from_runs(&runs);
            (
                recovered_events,
                recovered_views.into_values().collect::<Vec<_>>(),
            )
        };

        let (recovered_events, recovered_views) = recovered;
        for (record, event) in recovered_events {
            self.append_run_event(&record.view, event)?;
        }
        for view in recovered_views {
            self.publish_run(&view);
        }
        Ok(())
    }

    /// Marks one run as failed and persists the terminal state when it was still active.
    pub(crate) async fn mark_failed(&self, run_id: &str, error: &str) -> Result<Option<RunRecord>> {
        self.update_run_record(run_id, |record| {
            if record.view.status.is_terminal() {
                return Ok((None, false));
            }
            transition_run_status(&mut record.view, DaemonRunStatus::Failed)?;
            record.view.updated_at_ms = now_ms();
            record.view.finished_at_ms = Some(record.view.updated_at_ms);
            record.view.error = Some(error.to_string());
            clear_pending_state(&mut record.view);
            Ok((Some(record.clone()), true))
        })
        .await
    }

    /// Marks one run as interrupted and persists the terminal state when it was still active.
    pub(crate) async fn mark_interrupted(&self, run_id: &str) -> Result<Option<RunRecord>> {
        self.update_run_record(run_id, |record| {
            if record.view.status.is_terminal() {
                return Ok((None, false));
            }
            transition_run_status(&mut record.view, DaemonRunStatus::Interrupted)?;
            record.view.updated_at_ms = now_ms();
            record.view.finished_at_ms = Some(record.view.updated_at_ms);
            record.view.error = None;
            clear_pending_state(&mut record.view);
            Ok((Some(record.clone()), true))
        })
        .await
    }

    /// Marks one run as waiting for a structured user question and persists the update.
    pub(crate) async fn mark_waiting_for_user_question(
        &self,
        run_id: &str,
        request: UserQuestionRequest,
    ) -> Result<Option<RunRecord>> {
        let result = self
            .update_run_record(run_id, |record| {
                if record.view.status.is_terminal() {
                    return Ok((None, false));
                }
                transition_run_status(&mut record.view, DaemonRunStatus::WaitingForUserQuestion)?;
                record.view.updated_at_ms = now_ms();
                record.view.pending_approval_ids.clear();
                record.view.pending_approvals.clear();
                record.view.pending_question_ids = vec![request.id.clone()];
                record.view.pending_questions = vec![request];
                record.view.error = None;
                Ok((Some(record.clone()), true))
            })
            .await?;
        Ok(result)
    }

    /// Returns the durable completion state for one parent clarification run.
    pub(crate) async fn parent_clarification_completion_state(
        &self,
        run_id: &str,
    ) -> Result<Option<ParentClarificationCompletionState>> {
        let record = self.run_record(run_id).await?;
        let completion = match record.payload {
            RunRequestPayload::ParentClarification { completion, .. } => completion,
            _ => return Ok(None),
        };
        Ok(Some(completion))
    }

    /// Persists the claimed user resolution for one parent clarification run.
    pub(crate) async fn capture_parent_clarification_resolution(
        &self,
        run_id: &str,
        resolution: &UserQuestionResolution,
        reason: ParentClarificationCompletionReason,
    ) -> Result<Option<ParentClarificationCompletionState>> {
        self.update_parent_clarification_completion(run_id, |completion| {
            if let Some(existing) = completion.resolution.as_ref() {
                if existing == resolution {
                    if completion.reason.is_none() {
                        completion.reason = Some(reason);
                    }
                    return Ok(());
                }
                if let Some(error) = expired_user_question_resolution_error(completion, existing) {
                    anyhow::bail!("{error}");
                }
                return Err(DaemonProblem::question_resolution_conflict(format!(
                    "run {run_id} was already resolved with a different answer"
                ))
                .into());
            }
            completion.resolution = Some(resolution.clone());
            completion.reason = Some(reason);
            Ok(())
        })
        .await
    }

    /// Marks the parent clarification mailbox side effect as durable.
    pub(crate) async fn mark_parent_clarification_mailbox_posted(
        &self,
        run_id: &str,
    ) -> Result<Option<ParentClarificationCompletionState>> {
        self.update_parent_clarification_completion(run_id, |completion| {
            completion.mailbox_posted = true;
            Ok(())
        })
        .await
    }

    /// Marks the parent clarification session-output side effect as durable.
    pub(crate) async fn mark_parent_clarification_output_emitted(
        &self,
        run_id: &str,
    ) -> Result<Option<ParentClarificationCompletionState>> {
        self.update_parent_clarification_completion(run_id, |completion| {
            completion.output_emitted = true;
            Ok(())
        })
        .await
    }

    /// Marks the parent clarification hook side effect as durable.
    pub(crate) async fn mark_parent_clarification_hook_dispatched(
        &self,
        run_id: &str,
    ) -> Result<Option<ParentClarificationCompletionState>> {
        self.update_parent_clarification_completion(run_id, |completion| {
            completion.hook_dispatched = true;
            Ok(())
        })
        .await
    }

    /// Marks the parent clarification resolution event as durable.
    pub(crate) async fn mark_parent_clarification_resolution_recorded(
        &self,
        run_id: &str,
    ) -> Result<Option<ParentClarificationCompletionState>> {
        self.update_parent_clarification_completion(run_id, |completion| {
            completion.resolution_recorded = true;
            Ok(())
        })
        .await
    }

    /// Marks the parent clarification completion event as durable.
    pub(crate) async fn mark_parent_clarification_completion_recorded(
        &self,
        run_id: &str,
    ) -> Result<Option<ParentClarificationCompletionState>> {
        self.update_parent_clarification_completion(run_id, |completion| {
            completion.completion_recorded = true;
            Ok(())
        })
        .await
    }

    /// Marks one run as completed and persists the terminal state when it was still active.
    pub(crate) async fn mark_completed(&self, run_id: &str) -> Result<Option<RunRecord>> {
        self.update_run_record(run_id, |record| {
            if record.view.status.is_terminal() {
                return Ok((None, false));
            }
            transition_run_status(&mut record.view, DaemonRunStatus::Completed)?;
            record.view.updated_at_ms = now_ms();
            record.view.finished_at_ms = Some(record.view.updated_at_ms);
            record.view.error = None;
            clear_pending_state(&mut record.view);
            Ok((Some(record.clone()), true))
        })
        .await
    }

    /// Cancels one active or queued run, updates the owned queue index, and persists the result.
    pub(crate) async fn cancel_run(&self, run_id: &str) -> Result<Option<RunCancellationResult>> {
        self.cancel_run_with_error(run_id, None).await
    }

    /// Cancels one active or queued run and records an optional terminal error reason.
    pub(crate) async fn cancel_run_with_error(
        &self,
        run_id: &str,
        error: Option<String>,
    ) -> Result<Option<RunCancellationResult>> {
        let (record, was_active, queue_updates) = {
            let mut runs = self.runs.lock().await;
            let mut session_runs = self.session_runs.lock().await;
            let session_id = runs
                .get(run_id)
                .ok_or_else(|| DaemonProblem::run_not_found(format!("unknown run {run_id}")))?;
            if session_id.view.status.is_terminal() {
                return Ok(None);
            }
            let session_id = session_id.view.session_id.clone();
            let previous_session_state = session_runs.get(&session_id).cloned();
            let mut original_records = BTreeMap::new();
            if let Some(record) = runs.get(run_id).cloned() {
                original_records.insert(run_id.to_string(), Some(record));
            }
            if let Some(state) = session_runs.get(&session_id) {
                snapshot_session_run_records(&runs, state, &mut original_records);
            }
            let was_active = session_runs
                .get(&session_id)
                .and_then(|state| state.active_run_id.as_deref())
                == Some(run_id);
            if let Some(state) = session_runs.get_mut(&session_id) {
                if was_active {
                    state.active_run_id = None;
                } else {
                    state.queued_run_ids.retain(|queued| queued != run_id);
                }
                if state.active_run_id.is_none() && state.queued_run_ids.is_empty() {
                    session_runs.remove(&session_id);
                }
            }
            {
                let record = runs
                    .get_mut(run_id)
                    .expect("cancelled run should still exist in run map");
                transition_run_status(&mut record.view, DaemonRunStatus::Cancelled)?;
                record.view.updated_at_ms = now_ms();
                record.view.finished_at_ms = Some(record.view.updated_at_ms);
                record.view.error = error.clone();
                clear_pending_state(&mut record.view);
            }
            let record = runs
                .get(run_id)
                .cloned()
                .expect("cancelled run should still exist after mutation");
            let queue_records = if let Some(state) = session_runs.get(&session_id) {
                collect_queue_records(state, &mut runs)?
            } else {
                Vec::new()
            };
            let mut persist_records = vec![record.clone()];
            persist_records.extend(queue_records.iter().cloned());
            if let Err(error) =
                persist_run_batch_or_rollback(&self.run_store, &persist_records, &original_records)
            {
                restore_run_record_snapshots(&mut runs, &original_records);
                restore_session_run_state(&mut session_runs, &session_id, previous_session_state);
                return Err(error);
            }
            self.rebuild_indexes_from_runs(&runs);
            (
                record.clone(),
                was_active,
                queue_records
                    .into_iter()
                    .map(|record| record.view)
                    .collect::<Vec<_>>(),
            )
        };
        for view in queue_updates {
            self.publish_run(&view);
        }
        Ok(Some(RunCancellationResult { record, was_active }))
    }

    /// Releases the active slot for one terminal run and promotes the next queued run.
    pub(crate) async fn finish_active_run(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<RunQueueAdvanceResult> {
        let result = {
            let mut runs = self.runs.lock().await;
            let mut session_runs = self.session_runs.lock().await;
            let previous_session_state = session_runs.get(session_id).cloned();
            let mut original_records = BTreeMap::new();
            if let Some(state) = session_runs.get(session_id) {
                snapshot_session_run_records(&runs, state, &mut original_records);
            }
            let transition = advance_session_queue_locked(
                &mut runs,
                &mut session_runs,
                session_id,
                Some(run_id),
            )?;
            let mut persist_records = transition.dirty_queue_records.clone();
            if let Some(started) = transition.started_record.as_ref() {
                persist_records.push(started.clone());
            }
            if let Err(error) =
                persist_run_batch_or_rollback(&self.run_store, &persist_records, &original_records)
            {
                restore_run_record_snapshots(&mut runs, &original_records);
                restore_session_run_state(&mut session_runs, session_id, previous_session_state);
                return Err(error);
            }
            self.rebuild_indexes_from_runs(&runs);
            (
                transition
                    .dirty_queue_records
                    .into_iter()
                    .map(|record| record.view)
                    .collect::<Vec<_>>(),
                transition.started_record.map(|record| record.view),
                transition.finished_run_was_active,
                transition.session_idle,
            )
        };
        let (queue_updates, started_run, finished_run_was_active, session_idle) = result;
        for view in queue_updates {
            self.publish_run(&view);
        }
        if let Some(view) = started_run.as_ref() {
            self.append_run_event(view, RunEvent::Started)?;
            self.publish_run(view);
        }
        Ok(RunQueueAdvanceResult {
            started_run,
            finished_run_was_active,
            session_idle,
        })
    }

    /// Persists queued positions for one session after queue mutations.
    pub(crate) async fn refresh_session_queue(&self, session_id: &str) -> Result<()> {
        let records = {
            let mut runs = self.runs.lock().await;
            let session_runs = self.session_runs.lock().await;
            let Some(state) = session_runs.get(session_id) else {
                return Ok(());
            };
            let mut original_records = BTreeMap::new();
            snapshot_session_run_records(&runs, state, &mut original_records);
            let dirty = collect_queue_records(state, &mut runs)?;
            if let Err(error) =
                persist_run_batch_or_rollback(&self.run_store, &dirty, &original_records)
            {
                restore_run_record_snapshots(&mut runs, &original_records);
                return Err(error);
            }
            self.rebuild_indexes_from_runs(&runs);
            dirty
        };
        for record in records {
            self.publish_run(&record.view);
        }
        Ok(())
    }

    /// Loads one run debug summary or returns an empty bundle when capture is off.
    pub(crate) fn run_debug_view(&self, run_id: &str) -> Result<RunDebugView> {
        if let Some(view) = self.debug_store.load_view(run_id)? {
            return Ok(view);
        }
        if self.run_store.load_run(run_id)?.is_some() {
            return Ok(RunDebugView {
                run_id: run_id.to_string(),
                level: kheish_runtime::DebugCaptureLevel::Off,
                artifacts: Vec::new(),
            });
        }
        Err(DaemonProblem::run_not_found(format!("unknown run {run_id}")).into())
    }

    /// Reads the textual body of one persisted run debug artifact.
    pub(crate) fn run_debug_artifact(&self, run_id: &str, artifact_id: &str) -> Result<String> {
        if self.run_store.load_run(run_id)?.is_none() {
            return Err(DaemonProblem::run_not_found(format!("unknown run {run_id}")).into());
        }
        let view = self.debug_store.load_view(run_id)?.ok_or_else(|| {
            DaemonProblem::run_debug_not_found(format!("debug capture is off for run {run_id}"))
        })?;
        if !view
            .artifacts
            .iter()
            .any(|entry| entry.artifact_id == artifact_id)
        {
            return Err(DaemonProblem::run_debug_artifact_not_found(format!(
                "unknown debug artifact {artifact_id}"
            ))
            .into());
        }
        self.debug_store
            .read_artifact(run_id, artifact_id)
            .map_err(|error| {
                DaemonProblem::run_debug_artifact_unreadable(format!(
                    "debug artifact {artifact_id} for run {run_id} is unreadable: {error}"
                ))
                .into()
            })
    }

    /// Loads the signed external action audit records for one run.
    pub(crate) fn run_external_actions(
        &self,
        run_id: &str,
    ) -> Result<Vec<ExternalActionAuditRecord>> {
        if self.run_store.load_run(run_id)?.is_none() {
            return Err(DaemonProblem::run_not_found(format!("unknown run {run_id}")).into());
        }
        ExternalActionService::new(self.debug_store.root())?.records_for_run(run_id)
    }

    /// Loads the persisted event log for one run.
    pub(crate) fn run_events(&self, run_id: &str) -> Result<Vec<RunEventEntry>> {
        let _guard = self
            .event_append_lock
            .lock()
            .expect("run event append mutex poisoned");
        let record = self
            .run_store
            .load_run(run_id)?
            .ok_or_else(|| DaemonProblem::run_not_found(format!("unknown run {run_id}")))?;
        let mut events = self.run_store.load_events(run_id)?;
        let expected_events = reconciled_events(&record);
        let final_event_present = expected_events.last().is_some_and(|expected| {
            events
                .iter()
                .any(|entry| run_event_matches(&entry.event, expected))
        });
        let final_event_last =
            last_event_matches_expected(events.as_slice(), expected_events.last());
        let mut missing_events = Vec::new();
        for expected in &expected_events {
            if !events
                .iter()
                .any(|entry| run_event_matches(&entry.event, expected))
            {
                missing_events.push(expected.clone());
            }
        }
        if missing_events.is_empty() && final_event_last {
            return Ok(events);
        }

        if final_event_present {
            for event in missing_events {
                insert_reconciled_event_entry(&record, &expected_events, &mut events, event);
            }
            move_final_event_last(&expected_events, &mut events);
            return Ok(events);
        }

        for event in missing_events {
            self.append_run_event_once_locked(&record.view, event)?;
        }
        self.run_store.load_events(run_id)
    }

    /// Returns the run-memory store.
    pub(crate) fn run_memory_store(&self) -> &FileRunMemoryStore {
        &self.run_memory_store
    }

    async fn resume_waiting_run(
        &self,
        run_id: &str,
        expected_status: DaemonRunStatus,
        clear_pending_question_state: bool,
        build_resume: impl FnOnce(
            &RunRecord,
        )
            -> Result<(DaemonRunKind, RunRequestPayload, Option<RunEvent>)>,
    ) -> Result<RunView> {
        let view = {
            let mut runs = self.runs.lock().await;
            let record = runs
                .get_mut(run_id)
                .ok_or_else(|| DaemonProblem::run_not_found(format!("unknown run {run_id}")))?;
            let session_id = record.view.session_id.clone();
            let session_runs = self.session_runs.lock().await;
            if session_runs
                .get(&session_id)
                .and_then(|state| state.active_run_id.as_deref())
                != Some(run_id)
            {
                return Err(DaemonProblem::run_state_conflict(format!(
                    "run {run_id} is not the active waiting run for its session"
                ))
                .into());
            }
            drop(session_runs);

            let previous = record.clone();
            let (next_kind, next_payload, audit_event) = {
                if record.view.status != expected_status {
                    return Err(waiting_run_state_problem(run_id, &expected_status).into());
                }
                build_resume(record)?
            };
            record.payload = next_payload;
            record.view.kind = next_kind;
            transition_run_status(&mut record.view, DaemonRunStatus::Running)?;
            record.view.updated_at_ms = now_ms();
            record.view.error = None;
            record.view.queued_position = None;
            record.view.pending_approval_ids.clear();
            record.view.pending_approvals.clear();
            if clear_pending_question_state {
                clear_pending_questions(&mut record.view);
            }
            let event_guard = self
                .event_append_lock
                .lock()
                .expect("run event append mutex poisoned");
            if let Err(error) = self.run_store.save_run(record) {
                *record = previous;
                return Err(error);
            }
            let updated = record.clone();
            if let Err(error) = self.append_resume_events_locked(&updated.view, audit_event) {
                let rollback = self.run_store.save_run(&previous);
                *record = previous.clone();
                self.refresh_record_indexes(Some(&updated), &previous);
                return match rollback {
                    Ok(()) => Err(anyhow!(
                        "failed to persist resume audit event for run {run_id}; resume rolled back: {error}"
                    )),
                    Err(rollback_error) => Err(anyhow!(
                        "failed to persist resume audit event for run {run_id}: {error}; rollback failed: {rollback_error}"
                    )),
                };
            }
            drop(event_guard);
            self.refresh_record_indexes(Some(&previous), &updated);
            updated.view
        };
        self.publish_run(&view);
        Ok(view)
    }

    fn refresh_record_indexes(&self, previous: Option<&RunRecord>, updated: &RunRecord) {
        let mut indexes = self.indexes.lock().expect("run indexes mutex poisoned");
        if let Some(previous) = previous {
            deindex_run_record(previous, &mut indexes);
        }
        index_run_record(updated, &mut indexes);
    }

    fn sync_pending_question_index(&self, run: &RunView) {
        let mut pending = self
            .pending_questions
            .lock()
            .expect("pending question index mutex poisoned");
        pending.retain(|_, question| question.run_id.as_deref() != Some(run.run_id.as_str()));
        if run.status != DaemonRunStatus::WaitingForUserQuestion {
            return;
        }
        for request in &run.pending_questions {
            pending.insert(
                pending_question_index_key(&run.run_id, &request.id),
                self.runs
                    .try_lock()
                    .ok()
                    .and_then(|runs| runs.get(&run.run_id).cloned())
                    .map(|record| pending_question_view_for_record(&record, request))
                    .unwrap_or_else(|| PendingQuestionView {
                        session_id: run.session_id.clone(),
                        agent_id: run.agent_id.clone(),
                        run_id: Some(run.run_id.clone()),
                        run_kind: Some(run.kind.clone()),
                        requester_agent_id: None,
                        requester_session_id: None,
                        requester_run_id: None,
                        requester_tool_call_id: None,
                        requester_project_ids: Vec::new(),
                        requester_channel_ids: Vec::new(),
                        parent_project_ids: Vec::new(),
                        parent_channel_ids: Vec::new(),
                        request: request.clone(),
                    }),
            );
        }
    }
}

fn clear_pending_questions(view: &mut RunView) {
    view.pending_question_ids.clear();
    view.pending_questions.clear();
}

fn expired_user_question_resolution_error(
    completion: &ParentClarificationCompletionState,
    resolution: &UserQuestionResolution,
) -> Option<String> {
    if !resolution.declined {
        return None;
    }
    if let Some(ParentClarificationCompletionReason::Expired { expires_at_ms }) =
        completion.reason.as_ref()
    {
        return Some(format!(
            "user-question request {} expired at {}",
            resolution.request_id, *expires_at_ms
        ));
    }
    let justification = resolution.justification.as_deref()?;
    if !justification.contains("expired at ") {
        return None;
    }
    Some(format!(
        "user-question request {} {}",
        resolution.request_id, justification
    ))
}

fn parent_clarification_reason_for_resolution(
    resolution: &UserQuestionResolution,
) -> ParentClarificationCompletionReason {
    if !resolution.declined {
        return ParentClarificationCompletionReason::Answered;
    }
    if let Some(expires_at_ms) = parse_expired_question_justification(&resolution.justification) {
        return ParentClarificationCompletionReason::Expired { expires_at_ms };
    }
    ParentClarificationCompletionReason::Declined
}

fn parse_expired_question_justification(justification: &Option<String>) -> Option<u64> {
    let text = justification.as_deref()?;
    let (_, suffix) = text.rsplit_once("expired at ")?;
    suffix
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<u64>().ok())
}

fn resume_original_request(payload: &RunRequestPayload) -> Option<SubmitInputRequest> {
    match payload {
        RunRequestPayload::Input { request, .. }
        | RunRequestPayload::ScheduledInput { request, .. } => Some(request.clone()),
        RunRequestPayload::ObservationMaterialization { request }
        | RunRequestPayload::ScheduledObservationMaterialization { request, .. } => {
            Some(request.request.clone())
        }
        RunRequestPayload::ApprovalResume {
            original_request, ..
        }
        | RunRequestPayload::UserQuestionResume {
            original_request, ..
        } => original_request.clone(),
        RunRequestPayload::MailboxDelivery { .. }
        | RunRequestPayload::ChannelDelivery { .. }
        | RunRequestPayload::ParentClarification { .. } => None,
    }
}

pub(crate) fn scheduled_run_origin(payload: &RunRequestPayload) -> Option<ScheduledRunOrigin> {
    match payload {
        RunRequestPayload::ScheduledInput {
            schedule_id,
            fire_at_ms,
            ..
        }
        | RunRequestPayload::ScheduledObservationMaterialization {
            schedule_id,
            fire_at_ms,
            ..
        } => Some(ScheduledRunOrigin {
            schedule_id: schedule_id.clone(),
            fire_at_ms: *fire_at_ms,
        }),
        RunRequestPayload::ApprovalResume {
            scheduled_origin, ..
        }
        | RunRequestPayload::UserQuestionResume {
            scheduled_origin, ..
        } => scheduled_origin.clone(),
        RunRequestPayload::Input { .. }
        | RunRequestPayload::ObservationMaterialization { .. }
        | RunRequestPayload::MailboxDelivery { .. }
        | RunRequestPayload::ChannelDelivery { .. }
        | RunRequestPayload::ParentClarification { .. } => None,
    }
}

fn waiting_state_name(status: &DaemonRunStatus) -> &'static str {
    match status {
        DaemonRunStatus::WaitingForApproval => "approval",
        DaemonRunStatus::WaitingForUserQuestion => "user input",
        _ => "the requested state",
    }
}

fn validate_approval_resolution_ids(
    pending_approval_ids: &[String],
    resolutions: &[ApprovalResolution],
) -> Result<()> {
    if resolutions.is_empty() {
        return Err(DaemonProblem::approval_batch_empty(
            "approval resolution batch must not be empty",
        )
        .into());
    }
    let pending_ids = pending_approval_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut seen_ids = BTreeSet::new();
    for resolution in resolutions {
        if !seen_ids.insert(resolution.request_id.as_str()) {
            return Err(DaemonProblem::approval_duplicate_resolution(format!(
                "duplicate approval resolution for request {}",
                resolution.request_id
            ))
            .into());
        }
        if !pending_ids.contains(resolution.request_id.as_str()) {
            return Err(DaemonProblem::approval_request_not_pending(format!(
                "approval resolution references unknown pending request {}",
                resolution.request_id
            ))
            .into());
        }
    }
    Ok(())
}

fn reconciled_events(record: &RunRecord) -> Vec<RunEvent> {
    let mut events = Vec::new();
    events.push(RunEvent::Accepted);
    if record.view.status == DaemonRunStatus::Queued {
        events.push(RunEvent::Queued {
            position: record.view.queued_position.unwrap_or(1),
        });
    } else if record.view.started_at_ms.is_some() {
        events.push(RunEvent::Started);
    }
    if let RunRequestPayload::ParentClarification {
        request,
        completion:
            ParentClarificationCompletionState {
                resolution: Some(resolution),
                ..
            },
    } = &record.payload
    {
        events.push(RunEvent::ParentClarificationResolved {
            requester_agent_id: request.requester_agent_id.clone(),
            requester_session_id: request.requester_session_id.clone(),
            request_id: resolution.request_id.clone(),
            declined: resolution.declined,
            resolution: resolution.clone(),
        });
    }
    if let RunRequestPayload::ApprovalResume { request, .. } = &record.payload {
        events.push(RunEvent::ApprovalResolved {
            resolutions: request.resolutions.clone(),
        });
    } else if let RunRequestPayload::UserQuestionResume { request, .. } = &record.payload {
        events.push(RunEvent::UserQuestionResolved {
            resolution: request.resolution.clone(),
        });
    }
    events.extend(
        record
            .view
            .outputs
            .iter()
            .cloned()
            .map(|output| RunEvent::Output { output }),
    );
    if let Some(event) = reconciled_state_event(&record.view) {
        events.push(event);
    }
    events
}

fn run_event_entry(view: &RunView, event: RunEvent) -> RunEventEntry {
    RunEventEntry {
        timestamp_ms: now_ms(),
        run_id: view.run_id.clone(),
        session_id: view.session_id.clone(),
        agent_id: view.agent_id.clone(),
        event,
    }
}

fn run_event_matches(existing: &RunEvent, expected: &RunEvent) -> bool {
    match (existing, expected) {
        (
            RunEvent::WaitingForApproval {
                request_ids: existing_ids,
                requests: existing_requests,
            },
            RunEvent::WaitingForApproval {
                request_ids: expected_ids,
                requests: expected_requests,
            },
        ) => {
            existing_ids == expected_ids
                && (expected_requests.is_empty() || existing_requests == expected_requests)
        }
        (
            RunEvent::WaitingForUserQuestion {
                request_ids: existing_ids,
                requests: existing_requests,
            },
            RunEvent::WaitingForUserQuestion {
                request_ids: expected_ids,
                requests: expected_requests,
            },
        ) => {
            existing_ids == expected_ids
                && (expected_requests.is_empty() || existing_requests == expected_requests)
        }
        _ => existing == expected,
    }
}

fn last_event_matches_expected(entries: &[RunEventEntry], expected: Option<&RunEvent>) -> bool {
    match (entries.last(), expected) {
        (Some(entry), Some(expected)) => run_event_matches(&entry.event, expected),
        (None, None) => true,
        _ => false,
    }
}

fn move_final_event_last(expected: &[RunEvent], entries: &mut Vec<RunEventEntry>) {
    let Some(final_expected) = expected.last() else {
        return;
    };
    if last_event_matches_expected(entries.as_slice(), Some(final_expected)) {
        return;
    }
    if let Some(index) = entries
        .iter()
        .position(|entry| run_event_matches(&entry.event, final_expected))
    {
        let entry = entries.remove(index);
        entries.push(entry);
    }
}

fn insert_reconciled_event_entry(
    record: &RunRecord,
    expected: &[RunEvent],
    entries: &mut Vec<RunEventEntry>,
    event: RunEvent,
) {
    let expected_position = expected
        .iter()
        .position(|expected| run_event_matches(&event, expected));
    let insert_at = expected_position.and_then(|expected_position| {
        entries.iter().position(|entry| {
            expected
                .iter()
                .skip(expected_position + 1)
                .any(|later| run_event_matches(&entry.event, later))
        })
    });
    let timestamp_ms = insert_at
        .and_then(|index| entries.get(index).map(|entry| entry.timestamp_ms))
        .unwrap_or_else(|| reconciled_event_timestamp(record, &event));
    let entry = RunEventEntry {
        timestamp_ms,
        run_id: record.view.run_id.clone(),
        session_id: record.view.session_id.clone(),
        agent_id: record.view.agent_id.clone(),
        event,
    };
    if let Some(index) = insert_at {
        entries.insert(index, entry);
    } else {
        entries.push(entry);
    }
}

fn reconciled_event_timestamp(record: &RunRecord, event: &RunEvent) -> u64 {
    match event {
        RunEvent::Accepted => record.view.submitted_at_ms,
        RunEvent::Started => record
            .view
            .started_at_ms
            .unwrap_or(record.view.updated_at_ms),
        RunEvent::Completed
        | RunEvent::Failed { .. }
        | RunEvent::Interrupted
        | RunEvent::Cancelled => record
            .view
            .finished_at_ms
            .unwrap_or(record.view.updated_at_ms),
        RunEvent::Queued { .. }
        | RunEvent::WaitingForApproval { .. }
        | RunEvent::ApprovalResolved { .. }
        | RunEvent::WaitingForUserQuestion { .. }
        | RunEvent::UserQuestionResolved { .. }
        | RunEvent::ParentClarificationResolved { .. }
        | RunEvent::Output { .. } => record.view.updated_at_ms,
    }
}

fn reconciled_state_event(view: &RunView) -> Option<RunEvent> {
    match view.status {
        DaemonRunStatus::WaitingForApproval => Some(RunEvent::WaitingForApproval {
            request_ids: view.pending_approval_ids.clone(),
            requests: view.pending_approvals.clone(),
        }),
        DaemonRunStatus::WaitingForUserQuestion => Some(RunEvent::WaitingForUserQuestion {
            request_ids: view.pending_question_ids.clone(),
            requests: view.pending_questions.clone(),
        }),
        DaemonRunStatus::Completed => Some(RunEvent::Completed),
        DaemonRunStatus::Failed => Some(RunEvent::Failed {
            error: view
                .error
                .clone()
                .unwrap_or_else(|| "run failed".to_string()),
        }),
        DaemonRunStatus::Interrupted => Some(RunEvent::Interrupted),
        DaemonRunStatus::Cancelled => Some(RunEvent::Cancelled),
        DaemonRunStatus::Queued | DaemonRunStatus::Running => None,
    }
}

fn clear_pending_state(view: &mut RunView) {
    view.pending_approval_ids.clear();
    view.pending_approvals.clear();
    clear_pending_questions(view);
}

fn recover_running_record(
    record: &mut RunRecord,
    snapshot: Option<&ManagedAgentSnapshot>,
) -> Result<Option<RunEvent>> {
    record.view.updated_at_ms = now_ms();
    if let Some(snapshot) = snapshot {
        if !snapshot.pending_questions.is_empty() {
            transition_run_status(&mut record.view, DaemonRunStatus::WaitingForUserQuestion)?;
            record.view.pending_question_ids = snapshot
                .pending_questions
                .iter()
                .map(|request| request.id.clone())
                .collect();
            record.view.pending_questions = snapshot.pending_questions.clone();
            record.view.pending_approval_ids.clear();
            record.view.pending_approvals.clear();
            record.view.error = None;
            return Ok(Some(RunEvent::WaitingForUserQuestion {
                request_ids: record.view.pending_question_ids.clone(),
                requests: snapshot.pending_questions.clone(),
            }));
        }
        if !snapshot.pending_approvals.is_empty() {
            transition_run_status(&mut record.view, DaemonRunStatus::WaitingForApproval)?;
            record.view.pending_approval_ids = snapshot
                .pending_approvals
                .iter()
                .map(|request| request.id.clone())
                .collect();
            record.view.pending_approvals = snapshot.pending_approvals.clone();
            clear_pending_questions(&mut record.view);
            record.view.error = None;
            return Ok(Some(RunEvent::WaitingForApproval {
                request_ids: record.view.pending_approval_ids.clone(),
                requests: snapshot.pending_approvals.clone(),
            }));
        }
    }

    if matches!(
        record.payload,
        RunRequestPayload::MailboxDelivery { .. }
            | RunRequestPayload::ObservationMaterialization { .. }
            | RunRequestPayload::ParentClarification { .. }
    ) || scheduled_run_origin(&record.payload).is_some()
    {
        transition_run_status_for_restart_recovery(&mut record.view, DaemonRunStatus::Queued)?;
        record.view.error = None;
        record.view.finished_at_ms = None;
        return Ok(None);
    }

    transition_run_status(&mut record.view, DaemonRunStatus::Interrupted)?;
    record.view.finished_at_ms = Some(record.view.updated_at_ms);
    record.view.error = Some("daemon restarted while the run was active".to_string());
    clear_pending_state(&mut record.view);
    Ok(Some(RunEvent::Interrupted))
}

fn transition_run_status(view: &mut RunView, next: DaemonRunStatus) -> Result<()> {
    if !view.status.allows_transition_to(&next) {
        return Err(DaemonProblem::run_state_conflict(format!(
            "invalid run status transition for {}: {:?} -> {:?}",
            view.run_id, view.status, next
        ))
        .into());
    }
    view.status = next;
    Ok(())
}

fn transition_run_status_for_restart_recovery(
    view: &mut RunView,
    next: DaemonRunStatus,
) -> Result<()> {
    if next == DaemonRunStatus::Queued
        && matches!(
            view.status,
            DaemonRunStatus::Running
                | DaemonRunStatus::WaitingForApproval
                | DaemonRunStatus::WaitingForUserQuestion
                | DaemonRunStatus::Queued
        )
    {
        view.status = next;
        return Ok(());
    }
    transition_run_status(view, next)
}

fn waiting_run_state_problem(run_id: &str, expected_status: &DaemonRunStatus) -> DaemonProblem {
    match expected_status {
        DaemonRunStatus::WaitingForApproval => DaemonProblem::approval_state_conflict(format!(
            "run {run_id} is not waiting for approval"
        )),
        DaemonRunStatus::WaitingForUserQuestion => DaemonProblem::question_state_conflict(format!(
            "run {run_id} is not waiting for user input"
        )),
        _ => DaemonProblem::run_state_conflict(format!(
            "run {run_id} is not waiting for {}",
            waiting_state_name(expected_status)
        )),
    }
}

fn normalize_rebuilt_session_queues(
    runs: &mut BTreeMap<String, RunRecord>,
    session_runs: &mut BTreeMap<String, SessionRunState>,
    originals: &mut BTreeMap<String, Option<RunRecord>>,
) -> Result<Vec<(RunRecord, Option<RunEvent>)>> {
    let mut normalized = Vec::new();
    for state in session_runs.values_mut() {
        if let Some(active_run_id) = state.active_run_id.as_ref()
            && let Some(record) = runs.get_mut(active_run_id)
            && record.view.queued_position.is_some()
        {
            originals
                .entry(active_run_id.clone())
                .or_insert_with(|| Some(record.clone()));
            record.view.queued_position = None;
            record.view.updated_at_ms = now_ms();
            normalized.push((record.clone(), None));
        }
        let queued_run_ids = state.queued_run_ids.iter().cloned().collect::<Vec<_>>();
        let mut normalized_queue = VecDeque::new();
        for run_id in queued_run_ids {
            let Some(record) = runs.get_mut(&run_id) else {
                continue;
            };
            if record.view.status.is_terminal() {
                continue;
            }
            let status_changed = record.view.status != DaemonRunStatus::Queued;
            if status_changed && !restart_requeue_allowed(&record.payload) {
                originals
                    .entry(run_id.clone())
                    .or_insert_with(|| Some(record.clone()));
                transition_run_status(&mut record.view, DaemonRunStatus::Interrupted)?;
                clear_pending_state(&mut record.view);
                record.view.error = Some(
                    "daemon restarted with a non-replayable waiting run outside the active slot"
                        .to_string(),
                );
                record.view.finished_at_ms = Some(now_ms());
                record.view.queued_position = None;
                record.view.updated_at_ms = now_ms();
                normalized.push((record.clone(), Some(RunEvent::Interrupted)));
                continue;
            }
            let queued_position = Some(normalized_queue.len() + 1);
            let changed = status_changed || record.view.queued_position != queued_position;
            if !changed {
                normalized_queue.push_back(run_id);
                continue;
            }
            originals
                .entry(run_id.clone())
                .or_insert_with(|| Some(record.clone()));
            if status_changed {
                transition_run_status_for_restart_recovery(
                    &mut record.view,
                    DaemonRunStatus::Queued,
                )?;
                clear_pending_state(&mut record.view);
                record.view.error = None;
                record.view.finished_at_ms = None;
            }
            record.view.queued_position = queued_position;
            record.view.updated_at_ms = now_ms();
            normalized.push((
                record.clone(),
                Some(RunEvent::Queued {
                    position: queued_position.expect("queued position set"),
                }),
            ));
            normalized_queue.push_back(run_id);
        }
        state.queued_run_ids = normalized_queue;
    }
    Ok(normalized)
}

fn restart_requeue_allowed(payload: &RunRequestPayload) -> bool {
    matches!(
        payload,
        RunRequestPayload::MailboxDelivery { .. }
            | RunRequestPayload::ObservationMaterialization { .. }
            | RunRequestPayload::ParentClarification { .. }
    ) || scheduled_run_origin(payload).is_some()
}

fn build_run_indexes(runs: &BTreeMap<String, RunRecord>) -> RunIndexes {
    let mut indexes = RunIndexes::default();
    for record in runs.values() {
        index_run_record(record, &mut indexes);
    }
    indexes
}

struct SessionQueueTransition {
    dirty_queue_records: Vec<RunRecord>,
    started_record: Option<RunRecord>,
    finished_run_was_active: bool,
    session_idle: bool,
}

fn advance_session_queue_locked(
    runs: &mut BTreeMap<String, RunRecord>,
    session_runs: &mut BTreeMap<String, SessionRunState>,
    session_id: &str,
    finished_run_id: Option<&str>,
) -> Result<SessionQueueTransition> {
    let Some(state) = session_runs.get_mut(session_id) else {
        return Ok(SessionQueueTransition {
            dirty_queue_records: Vec::new(),
            started_record: None,
            finished_run_was_active: false,
            session_idle: true,
        });
    };

    let finished_run_was_active =
        finished_run_id.is_some_and(|run_id| state.active_run_id.as_deref() == Some(run_id));
    if finished_run_was_active {
        state.active_run_id = None;
    }

    let started_record = if state.active_run_id.is_none() {
        promote_next_queued_run_locked(runs, state)?
    } else {
        None
    };
    let dirty_queue_records = collect_queue_records(state, runs)?;
    let session_idle = state.active_run_id.is_none() && state.queued_run_ids.is_empty();
    if session_idle {
        session_runs.remove(session_id);
    }

    Ok(SessionQueueTransition {
        dirty_queue_records,
        started_record,
        finished_run_was_active,
        session_idle,
    })
}

fn promote_next_queued_run_locked(
    runs: &mut BTreeMap<String, RunRecord>,
    state: &mut SessionRunState,
) -> Result<Option<RunRecord>> {
    while let Some(next_run_id) = state.queued_run_ids.pop_front() {
        let record = runs
            .get_mut(&next_run_id)
            .ok_or_else(|| anyhow!("unknown queued run {next_run_id}"))?;
        if record.view.status.is_terminal() {
            continue;
        }
        state.active_run_id = Some(next_run_id);
        transition_run_status(&mut record.view, DaemonRunStatus::Running)?;
        record.view.started_at_ms.get_or_insert(now_ms());
        record.view.updated_at_ms = now_ms();
        record.view.queued_position = None;
        return Ok(Some(record.clone()));
    }
    state.active_run_id = None;
    Ok(None)
}

fn collect_queue_records(
    state: &SessionRunState,
    runs: &mut BTreeMap<String, RunRecord>,
) -> Result<Vec<RunRecord>> {
    let mut dirty = Vec::new();
    for (index, run_id) in state.queued_run_ids.iter().enumerate() {
        if let Some(record) = runs.get_mut(run_id) {
            transition_run_status(&mut record.view, DaemonRunStatus::Queued)?;
            record.view.queued_position = Some(index + 1);
            record.view.updated_at_ms = now_ms();
            dirty.push(record.clone());
        }
    }
    Ok(dirty)
}

fn snapshot_session_run_records(
    runs: &BTreeMap<String, RunRecord>,
    state: &SessionRunState,
    originals: &mut BTreeMap<String, Option<RunRecord>>,
) {
    if let Some(run_id) = state.active_run_id.as_ref() {
        originals
            .entry(run_id.clone())
            .or_insert_with(|| runs.get(run_id).cloned());
    }
    for run_id in &state.queued_run_ids {
        originals
            .entry(run_id.clone())
            .or_insert_with(|| runs.get(run_id).cloned());
    }
}

fn persist_run_batch_or_rollback(
    run_store: &FileRunStore,
    records: &[RunRecord],
    originals: &BTreeMap<String, Option<RunRecord>>,
) -> Result<()> {
    let mut persisted = Vec::new();
    for record in records {
        if let Err(error) = run_store.save_run(record) {
            rollback_persisted_run_batch(run_store, originals, &persisted);
            return Err(error);
        }
        persisted.push(record.view.run_id.clone());
    }
    Ok(())
}

fn rollback_persisted_run_batch(
    run_store: &FileRunStore,
    originals: &BTreeMap<String, Option<RunRecord>>,
    persisted_run_ids: &[String],
) {
    for run_id in persisted_run_ids.iter().rev() {
        match originals.get(run_id) {
            Some(Some(record)) => {
                let _ = run_store.save_run(record);
            }
            Some(None) => {
                let path = run_store.run_path(run_id);
                if path.exists() {
                    let _ = fs::remove_file(path);
                }
            }
            None => {}
        }
    }
}

fn restore_run_record_snapshots(
    runs: &mut BTreeMap<String, RunRecord>,
    originals: &BTreeMap<String, Option<RunRecord>>,
) {
    for (run_id, original) in originals {
        match original {
            Some(record) => {
                runs.insert(run_id.clone(), record.clone());
            }
            None => {
                runs.remove(run_id);
            }
        }
    }
}

fn restore_session_run_state(
    session_runs: &mut BTreeMap<String, SessionRunState>,
    session_id: &str,
    previous: Option<SessionRunState>,
) {
    if let Some(state) = previous {
        session_runs.insert(session_id.to_string(), state);
    } else {
        session_runs.remove(session_id);
    }
}

fn index_run_record(record: &RunRecord, indexes: &mut RunIndexes) {
    if record.view.status.is_terminal() {
        return;
    }
    if let Some(origin) = scheduled_run_origin(&record.payload) {
        indexes
            .scheduled_by_fire
            .entry(origin.schedule_id)
            .or_default()
            .insert(origin.fire_at_ms, record.view.run_id.clone());
        return;
    }
    if matches!(&record.payload, RunRequestPayload::MailboxDelivery { .. }) {
        indexes
            .mailbox_by_session
            .entry(record.view.session_id.clone())
            .or_default()
            .insert(record.view.run_id.clone());
    }
}

fn deindex_run_record(record: &RunRecord, indexes: &mut RunIndexes) {
    if let Some(origin) = scheduled_run_origin(&record.payload) {
        let remove_schedule = indexes
            .scheduled_by_fire
            .get_mut(&origin.schedule_id)
            .map(|entries| {
                if entries
                    .get(&origin.fire_at_ms)
                    .is_some_and(|run_id| run_id == &record.view.run_id)
                {
                    entries.remove(&origin.fire_at_ms);
                }
                entries.is_empty()
            })
            .unwrap_or(false);
        if remove_schedule {
            indexes.scheduled_by_fire.remove(&origin.schedule_id);
        }
        return;
    }
    if matches!(&record.payload, RunRequestPayload::MailboxDelivery { .. }) {
        let session_id = &record.view.session_id;
        let remove_session = indexes
            .mailbox_by_session
            .get_mut(session_id)
            .map(|run_ids| {
                run_ids.remove(&record.view.run_id);
                run_ids.is_empty()
            })
            .unwrap_or(false);
        if remove_session {
            indexes.mailbox_by_session.remove(session_id);
        }
    }
}

fn run_status_threshold_ms(env_name: &str, default_value: u64) -> u64 {
    std::env::var_os(env_name)
        .and_then(|value| value.to_str().and_then(|value| value.parse::<u64>().ok()))
        .unwrap_or(default_value)
}

fn run_activity_at_ms(run: &RunView) -> u64 {
    [
        Some(run.submitted_at_ms),
        Some(run.updated_at_ms),
        run.started_at_ms,
        run.finished_at_ms,
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(run.updated_at_ms)
}

fn combine_debug_prune_responses(
    mut left: RunRetentionPruneResponse,
    right: RunRetentionPruneResponse,
) -> RunRetentionPruneResponse {
    left.matched_run_count = left
        .matched_run_count
        .saturating_add(right.matched_run_count);
    left.candidate_debug_bytes = left
        .candidate_debug_bytes
        .saturating_add(right.candidate_debug_bytes);
    left.pruned_debug_bytes = left
        .pruned_debug_bytes
        .saturating_add(right.pruned_debug_bytes);
    left.candidate_run_ids.extend(right.candidate_run_ids);
    left.candidate_run_ids.sort();
    left.candidate_run_ids.dedup();
    left.pruned_debug_run_ids.extend(right.pruned_debug_run_ids);
    left.pruned_debug_run_ids.sort();
    left.pruned_debug_run_ids.dedup();
    left
}

fn record_stale_run_sample(sample: &mut Vec<(u64, String)>, idle_ms: u64, run_id: String) {
    sample.push((idle_ms, run_id));
    sample.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    if sample.len() > MAX_STALE_RUN_STATUS_IDS {
        sample.pop();
    }
}

fn output_preview(content: &str, max_chars: usize) -> (String, bool) {
    let mut preview = String::new();
    let mut truncated = false;
    for (index, character) in content.chars().enumerate() {
        if index >= max_chars {
            truncated = true;
            break;
        }
        preview.push(character);
    }
    (preview, truncated)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::fs;
    use std::sync::atomic::AtomicU64;
    use std::time::{Duration, Instant};

    use anyhow::Result;
    use kheish_agent::{
        AgentId, AgentRecord, AgentStatus, ChildRetentionPolicy, MailboxMessage,
        ManagedAgentSnapshot,
    };
    use kheish_runtime::{DebugArtifact, DebugArtifactFormat, DebugCaptureLevel};
    use serde_json::json;
    use tempfile::tempdir;

    use super::{
        QUEUED_RUN_LAG_WARNING_THRESHOLD_MS, RunCancellationResult, RunQueueAdvanceResult,
        RunService, STALE_NON_TERMINAL_RUN_THRESHOLD_MS, rollback_persisted_run_batch,
        transition_run_status, transition_run_status_for_restart_recovery,
        validate_approval_resolution_ids,
    };
    use crate::debug::{DEBUG_MAX_STORE_BYTES_ENV, DEBUG_TTL_MS_ENV, debug_capture_env_lock};
    use crate::events::DaemonEventBus;
    use crate::memory::FileRunMemoryStore;
    use crate::problems::DaemonProblem;
    use crate::runs::{
        DaemonRunKind, DaemonRunStatus, FileRunStore, RunEvent, RunEventEntry, RunRecord,
        RunRequestPayload, RunRequestSummary, RunView, ScheduledRunOrigin, SessionRunState,
        rebuild_pending_question_index, rebuild_session_run_state,
    };
    use crate::{
        ChannelDeliveryRunRequest, DaemonOutputRecord, FileDebugStore, PendingQuestionView,
        ResolveApprovalsRequest, ResolveUserQuestionRequest, SubmitInputRequest,
        summarize_mailbox_request,
    };
    use kheish_types::{
        ApprovalRequest, ApprovalResolution, ApprovalResolutionBehavior, ConversationKey,
        ModelGenerationConfig, UserQuestion, UserQuestionAnswer, UserQuestionRequest,
        UserQuestionResolution,
    };

    fn sample_run(run_id: &str, session_id: &str) -> RunRecord {
        RunRecord {
            view: RunView {
                run_id: run_id.to_string(),
                session_id: session_id.to_string(),
                agent_id: "agent-1".to_string(),
                kind: DaemonRunKind::Input,
                status: DaemonRunStatus::Queued,
                submitted_at_ms: 1,
                updated_at_ms: 1,
                started_at_ms: None,
                finished_at_ms: None,
                queued_position: None,
                request: RunRequestSummary {
                    source_plugin: "daemon".to_string(),
                    source_kind: "api".to_string(),
                    actor_id: "user".to_string(),
                    text_preview: Some("hello".to_string()),
                    provider: None,
                    model: None,
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
            payload: RunRequestPayload::MailboxDelivery {
                agent_id: "agent-1".to_string(),
                messages: Vec::new(),
            },
        }
    }

    fn sample_debug_artifact(run_id: &str) -> DebugArtifact {
        DebugArtifact {
            timestamp_ms: 1,
            session_id: Some("session-1".to_string()),
            agent_id: Some("agent-1".to_string()),
            run_id: Some(run_id.to_string()),
            level: DebugCaptureLevel::Full,
            name: "provider-request".to_string(),
            payload: json!({"ok": true}),
            turn: Some(1),
            attempt: Some(1),
            format: DebugArtifactFormat::Json,
        }
    }

    fn sample_input_request() -> SubmitInputRequest {
        SubmitInputRequest {
            source_plugin: None,
            source_kind: None,
            actor_id: None,
            provider: None,
            content: "hello".to_string(),
            input_items: Vec::new(),
            attachments: Vec::new(),
            generation: Some(ModelGenerationConfig::default()),
            completion_requirements: None,
            metadata: None,
            reply_address: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
        }
    }

    fn sample_channel_delivery_request() -> ChannelDeliveryRunRequest {
        ChannelDeliveryRunRequest {
            channel_id: "channel-1".to_string(),
            thread_root_message_id: "channel-message-1".to_string(),
            origin_message_id: "channel-message-2".to_string(),
            human_origin_message_id: "channel-message-2".to_string(),
            turn_id: "channel-turn-1".to_string(),
            addressed_member_ids: Vec::new(),
            provider: None,
            model: None,
        }
    }

    fn channel_approval_waiting_run(run_id: &str, session_id: &str) -> RunRecord {
        let mut record = approval_waiting_run(run_id, session_id);
        let request = sample_channel_delivery_request();
        record.view.kind = DaemonRunKind::ChannelDelivery;
        record.view.request.source_kind = "channel".to_string();
        record.view.input_metadata = Some(json!({
            "channel_id": request.channel_id.clone(),
            "thread_root_message_id": request.thread_root_message_id.clone(),
            "origin_message_id": request.origin_message_id.clone(),
            "human_origin_message_id": request.human_origin_message_id.clone(),
            "turn_id": request.turn_id.clone(),
        }));
        record.payload = RunRequestPayload::ChannelDelivery { request };
        record
    }

    fn channel_question_waiting_run(run_id: &str, session_id: &str) -> RunRecord {
        let mut record = question_waiting_run(run_id, session_id);
        let request = sample_channel_delivery_request();
        record.view.kind = DaemonRunKind::ChannelDelivery;
        record.view.request.source_kind = "channel".to_string();
        record.view.input_metadata = Some(json!({
            "channel_id": request.channel_id.clone(),
            "thread_root_message_id": request.thread_root_message_id.clone(),
            "origin_message_id": request.origin_message_id.clone(),
            "human_origin_message_id": request.human_origin_message_id.clone(),
            "turn_id": request.turn_id.clone(),
        }));
        record.payload = RunRequestPayload::ChannelDelivery { request };
        record
    }

    fn sample_snapshot(
        session_id: &str,
        approvals: Vec<ApprovalRequest>,
        questions: Vec<UserQuestionRequest>,
    ) -> ManagedAgentSnapshot {
        ManagedAgentSnapshot {
            agent: AgentRecord {
                id: AgentId("agent-1".to_string()),
                parent: None,
                name: Some("agent-1".to_string()),
                path: Some("agent-1".to_string()),
                nickname: None,
                conversation: ConversationKey {
                    session_id: session_id.to_string(),
                    thread_id: None,
                },
                status: AgentStatus::Running,
                retention: ChildRetentionPolicy::Retain,
                spawned_by_run_id: None,
                spawn_request_id: None,
                spawned_at_ms: 1,
                settled_at_ms: None,
                closed_at_ms: None,
                subtasks: Vec::new(),
                sidechain_session_id: None,
                fork_context: None,
                daemon_owned_worktree: None,
            },
            pending_approvals: approvals,
            pending_questions: questions,
            last_assistant_message: None,
            journal_len: 0,
            checkpoint_len: 0,
            last_error: None,
        }
    }

    fn active_session_runs(session_id: &str, run_id: &str) -> BTreeMap<String, SessionRunState> {
        BTreeMap::from([(
            session_id.to_string(),
            SessionRunState {
                active_run_id: Some(run_id.to_string()),
                queued_run_ids: VecDeque::new(),
            },
        )])
    }

    fn approval_waiting_run(run_id: &str, session_id: &str) -> RunRecord {
        let mut record = sample_run(run_id, session_id);
        record.view.status = DaemonRunStatus::WaitingForApproval;
        record.view.pending_approval_ids = vec!["approval-1".to_string()];
        record.view.pending_approvals = vec![approval_request()];
        record.payload = RunRequestPayload::Input {
            request: sample_input_request(),
            idempotency: None,
        };
        record
    }

    fn approval_request() -> ApprovalRequest {
        ApprovalRequest {
            id: "approval-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "echo".to_string(),
            input: json!({"text": "hello"}),
            scope: "workspace".to_string(),
            reason: "requires approval".to_string(),
        }
    }

    fn approval_resolution() -> ApprovalResolution {
        ApprovalResolution {
            request_id: "approval-1".to_string(),
            behavior: ApprovalResolutionBehavior::Allow,
            updated_input: None,
            justification: Some("approved".to_string()),
            reason: None,
        }
    }

    fn event_position(events: &[RunEvent], predicate: impl Fn(&RunEvent) -> bool) -> Option<usize> {
        events.iter().position(predicate)
    }

    fn scheduled_run(
        run_id: &str,
        session_id: &str,
        schedule_id: &str,
        fire_at_ms: u64,
    ) -> RunRecord {
        let mut record = sample_run(run_id, session_id);
        record.view.kind = DaemonRunKind::ScheduledInput;
        record.view.status = DaemonRunStatus::Running;
        record.payload = RunRequestPayload::ScheduledInput {
            schedule_id: schedule_id.to_string(),
            fire_at_ms,
            request: sample_input_request(),
        };
        record
    }

    fn scheduled_waiting_approval_run(
        run_id: &str,
        session_id: &str,
        schedule_id: &str,
        fire_at_ms: u64,
    ) -> RunRecord {
        let mut record = scheduled_run(run_id, session_id, schedule_id, fire_at_ms);
        record.view.status = DaemonRunStatus::WaitingForApproval;
        record.view.pending_approval_ids = vec!["approval-1".to_string()];
        record
    }

    fn question_waiting_run(run_id: &str, session_id: &str) -> RunRecord {
        let mut record = sample_run(run_id, session_id);
        record.view.status = DaemonRunStatus::WaitingForUserQuestion;
        record.view.pending_question_ids = vec!["question-1".to_string()];
        record.view.pending_questions = vec![UserQuestionRequest {
            id: "question-1".to_string(),
            tool_call_id: "tool-call-1".to_string(),
            questions: vec![UserQuestion {
                id: "question-a".to_string(),
                header: "Need input".to_string(),
                question: "Provide clarification".to_string(),
                options: Vec::new(),
                multi_select: false,
            }],
            created_at_ms: 1,
            expires_at_ms: None,
        }];
        record.payload = RunRequestPayload::Input {
            request: sample_input_request(),
            idempotency: None,
        };
        record
    }

    #[tokio::test]
    async fn run_service_schedules_active_and_queued_runs() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let first = service
            .schedule_run(sample_run("run-1", "session-1"))
            .await?;
        let second = service
            .schedule_run(sample_run("run-2", "session-1"))
            .await?;

        assert!(first.started_immediately);
        assert!(!second.started_immediately);
        assert_eq!(
            service.active_run_id("session-1").await.as_deref(),
            Some("run-1")
        );
        assert_eq!(service.get_run("run-2").await?.queued_position, Some(1));
        Ok(())
    }

    #[tokio::test]
    async fn run_service_prunes_debug_evidence_without_deleting_runs_or_events() -> Result<()> {
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let run_memory_store = FileRunMemoryStore::new(temp.path());
        let debug_store = FileDebugStore::new(temp.path());

        let mut completed = sample_run("run-2", "session-1");
        completed.view.status = DaemonRunStatus::Completed;
        completed.view.started_at_ms = Some(1);
        completed.view.finished_at_ms = Some(1);
        completed.view.updated_at_ms = 1;
        let mut completed_second = sample_run("run-10", "session-1");
        completed_second.view.status = DaemonRunStatus::Completed;
        completed_second.view.started_at_ms = Some(2);
        completed_second.view.finished_at_ms = Some(2);
        completed_second.view.updated_at_ms = 2;
        let mut legacy_finished = sample_run("run-legacy", "session-1");
        legacy_finished.view.status = DaemonRunStatus::Completed;
        legacy_finished.view.started_at_ms = Some(3);
        legacy_finished.view.finished_at_ms = None;
        legacy_finished.view.updated_at_ms = 3;
        let mut other_session = sample_run("run-other", "session-2");
        other_session.view.status = DaemonRunStatus::Completed;
        other_session.view.started_at_ms = Some(1);
        other_session.view.finished_at_ms = Some(1);
        other_session.view.updated_at_ms = 1;
        let mut recent = sample_run("run-recent", "session-1");
        recent.view.status = DaemonRunStatus::Completed;
        recent.view.started_at_ms = Some(9_500);
        recent.view.finished_at_ms = Some(9_500);
        recent.view.updated_at_ms = 9_500;
        let mut running = sample_run("run-running", "session-1");
        running.view.status = DaemonRunStatus::Running;
        running.view.started_at_ms = Some(1);
        running.view.updated_at_ms = 1;
        run_store.save_run(&completed)?;
        run_store.save_run(&completed_second)?;
        run_store.save_run(&legacy_finished)?;
        run_store.save_run(&other_session)?;
        run_store.save_run(&recent)?;
        run_store.save_run(&running)?;
        run_store.append_event(&RunEventEntry {
            timestamp_ms: 1,
            run_id: "run-2".to_string(),
            session_id: "session-1".to_string(),
            agent_id: "agent-1".to_string(),
            event: RunEvent::Completed,
        })?;
        debug_store.append_artifact(&sample_debug_artifact("run-2"))?;
        debug_store.append_artifact(&sample_debug_artifact("run-10"))?;
        debug_store.append_artifact(&sample_debug_artifact("run-legacy"))?;
        debug_store.append_artifact(&sample_debug_artifact("run-other"))?;
        debug_store.append_artifact(&sample_debug_artifact("run-recent"))?;
        debug_store.append_artifact(&sample_debug_artifact("run-running"))?;

        let service = RunService::new(
            run_store.clone(),
            run_memory_store,
            debug_store.clone(),
            DaemonEventBus::new(16),
            BTreeMap::from([
                ("run-2".to_string(), completed),
                ("run-10".to_string(), completed_second),
                ("run-legacy".to_string(), legacy_finished),
                ("run-other".to_string(), other_session),
                ("run-recent".to_string(), recent),
                ("run-running".to_string(), running),
            ]),
            BTreeMap::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let dry_run = service
            .prune_terminal_run_debug_evidence(1_000, Some("session-1"), Some(1), true, 10_000)
            .await?;
        assert_eq!(dry_run.limit, Some(1));
        assert_eq!(dry_run.matched_run_count, 3);
        assert_eq!(dry_run.candidate_run_ids, vec!["run-2".to_string()]);
        assert!(dry_run.candidate_debug_bytes > 0);
        assert!(dry_run.pruned_debug_run_ids.is_empty());
        assert_eq!(dry_run.pruned_debug_bytes, 0);
        assert!(debug_store.load_view("run-2")?.is_some());

        let pruned = service
            .prune_terminal_run_debug_evidence(1_000, Some("session-1"), Some(1), false, 10_000)
            .await?;
        assert_eq!(pruned.pruned_debug_run_ids, vec!["run-2".to_string()]);
        assert_eq!(pruned.pruned_debug_bytes, dry_run.candidate_debug_bytes);
        assert!(debug_store.load_view("run-2")?.is_none());
        assert!(debug_store.load_view("run-10")?.is_some());
        assert!(debug_store.load_view("run-legacy")?.is_some());
        assert!(debug_store.load_view("run-other")?.is_some());
        assert!(debug_store.load_view("run-recent")?.is_some());
        assert!(debug_store.load_view("run-running")?.is_some());
        assert!(run_store.load_run("run-2")?.is_some());
        assert_eq!(run_store.load_events("run-2")?.len(), 1);

        let legacy_pruned = service
            .prune_terminal_run_debug_evidence(1_000, Some("session-1"), Some(2), false, 10_000)
            .await?;
        assert_eq!(
            legacy_pruned.pruned_debug_run_ids,
            vec!["run-10".to_string(), "run-legacy".to_string()]
        );
        assert!(debug_store.load_view("run-legacy")?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn run_service_global_debug_budget_prunes_only_terminal_runs() -> Result<()> {
        let _guard = debug_capture_env_lock();
        unsafe {
            std::env::set_var(DEBUG_MAX_STORE_BYTES_ENV, "7000");
        }
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let run_memory_store = FileRunMemoryStore::new(temp.path());
        let debug_store = FileDebugStore::new(temp.path());
        unsafe {
            std::env::remove_var(DEBUG_MAX_STORE_BYTES_ENV);
        }

        let mut old = sample_run("run-old", "session-1");
        old.view.status = DaemonRunStatus::Completed;
        old.view.finished_at_ms = Some(1);
        old.view.updated_at_ms = 1;
        let mut running = sample_run("run-running", "session-1");
        running.view.status = DaemonRunStatus::Running;
        running.view.updated_at_ms = 1;
        let mut waiting = sample_run("run-waiting", "session-1");
        waiting.view.status = DaemonRunStatus::WaitingForApproval;
        waiting.view.updated_at_ms = 1;

        for run_id in ["run-old", "run-running", "run-waiting", "run-orphan"] {
            let mut artifact = sample_debug_artifact(run_id);
            artifact.payload = json!({"body": "x".repeat(4_000), "run_id": run_id});
            debug_store.append_artifact(&artifact)?;
        }

        let service = RunService::new(
            run_store,
            run_memory_store,
            debug_store.clone(),
            DaemonEventBus::new(16),
            BTreeMap::from([
                ("run-old".to_string(), old),
                ("run-running".to_string(), running),
                ("run-waiting".to_string(), waiting),
            ]),
            BTreeMap::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let response = service
            .prune_debug_store_over_budget(10)
            .await?
            .expect("global debug budget is configured");
        assert!(
            response
                .pruned_debug_run_ids
                .contains(&"run-old".to_string())
        );
        assert!(debug_store.load_view("run-old")?.is_none());
        assert!(debug_store.load_view("run-orphan")?.is_none());
        assert!(debug_store.load_view("run-running")?.is_some());
        assert!(debug_store.load_view("run-waiting")?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn run_service_debug_ttl_protects_non_terminal_and_prunes_orphan_bundles() -> Result<()> {
        let _guard = debug_capture_env_lock();
        unsafe {
            std::env::set_var(DEBUG_TTL_MS_ENV, "100");
            std::env::remove_var(DEBUG_MAX_STORE_BYTES_ENV);
        }
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let run_memory_store = FileRunMemoryStore::new(temp.path());
        let debug_store = FileDebugStore::new(temp.path());
        unsafe {
            std::env::remove_var(DEBUG_TTL_MS_ENV);
        }

        let mut waiting = sample_run("run-waiting", "session-1");
        waiting.view.status = DaemonRunStatus::WaitingForApproval;
        waiting.view.updated_at_ms = 1;
        let mut running = sample_run("run-running", "session-1");
        running.view.status = DaemonRunStatus::Running;
        running.view.updated_at_ms = 1;
        let mut recent_completed = sample_run("run-recent-completed", "session-1");
        recent_completed.view.status = DaemonRunStatus::Completed;
        recent_completed.view.finished_at_ms = Some(950);
        recent_completed.view.updated_at_ms = 950;

        debug_store.append_artifact(&sample_debug_artifact("run-waiting"))?;
        debug_store.append_artifact(&sample_debug_artifact("run-running"))?;
        debug_store.append_artifact(&sample_debug_artifact("run-recent-completed"))?;
        debug_store.append_artifact(&sample_debug_artifact("run-orphan"))?;

        let service = RunService::new(
            run_store,
            run_memory_store,
            debug_store.clone(),
            DaemonEventBus::new(16),
            BTreeMap::from([
                ("run-waiting".to_string(), waiting),
                ("run-running".to_string(), running),
                ("run-recent-completed".to_string(), recent_completed),
            ]),
            BTreeMap::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let response = service
            .prune_expired_debug_evidence(1_000)
            .await?
            .expect("ttl prune response");
        assert_eq!(response.cutoff_ms, 900);
        assert!(
            response
                .pruned_debug_run_ids
                .contains(&"run-orphan".to_string())
        );
        assert!(debug_store.load_view("run-waiting")?.is_some());
        assert!(debug_store.load_view("run-recent-completed")?.is_some());
        assert!(debug_store.load_view("run-orphan")?.is_none());
        assert!(debug_store.load_view("run-running")?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn run_service_prune_rejects_zero_retention_with_typed_problem() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let error = service
            .prune_terminal_run_debug_evidence(0, None, None, false, 10_000)
            .await
            .expect_err("zero retention should be rejected");
        let problem = error
            .downcast_ref::<DaemonProblem>()
            .expect("retention validation should be a typed daemon problem");
        assert_eq!(problem.status, 400);
        assert_eq!(problem.domain, "runs");
        assert_eq!(problem.code, "run_retention_invalid_request");
        assert_eq!(problem.detail(), "older_than_ms must be greater than zero");

        let error = service
            .prune_terminal_run_debug_evidence(1, None, Some(0), false, 10_000)
            .await
            .expect_err("zero limit should be rejected");
        let problem = error
            .downcast_ref::<DaemonProblem>()
            .expect("limit validation should be a typed daemon problem");
        assert_eq!(problem.status, 400);
        assert_eq!(problem.domain, "runs");
        assert_eq!(problem.code, "run_retention_invalid_request");
        assert_eq!(problem.detail(), "limit must be greater than zero");
        Ok(())
    }

    #[tokio::test]
    async fn run_service_status_snapshot_reports_operator_counts() -> Result<()> {
        let temp = tempdir()?;
        let mut queued = sample_run("run-1", "session-1");
        queued.view.status = DaemonRunStatus::Queued;
        queued.view.submitted_at_ms = 100;
        queued.view.updated_at_ms = 100;
        queued.view.pending_approval_ids = vec!["approval-1".to_string()];
        let mut running = sample_run("run-2", "session-1");
        running.view.status = DaemonRunStatus::Running;
        running.view.submitted_at_ms = 50;
        running.view.updated_at_ms = 50;
        let mut waiting_question = sample_run("run-3", "session-2");
        waiting_question.view.status = DaemonRunStatus::WaitingForUserQuestion;
        waiting_question.view.submitted_at_ms = 125;
        waiting_question.view.updated_at_ms = 125;
        waiting_question.view.pending_question_ids = vec!["question-1".to_string()];
        let mut completed = sample_run("run-4", "session-2");
        completed.view.status = DaemonRunStatus::Completed;
        completed.view.submitted_at_ms = 10;
        completed.view.updated_at_ms = 10;
        let mut waiting_approval = sample_run("run-5", "session-3");
        waiting_approval.view.status = DaemonRunStatus::WaitingForApproval;
        waiting_approval.view.submitted_at_ms = 150;
        waiting_approval.view.updated_at_ms = 150;
        waiting_approval.view.pending_approval_ids =
            vec!["approval-2".to_string(), "approval-3".to_string()];
        let mut failed = sample_run("run-6", "session-4");
        failed.view.status = DaemonRunStatus::Failed;
        failed.view.submitted_at_ms = 25;
        failed.view.updated_at_ms = 25;
        let mut interrupted = sample_run("run-7", "session-5");
        interrupted.view.status = DaemonRunStatus::Interrupted;
        interrupted.view.submitted_at_ms = 30;
        interrupted.view.updated_at_ms = 30;
        let mut cancelled = sample_run("run-8", "session-6");
        cancelled.view.status = DaemonRunStatus::Cancelled;
        cancelled.view.submitted_at_ms = 35;
        cancelled.view.updated_at_ms = 35;

        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([
                ("run-1".to_string(), queued),
                ("run-2".to_string(), running),
                ("run-3".to_string(), waiting_question),
                ("run-4".to_string(), completed),
                ("run-5".to_string(), waiting_approval),
                ("run-6".to_string(), failed),
                ("run-7".to_string(), interrupted),
                ("run-8".to_string(), cancelled),
            ]),
            BTreeMap::from([(
                "session-1".to_string(),
                SessionRunState {
                    active_run_id: Some("run-2".to_string()),
                    queued_run_ids: VecDeque::from(["run-1".to_string()]),
                },
            )]),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let snapshot = service
            .status_snapshot(STALE_NON_TERMINAL_RUN_THRESHOLD_MS + 201)
            .await;
        assert_eq!(snapshot.total, 8);
        assert_eq!(snapshot.queued, 1);
        assert_eq!(snapshot.running, 1);
        assert_eq!(snapshot.waiting_for_approval, 1);
        assert_eq!(snapshot.waiting_for_user_question, 1);
        assert_eq!(snapshot.completed, 1);
        assert_eq!(snapshot.failed, 1);
        assert_eq!(snapshot.interrupted, 1);
        assert_eq!(snapshot.cancelled, 1);
        assert_eq!(snapshot.pending_approval_count, 3);
        assert_eq!(snapshot.pending_question_count, 1);
        assert_eq!(snapshot.max_session_queue_depth, 1);
        assert_eq!(snapshot.oldest_queued_run_id.as_deref(), Some("run-1"));
        assert_eq!(
            snapshot.oldest_queued_run_age_ms,
            Some(STALE_NON_TERMINAL_RUN_THRESHOLD_MS + 101)
        );
        assert_eq!(
            snapshot.queued_run_lag_threshold_ms,
            QUEUED_RUN_LAG_WARNING_THRESHOLD_MS
        );
        assert_eq!(
            snapshot.oldest_non_terminal_run_id.as_deref(),
            Some("run-2")
        );
        assert_eq!(
            snapshot.oldest_non_terminal_run_age_ms,
            Some(STALE_NON_TERMINAL_RUN_THRESHOLD_MS + 151)
        );
        assert_eq!(
            snapshot.oldest_idle_non_terminal_run_id.as_deref(),
            Some("run-2")
        );
        assert_eq!(
            snapshot.oldest_non_terminal_run_idle_ms,
            Some(STALE_NON_TERMINAL_RUN_THRESHOLD_MS + 151)
        );
        assert_eq!(
            snapshot.stale_non_terminal_run_threshold_ms,
            STALE_NON_TERMINAL_RUN_THRESHOLD_MS
        );
        assert_eq!(snapshot.stale_non_terminal_run_count, 4);
        assert_eq!(
            snapshot.stale_non_terminal_run_ids,
            vec![
                "run-2".to_string(),
                "run-1".to_string(),
                "run-3".to_string(),
                "run-5".to_string()
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_status_snapshot_marks_stale_from_idle_time() -> Result<()> {
        let temp = tempdir()?;
        let now = STALE_NON_TERMINAL_RUN_THRESHOLD_MS + 10_000;

        let mut recently_updated = sample_run("run-recent", "session-1");
        recently_updated.view.status = DaemonRunStatus::Running;
        recently_updated.view.submitted_at_ms = 1;
        recently_updated.view.updated_at_ms = now - 1_000;

        let mut idle = sample_run("run-idle", "session-1");
        idle.view.status = DaemonRunStatus::WaitingForApproval;
        idle.view.submitted_at_ms = 2;
        idle.view.updated_at_ms = 2;

        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([
                ("run-recent".to_string(), recently_updated),
                ("run-idle".to_string(), idle),
            ]),
            BTreeMap::<String, SessionRunState>::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let snapshot = service.status_snapshot(now).await;
        assert_eq!(
            snapshot.oldest_non_terminal_run_id.as_deref(),
            Some("run-recent"),
            "age is still tracked from submission for compatibility"
        );
        assert_eq!(
            snapshot.oldest_idle_non_terminal_run_id.as_deref(),
            Some("run-idle")
        );
        assert_eq!(snapshot.stale_non_terminal_run_count, 1);
        assert_eq!(
            snapshot.stale_non_terminal_run_ids,
            vec!["run-idle".to_string()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_status_snapshot_keeps_bounded_stale_sample() -> Result<()> {
        let temp = tempdir()?;
        let now = STALE_NON_TERMINAL_RUN_THRESHOLD_MS + 20_000;
        let mut runs = BTreeMap::new();

        let mut recent = sample_run("run-recent", "session-1");
        recent.view.status = DaemonRunStatus::Running;
        recent.view.submitted_at_ms = 1;
        recent.view.updated_at_ms = now - 500;
        runs.insert(recent.view.run_id.clone(), recent);

        for index in 0..10 {
            let mut record = sample_run(&format!("run-stale-{index}"), "session-1");
            record.view.status = DaemonRunStatus::WaitingForApproval;
            record.view.submitted_at_ms = index as u64;
            record.view.updated_at_ms = (index + 1) as u64 * 100;
            runs.insert(record.view.run_id.clone(), record);
        }

        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            runs,
            BTreeMap::<String, SessionRunState>::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let snapshot = service.status_snapshot(now).await;
        assert_eq!(snapshot.stale_non_terminal_run_count, 10);
        assert_eq!(snapshot.stale_non_terminal_run_ids.len(), 5);
        assert_eq!(
            snapshot.stale_non_terminal_run_ids,
            vec![
                "run-stale-0".to_string(),
                "run-stale-1".to_string(),
                "run-stale-2".to_string(),
                "run-stale-3".to_string(),
                "run-stale-4".to_string(),
            ]
        );
        assert!(
            !snapshot
                .stale_non_terminal_run_ids
                .contains(&"run-recent".to_string())
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_status_snapshot_large_state_is_bounded() -> Result<()> {
        let temp = tempdir()?;
        let mut runs = BTreeMap::new();
        let mut session_runs = BTreeMap::new();
        for index in 0..10_000 {
            let run_id = format!("run-{index}");
            let session_id = format!("session-{}", index / 10);
            let mut record = sample_run(&run_id, &session_id);
            record.view.submitted_at_ms = index as u64;
            record.view.status = match index % 8 {
                0 => DaemonRunStatus::Queued,
                1 => DaemonRunStatus::Running,
                2 => DaemonRunStatus::WaitingForApproval,
                3 => DaemonRunStatus::WaitingForUserQuestion,
                4 => DaemonRunStatus::Completed,
                5 => DaemonRunStatus::Failed,
                6 => DaemonRunStatus::Interrupted,
                _ => DaemonRunStatus::Cancelled,
            };
            if record.view.status == DaemonRunStatus::WaitingForApproval {
                record.view.pending_approval_ids = vec![format!("approval-{index}")];
            }
            if record.view.status == DaemonRunStatus::WaitingForUserQuestion {
                record.view.pending_question_ids = vec![format!("question-{index}")];
            }
            if record.view.status == DaemonRunStatus::Queued {
                session_runs
                    .entry(session_id.clone())
                    .or_insert_with(SessionRunState::default)
                    .queued_run_ids
                    .push_back(run_id.clone());
            } else if record.view.status == DaemonRunStatus::Running {
                session_runs
                    .entry(session_id.clone())
                    .or_insert_with(SessionRunState::default)
                    .active_run_id = Some(run_id.clone());
            }
            runs.insert(run_id, record);
        }

        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            runs,
            session_runs,
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );
        let started = Instant::now();
        let snapshot = service
            .status_snapshot(STALE_NON_TERMINAL_RUN_THRESHOLD_MS + 10_001)
            .await;
        let elapsed = started.elapsed();
        assert_eq!(snapshot.total, 10_000);
        assert_eq!(snapshot.queued, 1_250);
        assert_eq!(
            snapshot.queued_run_lag_threshold_ms,
            QUEUED_RUN_LAG_WARNING_THRESHOLD_MS
        );
        assert_eq!(snapshot.oldest_queued_run_id.as_deref(), Some("run-0"));
        assert_eq!(snapshot.running, 1_250);
        assert_eq!(snapshot.waiting_for_approval, 1_250);
        assert_eq!(snapshot.waiting_for_user_question, 1_250);
        assert_eq!(snapshot.completed, 1_250);
        assert_eq!(snapshot.failed, 1_250);
        assert_eq!(snapshot.interrupted, 1_250);
        assert_eq!(snapshot.cancelled, 1_250);
        assert_eq!(snapshot.pending_approval_count, 1_250);
        assert_eq!(snapshot.pending_question_count, 1_250);
        assert!(
            elapsed < Duration::from_secs(1),
            "status snapshot over 10k runs took {elapsed:?}"
        );
        Ok(())
    }

    #[test]
    fn run_service_run_events_repairs_missing_completed_event() -> Result<()> {
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let mut record = sample_run("run-1", "session-1");
        record.view.status = DaemonRunStatus::Completed;
        record.view.finished_at_ms = Some(2);
        run_store.save_run(&record)?;

        let service = RunService::new(
            run_store.clone(),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-1".to_string(), record)]),
            BTreeMap::<String, SessionRunState>::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let events = service.run_events("run-1")?;
        assert!(
            events
                .iter()
                .any(|entry| matches!(entry.event, RunEvent::Completed))
        );
        assert!(
            run_store
                .load_events("run-1")?
                .iter()
                .any(|entry| matches!(entry.event, RunEvent::Completed))
        );
        Ok(())
    }

    #[test]
    fn run_service_run_events_repairs_missing_terminal_events() -> Result<()> {
        let cases = vec![
            (
                DaemonRunStatus::Failed,
                Some("boom".to_string()),
                RunEvent::Failed {
                    error: "boom".to_string(),
                },
            ),
            (DaemonRunStatus::Interrupted, None, RunEvent::Interrupted),
            (DaemonRunStatus::Cancelled, None, RunEvent::Cancelled),
        ];

        for (index, (status, error, expected)) in cases.into_iter().enumerate() {
            let temp = tempdir()?;
            let run_store = FileRunStore::new(temp.path());
            let run_id = format!("run-terminal-{index}");
            let mut record = sample_run(&run_id, "session-1");
            record.view.status = status;
            record.view.started_at_ms = Some(1);
            record.view.updated_at_ms = 2;
            record.view.finished_at_ms = Some(2);
            record.view.error = error;
            run_store.save_run(&record)?;

            let service = RunService::new(
                run_store.clone(),
                FileRunMemoryStore::new(temp.path()),
                FileDebugStore::new(temp.path()),
                DaemonEventBus::new(16),
                BTreeMap::from([(run_id.clone(), record)]),
                BTreeMap::<String, SessionRunState>::new(),
                BTreeMap::<String, PendingQuestionView>::new(),
                AtomicU64::new(0),
            );

            let events = service.run_events(&run_id)?;
            assert!(
                events.iter().any(|entry| entry.event == expected),
                "missing terminal event {expected:?}: {}",
                serde_json::to_string_pretty(&events)?
            );
            assert!(
                run_store
                    .load_events(&run_id)?
                    .iter()
                    .any(|entry| entry.event == expected)
            );
        }
        Ok(())
    }

    #[test]
    fn run_service_run_events_repairs_reconstructable_lifecycle_events() -> Result<()> {
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());

        let mut queued = sample_run("run-queued", "session-1");
        queued.view.queued_position = Some(3);
        run_store.save_run(&queued)?;

        let mut running = sample_run("run-running", "session-1");
        running.view.status = DaemonRunStatus::Running;
        running.view.started_at_ms = Some(2);
        running.view.updated_at_ms = 2;
        run_store.save_run(&running)?;

        let mut completed = sample_run("run-output", "session-1");
        completed.view.status = DaemonRunStatus::Completed;
        completed.view.started_at_ms = Some(2);
        completed.view.updated_at_ms = 3;
        completed.view.finished_at_ms = Some(3);
        completed.view.outputs.push(DaemonOutputRecord {
            session_id: "session-1".to_string(),
            run_id: Some("run-output".to_string()),
            content: "done".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            source_kind: None,
            plugin: Some("daemon".to_string()),
            address: Some("session-1".to_string()),
        });
        run_store.save_run(&completed)?;

        let service = RunService::new(
            run_store.clone(),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([
                ("run-queued".to_string(), queued),
                ("run-running".to_string(), running),
                ("run-output".to_string(), completed.clone()),
            ]),
            BTreeMap::from([(
                "session-1".to_string(),
                SessionRunState {
                    active_run_id: Some("run-running".to_string()),
                    queued_run_ids: VecDeque::from(["run-queued".to_string()]),
                },
            )]),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let queued_events = service.run_events("run-queued")?;
        assert!(
            queued_events
                .iter()
                .any(|entry| matches!(entry.event, RunEvent::Accepted))
        );
        assert!(
            queued_events
                .iter()
                .any(|entry| matches!(entry.event, RunEvent::Queued { position: 3 }))
        );

        let running_events = service.run_events("run-running")?;
        assert!(
            running_events
                .iter()
                .any(|entry| matches!(entry.event, RunEvent::Accepted))
        );
        assert!(
            running_events
                .iter()
                .any(|entry| matches!(entry.event, RunEvent::Started))
        );

        let output_events = service.run_events("run-output")?;
        assert!(
            output_events
                .iter()
                .any(|entry| matches!(entry.event, RunEvent::Started))
        );
        assert!(output_events.iter().any(|entry| {
            matches!(
                &entry.event,
                RunEvent::Output { output } if output.content == "done"
            )
        }));
        assert!(
            output_events
                .iter()
                .any(|entry| matches!(entry.event, RunEvent::Completed))
        );

        let persisted_output_events = run_store.load_events("run-output")?;
        assert!(persisted_output_events.iter().any(|entry| {
            matches!(
                &entry.event,
                RunEvent::Output { output } if output == &completed.view.outputs[0]
            )
        }));
        Ok(())
    }

    #[test]
    fn run_service_run_events_repairs_missing_waiting_question_event() -> Result<()> {
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let record = question_waiting_run("run-1", "session-1");
        run_store.save_run(&record)?;

        let service = RunService::new(
            run_store.clone(),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-1".to_string(), record.clone())]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let expected = RunEvent::WaitingForUserQuestion {
            request_ids: vec!["question-1".to_string()],
            requests: record.view.pending_questions.clone(),
        };
        let events = service.run_events("run-1")?;
        assert!(events.iter().any(|entry| entry.event == expected));
        assert!(
            run_store
                .load_events("run-1")?
                .iter()
                .any(|entry| entry.event == expected)
        );
        Ok(())
    }

    #[test]
    fn run_service_run_events_repairs_legacy_waiting_question_event_payload() -> Result<()> {
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let mut record = question_waiting_run("run-question-audit", "session-1");
        record.view.updated_at_ms = 10;
        run_store.save_run(&record)?;
        run_store.append_event(&RunEventEntry {
            timestamp_ms: 10,
            run_id: "run-question-audit".to_string(),
            session_id: "session-1".to_string(),
            agent_id: "agent-1".to_string(),
            event: RunEvent::WaitingForUserQuestion {
                request_ids: record.view.pending_question_ids.clone(),
                requests: Vec::new(),
            },
        })?;

        let service = RunService::new(
            run_store.clone(),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-question-audit".to_string(), record.clone())]),
            BTreeMap::<String, SessionRunState>::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let events = service.run_events("run-question-audit")?;
        assert!(
            events.iter().any(|entry| {
                matches!(
                    &entry.event,
                    RunEvent::WaitingForUserQuestion { requests, .. }
                        if requests == &record.view.pending_questions
                )
            }),
            "run events should include full question request payloads: {events:#?}"
        );
        assert!(
            run_store
                .load_events("run-question-audit")?
                .iter()
                .any(|entry| {
                    matches!(
                        &entry.event,
                        RunEvent::WaitingForUserQuestion { requests, .. }
                            if requests == &record.view.pending_questions
                    )
                }),
            "run events should persist the repaired question payloads"
        );
        Ok(())
    }

    #[test]
    fn run_service_run_events_repairs_missing_approval_resolution_event() -> Result<()> {
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let resolution = ApprovalResolution {
            request_id: "approval-1".to_string(),
            behavior: ApprovalResolutionBehavior::Allow,
            updated_input: None,
            justification: Some("approved".to_string()),
            reason: None,
        };
        let mut record = sample_run("run-1", "session-1");
        record.view.status = DaemonRunStatus::Completed;
        record.view.finished_at_ms = Some(2);
        record.payload = RunRequestPayload::ApprovalResume {
            request: ResolveApprovalsRequest {
                idempotency_key: None,
                resolutions: vec![resolution.clone()],
            },
            original_request: None,
            scheduled_origin: None,
            channel_delivery: None,
        };
        run_store.save_run(&record)?;

        let service = RunService::new(
            run_store.clone(),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-1".to_string(), record)]),
            BTreeMap::<String, SessionRunState>::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let expected = RunEvent::ApprovalResolved {
            resolutions: vec![resolution],
        };
        let events = service.run_events("run-1")?;
        assert!(events.iter().any(|entry| entry.event == expected));
        assert!(
            run_store
                .load_events("run-1")?
                .iter()
                .any(|entry| entry.event == expected)
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_run_events_repairs_resume_events_before_final_state_event() -> Result<()> {
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let approval = approval_resolution();
        let mut approval_record = sample_run("run-approval", "session-1");
        approval_record.view.status = DaemonRunStatus::Completed;
        approval_record.view.started_at_ms = Some(2);
        approval_record.view.updated_at_ms = 3;
        approval_record.view.finished_at_ms = Some(3);
        approval_record.view.outputs.push(DaemonOutputRecord {
            session_id: "session-1".to_string(),
            run_id: Some("run-approval".to_string()),
            content: "approved".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            source_kind: None,
            plugin: Some("daemon".to_string()),
            address: Some("session-1".to_string()),
        });
        approval_record.payload = RunRequestPayload::ApprovalResume {
            request: ResolveApprovalsRequest {
                idempotency_key: None,
                resolutions: vec![approval.clone()],
            },
            original_request: None,
            scheduled_origin: None,
            channel_delivery: None,
        };
        run_store.save_run(&approval_record)?;
        run_store.append_event(&RunEventEntry {
            timestamp_ms: 6,
            run_id: "run-approval".to_string(),
            session_id: "session-1".to_string(),
            agent_id: "agent-1".to_string(),
            event: RunEvent::Completed,
        })?;

        let question_resolution = UserQuestionResolution {
            request_id: "question-1".to_string(),
            answers: vec![UserQuestionAnswer {
                question_id: "question-a".to_string(),
                selected_option_ids: Vec::new(),
                freeform_answer: Some("provided".to_string()),
            }],
            declined: false,
            justification: None,
        };
        let mut question_record = sample_run("run-question", "session-1");
        question_record.view.status = DaemonRunStatus::Failed;
        question_record.view.started_at_ms = Some(4);
        question_record.view.updated_at_ms = 5;
        question_record.view.finished_at_ms = Some(5);
        question_record.view.error = Some("provider failed after question resume".to_string());
        question_record.payload = RunRequestPayload::UserQuestionResume {
            request: ResolveUserQuestionRequest {
                idempotency_key: None,
                resolution: question_resolution.clone(),
            },
            original_request: None,
            scheduled_origin: None,
            channel_delivery: None,
        };
        run_store.save_run(&question_record)?;
        run_store.append_event(&RunEventEntry {
            timestamp_ms: 7,
            run_id: "run-question".to_string(),
            session_id: "session-1".to_string(),
            agent_id: "agent-1".to_string(),
            event: RunEvent::Failed {
                error: "provider failed after question resume".to_string(),
            },
        })?;

        let service = RunService::new(
            run_store.clone(),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([
                ("run-approval".to_string(), approval_record),
                ("run-question".to_string(), question_record),
            ]),
            BTreeMap::<String, SessionRunState>::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let approval_events = service
            .run_events("run-approval")?
            .into_iter()
            .map(|entry| entry.event)
            .collect::<Vec<_>>();
        assert!(matches!(approval_events.last(), Some(RunEvent::Completed)));
        assert!(
            event_position(&approval_events, |event| {
                matches!(event, RunEvent::ApprovalResolved { resolutions } if resolutions.as_slice() == std::slice::from_ref(&approval))
            })
            .expect("missing approval resolution event")
                < event_position(&approval_events, |event| matches!(event, RunEvent::Completed))
                    .expect("missing completed event"),
            "approval resolution should be reconstructed before final state: {approval_events:#?}"
        );
        let persisted_approval_events = run_store
            .load_events("run-approval")?
            .into_iter()
            .map(|entry| entry.event)
            .collect::<Vec<_>>();
        assert!(matches!(
            persisted_approval_events.last(),
            Some(RunEvent::Completed)
        ));

        let question_events = service
            .run_events("run-question")?
            .into_iter()
            .map(|entry| entry.event)
            .collect::<Vec<_>>();
        assert!(matches!(
            question_events.last(),
            Some(RunEvent::Failed { .. })
        ));
        assert!(
            event_position(&question_events, |event| {
                matches!(event, RunEvent::UserQuestionResolved { resolution } if resolution == &question_resolution)
            })
            .expect("missing question resolution event")
                < event_position(&question_events, |event| matches!(event, RunEvent::Failed { .. }))
                    .expect("missing failed event"),
            "question resolution should be reconstructed before final state: {question_events:#?}"
        );
        let session_events = service.session_run_events("session-1").await?;
        let session_approval_events = session_events
            .iter()
            .filter(|entry| entry.run_id == "run-approval")
            .map(|entry| &entry.event)
            .collect::<Vec<_>>();
        assert!(matches!(
            session_approval_events.last(),
            Some(RunEvent::Completed)
        ));
        let session_question_events = session_events
            .iter()
            .filter(|entry| entry.run_id == "run-question")
            .map(|entry| &entry.event)
            .collect::<Vec<_>>();
        assert!(matches!(
            session_question_events.last(),
            Some(RunEvent::Failed { .. })
        ));
        Ok(())
    }

    #[test]
    fn run_service_run_events_preserves_healthy_resume_event_order() -> Result<()> {
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let approval = approval_resolution();
        let output = DaemonOutputRecord {
            session_id: "session-1".to_string(),
            run_id: Some("run-healthy".to_string()),
            content: "approved".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            source_kind: None,
            plugin: Some("daemon".to_string()),
            address: Some("session-1".to_string()),
        };
        let mut record = sample_run("run-healthy", "session-1");
        record.view.status = DaemonRunStatus::Completed;
        record.view.started_at_ms = Some(2);
        record.view.updated_at_ms = 6;
        record.view.finished_at_ms = Some(6);
        record.view.outputs.push(output.clone());
        record.payload = RunRequestPayload::ApprovalResume {
            request: ResolveApprovalsRequest {
                idempotency_key: None,
                resolutions: vec![approval.clone()],
            },
            original_request: None,
            scheduled_origin: None,
            channel_delivery: None,
        };
        run_store.save_run(&record)?;
        for (timestamp_ms, event) in [
            (1, RunEvent::Accepted),
            (2, RunEvent::Started),
            (
                3,
                RunEvent::WaitingForApproval {
                    request_ids: vec!["approval-1".to_string()],
                    requests: Vec::new(),
                },
            ),
            (4, RunEvent::Started),
            (
                5,
                RunEvent::ApprovalResolved {
                    resolutions: vec![approval],
                },
            ),
            (6, RunEvent::Output { output }),
            (7, RunEvent::Completed),
        ] {
            run_store.append_event(&RunEventEntry {
                timestamp_ms,
                run_id: "run-healthy".to_string(),
                session_id: "session-1".to_string(),
                agent_id: "agent-1".to_string(),
                event,
            })?;
        }
        let original_events = run_store.load_events("run-healthy")?;
        let service = RunService::new(
            run_store.clone(),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-healthy".to_string(), record)]),
            BTreeMap::<String, SessionRunState>::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let events = service.run_events("run-healthy")?;
        assert_eq!(events, original_events);
        assert_eq!(run_store.load_events("run-healthy")?, original_events);
        Ok(())
    }

    #[test]
    fn run_service_run_events_repairs_legacy_waiting_approval_event_payload() -> Result<()> {
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let mut record = approval_waiting_run("run-approval-audit", "session-1");
        record.view.updated_at_ms = 10;
        run_store.save_run(&record)?;
        run_store.append_event(&RunEventEntry {
            timestamp_ms: 10,
            run_id: "run-approval-audit".to_string(),
            session_id: "session-1".to_string(),
            agent_id: "agent-1".to_string(),
            event: RunEvent::WaitingForApproval {
                request_ids: record.view.pending_approval_ids.clone(),
                requests: Vec::new(),
            },
        })?;

        let service = RunService::new(
            run_store.clone(),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-approval-audit".to_string(), record.clone())]),
            BTreeMap::<String, SessionRunState>::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let events = service.run_events("run-approval-audit")?;
        assert!(
            events.iter().any(|entry| {
                matches!(
                    &entry.event,
                    RunEvent::WaitingForApproval { requests, .. }
                        if requests == &record.view.pending_approvals
                )
            }),
            "run events should include full approval request payloads: {events:#?}"
        );
        assert!(
            run_store
                .load_events("run-approval-audit")?
                .iter()
                .any(|entry| {
                    matches!(
                        &entry.event,
                        RunEvent::WaitingForApproval { requests, .. }
                            if requests == &record.view.pending_approvals
                    )
                }),
            "run events should persist the repaired approval payloads"
        );
        Ok(())
    }

    #[test]
    fn run_service_run_events_does_not_duplicate_reconciled_state_events() -> Result<()> {
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let mut record = sample_run("run-1", "session-1");
        record.view.status = DaemonRunStatus::Completed;
        record.view.finished_at_ms = Some(2);
        run_store.save_run(&record)?;

        let service = RunService::new(
            run_store.clone(),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-1".to_string(), record.clone())]),
            BTreeMap::<String, SessionRunState>::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );
        service.append_run_event_once(&record.view, RunEvent::Completed)?;
        service.run_events("run-1")?;

        let completed_count = run_store
            .load_events("run-1")?
            .iter()
            .filter(|entry| matches!(entry.event, RunEvent::Completed))
            .count();
        assert_eq!(completed_count, 1);
        Ok(())
    }

    #[test]
    fn run_service_run_events_rejects_unknown_run() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::<String, RunRecord>::new(),
            BTreeMap::<String, SessionRunState>::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let error = service
            .run_events("missing")
            .expect_err("missing run should fail");
        assert!(error.to_string().contains("unknown run missing"));
        Ok(())
    }

    #[tokio::test]
    async fn run_service_can_start_next_queued_run() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        service
            .schedule_run(sample_run("run-1", "session-1"))
            .await?;
        service
            .schedule_run(sample_run("run-2", "session-1"))
            .await?;
        let started = service
            .finish_active_run("session-1", "run-1")
            .await?
            .started_run
            .expect("queued run should start");
        assert_eq!(started.run_id, "run-2");
        assert_eq!(started.status, DaemonRunStatus::Running);
        Ok(())
    }

    #[tokio::test]
    async fn run_service_finish_active_run_reports_idle_sessions() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        service
            .schedule_run(sample_run("run-1", "session-1"))
            .await?;

        let result = service.finish_active_run("session-1", "run-1").await?;
        assert_eq!(
            result,
            RunQueueAdvanceResult {
                started_run: None,
                finished_run_was_active: true,
                session_idle: true,
            }
        );
        assert_eq!(service.session_state("session-1").await, None);
        Ok(())
    }

    #[tokio::test]
    async fn run_service_schedule_does_not_leapfrog_existing_queue_without_active() -> Result<()> {
        let temp = tempdir()?;
        let mut queued = sample_run("run-queued", "session-1");
        queued.view.status = DaemonRunStatus::Queued;
        queued.view.queued_position = Some(1);
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-queued".to_string(), queued)]),
            BTreeMap::from([(
                "session-1".to_string(),
                SessionRunState {
                    active_run_id: None,
                    queued_run_ids: VecDeque::from(["run-queued".to_string()]),
                },
            )]),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let scheduled = service
            .schedule_run(sample_run("run-late", "session-1"))
            .await?;
        assert_eq!(scheduled.view.status, DaemonRunStatus::Queued);
        assert!(!scheduled.started_immediately);
        assert_eq!(
            service.session_state("session-1").await,
            Some(SessionRunState {
                active_run_id: None,
                queued_run_ids: VecDeque::from(["run-queued".to_string(), "run-late".to_string()]),
            })
        );

        let started = service
            .start_next_queued_run("session-1")
            .await?
            .expect("first queued run should start");
        assert_eq!(started.run_id, "run-queued");
        assert_eq!(started.status, DaemonRunStatus::Running);
        let late = service.get_run("run-late").await?;
        assert_eq!(late.status, DaemonRunStatus::Queued);
        assert_eq!(late.queued_position, Some(1));
        Ok(())
    }

    #[tokio::test]
    async fn run_service_idle_reservation_starts_before_later_queued_work() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        assert!(
            service
                .reserve_idle_submission_slot("session-1", "run-reserved")
                .await?
        );
        let late = service
            .schedule_run(sample_run("run-late", "session-1"))
            .await?;
        assert_eq!(late.view.status, DaemonRunStatus::Queued);
        assert!(!late.started_immediately);

        let reserved = service
            .schedule_run_requiring_idle(sample_run("run-reserved", "session-1"))
            .await?;
        assert_eq!(reserved.view.status, DaemonRunStatus::Running);
        assert!(reserved.started_immediately);
        assert_eq!(
            service.session_state("session-1").await,
            Some(SessionRunState {
                active_run_id: Some("run-reserved".to_string()),
                queued_run_ids: VecDeque::from(["run-late".to_string()]),
            })
        );
        let late = service.get_run("run-late").await?;
        assert_eq!(late.status, DaemonRunStatus::Queued);
        assert_eq!(late.queued_position, Some(1));
        Ok(())
    }

    #[tokio::test]
    async fn run_service_goal_reservation_starts_before_later_queued_work() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        assert!(
            service
                .reserve_goal_continuation_slot("session-1", "run-goal")
                .await?
        );
        let late = service
            .schedule_run(sample_run("run-late", "session-1"))
            .await?;
        assert_eq!(late.view.status, DaemonRunStatus::Queued);
        assert!(!late.started_immediately);

        let goal = service
            .schedule_run(sample_run("run-goal", "session-1"))
            .await?;
        assert_eq!(goal.view.status, DaemonRunStatus::Running);
        assert!(goal.started_immediately);
        assert_eq!(
            service.session_state("session-1").await,
            Some(SessionRunState {
                active_run_id: Some("run-goal".to_string()),
                queued_run_ids: VecDeque::from(["run-late".to_string()]),
            })
        );
        let late = service.get_run("run-late").await?;
        assert_eq!(late.status, DaemonRunStatus::Queued);
        assert_eq!(late.queued_position, Some(1));
        service
            .release_goal_continuation_slot("session-1", "run-goal")
            .await;
        Ok(())
    }

    #[tokio::test]
    async fn run_service_finish_active_run_skips_terminal_queued_runs() -> Result<()> {
        let temp = tempdir()?;
        let mut terminal = sample_run("run-2", "session-1");
        terminal.view.status = DaemonRunStatus::Cancelled;
        terminal.view.finished_at_ms = Some(2);
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([
                ("run-1".to_string(), sample_run("run-1", "session-1")),
                ("run-2".to_string(), terminal),
                ("run-3".to_string(), sample_run("run-3", "session-1")),
            ]),
            BTreeMap::from([(
                "session-1".to_string(),
                SessionRunState {
                    active_run_id: Some("run-1".to_string()),
                    queued_run_ids: VecDeque::from(["run-2".to_string(), "run-3".to_string()]),
                },
            )]),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let result = service.finish_active_run("session-1", "run-1").await?;
        assert_eq!(
            result.started_run.as_ref().map(|run| run.run_id.as_str()),
            Some("run-3")
        );
        assert!(!result.session_idle);
        assert_eq!(
            service.active_run_id("session-1").await.as_deref(),
            Some("run-3")
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_finish_active_run_is_noop_when_the_slot_is_already_released() -> Result<()>
    {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-1".to_string(), sample_run("run-1", "session-1"))]),
            BTreeMap::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let result = service.finish_active_run("session-1", "run-1").await?;
        assert_eq!(
            result,
            RunQueueAdvanceResult {
                started_run: None,
                finished_run_was_active: false,
                session_idle: true,
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_goal_continuation_reservation_blocks_idle_guard() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        assert!(
            service
                .reserve_goal_continuation_slot("session-1", "run-goal-1")
                .await?
        );
        assert!(
            !service
                .reserve_goal_continuation_slot("session-1", "run-goal-2")
                .await?
        );

        let guarded = service
            .with_session_idle_guard("session-1", || async { Ok(()) })
            .await;
        assert!(
            guarded
                .expect_err("pending goal continuation should block idle mutations")
                .to_string()
                .contains("has active or queued runs")
        );

        service
            .release_goal_continuation_slot("session-1", "run-goal-1")
            .await;
        service
            .with_session_idle_guard("session-1", || async { Ok(()) })
            .await?;

        assert!(
            service
                .reserve_idle_submission_slot("session-1", "run-input-1")
                .await?
        );
        assert!(
            !service
                .reserve_goal_continuation_slot("session-1", "run-goal-3")
                .await?
        );
        assert!(
            service
                .with_session_idle_guard("session-1", || async { Ok(()) })
                .await
                .expect_err("pending idle submission should block idle mutations")
                .to_string()
                .contains("has active or queued runs")
        );
        let should_promote = service
            .release_idle_submission_slot("session-1", "run-input-1")
            .await;
        assert!(!should_promote);
        assert!(
            service
                .reserve_goal_continuation_slot("session-1", "run-goal-4")
                .await?
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_resumes_waiting_approval_runs() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([(
                "run-1".to_string(),
                approval_waiting_run("run-1", "session-1"),
            )]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let resumed = service
            .resume_waiting_approval_run(
                "run-1",
                ResolveApprovalsRequest {
                    idempotency_key: None,
                    resolutions: vec![ApprovalResolution {
                        request_id: "approval-1".to_string(),
                        behavior: ApprovalResolutionBehavior::Allow,
                        updated_input: None,
                        justification: Some("approved".to_string()),
                        reason: None,
                    }],
                },
            )
            .await?;

        assert_eq!(resumed.status, DaemonRunStatus::Running);
        assert_eq!(resumed.kind, DaemonRunKind::ApprovalResume);
        assert!(resumed.pending_approval_ids.is_empty());
        let record = service.run_record("run-1").await?;
        match record.payload {
            RunRequestPayload::ApprovalResume {
                original_request,
                scheduled_origin,
                ..
            } => {
                assert!(original_request.is_some());
                assert!(scheduled_origin.is_none());
            }
            payload => panic!("unexpected payload after approval resume: {payload:?}"),
        }
        Ok(())
    }

    #[test]
    fn run_status_transition_errors_are_typed() {
        let mut record = sample_run("run-1", "session-1");
        record.view.status = DaemonRunStatus::Completed;

        let error = transition_run_status(&mut record.view, DaemonRunStatus::Running)
            .expect_err("terminal runs must not transition back to running");
        let problem = error
            .downcast_ref::<DaemonProblem>()
            .expect("invalid transition should be a typed daemon problem");

        assert_eq!(problem.status, 409);
        assert_eq!(problem.domain, "runs");
        assert_eq!(problem.code, "run_state_conflict");
        assert_eq!(record.view.status, DaemonRunStatus::Completed);

        let mut running = sample_run("run-2", "session-1");
        running.view.status = DaemonRunStatus::Running;
        let error = transition_run_status(&mut running.view, DaemonRunStatus::Queued)
            .expect_err("running to queued is recovery-only");
        assert_eq!(
            error
                .downcast_ref::<DaemonProblem>()
                .expect("invalid transition should be typed")
                .code,
            "run_state_conflict"
        );
        transition_run_status_for_restart_recovery(&mut running.view, DaemonRunStatus::Queued)
            .expect("restart recovery may requeue replayable running runs");
        assert_eq!(running.view.status, DaemonRunStatus::Queued);
    }

    #[test]
    fn approval_resolution_validation_errors_are_typed() {
        let pending = vec!["approval-1".to_string()];
        let empty =
            validate_approval_resolution_ids(&pending, &[]).expect_err("empty batch should fail");
        let duplicate = validate_approval_resolution_ids(
            &pending,
            &[
                ApprovalResolution {
                    request_id: "approval-1".to_string(),
                    behavior: ApprovalResolutionBehavior::Allow,
                    updated_input: None,
                    justification: Some("first".to_string()),
                    reason: None,
                },
                ApprovalResolution {
                    request_id: "approval-1".to_string(),
                    behavior: ApprovalResolutionBehavior::Deny,
                    updated_input: None,
                    justification: Some("second".to_string()),
                    reason: Some("duplicate".to_string()),
                },
            ],
        )
        .expect_err("duplicate batch should fail");
        let unknown = validate_approval_resolution_ids(
            &pending,
            &[ApprovalResolution {
                request_id: "approval-missing".to_string(),
                behavior: ApprovalResolutionBehavior::Allow,
                updated_input: None,
                justification: Some("missing".to_string()),
                reason: None,
            }],
        )
        .expect_err("unknown request should fail");

        for (error, code) in [
            (empty, "approval_batch_empty"),
            (duplicate, "approval_duplicate_resolution"),
            (unknown, "approval_request_not_pending"),
        ] {
            let problem = error
                .downcast_ref::<DaemonProblem>()
                .expect("approval validation should use typed daemon problems");
            assert_eq!(problem.status, 400);
            assert_eq!(problem.domain, "approvals");
            assert_eq!(problem.code, code);
        }
    }

    #[tokio::test]
    async fn run_service_resume_waiting_approval_rolls_back_when_audit_append_fails() -> Result<()>
    {
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let record = approval_waiting_run("run-1", "session-1");
        run_store.save_run(&record)?;
        let service = RunService::new(
            run_store.clone(),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-1".to_string(), record)]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );
        let request = || ResolveApprovalsRequest {
            idempotency_key: None,
            resolutions: vec![approval_resolution()],
        };

        let events_path = run_store.events_path("run-1");
        fs::create_dir_all(&events_path)?;
        let error = service
            .resume_waiting_approval_run("run-1", request())
            .await
            .expect_err("approval resume should fail closed when audit cannot be appended");
        assert!(
            error.to_string().contains("resume rolled back"),
            "unexpected error: {error:#}"
        );
        let current = service.run_record("run-1").await?;
        assert_eq!(current.view.status, DaemonRunStatus::WaitingForApproval);
        assert_eq!(
            current.view.pending_approval_ids,
            vec!["approval-1".to_string()]
        );
        let persisted = run_store
            .load_run("run-1")?
            .expect("rolled-back run should remain persisted");
        assert_eq!(persisted.view.status, DaemonRunStatus::WaitingForApproval);

        fs::remove_dir_all(&events_path)?;
        let resumed = service
            .resume_waiting_approval_run("run-1", request())
            .await?;
        assert_eq!(resumed.status, DaemonRunStatus::Running);
        assert!(resumed.pending_approval_ids.is_empty());

        let events = run_store
            .load_events("run-1")?
            .into_iter()
            .map(|entry| entry.event)
            .collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEvent::Started))
                .count(),
            1
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                RunEvent::ApprovalResolved { resolutions }
                    if resolutions.as_slice() == [approval_resolution()]
            )),
            "missing approval audit event after retry: {events:#?}"
        );
        Ok(())
    }

    #[test]
    fn run_service_resume_event_append_skips_already_reconciled_audit() -> Result<()> {
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let approval = approval_resolution();
        let mut record = approval_waiting_run("run-1", "session-1");
        record.view.status = DaemonRunStatus::Running;
        record.view.pending_approval_ids.clear();
        record.view.pending_approvals.clear();
        record.payload = RunRequestPayload::ApprovalResume {
            request: ResolveApprovalsRequest {
                idempotency_key: None,
                resolutions: vec![approval.clone()],
            },
            original_request: None,
            scheduled_origin: None,
            channel_delivery: None,
        };
        run_store.save_run(&record)?;
        let audit = RunEvent::ApprovalResolved {
            resolutions: vec![approval],
        };
        for event in [RunEvent::Started, audit.clone()] {
            run_store.append_event(&RunEventEntry {
                timestamp_ms: 1,
                run_id: record.view.run_id.clone(),
                session_id: record.view.session_id.clone(),
                agent_id: record.view.agent_id.clone(),
                event,
            })?;
        }
        let service = RunService::new(
            run_store.clone(),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-1".to_string(), record.clone())]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let _guard = service
            .event_append_lock
            .lock()
            .expect("run event append mutex poisoned");
        service.append_resume_events_locked(&record.view, Some(audit))?;

        let events = run_store
            .load_events("run-1")?
            .into_iter()
            .map(|entry| entry.event)
            .collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEvent::Started))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEvent::ApprovalResolved { .. }))
                .count(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_resume_waiting_run_race_errors_are_typed() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([(
                "run-1".to_string(),
                approval_waiting_run("run-1", "session-1"),
            )]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let error = service
            .resume_waiting_run(
                "run-1",
                DaemonRunStatus::WaitingForUserQuestion,
                true,
                |_| {
                    Ok((
                        DaemonRunKind::UserQuestionResume,
                        RunRequestPayload::UserQuestionResume {
                            request: ResolveUserQuestionRequest {
                                idempotency_key: None,
                                resolution: UserQuestionResolution {
                                    request_id: "question-1".to_string(),
                                    answers: Vec::new(),
                                    declined: true,
                                    justification: Some("race loser".to_string()),
                                },
                            },
                            original_request: None,
                            scheduled_origin: None,
                            channel_delivery: None,
                        },
                        None,
                    ))
                },
            )
            .await
            .expect_err("resume race should fail with a typed state problem");
        let problem = error
            .downcast_ref::<DaemonProblem>()
            .expect("resume race should be a typed daemon problem");

        assert_eq!(problem.status, 409);
        assert_eq!(problem.domain, "questions");
        assert_eq!(problem.code, "question_state_conflict");
        assert_eq!(
            service.run_record("run-1").await?.view.status,
            DaemonRunStatus::WaitingForApproval
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_rejects_unknown_approval_resolution_without_resuming() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([(
                "run-1".to_string(),
                approval_waiting_run("run-1", "session-1"),
            )]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let error = service
            .resume_waiting_approval_run(
                "run-1",
                ResolveApprovalsRequest {
                    idempotency_key: None,
                    resolutions: vec![ApprovalResolution {
                        request_id: "approval-missing".to_string(),
                        behavior: ApprovalResolutionBehavior::Allow,
                        updated_input: None,
                        justification: None,
                        reason: None,
                    }],
                },
            )
            .await
            .expect_err("unknown approval resolution should fail");

        assert_eq!(
            error.to_string(),
            "approval resolution references unknown pending request approval-missing"
        );
        let record = service.run_record("run-1").await?;
        assert_eq!(record.view.status, DaemonRunStatus::WaitingForApproval);
        assert_eq!(
            record.view.pending_approval_ids,
            vec!["approval-1".to_string()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_rejects_duplicate_approval_resolution_without_resuming() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([(
                "run-1".to_string(),
                approval_waiting_run("run-1", "session-1"),
            )]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let resolution = ApprovalResolution {
            request_id: "approval-1".to_string(),
            behavior: ApprovalResolutionBehavior::Allow,
            updated_input: None,
            justification: None,
            reason: None,
        };
        let error = service
            .resume_waiting_approval_run(
                "run-1",
                ResolveApprovalsRequest {
                    idempotency_key: None,
                    resolutions: vec![resolution.clone(), resolution],
                },
            )
            .await
            .expect_err("duplicate approval resolution should fail");

        assert_eq!(
            error.to_string(),
            "duplicate approval resolution for request approval-1"
        );
        let record = service.run_record("run-1").await?;
        assert_eq!(record.view.status, DaemonRunStatus::WaitingForApproval);
        assert_eq!(
            record.view.pending_approval_ids,
            vec!["approval-1".to_string()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_preserves_channel_lineage_when_resuming_approval_runs() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([(
                "run-1".to_string(),
                channel_approval_waiting_run("run-1", "session-1"),
            )]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let resumed = service
            .resume_waiting_approval_run(
                "run-1",
                ResolveApprovalsRequest {
                    idempotency_key: None,
                    resolutions: vec![approval_resolution()],
                },
            )
            .await?;

        assert_eq!(resumed.status, DaemonRunStatus::Running);
        assert_eq!(resumed.kind, DaemonRunKind::ChannelDelivery);
        let record = service.run_record("run-1").await?;
        assert!(record.is_channel_delivery_lineage());
        match record.payload {
            RunRequestPayload::ApprovalResume {
                channel_delivery, ..
            } => {
                let channel_delivery =
                    channel_delivery.expect("approval resume should retain channel request");
                assert_eq!(channel_delivery.channel_id, "channel-1");
                assert_eq!(channel_delivery.origin_message_id, "channel-message-2");
            }
            payload => panic!("unexpected payload after channel approval resume: {payload:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn run_service_updates_scheduled_indexes_after_terminal_transitions() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([(
                "run-1".to_string(),
                scheduled_run("run-1", "session-1", "schedule-1", 42),
            )]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        assert_eq!(
            service
                .scheduled_execution_snapshot("schedule-1")
                .run_ids_by_fire_at_ms,
            BTreeMap::from([(42, "run-1".to_string())])
        );

        service
            .mark_completed("run-1")
            .await?
            .expect("run should complete");

        assert!(
            service
                .scheduled_execution_snapshot("schedule-1")
                .run_ids_by_fire_at_ms
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_removes_scheduled_indexes_when_resume_payload_changes() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([(
                "run-1".to_string(),
                scheduled_waiting_approval_run("run-1", "session-1", "schedule-1", 42),
            )]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        assert_eq!(
            service
                .scheduled_execution_snapshot("schedule-1")
                .run_ids_by_fire_at_ms,
            BTreeMap::from([(42, "run-1".to_string())])
        );

        service
            .resume_waiting_approval_run(
                "run-1",
                ResolveApprovalsRequest {
                    idempotency_key: None,
                    resolutions: vec![approval_resolution()],
                },
            )
            .await?;

        assert_eq!(
            service
                .scheduled_execution_snapshot("schedule-1")
                .run_ids_by_fire_at_ms,
            BTreeMap::from([(42, "run-1".to_string())])
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_tracks_pending_mailbox_delivery_by_session() -> Result<()> {
        let temp = tempdir()?;
        let mut run = sample_run("run-1", "session-1");
        run.view.status = DaemonRunStatus::Running;
        run.view.started_at_ms = Some(1);
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-1".to_string(), run)]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        assert!(service.has_pending_mailbox_delivery("session-1"));

        service
            .mark_completed("run-1")
            .await?
            .expect("mailbox delivery should complete");

        assert!(!service.has_pending_mailbox_delivery("session-1"));
        Ok(())
    }

    #[tokio::test]
    async fn run_service_mailbox_dedupe_only_trusts_inflight_or_completed_deliveries() -> Result<()>
    {
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let message = MailboxMessage::new(
            "mailbox-run-test".to_string(),
            AgentId("agent-root".to_string()),
            AgentId("agent-1".to_string()),
            "handoff".to_string(),
            json!({"message": "resume"}),
            0,
            None,
        );

        let mut failed = sample_run("run-failed", "session-1");
        failed.view.status = DaemonRunStatus::Failed;
        failed.payload = RunRequestPayload::MailboxDelivery {
            agent_id: "agent-1".to_string(),
            messages: vec![message.clone()],
        };
        run_store.save_run(&failed)?;

        let mut queued = sample_run("run-queued", "session-1");
        queued.payload = RunRequestPayload::MailboxDelivery {
            agent_id: "agent-1".to_string(),
            messages: vec![message.clone()],
        };
        run_store.save_run(&queued)?;

        let mut completed = sample_run("run-completed", "session-1");
        completed.view.status = DaemonRunStatus::Completed;
        completed.payload = RunRequestPayload::MailboxDelivery {
            agent_id: "agent-1".to_string(),
            messages: vec![message.clone()],
        };
        run_store.save_run(&completed)?;

        let service = RunService::new(
            run_store,
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([
                ("run-failed".to_string(), failed),
                ("run-queued".to_string(), queued),
                ("run-completed".to_string(), completed),
            ]),
            BTreeMap::<String, SessionRunState>::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        assert!(
            service
                .session_has_mailbox_delivery_message(
                    "session-1",
                    "agent-1",
                    "agent-root",
                    "handoff",
                    &json!({"message": "resume"}),
                )
                .await?,
            "queued mailbox delivery should still count as in-flight for duplicate suppression"
        );
        assert_eq!(
            service
                .session_mailbox_delivery_prefix_len(
                    "session-1",
                    "agent-1",
                    std::slice::from_ref(&message)
                )
                .await?,
            1,
            "only completed mailbox deliveries should satisfy stale-prefix replay dedupe"
        );

        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        run_store.save_run(&RunRecord {
            view: RunView {
                run_id: "run-failed-only".to_string(),
                session_id: "session-1".to_string(),
                agent_id: "agent-1".to_string(),
                kind: DaemonRunKind::MailboxDelivery,
                status: DaemonRunStatus::Failed,
                submitted_at_ms: 1,
                updated_at_ms: 1,
                started_at_ms: Some(1),
                finished_at_ms: Some(2),
                queued_position: None,
                request: summarize_mailbox_request("agent-1", 1, Some("handoff")),
                input_attachments: Vec::new(),
                input_metadata: None,
                pending_approval_ids: Vec::new(),
                pending_approvals: Vec::new(),
                pending_question_ids: Vec::new(),
                pending_questions: Vec::new(),
                outputs: Vec::new(),
                deliveries: Vec::new(),
                error: Some("delivery failed".to_string()),
            },
            reply_targets: Vec::new(),
            payload: RunRequestPayload::MailboxDelivery {
                agent_id: "agent-1".to_string(),
                messages: vec![message.clone()],
            },
        })?;
        let service = RunService::new(
            run_store,
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::<String, RunRecord>::new(),
            BTreeMap::<String, SessionRunState>::new(),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        assert!(
            !service
                .session_has_mailbox_delivery_message(
                    "session-1",
                    "agent-1",
                    "agent-root",
                    "handoff",
                    &json!({"message": "resume"}),
                )
                .await?,
            "failed mailbox delivery should not suppress a retry"
        );
        assert_eq!(
            service
                .session_mailbox_delivery_prefix_len(
                    "session-1",
                    "agent-1",
                    std::slice::from_ref(&message)
                )
                .await?,
            0,
            "failed mailbox delivery should not satisfy stale-prefix replay dedupe"
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_resumes_waiting_user_question_runs() -> Result<()> {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([(
                "run-1".to_string(),
                question_waiting_run("run-1", "session-1"),
            )]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let resumed = service
            .resume_waiting_user_question_run(
                "run-1",
                ResolveUserQuestionRequest {
                    idempotency_key: None,
                    resolution: UserQuestionResolution {
                        request_id: "question-1".to_string(),
                        answers: vec![UserQuestionAnswer {
                            question_id: "question-a".to_string(),
                            selected_option_ids: Vec::new(),
                            freeform_answer: Some("provided".to_string()),
                        }],
                        declined: false,
                        justification: None,
                    },
                },
            )
            .await?;

        assert_eq!(resumed.status, DaemonRunStatus::Running);
        assert_eq!(resumed.kind, DaemonRunKind::UserQuestionResume);
        assert!(resumed.pending_question_ids.is_empty());
        assert!(resumed.pending_questions.is_empty());
        let record = service.run_record("run-1").await?;
        match record.payload {
            RunRequestPayload::UserQuestionResume {
                original_request,
                scheduled_origin,
                ..
            } => {
                assert!(original_request.is_some());
                assert!(scheduled_origin.is_none());
            }
            payload => panic!("unexpected payload after question resume: {payload:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn run_service_preserves_channel_lineage_when_resuming_user_question_runs() -> Result<()>
    {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([(
                "run-1".to_string(),
                channel_question_waiting_run("run-1", "session-1"),
            )]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let resumed = service
            .resume_waiting_user_question_run(
                "run-1",
                ResolveUserQuestionRequest {
                    idempotency_key: None,
                    resolution: UserQuestionResolution {
                        request_id: "question-1".to_string(),
                        answers: vec![UserQuestionAnswer {
                            question_id: "question-a".to_string(),
                            selected_option_ids: Vec::new(),
                            freeform_answer: Some("provided".to_string()),
                        }],
                        declined: false,
                        justification: None,
                    },
                },
            )
            .await?;

        assert_eq!(resumed.status, DaemonRunStatus::Running);
        assert_eq!(resumed.kind, DaemonRunKind::ChannelDelivery);
        let record = service.run_record("run-1").await?;
        assert!(record.is_channel_delivery_lineage());
        match record.payload {
            RunRequestPayload::UserQuestionResume {
                channel_delivery, ..
            } => {
                let channel_delivery =
                    channel_delivery.expect("question resume should retain channel request");
                assert_eq!(channel_delivery.channel_id, "channel-1");
                assert_eq!(channel_delivery.origin_message_id, "channel-message-2");
            }
            payload => panic!("unexpected payload after channel question resume: {payload:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn run_service_keeps_scheduled_kind_and_origin_when_resuming_scheduled_run() -> Result<()>
    {
        let temp = tempdir()?;
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([(
                "run-1".to_string(),
                scheduled_waiting_approval_run("run-1", "session-1", "schedule-1", 42),
            )]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let resumed = service
            .resume_waiting_approval_run(
                "run-1",
                ResolveApprovalsRequest {
                    idempotency_key: None,
                    resolutions: vec![approval_resolution()],
                },
            )
            .await?;

        assert_eq!(resumed.kind, DaemonRunKind::ScheduledInput);
        let record = service.run_record("run-1").await?;
        match record.payload {
            RunRequestPayload::ApprovalResume {
                scheduled_origin, ..
            } => assert_eq!(
                scheduled_origin,
                Some(ScheduledRunOrigin {
                    schedule_id: "schedule-1".to_string(),
                    fire_at_ms: 42,
                })
            ),
            payload => panic!("unexpected payload after scheduled approval resume: {payload:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn run_service_marks_parent_question_wait_and_completion() -> Result<()> {
        let temp = tempdir()?;
        let mut run = sample_run("run-1", "session-1");
        run.view.status = DaemonRunStatus::Running;
        run.view.started_at_ms = Some(1);
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-1".to_string(), run)]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let waiting = service
            .mark_waiting_for_user_question(
                "run-1",
                UserQuestionRequest {
                    id: "question-1".to_string(),
                    tool_call_id: "tool-call-1".to_string(),
                    questions: vec![UserQuestion {
                        id: "question-a".to_string(),
                        header: "Need input".to_string(),
                        question: "Need clarification".to_string(),
                        options: Vec::new(),
                        multi_select: false,
                    }],
                    created_at_ms: 1,
                    expires_at_ms: None,
                },
            )
            .await?
            .expect("run should move to waiting");
        assert_eq!(waiting.view.status, DaemonRunStatus::WaitingForUserQuestion);
        assert_eq!(
            waiting.view.pending_question_ids,
            vec!["question-1".to_string()]
        );

        let completed = service
            .mark_completed("run-1")
            .await?
            .expect("run should complete");
        assert_eq!(completed.view.status, DaemonRunStatus::Completed);
        assert!(completed.view.pending_question_ids.is_empty());
        assert!(completed.view.pending_questions.is_empty());
        assert!(completed.view.finished_at_ms.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn run_service_apply_snapshot_marks_completed_runs_terminal() -> Result<()> {
        let temp = tempdir()?;
        let mut run = sample_run("run-1", "session-1");
        run.view.status = DaemonRunStatus::Running;
        run.view.started_at_ms = Some(1);
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-1".to_string(), run)]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let update = service
            .apply_snapshot(
                "run-1",
                &sample_snapshot("session-1", Vec::new(), Vec::new()),
            )
            .await?
            .expect("run should still be mutable");

        assert_eq!(update.next_session_id.as_deref(), Some("session-1"));
        assert_eq!(update.record.view.status, DaemonRunStatus::Completed);
        assert!(update.record.view.pending_approval_ids.is_empty());
        assert!(update.record.view.pending_question_ids.is_empty());
        assert!(update.record.view.pending_questions.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn run_service_mark_failed_clears_pending_state() -> Result<()> {
        let temp = tempdir()?;
        let mut run = question_waiting_run("run-1", "session-1");
        run.view.pending_approval_ids = vec!["approval-1".to_string()];
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-1".to_string(), run)]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let record = service
            .mark_failed("run-1", "boom")
            .await?
            .expect("run should fail");

        assert_eq!(record.view.status, DaemonRunStatus::Failed);
        assert_eq!(record.view.error.as_deref(), Some("boom"));
        assert!(record.view.pending_approval_ids.is_empty());
        assert!(record.view.pending_question_ids.is_empty());
        assert!(record.view.pending_questions.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn run_service_recover_running_mailbox_runs_requeues_and_reindexes() -> Result<()> {
        let temp = tempdir()?;
        let mut run = sample_run("run-1", "session-1");
        run.view.status = DaemonRunStatus::Running;
        run.view.started_at_ms = Some(1);
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-1".to_string(), run)]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        service.recover_running_runs(&BTreeMap::new()).await?;

        let recovered = service.get_run("run-1").await?;
        assert_eq!(recovered.status, DaemonRunStatus::Queued);
        assert_eq!(
            service.session_state("session-1").await,
            Some(SessionRunState {
                active_run_id: None,
                queued_run_ids: VecDeque::from(["run-1".to_string()]),
            })
        );
        assert!(service.has_pending_mailbox_delivery("session-1"));
        Ok(())
    }

    #[tokio::test]
    async fn run_service_recover_running_input_runs_interrupts_without_snapshot() -> Result<()> {
        let temp = tempdir()?;
        let mut run = sample_run("run-1", "session-1");
        run.view.status = DaemonRunStatus::Running;
        run.view.kind = DaemonRunKind::Input;
        run.payload = RunRequestPayload::Input {
            request: sample_input_request(),
            idempotency: None,
        };
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-1".to_string(), run)]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        service.recover_running_runs(&BTreeMap::new()).await?;

        let recovered = service.get_run("run-1").await?;
        assert_eq!(recovered.status, DaemonRunStatus::Interrupted);
        assert_eq!(
            recovered.error.as_deref(),
            Some("daemon restarted while the run was active")
        );
        assert_eq!(service.session_state("session-1").await, None);
        Ok(())
    }

    #[tokio::test]
    async fn run_service_recover_running_resumed_scheduled_runs_requeues_them() -> Result<()> {
        let temp = tempdir()?;
        let mut run = sample_run("run-1", "session-1");
        run.view.status = DaemonRunStatus::Running;
        run.view.kind = DaemonRunKind::ScheduledInput;
        run.payload = RunRequestPayload::ApprovalResume {
            request: ResolveApprovalsRequest {
                idempotency_key: None,
                resolutions: vec![approval_resolution()],
            },
            original_request: Some(sample_input_request()),
            scheduled_origin: Some(ScheduledRunOrigin {
                schedule_id: "schedule-1".to_string(),
                fire_at_ms: 42,
            }),
            channel_delivery: None,
        };
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-1".to_string(), run)]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        service.recover_running_runs(&BTreeMap::new()).await?;

        let recovered = service.get_run("run-1").await?;
        assert_eq!(recovered.status, DaemonRunStatus::Queued);
        assert_eq!(recovered.kind, DaemonRunKind::ScheduledInput);
        assert_eq!(
            service.session_state("session-1").await,
            Some(SessionRunState {
                active_run_id: None,
                queued_run_ids: VecDeque::from(["run-1".to_string()]),
            })
        );
        assert_eq!(
            service
                .scheduled_execution_snapshot("schedule-1")
                .run_ids_by_fire_at_ms,
            BTreeMap::from([(42, "run-1".to_string())])
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_recover_running_runs_restores_pending_questions_from_snapshot()
    -> Result<()> {
        let temp = tempdir()?;
        let mut run = sample_run("run-1", "session-1");
        run.view.status = DaemonRunStatus::Running;
        run.view.kind = DaemonRunKind::Input;
        run.payload = RunRequestPayload::Input {
            request: sample_input_request(),
            idempotency: None,
        };
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([("run-1".to_string(), run)]),
            active_session_runs("session-1", "run-1"),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        service
            .recover_running_runs(&BTreeMap::from([(
                "agent-1".to_string(),
                sample_snapshot(
                    "session-1",
                    Vec::new(),
                    vec![UserQuestionRequest {
                        id: "question-1".to_string(),
                        tool_call_id: "tool-call-1".to_string(),
                        questions: vec![UserQuestion {
                            id: "focus".to_string(),
                            header: "Focus".to_string(),
                            question: "Need one focus".to_string(),
                            options: Vec::new(),
                            multi_select: false,
                        }],
                        created_at_ms: 1,
                        expires_at_ms: None,
                    }],
                ),
            )]))
            .await?;

        let recovered = service.get_run("run-1").await?;
        assert_eq!(recovered.status, DaemonRunStatus::WaitingForUserQuestion);
        assert_eq!(
            recovered.pending_question_ids,
            vec!["question-1".to_string()]
        );
        assert_eq!(
            service.require_active_run_id("session-1").await?,
            "run-1".to_string()
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_recovery_interrupts_extra_non_replayable_waiting_runs() -> Result<()> {
        let temp = tempdir()?;
        let first = approval_waiting_run("run-1", "session-1");
        let mut second = question_waiting_run("run-2", "session-1");
        second.view.submitted_at_ms = first.view.submitted_at_ms;
        let runs = BTreeMap::from([("run-1".to_string(), first), ("run-2".to_string(), second)]);
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            runs.clone(),
            rebuild_session_run_state(&runs),
            rebuild_pending_question_index(&runs),
            AtomicU64::new(2),
        );

        service.recover_running_runs(&BTreeMap::new()).await?;

        let active = service.get_run("run-1").await?;
        assert_eq!(active.status, DaemonRunStatus::WaitingForApproval);
        assert_eq!(active.pending_approval_ids, vec!["approval-1".to_string()]);

        let interrupted = service.get_run("run-2").await?;
        assert_eq!(interrupted.status, DaemonRunStatus::Interrupted);
        assert_eq!(interrupted.queued_position, None);
        assert!(interrupted.pending_question_ids.is_empty());
        assert!(interrupted.pending_questions.is_empty());
        assert_eq!(
            service.session_state("session-1").await,
            Some(SessionRunState {
                active_run_id: Some("run-1".to_string()),
                queued_run_ids: VecDeque::new(),
            })
        );
        assert!(service.list_pending_questions(None).is_empty());
        assert!(
            service
                .run_events("run-2")?
                .iter()
                .any(|entry| matches!(entry.event, RunEvent::Interrupted))
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_recovery_requeues_extra_replayable_waiting_runs() -> Result<()> {
        let temp = tempdir()?;
        let first = approval_waiting_run("run-1", "session-1");
        let mut second = scheduled_waiting_approval_run("run-2", "session-1", "schedule-1", 1_000);
        second.view.submitted_at_ms = first.view.submitted_at_ms;
        let runs = BTreeMap::from([("run-1".to_string(), first), ("run-2".to_string(), second)]);
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            runs.clone(),
            rebuild_session_run_state(&runs),
            rebuild_pending_question_index(&runs),
            AtomicU64::new(2),
        );

        service.recover_running_runs(&BTreeMap::new()).await?;

        let queued = service.get_run("run-2").await?;
        assert_eq!(queued.status, DaemonRunStatus::Queued);
        assert_eq!(queued.queued_position, Some(1));
        assert!(queued.pending_approval_ids.is_empty());
        assert_eq!(
            service.session_state("session-1").await,
            Some(SessionRunState {
                active_run_id: Some("run-1".to_string()),
                queued_run_ids: VecDeque::from(["run-2".to_string()]),
            })
        );
        assert!(
            service
                .run_events("run-2")?
                .iter()
                .any(|entry| matches!(entry.event, RunEvent::Queued { position: 1 }))
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_pending_question_index_keeps_same_request_id_per_run() -> Result<()> {
        let temp = tempdir()?;
        let first = question_waiting_run("run-1", "session-1");
        let second = question_waiting_run("run-2", "session-2");
        let runs = BTreeMap::from([("run-1".to_string(), first), ("run-2".to_string(), second)]);
        let pending_questions = rebuild_pending_question_index(&runs);
        assert_eq!(pending_questions.len(), 2);
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            runs,
            BTreeMap::from([
                (
                    "session-1".to_string(),
                    SessionRunState {
                        active_run_id: Some("run-1".to_string()),
                        queued_run_ids: VecDeque::new(),
                    },
                ),
                (
                    "session-2".to_string(),
                    SessionRunState {
                        active_run_id: Some("run-2".to_string()),
                        queued_run_ids: VecDeque::new(),
                    },
                ),
            ]),
            pending_questions,
            AtomicU64::new(0),
        );

        let all_questions = service.list_pending_questions(None);
        assert_eq!(all_questions.len(), 2);
        assert_eq!(service.list_pending_questions(Some("session-1")).len(), 1);
        assert_eq!(service.list_pending_questions(Some("session-2")).len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn run_service_cancel_run_removes_queued_work_and_clears_pending_questions() -> Result<()>
    {
        let temp = tempdir()?;
        let queued = question_waiting_run("run-2", "session-1");
        let service = RunService::new(
            FileRunStore::new(temp.path()),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([
                ("run-1".to_string(), sample_run("run-1", "session-1")),
                ("run-2".to_string(), queued),
            ]),
            BTreeMap::from([(
                "session-1".to_string(),
                SessionRunState {
                    active_run_id: Some("run-1".to_string()),
                    queued_run_ids: VecDeque::from(["run-2".to_string()]),
                },
            )]),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let RunCancellationResult { record, was_active } = service
            .cancel_run("run-2")
            .await?
            .expect("queued run should be cancelled");

        assert!(!was_active);
        assert_eq!(record.view.status, DaemonRunStatus::Cancelled);
        assert!(record.view.pending_approval_ids.is_empty());
        assert!(record.view.pending_question_ids.is_empty());
        assert!(record.view.pending_questions.is_empty());
        assert!(
            service
                .session_state("session-1")
                .await
                .expect("session state")
                .queued_run_ids
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_service_rolls_back_batch_updates_when_later_save_fails() -> Result<()> {
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let mut active = sample_run("run-1", "session-1");
        active.view.status = DaemonRunStatus::Running;
        active.view.started_at_ms = Some(1);
        active.view.queued_position = None;
        let mut queued = sample_run("run-2", "session-1");
        queued.view.queued_position = Some(1);
        let mut trailing = sample_run("run-3", "session-1");
        trailing.view.queued_position = Some(2);
        run_store.save_run(&active)?;
        run_store.save_run(&queued)?;
        run_store.save_run(&trailing)?;
        let sabotaged_path = run_store.run_path("run-3");
        fs::remove_file(&sabotaged_path)?;
        fs::create_dir_all(&sabotaged_path)?;
        let service = RunService::new(
            run_store.clone(),
            FileRunMemoryStore::new(temp.path()),
            FileDebugStore::new(temp.path()),
            DaemonEventBus::new(16),
            BTreeMap::from([
                ("run-1".to_string(), active.clone()),
                ("run-2".to_string(), queued.clone()),
                ("run-3".to_string(), trailing.clone()),
            ]),
            BTreeMap::from([(
                "session-1".to_string(),
                SessionRunState {
                    active_run_id: Some("run-1".to_string()),
                    queued_run_ids: VecDeque::from(["run-2".to_string(), "run-3".to_string()]),
                },
            )]),
            BTreeMap::<String, PendingQuestionView>::new(),
            AtomicU64::new(0),
        );

        let error = service
            .cancel_run("run-2")
            .await
            .expect_err("queue persistence should fail");
        assert!(
            error.to_string().contains("failed to write"),
            "unexpected error: {error}"
        );
        assert_eq!(
            service.get_run("run-2").await?.status,
            DaemonRunStatus::Queued
        );
        assert_eq!(service.get_run("run-2").await?.queued_position, Some(1));
        assert_eq!(service.get_run("run-3").await?.queued_position, Some(2));
        assert_eq!(
            service
                .session_state("session-1")
                .await
                .expect("session state"),
            SessionRunState {
                active_run_id: Some("run-1".to_string()),
                queued_run_ids: VecDeque::from(["run-2".to_string(), "run-3".to_string()]),
            }
        );
        assert_eq!(
            run_store
                .load_run("run-2")?
                .expect("persisted queued run")
                .view
                .status,
            DaemonRunStatus::Queued
        );
        Ok(())
    }

    #[test]
    fn rollback_persisted_run_batch_removes_newly_created_run_files() -> Result<()> {
        let temp = tempdir()?;
        let run_store = FileRunStore::new(temp.path());
        let created = sample_run("run-created", "session-1");
        run_store.save_run(&created)?;

        rollback_persisted_run_batch(
            &run_store,
            &BTreeMap::from([("run-created".to_string(), None)]),
            &[String::from("run-created")],
        );

        assert!(run_store.load_run("run-created")?.is_none());
        Ok(())
    }
}
