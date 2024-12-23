use crate::core::task_state::TaskState;
use crate::core::Task;
use crate::core::TaskWorker;
use crate::event::Event;
use crate::llm::ChatMessage;
use tracing::error;

impl TaskWorker {
    /// Handles a module request from an agent by executing the requested module action
    ///
    /// This function:
    /// 1. Checks if the result is already cached
    /// 2. Executes the module action if not cached
    /// 3. Handles success/failure cases
    /// 4. Updates the task conversation with the result
    /// 5. Continues task execution
    ///
    /// # Arguments
    /// * `module_name` - Name of the module to execute
    /// * `action` - Action to perform on the module
    /// * `params` - Parameters for the module action
    /// * `current_role` - Current role executing the task
    /// * `task` - Task being processed
    pub async fn handle_module_request(
        &mut self,
        module_name: String,
        action: String,
        params: Vec<String>,
        current_role: &str,
        mut task: Task,
    ) {
        if let Some(manager_tx) = self.get_manager_tx() {
            let _ = manager_tx.send(Event::NewMessage(
                self.task_id.clone(),
                format!(
                    "🔌 The agent requests the '{}' module to assist...",
                    module_name
                ),
            ));
        }

        self.retry_count = 0;
        let module_cache_key = (module_name.clone(), action.clone(), params.clone());
        if self.module_results_cache.contains_key(&module_cache_key) {
            let message = "♻️ Module result already known, proceeding...";
            if let Some(manager_tx) = self.get_manager_tx() {
                let _ =
                    manager_tx.send(Event::NewMessage(self.task_id.clone(), message.to_string()));
            }
            self.execute_role(current_role, task.clone()).await;
            return;
        }

        if let Some(module) = self.modules_manager.get_module(&module_name) {
            let message = format!(
                "⚡ Executing module '{}' with action '{}' and params: {}",
                module_name,
                action,
                params.join(" ")
            );

            if let Some(manager_tx) = self.get_manager_tx() {
                let _ =
                    manager_tx.send(Event::NewMessage(self.task_id.clone(), message.to_string()));
            }

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
                    let message = format!(
                        "Module '{}' action '{}' failed. Stopping task.",
                        module_name, action
                    );
                    if let Some(manager_tx) = self.get_manager_tx() {
                        let _ = manager_tx
                            .send(Event::NewMessage(self.task_id.clone(), message.to_string()));
                    }

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

            let message = "⚙️ Module execution finished. Returning to the agent...";
            if let Some(manager_tx) = self.get_manager_tx() {
                let _ =
                    manager_tx.send(Event::NewMessage(self.task_id.clone(), message.to_string()));
            }
            self.execute_role(current_role, task.clone()).await;
        } else {
            let message = format!(
                "The agent tried to use a non-existent module '{}'.",
                module_name
            );
            if let Some(manager_tx) = self.get_manager_tx() {
                let _ =
                    manager_tx.send(Event::NewMessage(self.task_id.clone(), message.to_string()));
            }
            task.state = TaskState::Failed(format!("Module {} not found", module_name));

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
