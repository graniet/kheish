use super::manager::TaskManager;
use super::utils::pause_and_update;
use crate::agents::{AgentBehavior, AgentOutcome, FormatterAgent, ProposerAgent, ReviewerAgent, ValidatorAgent};
use crate::constants::*;
use tracing::debug;

/// Executes a specific agent role in the task workflow
///
/// # Arguments
/// * `manager` - Mutable reference to the TaskManager instance
/// * `role` - Role identifier string for the agent to execute
///
/// # Returns
/// * `AgentOutcome` - Result of the agent's execution step
pub async fn execute_role(manager: &mut TaskManager, role: &str) -> AgentOutcome {
    debug!("Executing role {}", role);

    let human_message = match role {
        "proposer" => "🤔 The proposer is preparing a new proposal to address the task...",
        "reviewer" => "🔍 The reviewer is examining the proposal to provide feedback...", 
        "validator" => "✅ The validator is checking the proposal for correctness and completeness...",
        "formatter" => "✨ The formatter is refining the proposal into the final desired output...",
        _ => "❓ An unknown agent is acting...",
    };

    pause_and_update(&manager.spinner, human_message).await;

    let (agent_config, default_prompt) = match role {
        "proposer" => (&manager.config.agents.proposer, PROPOSER_USER_PROMPT),
        "reviewer" => (&manager.config.agents.reviewer, REVIEWER_USER_PROMPT),
        "validator" => (&manager.config.agents.validator, VALIDATOR_USER_PROMPT),
        "formatter" => (&manager.config.agents.formatter, FORMATTER_USER_PROMPT),
        _ => return AgentOutcome::Failed(format!("Unknown role {}", role)),
    };

    let user_prompt = agent_config
        .user_prompt
        .as_deref()
        .unwrap_or(default_prompt);

    pause_and_update(&manager.spinner, &format!("🔄 {} is now working...", role)).await;

    let result = match role {
        "proposer" => {
            ProposerAgent {
                llm_client: &manager.llm_client,
                user_prompt,
            }
            .execute_step(&mut manager.task)
            .await
        }
        "reviewer" => {
            ReviewerAgent {
                llm_client: &manager.llm_client,
                user_prompt,
            }
            .execute_step(&mut manager.task)
            .await
        }
        "validator" => {
            ValidatorAgent {
                llm_client: &manager.llm_client,
                user_prompt,
            }
            .execute_step(&mut manager.task)
            .await
        }
        "formatter" => {
            FormatterAgent {
                llm_client: &manager.llm_client,
                user_prompt,
                output_format: manager.config.output.format.as_str(),
                output_file: manager.config.output.file.as_str(),
            }
            .execute_step(&mut manager.task)
            .await
        }
        _ => unreachable!(),
    };

    pause_and_update(&manager.spinner, "✅ The agent has finished this step...").await;

    result
}
