use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolInputKind,
    ToolSchema,
};
use kheish_types::{TaskRecord, TaskStatus};
use serde_json::{Value, json};

use crate::shell_tasks::{
    BACKGROUND_SHELL_TASK_KIND, DEFAULT_TASK_OUTPUT_TAIL_BYTES, MAX_TASK_OUTPUT_TAIL_BYTES,
    background_shell_metadata,
};

use super::helpers::{
    build_array_field, build_boolean_field, build_number_field, build_object_field,
    build_string_field, execution_agent_id, execution_run_id, execution_session_id,
    optional_u64_field, optional_usize_field,
};
use super::{DaemonToolControl, DaemonToolControlHandle, MessageAgentToolRequest, TaskMutation};

#[derive(Clone)]
pub(super) struct TaskCreateTool {
    control: DaemonToolControlHandle,
}

impl TaskCreateTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for TaskCreateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "task_create".to_string(),
            description: "Create a structured task in the current session.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("title", "Short task title.", true),
                    build_string_field("description", "Detailed task description.", false),
                    build_string_field("owner_agent_id", "Optional initial task owner.", false),
                    build_object_field("metadata", "Optional structured task metadata.", false),
                    build_array_field(
                        "blocked_by",
                        "Optional task IDs blocking this task.",
                        false,
                        ToolInputKind::String,
                    ),
                    build_array_field(
                        "blocks",
                        "Optional task IDs blocked by this task.",
                        false,
                        ToolInputKind::String,
                    ),
                ],
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let actor_agent_id = execution_agent_id(&ctx)?.to_string();
        let mut state = self
            .control
            .resolve()?
            .load_session_control_state(session_id)
            .await?;
        let title = input
            .get("title")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("title is required"))?;
        let now = crate::now_ms();
        ensure_unique_active_task_title(&state.tasks, title, None)?;
        ensure_known_dependencies(&state.tasks, &string_array(input.get("blocked_by")))?;
        ensure_known_dependencies(&state.tasks, &string_array(input.get("blocks")))?;
        let metadata = task_metadata_with_created_by_run_id(
            input.get("metadata").cloned().unwrap_or(Value::Null),
            execution_run_id(&ctx),
        )?;
        let task = TaskRecord {
            id: next_task_id(&state.tasks, now),
            title: title.to_string(),
            description: input
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            status: TaskStatus::Pending,
            owner_agent_id: input
                .get("owner_agent_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            blocked_by: string_array(input.get("blocked_by")),
            blocks: string_array(input.get("blocks")),
            output: None,
            metadata,
            created_at_ms: now,
            updated_at_ms: now,
        };
        state.tasks.push(task.clone());
        let state = self
            .control
            .resolve()?
            .save_session_control_state(session_id, state)
            .await?;
        if let Some(owner_agent_id) = task.owner_agent_id.as_ref() {
            if owner_agent_id != &actor_agent_id {
                let _ = self
                    .control
                    .resolve()?
                    .message_agent(
                        &actor_agent_id,
                        MessageAgentToolRequest {
                            agent_id: owner_agent_id.clone(),
                            subject: "task_assignment".to_string(),
                            message: format!("Task {} assigned: {}", task.id, task.title),
                            asset_ids: Vec::new(),
                            input_items: Vec::new(),
                            message_type: Some("task_assignment".to_string()),
                        },
                    )
                    .await;
            }
        }
        Ok(ToolExecutionOutput::json(json!({
            "task": task,
            "task_count": state.tasks.len(),
        })))
    }
}

#[derive(Clone)]
pub(super) struct TaskGetTool {
    control: DaemonToolControlHandle,
}

impl TaskGetTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for TaskGetTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "task_get".to_string(),
            description: "Fetch one structured task from the current session.".to_string(),
            schema: ToolSchema {
                fields: vec![build_string_field("task_id", "Task identifier.", true)],
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let task_id = input
            .get("task_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("task_id is required"))?;
        let state = self
            .control
            .resolve()?
            .load_session_control_state(session_id)
            .await?;
        let task = state
            .tasks
            .into_iter()
            .find(|task| task.id == task_id)
            .ok_or_else(|| anyhow!("unknown task {task_id}"))?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(task)?))
    }
}

#[derive(Clone)]
pub(super) struct TaskListTool {
    control: DaemonToolControlHandle,
}

impl TaskListTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for TaskListTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "task_list".to_string(),
            description: "List structured tasks in the current session.".to_string(),
            schema: ToolSchema {
                fields: vec![build_string_field(
                    "status",
                    "Optional status filter (pending, in_progress, blocked, completed, failed, cancelled).",
                    false,
                )],
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let filter = input
            .get("status")
            .and_then(Value::as_str)
            .map(parse_task_status)
            .transpose()?;
        let state = self
            .control
            .resolve()?
            .load_session_control_state(session_id)
            .await?;
        let tasks: Vec<TaskRecord> = state
            .tasks
            .into_iter()
            .filter(|task| {
                filter
                    .as_ref()
                    .map(|status| &task.status == status)
                    .unwrap_or(true)
            })
            .collect();
        Ok(ToolExecutionOutput::json(serde_json::to_value(tasks)?))
    }
}

#[derive(Clone)]
pub(super) struct TaskOutputTool {
    control: DaemonToolControlHandle,
}

impl TaskOutputTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for TaskOutputTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "task_output".to_string(),
            description: "Read persisted task output, optionally waiting for the task to leave pending or in_progress. retrieval_status=success means the output was read; it does not mean the command succeeded. For shell tasks, success requires task.status=completed and metadata.exit_code=0. If task.status is failed or cancelled, do not report the task as successful."
                .to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("task_id", "Task identifier.", true),
                    build_boolean_field(
                        "wait",
                        "Whether to wait for the task to leave pending or in_progress.",
                        false,
                    ),
                    build_number_field(
                        "timeout_ms",
                        "Optional maximum wait time in milliseconds.",
                        false,
                    ),
                    build_number_field(
                        "tail_bytes",
                        "Optional maximum number of output bytes returned from the task tail.",
                        false,
                    ),
                    build_boolean_field(
                        "full",
                        "Whether to include the full retained output body.",
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
        let session_id = execution_session_id(&ctx)?;
        let task_id = input
            .get("task_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("task_id is required"))?;
        let wait = input.get("wait").and_then(Value::as_bool).unwrap_or(true);
        let timeout_ms = optional_u64_field(&input, "timeout_ms").unwrap_or(30_000);
        let tail_bytes = optional_usize_field(&input, "tail_bytes")
            .unwrap_or(DEFAULT_TASK_OUTPUT_TAIL_BYTES)
            .min(MAX_TASK_OUTPUT_TAIL_BYTES);
        let full = input.get("full").and_then(Value::as_bool).unwrap_or(false);
        let view = self
            .control
            .resolve()?
            .task_output_view(
                session_id,
                task_id,
                wait,
                Duration::from_millis(timeout_ms),
                tail_bytes,
                full,
            )
            .await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(view)?))
    }
}

#[derive(Clone)]
pub(super) struct TaskUpdateTool {
    control: DaemonToolControlHandle,
}

impl TaskUpdateTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for TaskUpdateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "task_update".to_string(),
            description: "Update a structured task in the current session.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("task_id", "Task identifier.", true),
                    build_string_field("title", "Optional title replacement.", false),
                    build_string_field("description", "Optional description replacement.", false),
                    build_string_field("status", "Optional status replacement.", false),
                    build_string_field("owner_agent_id", "Optional owner override.", false),
                    build_string_field("output", "Optional task output.", false),
                    build_object_field("metadata", "Optional metadata merge payload.", false),
                    build_array_field(
                        "remove_metadata_keys",
                        "Optional metadata keys to delete after merge.",
                        false,
                        ToolInputKind::String,
                    ),
                    build_array_field(
                        "blocked_by",
                        "Optional replacement dependency list.",
                        false,
                        ToolInputKind::String,
                    ),
                    build_array_field(
                        "add_blocked_by",
                        "Optional blocking task IDs to append.",
                        false,
                        ToolInputKind::String,
                    ),
                    build_array_field(
                        "remove_blocked_by",
                        "Optional blocking task IDs to remove.",
                        false,
                        ToolInputKind::String,
                    ),
                    build_array_field(
                        "blocks",
                        "Optional replacement downstream dependency list.",
                        false,
                        ToolInputKind::String,
                    ),
                    build_array_field(
                        "add_blocks",
                        "Optional downstream task IDs to append.",
                        false,
                        ToolInputKind::String,
                    ),
                    build_array_field(
                        "remove_blocks",
                        "Optional downstream task IDs to remove.",
                        false,
                        ToolInputKind::String,
                    ),
                ],
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let actor_agent_id = execution_agent_id(&ctx)?.to_string();
        let task_id = input
            .get("task_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("task_id is required"))?;
        let mutation = TaskMutation {
            title: input
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
            description: input
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            status: input
                .get("status")
                .and_then(Value::as_str)
                .map(parse_task_status)
                .transpose()?,
            owner_agent_id: input
                .get("owner_agent_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            output: input
                .get("output")
                .and_then(Value::as_str)
                .map(str::to_string),
            metadata: input.get("metadata").cloned(),
            remove_metadata_keys: input
                .get("remove_metadata_keys")
                .map(|value| string_array(Some(value))),
            blocked_by: input
                .get("blocked_by")
                .map(|value| string_array(Some(value))),
            add_blocked_by: input
                .get("add_blocked_by")
                .map(|value| string_array(Some(value))),
            remove_blocked_by: input
                .get("remove_blocked_by")
                .map(|value| string_array(Some(value))),
            blocks: input.get("blocks").map(|value| string_array(Some(value))),
            add_blocks: input
                .get("add_blocks")
                .map(|value| string_array(Some(value))),
            remove_blocks: input
                .get("remove_blocks")
                .map(|value| string_array(Some(value))),
        };
        let state = update_task(
            self.control.resolve()?,
            session_id,
            task_id,
            mutation,
            Some(actor_agent_id.as_str()),
        )
        .await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(state)?))
    }
}

#[derive(Clone)]
pub(super) struct TaskStopTool {
    control: DaemonToolControlHandle,
}

impl TaskStopTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for TaskStopTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "task_stop".to_string(),
            description: "Cancel a structured task in the current session.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("task_id", "Task identifier.", true),
                    build_string_field(
                        "reason",
                        "Optional stop reason stored as task output.",
                        false,
                    ),
                ],
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let actor_agent_id = execution_agent_id(&ctx)?.to_string();
        let task_id = input
            .get("task_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("task_id is required"))?;
        let task = self
            .control
            .resolve()?
            .stop_task(
                session_id,
                task_id,
                input
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                Some(actor_agent_id),
            )
            .await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(task)?))
    }
}

#[derive(Clone)]
pub(super) struct TaskDeleteTool {
    control: DaemonToolControlHandle,
}

impl TaskDeleteTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for TaskDeleteTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "task_delete".to_string(),
            description: "Delete a structured task from the current session.".to_string(),
            schema: ToolSchema {
                fields: vec![build_string_field("task_id", "Task identifier.", true)],
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let task_id = input
            .get("task_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("task_id is required"))?;
        let deleted = delete_task(self.control.resolve()?, session_id, task_id).await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(deleted)?))
    }
}

async fn update_task(
    control: Arc<dyn DaemonToolControl>,
    session_id: &str,
    task_id: &str,
    mutation: TaskMutation,
    actor_agent_id: Option<&str>,
) -> Result<TaskRecord> {
    let mut state = control.load_session_control_state(session_id).await?;
    let now = crate::now_ms();
    ensure_task_mutation_valid(&state.tasks, task_id, &mutation)?;
    let task = state
        .tasks
        .iter_mut()
        .find(|task| task.id == task_id)
        .ok_or_else(|| anyhow!("unknown task {task_id}"))?;
    let previous_owner = task.owner_agent_id.clone();
    if let Some(title) = mutation.title {
        task.title = title;
    }
    if let Some(description) = mutation.description {
        task.description = description;
    }
    if let Some(status) = mutation.status {
        task.status = status;
        if task.status == TaskStatus::InProgress && task.owner_agent_id.is_none() {
            task.owner_agent_id = actor_agent_id.map(str::to_string);
        }
    }
    if let Some(owner_agent_id) = mutation.owner_agent_id {
        task.owner_agent_id = Some(owner_agent_id);
    }
    if let Some(output) = mutation.output {
        task.output = Some(output);
    }
    if let Some(metadata) = mutation.metadata {
        merge_task_metadata(&mut task.metadata, metadata);
    }
    if let Some(remove_metadata_keys) = mutation.remove_metadata_keys {
        remove_task_metadata_keys(&mut task.metadata, &remove_metadata_keys);
    }
    if let Some(blocked_by) = mutation.blocked_by {
        task.blocked_by = blocked_by;
    }
    if let Some(add_blocked_by) = mutation.add_blocked_by {
        extend_unique(&mut task.blocked_by, add_blocked_by);
    }
    if let Some(remove_blocked_by) = mutation.remove_blocked_by {
        remove_values(&mut task.blocked_by, &remove_blocked_by);
    }
    if let Some(blocks) = mutation.blocks {
        task.blocks = blocks;
    }
    if let Some(add_blocks) = mutation.add_blocks {
        extend_unique(&mut task.blocks, add_blocks);
    }
    if let Some(remove_blocks) = mutation.remove_blocks {
        remove_values(&mut task.blocks, &remove_blocks);
    }
    task.updated_at_ms = now;
    let updated = task.clone();
    control
        .save_session_control_state(session_id, state)
        .await?;
    if let Some(new_owner) = updated.owner_agent_id.as_ref() {
        if previous_owner.as_deref() != Some(new_owner.as_str())
            && actor_agent_id != Some(new_owner.as_str())
        {
            let _ = control
                .message_agent(
                    actor_agent_id.unwrap_or("supervisor"),
                    MessageAgentToolRequest {
                        agent_id: new_owner.clone(),
                        subject: "task_assignment".to_string(),
                        message: format!("Task {} assigned: {}", updated.id, updated.title),
                        asset_ids: Vec::new(),
                        input_items: Vec::new(),
                        message_type: Some("task_assignment".to_string()),
                    },
                )
                .await;
        }
    }
    if matches!(
        updated.status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    ) {
        if let Some(owner_agent_id) = updated.owner_agent_id.as_ref() {
            if actor_agent_id != Some(owner_agent_id.as_str()) {
                let summary = updated
                    .output
                    .as_deref()
                    .filter(|output| !output.trim().is_empty())
                    .unwrap_or("No task output was recorded.");
                let _ = control
                    .message_agent(
                        actor_agent_id.unwrap_or("supervisor"),
                        MessageAgentToolRequest {
                            agent_id: owner_agent_id.clone(),
                            subject: "task_completed".to_string(),
                            message: format!(
                                "Task {} is now {}.\n{}",
                                updated.id,
                                render_task_status(&updated.status),
                                summary
                            ),
                            asset_ids: Vec::new(),
                            input_items: Vec::new(),
                            message_type: Some("task_completed".to_string()),
                        },
                    )
                    .await;
            }
        }
    }
    Ok(updated)
}

fn ensure_unique_active_task_title(
    tasks: &[TaskRecord],
    title: &str,
    excluding_task_id: Option<&str>,
) -> Result<()> {
    let normalized = title.trim();
    if normalized.is_empty() {
        bail!("task title must not be empty");
    }
    let duplicate = tasks.iter().find(|task| {
        excluding_task_id.is_none_or(|task_id| task.id != task_id)
            && !matches!(
                task.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            )
            && task.title.trim().eq_ignore_ascii_case(normalized)
    });
    if let Some(existing) = duplicate {
        bail!("active task title already exists: {}", existing.id);
    }
    Ok(())
}

fn ensure_known_dependencies(tasks: &[TaskRecord], dependencies: &[String]) -> Result<()> {
    for dependency in dependencies {
        if !tasks.iter().any(|task| task.id == *dependency) {
            bail!("unknown task dependency {dependency}");
        }
    }
    Ok(())
}

fn ensure_task_mutation_valid(
    tasks: &[TaskRecord],
    task_id: &str,
    mutation: &TaskMutation,
) -> Result<()> {
    let task = tasks
        .iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| anyhow!("unknown task {task_id}"))?;
    if let Some(title) = mutation.title.as_deref() {
        ensure_unique_active_task_title(tasks, title, Some(task_id))?;
    }
    if let Some(metadata) = mutation.metadata.as_ref() {
        validate_user_task_metadata(metadata)?;
    }
    if let Some(keys) = mutation.remove_metadata_keys.as_ref() {
        ensure_no_daemon_owned_task_metadata_keys(keys)?;
    }
    ensure_shell_task_mutation_allowed(task, mutation)?;

    let mut blocked_by = task.blocked_by.clone();
    if let Some(replacement) = mutation.blocked_by.clone() {
        blocked_by = replacement;
    }
    if let Some(additions) = mutation.add_blocked_by.clone() {
        extend_unique(&mut blocked_by, additions);
    }
    if let Some(removals) = mutation.remove_blocked_by.clone() {
        remove_values(&mut blocked_by, &removals);
    }
    ensure_known_dependencies(tasks, &blocked_by)?;

    let mut blocks = task.blocks.clone();
    if let Some(replacement) = mutation.blocks.clone() {
        blocks = replacement;
    }
    if let Some(additions) = mutation.add_blocks.clone() {
        extend_unique(&mut blocks, additions);
    }
    if let Some(removals) = mutation.remove_blocks.clone() {
        remove_values(&mut blocks, &removals);
    }
    ensure_known_dependencies(tasks, &blocks)?;

    if matches!(
        mutation.status,
        Some(TaskStatus::InProgress | TaskStatus::Completed)
    ) {
        let unresolved = blocked_by.iter().filter(|dependency_id| {
            tasks.iter().any(|candidate| {
                candidate.id == **dependency_id && candidate.status != TaskStatus::Completed
            })
        });
        let unresolved: Vec<&String> = unresolved.collect();
        if !unresolved.is_empty() {
            bail!(
                "task {task_id} is blocked by unresolved dependencies: {}",
                unresolved
                    .into_iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    if matches!(mutation.status, Some(TaskStatus::Completed)) {
        let output = mutation
            .output
            .as_deref()
            .or(task.output.as_deref())
            .map(str::trim);
        if output.is_none_or(|value| value.is_empty()) {
            bail!("completed tasks must record output");
        }
    }

    Ok(())
}

fn ensure_shell_task_mutation_allowed(task: &TaskRecord, mutation: &TaskMutation) -> Result<()> {
    if background_shell_metadata(task).is_none() {
        return Ok(());
    }
    if mutation.metadata.is_some() || mutation.remove_metadata_keys.is_some() {
        bail!(
            "daemon-managed shell task {} owns its metadata; use task_stop and task_output for shell task lifecycle",
            task.id
        );
    }
    if matches!(
        task.status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    ) {
        return Ok(());
    }
    if mutation.status.is_some() || mutation.output.is_some() {
        bail!(
            "daemon-managed shell task {} is live; use task_stop and task_output for lifecycle changes",
            task.id
        );
    }
    Ok(())
}

fn next_task_id(tasks: &[TaskRecord], now: u64) -> String {
    let base = format!("task-{now}");
    if tasks.iter().all(|task| task.id != base) {
        return base;
    }
    let mut suffix = 1u32;
    loop {
        let candidate = format!("{base}-{suffix}");
        if tasks.iter().all(|task| task.id != candidate) {
            return candidate;
        }
        suffix = suffix.saturating_add(1);
    }
}

async fn delete_task(
    control: Arc<dyn DaemonToolControl>,
    session_id: &str,
    task_id: &str,
) -> Result<TaskRecord> {
    let mut state = control.load_session_control_state(session_id).await?;
    let index = state
        .tasks
        .iter()
        .position(|task| task.id == task_id)
        .ok_or_else(|| anyhow!("unknown task {task_id}"))?;
    if background_shell_metadata(&state.tasks[index]).is_some()
        && !matches!(
            state.tasks[index].status,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        )
    {
        bail!("daemon-managed shell task {task_id} is live; stop it before deleting it");
    }
    let deleted = state.tasks.remove(index);
    control
        .save_session_control_state(session_id, state)
        .await?;
    Ok(deleted)
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn extend_unique(target: &mut Vec<String>, additions: Vec<String>) {
    for value in additions {
        if !target.iter().any(|existing| existing == &value) {
            target.push(value);
        }
    }
}

fn remove_values(target: &mut Vec<String>, removals: &[String]) {
    target.retain(|value| !removals.iter().any(|removal| removal == value));
}

fn merge_task_metadata(target: &mut Value, patch: Value) {
    match (target, patch) {
        (Value::Object(target_object), Value::Object(patch_object)) => {
            for (key, value) in patch_object {
                target_object.insert(key, value);
            }
        }
        (target_value, patch_value) => {
            *target_value = patch_value;
        }
    }
}

fn validate_user_task_metadata(metadata: &Value) -> Result<()> {
    let Value::Object(object) = metadata else {
        return Ok(());
    };
    if object
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == BACKGROUND_SHELL_TASK_KIND)
    {
        bail!("metadata kind `{BACKGROUND_SHELL_TASK_KIND}` is daemon-owned");
    }
    let keys = object.keys().cloned().collect::<Vec<_>>();
    ensure_no_daemon_owned_task_metadata_keys(&keys)
}

fn ensure_no_daemon_owned_task_metadata_keys(keys: &[String]) -> Result<()> {
    for key in keys {
        if is_daemon_owned_task_metadata_key(key) {
            bail!("metadata key `{key}` is daemon-owned");
        }
    }
    Ok(())
}

fn is_daemon_owned_task_metadata_key(key: &str) -> bool {
    matches!(
        key,
        "created_by_run_id"
            | "output_file_path"
            | "tool_call_id"
            | "pid"
            | "process_group_id"
            | "exit_code"
            | "terminal_reason"
            | "recovered_on_boot"
            | "output_size_bytes"
            | "output_total_bytes"
            | "output_rotated"
            | "output_rotation_count"
            | "cancelled"
            | "killed_for_size"
            | "interactive_prompt_detected"
            | "started_in_background"
            | "reply_targets"
    )
}

fn remove_task_metadata_keys(target: &mut Value, keys: &[String]) {
    let Value::Object(object) = target else {
        return;
    };
    for key in keys {
        object.remove(key);
    }
    if object.is_empty() {
        *target = Value::Null;
    }
}

fn parse_task_status(value: &str) -> Result<TaskStatus> {
    match value {
        "pending" => Ok(TaskStatus::Pending),
        "in_progress" => Ok(TaskStatus::InProgress),
        "blocked" => Ok(TaskStatus::Blocked),
        "completed" => Ok(TaskStatus::Completed),
        "failed" => Ok(TaskStatus::Failed),
        "cancelled" => Ok(TaskStatus::Cancelled),
        other => Err(anyhow!("unknown task status {other}")),
    }
}

fn task_metadata_with_created_by_run_id(metadata: Value, run_id: Option<&str>) -> Result<Value> {
    validate_user_task_metadata(&metadata)?;
    let Some(run_id) = run_id else {
        return Ok(metadata);
    };
    match metadata {
        Value::Null => Ok(json!({ "created_by_run_id": run_id })),
        Value::Object(mut map) => {
            if map.contains_key("created_by_run_id") {
                bail!("metadata key `created_by_run_id` is daemon-owned");
            }
            map.insert(
                "created_by_run_id".to_string(),
                Value::String(run_id.to_string()),
            );
            Ok(Value::Object(map))
        }
        other => Ok(json!({
            "created_by_run_id": run_id,
            "user_metadata": other,
        })),
    }
}

fn render_task_status(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}
