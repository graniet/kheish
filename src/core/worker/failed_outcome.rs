use crate::core::task_state::TaskState;
use crate::core::Task;
use crate::core::TaskWorker;
use crate::event::Event;

impl TaskWorker {
    /// Handles a failed outcome from an agent by implementing retry logic
    ///
    /// If the maximum number of retries has not been reached, this will:
    /// 1. Increment the retry counter
    /// 2. Truncate the conversation to the last successful message
    /// 3. Re-execute the current role
    ///
    /// If max retries are exceeded, it will:
    /// 1. Mark the task as permanently failed
    /// 2. Update the task state
    /// 3. Notify the task manager
    ///
    /// # Arguments
    /// * `reason` - The error message explaining why the agent failed
    /// * `current_role` - The role that was executing when the failure occurred
    /// * `task` - The task being processed
    pub async fn handle_failed_outcome(
        &mut self,
        reason: String,
        current_role: &str,
        mut task: Task,
    ) {
        let next_attempt = self.retry_count + 1;
        if next_attempt <= self.max_retries {
            let message = format!(
                "The agent encountered an error. Retrying... Attempt {}/{}",
                next_attempt, self.max_retries
            );

            if let Some(manager_tx) = self.get_manager_tx() {
                let _ = manager_tx.send(Event::NewMessage(self.task_id.clone(), message));
            }

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

        let message = format!("The task failed permanently: {}", reason);
        if let Some(manager_tx) = self.get_manager_tx() {
            let _ = manager_tx.send(Event::NewMessage(self.task_id.clone(), message));
        }

        task.state = TaskState::Failed(format!(
            "Task failed after {} retries. Last error: {}",
            self.max_retries, reason
        ));

        if let Some(manager_tx) = self.get_manager_tx() {
            let _ = manager_tx.send(Event::TaskStateUpdated(self.task_id.clone(), task.state));
        }
    }
}
