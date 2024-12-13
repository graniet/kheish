use clap::Parser;

/// Command line interface for the application
#[derive(Parser)]
pub struct Cli {
    /// Path to the configuration file containing task definitions and settings
    #[arg(short, long)]
    pub task_config: String,

    /// Sets the logging verbosity level for the application
    /// Possible values: "error", "warn", "info", "debug", "trace"
    /// Default: "info"
    #[arg(long, default_value_t = String::from("info"))]
    pub logging_level: String,
}
