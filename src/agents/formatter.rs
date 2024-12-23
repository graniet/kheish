use super::{AgentBehavior, AgentOutcome};
use crate::config::AgentConfig;
use crate::constants::FORMATTER_USER_PROMPT;
use crate::core::Task;
use crate::event::Event;
use crate::llm::{ChatMessage, LlmClient};
use crate::llm::{build_validator, validate_response};
use async_trait::async_trait;
use std::fs;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info};

/// Agent responsible for formatting the final solution into the desired output format
pub struct FormatterAgent {
    /// LLM client for interacting with the language model
    pub llm_client: LlmClient,
    /// Custom prompt to guide the formatting behavior
    pub user_prompt: String,
    /// Target format for the output (e.g. "markdown", "html")
    pub output_format: String,
    /// File path where the formatted output will be written
    pub output_file: String,
    /// Channel receiver for incoming events
    pub self_rx: UnboundedReceiver<Event>,
    /// Schema for the output
    pub schema: Option<String>,
}

impl FormatterAgent {
    /// Creates a new FormatterAgent instance
    ///
    /// # Arguments
    /// * `config` - Configuration for the agent
    /// * `llm_provider` - The LLM provider to use (e.g. "openai")
    /// * `llm_model` - The specific model to use (e.g. "gpt-4")
    /// * `output_format` - The desired output format (e.g. "markdown", "html")
    /// * `output_file` - Path where the formatted output will be written
    ///
    /// # Returns
    /// * `(Self, UnboundedSender<Event>)` - The agent instance and a channel sender for events
    pub fn new(
        config: AgentConfig,
        llm_provider: &str,
        llm_model: &str,
        output_format: String,
        output_file: String,
    ) -> (Self, UnboundedSender<Event>) {
        let (self_tx, self_rx) = tokio::sync::mpsc::unbounded_channel();
        let content_schema = match config.schema.clone() {
            Some(schema) => {
                if let Some(stripped) = schema.strip_prefix("file://") {
                    std::fs::read_to_string(stripped).ok()
                } else {
                    Some(schema)
                }
            }
            None => None
        };

        (   
            Self {
                llm_client: LlmClient::new(llm_provider, llm_model)
                    .expect("Failed to create LLM client"),
                user_prompt: config
                    .user_prompt
                    .as_deref()
                    .unwrap_or(FORMATTER_USER_PROMPT)
                    .to_string(),
                output_format,
                output_file,
                self_rx,
                schema: content_schema,
            },
            self_tx,
        )
    }

    /// Main event loop that processes incoming events
    ///
    /// # Arguments
    /// * `worker_tx` - Channel sender to communicate back to the worker
    pub async fn run_loop(mut self, worker_tx: UnboundedSender<Event>) {
        loop {
            while let Some(event) = self.self_rx.recv().await {
                if let Event::NewRequest(role, task) = event {
                    if role == "formatter" {
                        let (outcome, task) = self.execute_step(task).await;
                        let _ = worker_tx.send(Event::AgentResponse(role, outcome, task));
                    }
                }
            }
        }
    }
}

/// Validates that the formatted output is not empty
///
/// # Arguments
/// * `resp` - The formatted response string to validate
///
/// # Returns
/// * `bool` - True if the response is non-empty after trimming whitespace
fn validate_final_output(resp: &str) -> bool {
    !resp.trim().is_empty()
}

/// Validates a response string against a JSON schema
///
/// # Arguments
/// * `schema` - The JSON schema string to validate against
/// * `resp` - The response string to validate
///
/// # Returns
/// * `bool` - True if validation succeeds, false if validation fails or there's an error
fn validate_schema(schema: &str, resp: &str) -> bool {
    build_validator(schema)
        .ok()
        .and_then(|validator| validate_response(&validator, resp).ok())
        .unwrap_or(false)
}

#[async_trait]
impl AgentBehavior for FormatterAgent {
    /// Executes the formatting step on the provided task
    ///
    /// # Arguments
    /// * `task` - The task containing the solution to format
    ///
    /// # Returns
    /// * `(AgentOutcome, Task)` - The outcome of the formatting and the updated task
    async fn execute_step(&self, mut task: Task) -> (AgentOutcome, Task) {
        debug!("FormatterAgent: formatting final proposal...");
        let proposal = if let Some(sol) = &task.current_proposal {
            sol
        } else {
            return (
                AgentOutcome::Failed("No final solution found in task for formatting".to_string()),
                task,
            );
        };

        let mut prompt = String::new();
        prompt.push_str("Current role: formatter\n");
        prompt.push_str("Specific instructions: ");
        prompt.push_str(&self.user_prompt);
        prompt.push_str("\n\nValidated solution:\n");
        prompt.push_str(proposal);
        prompt.push_str("\n\nConvert this solution to ");
        prompt.push_str(&self.output_format);
        prompt.push_str(" and only output the final formatted result, without comments.");

        if let Some(schema) = &self.schema {
            prompt.push_str("\n\nSchema:\n");
            prompt.push_str(schema);
        }

        task.conversation.push(ChatMessage::new("user", &prompt));

        match self
            .llm_client
            .call_llm_with_format_check(&mut task.conversation, validate_final_output, "", 2)
            .await
        {
            Ok(response) => {
                debug!("FormatterAgent: raw formatted output received.");
                if !validate_final_output(&response) {
                    return (
                        AgentOutcome::Failed("Formatted output is invalid or empty.".to_string()),
                        task,
                    );
                }

                if let Some(schema) = &self.schema {
                    if !validate_schema(schema, &response) {
                        return (
                            AgentOutcome::Failed("Output does not match schema".to_string()),
                            task,
                        );
                    }
                }

                if let Err(e) = fs::write(&self.output_file, response.as_bytes()) {
                    return (
                        AgentOutcome::Failed(format!("Failed to write output file: {}", e)),
                        task,
                    );
                }

                info!("FormatterAgent: result written to file {}", self.output_file);

                let cleaned_response = response.replace("```json", "").replace("```", "").trim().to_string();
                task.final_output = Some(serde_json::from_str(&cleaned_response).unwrap_or_default());
                
                (AgentOutcome::Exported, task)
            }
            Err(e) => (
                AgentOutcome::Failed(format!("LLM error in Formatter: {}", e)),
                task,
            ),
        }
    }
}
