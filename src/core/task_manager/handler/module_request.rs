use crate::core::task_manager::utils::pause_and_update;
use crate::core::task_state::TaskState;
use crate::core::Task;
use crate::core::TaskManager;
use crate::llm::ChatMessage;
use tracing::error;

impl TaskManager {
    /// Handles module requests from agents during task execution
    ///
    /// This function processes module requests by:
    /// - Checking the module cache for previous results
    /// - Executing the requested module action with parameters
    /// - Handling success/failure cases and updating task state
    /// - Managing large results and error messages
    /// - Continuing task execution with the current role
    ///
    /// # Arguments
    ///
    /// * `module_name` - Name of the module to execute
    /// * `action` - Action to perform within the module
    /// * `params` - Parameters for the module action
    /// * `current_role` - Current agent role making the request
    /// * `task` - Current task state and context
    ///
    /// # Examples
    ///
    /// ```
    /// let mut task_manager = TaskManager::new();
    /// task_manager.handle_module_request(
    ///     "git".to_string(),
    ///     "status".to_string(),
    ///     vec![],
    ///     "proposer",
    ///     task
    /// ).await;
    /// ```
    pub async fn handle_module_request(
        &mut self,
        module_name: String,
        action: String,
        params: Vec<String>,
        current_role: &str,
        mut task: Task,
    ) {
        pause_and_update(
            &self.spinner,
            &format!(
                "🔌 The agent requests the '{}' module to assist...",
                module_name
            ),
        )
        .await;

        self.retry_count = 0;
        let module_cache_key = (module_name.clone(), action.clone(), params.clone());
        if self.module_results_cache.contains_key(&module_cache_key) {
            pause_and_update(
                &self.spinner,
                "♻️ Module result already known, proceeding...",
            )
            .await;
            self.execute_role(current_role, task.clone()).await;
            return;
        }

        if let Some(module) = self.modules_manager.get_module(&module_name) {
            pause_and_update(
                &self.spinner,
                &format!(
                    "⚡ Executing module '{}' with action '{}' and params: {}",
                    module_name,
                    action,
                    params.join(" ")
                ),
            )
            .await;

            let action_result = module
                .handle_action(&mut self.vector_store, &action, &params)
                .await;

            let execution_message = match &action_result {
                Ok(result) => {
                    self.module_results_cache
                        .insert(module_cache_key, result.clone());

                    if result.chars().count() > 35000 {
                        format!(
                            "The result from module {} action '{}' is too large. Consider using the RAG module to index the content.\nFirst part: {}...",
                            module_name,
                            action,
                            &result[..200]
                        )
                    } else {
                        format!("Module '{}' provided a result:\n{}", module_name, result)
                    }
                }
                Err(e) => {
                    pause_and_update(
                        &self.spinner,
                        &format!(
                            "Module '{}' action '{}' failed. Stopping task.",
                            module_name, action
                        ),
                    )
                    .await;

                    error!("Module {} action '{}' failed: {}", module_name, action, e);
                    let action_availables = module
                        .get_actions()
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let err_msg = format!(
                        "Module {} action '{}' failed: {} \
Available actions: {}",
                        module_name, action, e, action_availables
                    );
                    task.state = TaskState::Failed(err_msg.clone());
                    err_msg
                }
            };

            task.conversation
                .push(ChatMessage::new("user", &execution_message));

            pause_and_update(
                &self.spinner,
                "⚙️ Module execution finished. Returning to the agent...",
            )
            .await;
            self.execute_role(current_role, task.clone()).await;
        } else {
            pause_and_update(
                &self.spinner,
                &format!(
                    "The agent tried to use a non-existent module '{}'.",
                    module_name
                ),
            )
            .await;

            let err_msg = format!(
                "Module {} not found. Available modules and their actions: {}",
                module_name,
                self.modules_manager
                    .modules
                    .iter()
                    .map(|m| format!(
                        "{} (actions: {})",
                        m.name(),
                        m.get_actions()
                            .iter()
                            .map(|a| a.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            );

            task.conversation
                .push(ChatMessage::new("assistant", &err_msg));
            task.state = TaskState::Failed(format!("Module {} not found", module_name));
            self.execute_role(current_role, task.clone()).await;
        }
    }
}
