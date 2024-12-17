use super::manager::TaskManager;
use crate::agents::{FormatterAgent, ProposerAgent, ReviewerAgent, ValidatorAgent};
use crate::event::Event;

/// Implementation of the TaskManager's run functionality.
///
/// This implementation handles:
/// - Task execution flow and state management
/// - Agent role transitions based on workflow
/// - Module request handling and caching
/// - Error handling and retries
/// - Conversation export
/// - Post-completion feedback collection
impl TaskManager {
    pub async fn run(&mut self) {
        let (llm_provider, llm_model) = Self::extract_llm_config(&self.config);
        let output_format = self.config.output.format.clone();
        let output_file = self.config.output.file.clone();

        let proposer_config = self.config.agents.proposer.clone();
        let (proposer, proposer_tx) = ProposerAgent::new(proposer_config, llm_provider, llm_model);

        let reviewer_config = self.config.agents.reviewer.clone();
        let (reviewer, reviewer_tx) = ReviewerAgent::new(reviewer_config, llm_provider, llm_model);

        let validator_config = self.config.agents.validator.clone();
        let (validator, validator_tx) =
            ValidatorAgent::new(validator_config, llm_provider, llm_model);

        let formatter_config = self.config.agents.formatter.clone();
        let (formatter, formatter_tx) = FormatterAgent::new(
            formatter_config,
            llm_provider,
            llm_model,
            output_format,
            output_file,
        );

        self.set_agent_channels(proposer_tx, reviewer_tx, validator_tx, formatter_tx);

        tokio::spawn(proposer.run_loop(self.self_tx.clone()));
        tokio::spawn(reviewer.run_loop(self.self_tx.clone()));
        tokio::spawn(validator.run_loop(self.self_tx.clone()));
        tokio::spawn(formatter.run_loop(self.self_tx.clone()));

        self.execute_role("proposer", self.task.clone()).await;

        loop {
            while let Some(msg) = self.self_rx.recv().await {
                match msg {
                    Event::AgentResponse(role, outcome, task) => {
                        self.handle_agent_response(role, outcome, task).await;
                    }
                    _ => {}
                }
            }
        }
    }
}
