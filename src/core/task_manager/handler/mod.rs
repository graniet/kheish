/// Module for handling different agent response outcomes
mod failed_outcome;
mod module_request;
mod standard_outcome;
mod task_completion;

use super::TaskManager;
use crate::agents::AgentOutcome;
use crate::core::Task;

impl TaskManager {
    /// Handles responses from agents after they complete their tasks
    ///
    /// # Arguments
    /// * `current_role` - The role of the agent that generated the response
    /// * `agent_outcome` - The outcome/result from the agent's execution
    /// * `task` - The task context that was being worked on
    ///
    /// This function routes the agent's response to the appropriate handler based on the outcome:
    /// - Failed outcomes are sent to handle_failed_outcome
    /// - Module requests are sent to handle_module_request  
    /// - All other outcomes are treated as standard and sent to handle_standard_outcome
    pub async fn handle_agent_response(
        &mut self,
        current_role: String,
        agent_outcome: AgentOutcome,
        task: Task,
    ) {
        match agent_outcome {
            AgentOutcome::Failed(reason) => {
                self.handle_failed_outcome(reason, &current_role, task)
                    .await;
            }
            AgentOutcome::ModuleRequest(module_name, action, params) => {
                self.handle_module_request(module_name, action, params, &current_role, task)
                    .await;
            }
            other_outcome => {
                let mut current_role = current_role.clone();
                self.handle_standard_outcome(other_outcome, &mut current_role, task)
                    .await;
            }
        }
    }
}
