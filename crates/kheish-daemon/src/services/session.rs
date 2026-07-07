use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};
use kheish_session::{FileSessionStore, PersistedSessionRecord, StoredSession};
use kheish_types::{
    ArchivedTaskCounts, ArchivedTaskRecord, CapabilityScope, CredentialScope,
    HOOK_RUNTIME_STATE_METADATA_KEY, HookRuntimeState, LearningScope, ReplyHandle,
    SESSION_CAPABILITY_SCOPE_METADATA_KEY, SESSION_CONTROL_STATE_METADATA_KEY,
    SESSION_CREDENTIAL_SCOPE_METADATA_KEY, SESSION_EXECUTION_IDENTITY_METADATA_KEY,
    SESSION_OPERATOR_CONFIG_METADATA_KEY, SESSION_PERSONA_BINDING_METADATA_KEY,
    SESSION_REPLY_TARGETS_METADATA_KEY, SESSION_ROUTE_POLICY_METADATA_KEY, SessionControlState,
    SessionExecutionIdentity, SessionOperatorConfig, SessionPersonaBinding, SessionRoutePolicy,
    TaskArchiveReason, TaskRecord,
};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::warn;

use crate::DaemonTaskStatusSummaryView;
use crate::connectors::{ConnectorKind, reply_target_references_connector};
use crate::memory::{
    RunMemoryIndex, RunMemoryIndexEntry, RunMemoryIndexUpdate, RunMemoryMaintenanceStatusView,
    RunMemoryMetricsSnapshot, RunMemoryPolicyConfig, RunMemoryRecord, RunMemoryStatusView,
    forget_run_memory, remember_run_memory_with_policy, run_memory_entry_expired,
};
use crate::now_ms;
use crate::state::{
    FileDaemonStore, ObservationIngressReceiptState, ObservationIngressReservation, SessionIndex,
    SessionRunIdempotencyReceiptState, SessionRunIdempotencyReservation,
    SessionTaskStatusSummaryState, SidechainSpawnReceiptState, prune_observation_ingress_receipts,
    prune_run_operation_idempotency_receipts, prune_session_run_idempotency_receipts,
};

/// Owns durable session metadata and daemon session index state.
pub(crate) struct SessionService {
    sessions: Arc<FileSessionStore>,
    store: FileDaemonStore,
    index: Mutex<SessionIndex>,
    session_control: Mutex<()>,
    next_session_id: AtomicU64,
    archived_tasks: parking_lot::Mutex<std::collections::HashMap<String, Arc<ArchivedTaskIndex>>>,
}

/// Compact per-session view of the task archive: every archived id plus the
/// terminal-status tally. Kept in memory so control-state saves stay O(live
/// tasks) instead of re-reading the archive file; the daemon is the only
/// writer of a locked state root, so the cache cannot go stale.
#[derive(Clone, Debug, Default)]
pub(crate) struct ArchivedTaskIndex {
    /// Every archived task id, including deleted tombstones.
    pub(crate) ids: BTreeSet<String>,
    /// Tally of archived terminal tasks (deleted tombstones excluded).
    pub(crate) counts: ArchivedTaskCounts,
}

impl ArchivedTaskIndex {
    pub(crate) fn from_entries(entries: &[ArchivedTaskRecord]) -> Self {
        let mut index = Self::default();
        for entry in entries {
            index.ids.insert(entry.task.id.clone());
            if matches!(entry.reason, TaskArchiveReason::Terminal) {
                index.counts.record(&entry.task.status);
            }
        }
        index
    }
}

/// Returns the archived snapshot of one terminal task from loaded archive
/// entries; the latest entry per id wins, and a deleted tombstone hides it.
pub(crate) fn latest_archived_terminal_task(
    entries: Vec<ArchivedTaskRecord>,
    task_id: &str,
) -> Option<TaskRecord> {
    entries
        .into_iter()
        .rev()
        .find(|entry| entry.task.id == task_id)
        .filter(|entry| matches!(entry.reason, TaskArchiveReason::Terminal))
        .map(|entry| entry.task)
}

/// Returns every archived terminal task in archival order, skipping tasks
/// hidden by a deleted tombstone.
pub(crate) fn archived_terminal_tasks(entries: Vec<ArchivedTaskRecord>) -> Vec<TaskRecord> {
    let deleted = entries
        .iter()
        .filter(|entry| matches!(entry.reason, TaskArchiveReason::Deleted))
        .map(|entry| entry.task.id.clone())
        .collect::<BTreeSet<_>>();
    entries
        .into_iter()
        .filter(|entry| {
            matches!(entry.reason, TaskArchiveReason::Terminal) && !deleted.contains(&entry.task.id)
        })
        .map(|entry| entry.task)
        .collect()
}

/// Cheap session-derived counters for `/v1/status`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionOperatorStatusSnapshot {
    pub(crate) session_count: usize,
    pub(crate) task_summary: DaemonTaskStatusSummaryView,
}

impl SessionService {
    /// Applies one index mutation, persists the candidate snapshot, then swaps it in-memory.
    async fn update_index<T>(
        &self,
        update: impl FnOnce(&mut SessionIndex) -> Result<(T, bool)>,
    ) -> Result<T> {
        let mut index = self.index.lock().await;
        let mut next = index.clone();
        let (result, changed) = update(&mut next)?;
        if changed {
            self.store.save_index(&next)?;
            *index = next;
        }
        Ok(result)
    }

    /// Creates a new session service backed by the daemon session store and index.
    pub(crate) fn new(
        sessions: Arc<FileSessionStore>,
        store: FileDaemonStore,
        index: SessionIndex,
        next_session_id: AtomicU64,
    ) -> Self {
        Self {
            sessions,
            store,
            index: Mutex::new(index),
            session_control: Mutex::new(()),
            next_session_id,
            archived_tasks: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Returns one fresh daemon-managed session identifier.
    pub(crate) fn next_session_id(&self) -> String {
        format!(
            "session-{}",
            self.next_session_id.fetch_add(1, Ordering::Relaxed) + 1
        )
    }

    /// Returns the internal daemon session index owned by the service.
    pub(crate) fn index(&self) -> &Mutex<SessionIndex> {
        &self.index
    }

    /// Returns the write lock that serializes durable session-control mutations.
    pub(crate) fn session_control_lock(&self) -> &Mutex<()> {
        &self.session_control
    }

    /// Returns the owning agent identifier for a session when known.
    pub(crate) async fn session_agent_id(&self, session_id: &str) -> Option<String> {
        self.index.lock().await.sessions.get(session_id).cloned()
    }

    /// Records or updates one session-to-agent mapping and persists the daemon index.
    pub(crate) async fn remember_session(&self, session_id: &str, agent_id: &str) -> Result<()> {
        self.update_index(|index| {
            if let Some(existing_agent_id) = index.sessions.get(session_id)
                && existing_agent_id != agent_id
            {
                bail!("session {session_id} is already owned by agent {existing_agent_id}");
            }
            let session_unchanged =
                index.sessions.get(session_id).map(String::as_str) == Some(agent_id);
            let summary_present = index.task_summaries.contains_key(session_id);
            if session_unchanged && summary_present {
                return Ok(((), false));
            }
            if !session_unchanged {
                index
                    .sessions
                    .insert(session_id.to_string(), agent_id.to_string());
            }
            if !summary_present {
                index.task_summaries.insert(
                    session_id.to_string(),
                    SessionTaskStatusSummaryState::default(),
                );
            }
            Ok(((), true))
        })
        .await
    }

    /// Removes one session-owned daemon index footprint during rollback or cleanup.
    pub(crate) async fn forget_session(&self, session_id: &str) -> Result<()> {
        self.update_index(|index| {
            let mut changed = index.sessions.remove(session_id).is_some();
            changed |= index.task_summaries.remove(session_id).is_some();
            changed |= index.session_personas.remove(session_id).is_some();
            changed |= index.reply_targets.remove(session_id).is_some();
            if let Some(entries) = index.run_memories.by_session.remove(session_id) {
                changed = true;
                for entry in entries {
                    changed |=
                        forget_run_memory(&mut index.run_memories, session_id, &entry.run_id);
                }
            }
            let bindings_before = index.bindings.len();
            index
                .bindings
                .retain(|_, bound_session_id| bound_session_id != session_id);
            changed |= bindings_before != index.bindings.len();
            let receipts_before = index.session_run_idempotency_receipts.len();
            index
                .session_run_idempotency_receipts
                .retain(|key, _| session_run_receipt_session_id(key) != Some(session_id));
            changed |= receipts_before != index.session_run_idempotency_receipts.len();
            let operation_receipts_before = index.run_operation_idempotency_receipts.len();
            index
                .run_operation_idempotency_receipts
                .retain(|key, _| run_operation_receipt_session_id(key) != Some(session_id));
            changed |= operation_receipts_before != index.run_operation_idempotency_receipts.len();
            Ok(((), changed))
        })
        .await
    }

    /// Persists the current in-memory daemon session index.
    pub(crate) async fn persist_index(&self) -> Result<()> {
        let index = self.index.lock().await;
        self.store.save_index(&index)
    }

    /// Returns the known session-to-agent mappings.
    pub(crate) async fn session_pairs(&self) -> BTreeMap<String, String> {
        self.index.lock().await.sessions.clone()
    }

    /// Returns cheap operator counters derived from the in-memory session index.
    pub(crate) async fn operator_status_snapshot(&self) -> SessionOperatorStatusSnapshot {
        let index = self.index.lock().await;
        let mut task_summary = DaemonTaskStatusSummaryView::default();
        for session_id in index.sessions.keys() {
            let Some(summary) = index.task_summaries.get(session_id) else {
                task_summary.unindexed_session_count += 1;
                continue;
            };
            task_summary.total += summary.total;
            task_summary.pending += summary.pending;
            task_summary.in_progress += summary.in_progress;
            task_summary.blocked += summary.blocked;
            task_summary.completed += summary.completed;
            task_summary.failed += summary.failed;
            task_summary.cancelled += summary.cancelled;
        }
        SessionOperatorStatusSnapshot {
            session_count: index.sessions.len(),
            task_summary,
        }
    }

    pub(crate) async fn run_memory_status_snapshot(
        &self,
        now_ms: u64,
        policy: RunMemoryPolicyConfig,
        metrics: RunMemoryMetricsSnapshot,
    ) -> RunMemoryStatusView {
        let index = self.index.lock().await;
        let mut unique_run_ids = BTreeSet::new();
        let mut stale_run_ids = BTreeSet::new();
        for entries in index.run_memories.by_session.values() {
            for entry in entries {
                unique_run_ids.insert(entry.run_id.clone());
                if run_memory_entry_expired(entry.recorded_at_ms, now_ms, &policy) {
                    stale_run_ids.insert(entry.run_id.clone());
                }
            }
        }
        RunMemoryStatusView {
            policy,
            maintenance: RunMemoryMaintenanceStatusView::default(),
            indexed_session_count: index.run_memories.by_session.len(),
            indexed_record_count: unique_run_ids.len(),
            indexed_scope_count: index.run_memories.by_scope.len(),
            stale_indexed_record_count: stale_run_ids.len(),
            metrics,
        }
    }

    /// Persists one session's compact task summary in the daemon index,
    /// counting live and archived terminal tasks together.
    pub(crate) async fn remember_task_summary(
        &self,
        session_id: &str,
        state: &SessionControlState,
    ) -> Result<()> {
        let summary = SessionTaskStatusSummaryState::from_control_state(state);
        self.update_index(|index| {
            let previous = index
                .task_summaries
                .insert(session_id.to_string(), summary.clone());
            Ok(((), previous.as_ref() != Some(&summary)))
        })
        .await
    }

    /// Repairs compact task summaries for sessions currently known to daemon topology.
    pub(crate) async fn repair_session_task_summary_index(&self) -> Result<()> {
        let known_sessions = self
            .index
            .lock()
            .await
            .sessions
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut repaired = BTreeMap::new();
        for session_id in known_sessions {
            let value = match self
                .sessions
                .load_metadata_value(&session_id, SESSION_CONTROL_STATE_METADATA_KEY)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    warn!(
                        session_id = %session_id,
                        error = %error,
                        "failed to load session control metadata while repairing task summary cache"
                    );
                    continue;
                }
            };
            let control_state = match value {
                Some(value) => match serde_json::from_value::<SessionControlState>(value) {
                    Ok(state) => state,
                    Err(error) => {
                        warn!(
                            session_id = %session_id,
                            error = %error,
                            "failed to decode session control state while repairing task summary cache"
                        );
                        continue;
                    }
                },
                None => SessionControlState::default(),
            };
            repaired.insert(
                session_id,
                SessionTaskStatusSummaryState::from_control_state(&control_state),
            );
        }
        self.update_index(|index| {
            if index.task_summaries == repaired {
                return Ok(((), false));
            }
            index.task_summaries = repaired.clone();
            Ok(((), true))
        })
        .await
    }

    /// Returns the known session-to-agent mappings currently bound to one persona.
    pub(crate) async fn session_pairs_for_persona(
        &self,
        persona_id: &str,
    ) -> BTreeMap<String, String> {
        let index = self.index.lock().await;
        index
            .session_personas
            .iter()
            .filter_map(|(session_id, bound_persona_id)| {
                (bound_persona_id == persona_id).then(|| {
                    index
                        .sessions
                        .get(session_id)
                        .cloned()
                        .map(|agent_id| (session_id.clone(), agent_id))
                })?
            })
            .collect()
    }

    /// Returns the known session identifiers tracked by the daemon index.
    pub(crate) async fn session_ids(&self) -> Vec<String> {
        self.index.lock().await.sessions.keys().cloned().collect()
    }

    /// Loads the full persisted session record set.
    pub(crate) async fn load_session(&self, session_id: &str) -> Result<StoredSession> {
        self.sessions.load(session_id).await
    }

    /// Deletes the persisted session transcript file during rollback cleanup.
    pub(crate) fn delete_session_file(&self, session_id: &str) -> Result<()> {
        self.archived_tasks.lock().remove(session_id);
        self.sessions.delete(session_id)
    }

    /// Returns the archived-task index of one session, loading it from the
    /// archive file on first access.
    pub(crate) async fn archived_task_index(
        &self,
        session_id: &str,
    ) -> Result<Arc<ArchivedTaskIndex>> {
        if let Some(cached) = self.archived_tasks.lock().get(session_id).cloned() {
            return Ok(cached);
        }
        let loaded = Arc::new(ArchivedTaskIndex::from_entries(
            &self.sessions.load_task_archive(session_id).await?,
        ));
        // A concurrent archiver may have inserted a fresher entry (it holds
        // the session control lock and rewrites the entry after appending);
        // keep whichever landed first — both derive from the same file.
        Ok(self
            .archived_tasks
            .lock()
            .entry(session_id.to_string())
            .or_insert(loaded)
            .clone())
    }

    /// Appends tasks to the per-session archive and returns the updated
    /// terminal tally. Callers must hold the session control lock so the
    /// append and the cache rewrite stay atomic with the hot-state save.
    pub(crate) async fn archive_session_tasks(
        &self,
        session_id: &str,
        entries: &[ArchivedTaskRecord],
    ) -> Result<ArchivedTaskCounts> {
        let current = self.archived_task_index(session_id).await?;
        if entries.is_empty() {
            return Ok(current.counts);
        }
        self.sessions
            .append_task_archive(session_id, entries)
            .await?;
        let mut next = ArchivedTaskIndex {
            ids: current.ids.clone(),
            counts: current.counts,
        };
        for entry in entries {
            next.ids.insert(entry.task.id.clone());
            if matches!(entry.reason, TaskArchiveReason::Terminal) {
                next.counts.record(&entry.task.status);
            }
        }
        let counts = next.counts;
        self.archived_tasks
            .lock()
            .insert(session_id.to_string(), Arc::new(next));
        Ok(counts)
    }

    /// Loads the full archived task records of one session, in archival order.
    pub(crate) async fn load_archived_session_tasks(
        &self,
        session_id: &str,
    ) -> Result<Vec<ArchivedTaskRecord>> {
        self.sessions.load_task_archive(session_id).await
    }

    /// Returns the tracked run-memory pointers for one session.
    pub(crate) async fn tracked_run_memory_entries(
        &self,
        session_id: &str,
    ) -> Vec<RunMemoryIndexEntry> {
        self.index
            .lock()
            .await
            .run_memories
            .by_session
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Returns the tracked run-memory pointers visible through the provided learning scopes.
    pub(crate) async fn tracked_run_memory_entries_for_scopes(
        &self,
        scopes: &[LearningScope],
    ) -> Vec<RunMemoryIndexEntry> {
        let index = self.index.lock().await;
        let mut deduped = BTreeMap::new();
        for scope in scopes {
            let Some(entries) = index.run_memories.by_scope.get(&scope.scope_key()) else {
                continue;
            };
            for entry in entries {
                deduped
                    .entry(entry.run_id.clone())
                    .and_modify(|existing: &mut RunMemoryIndexEntry| {
                        if entry.recorded_at_ms > existing.recorded_at_ms {
                            *existing = entry.clone();
                        }
                    })
                    .or_insert_with(|| entry.clone());
            }
        }
        let mut entries = deduped.into_values().collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            right
                .recorded_at_ms
                .cmp(&left.recorded_at_ms)
                .then_with(|| right.run_id.cmp(&left.run_id))
        });
        entries
    }

    /// Records one run-memory file in the daemon session index and persists the change.
    #[allow(dead_code)]
    pub(crate) async fn remember_run_memory_record(
        &self,
        record: &RunMemoryRecord,
        now_ms: u64,
    ) -> Result<RunMemoryIndexUpdate> {
        self.remember_run_memory_record_with_policy(
            record,
            now_ms,
            &RunMemoryPolicyConfig::default(),
        )
        .await
    }

    /// Records one run-memory file using the supplied policy and persists the index change.
    pub(crate) async fn remember_run_memory_record_with_policy(
        &self,
        record: &RunMemoryRecord,
        now_ms: u64,
        policy: &RunMemoryPolicyConfig,
    ) -> Result<RunMemoryIndexUpdate> {
        self.update_index(|index| {
            let update =
                remember_run_memory_with_policy(&mut index.run_memories, record, now_ms, policy);
            Ok((update.clone(), update.changed))
        })
        .await
    }

    /// Removes stale run-memory pointers for one session and persists the change when needed.
    pub(crate) async fn forget_run_memories(
        &self,
        session_id: &str,
        run_ids: &[String],
    ) -> Result<bool> {
        if run_ids.is_empty() {
            return Ok(false);
        }
        self.update_index(|index| {
            let changed = run_ids.iter().fold(false, |changed, run_id| {
                forget_run_memory(&mut index.run_memories, session_id, run_id) || changed
            });
            Ok((changed, changed))
        })
        .await
    }

    /// Replaces the complete run-memory topology index when policy enforcement changed it.
    pub(crate) async fn replace_run_memory_index(
        &self,
        run_memories: RunMemoryIndex,
    ) -> Result<bool> {
        self.update_index(|index| {
            let changed = index.run_memories != run_memories;
            index.run_memories = run_memories;
            Ok((changed, changed))
        })
        .await
    }

    /// Loads the stored session control state for one session.
    pub(crate) async fn load_session_control_state(
        &self,
        session_id: &str,
    ) -> Result<SessionControlState> {
        self.sessions
            .load_metadata_value(session_id, SESSION_CONTROL_STATE_METADATA_KEY)
            .await?
            .map(serde_json::from_value)
            .transpose()
            .map(|state| state.unwrap_or_default())
            .map_err(Into::into)
    }

    /// Persists the session control state for one session.
    pub(crate) async fn save_session_control_state(
        &self,
        session_id: &str,
        state: &SessionControlState,
    ) -> Result<SessionControlState> {
        self.sessions
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: SESSION_CONTROL_STATE_METADATA_KEY.to_string(),
                    value: serde_json::to_value(state)?,
                },
            )
            .await?;
        Ok(state.clone())
    }

    /// Loads the stored session route policy for one session.
    pub(crate) async fn load_session_route_policy(
        &self,
        session_id: &str,
    ) -> Result<SessionRoutePolicy> {
        self.sessions
            .load_metadata_value(session_id, SESSION_ROUTE_POLICY_METADATA_KEY)
            .await?
            .map(serde_json::from_value)
            .transpose()
            .map(|state| state.unwrap_or_default())
            .map_err(Into::into)
    }

    /// Persists the session route policy for one session.
    pub(crate) async fn save_session_route_policy(
        &self,
        session_id: &str,
        state: &SessionRoutePolicy,
    ) -> Result<SessionRoutePolicy> {
        self.sessions
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: SESSION_ROUTE_POLICY_METADATA_KEY.to_string(),
                    value: serde_json::to_value(state)?,
                },
            )
            .await?;
        Ok(state.clone())
    }

    /// Loads the stored social ledger for one session.
    pub(crate) async fn load_session_social_ledger(
        &self,
        session_id: &str,
    ) -> Result<kheish_types::SessionSocialLedger> {
        self.sessions
            .load_metadata_value(session_id, kheish_types::SESSION_SOCIAL_LEDGER_METADATA_KEY)
            .await?
            .filter(|value| !value.is_null())
            .map(serde_json::from_value)
            .transpose()
            .map(|ledger| ledger.unwrap_or_default())
            .map_err(Into::into)
    }

    /// Persists the social ledger for one session.
    pub(crate) async fn save_session_social_ledger(
        &self,
        session_id: &str,
        ledger: &kheish_types::SessionSocialLedger,
    ) -> Result<kheish_types::SessionSocialLedger> {
        self.sessions
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: kheish_types::SESSION_SOCIAL_LEDGER_METADATA_KEY.to_string(),
                    value: serde_json::to_value(ledger)?,
                },
            )
            .await?;
        Ok(ledger.clone())
    }

    /// Loads the model-facing operator contact policy for one session.
    pub(crate) async fn load_session_operator_config(
        &self,
        session_id: &str,
    ) -> Result<SessionOperatorConfig> {
        self.sessions
            .load_metadata_value(session_id, SESSION_OPERATOR_CONFIG_METADATA_KEY)
            .await?
            .filter(|value| !value.is_null())
            .map(serde_json::from_value)
            .transpose()
            .map(|state| state.unwrap_or_default())
            .map_err(Into::into)
    }

    /// Persists the model-facing operator contact policy for one session.
    pub(crate) async fn save_session_operator_config(
        &self,
        session_id: &str,
        config: &SessionOperatorConfig,
    ) -> Result<SessionOperatorConfig> {
        self.sessions
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: SESSION_OPERATOR_CONFIG_METADATA_KEY.to_string(),
                    value: if config.is_active() {
                        serde_json::to_value(config)?
                    } else {
                        Value::Null
                    },
                },
            )
            .await?;
        Ok(config.clone())
    }

    /// Loads the native tool surface overrides for one session.
    pub(crate) async fn load_session_tool_overrides(
        &self,
        session_id: &str,
    ) -> Result<kheish_types::SessionToolOverrides> {
        self.sessions
            .load_metadata_value(
                session_id,
                kheish_types::SESSION_TOOL_OVERRIDES_METADATA_KEY,
            )
            .await?
            .filter(|value| !value.is_null())
            .map(serde_json::from_value)
            .transpose()
            .map(|state| state.unwrap_or_default())
            .map_err(Into::into)
    }

    /// Persists the native tool surface overrides for one session.
    pub(crate) async fn save_session_tool_overrides(
        &self,
        session_id: &str,
        overrides: &kheish_types::SessionToolOverrides,
    ) -> Result<kheish_types::SessionToolOverrides> {
        self.sessions
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: kheish_types::SESSION_TOOL_OVERRIDES_METADATA_KEY.to_string(),
                    value: if overrides.is_empty() {
                        Value::Null
                    } else {
                        serde_json::to_value(overrides)?
                    },
                },
            )
            .await?;
        Ok(overrides.clone())
    }

    /// Loads the structured output contract configured on one session.
    pub(crate) async fn load_session_output_contract(
        &self,
        session_id: &str,
    ) -> Result<Option<kheish_types::StructuredOutputContract>> {
        self.sessions
            .load_metadata_value(
                session_id,
                kheish_types::SESSION_OUTPUT_CONTRACT_METADATA_KEY,
            )
            .await?
            .filter(|value| !value.is_null())
            .map(serde_json::from_value)
            .transpose()
            .map_err(Into::into)
    }

    /// Persists (or clears, with `None`) one session's output contract.
    pub(crate) async fn save_session_output_contract(
        &self,
        session_id: &str,
        contract: Option<&kheish_types::StructuredOutputContract>,
    ) -> Result<()> {
        self.sessions
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: kheish_types::SESSION_OUTPUT_CONTRACT_METADATA_KEY.to_string(),
                    value: match contract {
                        Some(contract) => serde_json::to_value(contract)?,
                        None => Value::Null,
                    },
                },
            )
            .await?;
        Ok(())
    }

    /// Loads the structured input contract configured on one session.
    pub(crate) async fn load_session_input_contract(
        &self,
        session_id: &str,
    ) -> Result<Option<kheish_types::StructuredInputContract>> {
        self.sessions
            .load_metadata_value(
                session_id,
                kheish_types::SESSION_INPUT_CONTRACT_METADATA_KEY,
            )
            .await?
            .filter(|value| !value.is_null())
            .map(serde_json::from_value)
            .transpose()
            .map_err(Into::into)
    }

    /// Persists (or clears, with `None`) one session's input contract.
    pub(crate) async fn save_session_input_contract(
        &self,
        session_id: &str,
        contract: Option<&kheish_types::StructuredInputContract>,
    ) -> Result<()> {
        self.sessions
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: kheish_types::SESSION_INPUT_CONTRACT_METADATA_KEY.to_string(),
                    value: match contract {
                        Some(contract) => serde_json::to_value(contract)?,
                        None => Value::Null,
                    },
                },
            )
            .await?;
        Ok(())
    }

    /// Loads the stored session capability scope override for one session.
    pub(crate) async fn load_session_capability_scope(
        &self,
        session_id: &str,
    ) -> Result<CapabilityScope> {
        self.sessions
            .load_metadata_value(session_id, SESSION_CAPABILITY_SCOPE_METADATA_KEY)
            .await?
            .map(serde_json::from_value)
            .transpose()
            .map(|scope| scope.unwrap_or_default())
            .map_err(Into::into)
    }

    /// Persists the session capability scope override for one session.
    pub(crate) async fn save_session_capability_scope(
        &self,
        session_id: &str,
        scope: &CapabilityScope,
    ) -> Result<CapabilityScope> {
        let current = self.load_session_capability_scope(session_id).await?;
        if current == *scope {
            return Ok(scope.clone());
        }
        self.sessions
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: SESSION_CAPABILITY_SCOPE_METADATA_KEY.to_string(),
                    value: serde_json::to_value(scope)?,
                },
            )
            .await?;
        Ok(scope.clone())
    }

    /// Loads the stored session credential scope override for one session.
    pub(crate) async fn load_session_credential_scope(
        &self,
        session_id: &str,
    ) -> Result<CredentialScope> {
        self.sessions
            .load_metadata_value(session_id, SESSION_CREDENTIAL_SCOPE_METADATA_KEY)
            .await?
            .map(serde_json::from_value)
            .transpose()
            .map(|scope| scope.unwrap_or_default())
            .map_err(Into::into)
    }

    /// Persists the session credential scope override for one session.
    pub(crate) async fn save_session_credential_scope(
        &self,
        session_id: &str,
        scope: &CredentialScope,
    ) -> Result<CredentialScope> {
        let current = self.load_session_credential_scope(session_id).await?;
        if current == *scope {
            return Ok(scope.clone());
        }
        self.sessions
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: SESSION_CREDENTIAL_SCOPE_METADATA_KEY.to_string(),
                    value: serde_json::to_value(scope)?,
                },
            )
            .await?;
        Ok(scope.clone())
    }

    /// Loads the stored session execution identity for one session.
    pub(crate) async fn load_session_execution_identity(
        &self,
        session_id: &str,
    ) -> Result<SessionExecutionIdentity> {
        self.sessions
            .load_metadata_value(session_id, SESSION_EXECUTION_IDENTITY_METADATA_KEY)
            .await?
            .map(serde_json::from_value)
            .transpose()
            .map(|identity| identity.unwrap_or_default())
            .map_err(Into::into)
    }

    /// Persists the session execution identity for one session.
    pub(crate) async fn save_session_execution_identity(
        &self,
        session_id: &str,
        identity: &SessionExecutionIdentity,
    ) -> Result<SessionExecutionIdentity> {
        let current = self.load_session_execution_identity(session_id).await?;
        if current == *identity {
            return Ok(identity.clone());
        }
        self.sessions
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: SESSION_EXECUTION_IDENTITY_METADATA_KEY.to_string(),
                    value: serde_json::to_value(identity)?,
                },
            )
            .await?;
        Ok(identity.clone())
    }

    /// Loads the stored session persona binding for one session.
    pub(crate) async fn load_session_persona_binding(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionPersonaBinding>> {
        self.sessions
            .load_metadata_value(session_id, SESSION_PERSONA_BINDING_METADATA_KEY)
            .await?
            .filter(|value| !value.is_null())
            .map(serde_json::from_value)
            .transpose()
            .map_err(Into::into)
    }

    /// Persists the session persona binding for one session.
    pub(crate) async fn save_session_persona_binding(
        &self,
        session_id: &str,
        binding: Option<&SessionPersonaBinding>,
    ) -> Result<Option<SessionPersonaBinding>> {
        let current = self.load_session_persona_binding(session_id).await?;
        if current.as_ref() == binding {
            return Ok(current);
        }
        self.sessions
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: SESSION_PERSONA_BINDING_METADATA_KEY.to_string(),
                    value: binding
                        .map(serde_json::to_value)
                        .transpose()?
                        .unwrap_or(Value::Null),
                },
            )
            .await?;
        Ok(binding.cloned())
    }

    /// Loads the stored hook runtime state for one session.
    pub(crate) async fn load_hook_runtime_state(
        &self,
        session_id: &str,
    ) -> Result<HookRuntimeState> {
        self.sessions
            .load_metadata_value(session_id, HOOK_RUNTIME_STATE_METADATA_KEY)
            .await?
            .map(serde_json::from_value)
            .transpose()
            .map(|state| state.unwrap_or_default())
            .map_err(Into::into)
    }

    /// Returns the persisted reply targets for one session.
    pub(crate) async fn session_reply_targets(&self, session_id: &str) -> Vec<ReplyHandle> {
        self.index
            .lock()
            .await
            .reply_targets
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Returns the session identifiers that currently have cached reply-target defaults.
    pub(crate) async fn cached_reply_target_session_ids(&self) -> Vec<String> {
        self.index
            .lock()
            .await
            .reply_targets
            .keys()
            .cloned()
            .collect()
    }

    /// Returns session identifiers whose cached reply-target defaults reference one connector.
    pub(crate) async fn cached_reply_target_session_ids_referencing_connector(
        &self,
        kind: ConnectorKind,
        name: &str,
    ) -> Vec<String> {
        self.index
            .lock()
            .await
            .reply_targets
            .iter()
            .filter(|(_, targets)| {
                targets
                    .iter()
                    .any(|target| reply_target_references_connector(target, kind, name))
            })
            .map(|(session_id, _)| session_id.clone())
            .collect()
    }

    /// Loads the explicit session reply-target defaults stored in transcript metadata.
    ///
    /// Returns `None` when the session never stored explicit defaults and `Some(Vec::new())`
    /// when the session explicitly cleared them.
    pub(crate) async fn load_session_reply_targets(
        &self,
        session_id: &str,
    ) -> Result<Option<Vec<ReplyHandle>>> {
        match self
            .sessions
            .load_metadata_value(session_id, SESSION_REPLY_TARGETS_METADATA_KEY)
            .await?
        {
            Some(value) if value.is_null() => Ok(Some(Vec::new())),
            Some(value) => serde_json::from_value(value).map(Some).map_err(Into::into),
            None => Ok(None),
        }
    }

    /// Persists explicit session reply-target defaults in transcript metadata.
    ///
    /// Passing an empty slice stores an explicit clear tombstone.
    pub(crate) async fn save_session_reply_targets(
        &self,
        session_id: &str,
        reply_targets: &[ReplyHandle],
    ) -> Result<Option<Vec<ReplyHandle>>> {
        let current = self.load_session_reply_targets(session_id).await?;
        let next = Some(reply_targets.to_vec());
        if current == next {
            return Ok(current);
        }
        self.sessions
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: SESSION_REPLY_TARGETS_METADATA_KEY.to_string(),
                    value: if reply_targets.is_empty() {
                        Value::Null
                    } else {
                        serde_json::to_value(reply_targets)?
                    },
                },
            )
            .await?;
        Ok(next)
    }

    /// Updates the in-memory reply-target cache immediately and persists the best-effort cache
    /// snapshot to the daemon index.
    ///
    /// Session transcript metadata remains the durable source of truth for reply-target defaults.
    /// When the index write fails after the transcript append succeeded, the daemon keeps the
    /// in-memory cache updated and relies on restart repair to rebuild the cache from metadata.
    async fn update_reply_target_cache(
        &self,
        session_id: &str,
        reply_targets: Vec<ReplyHandle>,
    ) -> Result<()> {
        let mut index = self.index.lock().await;
        if index.reply_targets.get(session_id) == Some(&reply_targets) {
            return Ok(());
        }
        let mut next = index.clone();
        next.reply_targets
            .insert(session_id.to_string(), reply_targets.clone());
        if let Err(error) = self.store.save_index(&next) {
            warn!(
                session_id = %session_id,
                error = %error,
                "failed to persist reply-target cache update to daemon index; relying on transcript metadata repair"
            );
        }
        *index = next;
        Ok(())
    }

    /// Persists the reply targets resolved for one session.
    pub(crate) async fn remember_session_reply_targets(
        &self,
        session_id: &str,
        reply_targets: Vec<ReplyHandle>,
    ) -> Result<()> {
        if reply_targets.is_empty() {
            return Ok(());
        }
        self.save_session_reply_targets(session_id, &reply_targets)
            .await?;
        self.update_reply_target_cache(session_id, reply_targets)
            .await
    }

    /// Replaces the persisted reply targets for one session, allowing the set to be cleared.
    pub(crate) async fn set_session_reply_targets(
        &self,
        session_id: &str,
        reply_targets: Vec<ReplyHandle>,
    ) -> Result<()> {
        self.save_session_reply_targets(session_id, &reply_targets)
            .await?;
        self.update_reply_target_cache(session_id, reply_targets)
            .await
    }

    /// Records the bound persona identifier for one session in the daemon index cache.
    pub(crate) async fn remember_session_persona(
        &self,
        session_id: &str,
        persona_id: &str,
    ) -> Result<()> {
        self.update_index(|index| {
            if index.session_personas.get(session_id).map(String::as_str) == Some(persona_id) {
                return Ok(((), false));
            }
            index
                .session_personas
                .insert(session_id.to_string(), persona_id.to_string());
            Ok(((), true))
        })
        .await
    }

    /// Removes the cached bound persona identifier for one session.
    pub(crate) async fn forget_session_persona(&self, session_id: &str) -> Result<()> {
        self.update_index(|index| Ok(((), index.session_personas.remove(session_id).is_some())))
            .await
    }

    /// Prunes persona-cache entries for sessions that no longer exist in the daemon topology.
    pub(crate) async fn prune_session_persona_index(&self) -> Result<()> {
        self.update_index(|index| {
            let known_sessions = index
                .sessions
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            let len_before = index.session_personas.len();
            index
                .session_personas
                .retain(|session_id, _| known_sessions.contains(session_id));
            Ok(((), index.session_personas.len() != len_before))
        })
        .await
    }

    /// Prunes reply-target cache entries for sessions that no longer exist in the daemon topology.
    pub(crate) async fn prune_session_reply_target_index(&self) -> Result<()> {
        self.update_index(|index| {
            let known_sessions = index
                .sessions
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            let len_before = index.reply_targets.len();
            index
                .reply_targets
                .retain(|session_id, _| known_sessions.contains(session_id));
            Ok(((), index.reply_targets.len() != len_before))
        })
        .await
    }

    /// Repairs reply-target cache entries for a targeted set of known sessions.
    pub(crate) async fn repair_session_reply_target_index_for_sessions<I>(
        &self,
        session_ids: I,
    ) -> Result<()>
    where
        I: IntoIterator<Item = String>,
    {
        let known_sessions = self
            .index
            .lock()
            .await
            .sessions
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut updates = Vec::new();
        for session_id in session_ids {
            if !known_sessions.contains(&session_id) {
                continue;
            }
            let value = match self
                .sessions
                .load_metadata_value(&session_id, SESSION_REPLY_TARGETS_METADATA_KEY)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    warn!(
                        session_id = %session_id,
                        error = %error,
                        "failed to load session reply-target metadata while repairing reply-target cache"
                    );
                    updates.push((session_id, None));
                    continue;
                }
            };
            let reply_targets = match value {
                Some(value) if value.is_null() => Some(Vec::new()),
                Some(value) => match serde_json::from_value::<Vec<ReplyHandle>>(value) {
                    Ok(reply_targets) => Some(reply_targets),
                    Err(error) => {
                        warn!(
                            session_id = %session_id,
                            error = %error,
                            "failed to decode session reply targets while repairing reply-target cache"
                        );
                        updates.push((session_id, None));
                        continue;
                    }
                },
                None => None,
            };
            updates.push((session_id, reply_targets));
        }
        self.update_index(|index| {
            let mut changed = false;
            for (session_id, reply_targets) in &updates {
                match reply_targets {
                    Some(reply_targets) => {
                        if index.reply_targets.get(session_id) != Some(reply_targets) {
                            index
                                .reply_targets
                                .insert(session_id.clone(), reply_targets.clone());
                            changed = true;
                        }
                    }
                    None => {
                        changed |= index.reply_targets.remove(session_id).is_some();
                    }
                }
            }
            Ok(((), changed))
        })
        .await
    }

    /// Repairs the explicit reply-target cache in the daemon index for sessions currently known to
    /// the daemon topology.
    pub(crate) async fn repair_session_reply_target_index(&self) -> Result<()> {
        let session_ids = self.sessions.list_session_ids()?;
        let known_sessions = self
            .index
            .lock()
            .await
            .sessions
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut repaired = BTreeMap::new();
        for session_id in session_ids {
            if !known_sessions.contains(&session_id) {
                continue;
            }
            let value = match self
                .sessions
                .load_metadata_value(&session_id, SESSION_REPLY_TARGETS_METADATA_KEY)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    warn!(
                        session_id = %session_id,
                        error = %error,
                        "failed to load session reply-target metadata while repairing reply-target cache"
                    );
                    continue;
                }
            };
            let Some(value) = value else {
                continue;
            };
            if value.is_null() {
                repaired.insert(session_id, Vec::new());
                continue;
            }
            match serde_json::from_value::<Vec<ReplyHandle>>(value.clone()) {
                Ok(reply_targets) => {
                    repaired.insert(session_id, reply_targets);
                }
                Err(error) => {
                    warn!(
                        session_id = %session_id,
                        error = %error,
                        "failed to decode session reply targets while repairing reply-target cache"
                    );
                }
            }
        }
        self.update_index(|index| {
            if index.reply_targets == repaired {
                return Ok(((), false));
            }
            index.reply_targets = repaired.clone();
            Ok(((), true))
        })
        .await
    }

    /// Repairs persona-cache entries for a targeted set of known sessions.
    pub(crate) async fn repair_session_persona_index_for_sessions<I>(
        &self,
        session_ids: I,
    ) -> Result<()>
    where
        I: IntoIterator<Item = String>,
    {
        let known_sessions = self
            .index
            .lock()
            .await
            .sessions
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut updates = Vec::new();
        for session_id in session_ids {
            if !known_sessions.contains(&session_id) {
                continue;
            }
            let value = match self
                .sessions
                .load_metadata_value(&session_id, SESSION_PERSONA_BINDING_METADATA_KEY)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    warn!(
                        session_id = %session_id,
                        error = %error,
                        "failed to load session persona metadata while repairing persona cache"
                    );
                    continue;
                }
            };
            let persona_id = match value {
                Some(value) if !value.is_null() => {
                    match serde_json::from_value::<SessionPersonaBinding>(value) {
                        Ok(binding) => Some(binding.persona_id),
                        Err(error) => {
                            warn!(
                                session_id = %session_id,
                                error = %error,
                                "failed to decode session persona binding while repairing persona cache"
                            );
                            continue;
                        }
                    }
                }
                _ => None,
            };
            updates.push((session_id, persona_id));
        }
        self.update_index(|index| {
            let mut changed = false;
            for (session_id, persona_id) in &updates {
                match persona_id {
                    Some(persona_id) => {
                        if index.session_personas.get(session_id) != Some(persona_id) {
                            index
                                .session_personas
                                .insert(session_id.clone(), persona_id.clone());
                            changed = true;
                        }
                    }
                    None => {
                        changed |= index.session_personas.remove(session_id).is_some();
                    }
                }
            }
            Ok(((), changed))
        })
        .await
    }

    /// Repairs the persona binding cache in the daemon index for sessions currently known to the
    /// daemon topology.
    pub(crate) async fn repair_session_persona_index(&self) -> Result<()> {
        let session_ids = self.sessions.list_session_ids()?;
        let known_sessions = self
            .index
            .lock()
            .await
            .sessions
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut repaired = BTreeMap::new();
        for session_id in session_ids {
            if !known_sessions.contains(&session_id) {
                continue;
            }
            let value = match self
                .sessions
                .load_metadata_value(&session_id, SESSION_PERSONA_BINDING_METADATA_KEY)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    warn!(
                        session_id = %session_id,
                        error = %error,
                        "failed to load session persona metadata while repairing persona cache"
                    );
                    continue;
                }
            };
            let Some(value) = value else {
                continue;
            };
            if value.is_null() {
                continue;
            }
            match serde_json::from_value::<SessionPersonaBinding>(value.clone()) {
                Ok(binding) => {
                    repaired.insert(session_id, binding.persona_id);
                }
                Err(error) => {
                    warn!(
                        session_id = %session_id,
                        error = %error,
                        "failed to decode session persona binding while repairing persona cache"
                    );
                }
            }
        }
        self.update_index(|index| {
            if index.session_personas == repaired {
                return Ok(((), false));
            }
            index.session_personas = repaired.clone();
            Ok(((), true))
        })
        .await
    }

    /// Associates the provided binding keys with one session and persists the index.
    pub(crate) async fn remember_session_bindings(
        &self,
        session_id: &str,
        binding_keys: Vec<String>,
    ) -> Result<()> {
        if binding_keys.is_empty() {
            return Ok(());
        }
        self.update_index(|index| {
            let mut changed = false;
            for binding in binding_keys {
                if let Some(bound) = index.bindings.get(&binding) {
                    if bound == session_id {
                        continue;
                    }
                    bail!("binding key '{binding}' is already associated with session '{bound}'");
                }
                index.bindings.insert(binding, session_id.to_string());
                changed = true;
            }
            Ok(((), changed))
        })
        .await
    }

    /// Resolves one set of binding keys to the single session they share.
    pub(crate) async fn bound_session_id(&self, binding_keys: &[String]) -> Result<Option<String>> {
        if binding_keys.is_empty() {
            return Ok(None);
        }
        let index = self.index.lock().await;
        let mut sessions = binding_keys
            .iter()
            .filter_map(|binding| index.bindings.get(binding).cloned())
            .collect::<Vec<_>>();
        sessions.sort();
        sessions.dedup();
        match sessions.len() {
            0 => Ok(None),
            1 => Ok(sessions.into_iter().next()),
            _ => bail!(
                "binding keys resolve to multiple sessions: {}",
                sessions.join(", ")
            ),
        }
    }

    /// Reserves one observation ingest key for idempotent submission.
    pub(crate) async fn begin_observation_ingress(
        &self,
        key: &str,
        request_fingerprint: &str,
    ) -> Result<ObservationIngressReservation> {
        let now = now_ms();
        self.update_index(|index| {
            let mut changed = prune_observation_ingress_receipts(index, now);
            let reservation = match index.observation_ingress_receipts.get(key) {
                Some(receipt) if receipt.request_fingerprint() != request_fingerprint => {
                    return Err(anyhow!(
                        "observation ingest key {key} was reused with a different payload"
                    ));
                }
                Some(ObservationIngressReceiptState::Pending { .. }) => {
                    ObservationIngressReservation::Pending
                }
                Some(ObservationIngressReceiptState::Submitted { observation_id, .. }) => {
                    ObservationIngressReservation::Existing {
                        observation_id: observation_id.clone(),
                    }
                }
                None => {
                    index.observation_ingress_receipts.insert(
                        key.to_string(),
                        ObservationIngressReceiptState::Pending {
                            request_fingerprint: request_fingerprint.to_string(),
                            recorded_at_ms: now,
                        },
                    );
                    changed = true;
                    ObservationIngressReservation::Reserved
                }
            };
            Ok((reservation, changed))
        })
        .await
    }

    /// Returns the persisted direct run idempotency receipt for one scoped key.
    pub(crate) async fn session_run_idempotency_receipt(
        &self,
        key: &str,
    ) -> Option<SessionRunIdempotencyReceiptState> {
        self.index
            .lock()
            .await
            .session_run_idempotency_receipts
            .get(key)
            .cloned()
    }

    /// Reserves one direct session-run idempotency key and preallocates its run identifier.
    pub(crate) async fn begin_session_run_idempotency<F>(
        &self,
        key: &str,
        request_fingerprint: &str,
        new_run_id: F,
    ) -> Result<SessionRunIdempotencyReservation>
    where
        F: FnOnce() -> String,
    {
        let now = now_ms();
        self.update_index(|index| {
            let mut changed = prune_session_run_idempotency_receipts(index, now);
            let reservation = match index.session_run_idempotency_receipts.get(key) {
                Some(receipt) if receipt.request_fingerprint() != request_fingerprint => {
                    return Err(anyhow!(
                        "session run idempotency key was reused with a different request payload"
                    ));
                }
                Some(SessionRunIdempotencyReceiptState::Pending { run_id, .. }) => {
                    SessionRunIdempotencyReservation::Pending {
                        run_id: run_id.clone(),
                    }
                }
                Some(SessionRunIdempotencyReceiptState::Submitted { run_id, .. }) => {
                    SessionRunIdempotencyReservation::Existing {
                        run_id: run_id.clone(),
                    }
                }
                None => {
                    let run_id = new_run_id();
                    index.session_run_idempotency_receipts.insert(
                        key.to_string(),
                        SessionRunIdempotencyReceiptState::Pending {
                            run_id: run_id.clone(),
                            request_fingerprint: request_fingerprint.to_string(),
                            recorded_at_ms: now,
                        },
                    );
                    changed = true;
                    SessionRunIdempotencyReservation::Reserved { run_id }
                }
            };
            Ok((reservation, changed))
        })
        .await
    }

    /// Marks one direct session-run idempotency key as submitted for the provided run identifier.
    pub(crate) async fn remember_session_run_idempotency(
        &self,
        key: &str,
        run_id: &str,
        request_fingerprint: &str,
    ) -> Result<()> {
        let now = now_ms();
        self.update_index(|index| {
            let mut changed = prune_session_run_idempotency_receipts(index, now);
            let next = SessionRunIdempotencyReceiptState::Submitted {
                run_id: run_id.to_string(),
                request_fingerprint: request_fingerprint.to_string(),
                recorded_at_ms: now,
            };
            if index.session_run_idempotency_receipts.get(key) != Some(&next) {
                index
                    .session_run_idempotency_receipts
                    .insert(key.to_string(), next);
                changed = true;
            }
            Ok(((), changed))
        })
        .await
    }

    /// Clears one direct session-run idempotency reservation or completed receipt.
    pub(crate) async fn forget_session_run_idempotency(&self, key: &str) -> Result<()> {
        self.update_index(|index| {
            Ok((
                (),
                index.session_run_idempotency_receipts.remove(key).is_some(),
            ))
        })
        .await
    }

    /// Returns the persisted run-operation idempotency receipt for one scoped key.
    pub(crate) async fn run_operation_idempotency_receipt(
        &self,
        key: &str,
    ) -> Option<SessionRunIdempotencyReceiptState> {
        self.index
            .lock()
            .await
            .run_operation_idempotency_receipts
            .get(key)
            .cloned()
    }

    /// Reserves one run-operation idempotency key for a known target run.
    pub(crate) async fn begin_run_operation_idempotency(
        &self,
        key: &str,
        run_id: &str,
        request_fingerprint: &str,
    ) -> Result<SessionRunIdempotencyReservation> {
        let now = now_ms();
        self.update_index(|index| {
            let mut changed = prune_run_operation_idempotency_receipts(index, now);
            let reservation = match index.run_operation_idempotency_receipts.get(key) {
                Some(receipt) if receipt.request_fingerprint() != request_fingerprint => {
                    return Err(anyhow!(
                        "run operation idempotency key was reused with a different request payload"
                    ));
                }
                Some(SessionRunIdempotencyReceiptState::Pending { run_id, .. }) => {
                    SessionRunIdempotencyReservation::Pending {
                        run_id: run_id.clone(),
                    }
                }
                Some(SessionRunIdempotencyReceiptState::Submitted { run_id, .. }) => {
                    SessionRunIdempotencyReservation::Existing {
                        run_id: run_id.clone(),
                    }
                }
                None => {
                    index.run_operation_idempotency_receipts.insert(
                        key.to_string(),
                        SessionRunIdempotencyReceiptState::Pending {
                            run_id: run_id.to_string(),
                            request_fingerprint: request_fingerprint.to_string(),
                            recorded_at_ms: now,
                        },
                    );
                    changed = true;
                    SessionRunIdempotencyReservation::Reserved {
                        run_id: run_id.to_string(),
                    }
                }
            };
            Ok((reservation, changed))
        })
        .await
    }

    /// Marks one run-operation idempotency key as submitted.
    pub(crate) async fn remember_run_operation_idempotency(
        &self,
        key: &str,
        run_id: &str,
        request_fingerprint: &str,
    ) -> Result<()> {
        let now = now_ms();
        self.update_index(|index| {
            let mut changed = prune_run_operation_idempotency_receipts(index, now);
            let next = SessionRunIdempotencyReceiptState::Submitted {
                run_id: run_id.to_string(),
                request_fingerprint: request_fingerprint.to_string(),
                recorded_at_ms: now,
            };
            if index.run_operation_idempotency_receipts.get(key) != Some(&next) {
                index
                    .run_operation_idempotency_receipts
                    .insert(key.to_string(), next);
                changed = true;
            }
            Ok(((), changed))
        })
        .await
    }

    /// Clears one run-operation idempotency reservation or completed receipt.
    pub(crate) async fn forget_run_operation_idempotency(&self, key: &str) -> Result<()> {
        self.update_index(|index| {
            Ok((
                (),
                index
                    .run_operation_idempotency_receipts
                    .remove(key)
                    .is_some(),
            ))
        })
        .await
    }

    /// Marks one observation ingest key as submitted for the provided observation identifier.
    pub(crate) async fn remember_observation_ingress(
        &self,
        key: &str,
        observation_id: &str,
        request_fingerprint: &str,
    ) -> Result<()> {
        let now = now_ms();
        self.update_index(|index| {
            let mut changed = prune_observation_ingress_receipts(index, now);
            let next = ObservationIngressReceiptState::Submitted {
                observation_id: observation_id.to_string(),
                request_fingerprint: request_fingerprint.to_string(),
                recorded_at_ms: now,
            };
            if index.observation_ingress_receipts.get(key) != Some(&next) {
                index
                    .observation_ingress_receipts
                    .insert(key.to_string(), next);
                changed = true;
            }
            Ok(((), changed))
        })
        .await
    }

    /// Clears one observation ingest reservation or completed receipt.
    pub(crate) async fn forget_observation_ingress(&self, key: &str) -> Result<()> {
        self.update_index(|index| {
            Ok(((), index.observation_ingress_receipts.remove(key).is_some()))
        })
        .await
    }

    /// Returns the persisted sidechain spawn receipt for one request key when present.
    pub(crate) async fn sidechain_spawn_receipt(
        &self,
        key: &str,
    ) -> Option<SidechainSpawnReceiptState> {
        self.index
            .lock()
            .await
            .sidechain_spawn_receipts
            .get(key)
            .cloned()
    }

    /// Returns a stable snapshot of all persisted sidechain spawn receipts.
    pub(crate) async fn sidechain_spawn_receipts(
        &self,
    ) -> BTreeMap<String, SidechainSpawnReceiptState> {
        self.index.lock().await.sidechain_spawn_receipts.clone()
    }

    /// Persists one sidechain spawn receipt owned by the daemon session index.
    pub(crate) async fn remember_sidechain_spawn_receipt(
        &self,
        key: &str,
        receipt: SidechainSpawnReceiptState,
    ) -> Result<()> {
        self.update_index(|index| {
            if index.sidechain_spawn_receipts.get(key) == Some(&receipt) {
                return Ok(((), false));
            }
            index
                .sidechain_spawn_receipts
                .insert(key.to_string(), receipt);
            Ok(((), true))
        })
        .await
    }

    /// Clears one persisted sidechain spawn receipt.
    pub(crate) async fn forget_sidechain_spawn_receipt(&self, key: &str) -> Result<()> {
        self.update_index(|index| Ok(((), index.sidechain_spawn_receipts.remove(key).is_some())))
            .await
    }
}

fn run_operation_receipt_session_id(key: &str) -> Option<&str> {
    let rest = key.strip_prefix("run-op:")?;
    let (operation_and_session, key_hash) = rest.rsplit_once(':')?;
    if key_hash.is_empty() {
        return None;
    }
    let (_operation, session_id) = operation_and_session.split_once(':')?;
    (!session_id.is_empty()).then_some(session_id)
}

fn session_run_receipt_session_id(key: &str) -> Option<&str> {
    let rest = key.strip_prefix("session-run:")?;
    let (session_id, key_hash) = rest.rsplit_once(':')?;
    if key_hash.is_empty() {
        return None;
    }
    (!session_id.is_empty()).then_some(session_id)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use anyhow::Result;
    use serde_json::Value;
    use tempfile::tempdir;

    use super::SessionService;
    use crate::memory::RunMemoryRecord;
    use crate::state::SessionRunIdempotencyReservation;
    use crate::state::{FileDaemonStore, SessionIndex, SidechainSpawnReceiptState};
    use kheish_session::{FileSessionStore, PersistedSessionRecord};
    use kheish_types::{
        CapabilityScope, HookPermissionUpdate, HookPermissionUpdateBehavior,
        HookPermissionUpdateScope, RecoveredMemoryEntry, ReplyHandle,
        SESSION_PERSONA_BINDING_METADATA_KEY, SESSION_REPLY_TARGETS_METADATA_KEY,
        SessionControlState, SessionPersonaBinding, TaskRecord, TaskStatus,
    };

    #[tokio::test]
    async fn session_service_round_trips_control_state_and_reply_targets() -> Result<()> {
        let temp = tempdir()?;
        let service = SessionService::new(
            Arc::new(FileSessionStore::new(temp.path().join("sessions"))),
            FileDaemonStore::new(temp.path()),
            SessionIndex::default(),
            AtomicU64::new(0),
        );

        let state = SessionControlState {
            plan_mode: true,
            session_permission_mode: Some("dontAsk".to_string()),
            session_permission_updates: vec![HookPermissionUpdate {
                scope: HookPermissionUpdateScope::Session,
                tool_name_pattern: "echo".to_string(),
                behavior: HookPermissionUpdateBehavior::Allow,
                reason: Some("persist me".to_string()),
            }],
            ..SessionControlState::default()
        };
        service
            .save_session_control_state("session-1", &state)
            .await?;
        service
            .remember_session_reply_targets(
                "session-1",
                vec![ReplyHandle {
                    plugin: "daemon".to_string(),
                    address: "session-1".to_string(),
                }],
            )
            .await?;

        let loaded = service.load_session_control_state("session-1").await?;
        assert!(loaded.plan_mode);
        assert_eq!(loaded.session_permission_mode.as_deref(), Some("dontAsk"));
        assert_eq!(
            loaded.session_permission_updates,
            state.session_permission_updates
        );
        assert_eq!(service.session_reply_targets("session-1").await.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn session_service_indexes_task_summary_for_status() -> Result<()> {
        let temp = tempdir()?;
        let service = SessionService::new(
            Arc::new(FileSessionStore::new(temp.path().join("sessions"))),
            FileDaemonStore::new(temp.path()),
            SessionIndex::default(),
            AtomicU64::new(0),
        );

        service.remember_session("session-1", "agent-1").await?;
        let conflict = service.remember_session("session-1", "agent-2").await;
        assert!(
            conflict
                .as_ref()
                .is_err_and(|error| error.to_string().contains("already owned")),
            "conflicting session ownership should be rejected: {conflict:?}"
        );
        let clean = service.operator_status_snapshot().await;
        assert_eq!(clean.session_count, 1);
        assert_eq!(clean.task_summary.total, 0);
        assert_eq!(clean.task_summary.unindexed_session_count, 0);

        service
            .remember_task_summary(
                "session-1",
                &SessionControlState {
                    tasks: vec![
                        test_task("task-1", TaskStatus::InProgress),
                        test_task("task-2", TaskStatus::Failed),
                        test_task("task-3", TaskStatus::Blocked),
                    ],
                    ..SessionControlState::default()
                },
            )
            .await?;

        let snapshot = service.operator_status_snapshot().await;
        assert_eq!(snapshot.task_summary.total, 3);
        assert_eq!(snapshot.task_summary.in_progress, 1);
        assert_eq!(snapshot.task_summary.failed, 1);
        assert_eq!(snapshot.task_summary.blocked, 1);
        assert_eq!(snapshot.task_summary.unindexed_session_count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn repair_session_task_summary_index_backfills_legacy_sessions() -> Result<()> {
        let temp = tempdir()?;
        let sessions = Arc::new(FileSessionStore::new(temp.path().join("sessions")));
        let service = SessionService::new(
            sessions,
            FileDaemonStore::new(temp.path()),
            SessionIndex {
                sessions: BTreeMap::from([("session-1".to_string(), "agent-1".to_string())]),
                ..SessionIndex::default()
            },
            AtomicU64::new(0),
        );
        service
            .save_session_control_state(
                "session-1",
                &SessionControlState {
                    tasks: vec![
                        test_task("task-1", TaskStatus::Completed),
                        test_task("task-2", TaskStatus::Failed),
                    ],
                    ..SessionControlState::default()
                },
            )
            .await?;

        let before = service.operator_status_snapshot().await;
        assert_eq!(before.task_summary.total, 0);
        assert_eq!(before.task_summary.unindexed_session_count, 1);

        service.repair_session_task_summary_index().await?;

        let after = service.operator_status_snapshot().await;
        assert_eq!(after.task_summary.total, 2);
        assert_eq!(after.task_summary.completed, 1);
        assert_eq!(after.task_summary.failed, 1);
        assert_eq!(after.task_summary.unindexed_session_count, 0);
        Ok(())
    }

    fn test_task(id: &str, status: TaskStatus) -> TaskRecord {
        TaskRecord {
            id: id.to_string(),
            title: id.to_string(),
            description: String::new(),
            status,
            owner_agent_id: None,
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            output: None,
            metadata: serde_json::Value::Null,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[tokio::test]
    async fn session_service_persists_explicit_reply_target_clear_tombstones() -> Result<()> {
        let temp = tempdir()?;
        let sessions = Arc::new(FileSessionStore::new(temp.path().join("sessions")));
        let service = SessionService::new(
            sessions.clone(),
            FileDaemonStore::new(temp.path()),
            SessionIndex::default(),
            AtomicU64::new(0),
        );

        service
            .set_session_reply_targets("session-1", Vec::new())
            .await?;

        assert_eq!(
            service.load_session_reply_targets("session-1").await?,
            Some(Vec::new())
        );
        assert_eq!(
            sessions
                .load_metadata_value("session-1", SESSION_REPLY_TARGETS_METADATA_KEY)
                .await?,
            Some(serde_json::Value::Null),
            "explicit clear should persist a null tombstone, not an absent key"
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_service_keeps_reply_target_updates_when_index_cache_persist_fails()
    -> Result<()> {
        let temp = tempdir()?;
        fs::create_dir_all(temp.path().join("daemon-index.json"))?;
        let service = SessionService::new(
            Arc::new(FileSessionStore::new(temp.path().join("sessions"))),
            FileDaemonStore::new(temp.path()),
            SessionIndex::default(),
            AtomicU64::new(0),
        );
        let reply_targets = vec![ReplyHandle {
            plugin: "daemon".to_string(),
            address: "session-1".to_string(),
        }];

        service
            .set_session_reply_targets("session-1", reply_targets.clone())
            .await?;

        assert_eq!(
            service.load_session_reply_targets("session-1").await?,
            Some(reply_targets.clone())
        );
        assert_eq!(
            service.session_reply_targets("session-1").await,
            reply_targets
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_service_rejects_binding_reassignment() -> Result<()> {
        let temp = tempdir()?;
        let service = SessionService::new(
            Arc::new(FileSessionStore::new(temp.path().join("sessions"))),
            FileDaemonStore::new(temp.path()),
            SessionIndex::default(),
            AtomicU64::new(0),
        );

        service
            .remember_session_bindings("session-a", vec!["team:alpha".to_string()])
            .await?;
        let error = service
            .remember_session_bindings("session-b", vec!["team:alpha".to_string()])
            .await
            .expect_err("duplicate binding should fail");
        assert!(
            error.to_string().contains("already associated"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn repair_session_reply_target_index_rebuilds_cache_from_metadata() -> Result<()> {
        let temp = tempdir()?;
        let sessions = Arc::new(FileSessionStore::new(temp.path().join("sessions")));
        let expected = vec![ReplyHandle {
            plugin: "telegram".to_string(),
            address: "{\"connector\":\"ops-bot\",\"chat_id\":42}".to_string(),
        }];
        let service = SessionService::new(
            sessions.clone(),
            FileDaemonStore::new(temp.path()),
            SessionIndex {
                sessions: BTreeMap::from([
                    ("session-1".to_string(), "agent-1".to_string()),
                    ("session-2".to_string(), "agent-2".to_string()),
                ]),
                reply_targets: BTreeMap::from([
                    (
                        "session-1".to_string(),
                        vec![ReplyHandle {
                            plugin: "daemon".to_string(),
                            address: "stale".to_string(),
                        }],
                    ),
                    (
                        "session-stale".to_string(),
                        vec![ReplyHandle {
                            plugin: "daemon".to_string(),
                            address: "ghost".to_string(),
                        }],
                    ),
                ]),
                ..SessionIndex::default()
            },
            AtomicU64::new(0),
        );

        sessions
            .append(
                "session-1",
                PersistedSessionRecord::Metadata {
                    key: SESSION_REPLY_TARGETS_METADATA_KEY.to_string(),
                    value: serde_json::to_value(&expected)?,
                },
            )
            .await?;
        sessions
            .append(
                "session-2",
                PersistedSessionRecord::Metadata {
                    key: SESSION_REPLY_TARGETS_METADATA_KEY.to_string(),
                    value: Value::Null,
                },
            )
            .await?;

        service.repair_session_reply_target_index().await?;

        let index = service.index().lock().await;
        assert_eq!(
            index.reply_targets,
            BTreeMap::from([
                ("session-1".to_string(), expected),
                ("session-2".to_string(), Vec::new()),
            ])
        );
        Ok(())
    }

    #[tokio::test]
    async fn repair_session_reply_target_index_for_sessions_clears_stale_cache_entries()
    -> Result<()> {
        let temp = tempdir()?;
        let sessions = Arc::new(FileSessionStore::new(temp.path().join("sessions")));
        let expected = vec![ReplyHandle {
            plugin: "slack".to_string(),
            address: "{\"connector\":\"alerts\",\"channel_id\":\"C1\"}".to_string(),
        }];
        let service = SessionService::new(
            sessions.clone(),
            FileDaemonStore::new(temp.path()),
            SessionIndex {
                sessions: BTreeMap::from([
                    ("session-1".to_string(), "agent-1".to_string()),
                    ("session-2".to_string(), "agent-2".to_string()),
                ]),
                reply_targets: BTreeMap::from([
                    (
                        "session-1".to_string(),
                        vec![ReplyHandle {
                            plugin: "daemon".to_string(),
                            address: "stale-1".to_string(),
                        }],
                    ),
                    (
                        "session-2".to_string(),
                        vec![ReplyHandle {
                            plugin: "daemon".to_string(),
                            address: "stale-2".to_string(),
                        }],
                    ),
                ]),
                ..SessionIndex::default()
            },
            AtomicU64::new(0),
        );

        sessions
            .append(
                "session-1",
                PersistedSessionRecord::Metadata {
                    key: SESSION_REPLY_TARGETS_METADATA_KEY.to_string(),
                    value: serde_json::to_value(&expected)?,
                },
            )
            .await?;

        service
            .repair_session_reply_target_index_for_sessions(vec![
                "session-1".to_string(),
                "session-2".to_string(),
            ])
            .await?;

        let index = service.index().lock().await;
        assert_eq!(
            index.reply_targets,
            BTreeMap::from([("session-1".to_string(), expected)])
        );
        Ok(())
    }

    #[tokio::test]
    async fn repair_session_persona_index_rebuilds_persona_cache_from_metadata() -> Result<()> {
        let temp = tempdir()?;
        let sessions = Arc::new(FileSessionStore::new(temp.path().join("sessions")));
        let service = SessionService::new(
            sessions.clone(),
            FileDaemonStore::new(temp.path()),
            SessionIndex {
                sessions: BTreeMap::from([
                    ("session-1".to_string(), "agent-1".to_string()),
                    ("session-2".to_string(), "agent-2".to_string()),
                ]),
                session_personas: BTreeMap::from([
                    ("session-1".to_string(), "stale.persona".to_string()),
                    ("session-2".to_string(), "stale.persona".to_string()),
                ]),
                ..SessionIndex::default()
            },
            AtomicU64::new(0),
        );

        sessions
            .append(
                "session-1",
                PersistedSessionRecord::Metadata {
                    key: SESSION_PERSONA_BINDING_METADATA_KEY.to_string(),
                    value: serde_json::to_value(SessionPersonaBinding {
                        persona_id: "persona-1".to_string(),
                        persona_version: 1,
                        display_name: "Persona One".to_string(),
                        soul: "Reply as Persona One.".to_string(),
                        soul_sha256: "hash".to_string(),
                        capability_scope: CapabilityScope::default(),
                        default_inline_skills: Vec::new(),
                        bound_at_ms: 1,
                    })?,
                },
            )
            .await?;
        sessions
            .append(
                "session-2",
                PersistedSessionRecord::Metadata {
                    key: SESSION_PERSONA_BINDING_METADATA_KEY.to_string(),
                    value: Value::Null,
                },
            )
            .await?;

        service.repair_session_persona_index().await?;

        let filtered = service.session_pairs_for_persona("persona-1").await;
        assert_eq!(
            filtered,
            BTreeMap::from([("session-1".to_string(), "agent-1".to_string())])
        );
        assert!(
            service
                .session_pairs_for_persona("stale.persona")
                .await
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn repair_session_persona_index_skips_sessions_missing_topology_mappings() -> Result<()> {
        let temp = tempdir()?;
        let sessions = Arc::new(FileSessionStore::new(temp.path().join("sessions")));
        let service = SessionService::new(
            sessions.clone(),
            FileDaemonStore::new(temp.path()),
            SessionIndex {
                sessions: BTreeMap::from([("session-1".to_string(), "agent-1".to_string())]),
                session_personas: BTreeMap::from([(
                    "session-orphan".to_string(),
                    "stale.persona".to_string(),
                )]),
                ..SessionIndex::default()
            },
            AtomicU64::new(0),
        );

        for session_id in ["session-1", "session-orphan"] {
            sessions
                .append(
                    session_id,
                    PersistedSessionRecord::Metadata {
                        key: SESSION_PERSONA_BINDING_METADATA_KEY.to_string(),
                        value: serde_json::to_value(SessionPersonaBinding {
                            persona_id: "persona-1".to_string(),
                            persona_version: 1,
                            display_name: "Persona One".to_string(),
                            soul: "Reply as Persona One.".to_string(),
                            soul_sha256: "hash".to_string(),
                            capability_scope: CapabilityScope::default(),
                            default_inline_skills: Vec::new(),
                            bound_at_ms: 1,
                        })?,
                    },
                )
                .await?;
        }

        service.repair_session_persona_index().await?;

        let index = service.index().lock().await;
        assert_eq!(
            index.session_personas,
            BTreeMap::from([("session-1".to_string(), "persona-1".to_string())])
        );
        Ok(())
    }

    #[tokio::test]
    async fn prune_session_persona_index_drops_stale_session_keys() -> Result<()> {
        let temp = tempdir()?;
        let sessions = Arc::new(FileSessionStore::new(temp.path().join("sessions")));
        let service = SessionService::new(
            sessions,
            FileDaemonStore::new(temp.path()),
            SessionIndex {
                sessions: BTreeMap::from([("session-1".to_string(), "agent-1".to_string())]),
                session_personas: BTreeMap::from([
                    ("session-1".to_string(), "persona-1".to_string()),
                    ("session-stale".to_string(), "persona-stale".to_string()),
                ]),
                ..SessionIndex::default()
            },
            AtomicU64::new(0),
        );

        service.prune_session_persona_index().await?;

        let index = service.index().lock().await;
        assert_eq!(
            index.session_personas,
            BTreeMap::from([("session-1".to_string(), "persona-1".to_string())])
        );
        Ok(())
    }

    #[tokio::test]
    async fn repair_session_persona_index_for_sessions_repairs_partial_cache() -> Result<()> {
        let temp = tempdir()?;
        let sessions = Arc::new(FileSessionStore::new(temp.path().join("sessions")));
        let service = SessionService::new(
            sessions.clone(),
            FileDaemonStore::new(temp.path()),
            SessionIndex {
                sessions: BTreeMap::from([
                    ("session-1".to_string(), "agent-1".to_string()),
                    ("session-2".to_string(), "agent-2".to_string()),
                ]),
                session_personas: BTreeMap::from([(
                    "session-1".to_string(),
                    "stale.persona".to_string(),
                )]),
                ..SessionIndex::default()
            },
            AtomicU64::new(0),
        );

        sessions
            .append(
                "session-1",
                PersistedSessionRecord::Metadata {
                    key: SESSION_PERSONA_BINDING_METADATA_KEY.to_string(),
                    value: serde_json::to_value(SessionPersonaBinding {
                        persona_id: "persona-1".to_string(),
                        persona_version: 1,
                        display_name: "Persona One".to_string(),
                        soul: "Reply as Persona One.".to_string(),
                        soul_sha256: "hash".to_string(),
                        capability_scope: CapabilityScope::default(),
                        default_inline_skills: Vec::new(),
                        bound_at_ms: 1,
                    })?,
                },
            )
            .await?;
        sessions
            .append(
                "session-2",
                PersistedSessionRecord::Metadata {
                    key: SESSION_PERSONA_BINDING_METADATA_KEY.to_string(),
                    value: serde_json::to_value(SessionPersonaBinding {
                        persona_id: "persona-2".to_string(),
                        persona_version: 1,
                        display_name: "Persona Two".to_string(),
                        soul: "Reply as Persona Two.".to_string(),
                        soul_sha256: "hash".to_string(),
                        capability_scope: CapabilityScope::default(),
                        default_inline_skills: Vec::new(),
                        bound_at_ms: 1,
                    })?,
                },
            )
            .await?;

        service
            .repair_session_persona_index_for_sessions(vec![
                "session-1".to_string(),
                "session-2".to_string(),
            ])
            .await?;

        let filtered_one = service.session_pairs_for_persona("persona-1").await;
        assert_eq!(
            filtered_one,
            BTreeMap::from([("session-1".to_string(), "agent-1".to_string())])
        );
        let filtered_two = service.session_pairs_for_persona("persona-2").await;
        assert_eq!(
            filtered_two,
            BTreeMap::from([("session-2".to_string(), "agent-2".to_string())])
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_service_skips_duplicate_persona_and_capability_scope_metadata() -> Result<()> {
        let temp = tempdir()?;
        let sessions = Arc::new(FileSessionStore::new(temp.path().join("sessions")));
        let service = SessionService::new(
            sessions.clone(),
            FileDaemonStore::new(temp.path()),
            SessionIndex::default(),
            AtomicU64::new(0),
        );

        let binding = SessionPersonaBinding {
            persona_id: "persona-1".to_string(),
            persona_version: 1,
            display_name: "Persona One".to_string(),
            soul: "Reply as Persona One.".to_string(),
            soul_sha256: "hash".to_string(),
            capability_scope: CapabilityScope::default(),
            default_inline_skills: Vec::new(),
            bound_at_ms: 1,
        };
        let scope = CapabilityScope {
            skill_allow: vec!["alpha".to_string()],
            ..CapabilityScope::default()
        };

        service
            .save_session_persona_binding("session-idempotent", Some(&binding))
            .await?;
        service
            .save_session_persona_binding("session-idempotent", Some(&binding))
            .await?;
        service
            .save_session_capability_scope("session-idempotent", &scope)
            .await?;
        service
            .save_session_capability_scope("session-idempotent", &scope)
            .await?;

        let transcript = std::fs::read_to_string(sessions.session_path("session-idempotent"))?;
        assert_eq!(
            transcript.lines().count(),
            0,
            "metadata writes must not grow the journal"
        );
        let sidecar_dir = kheish_session::safe_storage_path(
            &temp.path().join("sessions"),
            "session-idempotent",
            "meta",
        );
        let sidecar_files = std::fs::read_dir(&sidecar_dir)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .count();
        assert_eq!(
            sidecar_files, 2,
            "idempotent metadata writes should keep exactly one sidecar per key"
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_service_tracks_and_prunes_run_memory_index_entries() -> Result<()> {
        let temp = tempdir()?;
        let store = FileDaemonStore::new(temp.path());
        let service = SessionService::new(
            Arc::new(FileSessionStore::new(temp.path().join("sessions"))),
            store.clone(),
            SessionIndex::default(),
            AtomicU64::new(0),
        );

        let memory = RunMemoryRecord {
            session_id: "session-1".to_string(),
            scope_keys: vec![
                "session:session-1".to_string(),
                "project:project-1".to_string(),
            ],
            semantic_capture: crate::memory::RunMemorySemanticCaptureState::Completed,
            memory: RecoveredMemoryEntry {
                run_id: "run-1".to_string(),
                recorded_at_ms: 42,
                status: "completed".to_string(),
                request_preview: None,
                outcome_preview: None,
                artifact_ids: Vec::new(),
                failure_markers: Vec::new(),
                summary: "done".to_string(),
            },
        };

        let update = service.remember_run_memory_record(&memory, 42).await?;
        assert!(update.changed);
        assert_eq!(
            service
                .tracked_run_memory_entries("session-1")
                .await
                .into_iter()
                .map(|entry| entry.run_id)
                .collect::<Vec<_>>(),
            vec!["run-1".to_string()]
        );
        assert_eq!(
            service
                .tracked_run_memory_entries_for_scopes(&[
                    kheish_types::LearningScope {
                        kind: kheish_types::LearningScopeKind::Session,
                        id: "session-1".to_string(),
                    },
                    kheish_types::LearningScope {
                        kind: kheish_types::LearningScopeKind::Project,
                        id: "project-1".to_string(),
                    },
                ])
                .await
                .into_iter()
                .map(|entry| entry.run_id)
                .collect::<Vec<_>>(),
            vec!["run-1".to_string()]
        );

        let persisted = store.load_index()?;
        assert_eq!(
            persisted
                .run_memories
                .by_session
                .get("session-1")
                .map(|entries| entries.len()),
            Some(1)
        );
        assert_eq!(
            persisted
                .run_memories
                .by_scope
                .get("project:project-1")
                .map(|entries| entries.len()),
            Some(1)
        );

        assert!(
            service
                .forget_run_memories("session-1", &[String::from("run-1")])
                .await?
        );
        assert!(
            service
                .tracked_run_memory_entries("session-1")
                .await
                .is_empty()
        );
        assert!(
            service
                .tracked_run_memory_entries_for_scopes(&[kheish_types::LearningScope {
                    kind: kheish_types::LearningScopeKind::Project,
                    id: "project-1".to_string(),
                }])
                .await
                .is_empty()
        );
        assert!(
            store
                .load_index()?
                .run_memories
                .by_session
                .get("session-1")
                .is_none()
        );
        assert!(
            store
                .load_index()?
                .run_memories
                .by_scope
                .get("project:project-1")
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_service_does_not_mutate_index_when_persist_fails() -> Result<()> {
        let temp = tempdir()?;
        fs::create_dir_all(temp.path().join("daemon-index.json"))?;
        let service = SessionService::new(
            Arc::new(FileSessionStore::new(temp.path().join("sessions"))),
            FileDaemonStore::new(temp.path()),
            SessionIndex::default(),
            AtomicU64::new(0),
        );

        let error = service
            .remember_session("session-1", "agent-1")
            .await
            .expect_err("index persistence should fail");
        assert!(
            error.to_string().contains("failed to replace"),
            "unexpected error: {error}"
        );
        assert!(service.session_pairs().await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn session_service_round_trips_sidechain_spawn_receipts() -> Result<()> {
        let temp = tempdir()?;
        let store = FileDaemonStore::new(temp.path());
        let service = SessionService::new(
            Arc::new(FileSessionStore::new(temp.path().join("sessions"))),
            store.clone(),
            SessionIndex::default(),
            AtomicU64::new(0),
        );

        let receipt = SidechainSpawnReceiptState::Committed {
            agent_id: "agent-child-1".to_string(),
            session_id: "session-child-1".to_string(),
            thread_id: Some("thread-child-1".to_string()),
            request_fingerprint: "{\"request\":\"fingerprint\"}".to_string(),
            subtask_request_json: None,
            recorded_at_ms: 42,
        };
        service
            .remember_sidechain_spawn_receipt("agent-parent-1:req-1", receipt.clone())
            .await?;

        assert_eq!(
            service
                .sidechain_spawn_receipt("agent-parent-1:req-1")
                .await,
            Some(receipt.clone())
        );
        assert_eq!(
            store
                .load_index()?
                .sidechain_spawn_receipts
                .get("agent-parent-1:req-1"),
            Some(&receipt)
        );

        service
            .forget_sidechain_spawn_receipt("agent-parent-1:req-1")
            .await?;
        assert!(
            service
                .sidechain_spawn_receipt("agent-parent-1:req-1")
                .await
                .is_none()
        );
        assert!(
            store
                .load_index()?
                .sidechain_spawn_receipts
                .get("agent-parent-1:req-1")
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_service_round_trips_session_run_idempotency_receipts() -> Result<()> {
        let temp = tempdir()?;
        let store = FileDaemonStore::new(temp.path());
        let service = SessionService::new(
            Arc::new(FileSessionStore::new(temp.path().join("sessions"))),
            store.clone(),
            SessionIndex::default(),
            AtomicU64::new(0),
        );

        let key = "session-run:session-1:key-hash";
        let reserved = service
            .begin_session_run_idempotency(key, "fingerprint-1", || "run-1".to_string())
            .await?;
        assert_eq!(
            reserved,
            SessionRunIdempotencyReservation::Reserved {
                run_id: "run-1".to_string()
            }
        );
        assert_eq!(
            service
                .begin_session_run_idempotency(key, "fingerprint-1", || "run-unused".to_string())
                .await?,
            SessionRunIdempotencyReservation::Pending {
                run_id: "run-1".to_string()
            }
        );
        assert!(
            service
                .begin_session_run_idempotency(key, "fingerprint-2", || "run-unused".to_string())
                .await
                .is_err()
        );

        service
            .remember_session_run_idempotency(key, "run-1", "fingerprint-1")
            .await?;
        assert_eq!(
            service
                .begin_session_run_idempotency(key, "fingerprint-1", || "run-unused".to_string())
                .await?,
            SessionRunIdempotencyReservation::Existing {
                run_id: "run-1".to_string()
            }
        );
        assert_eq!(
            store
                .load_index()?
                .session_run_idempotency_receipts
                .get(key)
                .map(|receipt| receipt.run_id().to_string()),
            Some("run-1".to_string())
        );

        service.forget_session_run_idempotency(key).await?;
        assert!(service.session_run_idempotency_receipt(key).await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn session_service_round_trips_run_operation_idempotency_receipts() -> Result<()> {
        let temp = tempdir()?;
        let store = FileDaemonStore::new(temp.path());
        let service = SessionService::new(
            Arc::new(FileSessionStore::new(temp.path().join("sessions"))),
            store.clone(),
            SessionIndex::default(),
            AtomicU64::new(0),
        );

        let key = "run-op:approval:session-1:key-hash";
        let reserved = service
            .begin_run_operation_idempotency(key, "run-1", "fingerprint-1")
            .await?;
        assert_eq!(
            reserved,
            SessionRunIdempotencyReservation::Reserved {
                run_id: "run-1".to_string()
            }
        );
        assert_eq!(
            service
                .begin_run_operation_idempotency(key, "run-unused", "fingerprint-1")
                .await?,
            SessionRunIdempotencyReservation::Pending {
                run_id: "run-1".to_string()
            }
        );
        assert!(
            service
                .begin_run_operation_idempotency(key, "run-unused", "fingerprint-2")
                .await
                .is_err()
        );

        service
            .remember_run_operation_idempotency(key, "run-1", "fingerprint-1")
            .await?;
        assert_eq!(
            service
                .begin_run_operation_idempotency(key, "run-unused", "fingerprint-1")
                .await?,
            SessionRunIdempotencyReservation::Existing {
                run_id: "run-1".to_string()
            }
        );
        assert_eq!(
            store
                .load_index()?
                .run_operation_idempotency_receipts
                .get(key)
                .map(|receipt| receipt.run_id().to_string()),
            Some("run-1".to_string())
        );

        service.forget_run_operation_idempotency(key).await?;
        assert!(
            service
                .run_operation_idempotency_receipt(key)
                .await
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_service_forget_session_removes_colon_session_run_operation_receipts()
    -> Result<()> {
        let temp = tempdir()?;
        let store = FileDaemonStore::new(temp.path());
        let service = SessionService::new(
            Arc::new(FileSessionStore::new(temp.path().join("sessions"))),
            store.clone(),
            SessionIndex::default(),
            AtomicU64::new(0),
        );

        service.remember_session("team:demo", "agent-1").await?;
        service.remember_session("team", "agent-2").await?;
        service
            .begin_session_run_idempotency(
                "session-run:team:demo:key-hash",
                "fingerprint-1",
                || "run-1".to_string(),
            )
            .await?;
        service
            .begin_session_run_idempotency("session-run:team:key-hash", "fingerprint-2", || {
                "run-2".to_string()
            })
            .await?;
        service
            .begin_run_operation_idempotency(
                "run-op:approval:team:demo:key-hash",
                "run-1",
                "fingerprint-1",
            )
            .await?;
        service
            .begin_run_operation_idempotency(
                "run-op:approval:team:key-hash",
                "run-2",
                "fingerprint-2",
            )
            .await?;

        service.forget_session("team").await?;
        let index = store.load_index()?;
        assert!(
            index
                .session_run_idempotency_receipts
                .contains_key("session-run:team:demo:key-hash")
        );
        assert!(
            !index
                .session_run_idempotency_receipts
                .contains_key("session-run:team:key-hash")
        );
        assert!(
            index
                .run_operation_idempotency_receipts
                .contains_key("run-op:approval:team:demo:key-hash")
        );
        assert!(
            !index
                .run_operation_idempotency_receipts
                .contains_key("run-op:approval:team:key-hash")
        );

        service.forget_session("team:demo").await?;
        let index = store.load_index()?;
        assert!(
            !index
                .session_run_idempotency_receipts
                .contains_key("session-run:team:demo:key-hash")
        );
        assert!(
            !index
                .run_operation_idempotency_receipts
                .contains_key("run-op:approval:team:demo:key-hash")
        );
        Ok(())
    }
}
