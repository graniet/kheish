use super::context::process_task_context;
use crate::{
    config::TaskConfig,
    core::{
        rag::InMemoryVectorStore, task::Task, task_generation::generate_task_config_from_user,
        workflow::Workflow,
    },
    llm::{ChatMessage, LlmClient, OpenAIEmbedder},
    modules::ModulesManager,
    utils,
};
use colored::*;
use dialoguer::{theme::ColorfulTheme, Input};
use indicatif::ProgressBar;
use std::collections::HashMap;

const DEFAULT_MAX_RETRIES: usize = 3;
const DEFAULT_EMBEDDER_MODEL: &str = "text-embedding-3-small";

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

    fn display_welcome_message() {
        println!("{}", "\n🤖 Welcome to the Task Manager!".bold().cyan());
        println!(
            "{}",
            "Please briefly describe what you would like to do.".yellow()
        );
    }

    fn get_user_input() -> String {
        Input::with_theme(&ColorfulTheme::default())
            .with_prompt("📝 Your task")
            .interact_text()
            .expect("Failed to read input")
    }

    fn clear_screen() {
        print!("\x1B[2J\x1B[1;1H");
    }

    fn display_processing_message() {
        println!(
            "{}",
            "✨ Excellent! Starting to analyze your request...".green()
        );
    }

    pub fn new(config: &TaskConfig) -> Self {
        Self::from_config(config)
    }

    fn from_config(config: &TaskConfig) -> Self {
        let (llm_provider, llm_model) = Self::extract_llm_config(config);
        let vector_store = Self::initialize_vector_store(config);
        let llm_client = Self::create_llm_client(llm_provider, llm_model);

        let task = Self::create_task(config);
        let modules_manager = ModulesManager::new(config.modules.clone());

        let mut manager = Self {
            task,
            workflow: Workflow::new(config.workflow.steps.clone()),
            modules_manager,
            config: config.clone(),
            llm_client,
            module_results_cache: HashMap::new(),
            revision_count: 0,
            vector_store,
            retry_count: 0,
            max_retries: config.parameters.max_retries.unwrap_or(DEFAULT_MAX_RETRIES),
            spinner: ProgressBar::new_spinner(),
        };

        manager.init_spinner();
        manager
    }

    fn extract_llm_config(config: &TaskConfig) -> (&str, &str) {
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

    fn initialize_vector_store(config: &TaskConfig) -> InMemoryVectorStore<OpenAIEmbedder> {
        let embedder_config = config.parameters.embedder.clone().unwrap_or_default();
        let embedder_model = embedder_config
            .model
            .unwrap_or(DEFAULT_EMBEDDER_MODEL.to_string());
        InMemoryVectorStore::new(OpenAIEmbedder::new(&embedder_model).unwrap())
    }

    fn create_llm_client(provider: &str, model: &str) -> LlmClient {
        LlmClient::new(provider, model).expect("Failed to create LLM client")
    }

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
}
