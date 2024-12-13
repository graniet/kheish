use super::manager::TaskManager;
use super::role_execution::execute_role;
use super::utils::pause_and_update;
use crate::agents::AgentOutcome;
use crate::core::task_state::TaskState;
use crate::llm::ChatMessage;
use std::io::Write;
use tracing::error;

/// Implementation of the TaskManager's run functionality.
/// 
/// This implementation handles:
/// - Task execution flow and state management
/// - Agent role transitions based on workflow
/// - Module request handling and caching
/// - Error handling and retries
/// - Conversation export
/// - Post-completion feedback collection
impl TaskManager {
    /// Executes the task workflow by coordinating agents, modules and managing the overall task state.
    /// 
    /// The method will:
    /// - Initialize task state and display
    /// - Execute agent roles in sequence based on workflow
    /// - Handle module requests and caching
    /// - Manage retries on failures
    /// - Export conversation if configured
    /// - Collect post-completion feedback if enabled
    pub async fn run(&mut self) {
        self.spinner.set_message(format!(
            "{} | {} | {}",
            self.task.name,
            self.config.description.as_deref().unwrap_or(""),
            self.config.version.as_deref().unwrap_or("")
        ));
        self.task.state = TaskState::InProgress;

        let mut current_role = "proposer".to_string();

        loop {
            let agent_outcome = execute_role(self, &current_role).await;

            match agent_outcome {
                AgentOutcome::Failed(reason) => {
                    let next_attempt = self.retry_count + 1;
                    if next_attempt <= self.max_retries {
                        pause_and_update(&self.spinner, &format!(
                            "The agent encountered an error. Retrying... Attempt {}/{}",
                            next_attempt, self.max_retries
                        ))
                        .await;

                        self.retry_count = next_attempt;
                        if let Some(last_success) = self.task.conversation.iter().rposition(|msg| {
                            msg.role == "assistant" && !msg.content.contains("error")
                        }) {
                            self.task.conversation.truncate(last_success + 1);
                        }
                        continue;
                    }

                    self.spinner.finish_and_clear();
                    println!("The task failed permanently: {}", reason);
                    self.task.state = TaskState::Failed(format!(
                        "Task failed after {} retries. Last error: {}",
                        self.max_retries, reason
                    ));
                    break;
                }

                AgentOutcome::ModuleRequest(module_name, action, params) => {
                    pause_and_update(&self.spinner, &format!(
                        "🔌 The agent requests the '{}' module to assist...",
                        module_name
                    ))
                    .await;

                    self.retry_count = 0;
                    let module_cache_key = (module_name.clone(), action.clone(), params.clone());
                    if self.module_results_cache.contains_key(&module_cache_key) {
                        pause_and_update(&self.spinner, "♻️ Module result already known, proceeding...")
                            .await;
                        continue;
                    }

                    if let Some(module) = self.modules_manager.get_module(&module_name) {
                        pause_and_update(&self.spinner, &format!(
                            "⚡ Executing module '{}' with action '{}'...",
                            module_name, action
                        ))
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
                                pause_and_update(&self.spinner, &format!(
                                    "Module '{}' action '{}' failed. Stopping task.",
                                    module_name, action
                                ))
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
                                self.task.state = TaskState::Failed(err_msg.clone());
                                err_msg
                            }
                        };

                        self.task.conversation.push(ChatMessage::new("user", &execution_message));

                        pause_and_update(&self.spinner, "⚙️ Module execution finished. Returning to the agent...")
                            .await;
                        continue;
                    } else {
                        pause_and_update(&self.spinner, &format!(
                            "The agent tried to use a non-existent module '{}'.",
                            module_name
                        ))
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

                        self.task.conversation.push(ChatMessage::new("assistant", &err_msg));
                        self.task.state =
                            TaskState::Failed(format!("Module {} not found", module_name));
                        continue;
                    }
                }

                outcome => {
                    self.retry_count = 0;
                    let condition = outcome.as_condition();
                    match self.workflow.next_role(&current_role, condition) {
                        Some(next_role) => {
                            if condition == "revision_requested" {
                                pause_and_update(&self.spinner, &format!(
                                    "🔄 The agent requests a revision. Moving from '{}' to '{}'.",
                                    current_role, next_role
                                ))
                                .await;
                                self.revision_count += 1;
                            }

                            if next_role == "completed" {
                                self.spinner.finish_and_clear();
                                println!("✅ The task '{}' has been successfully completed!", self.task.name);

                                if self.config.parameters.export_conversation {
                                    let export_spinner = indicatif::ProgressBar::new_spinner();
                                    export_spinner.set_style(
                                        indicatif::ProgressStyle::default_spinner()
                                            .tick_chars("-\\|/")
                                            .template("{spinner} {msg}")
                                            .unwrap()
                                    );
                                    export_spinner.set_message("Exporting conversation data...");
                                    export_spinner.enable_steady_tick(std::time::Duration::from_millis(120));

                                    let json_path = format!(
                                        "logs/{}-{}-data.json",
                                        self.task.name,
                                        chrono::Local::now().format("%Y-%m-%d")
                                    );

                                    let export_res = serde_json::to_string_pretty(&self.task.conversation)
                                        .and_then(|json| std::fs::write(&json_path, json).map_err(serde_json::Error::io));

                                    export_spinner.finish_and_clear();
                                    match export_res {
                                        Ok(_) => println!("📝 Conversation exported to {}", json_path),
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
                                        self.task.conversation.push(ChatMessage::new("user", feedback_input));
                                        self.revision_count += 1;

                                        self.spinner.enable_steady_tick(std::time::Duration::from_millis(120));
                                        pause_and_update(&self.spinner, "💬 Feedback received. The proposer will prepare a new revision...")
                                            .await;
                                        current_role = "proposer".to_string();
                                        continue;
                                    }
                                }

                                break;
                            }

                            current_role = next_role;
                        }
                        None => {
                            self.spinner.finish_and_clear();
                            println!("Workflow error: No matching next step found");
                            self.task.state =
                                TaskState::Failed("No matching workflow step".to_string());
                            break;
                        }
                    }
                }
            }
        }
    }
}
