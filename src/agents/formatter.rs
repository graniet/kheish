use super::{AgentBehavior, AgentOutcome};
use crate::core::Task;
use crate::llm::{ChatMessage, LlmClient};
use async_trait::async_trait;
use std::fs;
use tracing::{debug, info};

pub struct FormatterAgent<'a> {
    pub llm_client: &'a LlmClient,
    pub user_prompt: &'a str,
    pub output_format: &'a str,
    pub output_file: &'a str,
}

fn validate_final_output(resp: &str) -> bool {
    !resp.trim().is_empty()
}

#[async_trait]
impl<'a> AgentBehavior for FormatterAgent<'a> {
    async fn execute_step(&self, task: &mut Task) -> AgentOutcome {
        debug!("FormatterAgent: formatting final proposal...");
        let proposal = if let Some(sol) = &task.current_proposal {
            sol
        } else {
            return AgentOutcome::Failed(
                "No final solution found in task for formatting".to_string(),
            );
        };

        let mut prompt = String::new();
        prompt.push_str("Current role: formatter\n");
        prompt.push_str("Specific instructions: ");
        prompt.push_str(self.user_prompt);
        prompt.push_str("\n\nValidated solution:\n");
        prompt.push_str(proposal);
        prompt.push_str("\n\nConvert this solution to ");
        prompt.push_str(self.output_format);
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
                    if let Err(e) = fs::write(self.output_file, response.as_bytes()) {
                        return AgentOutcome::Failed(format!("Failed to write output file: {}", e));
                    }
                    info!(
                        "FormatterAgent: result written to file {}",
                        self.output_file
                    );
                    task.final_output = Some(response);
                    AgentOutcome::Exported
                } else {
                    AgentOutcome::Failed("Formatted output is invalid or empty.".to_string())
                }
            }
            Err(e) => AgentOutcome::Failed(format!("LLM error in Formatter: {}", e)),
        }
    }
}
