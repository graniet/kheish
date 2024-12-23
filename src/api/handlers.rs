use crate::api::errors::{api_error, ApiError};
use crate::core::TaskState;
/// Database and task management imports
use crate::db::Database;
use crate::db::TaskRepository;
use axum::http::StatusCode;
use axum::{
    extract::{Extension, Path},
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Represents the request payload for creating a new task
#[derive(Deserialize)]
pub struct CreateTaskRequest {
    pub prompt: String,
}

/// Represents the response payload after successfully creating a task
#[derive(Serialize)]
pub struct CreateTaskResponse {
    pub status: String,
    pub task_id: String,
    pub created_at: String,
}

/// Contains task information returned by the API endpoints
#[derive(Serialize)]
pub struct TaskInfo {
    pub id: String,
    pub task_id: String,
    pub name: String,
    pub description: String,
    pub state: String,
    pub outputs: Vec<TaskOutputDTO>,
}

/// Data transfer object representing a task's output information
#[derive(Serialize)]
pub struct TaskOutputDTO {
    pub id: String,
    pub output: String,
    pub created_at: String,
}

/// Creates a new task in the system
///
/// # Arguments
/// * `database` - Database connection pool
/// * `payload` - JSON payload containing the task creation request
///
/// # Returns
/// * `Result<Json<CreateTaskResponse>, ApiError>` - Task creation response or error
#[axum::debug_handler]
pub async fn create_task(
    Extension(database): Extension<Database>,
    Json(payload): Json<CreateTaskRequest>,
) -> Result<Json<CreateTaskResponse>, ApiError> {
    let mut conn = database.get_conn();
    let mut repo = TaskRepository::new(&mut conn);

    let logical_task_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    let db_id = repo
        .insert_task(
            logical_task_id.clone(),
            Some(payload.prompt.clone()),
            Some(payload.prompt.clone()),
            TaskState::New.to_string(),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    Ok(Json(CreateTaskResponse {
        status: "new".to_string(),
        task_id: db_id,
        created_at: now,
    }))
}

/// Retrieves task information by its ID
///
/// # Arguments
/// * `id` - Task ID to look up
/// * `database` - Database connection pool
///
/// # Returns
/// * `Result<Json<TaskInfo>, ApiError>` - Task information or error
#[axum::debug_handler]
pub async fn get_task(
    Path(id): Path<String>,
    Extension(database): Extension<Database>,
) -> Result<Json<TaskInfo>, ApiError> {
    let mut conn = database.get_conn();
    let mut repo = TaskRepository::new(&mut conn);

    let task = match repo.get_task_by_db_id(&id) {
        Ok(t) => t,
        Err(_) => {
            return Err(api_error(StatusCode::NOT_FOUND, "Task not found"));
        }
    };

    let outputs = repo
        .get_task_outputs(&task.task_id)
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    let task_outputs: Vec<TaskOutputDTO> = outputs
        .into_iter()
        .map(|o| TaskOutputDTO {
            id: o.id.unwrap_or_default(),
            output: o.output,
            created_at: o.created_at,
        })
        .collect();

    Ok(Json(TaskInfo {
        id: task.id.unwrap_or("".to_string()),
        task_id: task.task_id,
        name: task.name.unwrap_or_default(),
        description: task.description.unwrap_or_default(),
        state: task.state,
        outputs: task_outputs,
    }))
}

/// Retrieves all outputs associated with a task ID
///
/// # Arguments
/// * `id` - Task ID to get outputs for
/// * `database` - Database connection pool
///
/// # Returns
/// * `Result<Json<Vec<TaskOutput>>, ApiError>` - List of task outputs or error
#[axum::debug_handler]
pub async fn get_task_outputs(
    Path(id): Path<String>,
    Extension(database): Extension<Database>,
) -> Result<Json<Vec<crate::db::TaskOutput>>, ApiError> {
    let mut conn = database.get_conn();
    let mut repo = TaskRepository::new(&mut conn);
    let outputs = repo
        .get_task_outputs(&id)
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(Json(outputs))
}
