use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use chrono::DateTime;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolSchema,
};
use kheish_types::ModelGenerationConfig;
use serde_json::{Value, json};
use tracing::warn;

use crate::{ScheduleCadence, ScheduleCreateRequest, ScheduleMisfirePolicy, ScheduleOverlapPolicy};

use super::DaemonToolControlHandle;
use super::helpers::{
    build_number_field, build_string_field, execution_agent_id, execution_session_id,
    optional_string_field, optional_u64_field, string_field,
};

#[derive(Clone)]
pub(super) struct WakeAfterTool {
    control: DaemonToolControlHandle,
}

#[derive(Clone)]
pub(super) struct WakeAtTool {
    control: DaemonToolControlHandle,
}

#[derive(Clone)]
pub(super) struct ScheduleCreateTool {
    control: DaemonToolControlHandle,
}

#[derive(Clone)]
pub(super) struct ScheduleListTool {
    control: DaemonToolControlHandle,
}

#[derive(Clone)]
pub(super) struct ScheduleGetTool {
    control: DaemonToolControlHandle,
}

#[derive(Clone)]
pub(super) struct ScheduleCancelTool {
    control: DaemonToolControlHandle,
}

#[derive(Clone)]
pub(super) struct SchedulePauseTool {
    control: DaemonToolControlHandle,
}

#[derive(Clone)]
pub(super) struct ScheduleResumeTool {
    control: DaemonToolControlHandle,
}

#[derive(Clone)]
pub(super) struct ScheduleTriggerNowTool {
    control: DaemonToolControlHandle,
}

#[derive(Clone, Copy)]
enum ScheduleTargetKind {
    SelfAgent,
    Parent,
    Session,
    Agent,
}

impl WakeAfterTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

impl WakeAtTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

impl ScheduleCreateTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

impl ScheduleListTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

impl ScheduleGetTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

impl ScheduleCancelTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

impl SchedulePauseTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

impl ScheduleResumeTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

impl ScheduleTriggerNowTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for WakeAfterTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "wake_after".to_string(),
            description: "Create one durable wake-up that submits a new scheduled message later. Use this instead of sleeping in-process.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_number_field("delay_seconds", "Delay before the wake-up fires.", true),
                    build_string_field("message", "The message or instruction to deliver later.", true),
                    build_string_field("target", "Optional target: self, parent, session, or agent.", false),
                    build_string_field("session_id", "Required when target=session.", false),
                    build_string_field("agent_id", "Required when target=agent.", false),
                    build_string_field("provider", "Optional provider override for the later run.", false),
                    build_string_field("model", "Optional model override for the later run.", false),
                    build_string_field("fallback_model", "Optional fallback model override for the later run.", false),
                ],
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let delay_seconds = optional_u64_field(&input, "delay_seconds")
            .ok_or_else(|| anyhow!("delay_seconds is required"))?;
        let request = build_schedule_request(
            &self.control,
            &ctx,
            &input,
            ScheduleCadence::Once {
                fire_at_ms: crate::now_ms().saturating_add(delay_seconds.saturating_mul(1000)),
            },
            Some(format!("wake-{}", delay_seconds)),
        )
        .await?;
        let schedule = self.control.resolve()?.create_schedule(request).await?;
        Ok(ToolExecutionOutput::json(json!({ "schedule": schedule })))
    }
}

#[async_trait]
impl Tool for WakeAtTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "wake_at".to_string(),
            description: "Create one durable wake-up at an exact RFC3339 timestamp.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field(
                        "at",
                        "The RFC3339 timestamp when the wake-up should fire.",
                        true,
                    ),
                    build_string_field(
                        "message",
                        "The message or instruction to deliver later.",
                        true,
                    ),
                    build_string_field(
                        "target",
                        "Optional target: self, parent, session, or agent.",
                        false,
                    ),
                    build_string_field("session_id", "Required when target=session.", false),
                    build_string_field("agent_id", "Required when target=agent.", false),
                    build_string_field(
                        "provider",
                        "Optional provider override for the later run.",
                        false,
                    ),
                    build_string_field(
                        "model",
                        "Optional model override for the later run.",
                        false,
                    ),
                    build_string_field(
                        "fallback_model",
                        "Optional fallback model override for the later run.",
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
        let at = string_field(&input, "at")?;
        let fire_at_ms = parse_fire_at_ms(&at)?;
        let request = build_schedule_request(
            &self.control,
            &ctx,
            &input,
            ScheduleCadence::Once { fire_at_ms },
            Some("wake-at".to_string()),
        )
        .await?;
        let schedule = self.control.resolve()?.create_schedule(request).await?;
        Ok(ToolExecutionOutput::json(json!({ "schedule": schedule })))
    }
}

#[async_trait]
impl Tool for ScheduleCreateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "schedule_create".to_string(),
            description: "Create one durable recurring or one-shot schedule owned by the daemon."
                .to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("name", "Human-readable schedule name.", true),
                    build_string_field(
                        "message",
                        "The message or instruction to deliver when the schedule fires.",
                        true,
                    ),
                    build_string_field(
                        "target",
                        "Optional target: self, parent, session, or agent.",
                        false,
                    ),
                    build_string_field("session_id", "Required when target=session.", false),
                    build_string_field("agent_id", "Required when target=agent.", false),
                    build_string_field(
                        "at",
                        "Optional RFC3339 timestamp for a one-shot schedule.",
                        false,
                    ),
                    build_number_field(
                        "every_seconds",
                        "Optional fixed interval in seconds.",
                        false,
                    ),
                    build_string_field(
                        "cron",
                        "Optional cron expression for recurring schedules.",
                        false,
                    ),
                    build_string_field(
                        "timezone",
                        "Optional IANA timezone for cron schedules.",
                        false,
                    ),
                    build_string_field(
                        "overlap_policy",
                        "Optional overlap policy: skip, queue_one, or parallel.",
                        false,
                    ),
                    build_string_field(
                        "misfire_policy",
                        "Optional misfire policy: coalesce_once or skip_missed.",
                        false,
                    ),
                    build_number_field(
                        "max_executions",
                        "Optional maximum number of dispatches before completion.",
                        false,
                    ),
                    build_string_field(
                        "provider",
                        "Optional provider override for later runs.",
                        false,
                    ),
                    build_string_field("model", "Optional model override for later runs.", false),
                    build_string_field(
                        "fallback_model",
                        "Optional fallback model override for later runs.",
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
        let cadence = parse_schedule_cadence(&input)?;
        let request = build_schedule_request(&self.control, &ctx, &input, cadence, None)
            .await
            .inspect_err(|error| {
                warn!(error = ?error, input = ?input, "failed to build schedule_create request");
            })?;
        let schedule = self
            .control
            .resolve()?
            .create_schedule(request)
            .await
            .inspect_err(|error| {
                warn!(error = ?error, "failed to persist schedule_create request");
            })?;
        Ok(ToolExecutionOutput::json(json!({ "schedule": schedule })))
    }
}

#[async_trait]
impl Tool for ScheduleListTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "schedule_list".to_string(),
            description: "List schedules owned by the current session.".to_string(),
            schema: ToolSchema { fields: Vec::new() },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, _input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let schedules = self
            .control
            .resolve()?
            .list_schedules(Some(session_id))
            .await?;
        Ok(ToolExecutionOutput::json(json!({ "schedules": schedules })))
    }
}

#[async_trait]
impl Tool for ScheduleGetTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "schedule_get".to_string(),
            description: "Load one schedule by identifier.".to_string(),
            schema: ToolSchema {
                fields: vec![build_string_field(
                    "schedule_id",
                    "The schedule identifier.",
                    true,
                )],
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let schedule =
            load_owned_schedule(&self.control, execution_session_id(&ctx)?, &input).await?;
        Ok(ToolExecutionOutput::json(json!({ "schedule": schedule })))
    }
}

macro_rules! impl_schedule_mutation_tool {
    ($tool:ident, $name:literal, $description:literal, $method:ident) => {
        #[async_trait]
        impl Tool for $tool {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor {
                    name: $name.to_string(),
                    description: $description.to_string(),
                    schema: ToolSchema {
                        fields: vec![build_string_field("schedule_id", "The schedule identifier.", true)],
                    },
                    timeout_ms: 10_000,
                    sandbox: SandboxProfile::Inherited,
                    allows_parallel: true,
                }
            }

            async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
                let session_id = execution_session_id(&ctx)?;
                let schedule_id = string_field(&input, "schedule_id")?;
                let existing = self.control.resolve()?.get_schedule(&schedule_id).await?;
                ensure_schedule_visible(session_id, &existing)?;
                let schedule = self.control.resolve()?.$method(&schedule_id).await?;
                Ok(ToolExecutionOutput::json(json!({ "schedule": schedule })))
            }
        }
    };
}

impl_schedule_mutation_tool!(
    ScheduleCancelTool,
    "schedule_cancel",
    "Cancel one durable schedule.",
    cancel_schedule
);
impl_schedule_mutation_tool!(
    SchedulePauseTool,
    "schedule_pause",
    "Pause one active schedule without deleting it.",
    pause_schedule
);
impl_schedule_mutation_tool!(
    ScheduleResumeTool,
    "schedule_resume",
    "Resume one paused schedule.",
    resume_schedule
);
impl_schedule_mutation_tool!(
    ScheduleTriggerNowTool,
    "schedule_trigger_now",
    "Trigger one schedule immediately while still respecting overlap policy.",
    trigger_schedule_now
);

async fn build_schedule_request(
    control: &DaemonToolControlHandle,
    ctx: &ToolContext,
    input: &Value,
    cadence: ScheduleCadence,
    default_name: Option<String>,
) -> Result<ScheduleCreateRequest> {
    let session_id = execution_session_id(ctx)?.to_string();
    let agent_id = execution_agent_id(ctx)?.to_string();
    let (target_session_id, target_agent_id) =
        resolve_schedule_target(control, &session_id, &agent_id, input).await?;
    let generation = generation_override_from_input(input);
    let request = ScheduleCreateRequest {
        name: optional_string_field(input, "name")
            .or(default_name)
            .unwrap_or_else(|| "scheduled-wakeup".to_string()),
        target_session_id,
        target_agent_id,
        owner_session_id: Some(session_id),
        owner_agent_id: Some(agent_id.clone()),
        created_by_run_id: ctx
            .metadata
            .get("run_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        cadence,
        max_executions: optional_u64_field(input, "max_executions"),
        overlap_policy: parse_overlap_policy(input.get("overlap_policy").and_then(Value::as_str))?,
        misfire_policy: parse_misfire_policy(input.get("misfire_policy").and_then(Value::as_str))?,
        request: Some(crate::SubmitInputRequest {
            provider: optional_string_field(input, "provider"),
            source_plugin: Some("scheduler".to_string()),
            source_kind: Some("agent_schedule".to_string()),
            actor_id: Some(agent_id),
            content: string_field(input, "message")?,
            input_items: Vec::new(),
            attachments: Vec::new(),
            generation,
            completion_requirements: None,
            metadata: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
            reply_address: None,
        }),
        observation_materialization: None,
    };
    Ok(request)
}

fn generation_override_from_input(input: &Value) -> Option<ModelGenerationConfig> {
    let model = optional_string_field(input, "model");
    let fallback_model = optional_string_field(input, "fallback_model");
    if model.is_none() && fallback_model.is_none() {
        return None;
    }
    Some(ModelGenerationConfig {
        model,
        fallback_model,
        ..ModelGenerationConfig::default()
    })
}

fn parse_schedule_cadence(input: &Value) -> Result<ScheduleCadence> {
    let at = optional_string_field(input, "at");
    let every_seconds = optional_u64_field(input, "every_seconds");
    let cron = optional_string_field(input, "cron");
    let mode_count = [at.is_some(), every_seconds.is_some(), cron.is_some()]
        .into_iter()
        .filter(|flag| *flag)
        .count();
    if mode_count != 1 {
        bail!("schedule_create requires exactly one of at, every_seconds, or cron");
    }
    if let Some(at) = at {
        return Ok(ScheduleCadence::Once {
            fire_at_ms: parse_fire_at_ms(&at)?,
        });
    }
    if let Some(every_seconds) = every_seconds {
        return Ok(ScheduleCadence::Interval { every_seconds });
    }
    Ok(ScheduleCadence::Cron {
        expression: cron.expect("cron is present when mode_count==1"),
        timezone: optional_string_field(input, "timezone")
            .unwrap_or_else(crate::scheduler::default_schedule_timezone),
    })
}

async fn resolve_schedule_target(
    control: &DaemonToolControlHandle,
    session_id: &str,
    agent_id: &str,
    input: &Value,
) -> Result<(String, Option<String>)> {
    let target = match input
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("self")
        .trim()
    {
        "self" => ScheduleTargetKind::SelfAgent,
        "parent" => ScheduleTargetKind::Parent,
        "session" => ScheduleTargetKind::Session,
        "agent" => ScheduleTargetKind::Agent,
        other => bail!("unknown target {other}"),
    };
    match target {
        ScheduleTargetKind::SelfAgent => Ok((session_id.to_string(), Some(agent_id.to_string()))),
        ScheduleTargetKind::Parent => {
            let agent = control.resolve()?.get_agent(agent_id, agent_id).await?;
            let parent = agent
                .agent
                .parent
                .ok_or_else(|| anyhow!("current agent has no parent"))?;
            let parent_snapshot = control.resolve()?.get_agent(agent_id, &parent.0).await?;
            Ok((
                parent_snapshot.agent.conversation.session_id,
                Some(parent_snapshot.agent.id.0),
            ))
        }
        ScheduleTargetKind::Session => {
            let target_session_id = string_field(input, "session_id")?;
            if target_session_id != session_id {
                bail!("target=session may only target the current session");
            }
            Ok((target_session_id, Some(agent_id.to_string())))
        }
        ScheduleTargetKind::Agent => {
            let target_agent_id = string_field(input, "agent_id")?;
            let current_agent = control.resolve()?.get_agent(agent_id, agent_id).await?;
            let is_parent = current_agent
                .agent
                .parent
                .as_ref()
                .map(|parent| parent.0.as_str() == target_agent_id)
                .unwrap_or(false);
            anyhow::ensure!(
                target_agent_id == agent_id || is_parent,
                "target=agent may only target the current agent or its parent"
            );
            let snapshot = control
                .resolve()?
                .get_agent(agent_id, &target_agent_id)
                .await?;
            Ok((
                snapshot.agent.conversation.session_id,
                Some(snapshot.agent.id.0),
            ))
        }
    }
}

fn parse_overlap_policy(value: Option<&str>) -> Result<ScheduleOverlapPolicy> {
    match value.unwrap_or("skip") {
        "skip" => Ok(ScheduleOverlapPolicy::Skip),
        "queue_one" => Ok(ScheduleOverlapPolicy::QueueOne),
        "parallel" => Ok(ScheduleOverlapPolicy::Parallel),
        other => bail!("unknown overlap_policy {other}"),
    }
}

fn parse_misfire_policy(value: Option<&str>) -> Result<ScheduleMisfirePolicy> {
    match value.unwrap_or("coalesce_once") {
        "coalesce_once" => Ok(ScheduleMisfirePolicy::CoalesceOnce),
        "skip_missed" => Ok(ScheduleMisfirePolicy::SkipMissed),
        other => bail!("unknown misfire_policy {other}"),
    }
}

fn parse_fire_at_ms(value: &str) -> Result<u64> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|error| anyhow!("invalid RFC3339 timestamp {value:?}: {error}"))?;
    let millis = parsed.timestamp_millis();
    if millis <= 0 {
        bail!("timestamp must be after the Unix epoch");
    }
    Ok(millis as u64)
}

async fn load_owned_schedule(
    control: &DaemonToolControlHandle,
    session_id: &str,
    input: &Value,
) -> Result<crate::ScheduleView> {
    let schedule_id = string_field(input, "schedule_id")?;
    let schedule = control.resolve()?.get_schedule(&schedule_id).await?;
    ensure_schedule_visible(session_id, &schedule)?;
    Ok(schedule)
}

fn ensure_schedule_visible(session_id: &str, schedule: &crate::ScheduleView) -> Result<()> {
    if schedule.owner_session_id.as_deref() == Some(session_id)
        || schedule.target_session_id == session_id
    {
        return Ok(());
    }
    bail!(
        "schedule {} is not visible to this session",
        schedule.schedule_id
    )
}
