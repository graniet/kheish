use crate::schema::{task_outputs, tasks};
use diesel::{AsChangeset, Identifiable, Insertable, Queryable};
use serde::{Deserialize, Serialize};

/// Represents a task in the database
#[derive(
    Debug, Clone, Serialize, Deserialize, Queryable, Identifiable, AsChangeset, Insertable,
)]
#[diesel(table_name = tasks)]
pub struct Task {
    /// Optional unique identifier for the task
    pub id: Option<String>,
    /// Task identifier used for tracking
    pub task_id: String,
    /// Optional name of the task
    pub name: Option<String>,
    /// Optional description of what the task does
    pub description: Option<String>,
    /// Current state of the task
    pub state: String,
    /// Optional JSON serialized task context
    pub context: Option<String>,
    /// Optional JSON serialized history of proposals
    pub proposal_history: Option<String>,
    /// Optional JSON serialized current proposal
    pub current_proposal: Option<String>,
    /// Optional JSON serialized feedback history
    pub feedback_history: Option<String>,
    /// Optional JSON serialized module execution history
    pub module_execution_history: Option<String>,
    /// Optional JSON serialized conversation history
    pub conversation: Option<String>,
    /// Optional JSON serialized task configuration
    pub config: Option<String>,
    /// Timestamp when the task was created
    pub created_at: String,
    /// Timestamp when the task was last updated
    pub updated_at: String,
}

/// Represents the output of a task in the database
#[derive(Debug, Clone, Serialize, Deserialize, Queryable, Identifiable, AsChangeset)]
#[diesel(table_name = task_outputs)]
pub struct TaskOutput {
    /// Optional unique identifier for the task output
    pub id: Option<String>,
    /// Reference to the associated task
    pub task_id: String,
    /// The output data
    pub output: String,
    /// Timestamp when the output was created
    pub created_at: String,
    /// Timestamp when the output was last updated
    pub updated_at: String,
}
