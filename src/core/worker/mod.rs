/// Module for handling task execution logic
mod execution;
/// Module for handling failed task outcomes
mod failed_outcome;
/// Module for handling module requests
mod module_request;
/// Module for handling standard task outcomes
mod standard_outcome;
/// Module for handling task completion
mod task_completion;

use crate::{
    agents::{FormatterAgent, ProposerAgent, ReviewerAgent, ValidatorAgent},
    config::TaskConfig,
    core::{rag::InMemoryVectorStore, task::Task, workflow::Workflow},
    event::Event,
    llm::ChatMessage,
    llm::OpenAIEmbedder,
    modules::ModulesManager,
    utils,
};
use std::collections::HashMap;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{error, info};
/// Default maximum number of retries for failed tasks
const DEFAULT_MAX_RETRIES: usize = 3;

/// Task worker struct responsible for managing and executing individual tasks
#[derive(Debug)]
pub struct TaskWorker {
    /// Unique identifier for the task
    pub task_id: String,
    /// The current task being managed
    pub task: Task,
    /// Workflow defining the execution steps
    pub workflow: Workflow,
    /// Manager for handling task modules
    pub modules_manager: ModulesManager,
    /// Cache for storing module execution results
    pub module_results_cache: HashMap<(String, String, Vec<String>), String>,
    /// Configuration for the task
    pub config: TaskConfig,
    /// Counter for tracking task revisions
    pub revision_count: usize,
    /// Vector store for RAG functionality
    pub vector_store: InMemoryVectorStore<OpenAIEmbedder>,
    /// Counter for tracking retry attempts
    pub retry_count: usize,
    /// Maximum number of retries allowed
    pub max_retries: usize,
    /// Channel sender for proposer agent
    pub proposer_tx: Option<UnboundedSender<Event>>,
    /// Channel sender for reviewer agent
    pub reviewer_tx: Option<UnboundedSender<Event>>,
    /// Channel sender for validator agent
    pub validator_tx: Option<UnboundedSender<Event>>,
    /// Channel sender for formatter agent
    pub formatter_tx: Option<UnboundedSender<Event>>,
    /// Channel sender for manager
    pub manager_tx: Option<UnboundedSender<Event>>,
    /// Channel sender for self
    pub self_tx: UnboundedSender<Event>,
    /// Channel receiver for self
    pub self_rx: UnboundedReceiver<Event>,
}

impl TaskWorker {
    /// Creates a new TaskWorker instance
    ///
    /// # Arguments
    /// * `task_id` - Unique identifier for the task
    /// * `task` - The task to be managed
    /// * `workflow` - Workflow defining execution steps
    /// * `config` - Task configuration
    /// * `vector_store` - Vector store for RAG functionality
    /// * `manager_tx` - Channel sender for the task manager
    pub fn new(
        task_id: String,
        mut task: Task,
        workflow: Workflow,
        config: TaskConfig,
        vector_store: InMemoryVectorStore<OpenAIEmbedder>,
        manager_tx: UnboundedSender<Event>,
    ) -> Self {
        let max_retries = config.parameters.max_retries.unwrap_or(DEFAULT_MAX_RETRIES);
        let (self_tx, self_rx) = unbounded_channel();
        let modules_manager = ModulesManager::new(config.modules.clone());
        let system_prompt = utils::generate_system_instructions(&config.agents, &modules_manager);

        if !task.conversation.iter().any(|msg| msg.role == "system") {
            info!("Adding system prompt to task conversation");
            task.conversation
                .push(ChatMessage::new("system", &system_prompt));
        }

        Self {
            task_id,
            task,
            workflow,
            modules_manager,
            module_results_cache: HashMap::new(),
            config,
            vector_store,
            retry_count: 0,
            max_retries,
            revision_count: 0,
            proposer_tx: None,
            reviewer_tx: None,
            validator_tx: None,
            formatter_tx: None,
            manager_tx: Some(manager_tx),
            self_tx,
            self_rx,
        }
    }

    /// Sets up communication channels for all agents
    ///
    /// # Arguments
    /// * `proposer_tx` - Channel sender for proposer agent
    /// * `reviewer_tx` - Channel sender for reviewer agent
    /// * `validator_tx` - Channel sender for validator agent
    /// * `formatter_tx` - Channel sender for formatter agent
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
    ///
    /// # Arguments
    /// * `role` - The role of the agent ("proposer", "reviewer", "validator", or "formatter")
    ///
    /// # Returns
    /// * `Option<UnboundedSender<Event>>` - The channel sender if the role exists
    pub fn get_role_tx(&self, role: &str) -> Option<UnboundedSender<Event>> {
        match role {
            "proposer" => self.proposer_tx.clone(),
            "reviewer" => self.reviewer_tx.clone(),
            "validator" => self.validator_tx.clone(),
            "formatter" => self.formatter_tx.clone(),
            _ => None,
        }
    }

    /// Gets the channel sender for the task manager
    ///
    /// # Returns
    /// * `Option<UnboundedSender<Event>>` - The channel sender for the manager
    pub fn get_manager_tx(&self) -> Option<UnboundedSender<Event>> {
        self.manager_tx.clone()
    }

    /// Extracts LLM configuration from task config
    ///
    /// # Arguments
    /// * `config` - The task configuration
    ///
    /// # Returns
    /// * `(&str, &str)` - Tuple of (llm_provider, llm_model)
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

    /// Runs the task worker, managing the lifecycle of the task
    pub async fn run(mut self) {
        info!("Starting task {}, name: {}", self.task_id, self.task.name);
        let (llm_provider, llm_model) = Self::extract_llm_config(&self.config);
        let output_format = self.config.output.format.clone();
        let output_file = self.config.output.file.clone();

        let proposer_config = self.config.agents.proposer.clone();
        let (proposer, proposer_tx) = ProposerAgent::new(proposer_config, llm_provider, llm_model);

        let reviewer_config = self.config.agents.reviewer.clone();
        let (reviewer, reviewer_tx) = ReviewerAgent::new(reviewer_config, llm_provider, llm_model);

        let validator_config = self.config.agents.validator.clone();
        let (validator, validator_tx) =
            ValidatorAgent::new(validator_config, llm_provider, llm_model);

        let formatter_config = self.config.agents.formatter.clone();
        let (formatter, formatter_tx) = FormatterAgent::new(
            formatter_config,
            llm_provider,
            llm_model,
            output_format,
            output_file,
        );

        self.set_agent_channels(proposer_tx, reviewer_tx, validator_tx, formatter_tx);

        tokio::spawn(proposer.run_loop(self.self_tx.clone()));
        tokio::spawn(reviewer.run_loop(self.self_tx.clone()));
        tokio::spawn(validator.run_loop(self.self_tx.clone()));
        tokio::spawn(formatter.run_loop(self.self_tx.clone()));

        self.execute_role("proposer", self.task.clone()).await;

        if let Some(manager_tx) = self.manager_tx.clone() {
            if let Err(e) = manager_tx.send(Event::CreateTask(self.task.clone())) {
                error!("Failed to send task state updated event: {}", e);
                return;
            }
        }

        loop {
            while let Some(msg) = self.self_rx.recv().await {
                match msg {
                    Event::AgentResponse(role, outcome, task) => {
                        self.handle_agent_response(role, outcome, task).await;
                    }
                    Event::TaskCompleted(task_id) => {
                        if task_id == self.task_id {
                            return;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
