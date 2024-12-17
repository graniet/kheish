use crate::core::task_manager::utils::pause_and_update;
use crate::core::task_state::TaskState;
use crate::core::Task;
use crate::core::TaskManager;
use crate::llm::ChatMessage;
use std::io::Write;
use tracing::error;

impl TaskManager {
    /// Handles the successful completion of a task
    ///
    /// This function:
    /// - Displays completion message
    /// - Exports conversation data if configured
    /// - Prompts for post-completion feedback if enabled
    /// - Updates task state to completed
    ///
    /// # Arguments
    ///
    /// * `task` - The completed task to handle
    ///
    /// # Examples
    ///
    /// ```
    /// let mut task_manager = TaskManager::new();
    /// let task = Task::new("example");
    /// task_manager.handle_task_completion(task).await;
    /// ```
    pub async fn handle_task_completion(&mut self, mut task: Task) {
        self.spinner.finish_and_clear();
        println!(
            "✅ The task '{}' has been successfully completed!",
            task.name
        );

        if self.config.parameters.export_conversation {
            let export_spinner = indicatif::ProgressBar::new_spinner();
            export_spinner.set_style(
                indicatif::ProgressStyle::default_spinner()
                    .tick_chars("-\\|/")
                    .template("{spinner} {msg}")
                    .unwrap(),
            );
            export_spinner.set_message("Exporting conversation data...");
            export_spinner.enable_steady_tick(std::time::Duration::from_millis(120));

            let json_path = format!(
                "logs/{}-{}-data.json",
                task.name,
                chrono::Local::now().format("%Y-%m-%d")
            );

            let export_res = serde_json::to_string_pretty(&task.conversation)
                .and_then(|json| std::fs::write(&json_path, json).map_err(serde_json::Error::io));

            export_spinner.finish_and_clear();
            match export_res {
                Ok(_) => {
                    println!("📝 Conversation exported to {}", json_path)
                }
                Err(e) => error!("Failed to export conversation: {}", e),
            }
        }

        if self.config.parameters.post_completion_feedback {
            println!("== Would you like to provide additional feedback? (Press Enter to skip) ==");
            print!("Kheish |> ");
            std::io::stdout().flush().expect("Failed to flush stdout");

            let mut feedback_input = String::new();
            std::io::stdin()
                .read_line(&mut feedback_input)
                .expect("Failed to read user input");
            let feedback_input = feedback_input.trim();

            if !feedback_input.is_empty() {
                task.conversation
                    .push(ChatMessage::new("user", feedback_input));
                self.revision_count += 1;

                self.spinner
                    .enable_steady_tick(std::time::Duration::from_millis(120));
                pause_and_update(
                    &self.spinner,
                    "💬 Feedback received. The proposer will prepare a new revision...",
                )
                .await;

                self.execute_role("proposer", task.clone()).await;
            }
        }

        task.state = TaskState::Completed;
    }
}
