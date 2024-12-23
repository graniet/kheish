//! Core module containing the main components of the task execution system
//!
//! This module contains:
//! - Task management and execution
//! - Worker implementation for processing tasks
//! - Workflow definitions and state management
//! - RAG (Retrieval Augmented Generation) functionality

mod manager;
pub mod rag;
mod task;
mod task_context;
mod task_generation;
mod task_state;
mod worker;
mod workflow;

pub use manager::*;
pub use task::*;
pub use task_state::*;
pub use worker::*;
pub use workflow::*;
