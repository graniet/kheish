use super::{AgentBehavior, AgentOutcome};
use crate::config::AgentConfig;
use crate::constants::VALIDATOR_FORMAT_REMINDER;
use crate::constants::VALIDATOR_USER_PROMPT;
use crate::core::Task;
use crate::event::Event;
use crate::llm::{ChatMessage, LlmClient};
use async_trait::async_trait;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::debug;

/// Agent responsible for validating final proposals.
///
/// This agent interacts with the LLM to:
/// - Validate proposals against requirements and context
/// - Request revisions with specific feedback
/// - Request module executions when needed
pub struct ValidatorAgent {
    /// Client for interacting with the language model
    pub llm_client: LlmClient,
    /// Custom prompt to guide the agent's behavior
    pub user_prompt: String,
    /// Receiver for events from the task manager
    pub self_rx: UnboundedReceiver<Event>,
}

impl ValidatorAgent {
    /// Creates a new ValidatorAgent instance
    ///
    /// # Arguments
    /// * `config` - Configuration for the agent
    /// * `llm_provider` - The LLM provider to use (e.g. "openai")
    /// * `llm_model` - The specific model to use (e.g. "gpt-4")
    ///
    /// # Returns
    /// * `(Self, UnboundedSender<Event>)` - The agent instance and a channel sender for events
    pub fn new(
        config: AgentConfig,
        llm_provider: &str,
        llm_model: &str,
    ) -> (Self, UnboundedSender<Event>) {
        let (self_tx, self_rx) = tokio::sync::mpsc::unbounded_channel();

        (
            Self {
                llm_client: LlmClient::new(llm_provider, llm_model)
                    .expect("Failed to create LLM client"),
                user_prompt: config
                    .user_prompt
                    .as_deref()
                    .unwrap_or(VALIDATOR_USER_PROMPT)
                    .to_string(),
                self_rx,
            },
            self_tx,
        )
    }

    pub async fn run_loop(mut self, manager_tx: UnboundedSender<Event>) {
        loop {
            while let Some(event) = self.self_rx.recv().await {
                match event {
                    Event::NewRequest(role, task) => {
                        if role == "validator" {
                            let (outcome, task) = self.execute_step(task).await;
                            let _ = manager_tx.send(Event::AgentResponse(role, outcome, task));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Validates that the LLM response is properly formatted.
///
/// # Arguments
/// * `resp` - The response string from the LLM
///
/// # Returns
/// * `bool` - True if response is "validated", starts with "not valid:", or contains "MODULE_REQUEST:"
fn validate_validator_response(resp: &str) -> bool {
    let lower = resp.to_lowercase();
    lower == "validated" || lower.starts_with("not valid:") || lower.contains("MODULE_REQUEST:")
}

#[async_trait]
impl AgentBehavior for ValidatorAgent {
    /// Executes a single step of the validation process.
    ///
    /// This method:
    /// 1. Retrieves the current proposal and context
    /// 2. Builds a prompt including the proposal, context and module execution history
    /// 3. Calls the LLM to validate the proposal
    /// 4. Processes the response to determine if the proposal is valid or needs revision
    ///
    /// # Arguments
    /// * `task` - The task containing the proposal and context to validate
    ///
    /// # Returns
    /// * `AgentOutcome` - The result of the validation (Validated, RevisionRequested, or Failed)
    async fn execute_step(&self, mut task: Task) -> (AgentOutcome, Task) {
        debug!("ValidatorAgent: validating final proposal...");

        let proposal = if let Some(sol) = &task.current_proposal {
            sol
        } else {
            return (
                AgentOutcome::Failed("No proposal found in task for validation".to_string()),
                task,
            );
        };

        let combined_context = task.context.combined_context();
        if combined_context.trim().is_empty() {
            return (
                AgentOutcome::Failed("No context available for validation".to_string()),
                task,
            );
        }

        let mut prompt = String::new();
        prompt.push_str("Current role: validator\n");
        prompt.push_str("Specific instructions: ");
        prompt.push_str(&self.user_prompt);
        prompt.push_str("\n\nContext:\n");
        prompt.push_str(&combined_context);
        prompt.push_str("\n\nFinal proposal:\n");
        prompt.push_str(proposal);

        prompt.push_str(
            "\n\nPlease respond with 'validated', 'not valid: ...' or 'MODULE_REQUEST:'.",
        );

        task.conversation.push(ChatMessage::new("user", &prompt));

        match self
            .llm_client
            .call_llm_with_format_check(
                &mut task.conversation,
                validate_validator_response,
                VALIDATOR_FORMAT_REMINDER,
                2,
            )
            .await
        {
            Ok(response) => {
                let resp = response.trim();
                task.conversation.push(ChatMessage::new("assistant", resp));
                if let Some(mr) = self.parse_module_request(resp) {
                    return (mr, task);
                }
                if resp.eq_ignore_ascii_case("validated") {
                    task.set_feedback(None);
                    (AgentOutcome::Validated, task)
                } else if resp.to_lowercase().starts_with("not valid:") {
                    let reason = resp.trim_start_matches("not valid:").trim().to_string();
                    task.set_feedback(Some(reason));
                    (AgentOutcome::RevisionRequested, task)
                } else {
                    (
                        AgentOutcome::Failed("Unexpected LLM response in Validator".to_string()),
                        task,
                    )
                }
            }
            Err(e) => (
                AgentOutcome::Failed(format!("LLM error in Validator: {}", e)),
                task,
            ),
        }
    }
}
