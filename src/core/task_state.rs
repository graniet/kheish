/// Represents the current state of a task
#[derive(Debug, Clone)]
pub enum TaskState {
    /// Task has been created but not yet started
    New,
    /// Task is currently being processed
    InProgress,
    /// Task failed with an error message
    #[allow(unused)]
    Failed(String),
}
