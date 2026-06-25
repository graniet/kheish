use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use kheish_agent::{AgentId, AgentRecord, AgentStatus, ChildRetentionPolicy, ManagedAgentSnapshot};
use kheish_runtime::{
    PermissionMode, PromptMergeMode, SandboxProfile, Tool, ToolContext, ToolExecutionOutput,
};
use kheish_skills::{SkillDefinition, SkillRuntimeConfig, SkillScope, SkillSummary};
use kheish_types::{
    ActorRef, AttachmentRef, ConversationKey, DYNAMIC_MCP_TOOL_ALLOWLIST_SENTINEL,
    ModelGenerationConfig, ReasoningConfig, ReasoningEffort, RichOutput, SessionControlState,
    SessionGoal, SessionGoalStatus, SkillExecutionContext, TaskRecord, TaskStatus,
    UserQuestionRequest,
};
use serde_json::json;

use super::helpers::{USER_QUESTION_INPUT_EXAMPLE, built_in_agent_profile};
use super::*;
use crate::scheduler::summarize_schedule_create_request;
use crate::shell_tasks::{BackgroundShellTaskRequest, TaskOutputView};
use crate::{PostMailboxResponse, ScheduleCreateRequest, ScheduleStatus, ScheduleView};

#[derive(Default)]
struct FakeControlState {
    spawn_requests: Vec<(String, SpawnAgentToolRequest)>,
    mailbox_requests: Vec<(String, MessageAgentToolRequest)>,
    generate_audio_requests: Vec<(String, Option<String>, GenerateAudioToolRequest)>,
    generate_image_requests: Vec<(String, GenerateImageToolRequest)>,
    edit_image_requests: Vec<(String, EditImageToolRequest)>,
    parent_clarification_requests: Vec<(String, String, UserQuestionRequest)>,
    waited_agents: Vec<String>,
    agents: Vec<ManagedAgentSnapshot>,
    run_outputs: BTreeMap<String, String>,
    assistant_messages: BTreeMap<(String, String), String>,
    skills: BTreeMap<String, SkillDefinition>,
    learning_skills: BTreeMap<String, crate::LearningSkillView>,
    session_control: BTreeMap<String, SessionControlState>,
    session_goals: BTreeMap<String, SessionGoal>,
    session_permission_modes: BTreeMap<String, Option<PermissionMode>>,
    schedules: BTreeMap<String, ScheduleView>,
    task_output_requests: Vec<(String, String, bool, u64, usize, bool)>,
    channel_thread_messages: BTreeMap<(String, String), Vec<crate::ChannelMessageView>>,
    channel_reaction_requests: Vec<(String, String, String, String)>,
    channel_stimulus_requests: Vec<(String, String, crate::CreateChannelStimulusRequest)>,
}

struct FakeControl {
    state: Mutex<FakeControlState>,
}

impl FakeControl {
    fn new() -> Self {
        Self {
            state: Mutex::new(FakeControlState::default()),
        }
    }

    fn context(session_id: &str, agent_id: &str) -> ToolContext {
        ToolContext {
            call_id: "call-1".to_string(),
            sandbox: SandboxProfile::Inherited,
            metadata: json!({
                "session_id": session_id,
                "agent_id": agent_id,
                "assistant_message_id": "assistant-1",
                "tool_call_id": "call-1",
                "workspace_root": std::env::temp_dir().display().to_string(),
            }),
        }
    }

    fn context_with_run(session_id: &str, agent_id: &str, run_id: &str) -> ToolContext {
        let mut context = Self::context(session_id, agent_id);
        context.metadata["run_id"] = json!(run_id);
        context
    }
}

#[async_trait]
impl DaemonToolControl for FakeControl {
    async fn run_foreground_shell_task(
        &self,
        _session_id: &str,
        _owner_agent_id: &str,
        _request: BackgroundShellTaskRequest,
    ) -> Result<ToolExecutionOutput> {
        Ok(ToolExecutionOutput::json(json!({
            "success": true,
            "stdout": "",
            "stderr": "",
            "exit_code": 0,
        })))
    }

    async fn load_session_goal(&self, session_id: &str) -> Result<Option<SessionGoal>> {
        Ok(self
            .state
            .lock()
            .expect("fake control mutex poisoned")
            .session_goals
            .get(session_id)
            .cloned())
    }

    async fn create_session_goal(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        objective: String,
        token_budget: Option<u64>,
    ) -> Result<SessionGoal> {
        let mut state = self.state.lock().expect("fake control mutex poisoned");
        let goal = SessionGoal {
            goal_id: "goal-1".to_string(),
            session_id: session_id.to_string(),
            objective,
            status: SessionGoalStatus::Active,
            token_budget,
            tokens_used: 0,
            time_used_ms: 0,
            created_at_ms: 1,
            updated_at_ms: 1,
            version: 1,
            definition_version: 1,
            created_by_run_id: run_id.map(str::to_string),
            accounted_usage: BTreeMap::new(),
            last_continuation_run_id: None,
            budget_limited_by_run_id: None,
            budget_wrapup_run_id: None,
            completed_by_run_id: None,
        };
        state
            .session_goals
            .insert(session_id.to_string(), goal.clone());
        Ok(goal)
    }

    async fn complete_session_goal(&self, session_id: &str, run_id: &str) -> Result<SessionGoal> {
        let mut state = self.state.lock().expect("fake control mutex poisoned");
        let goal = state
            .session_goals
            .get_mut(session_id)
            .ok_or_else(|| anyhow!("session has no goal"))?;
        goal.status = SessionGoalStatus::Complete;
        goal.completed_by_run_id = Some(run_id.to_string());
        Ok(goal.clone())
    }

    async fn start_background_shell_task(
        &self,
        session_id: &str,
        owner_agent_id: &str,
        request: BackgroundShellTaskRequest,
    ) -> Result<TaskRecord> {
        let mut state = self.state.lock().expect("fake control mutex poisoned");
        let session_state = state
            .session_control
            .entry(session_id.to_string())
            .or_default();
        let command = request.command.clone();
        let task = TaskRecord {
            id: "shell-task-1".to_string(),
            title: request.description,
            description: command.clone(),
            status: TaskStatus::InProgress,
            owner_agent_id: Some(owner_agent_id.to_string()),
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            output: None,
            metadata: json!({
                "kind": "background_shell",
                "command": command,
                "workdir": "/tmp",
                "output_file_path": "/tmp/fake-shell-task.log",
                "tool_call_id": "call-1",
                "created_by_run_id": request.created_by_run_id.clone(),
            }),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        session_state.tasks.push(task.clone());
        Ok(task)
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
        self.state
            .lock()
            .expect("fake control mutex poisoned")
            .task_output_requests
            .push((
                session_id.to_string(),
                task_id.to_string(),
                wait,
                timeout.as_millis() as u64,
                tail_bytes,
                include_full_output,
            ));
        let task = self
            .load_session_control_state(session_id)
            .await?
            .tasks
            .into_iter()
            .find(|task| task.id == task_id)
            .ok_or_else(|| anyhow!("unknown task {task_id}"))?;
        Ok(TaskOutputView {
            retrieval_status: if wait {
                "success".to_string()
            } else {
                "not_ready".to_string()
            },
            task,
            output_file_path: Some("/tmp/fake-shell-task.log".to_string()),
            output_excerpt: None,
            output_text: None,
            output_truncated: false,
            output_size_bytes: None,
            output_total_bytes: None,
            output_rotated: false,
            output_rotation_count: 0,
        })
    }

    async fn stop_task(
        &self,
        session_id: &str,
        task_id: &str,
        reason: Option<String>,
        _actor_agent_id: Option<String>,
    ) -> Result<TaskRecord> {
        let mut state = self.state.lock().expect("fake control mutex poisoned");
        let task = state
            .session_control
            .entry(session_id.to_string())
            .or_default()
            .tasks
            .iter_mut()
            .find(|task| task.id == task_id)
            .ok_or_else(|| anyhow!("unknown task {task_id}"))?;
        task.status = TaskStatus::Cancelled;
        task.output = reason.or_else(|| Some("cancelled".to_string()));
        Ok(task.clone())
    }

    async fn spawn_agent(
        &self,
        parent_agent_id: &str,
        request: SpawnAgentToolRequest,
    ) -> Result<SpawnAgentToolResponse> {
        self.state
            .lock()
            .expect("fake control mutex poisoned")
            .spawn_requests
            .push((parent_agent_id.to_string(), request.clone()));
        Ok(SpawnAgentToolResponse {
            agent_id: "agent-child".to_string(),
            session_id: request
                .session_id
                .unwrap_or_else(|| "child-session".to_string()),
            status: "running".to_string(),
            run_in_background: true,
            launch_run_id: Some("run-child".to_string()),
            final_output: None,
            team_name: request.team_name.clone(),
            isolation: request.isolation.clone().unwrap_or_default(),
            name: Some("agent_child".to_string()),
            path: Some("root/agent_child".to_string()),
            nickname: Some("Atlas".to_string()),
            retention: request.retention.clone(),
            snapshot: None,
        })
    }

    async fn latest_run_output(&self, run_id: &str) -> Result<Option<String>> {
        Ok(self
            .state
            .lock()
            .expect("fake control mutex poisoned")
            .run_outputs
            .get(run_id)
            .cloned())
    }

    async fn message_agent(
        &self,
        from_agent_id: &str,
        request: MessageAgentToolRequest,
    ) -> Result<PostMailboxResponse> {
        self.state
            .lock()
            .expect("fake control mutex poisoned")
            .mailbox_requests
            .push((from_agent_id.to_string(), request));
        Ok(PostMailboxResponse {
            accepted: true,
            message_id: "mailbox-test".to_string(),
            duplicate: false,
        })
    }

    async fn request_parent_clarification(
        &self,
        session_id: &str,
        requester_agent_id: &str,
        requester_run_id: Option<&str>,
        requester_tool_call_id: Option<&str>,
        request: UserQuestionRequest,
    ) -> Result<ParentClarificationToolResponse> {
        self.state
            .lock()
            .expect("fake control mutex poisoned")
            .parent_clarification_requests
            .push((
                session_id.to_string(),
                requester_agent_id.to_string(),
                request.clone(),
            ));
        Ok(ParentClarificationToolResponse {
            parent_agent_id: "agent-parent".to_string(),
            parent_session_id: "session-parent".to_string(),
            requester_run_id: requester_run_id.map(str::to_string),
            requester_tool_call_id: requester_tool_call_id
                .map(str::to_string)
                .or_else(|| Some(request.tool_call_id.clone())),
            run_id: "run-clarification-1".to_string(),
            request_id: request.id,
            response_message_type: PARENT_CLARIFICATION_ANSWER_MESSAGE_TYPE.to_string(),
        })
    }

    async fn wait_agent(
        &self,
        _caller_agent_id: &str,
        agent_id: &str,
        _timeout: Duration,
    ) -> Result<ManagedAgentSnapshot> {
        let mut state = self.state.lock().expect("fake control mutex poisoned");
        state.waited_agents.push(agent_id.to_string());
        state
            .agents
            .iter()
            .find(|snapshot| snapshot.agent.id.0 == agent_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown agent {agent_id}"))
    }

    async fn list_agents(&self, _caller_agent_id: &str) -> Result<Vec<ManagedAgentSnapshot>> {
        Ok(self
            .state
            .lock()
            .expect("fake control mutex poisoned")
            .agents
            .clone())
    }

    async fn list_agent_summaries(
        &self,
        _caller_agent_id: &str,
    ) -> Result<Vec<crate::AgentSummaryView>> {
        Ok(self
            .state
            .lock()
            .expect("fake control mutex poisoned")
            .agents
            .iter()
            .map(|snapshot| crate::AgentSummaryView::from_record(&snapshot.agent, 0, true))
            .collect())
    }

    async fn get_agent(
        &self,
        _caller_agent_id: &str,
        agent_id: &str,
    ) -> Result<ManagedAgentSnapshot> {
        self.state
            .lock()
            .expect("fake control mutex poisoned")
            .agents
            .iter()
            .find(|snapshot| snapshot.agent.id.0 == agent_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown agent {agent_id}"))
    }

    async fn load_assistant_message(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Option<String>> {
        Ok(self
            .state
            .lock()
            .expect("fake control mutex poisoned")
            .assistant_messages
            .get(&(session_id.to_string(), message_id.to_string()))
            .cloned())
    }

    async fn list_skills(&self, _query: Option<&str>) -> Result<Vec<SkillSummary>> {
        Ok(self
            .state
            .lock()
            .expect("fake control mutex poisoned")
            .skills
            .values()
            .map(SkillSummary::from)
            .collect())
    }

    async fn get_skill(&self, name: &str) -> Result<Option<SkillDefinition>> {
        Ok(self
            .state
            .lock()
            .expect("fake control mutex poisoned")
            .skills
            .get(name)
            .cloned())
    }

    async fn get_learning_skill(&self, name: &str) -> Result<Option<crate::LearningSkillView>> {
        Ok(self
            .state
            .lock()
            .expect("fake control mutex poisoned")
            .learning_skills
            .get(name)
            .cloned())
    }

    async fn load_asset_attachment(
        &self,
        _session_id: &str,
        asset_id: &str,
    ) -> Result<AttachmentRef> {
        Ok(AttachmentRef {
            id: asset_id.to_string(),
            media_type: "image/png".to_string(),
            uri: format!("asset://raw/{asset_id}.png"),
            file_name: Some(format!("{asset_id}.png")),
            sha256: None,
            byte_length: Some(4),
            text_uri: None,
            text_sha256: None,
            text_byte_length: None,
            preview_image_uri: None,
            preview_image_media_type: None,
            preview_image_sha256: None,
            preview_image_byte_length: None,
        })
    }

    async fn read_channel_thread(
        &self,
        _session_id: &str,
        channel_id: &str,
        thread_root_message_id: &str,
    ) -> Result<Vec<crate::ChannelMessageView>> {
        Ok(self
            .state
            .lock()
            .expect("fake control mutex poisoned")
            .channel_thread_messages
            .get(&(channel_id.to_string(), thread_root_message_id.to_string()))
            .cloned()
            .unwrap_or_default())
    }

    async fn set_channel_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        actor_id: &str,
        emoji: &str,
    ) -> Result<crate::ChannelMessageView> {
        self.state
            .lock()
            .expect("fake control mutex poisoned")
            .channel_reaction_requests
            .push((
                channel_id.to_string(),
                message_id.to_string(),
                actor_id.to_string(),
                emoji.to_string(),
            ));
        Ok(crate::ChannelMessageView {
            message_id: message_id.to_string(),
            channel_id: channel_id.to_string(),
            thread_root_message_id: None,
            reply_to_message_id: None,
            sender: ActorRef {
                id: actor_id.to_string(),
                display_name: None,
            },
            sender_session_id: Some(actor_id.to_string()),
            addressed_member_ids: Vec::new(),
            output: RichOutput::text("reacted"),
            created_at_ms: 1,
            reactions: Vec::new(),
            metadata: json!({}),
        })
    }

    async fn create_channel_stimulus(
        &self,
        session_id: &str,
        channel_id: &str,
        request: crate::CreateChannelStimulusRequest,
    ) -> Result<crate::ChannelStimulusView> {
        self.state
            .lock()
            .expect("fake control mutex poisoned")
            .channel_stimulus_requests
            .push((
                session_id.to_string(),
                channel_id.to_string(),
                request.clone(),
            ));
        Ok(crate::ChannelStimulusView {
            stimulus_id: "stimulus-1".to_string(),
            channel_id: channel_id.to_string(),
            scope: request.scope,
            thread_root_message_id: request.thread_root_message_id,
            state: crate::ChannelStimulusState::Pending,
            kind: request.kind,
            visibility_hint: request.visibility_hint.unwrap_or_default(),
            content: request.content,
            addressed_member_ids: request.addressed_member_ids,
            sender_session_id: request.sender_session_id,
            sender_actor_id: request.sender_actor_id,
            sender_display_name: request.sender_display_name,
            source_kind: request.source_kind,
            source_ref: request.source_ref,
            dedupe_key: request.dedupe_key,
            progress_key: request.progress_key,
            created_at_ms: 1,
            available_at_ms: 1,
            expires_at_ms: request.expires_at_ms,
            claimed_at_ms: None,
            dispatched_at_ms: None,
            last_error: None,
            metadata: request.metadata,
        })
    }

    async fn generate_image(
        &self,
        session_id: &str,
        _run_id: Option<&str>,
        _tool_call_id: Option<&str>,
        request: GenerateImageToolRequest,
    ) -> Result<GenerateImageToolResponse> {
        self.state
            .lock()
            .expect("fake control mutex poisoned")
            .generate_image_requests
            .push((session_id.to_string(), request.clone()));
        let asset = AttachmentRef {
            id: "asset-generated-1".to_string(),
            media_type: "image/png".to_string(),
            uri: "asset://raw/asset-generated-1.png".to_string(),
            file_name: Some("generated-image.png".to_string()),
            sha256: None,
            byte_length: None,
            text_uri: None,
            text_sha256: None,
            text_byte_length: None,
            preview_image_uri: None,
            preview_image_media_type: None,
            preview_image_sha256: None,
            preview_image_byte_length: None,
        };
        Ok(GenerateImageToolResponse {
            provider: "test".to_string(),
            model: "test-image-model".to_string(),
            route_id: Some("test".to_string()),
            assets: vec![asset],
            revised_prompt: Some(request.prompt),
        })
    }

    async fn generate_audio(
        &self,
        session_id: &str,
        _run_id: Option<&str>,
        tool_call_id: Option<&str>,
        request: GenerateAudioToolRequest,
    ) -> Result<GenerateAudioToolResponse> {
        self.state
            .lock()
            .expect("fake control mutex poisoned")
            .generate_audio_requests
            .push((
                session_id.to_string(),
                tool_call_id.map(ToOwned::to_owned),
                request.clone(),
            ));
        let asset = AttachmentRef {
            id: "asset-generated-audio-1".to_string(),
            media_type: "audio/mpeg".to_string(),
            uri: "asset://raw/asset-generated-audio-1.mp3".to_string(),
            file_name: Some("generated-audio.mp3".to_string()),
            sha256: None,
            byte_length: None,
            text_uri: Some("asset://raw/asset-generated-audio-1.txt".to_string()),
            text_sha256: None,
            text_byte_length: None,
            preview_image_uri: None,
            preview_image_media_type: None,
            preview_image_sha256: None,
            preview_image_byte_length: None,
        };
        Ok(GenerateAudioToolResponse {
            provider: "openrouter".to_string(),
            model: "openai/gpt-4o-mini-tts".to_string(),
            route_id: Some("openrouter".to_string()),
            assets: vec![asset],
            transcript: Some(request.input),
        })
    }

    async fn edit_image(
        &self,
        session_id: &str,
        _run_id: Option<&str>,
        _tool_call_id: Option<&str>,
        request: EditImageToolRequest,
    ) -> Result<EditImageToolResponse> {
        self.state
            .lock()
            .expect("fake control mutex poisoned")
            .edit_image_requests
            .push((session_id.to_string(), request.clone()));
        let asset = AttachmentRef {
            id: "asset-edited-1".to_string(),
            media_type: "image/png".to_string(),
            uri: "asset://raw/asset-edited-1.png".to_string(),
            file_name: Some("edited-image.png".to_string()),
            sha256: None,
            byte_length: None,
            text_uri: None,
            text_sha256: None,
            text_byte_length: None,
            preview_image_uri: None,
            preview_image_media_type: None,
            preview_image_sha256: None,
            preview_image_byte_length: None,
        };
        Ok(EditImageToolResponse {
            provider: "openai".to_string(),
            model: "gpt-image-1.5".to_string(),
            route_id: Some("openai".to_string()),
            assets: vec![asset],
            revised_prompt: Some(format!(
                "{}:{}",
                request.prompt,
                request.image_asset_ids.join(",")
            )),
        })
    }

    async fn load_session_control_state(&self, session_id: &str) -> Result<SessionControlState> {
        Ok(self
            .state
            .lock()
            .expect("fake control mutex poisoned")
            .session_control
            .get(session_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn save_session_control_state(
        &self,
        session_id: &str,
        state: SessionControlState,
    ) -> Result<SessionControlState> {
        self.state
            .lock()
            .expect("fake control mutex poisoned")
            .session_control
            .insert(session_id.to_string(), state.clone());
        Ok(state)
    }

    async fn enter_session_plan_mode(&self, session_id: &str) -> Result<SessionControlState> {
        let mut state = self.state.lock().expect("fake control mutex poisoned");
        let previous_mode = state
            .session_permission_modes
            .get(session_id)
            .and_then(Clone::clone)
            .unwrap_or(PermissionMode::Default);
        let session_state = state
            .session_control
            .entry(session_id.to_string())
            .or_default();
        let was_in_plan_mode = session_state.plan_mode;
        session_state.plan_mode = true;
        if !matches!(
            session_state.session_permission_mode.as_deref(),
            Some("plan")
        ) && (!was_in_plan_mode || session_state.pre_plan_mode.is_none())
        {
            session_state.pre_plan_mode =
                Some(super::helpers::render_permission_mode(&previous_mode).to_string());
        }
        session_state.session_permission_mode = Some("plan".to_string());
        let snapshot = session_state.clone();
        let _ = session_state;
        state
            .session_permission_modes
            .insert(session_id.to_string(), Some(PermissionMode::Plan));
        Ok(snapshot)
    }

    async fn exit_session_plan_mode(
        &self,
        session_id: &str,
        plan: String,
        summary: Option<String>,
    ) -> Result<ExitPlanModeOutcome> {
        let mut state = self.state.lock().expect("fake control mutex poisoned");
        let session_state = state
            .session_control
            .entry(session_id.to_string())
            .or_default();
        let now = 1;
        let plan_id = session_state
            .plan_artifact
            .as_ref()
            .map(|artifact| artifact.id.clone())
            .unwrap_or_else(|| "plan-1".to_string());
        let created_at_ms = session_state
            .plan_artifact
            .as_ref()
            .map(|artifact| artifact.created_at_ms)
            .unwrap_or(now);
        session_state.plan_artifact = Some(kheish_types::PlanArtifact {
            id: plan_id,
            content: plan,
            summary,
            created_at_ms,
            updated_at_ms: now,
        });
        session_state.plan_mode = false;
        let restored_permission_mode = session_state
            .pre_plan_mode
            .as_deref()
            .and_then(super::helpers::parse_permission_mode);
        session_state.pre_plan_mode = None;
        session_state.session_permission_mode = restored_permission_mode
            .as_ref()
            .map(super::helpers::render_permission_mode)
            .map(ToString::to_string);
        let snapshot = session_state.clone();
        let _ = session_state;
        state
            .session_permission_modes
            .insert(session_id.to_string(), restored_permission_mode.clone());
        Ok(ExitPlanModeOutcome {
            state: snapshot,
            restored_permission_mode,
        })
    }

    async fn create_schedule(&self, request: ScheduleCreateRequest) -> Result<ScheduleView> {
        let mut state = self.state.lock().expect("fake control mutex poisoned");
        let schedule_id = format!("schedule-{}", state.schedules.len() + 1);
        let request_summary = summarize_schedule_create_request(&request);
        let schedule = ScheduleView {
            schedule_id: schedule_id.clone(),
            name: request.name,
            target_session_id: request.target_session_id,
            target_agent_id: request.target_agent_id,
            owner_session_id: request.owner_session_id,
            owner_agent_id: request.owner_agent_id,
            created_by_run_id: request.created_by_run_id,
            status: ScheduleStatus::Active,
            cadence: request.cadence,
            overlap_policy: request.overlap_policy,
            misfire_policy: request.misfire_policy,
            max_executions: request.max_executions,
            created_at_ms: 1,
            updated_at_ms: 1,
            next_fire_at_ms: Some(1),
            queued_fire_at_ms: None,
            in_flight_fire_at_ms: None,
            in_flight_run_id: None,
            paused_remaining_ms: None,
            last_fire_at_ms: None,
            last_dispatched_run_id: None,
            last_error: None,
            last_scheduler_error: None,
            scheduler_retry_after_ms: None,
            scheduler_retry_attempt: 0,
            execution_count: 0,
            consecutive_failures: 0,
            recent_executions: Vec::new(),
            request: request_summary,
        };
        state.schedules.insert(schedule_id, schedule.clone());
        Ok(schedule)
    }

    async fn list_schedules(&self, session_id: Option<&str>) -> Result<Vec<ScheduleView>> {
        let state = self.state.lock().expect("fake control mutex poisoned");
        Ok(state
            .schedules
            .values()
            .filter(|schedule| {
                session_id.is_none_or(|session_id| {
                    schedule.owner_session_id.as_deref() == Some(session_id)
                        || schedule.target_session_id == session_id
                })
            })
            .cloned()
            .collect())
    }

    async fn get_schedule(&self, schedule_id: &str) -> Result<ScheduleView> {
        self.state
            .lock()
            .expect("fake control mutex poisoned")
            .schedules
            .get(schedule_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown schedule {schedule_id}"))
    }

    async fn cancel_schedule(&self, schedule_id: &str) -> Result<ScheduleView> {
        update_fake_schedule(&self.state, schedule_id, |schedule| {
            schedule.status = ScheduleStatus::Canceled;
        })
    }

    async fn pause_schedule(&self, schedule_id: &str) -> Result<ScheduleView> {
        update_fake_schedule(&self.state, schedule_id, |schedule| {
            schedule.status = ScheduleStatus::Paused;
        })
    }

    async fn resume_schedule(&self, schedule_id: &str) -> Result<ScheduleView> {
        update_fake_schedule(&self.state, schedule_id, |schedule| {
            schedule.status = ScheduleStatus::Active;
        })
    }

    async fn trigger_schedule_now(&self, schedule_id: &str) -> Result<ScheduleView> {
        update_fake_schedule(&self.state, schedule_id, |schedule| {
            schedule.queued_fire_at_ms = Some(1);
        })
    }
}

fn update_fake_schedule(
    state: &Mutex<FakeControlState>,
    schedule_id: &str,
    update: impl FnOnce(&mut ScheduleView),
) -> Result<ScheduleView> {
    let mut state = state.lock().expect("fake control mutex poisoned");
    let schedule = state
        .schedules
        .get_mut(schedule_id)
        .ok_or_else(|| anyhow!("unknown schedule {schedule_id}"))?;
    update(schedule);
    Ok(schedule.clone())
}

fn bind_control(control: &Arc<FakeControl>) -> DaemonToolControlHandle {
    let handle = DaemonToolControlHandle::new();
    let trait_object: Arc<dyn DaemonToolControl> = control.clone();
    handle.bind(&trait_object);
    handle
}

fn sample_snapshot(agent_id: &str, status: AgentStatus) -> ManagedAgentSnapshot {
    ManagedAgentSnapshot {
        agent: AgentRecord {
            id: AgentId(agent_id.to_string()),
            parent: None,
            name: Some(agent_id.replace('-', "_")),
            path: Some(agent_id.replace('-', "_")),
            nickname: Some("Atlas".to_string()),
            conversation: ConversationKey {
                session_id: "session-a".to_string(),
                thread_id: None,
            },
            status,
            retention: ChildRetentionPolicy::Retain,
            spawned_by_run_id: None,
            spawn_request_id: None,
            spawned_at_ms: 1,
            settled_at_ms: None,
            closed_at_ms: None,
            subtasks: Vec::new(),
            sidechain_session_id: None,
            fork_context: None,
        },
        pending_approvals: Vec::new(),
        pending_questions: Vec::new(),
        last_assistant_message: Some("ready".to_string()),
        journal_len: 0,
        checkpoint_len: 0,
        last_error: None,
    }
}

fn sample_skill(name: &str, context: SkillExecutionContext) -> SkillDefinition {
    let skill_root = std::env::temp_dir().join(name.replace(':', "_"));
    SkillDefinition {
        name: name.to_string(),
        description: format!("Run the reusable skill `{name}`."),
        when_to_use: Some("when the task explicitly matches this reusable workflow".to_string()),
        version: Some("1.0.0".to_string()),
        skill_path: skill_root.join("SKILL.md"),
        skill_root,
        scope: SkillScope::Repo,
        digest: format!("digest-{name}"),
        runtime: SkillRuntimeConfig {
            allowed_tools: vec!["read_file".to_string()],
            blocked_tools: vec!["bash".to_string()],
            context,
            agent_profile: (context == SkillExecutionContext::Fork)
                .then(|| "verification".to_string()),
            provider: None,
            model: None,
            fallback_model: None,
        },
        instructions: "Inspect the requested artifact and summarize the result.".to_string(),
    }
}

#[test]
fn user_question_tool_descriptors_include_valid_json_shape() -> Result<()> {
    let example = serde_json::from_str::<serde_json::Value>(USER_QUESTION_INPUT_EXAMPLE)?;
    let parsed =
        build_user_question_request(&FakeControl::context("session-a", "agent-a"), &example)?;
    assert_eq!(parsed.questions.len(), 1);
    assert_eq!(parsed.questions[0].id, "focus");
    assert_eq!(parsed.questions[0].options.len(), 2);

    let descriptor =
        RequestParentClarificationTool::new(DaemonToolControlHandle::new()).descriptor();
    assert!(
        descriptor.description.contains(USER_QUESTION_INPUT_EXAMPLE),
        "tool description should include a concrete input example: {}",
        descriptor.description
    );
    let questions_field = descriptor
        .schema
        .fields
        .iter()
        .find(|field| field.name == "questions")
        .expect("request_parent_clarification should declare questions field");
    assert!(
        questions_field
            .description
            .as_deref()
            .unwrap_or_default()
            .contains("\"options\":[{\"id\":\"memory\",\"label\":\"memory\""),
        "questions field should document nested option shape: {:?}",
        questions_field.description
    );
    assert!(
        questions_field.structured_schema.is_some(),
        "questions field should expose a provider-facing nested schema"
    );
    let input_schema = descriptor.definition().input_schema;
    assert_eq!(
        input_schema["properties"]["questions"]["type"],
        json!("array")
    );
    assert_eq!(
        input_schema["properties"]["questions"]["items"]["additionalProperties"],
        json!(false)
    );
    assert_eq!(
        input_schema["properties"]["questions"]["items"]["required"],
        json!(["options", "question"])
    );
    assert_eq!(
        input_schema["properties"]["questions"]["items"]["properties"]["options"]["items"]["properties"]
            ["label"]["type"],
        json!("string")
    );

    let ask_descriptor = AskUserQuestionTool.descriptor();
    assert!(
        ask_descriptor
            .description
            .contains(USER_QUESTION_INPUT_EXAMPLE),
        "ask_user_question description should include the same concrete input example: {}",
        ask_descriptor.description
    );
    assert_eq!(
        ask_descriptor.schema.fields[0].description,
        questions_field.description
    );
    for descriptor in [&descriptor, &ask_descriptor] {
        assert!(
            descriptor
                .schema
                .fields
                .iter()
                .any(|field| field.name == "expires_at_ms"),
            "{} should expose expires_at_ms",
            descriptor.name
        );
        assert!(
            descriptor
                .schema
                .fields
                .iter()
                .any(|field| field.name == "expires_after_ms"),
            "{} should expose expires_after_ms",
            descriptor.name
        );
    }
    Ok(())
}

#[tokio::test]
async fn agent_tools_delegate_to_daemon_control() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    control
        .state
        .lock()
        .expect("fake control mutex poisoned")
        .agents = vec![sample_snapshot("agent-child", AgentStatus::Idle)];
    let handle = bind_control(&control);

    let spawn = SpawnAgentTool::new(handle.clone());
    let response = spawn
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "name": "child",
                "description": "Investigate a subtask.",
                "prompt": "Inspect the workspace.",
                "session_id": "child-session",
                "wait": true,
                "timeout_ms": 10,
            }),
        )
        .await?;
    assert_eq!(response.output["agent_id"], "agent-child");
    assert_eq!(response.output["run_in_background"], false);
    assert_eq!(response.output["snapshot"]["agent"]["id"], "agent-child");

    let message = MessageAgentTool::new(handle.clone());
    let response = message
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "agent_id": "agent-child",
                "subject": "handoff",
                "message": "Please summarize the findings.",
            }),
        )
        .await?;
    assert_eq!(response.output["queued"], true);
    assert_eq!(response.output["message_id"], "mailbox-test");

    let clarification = RequestParentClarificationTool::new(handle.clone())
        .execute(
            FakeControl::context_with_run("session-a", "agent-child", "run-context-1"),
            json!({
                "questions": [{
                    "header": "Focus",
                    "question": "Which focus should I use?",
                    "options": [{"label": "memory"}, {"label": "kernel"}]
                }]
            }),
        )
        .await?;
    assert_eq!(
        clarification.output["response_message_type"],
        PARENT_CLARIFICATION_ANSWER_MESSAGE_TYPE
    );
    assert_eq!(clarification.output["requester_run_id"], "run-context-1");
    assert_eq!(clarification.output["requester_tool_call_id"], "call-1");

    let waited = WaitAgentTool::new(handle.clone())
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({"agent_id": "agent-child", "timeout_ms": 10}),
        )
        .await?;
    assert_eq!(waited.output["agent"]["id"], "agent-child");

    let listed = ListAgentsTool::new(handle.clone())
        .execute(FakeControl::context("session-a", "agent-parent"), json!({}))
        .await?;
    let listed_agents = listed
        .output
        .as_array()
        .expect("list agents output should be an array");
    assert_eq!(listed_agents.len(), 1);
    assert_eq!(listed_agents[0]["agent"]["id"], "agent-child");
    assert_eq!(listed_agents[0]["last_assistant_message"], "ready");

    let summaries_tool = ListAgentSummariesTool::new(handle.clone());
    let summaries = summaries_tool
        .execute(FakeControl::context("session-a", "agent-parent"), json!({}))
        .await?;
    let summary_agents = summaries
        .output
        .as_array()
        .expect("list agent summaries output should be an array");
    assert_eq!(summary_agents.len(), 1);
    assert_eq!(summary_agents[0]["agent_id"], "agent-child");
    assert!(summary_agents[0].get("last_assistant_message").is_none());
    let filtered_summaries = summaries_tool
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({"status": "completed"}),
        )
        .await?;
    assert_eq!(
        filtered_summaries
            .output
            .as_array()
            .expect("filtered summary output should be an array")
            .len(),
        0
    );

    let state = control.state.lock().expect("fake control mutex poisoned");
    assert_eq!(state.spawn_requests.len(), 1);
    assert_eq!(state.mailbox_requests.len(), 1);
    assert_eq!(state.parent_clarification_requests.len(), 1);
    assert_eq!(
        state.waited_agents,
        vec!["agent-child".to_string(), "agent-child".to_string()]
    );
    Ok(())
}

#[tokio::test]
async fn generate_image_tool_returns_daemon_owned_assets() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    let response = super::output::GenerateImageTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "prompt": "A black cat on a white background.",
                "count": 1,
            }),
        )
        .await?;

    assert_eq!(response.output["provider"], "test");
    assert_eq!(response.output["assets"][0]["id"], "asset-generated-1");
    assert_eq!(response.output["assets"][0]["media_type"], "image/png");
    Ok(())
}

#[tokio::test]
async fn generate_audio_tool_returns_daemon_owned_assets() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    let response = super::output::GenerateAudioTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "input": "OpenRouter audio generation check from kheish daemon.",
                "voice": "alloy",
                "format": "mp3",
            }),
        )
        .await?;

    assert_eq!(response.output["provider"], "openrouter");
    assert_eq!(response.output["route_id"], "openrouter");
    assert_eq!(
        response.output["assets"][0]["id"],
        "asset-generated-audio-1"
    );
    assert_eq!(response.output["assets"][0]["media_type"], "audio/mpeg");
    assert_eq!(
        response.output["transcript"],
        "OpenRouter audio generation check from kheish daemon."
    );
    Ok(())
}

#[tokio::test]
async fn generate_audio_tool_forwards_route_overrides() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    super::output::GenerateAudioTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "input": "Render this sentence as speech.",
                "voice": "alloy",
                "format": "mp3",
                "speed": 1.1,
                "provider": "openrouter",
                "model": "openai/gpt-4o-mini-tts",
            }),
        )
        .await?;

    let state = control.state.lock().expect("fake control mutex poisoned");
    let (session_id, tool_call_id, request) = state
        .generate_audio_requests
        .first()
        .ok_or_else(|| anyhow!("missing generate_audio request capture"))?;
    assert_eq!(session_id, "session-a");
    assert_eq!(tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(request.route.provider.as_deref(), Some("openrouter"));
    assert_eq!(
        request.route.model.as_deref(),
        Some("openai/gpt-4o-mini-tts")
    );
    assert_eq!(request.voice.as_deref(), Some("alloy"));
    assert_eq!(request.format.as_deref(), Some("mp3"));
    assert_eq!(request.speed, Some(1.1));
    Ok(())
}

#[tokio::test]
async fn generate_image_tool_accepts_integer_like_float_count() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    let response = super::output::GenerateImageTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "prompt": "A black cat on a white background.",
                "count": 1.0,
            }),
        )
        .await?;

    assert_eq!(response.output["assets"][0]["id"], "asset-generated-1");
    Ok(())
}

#[tokio::test]
async fn generate_image_tool_forwards_route_overrides() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    super::output::GenerateImageTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "prompt": "Render the exact floor plan geometry in 3D.",
                "count": 1,
                "provider": "google",
                "model": "gemini-3-pro-image-preview",
                "size": "1024x1024",
            }),
        )
        .await?;

    let state = control.state.lock().expect("fake control mutex poisoned");
    let (session_id, request) = state
        .generate_image_requests
        .first()
        .ok_or_else(|| anyhow!("missing generate_image request capture"))?;
    assert_eq!(session_id, "session-a");
    assert_eq!(request.route.provider.as_deref(), Some("google"));
    assert_eq!(
        request.route.model.as_deref(),
        Some("gemini-3-pro-image-preview")
    );
    assert_eq!(request.size.as_deref(), Some("1024x1024"));
    Ok(())
}

#[tokio::test]
async fn edit_image_tool_returns_daemon_owned_assets() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    let response = super::output::EditImageTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "prompt": "Turn this square red.",
                "image_asset_ids": ["asset-source-1"],
                "count": 1,
            }),
        )
        .await?;

    assert_eq!(response.output["provider"], "openai");
    assert_eq!(response.output["assets"][0]["id"], "asset-edited-1");
    assert_eq!(
        response.output["revised_prompt"],
        "Turn this square red.:asset-source-1"
    );
    Ok(())
}

#[tokio::test]
async fn edit_image_tool_accepts_integer_like_float_count() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    let response = super::output::EditImageTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "prompt": "Turn this square red.",
                "image_asset_ids": ["asset-source-1", "asset-reference-2"],
                "count": 2.0,
            }),
        )
        .await?;

    assert_eq!(response.output["assets"][0]["id"], "asset-edited-1");
    assert_eq!(
        response.output["revised_prompt"],
        "Turn this square red.:asset-source-1,asset-reference-2"
    );
    Ok(())
}

#[tokio::test]
async fn edit_image_tool_forwards_route_overrides() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    super::output::EditImageTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "prompt": "Apply the reviewer corrections without changing the geometry.",
                "image_asset_ids": ["asset-source-1", "asset-reference-2"],
                "count": 1,
                "provider": "google",
                "model": "gemini-2.5-flash-image",
                "size": "1536x1024",
            }),
        )
        .await?;

    let state = control.state.lock().expect("fake control mutex poisoned");
    let (session_id, request) = state
        .edit_image_requests
        .first()
        .ok_or_else(|| anyhow!("missing edit_image request capture"))?;
    assert_eq!(session_id, "session-a");
    assert_eq!(request.route.provider.as_deref(), Some("google"));
    assert_eq!(
        request.route.model.as_deref(),
        Some("gemini-2.5-flash-image")
    );
    assert_eq!(request.size.as_deref(), Some("1536x1024"));
    Ok(())
}

#[tokio::test]
async fn edit_image_tool_marks_omitted_asset_ids_for_daemon_inference() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    super::output::EditImageTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "prompt": "Use the attached image.",
                "count": 1,
            }),
        )
        .await?;

    let state = control.state.lock().expect("fake control mutex poisoned");
    let (_, request) = state
        .edit_image_requests
        .first()
        .ok_or_else(|| anyhow!("missing edit_image request capture"))?;
    assert!(request.image_asset_ids.is_empty());
    assert!(request.image_asset_ids_was_omitted);
    Ok(())
}

#[tokio::test]
async fn edit_image_tool_preserves_explicit_empty_asset_ids_without_inference() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    super::output::EditImageTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "prompt": "Use the attached image.",
                "image_asset_ids": [],
                "count": 1,
            }),
        )
        .await?;

    let state = control.state.lock().expect("fake control mutex poisoned");
    let (_, request) = state
        .edit_image_requests
        .first()
        .ok_or_else(|| anyhow!("missing edit_image request capture"))?;
    assert!(request.image_asset_ids.is_empty());
    assert!(!request.image_asset_ids_was_omitted);
    Ok(())
}

#[tokio::test]
async fn edit_image_tool_preserves_null_asset_ids_without_inference() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    super::output::EditImageTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "prompt": "Use the attached image.",
                "image_asset_ids": null,
                "count": 1,
            }),
        )
        .await?;

    let state = control.state.lock().expect("fake control mutex poisoned");
    let (_, request) = state
        .edit_image_requests
        .first()
        .ok_or_else(|| anyhow!("missing edit_image request capture"))?;
    assert!(request.image_asset_ids.is_empty());
    assert!(!request.image_asset_ids_was_omitted);
    Ok(())
}

#[tokio::test]
async fn emit_output_tool_normalizes_visible_parts_and_artifacts() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    let response = super::output::EmitOutputTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "content": "Here is the generated image.",
                "parts": [
                    { "type": "asset", "asset_id": "asset-inline" }
                ],
                "artifact_ids": ["asset-inline", "asset-extra"],
                "include_artifacts_inline": true,
            }),
        )
        .await?;

    assert_eq!(
        response.output["content"],
        "Here is the generated image.\nAttached asset: asset-inline.png (image/png)\nAttached asset: asset-extra.png (image/png)"
    );
    assert_eq!(
        response.output["parts"]
            .as_array()
            .expect("emit_output should return ordered parts")
            .len(),
        3
    );
    assert_eq!(
        response.output["artifacts"]
            .as_array()
            .expect("emit_output should return artifacts")
            .len(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn emit_output_tool_exposes_artifacts_when_no_visible_parts_are_provided() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    let response = super::output::EmitOutputTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "artifact_ids": ["asset-inline"],
            }),
        )
        .await?;

    assert_eq!(
        response.output["content"],
        "Attached asset: asset-inline.png (image/png)"
    );
    assert_eq!(
        response.output["parts"]
            .as_array()
            .expect("emit_output should synthesize one visible attachment")
            .len(),
        1
    );
    assert_eq!(response.output["parts"][0]["type"], "attachment");
    Ok(())
}

#[tokio::test]
async fn read_channel_thread_tool_returns_thread_messages() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    control
        .state
        .lock()
        .expect("fake control mutex poisoned")
        .channel_thread_messages
        .insert(
            (
                "channel-domains".to_string(),
                "channel-message-1".to_string(),
            ),
            vec![crate::ChannelMessageView {
                message_id: "channel-message-2".to_string(),
                channel_id: "channel-domains".to_string(),
                thread_root_message_id: Some("channel-message-1".to_string()),
                reply_to_message_id: Some("channel-message-1".to_string()),
                sender: ActorRef {
                    id: "marketing-room".to_string(),
                    display_name: Some("Marketing".to_string()),
                },
                sender_session_id: Some("marketing-room".to_string()),
                addressed_member_ids: Vec::new(),
                output: RichOutput::text("Looks promising."),
                created_at_ms: 42,
                reactions: Vec::new(),
                metadata: json!({}),
            }],
        );
    let handle = bind_control(&control);

    let response = super::channels::ReadChannelThreadTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "channel_id": "channel-domains",
                "thread_root_message_id": "channel-message-1",
            }),
        )
        .await?;

    assert_eq!(response.output.as_array().map(Vec::len), Some(1));
    assert_eq!(response.output[0]["message_id"], "channel-message-2");
    assert_eq!(response.output[0]["sender"]["display_name"], "Marketing");
    Ok(())
}

#[tokio::test]
async fn set_channel_reaction_tool_uses_current_session_as_actor() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    let response = super::channels::SetChannelReactionTool::new(handle)
        .execute(
            FakeControl::context("finance-room", "agent-parent"),
            json!({
                "channel_id": "channel-domains",
                "message_id": "channel-message-2",
                "emoji": "👍",
            }),
        )
        .await?;

    assert_eq!(response.output["message_id"], "channel-message-2");
    let state = control.state.lock().expect("fake control mutex poisoned");
    assert_eq!(
        state.channel_reaction_requests,
        vec![(
            "channel-domains".to_string(),
            "channel-message-2".to_string(),
            "finance-room".to_string(),
            "👍".to_string(),
        )]
    );
    Ok(())
}

#[tokio::test]
async fn create_channel_stimulus_tool_accepts_full_declared_schema_and_uses_current_session()
-> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    let response = super::channels::CreateChannelStimulusTool::new(handle)
        .execute(
            FakeControl::context("atlas-room", "agent-parent"),
            json!({
                "channel_id": "channel-ideas",
                "content": "I found a cleaner retry path for the worker.",
                "kind": "agent_idea",
                "scope": "channel",
                "visibility_hint": "main",
                "addressed_member_ids": ["aurora"],
                "source_kind": "agent_idea",
                "source_ref": "idea-alpha-1",
                "dedupe_key": "idea-alpha-1",
                "progress_key": "idea-alpha-1:latest",
                "expires_at_ms": 12345,
            }),
        )
        .await?;

    assert_eq!(response.output["stimulus_id"], "stimulus-1");
    let state = control.state.lock().expect("fake control mutex poisoned");
    assert_eq!(state.channel_stimulus_requests.len(), 1);
    let (session_id, channel_id, request) = &state.channel_stimulus_requests[0];
    assert_eq!(session_id, "atlas-room");
    assert_eq!(channel_id, "channel-ideas");
    assert_eq!(request.sender_session_id, None);
    assert_eq!(request.source_kind.as_deref(), Some("agent_idea"));
    assert_eq!(request.source_ref.as_deref(), Some("idea-alpha-1"));
    assert_eq!(request.addressed_member_ids, vec!["aurora".to_string()]);
    assert_eq!(request.progress_key.as_deref(), Some("idea-alpha-1:latest"));
    assert_eq!(request.expires_at_ms, Some(12345));
    Ok(())
}

#[tokio::test]
async fn skill_tools_list_and_activate_inline_skills() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    control
        .state
        .lock()
        .expect("fake control mutex poisoned")
        .skills
        .insert(
            "report:summary".to_string(),
            sample_skill("report:summary", SkillExecutionContext::Inline),
        );
    let handle = bind_control(&control);
    let context = FakeControl::context("session-a", "agent-parent");

    let listed = ListSkillsTool::new(handle.clone())
        .execute(context.clone(), json!({"query": "summary"}))
        .await?;
    assert_eq!(listed.output["count"], 1);
    assert_eq!(listed.output["skills"][0]["name"], "report:summary");

    let activated = UseSkillTool::new(handle)
        .execute(
            context,
            json!({
                "name": "report:summary",
                "args": "reports/today.md",
            }),
        )
        .await?;
    assert_eq!(activated.output["action"], "activate");
    assert_eq!(activated.output["mode"], "inline");
    assert_eq!(activated.output["active_skill"]["name"], "report:summary");
    assert_eq!(activated.output["active_skill"]["context"], "inline");
    assert_eq!(activated.hook_contexts.len(), 1);
    assert!(activated.hook_contexts[0].contains("reports/today.md"));
    Ok(())
}

#[tokio::test]
async fn list_skills_tool_filters_skills_hidden_by_execution_scope() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let mut context = FakeControl::context("session-a", "agent-parent");
    context.metadata["visible_skills"] = json!(["report:summary"]);
    control
        .state
        .lock()
        .expect("fake control mutex poisoned")
        .skills
        .extend([
            (
                "report:summary".to_string(),
                sample_skill("report:summary", SkillExecutionContext::Inline),
            ),
            (
                "review:artifact".to_string(),
                sample_skill("review:artifact", SkillExecutionContext::Inline),
            ),
        ]);

    let listed = ListSkillsTool::new(bind_control(&control))
        .execute(context, json!({}))
        .await?;

    assert_eq!(listed.output["count"], 1);
    assert_eq!(listed.output["skills"][0]["name"], "report:summary");
    Ok(())
}

#[tokio::test]
async fn use_skill_tool_forks_when_skill_requires_child_context() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    {
        let mut state = control.state.lock().expect("fake control mutex poisoned");
        state.agents = vec![sample_snapshot("agent-child", AgentStatus::Idle)];
        state.run_outputs.insert(
            "run-child".to_string(),
            "PROMOTED_PROCEDURAL_SKILL_OK:reports/today.md".to_string(),
        );
        state.skills.insert(
            "review:artifact".to_string(),
            sample_skill("review:artifact", SkillExecutionContext::Fork),
        );
    }
    let handle = bind_control(&control);

    let response = UseSkillTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "name": "review:artifact",
                "args": "reports/today.md",
                "wait": true,
                "timeout_ms": 10,
            }),
        )
        .await?;

    assert_eq!(response.output["action"], "fork");
    assert_eq!(response.output["mode"], "fork");
    assert_eq!(response.output["spawn"]["agent_id"], "agent-child");
    assert_eq!(
        response.output["spawn"]["final_output"],
        "PROMOTED_PROCEDURAL_SKILL_OK:reports/today.md"
    );
    let state = control.state.lock().expect("fake control mutex poisoned");
    assert_eq!(state.spawn_requests.len(), 1);
    let request = &state.spawn_requests[0].1;
    assert_eq!(request.agent_type.as_deref(), Some("verification"));
    assert_eq!(request.retention, Some(ChildRetentionPolicy::CloseOnSettle));
    assert_eq!(request.spawn_request_id.as_deref(), Some("call-1"));
    assert!(request.allowed_tools.iter().any(|tool| tool == "read_file"));
    assert!(request.prompt.contains("isolated child agent"));
    Ok(())
}

#[tokio::test]
async fn use_skill_tool_forces_worktree_isolation_for_promoted_learning_skills() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    {
        let mut state = control.state.lock().expect("fake control mutex poisoned");
        state.agents = vec![sample_snapshot("agent-child", AgentStatus::Idle)];
        let skill = sample_skill("learning:review", SkillExecutionContext::Fork);
        state
            .skills
            .insert("learning:review".to_string(), skill.clone());
        state.learning_skills.insert(
            "learning:review".to_string(),
            crate::LearningSkillView {
                skill_name: "learning:review".to_string(),
                source_learning_id: "learning-1".to_string(),
                source_scope: kheish_types::LearningScope {
                    kind: kheish_types::LearningScopeKind::Workspace,
                    id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
                },
                status: crate::LearningSkillStatus::Active,
                description: skill.description.clone(),
                when_to_use: skill.when_to_use.clone(),
                version: skill.version.clone(),
                instructions: skill.instructions.clone(),
                skill_path: skill.skill_path.display().to_string(),
                skill_root: skill.skill_root.display().to_string(),
                digest: skill.digest.clone(),
                definition_fingerprint: String::new(),
                runtime: skill.runtime.clone(),
                evidence_refs: Vec::new(),
                lifecycle_events: Vec::new(),
                verification_status: kheish_types::LearningVerificationStatus::Verified,
                successful_run_count: 2,
                distinct_session_count: 1,
                verifier_run_ids: vec!["run-verify".to_string(), "run-canary".to_string()],
                real_daemon_verified: true,
                last_verified_workspace_digest: None,
                canary_success_count: 1,
                canary_failure_count: 0,
                promoted_at_ms: 1,
                revoked_at_ms: None,
                revoked_reason: None,
            },
        );
    }

    let handle = bind_control(&control);
    UseSkillTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "name": "learning:review",
                "wait": false,
            }),
        )
        .await?;

    let state = control.state.lock().expect("fake control mutex poisoned");
    let request = &state.spawn_requests[0].1;
    assert_eq!(request.isolation, Some(SpawnIsolation::Worktree));
    assert!(
        request
            .cwd
            .as_deref()
            .is_some_and(|cwd| cwd.contains(".kheish-procedural-worktrees")),
        "promoted worktree requests must allocate a dedicated child workspace"
    );
    Ok(())
}

#[tokio::test]
async fn use_skill_tool_rejects_promoted_skill_when_loaded_definition_differs_from_record()
-> Result<()> {
    let control = Arc::new(FakeControl::new());
    {
        let mut state = control.state.lock().expect("fake control mutex poisoned");
        let mut skill = sample_skill("learning:review", SkillExecutionContext::Fork);
        skill.description = "Tampered loaded description".to_string();
        state
            .skills
            .insert("learning:review".to_string(), skill.clone());
        state.learning_skills.insert(
            "learning:review".to_string(),
            crate::LearningSkillView {
                skill_name: "learning:review".to_string(),
                source_learning_id: "learning-1".to_string(),
                source_scope: kheish_types::LearningScope {
                    kind: kheish_types::LearningScopeKind::Workspace,
                    id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
                },
                status: crate::LearningSkillStatus::Active,
                description: "Original record description".to_string(),
                when_to_use: skill.when_to_use.clone(),
                version: skill.version.clone(),
                instructions: skill.instructions.clone(),
                skill_path: skill.skill_path.display().to_string(),
                skill_root: skill.skill_root.display().to_string(),
                digest: skill.digest.clone(),
                definition_fingerprint: String::new(),
                runtime: skill.runtime.clone(),
                evidence_refs: Vec::new(),
                lifecycle_events: Vec::new(),
                verification_status: kheish_types::LearningVerificationStatus::Verified,
                successful_run_count: 2,
                distinct_session_count: 1,
                verifier_run_ids: vec!["run-verify".to_string(), "run-canary".to_string()],
                real_daemon_verified: true,
                last_verified_workspace_digest: None,
                canary_success_count: 1,
                canary_failure_count: 0,
                promoted_at_ms: 1,
                revoked_at_ms: None,
                revoked_reason: None,
            },
        );
    }

    let handle = bind_control(&control);
    let error = UseSkillTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "name": "learning:review",
                "wait": false,
            }),
        )
        .await
        .expect_err("definition drift should be rejected before activation");

    assert!(
        error
            .to_string()
            .contains("catalog binding does not match the active record"),
        "unexpected error: {error:?}"
    );
    Ok(())
}

#[tokio::test]
async fn use_skill_tool_accepts_integer_like_float_timeout() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    {
        let mut state = control.state.lock().expect("fake control mutex poisoned");
        state.agents = vec![sample_snapshot("agent-child", AgentStatus::Idle)];
        state.skills.insert(
            "review:artifact".to_string(),
            sample_skill("review:artifact", SkillExecutionContext::Fork),
        );
    }
    let handle = bind_control(&control);

    UseSkillTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "name": "review:artifact",
                "wait": false,
                "timeout_ms": 10.0,
            }),
        )
        .await?;

    let state = control.state.lock().expect("fake control mutex poisoned");
    assert_eq!(state.spawn_requests.len(), 1);
    assert_eq!(state.spawn_requests[0].1.timeout_ms, Some(10));
    Ok(())
}

#[test]
fn waiting_agent_tools_allow_child_waits_longer_than_the_default_spawn_timeout() {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);
    assert!(
        SpawnAgentTool::new(handle.clone()).descriptor().timeout_ms >= 60_000,
        "spawn_agent must allow child waits longer than 60s defaults"
    );
    assert!(
        super::skills::UseSkillTool::new(handle)
            .descriptor()
            .timeout_ms
            >= 60_000,
        "use_skill must allow child waits longer than 60s defaults"
    );
}

#[test]
fn spawn_agent_descriptor_describes_generic_provider_overrides_and_required_inputs() {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);
    let descriptor = SpawnAgentTool::new(handle).descriptor();
    assert!(
        descriptor
            .description
            .to_ascii_lowercase()
            .contains("provide at least one initial input source"),
        "spawn_agent description should explain the minimum input contract: {}",
        descriptor.description
    );
    let provider_field = descriptor
        .schema
        .fields
        .iter()
        .find(|field| field.name == "provider")
        .expect("spawn_agent provider field should exist");
    let description = provider_field
        .description
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        description.contains("configured daemon provider"),
        "spawn_agent provider description should stay generic: {description}"
    );
}

#[tokio::test]
async fn use_skill_tool_rejects_inline_execution_overrides_that_require_a_child_agent() {
    let control = Arc::new(FakeControl::new());
    let mut skill = sample_skill("inline:override", SkillExecutionContext::Inline);
    skill.runtime.model = Some("gpt-5.4".to_string());
    control
        .state
        .lock()
        .expect("fake control mutex poisoned")
        .skills
        .insert(skill.name.clone(), skill);

    let tool = super::skills::UseSkillTool::new(bind_control(&control));
    let error = tool
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "name": "inline:override",
            }),
        )
        .await
        .expect_err("inline overrides should require fork execution");
    assert!(
        error.to_string().contains("require context=fork"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn use_skill_tool_rejects_skills_hidden_by_execution_scope() {
    let control = Arc::new(FakeControl::new());
    control
        .state
        .lock()
        .expect("fake control mutex poisoned")
        .skills
        .insert(
            "report:summary".to_string(),
            sample_skill("report:summary", SkillExecutionContext::Inline),
        );
    let mut context = FakeControl::context("session-a", "agent-parent");
    context.metadata["visible_skills"] = json!(["review:artifact"]);

    let error = UseSkillTool::new(bind_control(&control))
        .execute(
            context,
            json!({
                "name": "report:summary",
            }),
        )
        .await
        .expect_err("hidden skills should not be activatable");
    assert!(
        error
            .to_string()
            .contains("skill `report:summary` is not available in this session"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn spawn_agent_tool_rebases_relative_cwd_within_workspace() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let workspace_root = temp.path().join("workspace");
    let child_root = workspace_root.join("child");
    std::fs::create_dir_all(&child_root)?;
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);
    SpawnAgentTool::new(handle)
        .execute(
            ToolContext {
                call_id: "call-1".to_string(),
                sandbox: SandboxProfile::Inherited,
                metadata: json!({
                    "session_id": "session-a",
                    "agent_id": "agent-parent",
                    "workspace_root": workspace_root.display().to_string(),
                }),
            },
            json!({
                "name": "child",
                "description": "Investigate a subtask.",
                "prompt": "Inspect the workspace.",
                "cwd": "child",
            }),
        )
        .await?;
    let state = control.state.lock().expect("fake control mutex poisoned");
    let request = &state.spawn_requests[0].1;
    assert_eq!(
        request.cwd.as_deref(),
        Some(
            std::fs::canonicalize(&child_root)?
                .to_string_lossy()
                .as_ref()
        )
    );
    Ok(())
}

#[tokio::test]
async fn spawn_agent_tool_rejects_workspace_escape_cwd() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let workspace_root = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace_root)?;
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);
    let error = SpawnAgentTool::new(handle)
        .execute(
            ToolContext {
                call_id: "call-1".to_string(),
                sandbox: SandboxProfile::Inherited,
                metadata: json!({
                    "session_id": "session-a",
                    "agent_id": "agent-parent",
                    "workspace_root": workspace_root.display().to_string(),
                }),
            },
            json!({
                "name": "child",
                "description": "Investigate a subtask.",
                "prompt": "Inspect the workspace.",
                "cwd": "/",
            }),
        )
        .await
        .expect_err("absolute cwd escape should fail");
    assert!(error.to_string().contains("escapes workspace root"));
    Ok(())
}

#[tokio::test]
async fn spawn_agent_tool_propagates_spawn_request_id_from_tool_call_context() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);
    SpawnAgentTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "name": "child",
                "description": "Investigate a subtask.",
                "prompt": "Inspect the workspace."
            }),
        )
        .await?;
    let state = control.state.lock().expect("fake control mutex poisoned");
    let request = &state.spawn_requests[0].1;
    assert_eq!(request.spawn_request_id.as_deref(), Some("call-1"));
    Ok(())
}

#[tokio::test]
async fn spawn_agent_tool_scopes_spawn_request_id_to_the_parent_run_when_present() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);
    let mut context = FakeControl::context("session-a", "agent-parent");
    context.metadata["run_id"] = json!("run-42");

    SpawnAgentTool::new(handle)
        .execute(
            context,
            json!({
                "name": "child",
                "description": "Investigate a subtask.",
                "prompt": "Inspect the workspace."
            }),
        )
        .await?;

    let state = control.state.lock().expect("fake control mutex poisoned");
    let request = &state.spawn_requests[0].1;
    assert_eq!(request.spawned_by_run_id.as_deref(), Some("run-42"));
    assert_eq!(request.spawn_request_id.as_deref(), Some("run-42:call-1"));
    Ok(())
}

#[tokio::test]
async fn task_and_plan_tools_persist_session_control_state() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);
    let context = FakeControl::context("session-a", "agent-parent");

    let task_create = TaskCreateTool::new(handle.clone());
    let prep = task_create
        .execute(
            context.clone(),
            json!({
                "title": "Prepare the environment",
                "description": "Collect the prerequisites.",
                "owner_agent_id": "agent-parent",
            }),
        )
        .await?;
    let prep_task_id = prep.output["task"]["id"]
        .as_str()
        .expect("prep task id should be present")
        .to_string();
    TaskUpdateTool::new(handle.clone())
        .execute(
            context.clone(),
            json!({
                "task_id": prep_task_id,
                "status": "completed",
                "output": "environment ready",
            }),
        )
        .await?;
    let created = task_create
        .execute(
            context.clone(),
            json!({
                "title": "Write a report",
                "description": "Create the final machine report.",
                "owner_agent_id": "agent-parent",
            }),
        )
        .await?;
    let task_id = created.output["task"]["id"]
        .as_str()
        .expect("task id should be present")
        .to_string();

    let task_update = TaskUpdateTool::new(handle.clone());
    let updated = task_update
        .execute(
            context.clone(),
            json!({
                "task_id": task_id,
                "status": "in_progress",
                "output": "collecting data",
                "metadata": {"phase": "inspect"},
                "add_blocked_by": [prep_task_id],
            }),
        )
        .await?;
    assert_eq!(updated.output["status"], "in_progress");
    assert_eq!(updated.output["metadata"]["phase"], "inspect");

    let task_list = TaskListTool::new(handle.clone())
        .execute(context.clone(), json!({"status": "in_progress"}))
        .await?;
    assert_eq!(
        task_list
            .output
            .as_array()
            .expect("task list output should be an array")
            .len(),
        1
    );

    let task_output = TaskOutputTool::new(handle.clone())
        .execute(
            context.clone(),
            json!({
                "task_id": task_id,
                "wait": false,
            }),
        )
        .await?;
    assert_eq!(task_output.output["retrieval_status"], "not_ready");
    {
        let state = control.state.lock().expect("fake control mutex poisoned");
        assert_eq!(state.task_output_requests.len(), 1);
        assert_eq!(state.task_output_requests[0].3, 30_000);
        assert_eq!(
            state.task_output_requests[0].4,
            crate::shell_tasks::DEFAULT_TASK_OUTPUT_TAIL_BYTES
        );
    }

    let float_task_output = TaskOutputTool::new(handle.clone())
        .execute(
            context.clone(),
            json!({
                "task_id": task_id,
                "wait": true,
                "timeout_ms": 12.0,
                "tail_bytes": 128.0,
            }),
        )
        .await?;
    assert_eq!(float_task_output.output["retrieval_status"], "success");
    {
        let state = control.state.lock().expect("fake control mutex poisoned");
        assert_eq!(state.task_output_requests.len(), 2);
        assert_eq!(state.task_output_requests[1].3, 12);
        assert_eq!(state.task_output_requests[1].4, 128);
        assert!(!state.task_output_requests[1].5);
    }

    let full_task_output = TaskOutputTool::new(handle.clone())
        .execute(
            context.clone(),
            json!({
                "task_id": task_id,
                "wait": true,
                "tail_bytes": 999999999.0,
                "full": true,
            }),
        )
        .await?;
    assert_eq!(full_task_output.output["retrieval_status"], "success");
    {
        let state = control.state.lock().expect("fake control mutex poisoned");
        assert_eq!(state.task_output_requests.len(), 3);
        assert_eq!(
            state.task_output_requests[2].4,
            crate::shell_tasks::MAX_TASK_OUTPUT_TAIL_BYTES
        );
        assert!(state.task_output_requests[2].5);
    }

    let todos = TodoWriteTool::new(handle.clone())
        .execute(
            context.clone(),
            json!({
                "todos": [
                    {"content": "Inspect the machine"},
                    {"content": "Write the report", "completed": true}
                ]
            }),
        )
        .await?;
    assert_eq!(
        todos.output["todos"]
            .as_array()
            .expect("todo output should be an array")
            .len(),
        2
    );

    let entered = EnterPlanModeTool::new(handle.clone())
        .execute(context.clone(), json!({"note": "think first"}))
        .await?;
    assert_eq!(entered.output["plan_mode"], true);
    assert_eq!(entered.output["pre_plan_mode"].as_str(), Some("default"));

    let exited = ExitPlanModeTool::new(handle.clone())
        .execute(
            context.clone(),
            json!({
                "plan": "1. Inspect the machine.\n2. Write the report.\n3. Verify the generated file.",
                "summary": "Inspect, write, verify",
            }),
        )
        .await?;
    assert_eq!(exited.output["plan_mode"], false);
    assert_eq!(
        exited.output["plan_artifact"]["summary"].as_str(),
        Some("Inspect, write, verify")
    );

    let stopped = TaskStopTool::new(handle)
        .execute(
            context,
            json!({
                "task_id": task_id,
                "reason": "done elsewhere",
            }),
        )
        .await?;
    assert_eq!(stopped.output["status"], "cancelled");

    let deleted = TaskDeleteTool::new(bind_control(&control))
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "task_id": task_id,
            }),
        )
        .await?;
    assert_eq!(deleted.output["id"], task_id);

    let state = control.state.lock().expect("fake control mutex poisoned");
    let session_state = state
        .session_control
        .get("session-a")
        .expect("session control state should be stored");
    assert!(!session_state.plan_mode);
    assert_eq!(session_state.pre_plan_mode, None);
    assert_eq!(
        session_state.session_permission_mode.as_deref(),
        Some("default")
    );
    assert_eq!(session_state.todos.len(), 2);
    assert_eq!(session_state.tasks.len(), 1);
    assert_eq!(
        session_state
            .plan_artifact
            .as_ref()
            .and_then(|artifact| artifact.summary.as_deref()),
        Some("Inspect, write, verify")
    );
    assert_eq!(
        state
            .session_permission_modes
            .get("session-a")
            .cloned()
            .flatten(),
        Some(PermissionMode::Default)
    );
    Ok(())
}

#[tokio::test]
async fn live_background_shell_tasks_reject_generic_terminal_update_and_delete() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);
    let context = FakeControl::context("session-shell", "agent-parent");
    {
        let mut state = control.state.lock().expect("fake control mutex poisoned");
        state
            .session_control
            .entry("session-shell".to_string())
            .or_default()
            .tasks
            .push(TaskRecord {
                id: "shell-task-protected".to_string(),
                title: "protected shell".to_string(),
                description: "daemon shell".to_string(),
                status: TaskStatus::InProgress,
                owner_agent_id: Some("agent-parent".to_string()),
                blocked_by: Vec::new(),
                blocks: Vec::new(),
                output: None,
                metadata: json!({
                    "kind": "background_shell",
                    "command": "sleep 30",
                    "workdir": "/tmp",
                    "output_file_path": "/tmp/fake-shell-task.log",
                    "tool_call_id": "call-shell",
                }),
                created_at_ms: 1,
                updated_at_ms: 1,
            });
    }

    let update_error = TaskUpdateTool::new(handle.clone())
        .execute(
            context.clone(),
            json!({
                "task_id": "shell-task-protected",
                "status": "completed",
                "output": "fake done",
            }),
        )
        .await
        .expect_err("live shell task lifecycle should reject generic update");
    assert!(
        update_error
            .to_string()
            .contains("use task_stop and task_output")
    );

    let delete_error = TaskDeleteTool::new(handle.clone())
        .execute(context.clone(), json!({"task_id": "shell-task-protected"}))
        .await
        .expect_err("live shell task should reject generic delete");
    assert!(delete_error.to_string().contains("stop it before deleting"));

    TaskStopTool::new(handle.clone())
        .execute(
            context.clone(),
            json!({
                "task_id": "shell-task-protected",
                "reason": "test stop",
            }),
        )
        .await?;
    let deleted = TaskDeleteTool::new(handle)
        .execute(context, json!({"task_id": "shell-task-protected"}))
        .await?;
    assert_eq!(deleted.output["status"], "cancelled");
    Ok(())
}

#[tokio::test]
async fn wake_after_accepts_integer_like_float_delay_seconds() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);

    let response = WakeAfterTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({
                "delay_seconds": 5.0,
                "message": "Drink water.",
            }),
        )
        .await?;

    assert_eq!(response.output["schedule"]["name"], "wake-5");
    Ok(())
}

#[tokio::test]
async fn enter_plan_mode_persists_plan_state_and_keeps_the_original_restore_mode() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    {
        let mut state = control.state.lock().expect("fake control mutex poisoned");
        state.session_permission_modes.insert(
            "session-a".to_string(),
            Some(PermissionMode::BypassPermissions),
        );
        state.session_control.insert(
            "session-a".to_string(),
            SessionControlState {
                session_permission_updates: vec![kheish_types::HookPermissionUpdate {
                    scope: kheish_types::HookPermissionUpdateScope::Session,
                    tool_name_pattern: "bash".to_string(),
                    behavior: kheish_types::HookPermissionUpdateBehavior::Allow,
                    reason: None,
                }],
                ..SessionControlState::default()
            },
        );
    }
    let handle = bind_control(&control);
    let context = FakeControl::context("session-a", "agent-parent");

    EnterPlanModeTool::new(handle.clone())
        .execute(context.clone(), json!({}))
        .await?;
    EnterPlanModeTool::new(handle.clone())
        .execute(context.clone(), json!({"note": "still planning"}))
        .await?;

    {
        let state = control.state.lock().expect("fake control mutex poisoned");
        let session_state = state
            .session_control
            .get("session-a")
            .expect("session control state should be stored");
        assert!(session_state.plan_mode);
        assert_eq!(
            session_state.pre_plan_mode.as_deref(),
            Some("bypassPermissions")
        );
        assert_eq!(
            session_state.session_permission_mode.as_deref(),
            Some("plan")
        );
        assert_eq!(session_state.session_permission_updates.len(), 1);
        assert_eq!(
            session_state.session_permission_updates[0].tool_name_pattern,
            "bash"
        );
        assert_eq!(
            state
                .session_permission_modes
                .get("session-a")
                .cloned()
                .flatten(),
            Some(PermissionMode::Plan)
        );
    }

    let exited = ExitPlanModeTool::new(handle)
        .execute(
            context,
            json!({
                "plan": "1. Investigate.\n2. Report.",
            }),
        )
        .await?;
    assert_eq!(
        exited.output["restored_permission_mode"].as_str(),
        Some("bypassPermissions")
    );

    let state = control.state.lock().expect("fake control mutex poisoned");
    let session_state = state
        .session_control
        .get("session-a")
        .expect("session control state should be stored");
    assert!(!session_state.plan_mode);
    assert_eq!(session_state.pre_plan_mode, None);
    assert_eq!(
        session_state.session_permission_mode.as_deref(),
        Some("bypassPermissions")
    );
    assert_eq!(
        state
            .session_permission_modes
            .get("session-a")
            .cloned()
            .flatten(),
        Some(PermissionMode::BypassPermissions)
    );
    Ok(())
}

#[tokio::test]
async fn get_agent_tool_returns_one_snapshot() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    control
        .state
        .lock()
        .expect("fake control mutex poisoned")
        .agents = vec![sample_snapshot("agent-child", AgentStatus::Idle)];
    let handle = bind_control(&control);
    let result = GetAgentTool::new(handle)
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({"agent_id": "agent-child"}),
        )
        .await?;
    assert_eq!(result.output["agent"]["id"], "agent-child");
    Ok(())
}

#[tokio::test]
async fn task_create_rejects_duplicate_active_titles() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);
    let context = FakeControl::context("session-a", "agent-parent");

    TaskCreateTool::new(handle.clone())
        .execute(context.clone(), json!({"title": "Review findings"}))
        .await?;

    let error = TaskCreateTool::new(handle)
        .execute(context, json!({"title": "review findings"}))
        .await
        .expect_err("duplicate active title should fail");
    assert!(
        error
            .to_string()
            .contains("active task title already exists")
    );
    Ok(())
}

#[tokio::test]
async fn task_update_rejects_progress_when_blocked() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    let handle = bind_control(&control);
    let context = FakeControl::context("session-a", "agent-parent");

    let blocked = TaskCreateTool::new(handle.clone())
        .execute(context.clone(), json!({"title": "Prepare inputs"}))
        .await?;
    let blocked_id = blocked.output["task"]["id"]
        .as_str()
        .expect("blocked task id")
        .to_string();

    let main = TaskCreateTool::new(handle.clone())
        .execute(
            context.clone(),
            json!({"title": "Run analysis", "blocked_by": [blocked_id]}),
        )
        .await?;
    let main_id = main.output["task"]["id"]
        .as_str()
        .expect("main task id")
        .to_string();

    let error = TaskUpdateTool::new(handle)
        .execute(
            context,
            json!({"task_id": main_id, "status": "in_progress", "output": "started"}),
        )
        .await
        .expect_err("blocked task should not start");
    assert!(
        error
            .to_string()
            .contains("blocked by unresolved dependencies")
    );
    Ok(())
}

#[test]
fn sidechain_request_from_tool_applies_profiles_and_explicit_overrides() -> Result<()> {
    let request = sidechain_request_from_tool(&SpawnAgentToolRequest {
        session_id: Some("child".to_string()),
        thread_id: None,
        cwd: None,
        team_name: Some("planning".to_string()),
        isolation: Some(SpawnIsolation::Worktree),
        name: "plan-child".to_string(),
        description: "Plan the work".to_string(),
        prompt: "Produce a plan".to_string(),
        asset_ids: Vec::new(),
        input_items: Vec::new(),
        agent_type: Some("plan".to_string()),
        system_prompt: Some("Keep it terse.".to_string()),
        prompt_merge_mode: Some(PromptMergeMode::Append),
        model: Some("claude-sonnet-4-5".to_string()),
        provider: Some("anthropic".to_string()),
        fallback_model: Some("claude-opus-4-6".to_string()),
        generation: None,
        mode: Some("acceptEdits".to_string()),
        retention: None,
        nickname: None,
        allowed_tools: vec!["spawn_agent".to_string()],
        blocked_tools: vec!["bash".to_string()],
        capability_scope: None,
        credential_scope: None,
        wait: false,
        run_in_background: true,
        timeout_ms: None,
        parent_assistant_message: None,
        inherited_tool_call_ids: Vec::new(),
        spawned_by_run_id: None,
        spawn_request_id: None,
    })?;

    assert_eq!(request.provider.as_deref(), Some("anthropic"));
    assert_eq!(request.permission_mode.as_deref(), Some("acceptEdits"));
    assert_eq!(
        request
            .fork_context
            .generation
            .as_ref()
            .and_then(|cfg| cfg.model.as_deref()),
        Some("claude-sonnet-4-5")
    );
    assert_eq!(
        request
            .fork_context
            .generation
            .as_ref()
            .and_then(|cfg| cfg.fallback_model.as_deref()),
        Some("claude-opus-4-6")
    );
    assert_eq!(
        request.fork_context.prompt_merge_mode,
        PromptMergeMode::Append
    );
    assert_eq!(request.fork_context.worktree_path, None);
    assert!(
        request
            .fork_context
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == "spawn_agent")
    );
    assert!(
        !request
            .fork_context
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == DYNAMIC_MCP_TOOL_ALLOWLIST_SENTINEL),
        "explicit allowlists should disable implicit dynamic MCP expansion"
    );
    assert!(
        request
            .fork_context
            .tool_surface
            .denylist
            .iter()
            .any(|tool| tool == "bash")
    );
    Ok(())
}

#[test]
fn sidechain_request_from_tool_accepts_full_generation_and_rejects_model_conflicts() -> Result<()> {
    let mut generation = ModelGenerationConfig::default();
    generation.model = Some("gpt-5.4".to_string());
    generation.reasoning = Some(ReasoningConfig {
        effort: Some(ReasoningEffort::High),
        ..ReasoningConfig::default()
    });
    let request = sidechain_request_from_tool(&SpawnAgentToolRequest {
        session_id: Some("child".to_string()),
        thread_id: None,
        cwd: None,
        team_name: None,
        isolation: None,
        name: "review-child".to_string(),
        description: "Review the work".to_string(),
        prompt: "Review carefully".to_string(),
        asset_ids: Vec::new(),
        input_items: Vec::new(),
        agent_type: None,
        system_prompt: None,
        prompt_merge_mode: None,
        model: None,
        provider: Some("openai".to_string()),
        fallback_model: None,
        generation: Some(generation.clone()),
        mode: None,
        retention: None,
        nickname: None,
        allowed_tools: Vec::new(),
        blocked_tools: Vec::new(),
        capability_scope: None,
        credential_scope: None,
        wait: false,
        run_in_background: true,
        timeout_ms: None,
        parent_assistant_message: None,
        inherited_tool_call_ids: Vec::new(),
        spawned_by_run_id: None,
        spawn_request_id: None,
    })?;

    assert_eq!(
        request
            .fork_context
            .generation
            .as_ref()
            .and_then(|generation| generation.reasoning.as_ref())
            .and_then(|reasoning| reasoning.effort),
        Some(ReasoningEffort::High)
    );

    let mut conflicting = generation;
    conflicting.model = Some("gpt-5-mini".to_string());
    let error = sidechain_request_from_tool(&SpawnAgentToolRequest {
        session_id: Some("child".to_string()),
        thread_id: None,
        cwd: None,
        team_name: None,
        isolation: None,
        name: "review-child".to_string(),
        description: "Review the work".to_string(),
        prompt: "Review carefully".to_string(),
        asset_ids: Vec::new(),
        input_items: Vec::new(),
        agent_type: None,
        system_prompt: None,
        prompt_merge_mode: None,
        model: Some("gpt-5.4".to_string()),
        provider: Some("openai".to_string()),
        fallback_model: None,
        generation: Some(conflicting),
        mode: None,
        retention: None,
        nickname: None,
        allowed_tools: Vec::new(),
        blocked_tools: Vec::new(),
        capability_scope: None,
        credential_scope: None,
        wait: false,
        run_in_background: true,
        timeout_ms: None,
        parent_assistant_message: None,
        inherited_tool_call_ids: Vec::new(),
        spawned_by_run_id: None,
        spawn_request_id: None,
    })
    .expect_err("conflicting generation model should be rejected");
    assert!(error.to_string().contains("generation.model"));
    Ok(())
}

#[test]
fn sidechain_request_from_tool_supports_multimodal_input_items() -> Result<()> {
    let request = sidechain_request_from_tool(&SpawnAgentToolRequest {
        session_id: Some("child".to_string()),
        thread_id: None,
        cwd: None,
        team_name: None,
        isolation: None,
        name: "review-child".to_string(),
        description: "Review the provided assets".to_string(),
        prompt: String::new(),
        asset_ids: Vec::new(),
        input_items: vec![
            crate::SubmitInputItemRequest::Text {
                text: "Review these assets carefully.".to_string(),
            },
            crate::SubmitInputItemRequest::AssetReference {
                asset_id: "asset-42".to_string(),
            },
        ],
        agent_type: Some("verification".to_string()),
        system_prompt: None,
        prompt_merge_mode: None,
        model: None,
        provider: None,
        fallback_model: None,
        generation: None,
        mode: None,
        retention: None,
        nickname: None,
        allowed_tools: Vec::new(),
        blocked_tools: Vec::new(),
        capability_scope: None,
        credential_scope: None,
        wait: false,
        run_in_background: true,
        timeout_ms: None,
        parent_assistant_message: None,
        inherited_tool_call_ids: Vec::new(),
        spawned_by_run_id: None,
        spawn_request_id: None,
    })?;

    let subtask = request.subtask.expect("subtask should be present");
    assert!(subtask.content.is_empty());
    assert_eq!(subtask.input_items.len(), 2);
    assert!(matches!(
        &subtask.input_items[1],
        crate::SubmitInputItemRequest::AssetReference { asset_id } if asset_id == "asset-42"
    ));
    Ok(())
}

#[test]
fn sidechain_request_from_tool_rejects_prompt_and_input_items_combination() {
    let error = sidechain_request_from_tool(&SpawnAgentToolRequest {
        session_id: Some("child".to_string()),
        thread_id: None,
        cwd: None,
        team_name: None,
        isolation: None,
        name: "invalid-child".to_string(),
        description: "Invalid request".to_string(),
        prompt: "prompt".to_string(),
        asset_ids: Vec::new(),
        input_items: vec![crate::SubmitInputItemRequest::Text {
            text: "duplicate".to_string(),
        }],
        agent_type: None,
        system_prompt: None,
        prompt_merge_mode: None,
        model: None,
        provider: None,
        fallback_model: None,
        generation: None,
        mode: None,
        retention: None,
        nickname: None,
        allowed_tools: Vec::new(),
        blocked_tools: Vec::new(),
        capability_scope: None,
        credential_scope: None,
        wait: false,
        run_in_background: true,
        timeout_ms: None,
        parent_assistant_message: None,
        inherited_tool_call_ids: Vec::new(),
        spawned_by_run_id: None,
        spawn_request_id: None,
    })
    .expect_err("prompt plus input_items should be rejected");
    assert!(
        error
            .to_string()
            .contains("prompt cannot be combined with input_items")
    );
}

#[test]
fn mailbox_request_from_tool_supports_multimodal_input_items() -> Result<()> {
    let request = mailbox_request_from_tool(
        "agent-parent",
        MessageAgentToolRequest {
            agent_id: "agent-child".to_string(),
            subject: "review".to_string(),
            message: String::new(),
            asset_ids: Vec::new(),
            input_items: vec![
                crate::SubmitInputItemRequest::Text {
                    text: "Review the candidate.".to_string(),
                },
                crate::SubmitInputItemRequest::AssetReference {
                    asset_id: "asset-77".to_string(),
                },
            ],
            message_type: Some("review_request".to_string()),
        },
    )?;

    assert_eq!(request.from_agent_id, "agent-parent");
    assert_eq!(request.subject, "review");
    assert_eq!(request.payload["type"].as_str(), Some("review_request"));
    assert_eq!(
        request.payload["input_items"]
            .as_array()
            .expect("mailbox payload input_items should be an array")
            .len(),
        2
    );
    Ok(())
}

#[test]
fn mailbox_request_from_tool_keeps_text_messages_legacy() -> Result<()> {
    let request = mailbox_request_from_tool(
        "agent-parent",
        MessageAgentToolRequest {
            agent_id: "agent-child".to_string(),
            subject: "note".to_string(),
            message: "Review the final summary.".to_string(),
            asset_ids: Vec::new(),
            input_items: Vec::new(),
            message_type: None,
        },
    )?;

    assert_eq!(request.payload["type"].as_str(), Some("message"));
    assert_eq!(
        request.payload["message"].as_str(),
        Some("Review the final summary.")
    );
    assert!(request.payload.get("input_items").is_none());
    Ok(())
}

#[test]
fn sidechain_request_from_tool_supports_prompt_plus_asset_ids() -> Result<()> {
    let request = sidechain_request_from_tool(&SpawnAgentToolRequest {
        session_id: Some("child".to_string()),
        thread_id: None,
        cwd: None,
        team_name: None,
        isolation: None,
        name: "review-child".to_string(),
        description: "Review the provided assets".to_string(),
        prompt: "Review these assets carefully.".to_string(),
        asset_ids: vec!["asset-42".to_string(), "asset-43".to_string()],
        input_items: Vec::new(),
        agent_type: Some("verification".to_string()),
        system_prompt: None,
        prompt_merge_mode: None,
        model: None,
        provider: None,
        fallback_model: None,
        generation: None,
        mode: None,
        retention: None,
        nickname: None,
        allowed_tools: Vec::new(),
        blocked_tools: Vec::new(),
        capability_scope: None,
        credential_scope: None,
        wait: false,
        run_in_background: true,
        timeout_ms: None,
        parent_assistant_message: None,
        inherited_tool_call_ids: Vec::new(),
        spawned_by_run_id: None,
        spawn_request_id: None,
    })?;

    let subtask = request.subtask.expect("subtask should be present");
    assert!(subtask.content.is_empty());
    assert_eq!(
        subtask.input_items,
        vec![
            crate::SubmitInputItemRequest::Text {
                text: "Review these assets carefully.".to_string(),
            },
            crate::SubmitInputItemRequest::AssetReference {
                asset_id: "asset-42".to_string(),
            },
            crate::SubmitInputItemRequest::AssetReference {
                asset_id: "asset-43".to_string(),
            },
        ]
    );
    Ok(())
}

#[test]
fn sidechain_request_from_tool_supports_asset_ids_without_prompt() -> Result<()> {
    let request = sidechain_request_from_tool(&SpawnAgentToolRequest {
        session_id: Some("child".to_string()),
        thread_id: None,
        cwd: None,
        team_name: None,
        isolation: None,
        name: "review-child".to_string(),
        description: "Review the provided assets".to_string(),
        prompt: String::new(),
        asset_ids: vec!["asset-42".to_string()],
        input_items: Vec::new(),
        agent_type: Some("verification".to_string()),
        system_prompt: None,
        prompt_merge_mode: None,
        model: None,
        provider: None,
        fallback_model: None,
        generation: None,
        mode: None,
        retention: None,
        nickname: None,
        allowed_tools: Vec::new(),
        blocked_tools: Vec::new(),
        capability_scope: None,
        credential_scope: None,
        wait: false,
        run_in_background: true,
        timeout_ms: None,
        parent_assistant_message: None,
        inherited_tool_call_ids: Vec::new(),
        spawned_by_run_id: None,
        spawn_request_id: None,
    })?;

    let subtask = request.subtask.expect("subtask should be present");
    assert!(subtask.content.is_empty());
    assert_eq!(
        subtask.input_items,
        vec![crate::SubmitInputItemRequest::AssetReference {
            asset_id: "asset-42".to_string(),
        }]
    );
    Ok(())
}

#[test]
fn sidechain_request_from_tool_rejects_input_items_and_asset_ids_combination() {
    let error = sidechain_request_from_tool(&SpawnAgentToolRequest {
        session_id: Some("child".to_string()),
        thread_id: None,
        cwd: None,
        team_name: None,
        isolation: None,
        name: "invalid-child".to_string(),
        description: "Invalid request".to_string(),
        prompt: String::new(),
        asset_ids: vec!["asset-42".to_string()],
        input_items: vec![crate::SubmitInputItemRequest::Text {
            text: "duplicate".to_string(),
        }],
        agent_type: None,
        system_prompt: None,
        prompt_merge_mode: None,
        model: None,
        provider: None,
        fallback_model: None,
        generation: None,
        mode: None,
        retention: None,
        nickname: None,
        allowed_tools: Vec::new(),
        blocked_tools: Vec::new(),
        capability_scope: None,
        credential_scope: None,
        wait: false,
        run_in_background: true,
        timeout_ms: None,
        parent_assistant_message: None,
        inherited_tool_call_ids: Vec::new(),
        spawned_by_run_id: None,
        spawn_request_id: None,
    })
    .expect_err("input_items plus asset_ids should be rejected");
    assert!(
        error
            .to_string()
            .contains("asset_ids cannot be combined with input_items")
    );
}

#[test]
fn mailbox_request_from_tool_supports_message_plus_asset_ids() -> Result<()> {
    let request = mailbox_request_from_tool(
        "agent-parent",
        MessageAgentToolRequest {
            agent_id: "agent-child".to_string(),
            subject: "review".to_string(),
            message: "Review the candidate.".to_string(),
            asset_ids: vec!["asset-77".to_string(), "asset-88".to_string()],
            input_items: Vec::new(),
            message_type: Some("review_request".to_string()),
        },
    )?;

    assert_eq!(request.payload["type"].as_str(), Some("review_request"));
    assert!(request.payload.get("message").is_none());
    assert_eq!(
        request.payload["input_items"],
        json!([
            {"type": "text", "text": "Review the candidate."},
            {"type": "asset_reference", "asset_id": "asset-77"},
            {"type": "asset_reference", "asset_id": "asset-88"},
        ])
    );
    Ok(())
}

#[test]
fn built_in_profiles_apply_restricted_tool_surfaces() -> Result<()> {
    let default_profile = built_in_agent_profile(None)?;
    assert!(
        default_profile
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == "write_file")
    );
    assert!(
        default_profile
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == "apply_patch")
    );
    assert!(
        default_profile
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == "web_search")
    );
    assert!(
        default_profile
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == "task_output")
    );
    assert!(
        default_profile
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == "task_stop")
    );
    assert!(
        default_profile
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == "request_parent_clarification")
    );
    assert!(
        default_profile
            .tool_surface
            .denylist
            .iter()
            .any(|tool| tool == "ask_user_question")
    );
    assert!(
        !default_profile
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == "spawn_agent")
    );
    assert!(
        default_profile
            .tool_surface
            .denylist
            .iter()
            .any(|tool| tool == "list_agent_summaries")
    );

    let coordinator_profile = built_in_agent_profile(Some("coordinator"))?;
    assert!(
        coordinator_profile
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == "spawn_agent")
    );
    assert!(
        coordinator_profile
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == "list_agent_summaries")
    );
    assert!(
        coordinator_profile
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == "web_search")
    );
    assert!(
        coordinator_profile
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == "task_output")
    );
    assert!(
        coordinator_profile
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == "bash")
    );
    assert!(
        coordinator_profile
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == "request_parent_clarification")
    );
    assert!(
        !coordinator_profile
            .tool_surface
            .allowlist
            .iter()
            .any(|tool| tool == "ask_user_question")
    );
    Ok(())
}

#[test]
fn ask_user_question_rejects_duplicate_question_text_and_option_labels() {
    let context = FakeControl::context("session-a", "agent-parent");
    let duplicate_questions = build_user_question_request(
        &context,
        &json!({
            "questions": [
                {
                    "header": "Focus A",
                    "question": "Which focus should I use?",
                    "options": [{"label": "memory"}, {"label": "kernel"}]
                },
                {
                    "header": "Focus B",
                    "question": "Which focus should I use?",
                    "options": [{"label": "alpha"}, {"label": "beta"}]
                }
            ]
        }),
    )
    .expect_err("duplicate question text should fail");
    assert!(
        duplicate_questions
            .to_string()
            .contains("unique question text")
    );

    let duplicate_options = build_user_question_request(
        &context,
        &json!({
            "questions": [{
                "header": "Focus",
                "question": "Which focus should I use?",
                "options": [{"label": "memory"}, {"label": "memory"}]
            }]
        }),
    )
    .expect_err("duplicate option labels should fail");
    assert!(
        duplicate_options
            .to_string()
            .contains("unique option labels")
    );
}

#[test]
fn ask_user_question_accepts_common_model_alias_shapes() -> Result<()> {
    let context = FakeControl::context("session-a", "agent-parent");

    let single = build_user_question_request(
        &context,
        &json!({
            "question": "Should I focus on daemon modules or runtime modules?",
            "options": [{"label": "daemon"}, {"label": "runtime"}]
        }),
    )?;
    assert_eq!(single.questions.len(), 1);
    assert_eq!(single.questions[0].header, "Question 1");
    assert_eq!(
        single.questions[0].question,
        "Should I focus on daemon modules or runtime modules?"
    );

    let aliased = build_user_question_request(
        &context,
        &json!({
            "questions": [{
                "id": "focus",
                "text": "Should I focus on daemon modules or runtime modules?",
                "options": [{"label": "daemon"}, {"label": "runtime"}]
            }]
        }),
    )?;
    assert_eq!(aliased.questions.len(), 1);
    assert_eq!(aliased.questions[0].id, "focus");
    assert_eq!(aliased.questions[0].header, "Question 1");
    assert_eq!(
        aliased.questions[0].question,
        "Should I focus on daemon modules or runtime modules?"
    );
    Ok(())
}

#[test]
fn ask_user_question_accepts_and_validates_expiration_fields() -> Result<()> {
    let context = FakeControl::context("session-a", "agent-parent");
    let absolute = build_user_question_request(
        &context,
        &json!({
            "expires_at_ms": 12345,
            "questions": [{
                "id": "focus",
                "question": "Which focus?",
                "options": [{"label": "daemon"}, {"label": "runtime"}]
            }]
        }),
    )?;
    assert_eq!(absolute.expires_at_ms, Some(12345));

    let relative = build_user_question_request(
        &context,
        &json!({
            "expires_after_ms": 5000,
            "questions": [{
                "id": "focus",
                "question": "Which focus?",
                "options": [{"label": "daemon"}, {"label": "runtime"}]
            }]
        }),
    )?;
    assert_eq!(relative.expires_at_ms, Some(relative.created_at_ms + 5000));

    let relative_float = build_user_question_request(
        &context,
        &json!({
            "expires_after_ms": 5000.0,
            "questions": [{
                "id": "focus",
                "question": "Which focus?",
                "options": [{"label": "daemon"}, {"label": "runtime"}]
            }]
        }),
    )
    .expect_err("integer-like float expiration should fail");
    assert!(relative_float.to_string().contains("non-negative integer"));

    let conflict = build_user_question_request(
        &context,
        &json!({
            "expires_at_ms": 12345,
            "expires_after_ms": 5000,
            "questions": [{
                "id": "focus",
                "question": "Which focus?",
                "options": [{"label": "daemon"}, {"label": "runtime"}]
            }]
        }),
    )
    .expect_err("conflicting expiration fields should fail");
    assert!(conflict.to_string().contains("cannot combine"));

    let zero = build_user_question_request(
        &context,
        &json!({
            "expires_after_ms": 0,
            "questions": [{
                "id": "focus",
                "question": "Which focus?",
                "options": [{"label": "daemon"}, {"label": "runtime"}]
            }]
        }),
    )
    .expect_err("zero relative expiration should fail");
    assert!(zero.to_string().contains("greater than zero"));

    let overflow = build_user_question_request(
        &context,
        &json!({
            "expires_after_ms": u64::MAX,
            "questions": [{
                "id": "focus",
                "question": "Which focus?",
                "options": [{"label": "daemon"}, {"label": "runtime"}]
            }]
        }),
    )
    .expect_err("overflowing relative expiration should fail");
    assert!(overflow.to_string().contains("overflows"));

    let float = build_user_question_request(
        &context,
        &json!({
            "expires_after_ms": 1.5,
            "questions": [{
                "id": "focus",
                "question": "Which focus?",
                "options": [{"label": "daemon"}, {"label": "runtime"}]
            }]
        }),
    )
    .expect_err("non-integer expiration should fail");
    assert!(float.to_string().contains("non-negative integer"));
    Ok(())
}

#[test]
fn ask_user_question_rejects_wrong_typed_optional_fields() {
    let context = FakeControl::context("session-a", "agent-parent");
    for (field, payload) in [
        (
            "id",
            json!({
                "questions": [{
                    "id": 42,
                    "question": "Which focus?",
                    "options": [{"label": "daemon"}, {"label": "runtime"}]
                }]
            }),
        ),
        (
            "header",
            json!({
                "questions": [{
                    "header": false,
                    "question": "Which focus?",
                    "options": [{"label": "daemon"}, {"label": "runtime"}]
                }]
            }),
        ),
        (
            "multi_select",
            json!({
                "questions": [{
                    "question": "Which focus?",
                    "multi_select": "false",
                    "options": [{"label": "daemon"}, {"label": "runtime"}]
                }]
            }),
        ),
        (
            "description",
            json!({
                "questions": [{
                    "question": "Which focus?",
                    "options": [
                        {"label": "daemon", "description": 7},
                        {"label": "runtime"}
                    ]
                }]
            }),
        ),
        (
            "preview",
            json!({
                "questions": [{
                    "question": "Which focus?",
                    "options": [
                        {"label": "daemon", "preview": true},
                        {"label": "runtime"}
                    ]
                }]
            }),
        ),
    ] {
        let error = build_user_question_request(&context, &payload)
            .expect_err("wrong typed optional user question field should fail");
        assert!(
            error.to_string().contains(field),
            "expected error for {field}, got {error}"
        );
    }
}

#[test]
fn ask_user_question_rejects_invalid_questions_field_shape() {
    let context = FakeControl::context("session-a", "agent-parent");

    let wrong_type = build_user_question_request(
        &context,
        &json!({
            "questions": "not-an-array",
            "question": "Which focus?",
            "options": [{"label": "daemon"}, {"label": "runtime"}]
        }),
    )
    .expect_err("wrong typed questions field should not fall back to shorthand");
    assert!(
        wrong_type
            .to_string()
            .contains("questions must be an array")
    );

    let mixed = build_user_question_request(
        &context,
        &json!({
            "questions": [{
                "question": "Which focus?",
                "options": [{"label": "daemon"}, {"label": "runtime"}]
            }],
            "question": "Top-level shorthand should not be mixed"
        }),
    )
    .expect_err("questions array should reject top-level shorthand fields");
    assert!(mixed.to_string().contains("cannot be combined"));
}

#[tokio::test]
async fn task_update_auto_claims_and_notifies_owner_changes() -> Result<()> {
    let control = Arc::new(FakeControl::new());
    control
        .state
        .lock()
        .expect("fake control mutex poisoned")
        .session_control
        .insert(
            "session-a".to_string(),
            SessionControlState {
                plan_mode: false,
                pre_plan_mode: None,
                tasks: vec![TaskRecord {
                    id: "task-1".to_string(),
                    title: "Investigate".to_string(),
                    description: String::new(),
                    status: TaskStatus::Pending,
                    owner_agent_id: None,
                    blocked_by: Vec::new(),
                    blocks: Vec::new(),
                    output: None,
                    metadata: Value::Null,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                }],
                ..SessionControlState::default()
            },
        );
    let handle = bind_control(&control);
    let tool = TaskUpdateTool::new(handle.clone());

    let claimed = tool
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({"task_id": "task-1", "status": "in_progress"}),
        )
        .await?;
    assert_eq!(claimed.output["owner_agent_id"], "agent-parent");

    let reassigned = tool
        .execute(
            FakeControl::context("session-a", "agent-parent"),
            json!({"task_id": "task-1", "owner_agent_id": "agent-child"}),
        )
        .await?;
    assert_eq!(reassigned.output["owner_agent_id"], "agent-child");

    let state = control.state.lock().expect("fake control mutex poisoned");
    assert_eq!(state.mailbox_requests.len(), 1);
    assert_eq!(
        state.mailbox_requests[0].1.message_type.as_deref(),
        Some("task_assignment")
    );
    Ok(())
}
