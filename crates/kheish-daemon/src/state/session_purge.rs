//! Purging ended sessions implemented on [`DaemonState`].
//!
//! A purge removes every durable trace one ended session leaves in the state
//! root: its journal and metadata sidecars, its persisted daemon outputs, its
//! run records and run events, its recovered run memories and debug
//! artifacts, and its session-scoped learnings and learning candidates. The
//! signed external-action ledger is append-only and kept.
//!
//! The purge is crash-safe. A durable plan is written first; the session then
//! leaves the session index and the supervisor topology in one persisted step,
//! so listings never see a half-deleted session; only after that are files
//! deleted, and the plan is removed last. A daemon that stops anywhere in
//! between finishes the purge from the plan at its next boot.

use crate::problems::DaemonProblem;

use super::*;

/// Directory under the state root holding one plan per purge in progress.
const SESSION_PURGE_DIR: &str = "session-purges";
/// Session metadata key recording that `POST /v1/sessions/{id}/end` ran.
pub(crate) const SESSION_ENDED_METADATA_KEY: &str = "session_ended";
/// Supervisor audit event recorded for every purge.
const SESSION_PURGED_AUDIT_EVENT: &str = "session_purged";

/// Durable record of when and why a session was ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionEndedMarker {
    pub(crate) ended_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

/// Durable plan of one purge in progress.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionPurgePlan {
    pub(crate) session_id: String,
    pub(crate) agent_id: String,
    pub(crate) reason: String,
    pub(crate) requested_at_ms: u64,
}

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    /// Records that one session was ended, so it may later be purged.
    pub(crate) async fn record_session_ended(
        &self,
        session_id: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        self.session_service
            .save_session_metadata_value(
                session_id,
                SESSION_ENDED_METADATA_KEY,
                serde_json::to_value(SessionEndedMarker {
                    ended_at_ms: now_ms(),
                    reason: reason.map(str::to_string),
                })?,
            )
            .await
    }

    async fn session_ended_marker(&self, session_id: &str) -> Result<Option<SessionEndedMarker>> {
        self.session_service
            .load_session_metadata_value(session_id, SESSION_ENDED_METADATA_KEY)
            .await?
            .map(serde_json::from_value)
            .transpose()
            .map_err(Into::into)
    }

    /// Purges one ended, idle session and everything it left on disk.
    pub(crate) async fn purge_session(
        self: &Arc<Self>,
        session_id: &str,
        reason: Option<String>,
    ) -> Result<crate::SessionPurgeView> {
        let _purge = self.session_purge_lock.lock().await;
        let agent_id = self.agent_id_for_session(session_id).await?;
        if self.session_ended_marker(session_id).await?.is_none() {
            return Err(DaemonProblem::conflict(
                "sessions",
                "session_not_ended",
                format!(
                    "session {session_id} has not been ended; end it with POST /v1/sessions/{session_id}/end before purging it"
                ),
            )
            .into());
        }
        let descendants = self.supervisor.descendants_of(&agent_id);
        if !descendants.is_empty() {
            return Err(DaemonProblem::conflict(
                "sessions",
                "session_has_descendants",
                format!(
                    "session {session_id} still has {} child agent(s); purge their sessions first",
                    descendants.len()
                ),
            )
            .into());
        }
        self.reject_session_project_dependencies(session_id)
            .await
            .map_err(|error| {
                DaemonProblem::conflict("sessions", "session_has_dependencies", error.to_string())
            })?;
        let live_schedules = self
            .schedule_service
            .list_schedules(Some(session_id))
            .await
            .into_iter()
            .filter(|schedule| !schedule.status.is_terminal())
            .map(|schedule| schedule.schedule_id)
            .collect::<Vec<_>>();
        if !live_schedules.is_empty() {
            return Err(DaemonProblem::conflict(
                "sessions",
                "session_has_dependencies",
                format!(
                    "session {session_id} is still targeted by schedules {}",
                    live_schedules.join(", ")
                ),
            )
            .into());
        }
        if !self
            .session_is_idle_for_topology_mutation(session_id)
            .await?
        {
            return Err(DaemonProblem::session_busy(format!(
                "session {session_id} has non-terminal work or live descendants; purge it once its runs settle"
            ))
            .into());
        }
        let plan = SessionPurgePlan {
            session_id: session_id.to_string(),
            agent_id: agent_id.0.clone(),
            reason: reason
                .map(|reason| reason.trim().to_string())
                .filter(|reason| !reason.is_empty())
                .unwrap_or_else(|| "purged through the control plane".to_string()),
            requested_at_ms: now_ms(),
        };
        // The idle guard keeps any run from starting while the session leaves
        // the topology; once it has left, no run can target it any more.
        self.run_service
            .with_session_idle_guard(session_id, || async {
                self.save_session_purge_plan(&plan)?;
                self.detach_purged_session(&plan).await
            })
            .await
            .map_err(|error| {
                if error.to_string().contains("has active or queued runs") {
                    DaemonProblem::session_busy(format!(
                        "session {session_id} has non-terminal work; purge it once its runs settle"
                    ))
                    .into()
                } else {
                    error
                }
            })?;
        self.finish_session_purge(&plan).await
    }

    /// Finishes every purge a previous daemon life left unfinished.
    pub(crate) async fn resume_pending_session_purges(self: &Arc<Self>) {
        let plans = match self.pending_session_purge_plans() {
            Ok(plans) => plans,
            Err(error) => {
                warn!(error = %format!("{error:#}"), "failed to read pending session purges");
                return;
            }
        };
        for plan in plans {
            let result = async {
                self.detach_purged_session(&plan).await?;
                self.finish_session_purge(&plan).await
            }
            .await;
            match result {
                Ok(view) => info!(
                    session_id = %view.session_id,
                    runs = view.run_ids.len(),
                    "finished a session purge interrupted by a restart"
                ),
                Err(error) => warn!(
                    session_id = %plan.session_id,
                    error = %format!("{error:#}"),
                    "failed to finish an interrupted session purge; it is retried at the next boot"
                ),
            }
        }
    }

    /// Takes the session out of the index and the supervisor topology in one
    /// persisted step, and audits the purge. Idempotent.
    async fn detach_purged_session(&self, plan: &SessionPurgePlan) -> Result<()> {
        let agent_id = AgentId(plan.agent_id.clone());
        if let Some(record) = self.supervisor.get(&agent_id) {
            self.supervisor.record_lifecycle_event(
                SESSION_PURGED_AUDIT_EVENT,
                &record,
                Some(plan.reason.as_str()),
            );
            let _ = self.orchestrator.close_runtime(&agent_id);
            self.supervisor.remove_agent(&agent_id);
        }
        self.session_service
            .forget_session(&plan.session_id)
            .await?;
        self.persist_topology().await
    }

    /// Deletes everything the detached session left on disk, then the plan.
    /// Idempotent: missing files are not errors.
    async fn finish_session_purge(
        &self,
        plan: &SessionPurgePlan,
    ) -> Result<crate::SessionPurgeView> {
        let detached = self
            .run_service
            .detach_session_runs(&plan.session_id)
            .await?;
        let run_ids = detached
            .iter()
            .map(|record| record.view.run_id.clone())
            .collect::<Vec<_>>();
        self.run_service.retire_run_ids(&run_ids)?;
        for run_id in &run_ids {
            self.run_service.delete_run_artifacts(run_id)?;
        }
        let (learning_ids, learning_candidate_ids) = self
            .learning_service
            .purge_session_scope(&plan.session_id)
            .await?;
        self.store.delete_outputs(&plan.session_id)?;
        self.session_service.delete_session_file(&plan.session_id)?;
        self.remove_session_purge_plan(&plan.session_id)?;
        let purged_at_ms = now_ms();
        info!(
            session_id = %plan.session_id,
            agent_id = %plan.agent_id,
            runs = run_ids.len(),
            learnings = learning_ids.len(),
            learning_candidates = learning_candidate_ids.len(),
            reason = %plan.reason,
            "purged daemon session"
        );
        Ok(crate::SessionPurgeView {
            session_id: plan.session_id.clone(),
            agent_id: plan.agent_id.clone(),
            purged_at_ms,
            run_ids,
            learning_ids,
            learning_candidate_ids,
        })
    }

    fn session_purge_dir(&self) -> PathBuf {
        self.store.root().join(SESSION_PURGE_DIR)
    }

    fn save_session_purge_plan(&self, plan: &SessionPurgePlan) -> Result<()> {
        let path = kheish_session::prepare_storage_path_for_write(
            &self.session_purge_dir(),
            &plan.session_id,
            "json",
        )?;
        kheish_session::write_json_pretty_atomically(&path, plan)
            .with_context(|| format!("failed to write session purge plan {}", path.display()))
    }

    fn remove_session_purge_plan(&self, session_id: &str) -> Result<()> {
        let path = kheish_session::resolve_storage_path_for_read(
            &self.session_purge_dir(),
            session_id,
            "json",
        );
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("failed to remove session purge plan {}", path.display())),
        }
    }

    fn pending_session_purge_plans(&self) -> Result<Vec<SessionPurgePlan>> {
        let root = self.session_purge_dir();
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut plans = Vec::new();
        let mut pending = vec![root];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir)
                .with_context(|| format!("failed to read {}", dir.display()))?
            {
                let path = entry?.path();
                if path.is_dir() {
                    pending.push(path);
                } else if path
                    .extension()
                    .is_some_and(|extension| extension == "json")
                {
                    match crate::state_files::read_json_or_quarantine::<SessionPurgePlan>(
                        &path,
                        "session purge plan",
                    )? {
                        Some(plan) => plans.push(plan),
                        None => continue,
                    }
                }
            }
        }
        plans.sort_by_key(|plan| plan.requested_at_ms);
        Ok(plans)
    }
}
