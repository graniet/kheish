use std::str::FromStr;

/// Represents the current state of a task in the system
#[derive(Debug, Clone)]
pub enum TaskState {
    /// Initial state when a task is first created but not yet started
    New,
    /// State indicating the task is ready to begin execution
    Ready,
    /// State when the task is being configured with initial parameters
    Configuring,
    /// State when the task has failed, includes error message details
    #[allow(unused)]
    Failed(String),
    /// State when the task has successfully finished execution
    Completed,
    /// State when the task is actively being executed
    InProgress,
    /// State when the task is waiting for a specific interval to pass
    WaitingWakeUp,
}

#[allow(clippy::to_string_trait_impl)]
impl ToString for TaskState {
    /// Converts the TaskState enum to its string representation
    fn to_string(&self) -> String {
        match self {
            TaskState::New => "New".to_string(),
            TaskState::Failed(msg) => format!("Failed: {}", msg),
            TaskState::Completed => "Completed".to_string(),
            TaskState::InProgress => "In Progress".to_string(),
            TaskState::Ready => "Ready".to_string(),
            TaskState::Configuring => "Configuring".to_string(),
            TaskState::WaitingWakeUp => "WaitingWakeUp".to_string(),
        }
    }
}

impl FromStr for TaskState {
    type Err = ();

    /// Attempts to create a TaskState from a string representation
    ///
    /// # Arguments
    /// * `s` - String slice containing the state name
    ///
    /// # Returns
    /// * `Ok(TaskState)` if the string matches a valid state
    /// * `Err(())` if the string does not match any valid state
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "New" => Ok(TaskState::New),
            "InProgress" => Ok(TaskState::InProgress),
            "Completed" => Ok(TaskState::Completed),
            "Failed" => Ok(TaskState::Failed(String::new())),
            "Ready" => Ok(TaskState::Ready),
            "Configuring" => Ok(TaskState::Configuring),
            _ => Err(()),
        }
    }
}
