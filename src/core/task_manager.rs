use super::rag::InMemoryVectorStore;
use super::task::Task;
use super::task_context::TaskContext;
use super::task_state::TaskState;
use super::workflow::Workflow;
use crate::agents::{AgentBehavior, AgentOutcome};
use crate::agents::{FormatterAgent, ProposerAgent, ReviewerAgent, ValidatorAgent};
use crate::config::TaskConfig;
use crate::constants::{
    FORMATTER_USER_PROMPT, PROPOSER_USER_PROMPT, REVIEWER_USER_PROMPT, VALIDATOR_USER_PROMPT,
};
use crate::llm::ChatMessage;
use crate::llm::LlmClient;
use crate::llm::OpenAIEmbedder;
use crate::modules::ModulesManager;
use crate::utils;
use std::collections::HashMap;
use std::fs;
use tracing::{debug, error, info};
use std::io::{self, Write};

/// Main manager for task execution and coordination.
///
/// This struct is responsible for:
/// - Managing the lifecycle of a task
/// - Coordinating between different agent roles
/// - Handling module requests and caching results
/// - Managing the conversation flow
/// - Exporting results
#[derive(Debug)]
pub struct TaskManager {
    /// The task being managed
    task: Task,
    /// Workflow defining the execution steps and transitions
    pub workflow: Workflow,
    /// Manager for coordinating module access and execution
    pub modules_manager: ModulesManager,
    /// Configuration for the task execution
    pub config: TaskConfig,
    /// Client for interacting with the language model
    pub llm_client: LlmClient,
    /// Cache storing results of module executions to avoid redundant calls
    pub module_results_cache: HashMap<(String, String, Vec<String>), String>,
    /// Counter tracking the number of revision requests
    pub revision_count: usize,
    /// Storage for vector embeddings used in retrieval
    pub vector_store: InMemoryVectorStore<OpenAIEmbedder>,
}

impl TaskManager {
    /// Creates a new TaskManager instance from the provided configuration.
    ///
    /// # Arguments
    /// * `config` - The task configuration containing all necessary parameters
    ///
    /// # Returns
    /// A new TaskManager instance initialized with the provided configuration
    pub fn new(config: &TaskConfig) -> Self {
        let llm_provider = config
            .parameters
            .llm_provider
            .as_deref()
            .expect("LLM provider is required");
        let llm_model = config
            .parameters
            .llm_model
            .as_deref()
            .expect("LLM model is required");

        let context = Self::process_task_context(config);
        let workflow = Workflow::new(config.workflow.steps.clone());
        let modules_manager = ModulesManager::new(config.modules.clone());
        let embedder_config = config.parameters.embedder.clone().unwrap_or_default();
        let embedder_model = embedder_config
            .model
            .unwrap_or("text-embedding-3-small".to_string());
        let vector_store = InMemoryVectorStore::new(OpenAIEmbedder::new(&embedder_model).unwrap());
        let llm_client =
            LlmClient::new(llm_provider, llm_model).expect("Failed to create LLM client");

        let mut task = Task::new(config.name.clone(), context);
        let system_instructions =
            utils::generate_system_instructions(&config.agents, &modules_manager);
        task.conversation
            .push(ChatMessage::new("system", &system_instructions));

        Self {
            task,
            workflow,
            modules_manager,
            config: config.clone(),
            llm_client,
            module_results_cache: HashMap::new(),
            revision_count: 0,
            vector_store,
        }
    }

    /// Processes the task context from the configuration.
    ///
    /// # Arguments
    /// * `config` - The task configuration containing context items
    ///
    /// # Returns
    /// A TaskContext containing processed file and text inputs
    fn process_task_context(config: &TaskConfig) -> TaskContext {
        let mut ctx = TaskContext::new();

        for item in &config.context {
            match item.kind.as_str() {
                "file" => {
                    if let Some(path) = &item.path {
                        let content = fs::read_to_string(path)
                            .unwrap_or_else(|_| panic!("Failed to read file: {}", path));
                        let alias = item.alias.clone().unwrap_or_else(|| path.clone());
                        ctx.files.push((alias, content));
                    }
                }
                "user_input" => {
                    if let Some(content) = &item.content {
                        ctx.text.push_str(content);
                        ctx.text.push('\n');
                    } else {
                        print!("Kheish |> ");
                        io::stdout().flush().expect("Failed to flush stdout");
                        let mut input = String::new();
                        io::stdin()
                            .read_line(&mut input)
                            .expect("Failed to read user input");
                        ctx.text.push_str(&input);
                    }
                }
                "text" => {
                    if let Some(content) = &item.content {
                        ctx.text.push_str(content);
                        ctx.text.push('\n');
                    }
                }
                kind => {
                    error!("Unknown context kind: {}", kind);
                }
            }
        }

        ctx
    }

    /// Runs the task execution process.
    ///
    /// This method coordinates the execution flow between different agents,
    /// handles module requests, and manages the task state until completion
    /// or failure.
    pub async fn run(&mut self) {
        info!("Starting task '{}'...", self.task.name);
        if let Some(description) = &self.config.description {
            info!("Task description: {}", description);
        }
        if let Some(version) = &self.config.version {
            info!("Task version: {}", version);
        }
        self.task.state = TaskState::InProgress;

        let mut current_role = "proposer".to_string();

        loop {
            debug!("Current role: {}", current_role);
            let agent_outcome = self.execute_role(&current_role).await;

            match agent_outcome {
                AgentOutcome::Failed(reason) => {
                    error!("Task failed at role {} : {}", current_role, reason);
                    self.task.state = TaskState::Failed(reason);
                    break;
                }

                AgentOutcome::ModuleRequest(module_name, action, params) => {
                    info!("Module request: {} {} {:?}", module_name, action, params);
                    let module_cache_key = (module_name.clone(), action.clone(), params.clone());
                    if self.module_results_cache.contains_key(&module_cache_key) {
                        continue;
                    }

                    if let Some(module) = self.modules_manager.get_module(&module_name) {
                        let action_result = module
                            .handle_action(&mut self.vector_store, &action, &params)
                            .await;

                        let execution_message = match &action_result {
                            Ok(result) => {
                                self.module_results_cache
                                    .insert(module_cache_key, result.clone());

                                if result.chars().count() > 35000 {
                                    format!(
                                        "The result from module {} action '{}' is too large to process directly.\n\
                                        Please use the RAG module to index this content first:\n\
                                        1. Use 'rag index' to store the content\n\
                                        2. Then use 'rag search' with relevant keywords to retrieve specific information\n\
                                        \nFirst few characters of content: {}...",
                                        module_name,
                                        action,
                                        &result[..200]
                                    )
                                } else {
                                    format!(
                                        "{}",
                                        result
                                    )
                                }
                            }
                            Err(e) => {
                                error!("Module {} action '{}' failed: {}", module_name, action, e);
                                let err_msg = format!(
                                    "Module {} action '{}' failed: {}",
                                    module_name, action, e
                                );
                                self.task.state = TaskState::Failed(err_msg.clone());
                                format!(
                                    "Result of module {} action '{} {}':\n{}",
                                    module_name,
                                    action,
                                    params.join(" "),
                                    e
                                )
                            }
                        };

                        self.task.conversation.push(ChatMessage::new("user", &execution_message));
                        continue;
                    } else {
                        let err_msg = format!(
                            "Module {} not found. Available modules and their actions: {}",
                            module_name, 
                            self.modules_manager
                                .modules
                                .iter()
                                .map(|m| format!("{} (actions: {})", m.name(), 
                                    m.get_actions().iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")
                                ))
                                .collect::<Vec<_>>()
                                .join("; ")
                        );

                        self.task
                            .conversation
                            .push(ChatMessage::new("assistant", &err_msg));
                        self.task.state =
                            TaskState::Failed(format!("Module {} not found", module_name));
                        continue;
                    }
                }

                outcome => {
                    let condition = outcome.as_condition();
                    match self.workflow.next_role(&current_role, condition) {
                        Some(next_role) => {
                            if condition == "revision_requested" {
                                self.revision_count += 1;
                            }

                            if next_role == "completed" {
                                self.task.state = TaskState::Completed;
                                info!("Task '{}' completed successfully!", self.task.name);

                                let json_path = format!(
                                    "logs/{}-{}-data.json",
                                    self.task.name,
                                    chrono::Local::now().format("%Y-%m-%d")
                                );

                                if self.config.parameters.export_conversation {
                                    info!("Exporting conversation to {}", json_path);
                                    if serde_json::to_string_pretty(&self.task.conversation)
                                        .map_err(|e| error!("JSON serialization failed: {}", e))
                                        .and_then(|json| {
                                            std::fs::write(&json_path, json)
                                                .map_err(|e| error!("File write failed: {}", e))
                                        })
                                        .is_ok()
                                    {
                                        info!("Exported conversation to {}", json_path);
                                    }
                                }

                                break;
                            }

                            current_role = next_role;
                        }
                        None => {
                            error!(
                                "No next step found for role {} and condition {}",
                                current_role, condition
                            );
                            self.task.state =
                                TaskState::Failed("No matching workflow step".to_string());
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Executes a specific role in the task workflow.
    ///
    /// # Arguments
    /// * `role` - The role to execute (proposer, reviewer, validator, or formatter)
    ///
    /// # Returns
    /// The outcome of the agent's execution
    async fn execute_role(&mut self, role: &str) -> AgentOutcome {
        info!("Executing role {}", role);

        let (agent_config, default_prompt) = match role {
            "proposer" => (&self.config.agents.proposer, PROPOSER_USER_PROMPT),
            "reviewer" => (&self.config.agents.reviewer, REVIEWER_USER_PROMPT),
            "validator" => (&self.config.agents.validator, VALIDATOR_USER_PROMPT),
            "formatter" => (&self.config.agents.formatter, FORMATTER_USER_PROMPT),
            _ => return AgentOutcome::Failed(format!("Unknown role {}", role)),
        };

        let user_prompt = agent_config
            .user_prompt
            .as_deref()
            .unwrap_or(default_prompt);

        match role {
            "proposer" => {
                ProposerAgent {
                    llm_client: &self.llm_client,
                    user_prompt,
                }
                .execute_step(&mut self.task)
                .await
            }
            "reviewer" => {
                ReviewerAgent {
                    llm_client: &self.llm_client,
                    user_prompt,
                }
                .execute_step(&mut self.task)
                .await
            }
            "validator" => {
                ValidatorAgent {
                    llm_client: &self.llm_client,
                    user_prompt,
                }
                .execute_step(&mut self.task)
                .await
            }
            "formatter" => {
                FormatterAgent {
                    llm_client: &self.llm_client,
                    user_prompt,
                    output_format: self.config.output.format.as_str(),
                    output_file: self.config.output.file.as_str(),
                }
                .execute_step(&mut self.task)
                .await
            }
            _ => unreachable!(),
        }
    }
}
