use super::{AgentBehavior, AgentOutcome};
use crate::constants::PROPOSER_FORMAT_REMINDER;
use crate::core::Task;
use crate::llm::{ChatMessage, LlmClient};
use async_trait::async_trait;
use tracing::{debug, info};

/// Agent responsible for generating and revising proposals.
/// 
/// This agent interacts with the LLM to:
/// - Generate initial content proposals based on context
/// - Request module executions when needed
/// - Revise proposals based on feedback
pub struct ProposerAgent<'a> {
    /// Client for interacting with the language model
    pub llm_client: &'a LlmClient,
    /// Custom prompt to guide the agent's behavior
    pub user_prompt: &'a str,
}

/// Validates that the LLM response is properly formatted.
/// 
/// # Arguments
/// * `resp` - The response string from the LLM
/// 
/// # Returns
/// * `bool` - True if response starts with "Proposal:" or contains "MODULE_REQUEST:"
fn validate_proposer_response(resp: &str) -> bool {
    resp.starts_with("Proposal:") || resp.contains("MODULE_REQUEST:")
}

#[async_trait]
impl<'a> AgentBehavior for ProposerAgent<'a> {
    /// Executes a single step of the proposal generation process.
    ///
    /// This method:
    /// 1. Builds a prompt from the task context and any previous feedback
    /// 2. Calls the LLM to generate a proposal or module request
    /// 3. Validates and processes the response
    /// 
    /// # Arguments
    /// * `task` - The task containing context and conversation history
    ///
    /// # Returns
    /// * `AgentOutcome` - The result of the execution step
    async fn execute_step(&self, task: &mut Task) -> AgentOutcome {
        debug!("ProposerAgent: generating initial proposal...");

        let source_text = task.context.combined_context();
        if source_text.trim().is_empty() {
            return AgentOutcome::Failed(
                "No source text available for proposing content".to_string(),
            );
        }

        let feedbacks = task.feedback_for_prompt();

        let mut prompt = String::new();
        prompt.push_str("Current role: proposer\n");
        prompt.push_str("Specific instructions: ");
        prompt.push_str(self.user_prompt);
        prompt.push_str("\n\nContext:\n");
        prompt.push_str(&source_text);

        if !feedbacks.is_empty() {
            prompt.push_str("\n\nPrevious feedback:\n");
            prompt.push_str(&feedbacks);
            if let Some(prev_sol) = &task.current_proposal {
                prompt.push_str("\nPrevious proposal:\n");
                prompt.push_str(prev_sol);
            }
            prompt.push_str("\nPlease improve the proposal taking into account the feedback.");
        }

        prompt.push_str("\n\nPlease now provide a 'Proposal:' or a 'MODULE_REQUEST:'.");

        task.conversation.push(ChatMessage::new("user", &prompt));

        match self
            .llm_client
            .call_llm_with_format_check(
                &mut task.conversation,
                validate_proposer_response,
                PROPOSER_FORMAT_REMINDER,
                2,
            )
            .await
        {
            Ok(new_proposal) => {
                task.conversation
                    .push(ChatMessage::new("assistant", &new_proposal));
                if let Some(mr) = self.parse_module_request(&new_proposal) {
                    return mr;
                }

                info!("ProposerAgent: proposal generated: {}", new_proposal);
                task.add_proposal(new_proposal);
                AgentOutcome::ProposalGenerated
            }
            Err(e) => AgentOutcome::Failed(format!("LLM error in Proposer: {}", e)),
        }
    }
}
