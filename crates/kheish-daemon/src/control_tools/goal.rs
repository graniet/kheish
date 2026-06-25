use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolSchema,
};
use kheish_types::SessionGoal;
use serde_json::{Value, json};

use super::DaemonToolControlHandle;
use super::helpers::{
    build_number_field, build_string_field, execution_run_id, execution_session_id,
    optional_u64_field,
};

fn goal_json(goal: Option<SessionGoal>) -> Value {
    let remaining_tokens = goal.as_ref().and_then(SessionGoal::remaining_tokens);
    json!({
        "goal": goal,
        "remaining_tokens": remaining_tokens,
    })
}

#[derive(Clone)]
pub(super) struct GetGoalTool {
    control: DaemonToolControlHandle,
}

impl GetGoalTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for GetGoalTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "get_goal".to_string(),
            description: "Get the active long-running goal for the current session, including usage and remaining token budget.".to_string(),
            schema: ToolSchema { fields: Vec::new() },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, _input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let goal = self
            .control
            .resolve()?
            .load_session_goal(session_id)
            .await?;
        Ok(ToolExecutionOutput::json(goal_json(goal)))
    }
}

#[derive(Clone)]
pub(super) struct CreateGoalTool {
    control: DaemonToolControlHandle,
}

impl CreateGoalTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for CreateGoalTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "create_goal".to_string(),
            description: "Create one long-running goal for this session. Fails if the session already has a goal.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("objective", "Concrete objective to pursue.", true),
                    build_number_field(
                        "token_budget",
                        "Optional positive token budget for this goal.",
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
        let objective = input
            .get("objective")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|objective| !objective.is_empty())
            .ok_or_else(|| anyhow!("objective is required"))?
            .to_string();
        let token_budget = optional_u64_field(&input, "token_budget");
        let run_id = execution_run_id(&ctx);
        let goal = self
            .control
            .resolve()?
            .create_session_goal(session_id, run_id, objective, token_budget)
            .await?;
        Ok(ToolExecutionOutput::json(goal_json(Some(goal))))
    }
}

#[derive(Clone)]
pub(super) struct UpdateGoalTool {
    control: DaemonToolControlHandle,
}

impl UpdateGoalTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for UpdateGoalTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "update_goal".to_string(),
            description: "Mark the current session goal complete. The model may only use this when the goal is actually achieved.".to_string(),
            schema: ToolSchema {
                fields: vec![build_string_field(
                    "status",
                    "Only 'complete' is accepted.",
                    true,
                )],
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let status = input
            .get("status")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if status != "complete" {
            bail!("update_goal only accepts status 'complete'");
        }
        let run_id =
            execution_run_id(&ctx).ok_or_else(|| anyhow!("tool execution missing run_id"))?;
        let goal = self
            .control
            .resolve()?
            .complete_session_goal(session_id, run_id)
            .await?;
        Ok(ToolExecutionOutput::json(goal_json(Some(goal))))
    }
}
