mod context;
mod message;
mod run;
mod task;
mod utils;

use crate::{
    config::TaskConfig,
    core::TaskWorker,
    core::{
        rag::InMemoryVectorStore, task::Task, task_generation::generate_task_config_from_user,
        workflow::Workflow,
    },
    db::Database,
    event::Event,
    llm::{ChatMessage, LlmClient, OpenAIEmbedder},
    modules::ModulesManager,
    utils::generate_system_instructions,
};
use colored::*;
pub use context::process_task_context;
use dialoguer::{theme::ColorfulTheme, Input};
use indicatif::ProgressBar;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

const DEFAULT_EMBEDDER_MODEL: &str = "text-embedding-3-small";

/// Main task manager struct responsible for coordinating task execution and agent interactions
#[derive(Debug)]
pub struct TaskManager {
    /// Progress spinner for UI feedback
    pub spinner: ProgressBar,
    /// Whether the task manager is running without a predefined task
    pub without_task: bool,
    /// Channel receiver for task manager events
    pub self_rx: UnboundedReceiver<Event>,
    /// Channel sender for task manager events
    pub self_tx: UnboundedSender<Event>,
    // Database
    pub database: Database,
    /// LLM client for task generation
    pub llm_client: LlmClient,
    /// API enabled
    pub api_enabled: bool,
}

impl TaskManager {
    /// Creates a new TaskManager instance without a predefined task
    pub async fn new_without_task_creation(api_enabled: bool) -> (Self, Vec<TaskWorker>) {
        Self::display_welcome_message();

        let user_input = Self::get_user_input("📝 Your task");
        Self::clear_screen();

        Self::display_processing_message();
        std::thread::sleep(std::time::Duration::from_secs(1));
        Self::clear_screen();

        let llm_provider = Self::get_user_input("LLM provider (e.g openai)");
        let llm_model = Self::get_user_input("LLM model (e.g gpt-4)");

        let llm_client =
            LlmClient::new(&llm_provider, &llm_model).expect("Failed to create LLM client");

        let config = generate_task_config_from_user(&user_input, &llm_client).await;
        Self::from_config(&config, api_enabled)
    }

    /// Creates a new TaskManager instance without any predefined tasks
    ///
    /// Initializes a TaskManager with all available modules loaded but no active tasks.
    /// Sets up communication channels and creates an empty database connection.
    ///
    /// # Returns
    /// * `Self` - New TaskManager instance with default configuration
    pub async fn new_without_task(
        llm_provider: &str,
        llm_model: &str,
        api_enabled: bool,
    ) -> (Self, Vec<TaskWorker>) {
        let without_task = true;
        let (self_tx, self_rx) = tokio::sync::mpsc::unbounded_channel();
        let llm_client =
            LlmClient::new(llm_provider, llm_model).expect("Failed to create LLM client");

        (
            Self {
                spinner: ProgressBar::new_spinner(),
                self_rx,
                self_tx,
                database: Database::new("kheish.db"),
                without_task,
                llm_client,
                api_enabled,
            },
            Vec::new(),
        )
    }

    /// Displays welcome message to the user
    fn display_welcome_message() {
        println!("{}", "\n🤖 Welcome to the Task Manager!".bold().cyan());
        println!(
            "{}",
            "Please briefly describe what you would like to do.".yellow()
        );
    }

    /// Gets task description input from user
    fn get_user_input(ask: &str) -> String {
        Input::with_theme(&ColorfulTheme::default())
            .with_prompt(ask)
            .interact_text()
            .expect("Failed to read input")
    }

    /// Clears the terminal screen
    fn clear_screen() {
        print!("\x1B[2J\x1B[1;1H");
    }

    /// Displays processing message to user
    fn display_processing_message() {
        println!(
            "{}",
            "✨ Excellent! Starting to analyze your request...".green()
        );
    }

    /// Creates a new TaskManager instance from a config
    pub fn new(config: &TaskConfig, api_enabled: bool) -> (Self, Vec<TaskWorker>) {
        Self::from_config(config, api_enabled)
    }

    /// Internal constructor to create TaskManager from config
    fn from_config(config: &TaskConfig, api_enabled: bool) -> (Self, Vec<TaskWorker>) {
        let vector_store = Self::initialize_vector_store(config);
        let task = Self::create_task(config);
        let (self_tx, self_rx) = tokio::sync::mpsc::unbounded_channel();
        let database =
            Database::new(&std::env::var("DATABASE_PATH").unwrap_or("kheish.db".to_string()));
        let without_task = false;
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
        let llm_client =
            LlmClient::new(llm_provider, llm_model).expect("Failed to create LLM client");
        let task_worker = TaskWorker::new(
            task.task_id.clone(),
            task,
            Workflow::new(config.workflow.steps.clone()),
            config.clone(),
            vector_store,
            self_tx.clone(),
        );
        let workers = vec![task_worker];

        let mut manager = Self {
            spinner: ProgressBar::new_spinner(),

            self_rx,
            self_tx,

            database,
            without_task,
            llm_client,
            api_enabled,
        };

        manager.init_spinner();
        (manager, workers)
    }

    /// Extracts LLM provider and model from config
    pub fn extract_llm_config(config: &TaskConfig) -> (&str, &str) {
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
        (llm_provider, llm_model)
    }

    /// Initializes the vector store with embedder configuration
    fn initialize_vector_store(config: &TaskConfig) -> InMemoryVectorStore<OpenAIEmbedder> {
        let embedder_config = config.parameters.embedder.clone().unwrap_or_default();
        let embedder_model = embedder_config
            .model
            .unwrap_or(DEFAULT_EMBEDDER_MODEL.to_string());
        InMemoryVectorStore::new(OpenAIEmbedder::new(&embedder_model).unwrap())
    }

    /// Creates a new task with context and system instructions
    fn create_task(config: &TaskConfig) -> Task {
        let task_id = uuid::Uuid::new_v4().to_string();
        let context = process_task_context(config);
        let mut task = Task::new(
            task_id,
            config.name.clone(),
            config.description.clone().unwrap_or("".to_string()),
            context,
            config.interval.clone(),
        );

        let system_instructions = generate_system_instructions(
            &config.agents,
            &ModulesManager::new(config.modules.clone()),
        );
        task.conversation
            .push(ChatMessage::new("system", &system_instructions));

        task
    }
}
