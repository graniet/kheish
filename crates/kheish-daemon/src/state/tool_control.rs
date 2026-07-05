//! Daemon tool-control adapter implemented separately from core daemon state.

use super::*;
use async_trait::async_trait;
use kheish_agent::{AgentId, AgentSupervisor};

/// Bridges the daemon control-plane state into the tool-control interface.
pub(crate) struct DaemonToolControlAdapter<M>(pub(crate) Arc<DaemonState<M>>);

fn agent_is_visible_to_caller(
    supervisor: &AgentSupervisor,
    caller_agent_id: &str,
    target_agent_id: &str,
) -> Result<bool> {
    let caller = AgentId(caller_agent_id.to_string());
    let target = AgentId(target_agent_id.to_string());
    supervisor.shares_root_with(&caller, &target)
}

#[async_trait]
impl<M> DaemonToolControl for DaemonToolControlAdapter<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    async fn spawn_agent(
        &self,
        parent_agent_id: &str,
        request: SpawnAgentToolRequest,
    ) -> Result<SpawnAgentToolResponse> {
        let team_name = request.team_name.clone();
        let isolation = request.isolation.clone().unwrap_or_default();
        let view = self
            .0
            .spawn_sidechain(parent_agent_id, sidechain_request_from_tool(&request)?)
            .await?;
        let launch_run_id = self
            .0
            .list_runs(Some(&view.session_id))
            .await?
            .into_iter()
            .find(|run| !run.status.is_terminal())
            .map(|run| run.run_id);
        Ok(SpawnAgentToolResponse {
            agent_id: view.agent_id,
            session_id: view.session_id,
            status: format!("{:?}", view.snapshot.agent.status).to_ascii_lowercase(),
            run_in_background: true,
            launch_run_id,
            final_output: None,
            team_name,
            isolation,
            name: view.snapshot.agent.name.clone(),
            path: view.snapshot.agent.path.clone(),
            nickname: view.snapshot.agent.nickname.clone(),
            retention: Some(view.snapshot.agent.retention.clone()),
            snapshot: None,
        })
    }

    async fn message_agent(
        &self,
        from_agent_id: &str,
        request: MessageAgentToolRequest,
    ) -> Result<PostMailboxResponse> {
        anyhow::ensure!(
            agent_is_visible_to_caller(&self.0.supervisor, from_agent_id, &request.agent_id)?,
            "agent {} is outside the caller's agent lineage",
            request.agent_id
        );
        self.0
            .post_mailbox(mailbox_request_from_tool(from_agent_id, request)?)
            .await
    }

    async fn latest_run_output(&self, run_id: &str) -> Result<Option<String>> {
        let run = self.0.run_service.run_record(run_id).await?;
        Ok(run
            .view
            .outputs
            .iter()
            .rev()
            .find_map(|output| (!output.content.trim().is_empty()).then(|| output.content.clone())))
    }

    async fn request_parent_clarification(
        &self,
        session_id: &str,
        requester_agent_id: &str,
        requester_run_id: Option<&str>,
        requester_tool_call_id: Option<&str>,
        request: UserQuestionRequest,
    ) -> Result<control_tools::ParentClarificationToolResponse> {
        self.0
            .request_parent_clarification(
                session_id,
                requester_agent_id,
                requester_run_id,
                requester_tool_call_id,
                request,
            )
            .await
    }

    async fn load_session_operator_config(
        &self,
        session_id: &str,
    ) -> Result<kheish_types::SessionOperatorConfig> {
        self.0.load_session_operator_config(session_id).await
    }

    async fn notify_operator(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        request: control_tools::OperatorNotificationRequest,
    ) -> Result<control_tools::OperatorNotificationToolResponse> {
        self.0
            .emit_operator_notification(session_id, run_id, request)
            .await
    }

    async fn wait_agent(
        &self,
        caller_agent_id: &str,
        agent_id: &str,
        timeout: std::time::Duration,
    ) -> Result<ManagedAgentSnapshot> {
        wait_for_agent_snapshot(self, caller_agent_id, agent_id, timeout).await
    }

    async fn list_agents(&self, caller_agent_id: &str) -> Result<Vec<ManagedAgentSnapshot>> {
        let mut agents = Vec::new();
        for record in self
            .0
            .supervisor
            .root_tree_records(&AgentId(caller_agent_id.to_string()))?
        {
            agents.push(self.0.live_snapshot(&record.id).await?);
        }
        Ok(agents)
    }

    async fn list_agent_summaries(&self, caller_agent_id: &str) -> Result<Vec<AgentSummaryView>> {
        self.0
            .list_agent_summaries_for_root(&AgentId(caller_agent_id.to_string()))
            .await
    }

    async fn get_agent(
        &self,
        caller_agent_id: &str,
        agent_id: &str,
    ) -> Result<ManagedAgentSnapshot> {
        anyhow::ensure!(
            agent_is_visible_to_caller(&self.0.supervisor, caller_agent_id, agent_id)?,
            "agent {agent_id} is outside the caller's agent lineage"
        );
        self.0.live_snapshot(&AgentId(agent_id.to_string())).await
    }

    async fn load_assistant_message(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Option<String>> {
        let stored = self.0.session_service.load_session(session_id).await?;
        Ok(stored.journal.iter().find_map(|entry| match &entry.event {
            kheish_types::SessionEvent::MessageAppended { message }
                if message.id == message_id
                    && matches!(message.role, kheish_types::Role::Assistant) =>
            {
                Some(message.content.clone())
            }
            _ => None,
        }))
    }

    async fn list_skills(&self, query: Option<&str>) -> Result<Vec<kheish_skills::SkillSummary>> {
        Ok(self.0.skills.search(query))
    }

    async fn get_skill(&self, name: &str) -> Result<Option<kheish_skills::SkillDefinition>> {
        Ok(self.0.skills.get(name))
    }

    async fn get_learning_skill(&self, name: &str) -> Result<Option<crate::LearningSkillView>> {
        Ok(self.0.learning_skill_service.get(name).await)
    }

    async fn store_workspace_asset(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        tool_call_id: Option<&str>,
        path: &str,
        label: Option<&str>,
        media_type: Option<&str>,
    ) -> Result<crate::AssetView> {
        self.0
            .store_workspace_asset(session_id, run_id, tool_call_id, path, label, media_type)
            .await
    }

    async fn load_asset_attachment(
        &self,
        _session_id: &str,
        asset_id: &str,
    ) -> Result<AttachmentRef> {
        self.0.load_asset_attachment(asset_id).await
    }

    async fn read_channel_thread(
        &self,
        session_id: &str,
        channel_id: &str,
        thread_root_message_id: &str,
    ) -> Result<Vec<crate::ChannelMessageView>> {
        anyhow::ensure!(
            self.0
                .channel_service
                .channel_has_session(channel_id, session_id)
                .await,
            "session {session_id} is not a member of channel {channel_id}"
        );
        self.0
            .list_channel_messages(channel_id, Some(thread_root_message_id), None, None)
            .await
    }

    async fn set_channel_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        actor_id: &str,
        emoji: &str,
    ) -> Result<crate::ChannelMessageView> {
        self.0
            .set_channel_reaction(
                channel_id,
                message_id,
                crate::SetChannelReactionRequest {
                    actor_id: actor_id.to_string(),
                    emoji: emoji.to_string(),
                },
            )
            .await
    }

    async fn agent_list_project_tasks(
        &self,
        session_id: &str,
        project_id: Option<&str>,
        status: Option<kheish_types::TaskStatus>,
    ) -> Result<Vec<crate::ProjectTaskView>> {
        self.0
            .agent_list_project_tasks(session_id, project_id, status)
            .await
    }

    async fn agent_claim_project_task(
        &self,
        session_id: &str,
        run_id: &str,
        project_id: &str,
        task_id: &str,
    ) -> Result<crate::ProjectTaskView> {
        self.0
            .agent_claim_project_task(session_id, run_id, project_id, task_id)
            .await
    }

    async fn agent_update_project_task(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        project_id: &str,
        task_id: &str,
        status: Option<kheish_types::TaskStatus>,
        output: Option<String>,
    ) -> Result<crate::ProjectTaskView> {
        self.0
            .agent_update_project_task(session_id, run_id, project_id, task_id, status, output)
            .await
    }

    async fn agent_create_project_task(
        &self,
        session_id: &str,
        project_id: &str,
        title: String,
        description: String,
        blocked_by: Vec<String>,
        parent_task_id: Option<String>,
        assign_to_self: bool,
    ) -> Result<crate::ProjectTaskView> {
        self.0
            .agent_create_project_task(
                session_id,
                project_id,
                title,
                description,
                blocked_by,
                parent_task_id,
                assign_to_self,
            )
            .await
    }

    async fn create_channel_stimulus(
        &self,
        session_id: &str,
        channel_id: &str,
        mut request: crate::CreateChannelStimulusRequest,
    ) -> Result<crate::ChannelStimulusView> {
        anyhow::ensure!(
            self.0
                .channel_service
                .channel_has_session(channel_id, session_id)
                .await,
            "session {session_id} is not a member of channel {channel_id}"
        );
        if request.sender_session_id.is_none() {
            request.sender_session_id = Some(session_id.to_string());
        }
        self.0.create_channel_stimulus(&channel_id, request).await
    }

    async fn generate_image(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        tool_call_id: Option<&str>,
        request: GenerateImageToolRequest,
    ) -> Result<GenerateImageToolResponse> {
        self.0
            .generate_image(session_id, run_id, tool_call_id, request)
            .await
    }

    async fn generate_audio(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        tool_call_id: Option<&str>,
        request: GenerateAudioToolRequest,
    ) -> Result<GenerateAudioToolResponse> {
        self.0
            .generate_audio(session_id, run_id, tool_call_id, request)
            .await
    }

    async fn edit_image(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        tool_call_id: Option<&str>,
        request: EditImageToolRequest,
    ) -> Result<EditImageToolResponse> {
        self.0
            .edit_image(session_id, run_id, tool_call_id, request)
            .await
    }

    async fn start_background_shell_task(
        &self,
        session_id: &str,
        owner_agent_id: &str,
        request: BackgroundShellTaskRequest,
    ) -> Result<kheish_types::TaskRecord> {
        self.0
            .start_background_shell_task(session_id, owner_agent_id, request)
            .await
    }

    async fn run_foreground_shell_task(
        &self,
        session_id: &str,
        owner_agent_id: &str,
        request: BackgroundShellTaskRequest,
    ) -> Result<ToolExecutionOutput> {
        self.0
            .run_foreground_shell_task(session_id, owner_agent_id, request)
            .await
    }

    async fn task_output_view(
        &self,
        session_id: &str,
        task_id: &str,
        wait: bool,
        timeout: Duration,
        tail_bytes: usize,
        include_full_output: bool,
    ) -> Result<TaskOutputView> {
        self.0
            .task_output_view(
                session_id,
                task_id,
                wait,
                timeout,
                tail_bytes,
                include_full_output,
            )
            .await
    }

    async fn stop_task(
        &self,
        session_id: &str,
        task_id: &str,
        reason: Option<String>,
        actor_agent_id: Option<String>,
    ) -> Result<kheish_types::TaskRecord> {
        self.0
            .stop_session_task(session_id, task_id, reason, actor_agent_id)
            .await
    }

    async fn load_session_control_state(&self, session_id: &str) -> Result<SessionControlState> {
        DaemonState::load_session_control_state(self.0.as_ref(), session_id).await
    }

    async fn save_session_control_state(
        &self,
        session_id: &str,
        state: SessionControlState,
    ) -> Result<SessionControlState> {
        DaemonState::save_session_control_state(self.0.as_ref(), session_id, state).await
    }

    async fn archived_session_task_index(
        &self,
        session_id: &str,
    ) -> Result<std::sync::Arc<crate::services::ArchivedTaskIndex>> {
        self.0.archived_session_task_index(session_id).await
    }

    async fn load_archived_session_tasks(
        &self,
        session_id: &str,
    ) -> Result<Vec<kheish_types::ArchivedTaskRecord>> {
        self.0.load_archived_session_tasks(session_id).await
    }

    async fn delete_session_task(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<kheish_types::TaskRecord> {
        self.0.delete_session_task(session_id, task_id).await
    }

    async fn load_session_goal(
        &self,
        session_id: &str,
    ) -> Result<Option<kheish_types::SessionGoal>> {
        self.0.load_session_goal(session_id).await
    }

    async fn create_session_goal(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        objective: String,
        token_budget: Option<u64>,
        replace_if_inactive: bool,
    ) -> Result<kheish_types::SessionGoal> {
        self.0
            .create_session_goal_from_tool(
                session_id,
                objective,
                token_budget,
                run_id.map(str::to_string),
                replace_if_inactive,
            )
            .await
            .and_then(|response| {
                response
                    .goal
                    .ok_or_else(|| anyhow::anyhow!("goal was not created"))
            })
    }

    async fn complete_session_goal(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<kheish_types::SessionGoal> {
        self.0
            .complete_session_goal_from_run(session_id, run_id)
            .await
            .and_then(|response| {
                response
                    .goal
                    .ok_or_else(|| anyhow::anyhow!("goal was not completed"))
            })
    }

    async fn pause_session_goal(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<kheish_types::SessionGoal> {
        self.0
            .pause_session_goal_from_run(session_id, run_id)
            .await
            .and_then(|response| {
                response
                    .goal
                    .ok_or_else(|| anyhow::anyhow!("goal was not paused"))
            })
    }

    async fn enter_session_plan_mode(&self, session_id: &str) -> Result<SessionControlState> {
        self.0.enter_session_plan_mode(session_id).await
    }

    async fn exit_session_plan_mode(
        &self,
        session_id: &str,
        plan: String,
        summary: Option<String>,
    ) -> Result<control_tools::ExitPlanModeOutcome> {
        self.0
            .exit_session_plan_mode(session_id, plan, summary)
            .await
    }

    async fn create_schedule(&self, request: ScheduleCreateRequest) -> Result<ScheduleView> {
        self.0.create_schedule(request).await
    }

    async fn list_schedules(&self, session_id: Option<&str>) -> Result<Vec<ScheduleView>> {
        self.0.list_schedules(session_id).await
    }

    async fn get_schedule(&self, schedule_id: &str) -> Result<ScheduleView> {
        self.0.get_schedule(schedule_id).await
    }

    async fn cancel_schedule(&self, schedule_id: &str) -> Result<ScheduleView> {
        self.0.cancel_schedule(schedule_id).await
    }

    async fn pause_schedule(&self, schedule_id: &str) -> Result<ScheduleView> {
        self.0.pause_schedule(schedule_id).await
    }

    async fn resume_schedule(&self, schedule_id: &str) -> Result<ScheduleView> {
        self.0.resume_schedule(schedule_id).await
    }

    async fn trigger_schedule_now(&self, schedule_id: &str) -> Result<ScheduleView> {
        self.0.trigger_schedule_now(schedule_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::agent_is_visible_to_caller;
    use anyhow::Result;
    use kheish_agent::{AgentId, AgentSupervisor, ChildRetentionPolicy};
    use kheish_runtime::NoopObserver;
    use kheish_types::ConversationKey;
    use std::sync::Arc;

    #[test]
    fn agent_tree_visibility_allows_same_root_and_rejects_foreign_roots() -> Result<()> {
        let supervisor = AgentSupervisor::new(Arc::new(NoopObserver));
        let root = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "root".to_string(),
                thread_id: None,
            },
            Some("root"),
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        let child = supervisor.spawn(
            Some(root.id.clone()),
            ConversationKey {
                session_id: "child".to_string(),
                thread_id: None,
            },
            Some("child"),
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        let grandchild = supervisor.spawn(
            Some(child.id.clone()),
            ConversationKey {
                session_id: "grandchild".to_string(),
                thread_id: None,
            },
            Some("grandchild"),
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        let sibling = supervisor.spawn(
            Some(root.id.clone()),
            ConversationKey {
                session_id: "sibling".to_string(),
                thread_id: None,
            },
            Some("sibling"),
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        let foreign_root = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "foreign".to_string(),
                thread_id: None,
            },
            Some("foreign"),
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;

        assert!(agent_is_visible_to_caller(
            &supervisor,
            &child.id.0,
            &root.id.0
        )?);
        assert!(agent_is_visible_to_caller(
            &supervisor,
            &child.id.0,
            &grandchild.id.0
        )?);
        assert!(agent_is_visible_to_caller(
            &supervisor,
            &child.id.0,
            &sibling.id.0
        )?);
        assert!(!agent_is_visible_to_caller(
            &supervisor,
            &child.id.0,
            &foreign_root.id.0
        )?);
        assert!(!agent_is_visible_to_caller(
            &supervisor,
            &child.id.0,
            &AgentId("missing".to_string()).0
        )?);
        Ok(())
    }
}
