use super::manager::TaskManager;
use super::utils::pause_and_update;
use crate::core::Task;
use crate::event::Event;
use tracing::debug;

/// Executes a specific agent role in the task workflow
///
/// # Arguments
/// * `manager` - Mutable reference to the TaskManager instance
/// * `role` - Role identifier string for the agent to execute
///
/// # Returns
/// * `AgentOutcome` - Result of the agent's execution step
impl TaskManager {
    pub async fn execute_role(&mut self, role: &str, task: Task) {
        debug!("Executing role {}", role);

        let human_message = match role {
            "proposer" => "🤔 The proposer is preparing a new proposal...",
            "reviewer" => "🔍 The reviewer is examining the proposal...",
            "validator" => "✅ The validator is checking correctness...",
            "formatter" => "✨ The formatter is refining the final output...",
            _ => "❓ An unknown agent is acting...",
        };

        pause_and_update(&self.spinner, human_message).await;
        pause_and_update(&self.spinner, &format!("🔄 {} is now working...", role)).await;

        if let Some(tx) = self.get_role_tx(role) {
            let _ = tx.send(Event::NewRequest(role.to_string(), task));
        } else {
            eprintln!("No tx found for role: {}", role);
        }
    }
}
