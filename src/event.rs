use crate::core::TaskState;
use crate::{agents::AgentOutcome, core::Task};
use serde_json::Value;

/// Represents different events that can occur in the system
#[derive(Debug, Clone)]
pub enum Event {
    /// A new request is created with a role and task
    NewRequest(String, Task),

    /// An agent responds with a role, outcome and task
    AgentResponse(String, AgentOutcome, Task),

    /// A new message is added to a task
    NewMessage(String, String),

    /// A task is marked as completed
    TaskCompleted(String),

    /// The state of a task is updated
    TaskStateUpdated(String, TaskState),

    /// New output data is added to a task
    NewOutput(String, Value),

    /// A new task is created
    CreateTask(Task),
}
