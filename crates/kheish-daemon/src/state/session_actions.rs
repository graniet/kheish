//! Session action methods implemented on [`DaemonState`].

use super::*;

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(crate) async fn recover_run_scheduler_on_boot(self: &Arc<Self>) -> Result<()> {
        let sessions = self.session_service.session_pairs().await;
        let running = self.run_service.running_run_triplets().await;
        let mut snapshots = BTreeMap::new();
        for (_, session_id, agent_id) in &running {
            if sessions.get(session_id) != Some(agent_id) {
                continue;
            }
            if let Ok(snapshot) = self.orchestrator.snapshot(&AgentId(agent_id.clone())).await {
                snapshots.insert(agent_id.clone(), snapshot);
            }
        }
        self.run_service.recover_running_runs(&snapshots).await?;
        self.restore_channel_leases().await
    }

    pub(crate) async fn start_restored_run_scheduler_on_boot(self: &Arc<Self>) -> Result<()> {
        let queued_sessions = self.run_service.session_ids().await;
        for session_id in queued_sessions {
            self.refresh_session_queue(&session_id).await?;
            self.start_next_queued_run(&session_id).await?;
        }
        let mailbox_agents = self
            .supervisor
            .snapshot()
            .mailboxes
            .into_keys()
            .collect::<Vec<_>>();
        for agent_id in mailbox_agents {
            let _ = self.schedule_mailbox_run(&agent_id).await?;
        }
        Ok(())
    }

    pub(crate) async fn submit_input(
        self: &Arc<Self>,
        session_id: &str,
        request: SubmitInputRequest,
    ) -> Result<SessionView> {
        let accepted = self
            .submit_input_run_requiring_idle(session_id, request)
            .await?;
        let settled = self.wait_for_run_settled(&accepted.run_id).await?;
        if settled.status == DaemonRunStatus::Interrupted {
            anyhow::bail!("run interrupted");
        }
        if settled.status == DaemonRunStatus::Cancelled {
            anyhow::bail!("run cancelled");
        }
        if let Some(error) = settled.error.clone() {
            anyhow::bail!(error);
        }
        let agent_id = self.agent_id_for_session(session_id).await?;
        let view = self.session_view(session_id, &agent_id).await?;
        self.publish_snapshot(&view);
        Ok(view)
    }

    pub(crate) async fn resolve_approvals(
        self: &Arc<Self>,
        session_id: &str,
        request: ResolveApprovalsRequest,
    ) -> Result<SessionView> {
        let accepted = self.resolve_approval_run(session_id, request).await?;
        let settled = self.wait_for_run_settled(&accepted.run_id).await?;
        if settled.status == DaemonRunStatus::Interrupted {
            anyhow::bail!("run interrupted");
        }
        if settled.status == DaemonRunStatus::Cancelled {
            anyhow::bail!("run cancelled");
        }
        if let Some(error) = settled.error.clone() {
            anyhow::bail!(error);
        }
        let agent_id = self.agent_id_for_session(session_id).await?;
        let view = self.session_view(session_id, &agent_id).await?;
        self.publish_snapshot(&view);
        Ok(view)
    }

    pub(crate) async fn resolve_user_question_for_session(
        self: &Arc<Self>,
        session_id: &str,
        request: ResolveUserQuestionRequest,
    ) -> Result<SessionView> {
        let accepted = self.resolve_user_question(session_id, request).await?;
        let settled = self.wait_for_run_settled(&accepted.run_id).await?;
        if settled.status == DaemonRunStatus::Interrupted {
            anyhow::bail!("run interrupted");
        }
        if settled.status == DaemonRunStatus::Cancelled {
            anyhow::bail!("run cancelled");
        }
        if let Some(error) = settled.error.clone() {
            anyhow::bail!(error);
        }
        let agent_id = self.agent_id_for_session(session_id).await?;
        let view = self.session_view(session_id, &agent_id).await?;
        self.publish_snapshot(&view);
        Ok(view)
    }
}
