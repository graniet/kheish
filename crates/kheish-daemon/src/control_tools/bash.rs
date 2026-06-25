use anyhow::Result;
use async_trait::async_trait;
use kheish_coding_tools::{CodingToolConfig, resolve_bash_workdir};
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolSchema,
};
use serde_json::{Value, json};

use crate::shell_tasks::BackgroundShellTaskRequest;

use super::DaemonToolControlHandle;
use super::helpers::{
    build_boolean_field, build_string_field, execution_agent_id, execution_run_id,
    execution_session_id, optional_string_field, string_field, summarize_managed_command,
};

const FOREGROUND_BASH_TIMEOUT_MS: u64 = 60 * 60 * 1000;

#[derive(Clone)]
pub(super) struct DaemonBashTool {
    control: DaemonToolControlHandle,
    config: CodingToolConfig,
}

impl DaemonBashTool {
    pub(super) fn new(control: DaemonToolControlHandle, config: CodingToolConfig) -> Self {
        Self { control, config }
    }
}

#[async_trait]
impl Tool for DaemonBashTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "bash".to_string(),
            description: "Executes one shell command inside the workspace root. Use run_in_background for long-running commands you want to inspect later.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("command", "Shell command to execute.", true),
                    build_string_field(
                        "workdir",
                        "Optional working directory inside the workspace.",
                        false,
                    ),
                    build_boolean_field(
                        "run_in_background",
                        "When true, launch the command as one daemon-managed background task and return immediately.",
                        false,
                    ),
                    build_string_field(
                        "description",
                        "Optional task title used when the command runs in the background.",
                        false,
                    ),
                ],
            },
            timeout_ms: FOREGROUND_BASH_TIMEOUT_MS,
            sandbox: SandboxProfile::WorkspaceWrite,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let command = string_field(&input, "command")?;
        let workdir = optional_string_field(&input, "workdir");
        let run_in_background = input
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let session_id = execution_session_id(&ctx)?;
        let agent_id = execution_agent_id(&ctx)?;
        let resolved_workdir = resolve_bash_workdir(&self.config, &ctx, workdir.as_deref())?;
        let description = input
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                summarize_managed_command(&command, &resolved_workdir, run_in_background)
            });
        let request = BackgroundShellTaskRequest {
            command: command.clone(),
            shell: self.config.shell.clone(),
            workdir: resolved_workdir,
            description,
            tool_call_id: ctx.call_id.clone(),
            created_by_run_id: execution_run_id(&ctx).map(str::to_string),
            started_in_background: run_in_background,
            reply_targets: Vec::new(),
        };
        if !run_in_background {
            return self
                .control
                .resolve()?
                .run_foreground_shell_task(session_id, agent_id, request)
                .await;
        }

        let task = self
            .control
            .resolve()?
            .start_background_shell_task(session_id, agent_id, request)
            .await?;
        Ok(ToolExecutionOutput::json(json!({
            "success": true,
            "stdout": "",
            "stderr": "",
            "exit_code": 0,
            "background_task_id": task.id,
            "backgrounded_by_user": true,
            "task": task,
        })))
    }
}
