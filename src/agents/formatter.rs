use super::{AgentBehavior, AgentOutcome};
use crate::config::AgentConfig;
use crate::constants::FORMATTER_USER_PROMPT;
use crate::core::Task;
use crate::event::Event;
use crate::llm::{ChatMessage, LlmClient};
use async_trait::async_trait;
use std::fs;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info};

pub struct FormatterAgent {
    pub llm_client: LlmClient,
    pub user_prompt: String,
    pub output_format: String,
    pub output_file: String,
    pub self_rx: UnboundedReceiver<Event>,
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
            },
            self_tx,
        )
    }

    pub async fn run_loop(mut self, manager_tx: UnboundedSender<Event>) {
        loop {
            while let Some(event) = self.self_rx.recv().await {
                match event {
                    Event::NewRequest(role, task) => {
                        if role == "formatter" {
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

fn validate_final_output(resp: &str) -> bool {
    !resp.trim().is_empty()
}

#[async_trait]
impl AgentBehavior for FormatterAgent {
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

        task.conversation.push(ChatMessage::new("user", &prompt));

        match self
            .llm_client
            .call_llm_with_format_check(&mut task.conversation, validate_final_output, "", 2)
            .await
        {
            Ok(response) => {
                debug!("FormatterAgent: raw formatted output received.");
                if validate_final_output(&response) {
                    if let Err(e) = fs::write(&self.output_file, response.as_bytes()) {
                        return (
                            AgentOutcome::Failed(format!("Failed to write output file: {}", e)),
                            task,
                        );
                    }
                    info!(
                        "FormatterAgent: result written to file {}",
                        self.output_file
                    );
                    task.final_output = Some(response);
                    (AgentOutcome::Exported, task)
                } else {
                    (
                        AgentOutcome::Failed("Formatted output is invalid or empty.".to_string()),
                        task,
                    )
                }
            }
            Err(e) => (
                AgentOutcome::Failed(format!("LLM error in Formatter: {}", e)),
                task,
            ),
        }
    }
}
