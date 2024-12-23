//! API routes configuration module

use crate::api::handlers::{create_task, get_task, get_task_outputs};
use crate::db::Database;
use axum::{
    routing::{get, post},
    Extension, Router,
};

/// Creates and configures the API router with all routes
///
/// # Arguments
/// * `database` - Database connection pool to be shared across handlers
///
/// # Returns
/// * `Router` - Configured router with all API endpoints and middleware
pub fn app(database: Database) -> Router {
    Router::new()
        .route("/tasks", post(create_task))
        .route("/tasks/:id", get(get_task))
        .route("/tasks/:id/outputs", get(get_task_outputs))
        .layer(Extension(database))
}
