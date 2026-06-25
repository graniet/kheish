//! Daemon event transport and observer integration.

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::fs::{self, OpenOptions};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::services::ExternalActionService;
use crate::{
    DaemonEventStatusView, DaemonOutputRecord, FileDebugStore, RunView, RuntimeSettingsView,
};
use anyhow::{Context as _, anyhow};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::StreamExt;
use kheish_agent::ManagedAgentSnapshot;
use kheish_runtime::{
    DebugArtifact, DebugCaptureLevel, DebugControl, MetricsSnapshot, RuntimeObserver, TraceEvent,
    TraceEventKind, current_execution_scope,
};
use kheish_types::SessionGoal;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tracing::{error, warn};

const EVENT_ID_EPOCH_SHIFT: u32 = 32;
const EVENT_IDS_PER_EPOCH: u64 = 1_u64 << EVENT_ID_EPOCH_SHIFT;
const MAX_EVENT_HISTORY_CAPACITY: usize = 262_144;
const SCOPE_EVICTION_RETENTION_MULTIPLIER: u64 = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamGapReason {
    #[default]
    ReplayWindow,
    RestartEpoch,
    LiveLag,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamGapScope {
    #[default]
    Global,
    Session,
    Run,
}

/// One event emitted by the daemon event bus.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonEvent {
    Trace {
        trace: TraceEvent,
    },
    SessionStateChanged {
        session_id: String,
        agent_id: String,
        status: kheish_agent::AgentStatus,
        pending_approvals: usize,
        #[serde(default)]
        pending_questions: usize,
    },
    SessionSnapshot {
        session_id: String,
        snapshot: ManagedAgentSnapshot,
    },
    Output {
        output: DaemonOutputRecord,
    },
    RunUpdated {
        run: RunView,
    },
    SessionGoalUpdated {
        session_id: String,
        goal: Option<SessionGoal>,
    },
    Interrupted {
        session_id: String,
        agent_id: String,
    },
    RuntimeUpdated {
        runtime: RuntimeSettingsView,
    },
    Heartbeat,
    StreamGap {
        skipped: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_after_id: Option<String>,
        #[serde(default)]
        reason: StreamGapReason,
        #[serde(default)]
        scope: StreamGapScope,
        #[serde(default)]
        skipped_is_estimate: bool,
    },
}

impl DaemonEvent {
    pub(crate) fn session_id(&self) -> Option<&str> {
        match self {
            Self::Trace { trace } => trace.session_id.as_deref(),
            Self::SessionStateChanged { session_id, .. } => Some(session_id.as_str()),
            Self::SessionSnapshot { session_id, .. } => Some(session_id.as_str()),
            Self::Output { output } => Some(output.session_id.as_str()),
            Self::RunUpdated { run } => Some(run.session_id.as_str()),
            Self::SessionGoalUpdated { session_id, .. } => Some(session_id.as_str()),
            Self::Interrupted { session_id, .. } => Some(session_id.as_str()),
            Self::RuntimeUpdated { .. } | Self::Heartbeat | Self::StreamGap { .. } => None,
        }
    }

    pub(crate) fn run_id(&self) -> Option<&str> {
        match self {
            Self::Trace { trace } => trace.run_id.as_deref(),
            Self::Output { output } => output.run_id.as_deref(),
            Self::RunUpdated { run } => Some(run.run_id.as_str()),
            _ => None,
        }
    }

    pub(crate) fn event_name(&self) -> &'static str {
        match self {
            Self::Trace { .. } => "trace",
            Self::SessionStateChanged { .. } => "session_state_changed",
            Self::SessionSnapshot { .. } => "session_snapshot",
            Self::Output { .. } => "output",
            Self::RunUpdated { .. } => "run_updated",
            Self::SessionGoalUpdated { .. } => "session_goal_updated",
            Self::Interrupted { .. } => "interrupted",
            Self::RuntimeUpdated { .. } => "runtime_updated",
            Self::Heartbeat => "heartbeat",
            Self::StreamGap { .. } => "stream_gap",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonEventEnvelope {
    pub id: u64,
    pub event: DaemonEvent,
}

#[derive(Clone)]
pub(crate) struct DaemonEventBus {
    sender: broadcast::Sender<DaemonEventEnvelope>,
    state: Arc<StdMutex<DaemonEventBusState>>,
    history_capacity: usize,
    start_id: u64,
    next_epoch_start_id: u64,
}

#[derive(Debug)]
struct DaemonEventBusState {
    history: VecDeque<DaemonEventEnvelope>,
    next_id: u64,
    last_evicted_by_session: BTreeMap<String, u64>,
    last_evicted_by_run: BTreeMap<String, u64>,
    scope_eviction_floor_id: u64,
    evicted_event_count: u64,
    replay_gap_count: u64,
    stream_lagged_event_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScopeEvictionStatus {
    KnownLoss { resume_after_id: u64 },
    MetadataGap { resume_after_id: u64 },
}

impl ScopeEvictionStatus {
    fn resume_after_id(self) -> u64 {
        match self {
            Self::KnownLoss { resume_after_id } | Self::MetadataGap { resume_after_id } => {
                resume_after_id
            }
        }
    }
}

impl DaemonEventBus {
    #[cfg(test)]
    pub(crate) fn new(buffer: usize) -> Self {
        Self::new_with_start_id(buffer, 1)
    }

    pub(crate) fn new_with_persistent_epoch(
        buffer: usize,
        epoch_path: &Path,
    ) -> anyhow::Result<Self> {
        let now_epoch = current_event_epoch();
        let previous_epoch = match fs::read_to_string(epoch_path) {
            Ok(contents) => match contents.trim().parse::<u64>() {
                Ok(epoch) => epoch,
                Err(error) => {
                    warn!(
                        error = ?error,
                        path = %epoch_path.display(),
                        "daemon SSE event epoch is corrupt; reseeding from wall clock"
                    );
                    0
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to read daemon SSE event epoch at {}",
                        epoch_path.display()
                    )
                });
            }
        };
        let epoch = now_epoch.max(previous_epoch.saturating_add(1)).max(1);
        let max_epoch = u64::MAX >> EVENT_ID_EPOCH_SHIFT;
        if epoch > max_epoch {
            return Err(anyhow!(
                "daemon SSE event epoch {epoch} exceeds maximum supported epoch {max_epoch}"
            ));
        }
        if let Some(parent) = epoch_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create daemon SSE event epoch dir {}",
                    parent.display()
                )
            })?;
        }
        persist_event_epoch(epoch_path, epoch)?;
        let start_id = epoch.checked_shl(EVENT_ID_EPOCH_SHIFT).ok_or_else(|| {
            anyhow!("daemon SSE event epoch {epoch} is too large for u64 event ids")
        })?;
        Ok(Self::new_with_start_id(buffer, start_id))
    }

    fn new_with_start_id(buffer: usize, start_id: u64) -> Self {
        let history_capacity = normalize_event_history_capacity(buffer);
        let (sender, _) = broadcast::channel(history_capacity);
        Self {
            sender,
            state: Arc::new(StdMutex::new(DaemonEventBusState {
                history: VecDeque::with_capacity(history_capacity),
                next_id: start_id,
                last_evicted_by_session: BTreeMap::new(),
                last_evicted_by_run: BTreeMap::new(),
                scope_eviction_floor_id: start_id.saturating_sub(1),
                evicted_event_count: 0,
                replay_gap_count: 0,
                stream_lagged_event_count: 0,
            })),
            history_capacity,
            start_id,
            next_epoch_start_id: start_id.saturating_add(EVENT_IDS_PER_EPOCH),
        }
    }

    pub(crate) fn publish(&self, event: DaemonEvent) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.next_id >= self.next_epoch_start_id {
            error!(
                next_id = state.next_id,
                start_id = self.start_id,
                next_epoch_start_id = self.next_epoch_start_id,
                "daemon SSE event id range for this epoch is exhausted; dropping event to avoid non-monotone ids"
            );
            return;
        }
        let envelope = DaemonEventEnvelope {
            id: state.next_id,
            event,
        };
        state.next_id = state.next_id.saturating_add(1);
        if state.history.len() >= self.history_capacity {
            state.record_eviction(self.scope_eviction_retention());
        }
        state.history.push_back(envelope.clone());
        let _ = self.sender.send(envelope);
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<DaemonEventEnvelope> {
        self.sender.subscribe()
    }

    pub(crate) fn subscribe_after(&self, after_id: Option<u64>) -> DaemonEventSubscription {
        let receiver = self.subscribe();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let oldest_history_id = state.history.front().map(|envelope| envelope.id);
        let newest_history_id = state.history.back().map(|envelope| envelope.id);
        let max_visible_id = newest_history_id.unwrap_or_else(|| self.start_id.saturating_sub(1));
        let effective_after_id = after_id.map(|after_id| after_id.min(max_visible_id));
        let replay = state
            .history
            .iter()
            .filter(|envelope| effective_after_id.is_some_and(|after_id| envelope.id > after_id))
            .cloned()
            .collect::<Vec<_>>();
        let history_was_truncated = state.history.len() >= self.history_capacity;
        let start_floor = self.start_id.saturating_sub(1);
        let stream_gap = match (after_id, effective_after_id, oldest_history_id) {
            (Some(requested_after), _, _)
                if requested_after > 0 && requested_after < start_floor =>
            {
                Some(StreamGapInfo {
                    skipped: start_floor.saturating_sub(requested_after).max(1),
                    resume_after_id: Some(start_floor),
                    emit_without_matching_replay: true,
                    reason: StreamGapReason::RestartEpoch,
                    skipped_is_estimate: true,
                })
            }
            (_, Some(after_id), Some(oldest_id)) if history_was_truncated => {
                let effective_after = after_id.max(start_floor);
                (effective_after.saturating_add(1) < oldest_id)
                    .then(|| oldest_id.saturating_sub(effective_after).saturating_sub(1))
                    .filter(|skipped| *skipped > 0)
                    .map(|skipped| StreamGapInfo {
                        skipped,
                        resume_after_id: Some(oldest_id.saturating_sub(1)),
                        emit_without_matching_replay: false,
                        reason: StreamGapReason::ReplayWindow,
                        skipped_is_estimate: false,
                    })
            }
            _ => None,
        };
        if stream_gap.is_some() {
            state.replay_gap_count = state.replay_gap_count.saturating_add(1);
        }
        DaemonEventSubscription {
            receiver,
            replay,
            state: self.state.clone(),
            effective_after_id,
            live_after_id: newest_history_id.or(effective_after_id),
            stream_gap,
        }
    }

    fn scope_eviction_retention(&self) -> u64 {
        (self.history_capacity as u64)
            .saturating_mul(SCOPE_EVICTION_RETENTION_MULTIPLIER)
            .max(1)
    }

    pub(crate) fn status_snapshot(&self) -> DaemonEventStatusView {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let retained_event_count = state.history.len();
        let replay_buffer_utilization_percent = if self.history_capacity == 0 {
            0
        } else {
            ((retained_event_count.saturating_mul(100)) / self.history_capacity).min(100) as u8
        };
        DaemonEventStatusView {
            history_capacity: self.history_capacity,
            retained_event_count,
            subscriber_count: self.sender.receiver_count(),
            oldest_event_id: state.history.front().map(|envelope| envelope.id),
            newest_event_id: state.history.back().map(|envelope| envelope.id),
            next_event_id: state.next_id,
            tail_event_id_cursor: Some(
                state
                    .history
                    .back()
                    .map(|envelope| envelope.id)
                    .unwrap_or_else(|| self.start_id.saturating_sub(1))
                    .to_string(),
            ),
            replay_buffer_utilization_percent,
            evicted_event_count: state.evicted_event_count,
            replay_gap_count: state.replay_gap_count,
            stream_lagged_event_count: state.stream_lagged_event_count,
            scope_eviction_floor_id: state.scope_eviction_floor_id,
            evicted_session_scope_count: state.last_evicted_by_session.len(),
            evicted_run_scope_count: state.last_evicted_by_run.len(),
        }
    }
}

fn normalize_event_history_capacity(buffer: usize) -> usize {
    let normalized = buffer.clamp(1, MAX_EVENT_HISTORY_CAPACITY);
    if normalized != buffer {
        warn!(
            requested_capacity = buffer,
            applied_capacity = normalized,
            "normalized daemon SSE event history capacity"
        );
    }
    normalized
}

impl DaemonEventBusState {
    fn record_eviction(&mut self, scope_eviction_retention: u64) {
        let Some(envelope) = self.history.pop_front() else {
            return;
        };
        self.evicted_event_count = self.evicted_event_count.saturating_add(1);
        if let Some(session_id) = envelope.event.session_id() {
            self.last_evicted_by_session
                .insert(session_id.to_string(), envelope.id);
        }
        if let Some(run_id) = envelope.event.run_id() {
            self.last_evicted_by_run
                .insert(run_id.to_string(), envelope.id);
        }
        self.prune_scope_evictions(envelope.id.saturating_sub(scope_eviction_retention));
    }

    fn prune_scope_evictions(&mut self, floor_id: u64) {
        if floor_id <= self.scope_eviction_floor_id {
            return;
        }
        self.scope_eviction_floor_id = floor_id;
        self.last_evicted_by_session
            .retain(|_, evicted_id| *evicted_id > floor_id);
        self.last_evicted_by_run
            .retain(|_, evicted_id| *evicted_id > floor_id);
    }

    fn scope_eviction_after(
        &self,
        session_filter: Option<&str>,
        run_filter: Option<&str>,
        after_id: u64,
    ) -> Option<ScopeEvictionStatus> {
        if let Some(run_id) = run_filter {
            if let Some(id) = self.last_evicted_by_run.get(run_id)
                && *id > after_id
            {
                return Some(ScopeEvictionStatus::KnownLoss {
                    resume_after_id: *id,
                });
            }
            return (after_id < self.scope_eviction_floor_id).then_some(
                ScopeEvictionStatus::MetadataGap {
                    resume_after_id: self.scope_eviction_floor_id,
                },
            );
        }
        if let Some(session_id) = session_filter {
            if let Some(id) = self.last_evicted_by_session.get(session_id)
                && *id > after_id
            {
                return Some(ScopeEvictionStatus::KnownLoss {
                    resume_after_id: *id,
                });
            }
            return (after_id < self.scope_eviction_floor_id).then_some(
                ScopeEvictionStatus::MetadataGap {
                    resume_after_id: self.scope_eviction_floor_id,
                },
            );
        }
        None
    }

    fn record_stream_lag(&mut self, skipped: u64) {
        self.stream_lagged_event_count = self.stream_lagged_event_count.saturating_add(skipped);
    }
}

fn current_event_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn persist_event_epoch(epoch_path: &Path, epoch: u64) -> anyhow::Result<()> {
    let tmp_path = epoch_path.with_extension("tmp");
    fs::write(&tmp_path, format!("{epoch}\n")).with_context(|| {
        format!(
            "failed to persist daemon SSE event epoch temp file at {}",
            tmp_path.display()
        )
    })?;
    OpenOptions::new()
        .read(true)
        .open(&tmp_path)
        .and_then(|file| file.sync_all())
        .with_context(|| {
            format!(
                "failed to sync daemon SSE event epoch temp file at {}",
                tmp_path.display()
            )
        })?;
    fs::rename(&tmp_path, epoch_path).with_context(|| {
        format!(
            "failed to atomically replace daemon SSE event epoch at {}",
            epoch_path.display()
        )
    })?;
    if let Some(parent) = epoch_path.parent() {
        OpenOptions::new()
            .read(true)
            .open(parent)
            .and_then(|dir| dir.sync_all())
            .with_context(|| {
                format!(
                    "failed to sync daemon SSE event epoch dir {}",
                    parent.display()
                )
            })?;
    }
    Ok(())
}

pub(crate) struct DaemonEventSubscription {
    receiver: broadcast::Receiver<DaemonEventEnvelope>,
    replay: Vec<DaemonEventEnvelope>,
    state: Arc<StdMutex<DaemonEventBusState>>,
    effective_after_id: Option<u64>,
    live_after_id: Option<u64>,
    stream_gap: Option<StreamGapInfo>,
}

pub(crate) struct StreamGapInfo {
    skipped: u64,
    resume_after_id: Option<u64>,
    emit_without_matching_replay: bool,
    reason: StreamGapReason,
    skipped_is_estimate: bool,
}

pub(crate) struct DaemonObserver {
    events: DaemonEventBus,
    debug: DebugControl,
    debug_store: FileDebugStore,
    external_actions: Option<ExternalActionService>,
    external_action_failure: StdMutex<Option<String>>,
    counters: StdMutex<BTreeMap<String, u64>>,
}

impl DaemonObserver {
    pub(crate) fn shared(
        events: DaemonEventBus,
        debug: DebugControl,
        debug_store: FileDebugStore,
    ) -> Arc<Self> {
        let external_actions = match ExternalActionService::new(debug_store.root()) {
            Ok(service) => Some(service),
            Err(error) => {
                error!(
                    error = ?error,
                    "external action audit failed to initialize; external actions fail closed"
                );
                None
            }
        };
        let external_action_failure = StdMutex::new(
            external_actions
                .is_none()
                .then_some("external action audit failed to initialize".to_string()),
        );
        Arc::new(Self {
            events,
            debug,
            external_actions,
            debug_store,
            external_action_failure,
            counters: StdMutex::new(BTreeMap::new()),
        })
    }

    fn lock_external_action_failure(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.external_action_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_counters(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, u64>> {
        self.counters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl RuntimeObserver for DaemonObserver {
    fn debug_level(&self) -> DebugCaptureLevel {
        let run_id = current_execution_scope().and_then(|scope| scope.run_id);
        self.debug.level_for_run(run_id.as_deref())
    }

    fn record(&self, event: TraceEvent) {
        if matches!(event.kind, TraceEventKind::ExternalAction { .. }) {
            if let Some(external_actions) = &self.external_actions {
                if let Err(error) =
                    append_external_action_without_blocking_core(external_actions, &event)
                {
                    let message = error.to_string();
                    let mut failure = self.lock_external_action_failure();
                    if failure.is_none() {
                        error!(
                            error = ?error,
                            "external action audit became unavailable; future external actions fail closed"
                        );
                        *failure = Some(message);
                    }
                }
            } else {
                let mut failure = self.lock_external_action_failure();
                if failure.is_none() {
                    *failure = Some("external action audit is unavailable".to_string());
                }
            }
        }
        self.events.publish(DaemonEvent::Trace { trace: event });
    }

    fn external_action_audit_failure(&self) -> Option<String> {
        self.lock_external_action_failure().clone()
    }

    fn record_debug_artifact(&self, artifact: DebugArtifact) {
        if let Err(error) = self.debug_store.append_artifact(&artifact) {
            error!(
                error = ?error,
                run_id = artifact.run_id.as_deref(),
                artifact_name = %artifact.name,
                "failed to persist debug artifact"
            );
            let mut counters = self.lock_counters();
            *counters
                .entry("debug_artifact_persist_failures".to_string())
                .or_default() += 1;
        }
    }

    fn increment_counter(&self, name: &str, delta: u64) {
        let mut counters = self.lock_counters();
        *counters.entry(name.to_string()).or_default() += delta;
    }

    fn metrics_snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            counters: self.lock_counters().clone(),
        }
    }
}

fn append_external_action_without_blocking_core(
    external_actions: &ExternalActionService,
    event: &TraceEvent,
) -> anyhow::Result<Option<crate::ExternalActionAuditRecord>> {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return external_actions.append_trace(event);
    };
    if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
        return external_actions.append_trace(event);
    }
    catch_unwind(AssertUnwindSafe(|| {
        tokio::task::block_in_place(|| external_actions.append_trace(event))
    }))
    .unwrap_or_else(|_| external_actions.append_trace(event))
}

/// Builds an SSE stream filtered by session and run identifiers.
pub(crate) fn sse_stream(
    subscription: DaemonEventSubscription,
    session_filter: Option<String>,
    run_filter: Option<String>,
) -> Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>> {
    let has_filter = session_filter.is_some() || run_filter.is_some();
    let stream_state = subscription.state.clone();
    let effective_after_id = subscription.effective_after_id;
    let filtered_replay = subscription
        .replay
        .into_iter()
        .filter(|envelope| {
            event_matches_filters(envelope, session_filter.as_deref(), run_filter.as_deref())
        })
        .collect::<Vec<_>>();
    let filtered_replay_last_id = filtered_replay
        .last()
        .map(|envelope| envelope.id)
        .or(effective_after_id)
        .or(subscription.live_after_id);
    let initial_gap = subscription.stream_gap.filter(|gap| {
        if !has_filter || gap.emit_without_matching_replay {
            return true;
        }
        let Some(after_id) = effective_after_id else {
            return false;
        };
        stream_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .scope_eviction_after(session_filter.as_deref(), run_filter.as_deref(), after_id)
            .is_some()
    });
    let initial_gap_resume_after_id = initial_gap.as_ref().and_then(|gap| gap.resume_after_id);
    let live_watermark_id = filtered_replay_last_id.max(initial_gap_resume_after_id);
    let scope = stream_gap_scope(session_filter.as_deref(), run_filter.as_deref());
    let gap_stream = futures_util::stream::iter(
        initial_gap
            .and_then(|gap| {
                stream_gap_event(
                    gap.skipped,
                    gap.resume_after_id,
                    gap.reason,
                    scope,
                    gap.skipped_is_estimate || (has_filter && !gap.emit_without_matching_replay),
                )
            })
            .map(Ok)
            .into_iter(),
    );
    let replay_stream = futures_util::stream::iter(
        filtered_replay
            .into_iter()
            .filter_map(move |envelope| envelope_to_sse(&envelope, None, None).map(Ok)),
    );

    let live_stream = futures_util::stream::unfold(
        LiveSseState {
            receiver: BroadcastStream::new(subscription.receiver),
            session_filter,
            run_filter,
            live_after_id: subscription.live_after_id,
            last_matching_id: live_watermark_id,
            last_delivered_id: live_watermark_id,
            state: subscription.state,
            pending_filtered_gap: None,
            pending_event: None,
            pending_event_id: None,
        },
        |state| async move { next_live_sse_event(state).await },
    );
    Sse::new(gap_stream.chain(replay_stream).chain(live_stream)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(10))
            .event(heartbeat_event()),
    )
}

struct LiveSseState {
    receiver: BroadcastStream<DaemonEventEnvelope>,
    session_filter: Option<String>,
    run_filter: Option<String>,
    live_after_id: Option<u64>,
    last_matching_id: Option<u64>,
    last_delivered_id: Option<u64>,
    state: Arc<StdMutex<DaemonEventBusState>>,
    pending_filtered_gap: Option<u64>,
    pending_event: Option<Event>,
    pending_event_id: Option<u64>,
}

async fn next_live_sse_event(
    mut state: LiveSseState,
) -> Option<(Result<Event, Infallible>, LiveSseState)> {
    if let Some(event) = state.pending_event.take() {
        state.last_delivered_id = state.pending_event_id.take().or(state.last_delivered_id);
        return Some((Ok(event), state));
    }

    loop {
        match state.receiver.next().await? {
            Ok(envelope) if state.live_after_id.is_some_and(|id| envelope.id <= id) => continue,
            Ok(envelope) => {
                if !event_matches_filters(
                    &envelope,
                    state.session_filter.as_deref(),
                    state.run_filter.as_deref(),
                ) {
                    continue;
                }
                let previous_matching_id = state.last_matching_id.unwrap_or(0);
                let Some(event) = envelope_to_sse(&envelope, None, None) else {
                    continue;
                };
                if let Some(skipped) = state.pending_filtered_gap.take() {
                    let scoped_eviction = state
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .scope_eviction_after(
                            state.session_filter.as_deref(),
                            state.run_filter.as_deref(),
                            previous_matching_id,
                        );
                    state.last_matching_id = Some(envelope.id);
                    if let Some(scoped_eviction) = scoped_eviction
                        && let Some((gap, gap_id)) = stream_gap_event_after(
                            skipped,
                            Some(scoped_eviction.resume_after_id()),
                            state.last_delivered_id,
                            StreamGapReason::LiveLag,
                            stream_gap_scope(
                                state.session_filter.as_deref(),
                                state.run_filter.as_deref(),
                            ),
                            true,
                        )
                    {
                        state.last_delivered_id = gap_id.or(state.last_delivered_id);
                        state.pending_event = Some(event);
                        state.pending_event_id = Some(envelope.id);
                        return Some((Ok(gap), state));
                    }
                    state.last_delivered_id = Some(envelope.id);
                    return Some((Ok(event), state));
                }
                state.last_matching_id = Some(envelope.id);
                state.last_delivered_id = Some(envelope.id);
                return Some((Ok(event), state));
            }
            Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                state
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .record_stream_lag(skipped);
                if state.session_filter.is_none() && state.run_filter.is_none() {
                    let resume_after_id =
                        safe_live_resume_after_id(&state.state, state.last_delivered_id);
                    if let Some((gap, gap_id)) = stream_gap_event_after(
                        skipped,
                        resume_after_id,
                        state.last_delivered_id,
                        StreamGapReason::LiveLag,
                        StreamGapScope::Global,
                        false,
                    ) {
                        state.last_delivered_id = gap_id.or(state.last_delivered_id);
                        return Some((Ok(gap), state));
                    }
                    continue;
                }
                let scoped_eviction = state
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .scope_eviction_after(
                        state.session_filter.as_deref(),
                        state.run_filter.as_deref(),
                        state.last_matching_id.unwrap_or(0),
                    );
                let skipped = state
                    .pending_filtered_gap
                    .unwrap_or(0)
                    .saturating_add(skipped);
                if let Some(scoped_eviction) = scoped_eviction {
                    let resume_after_id = scoped_eviction.resume_after_id();
                    state.last_matching_id = Some(resume_after_id);
                    state.pending_filtered_gap = None;
                    if let Some((gap, gap_id)) = stream_gap_event_after(
                        skipped,
                        Some(resume_after_id),
                        state.last_delivered_id,
                        StreamGapReason::LiveLag,
                        stream_gap_scope(
                            state.session_filter.as_deref(),
                            state.run_filter.as_deref(),
                        ),
                        true,
                    ) {
                        state.last_delivered_id = gap_id.or(state.last_delivered_id);
                        return Some((Ok(gap), state));
                    }
                    continue;
                }
                state.pending_filtered_gap = Some(skipped);
            }
        }
    }
}

fn event_matches_filters(
    envelope: &DaemonEventEnvelope,
    session_filter: Option<&str>,
    run_filter: Option<&str>,
) -> bool {
    if let Some(session_filter) = session_filter {
        if envelope.event.session_id() != Some(session_filter) {
            return false;
        }
    }
    if let Some(run_filter) = run_filter {
        if envelope.event.run_id() != Some(run_filter) {
            return false;
        }
    }
    true
}

fn envelope_to_sse(
    envelope: &DaemonEventEnvelope,
    session_filter: Option<&str>,
    run_filter: Option<&str>,
) -> Option<Event> {
    if !event_matches_filters(envelope, session_filter, run_filter) {
        return None;
    }
    Event::default()
        .id(envelope.id.to_string())
        .event(envelope.event.event_name())
        .json_data(&envelope.event)
        .ok()
}

fn heartbeat_event() -> Event {
    Event::default()
        .event(DaemonEvent::Heartbeat.event_name())
        .json_data(&DaemonEvent::Heartbeat)
        .expect("heartbeat SSE event should serialize")
}

fn stream_gap_scope(session_filter: Option<&str>, run_filter: Option<&str>) -> StreamGapScope {
    if run_filter.is_some() {
        StreamGapScope::Run
    } else if session_filter.is_some() {
        StreamGapScope::Session
    } else {
        StreamGapScope::Global
    }
}

fn safe_live_resume_after_id(
    state: &Arc<StdMutex<DaemonEventBusState>>,
    last_delivered_id: Option<u64>,
) -> Option<u64> {
    let resume_after_id = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .history
        .front()
        .map(|envelope| envelope.id.saturating_sub(1));
    resume_after_id.filter(|id| last_delivered_id.is_none_or(|last| *id > last))
}

fn stream_gap_event(
    skipped: u64,
    resume_after_id: Option<u64>,
    reason: StreamGapReason,
    scope: StreamGapScope,
    skipped_is_estimate: bool,
) -> Option<Event> {
    stream_gap_event_with_id(
        skipped,
        resume_after_id,
        resume_after_id,
        reason,
        scope,
        skipped_is_estimate,
    )
}

fn stream_gap_event_after(
    skipped: u64,
    resume_after_id: Option<u64>,
    last_delivered_id: Option<u64>,
    reason: StreamGapReason,
    scope: StreamGapScope,
    skipped_is_estimate: bool,
) -> Option<(Event, Option<u64>)> {
    let event_id = resume_after_id.filter(|id| last_delivered_id.is_none_or(|last| *id > last));
    stream_gap_event_with_id(
        skipped,
        resume_after_id,
        event_id,
        reason,
        scope,
        skipped_is_estimate,
    )
    .map(|event| (event, event_id))
}

fn stream_gap_event_with_id(
    skipped: u64,
    resume_after_id: Option<u64>,
    event_id: Option<u64>,
    reason: StreamGapReason,
    scope: StreamGapScope,
    skipped_is_estimate: bool,
) -> Option<Event> {
    let resume_after_id = resume_after_id.map(|id| id.to_string());
    let event = DaemonEvent::StreamGap {
        skipped,
        resume_after_id: resume_after_id.clone(),
        reason,
        scope,
        skipped_is_estimate,
    };
    let mut sse_event = Event::default().event(event.event_name());
    if let Some(event_id) = event_id {
        sse_event = sse_event.id(event_id.to_string());
    }
    sse_event.json_data(&event).ok()
}

#[cfg(test)]
mod tests {
    use std::{fs, thread, time::Duration};

    use super::{
        DaemonEvent, DaemonEventBus, MAX_EVENT_HISTORY_CAPACITY, ScopeEvictionStatus,
        StreamGapReason,
    };
    use tokio_stream::wrappers::BroadcastStream;

    fn interrupted(session_id: &str) -> DaemonEvent {
        DaemonEvent::Interrupted {
            session_id: session_id.to_string(),
            agent_id: "agent-1".to_string(),
        }
    }

    #[test]
    fn event_bus_assigns_monotone_ids_and_replays_from_cursor() {
        let bus = DaemonEventBus::new(2);
        bus.publish(interrupted("s1"));
        bus.publish(interrupted("s2"));
        bus.publish(interrupted("s3"));

        let replay = bus.subscribe_after(Some(0)).replay;
        let ids = replay.iter().map(|event| event.id).collect::<Vec<_>>();
        assert_eq!(ids, vec![2, 3]);

        let replay = bus.subscribe_after(Some(2)).replay;
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].id, 3);
        assert_eq!(replay[0].event.session_id(), Some("s3"));
    }

    #[test]
    fn event_bus_status_reports_replay_retention_and_gaps() {
        let bus = DaemonEventBus::new(2);
        bus.publish(interrupted("s1"));
        bus.publish(interrupted("s2"));
        bus.publish(interrupted("s3"));

        let _subscription = bus.subscribe_after(Some(0));
        {
            let mut state = bus
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.record_stream_lag(5);
        }

        let status = bus.status_snapshot();
        assert_eq!(status.history_capacity, 2);
        assert_eq!(status.retained_event_count, 2);
        assert_eq!(status.oldest_event_id, Some(2));
        assert_eq!(status.newest_event_id, Some(3));
        assert_eq!(status.next_event_id, 4);
        assert_eq!(status.tail_event_id_cursor.as_deref(), Some("3"));
        assert_eq!(status.replay_buffer_utilization_percent, 100);
        assert_eq!(status.evicted_event_count, 1);
        assert_eq!(status.replay_gap_count, 1);
        assert_eq!(status.stream_lagged_event_count, 5);
        assert_eq!(status.evicted_session_scope_count, 1);
    }

    #[test]
    fn event_bus_serializes_concurrent_publish_delivery_order() {
        let bus = DaemonEventBus::new(2_048);
        let mut receiver = bus.subscribe();
        let publishers = (0..16)
            .map(|publisher| {
                let bus = bus.clone();
                thread::spawn(move || {
                    for index in 0..100 {
                        bus.publish(interrupted(&format!("{publisher}-{index}")));
                    }
                })
            })
            .collect::<Vec<_>>();
        for publisher in publishers {
            publisher.join().expect("publisher should not panic");
        }

        let mut live_ids = Vec::new();
        while let Ok(envelope) = receiver.try_recv() {
            live_ids.push(envelope.id);
        }
        assert_eq!(live_ids.len(), 1_600);
        assert!(
            live_ids.windows(2).all(|window| window[0] < window[1]),
            "live delivery order must follow monotonically increasing ids"
        );

        let replay_ids = bus
            .subscribe_after(Some(0))
            .replay
            .iter()
            .map(|event| event.id)
            .collect::<Vec<_>>();
        assert_eq!(replay_ids.len(), 1_600);
        assert_eq!(replay_ids, live_ids);
    }

    #[test]
    fn event_bus_subscription_reports_truncated_cursor_gap() {
        let bus = DaemonEventBus::new(2);
        bus.publish(interrupted("s1"));
        bus.publish(interrupted("s2"));
        bus.publish(interrupted("s3"));

        let subscription = bus.subscribe_after(Some(0));
        let gap = subscription.stream_gap.expect("cursor should report gap");
        assert_eq!(gap.skipped, 1);
        assert_eq!(gap.resume_after_id, Some(1));
        assert_eq!(
            subscription
                .replay
                .iter()
                .map(|event| event.id)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(subscription.live_after_id, Some(3));
    }

    #[test]
    fn event_bus_tracks_evicted_events_by_filter_scope() {
        let bus = DaemonEventBus::new(2);
        bus.publish(interrupted("s1"));
        bus.publish(interrupted("s2"));
        bus.publish(interrupted("s2"));

        let subscription = bus.subscribe_after(Some(0));
        let state = subscription
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            state.scope_eviction_after(Some("s1"), None, 0),
            Some(ScopeEvictionStatus::KnownLoss { resume_after_id: 1 })
        );
        assert_eq!(state.scope_eviction_after(Some("s2"), None, 0), None);
    }

    #[test]
    fn event_bus_bounds_scope_eviction_index_and_reports_metadata_gap() {
        let bus = DaemonEventBus::new(2);
        for index in 0..20 {
            bus.publish(interrupted(&format!("s{index}")));
        }

        let subscription = bus.subscribe_after(Some(0));
        let state = subscription
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(state.last_evicted_by_session.len() <= bus.scope_eviction_retention() as usize);
        assert!(state.scope_eviction_floor_id > 0);
        assert_eq!(
            state.scope_eviction_after(Some("s0"), None, 0),
            Some(ScopeEvictionStatus::MetadataGap {
                resume_after_id: state.scope_eviction_floor_id
            })
        );
    }

    #[tokio::test]
    async fn filtered_live_lag_emits_gap_without_future_matching_event() {
        let bus = DaemonEventBus::new(2);
        let receiver = bus.subscribe();
        bus.publish(interrupted("target"));
        bus.publish(interrupted("noise-1"));
        bus.publish(interrupted("noise-2"));

        let state = super::LiveSseState {
            receiver: BroadcastStream::new(receiver),
            session_filter: Some("target".to_string()),
            run_filter: None,
            live_after_id: None,
            last_matching_id: Some(0),
            last_delivered_id: None,
            state: bus.state.clone(),
            pending_filtered_gap: None,
            pending_event: None,
            pending_event_id: None,
        };
        let result =
            tokio::time::timeout(Duration::from_secs(1), super::next_live_sse_event(state))
                .await
                .expect("filtered lag should emit a gap without waiting for a future scoped event")
                .expect("stream should remain open");
        assert!(result.0.is_ok());
    }

    #[tokio::test]
    async fn filtered_live_lag_advances_scope_watermark_after_gap() {
        let bus = DaemonEventBus::new(2);
        let receiver = bus.subscribe();
        bus.publish(interrupted("target"));
        bus.publish(interrupted("noise-1"));
        bus.publish(interrupted("noise-2"));

        let state = super::LiveSseState {
            receiver: BroadcastStream::new(receiver),
            session_filter: Some("target".to_string()),
            run_filter: None,
            live_after_id: None,
            last_matching_id: Some(0),
            last_delivered_id: None,
            state: bus.state.clone(),
            pending_filtered_gap: None,
            pending_event: None,
            pending_event_id: None,
        };
        let (_, state) =
            tokio::time::timeout(Duration::from_secs(1), super::next_live_sse_event(state))
                .await
                .expect("filtered lag should emit a scoped gap")
                .expect("stream should remain open");
        assert_eq!(state.last_matching_id, Some(1));

        bus.publish(interrupted("noise-3"));
        bus.publish(interrupted("noise-4"));
        bus.publish(interrupted("noise-5"));

        let duplicate = tokio::time::timeout(
            Duration::from_millis(100),
            super::next_live_sse_event(state),
        )
        .await;
        assert!(
            duplicate.is_err(),
            "unrelated later lag must not repeat the already reported scoped gap"
        );
    }

    #[tokio::test]
    async fn global_live_lag_gap_advances_to_safe_replay_cursor() {
        let bus = DaemonEventBus::new(2);
        let receiver = bus.subscribe();
        bus.publish(interrupted("s1"));
        bus.publish(interrupted("s2"));
        bus.publish(interrupted("s3"));

        let state = super::LiveSseState {
            receiver: BroadcastStream::new(receiver),
            session_filter: None,
            run_filter: None,
            live_after_id: None,
            last_matching_id: None,
            last_delivered_id: None,
            state: bus.state.clone(),
            pending_filtered_gap: None,
            pending_event: None,
            pending_event_id: None,
        };
        let (_, state) =
            tokio::time::timeout(Duration::from_secs(1), super::next_live_sse_event(state))
                .await
                .expect("global lag should emit a gap")
                .expect("stream should remain open");

        assert_eq!(
            state.last_delivered_id,
            Some(1),
            "live lag gap should carry the cursor just before retained replay history"
        );
        assert_eq!(bus.status_snapshot().stream_lagged_event_count, 1);
    }

    #[test]
    fn event_bus_zero_capacity_is_clamped() {
        let bus = DaemonEventBus::new(0);
        bus.publish(interrupted("s1"));
        bus.publish(interrupted("s2"));

        let status = bus.status_snapshot();
        assert_eq!(status.history_capacity, 1);
        assert_eq!(status.retained_event_count, 1);
        assert_eq!(status.newest_event_id, Some(2));
    }

    #[test]
    fn event_bus_excessive_capacity_is_clamped() {
        let bus = DaemonEventBus::new(usize::MAX);
        assert_eq!(
            bus.status_snapshot().history_capacity,
            MAX_EVENT_HISTORY_CAPACITY
        );
    }

    #[tokio::test]
    async fn filtered_live_lag_does_not_regress_after_initial_gap_id() {
        let bus = DaemonEventBus::new(2);
        let receiver = bus.subscribe();
        bus.publish(interrupted("target"));
        bus.publish(interrupted("noise-1"));
        bus.publish(interrupted("noise-2"));

        let state = super::LiveSseState {
            receiver: BroadcastStream::new(receiver),
            session_filter: Some("target".to_string()),
            run_filter: None,
            live_after_id: None,
            last_matching_id: Some(0),
            last_delivered_id: Some(99),
            state: bus.state.clone(),
            pending_filtered_gap: None,
            pending_event: None,
            pending_event_id: None,
        };
        let (_, state) =
            tokio::time::timeout(Duration::from_secs(1), super::next_live_sse_event(state))
                .await
                .expect("filtered lag should emit a scoped gap")
                .expect("stream should remain open");

        assert_eq!(state.last_delivered_id, Some(99));
    }

    #[test]
    fn event_bus_clamps_future_cursors_so_live_events_are_not_starved() {
        let bus = DaemonEventBus::new(2);
        let subscription = bus.subscribe_after(Some(u64::MAX));
        assert_eq!(subscription.live_after_id, Some(0));
        assert!(subscription.stream_gap.is_none());
    }

    #[test]
    fn event_bus_reports_epoch_gap_for_pre_restart_cursor_without_history() {
        let bus = DaemonEventBus::new_with_start_id(2, 100);
        let subscription = bus.subscribe_after(Some(42));
        let gap = subscription
            .stream_gap
            .expect("pre-epoch cursor should report restart gap");

        assert_eq!(gap.resume_after_id, Some(99));
        assert_eq!(gap.skipped, 57);
        assert!(gap.emit_without_matching_replay);
        assert!(subscription.replay.is_empty());
        assert_eq!(subscription.live_after_id, Some(42));
    }

    #[test]
    fn event_bus_drops_events_before_epoch_id_range_can_repeat() {
        let bus = DaemonEventBus::new_with_start_id(4, u64::MAX - 1);
        bus.publish(interrupted("last-safe"));
        bus.publish(interrupted("dropped"));

        let status = bus.status_snapshot();
        assert_eq!(status.retained_event_count, 1);
        assert_eq!(status.newest_event_id, Some(u64::MAX - 1));
        assert_eq!(status.next_event_id, u64::MAX);
    }

    #[test]
    fn stream_gap_resume_cursor_serializes_as_string() {
        let event = DaemonEvent::StreamGap {
            skipped: 1,
            resume_after_id: Some(u64::MAX.to_string()),
            reason: StreamGapReason::ReplayWindow,
            scope: super::StreamGapScope::Global,
            skipped_is_estimate: false,
        };
        let value = serde_json::to_value(event).expect("stream gap should serialize");

        assert_eq!(value["type"], "stream_gap");
        assert_eq!(value["resume_after_id"], u64::MAX.to_string());
        assert_eq!(value["reason"], "replay_window");
        assert_eq!(value["scope"], "global");
        assert_eq!(value["skipped_is_estimate"], false);
    }

    #[test]
    fn persistent_epoch_keeps_event_ids_monotone_across_restart() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let epoch_path = temp.path().join("events").join("event-id-epoch");

        let first_bus = DaemonEventBus::new_with_persistent_epoch(4, &epoch_path)?;
        first_bus.publish(interrupted("before-restart"));
        let first_id = first_bus
            .subscribe_after(Some(0))
            .replay
            .last()
            .expect("first event")
            .id;

        let restarted_bus = DaemonEventBus::new_with_persistent_epoch(4, &epoch_path)?;
        restarted_bus.publish(interrupted("after-restart"));
        let restarted_id = restarted_bus
            .subscribe_after(Some(0))
            .replay
            .last()
            .expect("restarted event")
            .id;

        assert!(
            restarted_id > first_id,
            "SSE ids must stay monotone across daemon restarts so Last-Event-ID cannot mask fresh events"
        );
        Ok(())
    }

    #[test]
    fn persistent_epoch_recovers_from_corrupt_epoch_file() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let epoch_path = temp.path().join("events").join("event-id-epoch");
        fs::create_dir_all(epoch_path.parent().expect("epoch parent"))?;
        fs::write(&epoch_path, "not-a-number\n")?;

        let bus = DaemonEventBus::new_with_persistent_epoch(4, &epoch_path)?;
        bus.publish(interrupted("after-corrupt-epoch"));
        let event_id = bus
            .subscribe_after(Some(0))
            .replay
            .last()
            .expect("event after corrupt epoch")
            .id;
        assert!(event_id > 0);
        Ok(())
    }

    #[test]
    fn persistent_epoch_rejects_unrepresentable_epoch_file() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let epoch_path = temp.path().join("events").join("event-id-epoch");
        fs::create_dir_all(epoch_path.parent().expect("epoch parent"))?;
        fs::write(
            &epoch_path,
            format!("{}\n", (u64::MAX >> super::EVENT_ID_EPOCH_SHIFT) + 1),
        )?;

        let error = match DaemonEventBus::new_with_persistent_epoch(4, &epoch_path) {
            Ok(_) => panic!("unrepresentable epoch should fail closed"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("exceeds maximum supported epoch")
        );
        Ok(())
    }
}
