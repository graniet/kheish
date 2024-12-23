mod parser;
use serde::{Deserialize, Serialize};

pub use parser::load_task_config;

/// Main configuration structure for a task
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TaskConfig {
    /// Name of the task
    pub name: String,
    /// Optional description of what the task does
    #[serde(default)]
    pub description: Option<String>,
    /// Optional version of the task configuration
    #[serde(default)]
    pub version: Option<String>,
    /// List of context items providing input data
    #[serde(default)]
    pub context: Vec<ContextItem>,
    /// Configuration for the different agent roles
    #[serde(default)]
    pub agents: AgentsConfig,
    /// List of module configurations
    #[serde(default)]
    pub modules: Vec<ModuleConfig>,
    /// Workflow configuration defining the execution steps
    #[serde(default)]
    pub workflow: WorkflowConfig,
    /// Global parameters for task execution
    #[serde(default)]
    pub parameters: ParametersConfig,
    /// Output configuration
    #[serde(default)]
    pub output: OutputConfig,
}

/// Configuration for the different agent roles
#[derive(Debug, Deserialize, Default, Clone, Serialize)]
pub struct AgentsConfig {
    /// Configuration for proposer agents
    #[serde(default)]
    pub proposer: AgentConfig,
    /// Configuration for reviewer agents
    #[serde(default)]
    pub reviewer: AgentConfig,
    /// Configuration for validator agents
    #[serde(default)]
    pub validator: AgentConfig,
    /// Configuration for formatter agents
    #[serde(default)]
    pub formatter: AgentConfig,
}

/// Configuration for a specific agent role
#[derive(Debug, Deserialize, Default, Clone, Serialize)]
pub struct AgentConfig {
    /// Optional strategy name for agent behavior
    #[allow(unused)]
    #[serde(default)]
    pub strategy: Option<String>,
    /// Optional system prompt for the agent
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Optional user prompt for the agent
    #[serde(default)]
    pub user_prompt: Option<String>,
}

/// Represents a single context item providing input data
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct ContextItem {
    /// Type of the context item
    pub kind: String,
    /// Optional file path for the context
    #[serde(default)]
    pub path: Option<String>,
    /// Optional inline content
    #[serde(default)]
    pub content: Option<String>,
    /// Optional alias name for the context
    #[serde(default)]
    pub alias: Option<String>,
}

/// Configuration for a module
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct ModuleConfig {
    /// Name of the module
    pub name: String,
    /// Optional version of the module
    #[serde(default)]
    pub version: Option<String>,
    /// Optional module-specific configuration
    #[serde(default)]
    pub config: Option<toml::Value>,
}

/// Configuration for the workflow execution
#[derive(Debug, Deserialize, Default, Clone, Serialize)]
pub struct WorkflowConfig {
    /// List of workflow steps
    #[serde(default)]
    pub steps: Vec<WorkflowStep>,
}

/// Represents a single step in the workflow
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct WorkflowStep {
    /// Source state
    pub from: String,
    /// Target state
    pub to: String,
    /// Condition for the transition
    pub condition: String,
}

/// Global parameters for task execution
#[derive(Debug, Deserialize, Default, Clone, Serialize)]
pub struct ParametersConfig {
    /// Name/identifier of the LLM model to use
    #[serde(default)]
    pub llm_model: Option<String>,
    /// Name/identifier of the LLM provider to use
    #[serde(default)]
    pub llm_provider: Option<String>,
    /// Whether to export the conversation to a JSON file
    #[serde(default)]
    pub export_conversation: bool,
    /// Embedder configuration
    #[serde(default)]
    pub embedder: Option<EmbedderConfig>,
    /// Whether to collect feedback after completion
    #[serde(default)]
    pub post_completion_feedback: bool,
    /// Maximum number of retries allowed
    #[serde(default)]
    pub max_retries: Option<usize>,
}

/// Output configuration
#[derive(Debug, Deserialize, Default, Clone, Serialize)]
pub struct OutputConfig {
    /// Format of the output
    pub format: String,
    /// File path for the output
    pub file: String,
}

/// Embedder configuration
#[derive(Debug, Deserialize, Clone, Default, Serialize)]
pub struct EmbedderConfig {
    /// Name/identifier of the embedder
    pub model: Option<String>,
}
