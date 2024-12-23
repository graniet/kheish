use crate::agents::AgentOutcome;
use crate::core::Task;
use crate::core::TaskWorker;
use crate::event::Event;
use tracing::error;

impl TaskWorker {
    /// Handles standard outcomes from agent executions
    ///
    /// This function processes the standard outcome from an agent by:
    /// - Resetting the retry counter
    /// - Determining the next workflow role based on the outcome condition
    /// - Managing revision requests with appropriate messaging
    /// - Handling exports and output notifications
    /// - Processing task completion
    /// - Executing the next role or handling workflow errors
    ///
    /// # Arguments
    ///
    /// * `outcome` - The outcome returned by the agent execution
    /// * `current_role` - Mutable reference to the current role being executed
    /// * `task` - The task being processed
    pub async fn handle_standard_outcome(
        &mut self,
        outcome: AgentOutcome,
        current_role: &mut String,
        mut task: Task,
    ) {
        self.retry_count = 0;
        let condition = outcome.as_condition();
        match self.workflow.next_role(current_role, condition) {
            Some(next_role) => {
                if condition == "revision_requested" {
                    let message = format!(
                        "🔄 The agent requests a revision. Moving from '{}' to '{}'.",
                        current_role, next_role
                    );
                    if let Some(manager_tx) = self.get_manager_tx() {
                        let _ = manager_tx
                            .send(Event::NewMessage(self.task_id.clone(), message.to_string()));
                    }
                    self.revision_count += 1;
                    self.execute_role(&next_role, task.clone()).await;
                }

                if condition == "exported" || outcome == AgentOutcome::Exported {
                    let message = format!(
                        "🔄 The agent has exported the task. Moving from '{}' to '{}'.",
                        current_role, next_role
                    );
                    if let Some(manager_tx) = self.get_manager_tx() {
                        let _ = manager_tx
                            .send(Event::NewMessage(self.task_id.clone(), message.to_string()));
                        let _ = manager_tx.send(Event::NewOutput(
                            self.task_id.clone(),
                            task.final_output.clone().unwrap_or_default(),
                        ));
                    }
                }

                if next_role == "completed" {
                    self.handle_task_completion(task).await;
                    return;
                }

                *current_role = next_role.clone();
                self.execute_role(&next_role, task.clone()).await;
            }
            None => {
                let message = match outcome {
                    AgentOutcome::Failed(ref error) => format!(
                        "Workflow error: {}, step: {}, condition: {}",
                        error, current_role, condition
                    ),
                    _ => format!(
                        "Workflow error: No matching next step found, step: {}, condition: {}",
                        current_role, condition
                    ),
                };

                error!("{}", message);
                if let Some(manager_tx) = self.get_manager_tx() {
                    let _ = manager_tx.send(Event::NewMessage(self.task_id.clone(), message));
                }
                task.state = crate::core::task_state::TaskState::Failed(
                    "No matching workflow step".to_string(),
                );
            }
        }
    }
}
