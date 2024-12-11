//! Main entry point for the application.
//!
//! This module initializes logging, loads environment variables and configuration,
//! and starts the task manager to handle execution.

#![feature(trait_upcasting)]

mod agents;
mod cli;
mod config;
mod constants;
mod core;
mod llm;
mod modules;
mod utils;

use clap::Parser;
use tracing::warn;

/// Main entry point that initializes and runs the application
#[tokio::main]
async fn main() {
    // Parse command line arguments
    let cli = cli::Cli::try_parse().expect("Failed to parse CLI arguments");
    utils::init_logging(&cli.logging_level, cli.with_file);

    // Load environment variables from .env file
    if let Err(e) = dotenvy::dotenv() {
        warn!("Failed to load .env file: {}", e);
    }

    // Load task configuration
    let config =
        config::load_task_config(&cli.task_config).expect("Failed to parse task configuration");

    // Initialize and run task manager
    let mut task_manager = core::TaskManager::new(&config);
    task_manager.run().await;
}
