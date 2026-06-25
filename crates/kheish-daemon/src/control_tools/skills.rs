use std::fs;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use kheish_agent::ChildRetentionPolicy;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolSchema,
    tool_context_string_allowlist,
};
use kheish_types::SkillExecutionContext;
use serde::Deserialize;
use serde_json::{Value, json};

use super::helpers::{
    build_boolean_field, build_number_field, build_string_field, deserialize_tool_request,
    execution_agent_id, populate_spawn_request_from_context, wait_for_agent_snapshot,
};
use super::{DaemonToolControlHandle, SpawnAgentToolRequest};

#[derive(Clone)]
pub(super) struct ListSkillsTool {
    control: DaemonToolControlHandle,
}

impl ListSkillsTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for ListSkillsTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "list_skills".to_string(),
            description: "List reusable skills available in the current daemon runtime."
                .to_string(),
            schema: ToolSchema {
                fields: vec![build_string_field(
                    "query",
                    "Optional case-insensitive filter applied to skill name, description, or when-to-use guidance.",
                    false,
                )],
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let control = self.control.resolve()?;
        let query = input.get("query").and_then(Value::as_str).map(str::trim);
        let mut skills = control
            .list_skills(query.filter(|value| !value.is_empty()))
            .await?;
        if let Some(visible) = tool_context_string_allowlist(&ctx.metadata, "visible_skills") {
            skills.retain(|skill| visible.contains(&skill.name));
        }
        Ok(ToolExecutionOutput::json(json!({
            "skills": skills,
            "count": skills.len(),
        })))
    }
}

#[derive(Clone)]
pub(super) struct UseSkillTool {
    control: DaemonToolControlHandle,
}

impl UseSkillTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[derive(Debug, Deserialize)]
struct UseSkillToolRequest {
    name: String,
    #[serde(default)]
    args: Option<String>,
    #[serde(default)]
    context: Option<SkillExecutionContext>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    wait: Option<bool>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[async_trait]
impl Tool for UseSkillTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "use_skill".to_string(),
            description:
                "Activate a reusable skill inline or execute it in a child agent when the skill matches the user's task."
                    .to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("name", "Registered skill name.", true),
                    build_string_field(
                        "args",
                        "Optional free-form arguments passed through to the skill template.",
                        false,
                    ),
                    build_string_field(
                        "context",
                        "Optional execution context override: inline or fork.",
                        false,
                    ),
                    build_string_field(
                        "reason",
                        "Optional activation reason captured for auditability.",
                        false,
                    ),
                    build_boolean_field(
                        "wait",
                        "When context=fork, wait for the child agent to settle before returning.",
                        false,
                    ),
                    build_number_field(
                        "timeout_ms",
                        "Optional child wait timeout in milliseconds when context=fork.",
                        false,
                    ),
                ],
            },
            timeout_ms: 120_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let parent_agent_id = execution_agent_id(&ctx)?;
        let request = deserialize_tool_request::<UseSkillToolRequest>(input)?;
        let control = self.control.resolve()?;
        if let Some(visible) = tool_context_string_allowlist(&ctx.metadata, "visible_skills")
            && !visible.contains(&request.name)
        {
            anyhow::bail!("skill `{}` is not available in this session", request.name);
        }
        let skill = control
            .get_skill(&request.name)
            .await?
            .ok_or_else(|| anyhow!("unknown skill `{}`", request.name))?;
        let promoted_skill = control.get_learning_skill(&request.name).await?;
        if let Some(record) = promoted_skill.as_ref() {
            if record.status != crate::LearningSkillStatus::Active {
                anyhow::bail!("promoted skill `{}` is not active", request.name);
            }
            if record.digest != skill.digest
                || record.skill_path != skill.skill_path.display().to_string()
                || record.skill_root != skill.skill_root.display().to_string()
                || record.description != skill.description
                || record.when_to_use != skill.when_to_use
                || record.version != skill.version
                || record.instructions.trim() != skill.instructions.trim()
                || record.runtime != skill.runtime
            {
                anyhow::bail!(
                    "promoted skill `{}` catalog binding does not match the active record",
                    request.name
                );
            }
        }
        let context = request.context.unwrap_or(skill.runtime.context);
        let reason = request.reason.clone().unwrap_or_else(|| {
            format!("activated via use_skill ({context:?})").to_ascii_lowercase()
        });

        match context {
            SkillExecutionContext::Inline => {
                skill.validate_inline_activation()?;
                let snapshot =
                    skill.to_active_snapshot(request.args.as_deref(), context, Some(reason));
                Ok(ToolExecutionOutput {
                    output: json!({
                        "action": "activate",
                        "mode": "inline",
                        "name": snapshot.name,
                        "active_skill": snapshot,
                    }),
                    context_updates: Vec::new(),
                    hook_contexts: vec![skill.render_inline_instructions(request.args.as_deref())],
                })
            }
            SkillExecutionContext::Fork => {
                let wait = request.wait.unwrap_or(true);
                let mut spawn_request = SpawnAgentToolRequest {
                    session_id: None,
                    thread_id: None,
                    cwd: None,
                    team_name: None,
                    isolation: None,
                    name: format!("skill:{}", skill.name),
                    description: skill.description.clone(),
                    prompt: skill.render_fork_prompt(request.args.as_deref()),
                    asset_ids: Vec::new(),
                    input_items: Vec::new(),
                    agent_type: skill.runtime.agent_profile.clone(),
                    system_prompt: None,
                    prompt_merge_mode: None,
                    model: skill.runtime.model.clone(),
                    provider: skill.runtime.provider.clone(),
                    fallback_model: skill.runtime.fallback_model.clone(),
                    generation: None,
                    mode: None,
                    retention: Some(ChildRetentionPolicy::CloseOnSettle),
                    nickname: Some(skill.name.clone()),
                    allowed_tools: skill.runtime.allowed_tools.clone(),
                    blocked_tools: skill.runtime.blocked_tools.clone(),
                    capability_scope: None,
                    credential_scope: None,
                    wait,
                    run_in_background: !wait,
                    timeout_ms: request.timeout_ms,
                    parent_assistant_message: None,
                    inherited_tool_call_ids: Vec::new(),
                    spawned_by_run_id: None,
                    spawn_request_id: None,
                };
                if promoted_skill.is_some() {
                    spawn_request.isolation = Some(super::SpawnIsolation::Worktree);
                    let workspace_root = ctx
                        .metadata
                        .get("workspace_root")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                            anyhow!("promoted skills require workspace_root metadata")
                        })?;
                    let isolated_root =
                        procedural_skill_worktree_path(workspace_root, &skill.name, &ctx.call_id);
                    fs::create_dir_all(&isolated_root)?;
                    spawn_request.cwd = Some(isolated_root.display().to_string());
                }
                populate_spawn_request_from_context(control.as_ref(), &ctx, &mut spawn_request)
                    .await?;
                let timeout_ms = spawn_request.timeout_ms.unwrap_or(60_000);
                let mut response = control.spawn_agent(parent_agent_id, spawn_request).await?;
                if wait {
                    let snapshot = wait_for_agent_snapshot(
                        control.as_ref(),
                        parent_agent_id,
                        &response.agent_id,
                        std::time::Duration::from_millis(timeout_ms),
                    )
                    .await?;
                    response.status = format!("{:?}", snapshot.agent.status).to_ascii_lowercase();
                    if let Some(run_id) = response.launch_run_id.as_deref() {
                        response.final_output = control.latest_run_output(run_id).await?;
                    }
                    response.snapshot = Some(snapshot);
                    response.run_in_background = false;
                }
                Ok(ToolExecutionOutput::json(json!({
                    "action": "fork",
                    "mode": "fork",
                    "name": skill.name,
                    "digest": skill.digest,
                    "spawn": response,
                })))
            }
        }
    }
}

fn procedural_skill_worktree_path(
    workspace_root: &str,
    skill_name: &str,
    call_id: &str,
) -> PathBuf {
    let safe_skill_name = skill_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    PathBuf::from(workspace_root)
        .join(".kheish-procedural-worktrees")
        .join(safe_skill_name)
        .join(call_id)
}
