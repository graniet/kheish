use super::{AgentBehavior, AgentOutcome};
use crate::constants::REVIEWER_FORMAT_REMINDER;
use crate::core::Task;
use crate::llm::{ChatMessage, LlmClient};
use async_trait::async_trait;
use tracing::debug;

/// Agent responsible for reviewing proposals and providing feedback.
///
/// This agent interacts with the LLM to:
/// - Review generated proposals against the context
/// - Approve proposals that meet requirements
/// - Request revisions with specific feedback
/// - Request module executions when needed
pub struct ReviewerAgent<'a> {
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
/// * `bool` - True if response is "approved", starts with "revise:", or contains "MODULE_REQUEST:"
fn validate_reviewer_response(resp: &str) -> bool {
    let lower = resp.to_lowercase();
    lower == "approved" || lower.starts_with("revise:") || lower.contains("MODULE_REQUEST:")
}

#[async_trait]
impl<'a> AgentBehavior for ReviewerAgent<'a> {
    /// Executes a single step of the review process.
    ///
    /// This method:
    /// 1. Retrieves the current proposal and context
    /// 2. Builds a prompt including the proposal, context and module execution history
    /// 3. Calls the LLM to review the proposal
    /// 4. Processes the response to determine if the proposal is approved or needs revision
    ///
    /// # Arguments
    /// * `task` - The task containing the proposal and context to review
    ///
    /// # Returns
    /// * `AgentOutcome` - The result of the review (Approved, RevisionRequested, or Failed)
    async fn execute_step(&self, task: &mut Task) -> AgentOutcome {
        debug!("ReviewerAgent: reviewing proposal...");

        let proposal = if let Some(sol) = &task.current_proposal {
            sol.clone()
        } else {
            return AgentOutcome::Failed("No proposal found in task for reviewing".to_string());
        };

        let combined_context = task.context.combined_context();
        if combined_context.trim().is_empty() {
            return AgentOutcome::Failed("No context available for reviewing".to_string());
        }

        let mut prompt = String::new();
        prompt.push_str("Current role: reviewer\n");
        prompt.push_str("Specific instructions: ");
        prompt.push_str(self.user_prompt);
        prompt.push_str("\n\nContext:\n");
        prompt.push_str(&combined_context);
        prompt.push_str("\n\nProposal to review:\n");
        prompt.push_str(&proposal);

        for execution in &task.module_execution_history {
            prompt.push_str("\n\nModule execution context:\n");
            prompt.push_str(execution);
        }

        prompt.push_str(
            "\n\nPlease respond with 'approved' or 'revise: ...' or optionally 'MODULE_REQUEST:'.",
        );

        task.conversation.push(ChatMessage::new("user", &prompt));

        match self
            .llm_client
            .call_llm_with_format_check(
                &mut task.conversation,
                validate_reviewer_response,
                REVIEWER_FORMAT_REMINDER,
                2,
            )
            .await
        {
            Ok(response) => {
                let resp = response.trim();
                task.conversation.push(ChatMessage::new("assistant", resp));
                if let Some(mr) = self.parse_module_request(resp) {
                    return mr;
                }
                if resp.to_lowercase().contains("approved") {
                    task.set_feedback(None);
                    AgentOutcome::Approved
                } else if resp.to_lowercase().starts_with("revise:") {
                    let feedback = resp.trim_start_matches("revise:").trim().to_string();
                    task.set_feedback(Some(feedback));
                    AgentOutcome::RevisionRequested
                } else {
                    AgentOutcome::Failed("Unexpected LLM response in Reviewer".to_string())
                }
            }
            Err(e) => AgentOutcome::Failed(format!("LLM error in Reviewer: {}", e)),
        }
    }
}
