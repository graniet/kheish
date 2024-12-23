//! Main entry point for the application.
//!
//! This module initializes logging, loads environment variables and configuration,
//! and starts the task manager to handle execution.
//!
//! The application can be started in different modes:
//! - With a task configuration file
//! - Without a task configuration but with LLM provider/model
//! - In task creation mode
//!
//! It also supports running an optional API server for external interaction.

#![feature(trait_upcasting)]

mod agents;
mod api;
mod cli;
mod config;
mod constants;
mod core;
mod db;
mod errors;
mod event;
mod llm;
mod modules;
mod schema;
mod utils;

use clap::Parser;
use core::TaskManager;
use tracing::{error, info, warn};

/// Main entry point that initializes and runs the application.
///
/// # Initialization steps:
/// 1. Parse CLI arguments
/// 2. Initialize logging system
/// 3. Load environment variables
/// 4. Start API server if enabled
/// 5. Create and run task manager
#[tokio::main]
async fn main() {
    let cli = cli::Cli::try_parse().expect("Failed to parse CLI arguments");
    utils::init_logging(&cli.logging_level, cli.api_enabled);

    if let Err(e) = dotenvy::dotenv() {
        warn!("Failed to load .env file: {}", e);
    }

    if cli.api_enabled {
        info!("Starting API server on port {}", cli.api_port);
        tokio::spawn(async move {
            if let Err(e) = crate::api::server::launch_server(3000).await {
                error!("Failed to start server: {}", e);
            }
        });
    }

    let (mut task_manager, workers) = match cli.task_config {
        None => {
            if cli.new_task {
                TaskManager::new_without_task_creation(cli.api_enabled).await
            } else {
                let llm_provider = cli.llm_provider.expect("LLM provider is required");
                let llm_model = cli.llm_model.expect("LLM model is required");
                TaskManager::new_without_task(&llm_provider, &llm_model, cli.api_enabled).await
            }
        }
        Some(task_path) => {
            let config =
                config::load_task_config(&task_path).expect("Failed to parse task configuration");
            TaskManager::new(&config, cli.api_enabled)
        }
    };

    task_manager.run(workers).await;
}
