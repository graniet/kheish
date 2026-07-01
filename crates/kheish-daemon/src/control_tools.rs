//! Daemon-backed orchestration tools exposed to Kheish agents.

mod bash;
mod channels;
mod goal;
mod helpers;
mod output;
mod planning;
mod scheduling;
mod skills;
mod tasks;
mod types;

#[cfg(test)]
mod tests;

use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use kheish_agent::AgentStatus;
use kheish_coding_tools::CodingToolConfig;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolInputKind,
    ToolRuntime, ToolSchema, ToolSchemaField,
};
use serde_json::{Value, json};

use bash::DaemonBashTool;
use channels::{CreateChannelStimulusTool, ReadChannelThreadTool, SetChannelReactionTool};
use goal::{CreateGoalTool, GetGoalTool, UpdateGoalTool};
pub(crate) use helpers::parse_permission_mode;
use helpers::{
    USER_QUESTION_INPUT_EXAMPLE, build_array_field, build_boolean_field,
    build_capability_scope_field, build_credential_scope_field, build_number_field,
    build_object_field, build_string_field, build_user_question_expiration_fields,
    build_user_question_request, build_user_questions_field, execution_agent_id,
    execution_session_id, populate_spawn_request_from_context,
};
pub use helpers::{
    mailbox_request_from_tool, sidechain_request_from_tool, wait_for_agent_snapshot,
};
use output::{EditImageTool, EmitOutputTool, GenerateAudioTool, GenerateImageTool};
use planning::{AskUserQuestionTool, EnterPlanModeTool, ExitPlanModeTool, TodoWriteTool};
use scheduling::{
    ScheduleCancelTool, ScheduleCreateTool, ScheduleGetTool, ScheduleListTool, SchedulePauseTool,
    ScheduleResumeTool, ScheduleTriggerNowTool, WakeAfterTool, WakeAtTool,
};
use skills::{ListSkillsTool, UseSkillTool};
use tasks::{
    TaskCreateTool, TaskDeleteTool, TaskGetTool, TaskListTool, TaskOutputTool, TaskStopTool,
    TaskUpdateTool,
};
use types::AgentProfileTemplate;
pub use types::{
    DaemonToolControl, DaemonToolControlHandle, EditImageToolRequest, EditImageToolResponse,
    ExitPlanModeOutcome, GenerateAudioToolRequest, GenerateAudioToolResponse,
    GenerateImageToolRequest, GenerateImageToolResponse, ImageToolResponse, ImageToolRouteOverride,
    MessageAgentToolRequest, PARENT_CLARIFICATION_ANSWER_MESSAGE_TYPE,
    PARENT_CLARIFICATION_ANSWER_SUBJECT, ParentClarificationToolResponse, SpawnAgentToolRequest,
    SpawnAgentToolResponse, SpawnIsolation, TaskMutation,
};

/// Registers daemon-backed orchestration tools into the provided runtime.
pub fn register_daemon_control_tools(
    runtime: &mut ToolRuntime,
    control: DaemonToolControlHandle,
    coding_config: CodingToolConfig,
    enable_audio_generation: bool,
    enable_image_generation: bool,
    enable_image_edit: bool,
) {
    runtime.register(DaemonBashTool::new(control.clone(), coding_config));
    if enable_audio_generation {
        runtime.register(GenerateAudioTool::new(control.clone()));
    }
    if enable_image_generation {
        runtime.register(GenerateImageTool::new(control.clone()));
    }
    if enable_image_edit {
        runtime.register(EditImageTool::new(control.clone()));
    }
    runtime.register(EmitOutputTool::new(control.clone()));
    runtime.register(ReadChannelThreadTool::new(control.clone()));
    runtime.register(SetChannelReactionTool::new(control.clone()));
    runtime.register(CreateChannelStimulusTool::new(control.clone()));
    runtime.register(ListSkillsTool::new(control.clone()));
    runtime.register(UseSkillTool::new(control.clone()));
    runtime.register(SpawnAgentTool::new(control.clone()));
    runtime.register(MessageAgentTool::new(control.clone()));
    runtime.register(WaitAgentTool::new(control.clone()));
    runtime.register(ListAgentsTool::new(control.clone()));
    runtime.register(ListAgentSummariesTool::new(control.clone()));
    runtime.register(GetAgentTool::new(control.clone()));
    runtime.register(TaskCreateTool::new(control.clone()));
    runtime.register(TaskGetTool::new(control.clone()));
    runtime.register(TaskListTool::new(control.clone()));
    runtime.register(TaskOutputTool::new(control.clone()));
    runtime.register(TaskUpdateTool::new(control.clone()));
    runtime.register(TaskStopTool::new(control.clone()));
    runtime.register(TaskDeleteTool::new(control.clone()));
    runtime.register(GetGoalTool::new(control.clone()));
    runtime.register(CreateGoalTool::new(control.clone()));
    runtime.register(UpdateGoalTool::new(control.clone()));
    runtime.register(TodoWriteTool::new(control.clone()));
    runtime.register(EnterPlanModeTool::new(control.clone()));
    runtime.register(ExitPlanModeTool::new(control.clone()));
    runtime.register(RequestParentClarificationTool::new(control.clone()));
    runtime.register(AskUserQuestionTool);
    runtime.register(WakeAfterTool::new(control.clone()));
    runtime.register(WakeAtTool::new(control.clone()));
    runtime.register(ScheduleCreateTool::new(control.clone()));
    runtime.register(ScheduleListTool::new(control.clone()));
    runtime.register(ScheduleGetTool::new(control.clone()));
    runtime.register(ScheduleCancelTool::new(control.clone()));
    runtime.register(SchedulePauseTool::new(control.clone()));
    runtime.register(ScheduleResumeTool::new(control.clone()));
    runtime.register(ScheduleTriggerNowTool::new(control));
}

fn optional_agent_status_field(input: &Value, name: &str) -> Result<Option<AgentStatus>> {
    input
        .get(name)
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| anyhow!("{name} must be a valid agent status: {error}"))
}

fn optional_bool_field(input: &Value, name: &str) -> Result<Option<bool>> {
    input
        .get(name)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| anyhow!("{name} must be a boolean"))
        })
        .transpose()
}

#[derive(Clone)]
struct SpawnAgentTool {
    control: DaemonToolControlHandle,
}

impl SpawnAgentTool {
    fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for SpawnAgentTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "spawn_agent".to_string(),
            description:
                "Spawn a background child agent with its own isolated session and initial subtask. Provide at least one initial input source: prompt, asset_ids, or input_items."
                    .to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("name", "Short child agent name.", true),
                    build_string_field(
                        "description",
                        "What the child agent is responsible for.",
                        false,
                    ),
                    build_string_field(
                        "prompt",
                        "Legacy plain-text initial task prompt for the child agent. Provide this, asset_ids, or input_items.",
                        false,
                    ),
                    build_array_field(
                        "asset_ids",
                        "Optional daemon-owned asset IDs appended after prompt. Provide this, prompt, or input_items. Prefer this simpler field for one text prompt plus existing assets, and do not paste asset IDs into prompt text. Use input_items only when you need precise multimodal ordering or inline uploads.",
                        false,
                        ToolInputKind::String,
                    ),
                    build_array_field(
                        "input_items",
                        "Ordered multimodal child input items [{type, text? | asset_id? | file_name?, media_type?, content_base64?}]. Provide this, prompt, or asset_ids. Use this instead of prompt for text plus assets.",
                        false,
                        ToolInputKind::Object,
                    ),
                    build_string_field(
                        "agent_type",
                        "Optional built-in child profile: default, plan, verification, coordinator.",
                        false,
                    ),
                    build_string_field(
                        "system_prompt",
                        "Optional custom system prompt for the child.",
                        false,
                    ),
                    build_string_field(
                        "prompt_merge_mode",
                        "Optional prompt merge mode: replace or append.",
                        false,
                    ),
                    build_string_field("model", "Optional child model override.", false),
                    build_string_field(
                        "provider",
                        "Optional child provider override using any configured daemon provider name.",
                        false,
                    ),
                    build_string_field(
                        "fallback_model",
                        "Optional child fallback model override.",
                        false,
                    ),
                    build_object_field(
                        "generation",
                        "Optional full child generation override. Use this for provider-neutral settings such as reasoning; model and fallback_model are shortcuts and must not conflict.",
                        false,
                    ),
                    build_string_field(
                        "mode",
                        "Optional child permission mode: default, acceptEdits, bypassPermissions, plan, dontAsk. The daemon rejects modes that would be more permissive than the parent session.",
                        false,
                    ),
                    build_string_field(
                        "retention",
                        "Optional child retention policy: retain or close_on_settle.",
                        false,
                    ),
                    build_string_field(
                        "nickname",
                        "Optional human-friendly child nickname.",
                        false,
                    ),
                    build_array_field(
                        "allowed_tools",
                        "Optional tool allow-list for the child.",
                        false,
                        ToolInputKind::String,
                    ),
                    build_array_field(
                        "blocked_tools",
                        "Optional tool deny-list for the child.",
                        false,
                        ToolInputKind::String,
                    ),
                    build_capability_scope_field(),
                    build_credential_scope_field(),
                    build_string_field("session_id", "Optional child session identifier.", false),
                    build_string_field("thread_id", "Optional child thread identifier.", false),
                    build_string_field(
                        "cwd",
                        "Optional child workspace root or working directory.",
                        false,
                    ),
                    build_string_field(
                        "team_name",
                        "Optional team label used for coordination metadata.",
                        false,
                    ),
                    build_string_field(
                        "isolation",
                        "Optional isolation mode: shared or worktree.",
                        false,
                    ),
                    build_boolean_field(
                        "wait",
                        "When true, wait for the child to settle before returning.",
                        false,
                    ),
                    build_boolean_field(
                        "run_in_background",
                        "When false, wait for the child run instead of returning immediately.",
                        false,
                    ),
                    build_number_field(
                        "timeout_ms",
                        "Optional wait timeout in milliseconds.",
                        false,
                    ),
                ],
            },
            timeout_ms: 120_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let parent_agent_id = execution_agent_id(&ctx)?;
        let mut request = serde_json::from_value::<SpawnAgentToolRequest>(input)?;
        let control = self.control.resolve()?;
        populate_spawn_request_from_context(control.as_ref(), &ctx, &mut request).await?;
        let wait = request.wait || !request.run_in_background;
        let timeout_ms = request.timeout_ms.unwrap_or(60_000);
        let mut response = control.spawn_agent(parent_agent_id, request).await?;
        if wait {
            let snapshot = control
                .wait_agent(
                    parent_agent_id,
                    &response.agent_id,
                    Duration::from_millis(timeout_ms),
                )
                .await?;
            response.status = format!("{:?}", snapshot.agent.status).to_ascii_lowercase();
            response.snapshot = Some(snapshot);
            response.run_in_background = false;
        }
        Ok(ToolExecutionOutput::json(serde_json::to_value(response)?))
    }
}

#[derive(Clone)]
struct MessageAgentTool {
    control: DaemonToolControlHandle,
}

impl MessageAgentTool {
    fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for MessageAgentTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "message_agent".to_string(),
            description: "Send a mailbox message to another background agent. Provide message, asset_ids, or input_items.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("agent_id", "Destination agent identifier.", true),
                    build_string_field("subject", "Short subject for the handoff.", true),
                    build_string_field(
                        "message",
                        "Legacy plain-text message body to send. Provide this, asset_ids, or input_items.",
                        false,
                    ),
                    build_array_field(
                        "asset_ids",
                        "Optional daemon-owned asset IDs appended after message. Provide this, message, or input_items. Prefer this simpler field for one text message plus existing assets, and do not paste asset IDs into message text. Use input_items only when you need precise multimodal ordering or inline uploads.",
                        false,
                        ToolInputKind::String,
                    ),
                    build_array_field(
                        "input_items",
                        "Ordered multimodal message items [{type, text? | asset_id? | file_name?, media_type?, content_base64?}]. Provide this, message, or asset_ids. Use this instead of message for text plus assets.",
                        false,
                        ToolInputKind::Object,
                    ),
                    build_string_field(
                        "message_type",
                        "Optional structured mailbox message type.",
                        false,
                    ),
                ],
            },
            timeout_ms: 15_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let from_agent_id = execution_agent_id(&ctx)?;
        let request = serde_json::from_value::<MessageAgentToolRequest>(input)?;
        let response = self
            .control
            .resolve()?
            .message_agent(from_agent_id, request.clone())
            .await?;
        Ok(ToolExecutionOutput::json(json!({
            "queued": response.accepted,
            "message_id": response.message_id,
            "duplicate": response.duplicate,
            "agent_id": request.agent_id,
            "subject": request.subject,
            "message_type": request.message_type,
        })))
    }
}

#[derive(Clone)]
struct RequestParentClarificationTool {
    control: DaemonToolControlHandle,
}

impl RequestParentClarificationTool {
    fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for RequestParentClarificationTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "request_parent_clarification".to_string(),
            description: format!(
                "Surface one structured clarification request through your parent session. Use this from subagents instead of ask_user_question. Pass input like {USER_QUESTION_INPUT_EXAMPLE}. The user's answer will arrive later through mailbox messages with payload.type=`{}`.",
                PARENT_CLARIFICATION_ANSWER_MESSAGE_TYPE
            ),
            schema: ToolSchema {
                fields: {
                    let mut fields = vec![build_user_questions_field()];
                    fields.extend(build_user_question_expiration_fields());
                    fields
                },
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let requester_agent_id = execution_agent_id(&ctx)?;
        let request = build_user_question_request(&ctx, &input)?;
        let requester_run_id = ctx
            .metadata
            .get("run_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let requester_tool_call_id = ctx
            .metadata
            .get("tool_call_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| Some(ctx.call_id.clone()));
        let response = self
            .control
            .resolve()?
            .request_parent_clarification(
                session_id,
                requester_agent_id,
                requester_run_id.as_deref(),
                requester_tool_call_id.as_deref(),
                request,
            )
            .await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(response)?))
    }
}

#[derive(Clone)]
struct WaitAgentTool {
    control: DaemonToolControlHandle,
}

impl WaitAgentTool {
    fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for WaitAgentTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "wait_agent".to_string(),
            description: "Wait for a background agent to settle or time out.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("agent_id", "Agent identifier to wait for.", true),
                    ToolSchemaField {
                        name: "timeout_ms".to_string(),
                        kind: ToolInputKind::Number,
                        item_kind: None,
                        structured_schema: None,
                        required: false,
                        description: Some("Maximum wait time in milliseconds.".to_string()),
                    },
                ],
            },
            timeout_ms: 120_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let caller_agent_id = execution_agent_id(&ctx)?;
        let agent_id = input
            .get("agent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("agent_id is required"))?;
        let timeout_ms = input
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000);
        let snapshot = self
            .control
            .resolve()?
            .wait_agent(caller_agent_id, agent_id, Duration::from_millis(timeout_ms))
            .await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(snapshot)?))
    }
}

#[derive(Clone)]
struct ListAgentsTool {
    control: DaemonToolControlHandle,
}

impl ListAgentsTool {
    fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[derive(Clone)]
struct ListAgentSummariesTool {
    control: DaemonToolControlHandle,
}

impl ListAgentSummariesTool {
    fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[derive(Clone)]
struct GetAgentTool {
    control: DaemonToolControlHandle,
}

impl GetAgentTool {
    fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for GetAgentTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "get_agent".to_string(),
            description: "Fetch one supervised agent snapshot by identifier.".to_string(),
            schema: ToolSchema {
                fields: vec![build_string_field("agent_id", "Agent identifier.", true)],
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let caller_agent_id = execution_agent_id(&ctx)?;
        let agent_id = input
            .get("agent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("agent_id is required"))?;
        let snapshot = self
            .control
            .resolve()?
            .get_agent(caller_agent_id, agent_id)
            .await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(snapshot)?))
    }
}

#[async_trait]
impl Tool for ListAgentsTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "list_agents".to_string(),
            description: "List full snapshots for the currently visible background agents."
                .to_string(),
            schema: ToolSchema::default(),
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, _input: Value) -> Result<ToolExecutionOutput> {
        let caller_agent_id = execution_agent_id(&ctx)?;
        let agents = self.control.resolve()?.list_agents(caller_agent_id).await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(agents)?))
    }
}

#[async_trait]
impl Tool for ListAgentSummariesTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "list_agent_summaries".to_string(),
            description: "List lightweight summaries for the currently visible background agents."
                .to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field(
                        "session_id",
                        "Optional session id filter within the caller-visible agent tree.",
                        false,
                    ),
                    build_string_field(
                        "status",
                        "Optional lifecycle status filter: idle, running, waiting_for_approval, waiting_for_user_input, failed, or completed.",
                        false,
                    ),
                    build_boolean_field(
                        "has_runtime",
                        "Optional filter for whether the daemon currently has a live runtime actor.",
                        false,
                    ),
                ],
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let caller_agent_id = execution_agent_id(&ctx)?;
        let status = optional_agent_status_field(&input, "status")?;
        let session_id = input.get("session_id").and_then(Value::as_str);
        let has_runtime = optional_bool_field(&input, "has_runtime")?;
        let agents = self
            .control
            .resolve()?
            .list_agent_summaries(caller_agent_id)
            .await?
            .into_iter()
            .filter(|summary| session_id.is_none_or(|session_id| summary.session_id == session_id))
            .filter(|summary| {
                status
                    .as_ref()
                    .is_none_or(|status| &summary.status == status)
            })
            .filter(|summary| {
                has_runtime.is_none_or(|has_runtime| summary.has_runtime == has_runtime)
            })
            .collect::<Vec<_>>();
        Ok(ToolExecutionOutput::json(serde_json::to_value(agents)?))
    }
}
