use clap::Parser;

/// Command line interface for the application
#[derive(Parser)]
pub struct Cli {
    /// Path to the configuration file containing task definitions and settings
    #[arg(short, long)]
    pub task_config: Option<String>,

    /// Sets the logging verbosity level for the application
    /// Possible values: "error", "warn", "info", "debug", "trace"
    /// Default: "info"
    #[arg(long, default_value_t = String::from("info"))]
    pub logging_level: String,

    /// Sets the port for the API server
    #[arg(long, default_value_t = 3000)]
    pub api_port: u16,

    /// Enables the API server
    #[arg(long, default_value_t = false)]
    pub api_enabled: bool,

    /// Creates a new task
    #[arg(long, default_value_t = false)]
    pub new_task: bool,

    /// LLM provider (e.g openai)
    #[arg(long)]
    pub llm_provider: Option<String>,

    /// LLM model (e.g gpt-4)
    #[arg(long)]
    pub llm_model: Option<String>,
}
