use crate::agents::AgentOutcome;
use crate::core::task_manager::utils::pause_and_update;
use crate::core::Task;
use crate::core::TaskManager;

impl TaskManager {
    /// Handles standard agent outcomes during task execution
    ///
    /// This function processes standard agent outcomes by:
    /// - Resetting retry count
    /// - Determining next workflow step based on outcome condition
    /// - Handling revision requests and task completion
    /// - Transitioning to next role in workflow
    /// - Handling workflow errors
    ///
    /// # Arguments
    ///
    /// * `outcome` - The outcome from the agent's execution
    /// * `current_role` - The current role being executed
    /// * `task` - The task being processed
    pub async fn handle_standard_outcome(
        &mut self,
        outcome: AgentOutcome,
        current_role: &mut String,
        mut task: Task,
    ) {
        self.retry_count = 0;
        let condition = outcome.as_condition();
        match self.workflow.next_role(&current_role, condition) {
            Some(next_role) => {
                if condition == "revision_requested" {
                    pause_and_update(
                        &self.spinner,
                        &format!(
                            "🔄 The agent requests a revision. Moving from '{}' to '{}'.",
                            current_role, next_role
                        ),
                    )
                    .await;
                    self.revision_count += 1;
                }

                if next_role == "completed" {
                    self.handle_task_completion(task).await;
                    return;
                }

                *current_role = next_role.clone();
                self.execute_role(&next_role, task.clone()).await;
            }
            None => {
                self.spinner.finish_and_clear();
                println!("Workflow error: No matching next step found");
                task.state = crate::core::task_state::TaskState::Failed(
                    "No matching workflow step".to_string(),
                );
            }
        }
    }
}
