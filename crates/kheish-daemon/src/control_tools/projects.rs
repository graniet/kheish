//! Tools that let agents pull, advance, and decompose project tasks.

use anyhow::{Result, bail};
use async_trait::async_trait;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolSchema,
};
use kheish_types::TaskStatus;
use serde::Deserialize;
use serde_json::{Value, json};

use super::DaemonToolControlHandle;
use super::helpers::{
    build_array_field, build_boolean_field, build_string_field, deserialize_tool_request,
    execution_run_id, execution_session_id,
};

#[derive(Clone)]
pub(crate) struct ProjectListTasksTool {
    control: DaemonToolControlHandle,
}

impl ProjectListTasksTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[derive(Clone)]
pub(crate) struct ProjectClaimTaskTool {
    control: DaemonToolControlHandle,
}

impl ProjectClaimTaskTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[derive(Clone)]
pub(crate) struct ProjectUpdateTaskTool {
    control: DaemonToolControlHandle,
}

impl ProjectUpdateTaskTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[derive(Clone)]
pub(crate) struct ProjectCreateTaskTool {
    control: DaemonToolControlHandle,
}

impl ProjectCreateTaskTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[derive(Debug, Deserialize)]
struct ProjectListTasksRequest {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProjectClaimTaskRequest {
    project_id: String,
    project_task_id: String,
}

#[derive(Debug, Deserialize)]
struct ProjectUpdateTaskRequest {
    project_id: String,
    project_task_id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    output: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProjectCreateTaskRequest {
    project_id: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    blocked_by: Vec<String>,
    #[serde(default)]
    parent_task_id: Option<String>,
    #[serde(default)]
    assign_to_self: bool,
}

fn parse_task_status(value: &str) -> Result<TaskStatus> {
    Ok(match value.trim().to_ascii_lowercase().as_str() {
        "pending" => TaskStatus::Pending,
        "in_progress" => TaskStatus::InProgress,
        "blocked" => TaskStatus::Blocked,
        "completed" => TaskStatus::Completed,
        "failed" => TaskStatus::Failed,
        "cancelled" => TaskStatus::Cancelled,
        other => bail!(
            "unknown task status {other}; expected pending, in_progress, blocked, completed, failed, or cancelled"
        ),
    })
}

/// Compact task JSON for tool output — the full view carries metadata and
/// timestamps the model rarely needs.
fn compact_task_json(task: &crate::projects::ProjectTaskView) -> Value {
    json!({
        "project_id": task.project_id,
        "project_task_id": task.project_task_id,
        "title": task.title,
        "description": task.description,
        "status": task.status,
        "assignee_member_id": task.assignee_member_id,
        "primary_session_id": task.primary_session_id,
        "blocked_by": task.blocked_by,
        "parent_task_id": task.parent_task_id,
        "output": task.output,
    })
}

#[async_trait]
impl Tool for ProjectListTasksTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "project_list_tasks".to_string(),
            description:
                "List the project tasks visible to this session across its projects, to find work to claim."
                    .to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field(
                        "project_id",
                        "Optional project identifier to list one project only.",
                        false,
                    ),
                    build_string_field(
                        "status",
                        "Optional status filter: pending, in_progress, blocked, completed, failed, or cancelled.",
                        false,
                    ),
                ],
            },
            timeout_ms: 30_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let request = deserialize_tool_request::<ProjectListTasksRequest>(input)?;
        let status = request
            .status
            .as_deref()
            .map(parse_task_status)
            .transpose()?;
        let control = self.control.resolve()?;
        let tasks = control
            .agent_list_project_tasks(&session_id, request.project_id.as_deref(), status)
            .await?;
        Ok(ToolExecutionOutput::json(json!({
            "tasks": tasks.iter().map(compact_task_json).collect::<Vec<_>>(),
        })))
    }
}

#[async_trait]
impl Tool for ProjectClaimTaskTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "project_claim_task".to_string(),
            description:
                "Claim one unassigned project task for this session and start working on it in the current run."
                    .to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("project_id", "The project identifier.", true),
                    build_string_field("project_task_id", "The task identifier to claim.", true),
                ],
            },
            timeout_ms: 30_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let run_id = execution_run_id(&ctx)
            .ok_or_else(|| anyhow::anyhow!("project_claim_task requires a run context"))?
            .to_owned();
        let request = deserialize_tool_request::<ProjectClaimTaskRequest>(input)?;
        let control = self.control.resolve()?;
        let task = control
            .agent_claim_project_task(
                &session_id,
                &run_id,
                &request.project_id,
                &request.project_task_id,
            )
            .await?;
        Ok(ToolExecutionOutput::json(compact_task_json(&task)))
    }
}

#[async_trait]
impl Tool for ProjectUpdateTaskTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "project_update_task".to_string(),
            description:
                "Advance one project task this session holds: set its status, record its output, or both."
                    .to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("project_id", "The project identifier.", true),
                    build_string_field("project_task_id", "The task identifier to update.", true),
                    build_string_field(
                        "status",
                        "Optional new status: in_progress, blocked, completed, or failed.",
                        false,
                    ),
                    build_string_field(
                        "output",
                        "Optional recorded result or conclusion for the task.",
                        false,
                    ),
                ],
            },
            timeout_ms: 30_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let run_id = execution_run_id(&ctx).map(ToOwned::to_owned);
        let request = deserialize_tool_request::<ProjectUpdateTaskRequest>(input)?;
        let status = request
            .status
            .as_deref()
            .map(parse_task_status)
            .transpose()?;
        let control = self.control.resolve()?;
        let task = control
            .agent_update_project_task(
                &session_id,
                run_id.as_deref(),
                &request.project_id,
                &request.project_task_id,
                status,
                request.output,
            )
            .await?;
        Ok(ToolExecutionOutput::json(compact_task_json(&task)))
    }
}

#[async_trait]
impl Tool for ProjectCreateTaskTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "project_create_task".to_string(),
            description:
                "Create one new task or subtask in a project this session belongs to, optionally self-assigned."
                    .to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("project_id", "The project identifier.", true),
                    build_string_field("title", "The short task title.", true),
                    build_string_field("description", "The longer task description.", false),
                    build_array_field(
                        "blocked_by",
                        "Optional task identifiers this task depends on.",
                        false,
                        kheish_runtime::ToolInputKind::String,
                    ),
                    build_string_field(
                        "parent_task_id",
                        "Optional parent task identifier when creating a subtask.",
                        false,
                    ),
                    build_boolean_field(
                        "assign_to_self",
                        "Whether the task should be assigned to this session immediately.",
                        false,
                    ),
                ],
            },
            timeout_ms: 30_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let request = deserialize_tool_request::<ProjectCreateTaskRequest>(input)?;
        if request.title.trim().is_empty() {
            bail!("title is required");
        }
        let control = self.control.resolve()?;
        let task = control
            .agent_create_project_task(
                &session_id,
                &request.project_id,
                request.title,
                request.description,
                request.blocked_by,
                request.parent_task_id,
                request.assign_to_self,
            )
            .await?;
        Ok(ToolExecutionOutput::json(compact_task_json(&task)))
    }
}
