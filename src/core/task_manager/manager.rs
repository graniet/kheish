/// Core module for task management and coordination.
use super::context::process_task_context;
use crate::{
    config::TaskConfig,
    core::{
        rag::InMemoryVectorStore, task::Task, task_generation::generate_task_config_from_user,
        workflow::Workflow,
    },
    event::Event,
    llm::{ChatMessage, LlmClient, OpenAIEmbedder},
    modules::ModulesManager,
    utils,
};
use colored::*;
use dialoguer::{theme::ColorfulTheme, Input};
use indicatif::ProgressBar;
use std::collections::HashMap;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

const DEFAULT_MAX_RETRIES: usize = 3;
const DEFAULT_EMBEDDER_MODEL: &str = "text-embedding-3-small";

/// Main task manager struct responsible for coordinating task execution and agent interactions
#[derive(Debug)]
pub struct TaskManager {
    /// The current task being managed
    pub task: Task,
    /// Workflow defining the execution steps
    pub workflow: Workflow,
    /// Manager for available modules
    pub modules_manager: ModulesManager,
    /// Task configuration
    pub config: TaskConfig,
    /// Cache for module execution results
    pub module_results_cache: HashMap<(String, String, Vec<String>), String>,
    /// Counter for task revisions
    pub revision_count: usize,
    /// Vector store for RAG functionality
    pub vector_store: InMemoryVectorStore<OpenAIEmbedder>,
    /// Counter for retry attempts
    pub retry_count: usize,
    /// Maximum number of retries allowed
    pub max_retries: usize,
    /// Progress spinner for UI feedback
    pub spinner: ProgressBar,

    /// Channel sender for proposer agent
    proposer_tx: Option<UnboundedSender<Event>>,
    /// Channel sender for reviewer agent
    reviewer_tx: Option<UnboundedSender<Event>>,
    /// Channel sender for validator agent
    validator_tx: Option<UnboundedSender<Event>>,
    /// Channel sender for formatter agent
    formatter_tx: Option<UnboundedSender<Event>>,

    /// Channel receiver for task manager events
    pub self_rx: UnboundedReceiver<Event>,
    /// Channel sender for task manager events
    pub self_tx: UnboundedSender<Event>,
}

impl TaskManager {
    /// Creates a new TaskManager instance without a predefined task
    pub async fn new_without_task() -> Self {
        Self::display_welcome_message();

        let user_input = Self::get_user_input();
        Self::clear_screen();

        Self::display_processing_message();
        std::thread::sleep(std::time::Duration::from_secs(1));
        Self::clear_screen();

        let llm_client = LlmClient::new("openai", "gpt-4").expect("Failed to create LLM client");

        let config = generate_task_config_from_user(&user_input, &llm_client).await;
        Self::from_config(&config)
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
    fn get_user_input() -> String {
        Input::with_theme(&ColorfulTheme::default())
            .with_prompt("📝 Your task")
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
    pub fn new(config: &TaskConfig) -> Self {
        Self::from_config(config)
    }

    /// Internal constructor to create TaskManager from config
    fn from_config(config: &TaskConfig) -> Self {
        let vector_store = Self::initialize_vector_store(config);
        let task = Self::create_task(config);
        let modules_manager = ModulesManager::new(config.modules.clone());
        let (self_tx, self_rx) = tokio::sync::mpsc::unbounded_channel();

        let mut manager = Self {
            task,
            workflow: Workflow::new(config.workflow.steps.clone()),
            modules_manager,
            config: config.clone(),
            module_results_cache: HashMap::new(),
            revision_count: 0,
            vector_store,
            retry_count: 0,
            max_retries: config.parameters.max_retries.unwrap_or(DEFAULT_MAX_RETRIES),
            spinner: ProgressBar::new_spinner(),

            proposer_tx: None,
            reviewer_tx: None,
            validator_tx: None,
            formatter_tx: None,

            self_rx,
            self_tx,
        };

        manager.init_spinner();
        manager
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
        let context = process_task_context(config);
        let mut task = Task::new(config.name.clone(), context);

        let system_instructions = utils::generate_system_instructions(
            &config.agents,
            &ModulesManager::new(config.modules.clone()),
        );
        task.conversation
            .push(ChatMessage::new("system", &system_instructions));

        task
    }

    /// Sets up communication channels for all agents
    pub fn set_agent_channels(
        &mut self,
        proposer_tx: UnboundedSender<Event>,
        reviewer_tx: UnboundedSender<Event>,
        validator_tx: UnboundedSender<Event>,
        formatter_tx: UnboundedSender<Event>,
    ) {
        self.proposer_tx = Some(proposer_tx);
        self.reviewer_tx = Some(reviewer_tx);
        self.validator_tx = Some(validator_tx);
        self.formatter_tx = Some(formatter_tx);
    }

    /// Gets the channel sender for a specific agent role
    pub fn get_role_tx(&self, role: &str) -> Option<UnboundedSender<Event>> {
        match role {
            "proposer" => self.proposer_tx.clone(),
            "reviewer" => self.reviewer_tx.clone(),
            "validator" => self.validator_tx.clone(),
            "formatter" => self.formatter_tx.clone(),
            _ => None,
        }
    }
}
