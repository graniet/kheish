use crate::core::task_manager::utils::pause_and_update;
use crate::core::task_state::TaskState;
use crate::core::Task;
use crate::core::TaskManager;

impl TaskManager {
    /// Handles a failed task outcome by attempting retries or marking as permanently failed
    ///
    /// # Arguments
    ///
    /// * `reason` - The error reason/message for why the task failed
    /// * `current_role` - The role that was executing when the failure occurred
    /// * `task` - The task that failed
    ///
    /// # Details
    ///
    /// This function will:
    /// 1. Attempt to retry the failed task up to max_retries times
    /// 2. For retries:
    ///    - Update the spinner with retry attempt count
    ///    - Truncate conversation history to last successful message
    ///    - Re-execute the current role
    /// 3. If max retries exceeded:
    ///    - Mark task as permanently failed
    ///    - Display failure message
    pub async fn handle_failed_outcome(
        &mut self,
        reason: String,
        current_role: &str,
        mut task: Task,
    ) {
        let next_attempt = self.retry_count + 1;
        if next_attempt <= self.max_retries {
            pause_and_update(
                &self.spinner,
                &format!(
                    "The agent encountered an error. Retrying... Attempt {}/{}",
                    next_attempt, self.max_retries
                ),
            )
            .await;

            self.retry_count = next_attempt;
            if let Some(last_success) = task
                .conversation
                .iter()
                .rposition(|msg| msg.role == "assistant" && !msg.content.contains("error"))
            {
                task.conversation.truncate(last_success + 1);
            }

            self.execute_role(current_role, task.clone()).await;
            return;
        }

        self.spinner.finish_and_clear();
        println!("The task failed permanently: {}", reason);
        task.state = TaskState::Failed(format!(
            "Task failed after {} retries. Last error: {}",
            self.max_retries, reason
        ));
    }
}
