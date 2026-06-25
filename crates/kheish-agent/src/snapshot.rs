use std::path::PathBuf;

use anyhow::{Result, anyhow};
use kheish_core::ModelDriver;
use kheish_runtime::{AgentPromptOverride, AgentRuntime, PromptMergeMode};
use kheish_types::{
    ApprovalRequest, ModelGenerationConfig, PermissionDecision, Role, ToolSurfaceFilter,
    UserQuestionRequest,
};

use crate::supervisor::AgentSupervisor;
use crate::types::{AgentId, AgentStatus, ForkContext, ManagedAgentSnapshot};

pub(crate) fn build_snapshot<M>(
    supervisor: &AgentSupervisor,
    agent_id: &AgentId,
    runtime: &AgentRuntime<M>,
    last_error: Option<String>,
) -> Result<ManagedAgentSnapshot>
where
    M: ModelDriver + Send + Sync,
{
    let mut agent = supervisor
        .get(agent_id)
        .ok_or_else(|| anyhow!("unknown agent {}", agent_id.0))?;
    let pending_approvals: Vec<ApprovalRequest> = runtime
        .pending_batch()
        .map(|batch| {
            batch
                .decisions
                .iter()
                .filter_map(|decision| match &decision.decision {
                    PermissionDecision::Ask { request } => Some(request.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let pending_questions: Vec<UserQuestionRequest> = runtime
        .pending_question()
        .map(|question| vec![question.request.clone()])
        .unwrap_or_default();
    if !pending_questions.is_empty() {
        agent.status = AgentStatus::WaitingForUserInput;
    } else if !pending_approvals.is_empty() {
        agent.status = AgentStatus::WaitingForApproval;
    }
    let last_assistant_message = runtime
        .engine()
        .replay_from_journal()
        .messages
        .into_iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| message.content);
    Ok(ManagedAgentSnapshot {
        agent,
        pending_approvals,
        pending_questions,
        last_assistant_message,
        journal_len: runtime.engine().journal().len(),
        checkpoint_len: runtime.engine().checkpoints().len(),
        last_error,
    })
}

pub(crate) fn agent_prompt_override(
    fork_context: Option<&ForkContext>,
) -> Option<AgentPromptOverride> {
    let prompt = fork_context.and_then(|fork_context| {
        let trimmed = fork_context.system_prompt.trim();
        (!trimmed.is_empty()).then_some(trimmed.to_string())
    })?;
    Some(AgentPromptOverride {
        prompt,
        mode: fork_context
            .map(|fork_context| fork_context.prompt_merge_mode.clone())
            .unwrap_or(PromptMergeMode::default()),
    })
}

pub(crate) fn agent_default_generation(
    fork_context: Option<&ForkContext>,
) -> ModelGenerationConfig {
    fork_context
        .and_then(|fork_context| fork_context.generation.clone())
        .unwrap_or_default()
}

pub(crate) fn agent_tool_surface(fork_context: Option<&ForkContext>) -> ToolSurfaceFilter {
    fork_context
        .map(|fork_context| fork_context.tool_surface.clone())
        .unwrap_or_default()
}

pub(crate) fn agent_workspace_root(fork_context: Option<&ForkContext>) -> Option<PathBuf> {
    fork_context
        .and_then(|fork_context| fork_context.worktree_path.as_deref())
        .map(PathBuf::from)
}
