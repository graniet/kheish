//! Subagent lifecycle methods implemented on [`DaemonState`].

use super::*;

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(super) fn passive_snapshot_for_record(
        record: kheish_agent::AgentRecord,
    ) -> ManagedAgentSnapshot {
        ManagedAgentSnapshot {
            agent: record,
            pending_approvals: Vec::new(),
            pending_questions: Vec::new(),
            last_assistant_message: None,
            journal_len: 0,
            checkpoint_len: 0,
            last_error: None,
        }
    }

    pub(super) fn clear_run_spawn_count(&self, run_id: &str) {
        self.subagent_service.clear_run_spawn_count(run_id);
    }

    pub(super) async fn effective_spawn_run_id(
        &self,
        parent: &AgentId,
        requested_run_id: Option<&str>,
    ) -> Result<Option<String>> {
        let parent_record = self
            .supervisor
            .get(parent)
            .ok_or_else(|| anyhow!("unknown agent {}", parent.0))?;
        let active_run_id = self
            .run_service
            .active_run_id(&parent_record.conversation.session_id)
            .await;
        if let Some(requested_run_id) = requested_run_id {
            anyhow::ensure!(
                active_run_id.as_deref() == Some(requested_run_id),
                "spawned_by_run_id must match the active parent run"
            );
        }
        Ok(active_run_id)
    }

    pub(super) fn count_spawned_children_for_run(&self, run_id: &str) -> usize {
        self.supervisor
            .list()
            .into_iter()
            .filter(|record| record.spawned_by_run_id.as_deref() == Some(run_id))
            .count()
    }

    pub(super) fn reserve_subagent_spawn(
        &self,
        parent: &AgentId,
        effective_run_id: Option<&str>,
        request_key: Option<&str>,
        conversation_key: &str,
        enforce_policy_limits: bool,
    ) -> Result<SpawnReservation> {
        self.subagent_service.reserve_subagent_spawn(
            &self.supervisor,
            parent,
            effective_run_id,
            request_key,
            conversation_key,
            enforce_policy_limits,
            &self.subagent_policy,
            |agent_id| self.orchestrator.has_runtime(agent_id),
            |run_id| self.count_spawned_children_for_run(run_id),
        )
    }

    pub(super) fn reserve_subagent_spawn_request(
        &self,
        request_key: &str,
    ) -> Result<SpawnRequestReservation> {
        self.subagent_service.reserve_spawn_request(request_key)
    }

    pub(super) fn release_subagent_spawn_request_reservation(
        &self,
        reservation: SpawnRequestReservation,
    ) {
        self.subagent_service
            .release_spawn_request_reservation(reservation);
    }

    pub(super) fn reserve_subagent_spawn_with_held_request(
        &self,
        parent: &AgentId,
        effective_run_id: Option<&str>,
        request_key: &str,
        conversation_key: &str,
        enforce_policy_limits: bool,
    ) -> Result<SpawnReservation> {
        self.subagent_service
            .reserve_subagent_spawn_with_held_request(
                &self.supervisor,
                parent,
                effective_run_id,
                request_key,
                conversation_key,
                enforce_policy_limits,
                &self.subagent_policy,
                |agent_id| self.orchestrator.has_runtime(agent_id),
                |run_id| self.count_spawned_children_for_run(run_id),
            )
    }

    pub(super) fn reserve_subagent_spawn_with_policy(
        &self,
        parent: &AgentId,
        effective_run_id: Option<&str>,
        request_key: Option<&str>,
        conversation_key: &str,
        enforce_policy_limits: bool,
        policy_scope: &crate::SubagentPolicyScopeView,
        estimate: &crate::SubagentPolicyEstimateView,
        request_fingerprint: &str,
    ) -> Result<SpawnReservation> {
        self.subagent_service.reserve_subagent_spawn_with_policy(
            &self.supervisor,
            parent,
            effective_run_id,
            request_key,
            conversation_key,
            enforce_policy_limits,
            &self.subagent_policy,
            policy_scope,
            estimate,
            request_fingerprint,
            |agent_id| self.orchestrator.has_runtime(agent_id),
            |run_id| self.count_spawned_children_for_run(run_id),
        )
    }

    pub(super) fn reserve_subagent_spawn_with_policy_and_held_request(
        &self,
        parent: &AgentId,
        effective_run_id: Option<&str>,
        request_key: &str,
        conversation_key: &str,
        enforce_policy_limits: bool,
        policy_scope: &crate::SubagentPolicyScopeView,
        estimate: &crate::SubagentPolicyEstimateView,
        request_fingerprint: &str,
    ) -> Result<SpawnReservation> {
        self.subagent_service
            .reserve_subagent_spawn_with_policy_and_held_request(
                &self.supervisor,
                parent,
                effective_run_id,
                request_key,
                conversation_key,
                enforce_policy_limits,
                &self.subagent_policy,
                policy_scope,
                estimate,
                request_fingerprint,
                |agent_id| self.orchestrator.has_runtime(agent_id),
                |run_id| self.count_spawned_children_for_run(run_id),
            )
    }

    pub(super) fn release_subagent_spawn_reservation(&self, reservation: SpawnReservation) {
        self.subagent_service.release_spawn_reservation(reservation);
    }

    pub(crate) async fn archive_settled_subagents_on_boot(self: &Arc<Self>) -> Result<()> {
        let reconciled = self.reconcile_supervisor_terminal_runs_on_boot().await?;
        let force_closed = self
            .force_close_failed_terminal_run_descendants_on_boot()
            .await?;
        let _ = self.reap_close_on_settle_agents().await?;
        if reconciled > 0 || force_closed > 0 {
            info!(
                reconciled_agents = reconciled,
                force_closed_descendants = force_closed,
                "reconciled supervisor topology from terminal runs during daemon startup"
            );
        }
        Ok(())
    }

    async fn latest_run_records_by_agent(&self) -> BTreeMap<AgentId, RunRecord> {
        let mut latest = BTreeMap::<AgentId, RunRecord>::new();
        for record in self.run_service.run_records_snapshot().await.into_values() {
            let agent_id = AgentId(record.view.agent_id.clone());
            let replace = latest
                .get(&agent_id)
                .map(|existing| run_record_is_newer(&record, existing))
                .unwrap_or(true);
            if replace {
                latest.insert(agent_id, record);
            }
        }
        latest
    }

    async fn reconcile_supervisor_terminal_runs_on_boot(self: &Arc<Self>) -> Result<usize> {
        let latest = self.latest_run_records_by_agent().await;
        let mut reconciled = 0usize;

        for (agent_id, record) in latest {
            if !record.view.status.is_terminal() {
                continue;
            }
            let Some(agent) = self.supervisor.get(&agent_id) else {
                continue;
            };
            if agent.closed_at_ms.is_some() {
                continue;
            }
            let Some(status) = supervisor_status_for_terminal_run(&record, &agent) else {
                continue;
            };
            let settled_at_ms = matches!(status, AgentStatus::Completed | AgentStatus::Failed)
                .then(|| terminal_run_at_ms(&record));
            let needs_update = agent.status != status
                || (settled_at_ms.is_some() && agent.settled_at_ms.is_none());
            if !needs_update {
                continue;
            }

            let updated = self.supervisor.update_record(&agent_id, |agent| {
                agent.status = status.clone();
                agent.settled_at_ms = settled_at_ms;
                agent.closed_at_ms = None;
            })?;
            self.supervisor.record_lifecycle_event(
                "terminal_run_reconciled",
                &updated,
                Some(&format!(
                    "restored terminal run {} with status {:?}",
                    record.view.run_id, record.view.status
                )),
            );
            reconciled += 1;
        }

        if reconciled > 0 {
            self.persist_topology().await?;
        }
        Ok(reconciled)
    }

    async fn force_close_failed_terminal_run_descendants_on_boot(
        self: &Arc<Self>,
    ) -> Result<usize> {
        let latest = self.latest_run_records_by_agent().await;
        let mut failed_parent_runs = latest
            .into_values()
            .filter(|record| record.view.status == DaemonRunStatus::Failed)
            .collect::<Vec<_>>();
        failed_parent_runs.sort_by(|left, right| {
            let left_id = AgentId(left.view.agent_id.clone());
            let right_id = AgentId(right.view.agent_id.clone());
            let left_depth = self.supervisor.depth_of(&left_id).unwrap_or(0);
            let right_depth = self.supervisor.depth_of(&right_id).unwrap_or(0);
            left_depth
                .cmp(&right_depth)
                .then_with(|| left.view.agent_id.cmp(&right.view.agent_id))
                .then_with(|| {
                    run_id_chronology_key(&left.view.run_id)
                        .cmp(&run_id_chronology_key(&right.view.run_id))
                })
        });

        let mut closed = 0usize;
        for record in failed_parent_runs {
            let reason = format!(
                "parent run {} failed before supervisor reconciliation: {}",
                record.view.run_id,
                record
                    .view
                    .error
                    .as_deref()
                    .unwrap_or("terminal run failed")
            );
            closed += self
                .fail_close_on_settle_descendants(&record.view.agent_id, &reason)
                .await?;
        }
        Ok(closed)
    }

    pub(super) async fn collect_session_run_ids(&self, session_id: &str) -> Vec<String> {
        self.run_service.collect_session_run_ids(session_id).await
    }

    pub(super) async fn has_session_runs(&self, session_id: &str) -> bool {
        self.run_service.has_session_runs(session_id).await
    }

    pub(super) async fn force_close_agent(
        self: &Arc<Self>,
        agent_id: &AgentId,
        final_status: AgentStatus,
        reason: &str,
    ) -> Result<bool> {
        let Some(record) = self.supervisor.get(agent_id) else {
            return Ok(false);
        };
        if record.closed_at_ms.is_some() {
            return Ok(false);
        }

        let run_ids = self
            .collect_session_run_ids(&record.conversation.session_id)
            .await;
        if self.orchestrator.has_runtime(agent_id) {
            let _ = self.orchestrator.interrupt(agent_id).await;
        }
        for run_id in run_ids {
            let _ = self.interrupt_run(&run_id).await;
        }
        self.supervisor
            .dead_letter_mailbox(agent_id, &format!("agent closed: {reason}"));

        let mut snapshot = if self.orchestrator.has_runtime(agent_id) {
            self.orchestrator.snapshot(agent_id).await?
        } else if let Some(snapshot) = self.supervisor.terminal_snapshot(agent_id) {
            snapshot
        } else {
            Self::passive_snapshot_for_record(record.clone())
        };
        let closed_at_ms = now_ms();
        let updated = self.supervisor.update_record(agent_id, |agent| {
            agent.status = final_status.clone();
            agent.settled_at_ms = Some(closed_at_ms);
            agent.closed_at_ms = Some(closed_at_ms);
        })?;
        self.supervisor
            .record_lifecycle_event("agent_closed", &updated, Some(reason));
        if reason == "close_on_settle" {
            self.supervisor.record_lifecycle_event(
                "close_on_settle_reaped",
                &updated,
                Some(reason),
            );
        }
        let _ = self
            .end_session(&updated.conversation.session_id, Some(reason.to_string()))
            .await;
        let _ = self.orchestrator.close_runtime(agent_id);
        snapshot.agent = updated;
        snapshot.last_error = Some(reason.to_string());
        self.supervisor.record_terminal_snapshot(snapshot.clone());
        self.persist_topology().await?;
        self.publish_snapshot_for_session(&snapshot.agent.conversation.session_id, &snapshot);
        Ok(true)
    }

    pub(super) async fn fail_close_on_settle_descendants(
        self: &Arc<Self>,
        parent_agent_id: &str,
        reason: &str,
    ) -> Result<usize> {
        let parent = AgentId(parent_agent_id.to_string());
        let mut descendants = self
            .supervisor
            .descendants_of(&parent)
            .into_iter()
            .filter(|record| {
                record.retention == ChildRetentionPolicy::CloseOnSettle
                    && record.closed_at_ms.is_none()
            })
            .collect::<Vec<_>>();
        descendants.sort_by(|left, right| {
            let left_depth = self.supervisor.depth_of(&left.id).unwrap_or(0);
            let right_depth = self.supervisor.depth_of(&right.id).unwrap_or(0);
            right_depth
                .cmp(&left_depth)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.id.cmp(&right.id))
        });

        let mut closed = 0usize;
        for descendant in descendants {
            if self
                .force_close_agent(&descendant.id, AgentStatus::Failed, reason)
                .await?
            {
                if let Some(record) = self.supervisor.get(&descendant.id) {
                    self.supervisor.record_lifecycle_event(
                        "descendant_force_closed",
                        &record,
                        Some(reason),
                    );
                    self.persist_topology().await?;
                }
                closed += 1;
            }
        }
        Ok(closed)
    }

    pub(super) async fn can_close_agent(&self, agent: &kheish_agent::AgentRecord) -> Result<bool> {
        if agent.retention != ChildRetentionPolicy::CloseOnSettle || agent.closed_at_ms.is_some() {
            return Ok(false);
        }
        if !self.orchestrator.has_runtime(&agent.id) {
            return Ok(false);
        }
        if !matches!(agent.status, AgentStatus::Completed | AgentStatus::Failed) {
            return Ok(false);
        }
        let has_background_work = self
            .run_service
            .session_state(&agent.conversation.session_id)
            .await
            .map(|state| state.active_run_id.is_some() || !state.queued_run_ids.is_empty())
            .unwrap_or(false);
        if has_background_work {
            return Ok(false);
        }
        if self.supervisor.mailbox_len(&agent.id) > 0 {
            return Ok(false);
        }
        if self
            .supervisor
            .descendants_of(&agent.id)
            .into_iter()
            .any(|record| self.orchestrator.has_runtime(&record.id))
        {
            return Ok(false);
        }
        let snapshot = self.orchestrator.snapshot(&agent.id).await?;
        Ok(snapshot.pending_approvals.is_empty() && snapshot.pending_questions.is_empty())
    }

    pub(super) async fn close_agent_runtime(self: &Arc<Self>, agent_id: &AgentId) -> Result<bool> {
        let Some(record) = self.supervisor.get(agent_id) else {
            return Ok(false);
        };
        self.force_close_agent(agent_id, record.status, "close_on_settle")
            .await
    }

    pub(crate) async fn reap_close_on_settle_agents(self: &Arc<Self>) -> Result<usize> {
        let mut candidates = self
            .supervisor
            .list()
            .into_iter()
            .filter(|record| {
                record.retention == ChildRetentionPolicy::CloseOnSettle
                    && record.closed_at_ms.is_none()
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            let left_depth = self.supervisor.depth_of(&left.id).unwrap_or(0);
            let right_depth = self.supervisor.depth_of(&right.id).unwrap_or(0);
            right_depth
                .cmp(&left_depth)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.id.cmp(&right.id))
        });
        let mut closed = 0usize;
        for agent in candidates {
            if self.can_close_agent(&agent).await? && self.close_agent_runtime(&agent.id).await? {
                closed += 1;
            }
        }
        if closed > 0 {
            info!(
                closed_agents = closed,
                "reaped settled close_on_settle agents"
            );
        }
        Ok(closed)
    }

    pub(crate) async fn restore_registered_agents(&self) -> Result<()> {
        debug!("restoring registered daemon agents");
        self.orchestrator.restore_registered_agents().await
    }
}

fn terminal_run_at_ms(record: &RunRecord) -> u64 {
    record
        .view
        .finished_at_ms
        .unwrap_or(record.view.updated_at_ms)
        .max(record.view.submitted_at_ms)
}

fn run_record_is_newer(candidate: &RunRecord, current: &RunRecord) -> bool {
    (
        terminal_run_at_ms(candidate),
        candidate.view.submitted_at_ms,
        run_id_chronology_key(&candidate.view.run_id),
    ) > (
        terminal_run_at_ms(current),
        current.view.submitted_at_ms,
        run_id_chronology_key(&current.view.run_id),
    )
}

fn run_id_chronology_key(run_id: &str) -> (u8, u64, &str) {
    run_id
        .strip_prefix("run-")
        .and_then(|suffix| suffix.parse::<u64>().ok())
        .map(|sequence| (1, sequence, run_id))
        .unwrap_or((0, 0, run_id))
}

fn supervisor_status_for_terminal_run(
    record: &RunRecord,
    agent: &kheish_agent::AgentRecord,
) -> Option<AgentStatus> {
    match record.view.status {
        DaemonRunStatus::Completed if agent.retention == ChildRetentionPolicy::CloseOnSettle => {
            Some(AgentStatus::Completed)
        }
        DaemonRunStatus::Completed | DaemonRunStatus::Interrupted | DaemonRunStatus::Cancelled => {
            Some(AgentStatus::Idle)
        }
        DaemonRunStatus::Failed => Some(AgentStatus::Failed),
        DaemonRunStatus::Queued
        | DaemonRunStatus::Running
        | DaemonRunStatus::WaitingForApproval
        | DaemonRunStatus::WaitingForUserQuestion => None,
    }
}
