use anyhow::{Result, anyhow};
use async_trait::async_trait;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolInputKind,
    ToolSchema,
};
use kheish_types::TodoItem;
use serde_json::{Value, json};

use super::DaemonToolControlHandle;
use super::helpers::{
    USER_QUESTION_INPUT_EXAMPLE, build_array_field, build_string_field,
    build_user_question_expiration_fields, build_user_question_request, build_user_questions_field,
    execution_session_id, render_permission_mode,
};

#[derive(Clone)]
pub(super) struct TodoWriteTool {
    control: DaemonToolControlHandle,
}

impl TodoWriteTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "todo_write".to_string(),
            description: "Replace the current ordered todo list for the session.".to_string(),
            schema: ToolSchema {
                fields: vec![build_array_field(
                    "todos",
                    "Ordered todo items {id?, content, completed?}.",
                    true,
                    ToolInputKind::Object,
                )],
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let todos_value = input
            .get("todos")
            .cloned()
            .ok_or_else(|| anyhow!("todos is required"))?;
        let mut todos = Vec::new();
        for (index, value) in todos_value
            .as_array()
            .ok_or_else(|| anyhow!("todos must be an array"))?
            .iter()
            .enumerate()
        {
            todos.push(TodoItem {
                id: value
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("todo-{}", index + 1)),
                content: value
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("todo content is required"))?
                    .to_string(),
                completed: value
                    .get("completed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
        let mut state = self
            .control
            .resolve()?
            .load_session_control_state(session_id)
            .await?;
        state.todos = todos.clone();
        let state = self
            .control
            .resolve()?
            .save_session_control_state(session_id, state)
            .await?;
        Ok(ToolExecutionOutput::json(json!({
            "todos": state.todos,
        })))
    }
}

#[derive(Clone)]
pub(super) struct EnterPlanModeTool {
    control: DaemonToolControlHandle,
}

impl EnterPlanModeTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "enter_plan_mode".to_string(),
            description: "Switch the current session into read-only planning mode.".to_string(),
            schema: ToolSchema {
                fields: vec![build_string_field("note", "Optional planning note.", false)],
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let state = self
            .control
            .resolve()?
            .enter_session_plan_mode(session_id)
            .await?;
        Ok(ToolExecutionOutput::json(json!({
            "plan_mode": true,
            "pre_plan_mode": state.pre_plan_mode,
            "note": input.get("note").and_then(Value::as_str),
            "todos": state.todos,
            "tasks": state.tasks,
        })))
    }
}

#[derive(Clone)]
pub(super) struct ExitPlanModeTool {
    control: DaemonToolControlHandle,
}

impl ExitPlanModeTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "exit_plan_mode".to_string(),
            description: "Leave read-only planning mode for the current session and persist the latest plan artifact.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field(
                        "plan",
                        "The final plan body captured while leaving plan mode.",
                        true,
                    ),
                    build_string_field(
                        "summary",
                        "Optional short plan summary for operators.",
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
        let plan = input
            .get("plan")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|plan| !plan.is_empty())
            .ok_or_else(|| anyhow!("plan is required"))?;
        let summary = input
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|summary| !summary.is_empty())
            .map(str::to_string);
        let outcome = self
            .control
            .resolve()?
            .exit_session_plan_mode(session_id, plan.to_string(), summary)
            .await?;
        let restored_permission_mode = outcome
            .restored_permission_mode
            .as_ref()
            .map(render_permission_mode);
        Ok(ToolExecutionOutput::json(json!({
            "plan_mode": false,
            "restored_permission_mode": restored_permission_mode,
            "plan_artifact": outcome.state.plan_artifact,
            "todos": outcome.state.todos,
            "tasks": outcome.state.tasks,
        })))
    }
}

#[derive(Clone, Default)]
pub(super) struct AskUserQuestionTool;

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "ask_user_question".to_string(),
            description: format!(
                "Ask the user one structured clarification request. Use this only from the main agent when you genuinely need user input before continuing. Ask between 1 and 4 questions, each with 2 to 4 options. Pass input like {USER_QUESTION_INPUT_EXAMPLE}."
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
        let request = build_user_question_request(&ctx, &input)?;
        Ok(ToolExecutionOutput::json(json!({
            "_kheish_pending_user_question": request,
        })))
    }
}
