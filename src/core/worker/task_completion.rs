use crate::core::task_state::TaskState;
use crate::core::Task;
use crate::core::TaskWorker;
use crate::event::Event;

impl TaskWorker {
    /// Handles the completion of a task by performing cleanup and notifications
    ///
    /// This function:
    /// 1. Sends completion notification message
    /// 2. Exports conversation history if configured
    /// 3. Updates task state to completed
    /// 4. Notifies task manager of completion
    /// 5. Sends task completed event
    ///
    /// # Arguments
    /// * `task` - The task that has completed
    pub async fn handle_task_completion(&mut self, mut task: Task) {
        let message = format!(
            "✅ The task '{}' has been successfully completed!",
            task.name
        );
        if let Some(manager_tx) = self.get_manager_tx() {
            let _ = manager_tx.send(Event::NewMessage(self.task_id.clone(), message.to_string()));
        }
        if self.config.parameters.export_conversation {
            let json_path = format!(
                "logs/{}-{}-data.json",
                task.name,
                chrono::Local::now().format("%Y-%m-%d")
            );

            let export_res = serde_json::to_string_pretty(&task.conversation)
                .and_then(|json| std::fs::write(&json_path, json).map_err(serde_json::Error::io));

            let message = match export_res {
                Ok(_) => format!("📝 Conversation exported to {}", json_path),
                Err(e) => format!("Failed to export conversation: {}", e),
            };
            if let Some(manager_tx) = self.get_manager_tx() {
                let _ = manager_tx.send(Event::NewMessage(self.task_id.clone(), message));
            }
        }

        if self.config.parameters.post_completion_feedback {
            // println!("== Would you like to provide additional feedback? (Press Enter to skip) ==");
            // print!("Kheish |> ");
            // std::io::stdout().flush().expect("Failed to flush stdout");

            // let mut feedback_input = String::new();
            // std::io::stdin()
            //     .read_line(&mut feedback_input)
            //     .expect("Failed to read user input");
            // let feedback_input = feedback_input.trim();

            // if !feedback_input.is_empty() {
            //     task.conversation
            //         .push(ChatMessage::new("user", feedback_input));
            //     self.revision_count += 1;

            //     let message = "💬 Feedback received. The proposer will prepare a new revision...".to_string();
            //     if let Some(manager_tx) = self.get_manager_tx() {
            //         let _ = manager_tx.send(Event::NewMessage(self.task_id.clone(), message));
            //     }
            //     self.execute_role("proposer", task.clone()).await;
            // }
        }

        task.state = TaskState::Completed;

        if let Some(manager_tx) = self.get_manager_tx() {
            let _ = manager_tx.send(Event::TaskStateUpdated(self.task_id.clone(), task.state));
            let _ = manager_tx.send(Event::NewMessage(
                self.task_id.clone(),
                "Task completed".to_string(),
            ));
        }

        let _ = self
            .self_tx
            .send(Event::TaskCompleted(self.task_id.clone()));
    }
}
