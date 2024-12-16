use super::context::process_task_context;
use crate::config::TaskConfig;
use crate::core::rag::InMemoryVectorStore;
use crate::core::task::Task;
use crate::core::task_generation::generate_task_config_from_user;
use crate::core::workflow::Workflow;
use crate::llm::{ChatMessage, LlmClient, OpenAIEmbedder};
use crate::modules::ModulesManager;
use crate::utils;
use colored::*;
use dialoguer::Input;
use indicatif::ProgressBar;
use std::collections::HashMap;

/// Manages the execution of a task by coordinating agents, modules, and workflow.
///
/// Handles:
/// - Task state and context management
/// - Workflow execution and agent coordination
/// - Module request handling and caching
/// - LLM client interactions
/// - Progress display and user feedback
#[derive(Debug)]
pub struct TaskManager {
    pub task: Task,
    pub workflow: Workflow,
    pub modules_manager: ModulesManager,
    pub config: TaskConfig,
    pub llm_client: LlmClient,
    pub module_results_cache: HashMap<(String, String, Vec<String>), String>,
    pub revision_count: usize,
    pub vector_store: InMemoryVectorStore<OpenAIEmbedder>,
    pub retry_count: usize,
    pub max_retries: usize,
    pub spinner: ProgressBar,
}

impl TaskManager {
    pub async fn new_without_task() -> Self {
        println!("{}", "No task configuration provided, please enter a short description of what you want to do.".yellow());

        let user_request: String = Input::new()
            .with_prompt("Kheish")
            .interact_text()
            .expect("Failed to read user input");

        print!("\x1B[2J\x1B[1;1H");

        println!(
            "{}",
            "Great! I'll try to begin a task that matches your description.".green()
        );

        std::thread::sleep(std::time::Duration::from_secs(1));

        print!("\x1B[2J\x1B[1;1H");

        let llm_client = LlmClient::new("openai", "gpt-4o").expect("Failed to create LLM client");
        let config: TaskConfig = generate_task_config_from_user(&user_request, &llm_client).await;

        Self::from_config(&config)
    }

    /// Creates a new TaskManager instance with the provided configuration.
    ///
    /// # Arguments
    /// * `config` - Task configuration containing parameters, workflow steps, and module settings
    ///
    /// # Returns
    /// * `Self` - Configured TaskManager instance ready for task execution
    pub fn new(config: &TaskConfig) -> Self {
        Self::from_config(config)
    }

    /// Internal helper function to avoid code duplication in `new` and `new_without_task`.
    fn from_config(config: &TaskConfig) -> Self {
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

        let context = process_task_context(config);
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

        let spinner = ProgressBar::new_spinner();
        let mut manager = Self {
            task,
            workflow,
            modules_manager,
            config: config.clone(),
            llm_client,
            module_results_cache: HashMap::new(),
            revision_count: 0,
            vector_store,
            retry_count: 0,
            max_retries: config.parameters.max_retries.unwrap_or(3),
            spinner,
        };
        manager.init_spinner();
        manager
    }
}
