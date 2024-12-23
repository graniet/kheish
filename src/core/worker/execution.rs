use super::TaskWorker;
use crate::agents::AgentOutcome;
use crate::core::Task;
use crate::event::Event;
use tracing::debug;

impl TaskWorker {
    /// Handles the response from an agent by processing different outcome types
    ///
    /// # Arguments
    /// * `current_role` - The role of the agent that generated the response
    /// * `agent_outcome` - The outcome returned by the agent
    /// * `task` - The task being processed
    pub async fn handle_agent_response(
        &mut self,
        current_role: String,
        agent_outcome: AgentOutcome,
        task: Task,
    ) {
        match agent_outcome {
            AgentOutcome::ModuleRequest(module_name, action, params) => {
                self.handle_module_request(module_name, action, params, &current_role, task)
                    .await;
            }
            AgentOutcome::Failed(error_message) => {
                self.handle_failed_outcome(error_message, &current_role, task)
                    .await;
            }
            other_outcome => {
                let mut current_role = current_role.clone();
                self.handle_standard_outcome(other_outcome, &mut current_role, task)
                    .await;
            }
        }
    }

    /// Executes a specific role in the task workflow
    ///
    /// Sends appropriate events to notify the task manager and role handler about the execution.
    ///
    /// # Arguments
    /// * `role` - The role to execute (e.g., "proposer", "reviewer")
    /// * `task` - The task to be processed by the role
    pub async fn execute_role(&mut self, role: &str, task: Task) {
        debug!("Executing role {}", role);

        let human_message = match role {
            "proposer" => "🤔 The proposer is preparing a new proposal...",
            "reviewer" => "🔍 The reviewer is examining the proposal...",
            "validator" => "✅ The validator is checking correctness...",
            "formatter" => "✨ The formatter is refining the final output...",
            _ => "❓ An unknown agent is acting...",
        };

        if let Some(tx) = self.get_manager_tx() {
            let _ = tx.send(Event::NewMessage(
                self.task_id.clone(),
                human_message.to_string(),
            ));
            let _ = tx.send(Event::NewMessage(
                self.task_id.clone(),
                format!("🔄 {} is now working...", role),
            ));
        } else {
            eprintln!("No tx found for role: {}", role);
        }

        if let Some(tx) = self.get_role_tx(role) {
            let _ = tx.send(Event::NewRequest(role.to_string(), task));
        } else {
            eprintln!("No tx found for role: {}", role);
        }
    }
}
