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
use core::TaskManager;
use tracing::warn;

/// Main entry point that initializes and runs the application
#[tokio::main]
async fn main() {
    let cli = cli::Cli::try_parse().expect("Failed to parse CLI arguments");
    utils::init_logging(&cli.logging_level);

    if let Err(e) = dotenvy::dotenv() {
        warn!("Failed to load .env file: {}", e);
    }

    let mut task_manager = match cli.task_config {
        None => TaskManager::new_without_task().await,
        Some(task_path) => {
            let config =
                config::load_task_config(&task_path).expect("Failed to parse task configuration");
            TaskManager::new(&config)
        }
    };

    task_manager.run().await;
}
