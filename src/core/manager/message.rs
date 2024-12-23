use crate::core::manager::utils::pause_and_update;
use crate::core::TaskManager;
use crate::errors::Error;

impl TaskManager {
    /// Handles a new message event by displaying it with the task ID
    ///
    /// # Arguments
    /// * `task_id` - The ID of the task this message belongs to
    /// * `message` - The message content to display
    pub async fn handle_new_message(
        &mut self,
        task_id: String,
        message: String,
    ) -> Result<(), Error> {
        pause_and_update(&self.spinner, &format!("{}: {}", task_id, message)).await;
        Ok(())
    }
}
