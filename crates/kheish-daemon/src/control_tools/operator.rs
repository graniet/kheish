//! Tools that let the model contact the configured human operator.

use anyhow::{Result, bail};
use async_trait::async_trait;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolSchema,
};
use serde_json::{Value, json};

use super::helpers::{
    USER_QUESTION_INPUT_EXAMPLE, build_string_field, build_user_question_expiration_fields,
    build_user_question_request, build_user_questions_field, execution_run_id,
    execution_session_id, optional_string_field, string_field,
};
use super::{DaemonToolControlHandle, OperatorNotificationRequest};

#[derive(Clone)]
pub(crate) struct NotifyOperatorTool {
    control: DaemonToolControlHandle,
}

impl NotifyOperatorTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for NotifyOperatorTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "notify_operator".to_string(),
            description: "Queue one non-blocking message to the configured human operator for this session. Use it for progress updates, blockers, or operational visibility when you do not need an immediate answer. Do not include secrets or raw credentials.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("subject", "Optional short operator-facing subject.", false),
                    build_string_field("message", "Required concise operator-facing message body.", true),
                    build_string_field("urgency", "Optional urgency label such as info, warning, blocker, or critical.", false),
                ],
            },
            timeout_ms: 15_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let control = self.control.resolve()?;
        let operator = control.load_session_operator_config(session_id).await?;
        if !operator.enabled || !operator.allow_notify {
            bail!("notify_operator is not enabled for this session");
        }
        let request = OperatorNotificationRequest {
            subject: trim_optional(optional_string_field(&input, "subject")),
            message: required_trimmed_string(&input, "message")?,
            urgency: trim_optional(optional_string_field(&input, "urgency")),
            idempotency_key: Some(format!(
                "operator-notification:{}:{}",
                session_id, ctx.call_id
            )),
        };
        let response = control
            .notify_operator(session_id, execution_run_id(&ctx), request)
            .await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(response)?))
    }
}

#[derive(Clone)]
pub(crate) struct AskOperatorTool {
    control: DaemonToolControlHandle,
}

impl AskOperatorTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for AskOperatorTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "ask_operator".to_string(),
            description: format!(
                "Ask the configured human operator one structured blocking question and suspend this run until it is answered through the daemon question flow. Use this only when you need operator input before continuing. Ask between 1 and 4 questions, each with 2 to 4 options. Pass input like {USER_QUESTION_INPUT_EXAMPLE}."
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
        let operator = self
            .control
            .resolve()?
            .load_session_operator_config(session_id)
            .await?;
        if !operator.enabled || !operator.allow_questions {
            bail!("ask_operator is not enabled for this session");
        }
        let request = build_user_question_request(&ctx, &input)?;
        Ok(ToolExecutionOutput::json(json!({
            "_kheish_pending_user_question": request,
        })))
    }
}

fn trim_optional(value: Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn required_trimmed_string(input: &Value, field: &str) -> Result<String> {
    let value = string_field(input, field)?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{field} is required");
    }
    Ok(trimmed.to_string())
}
