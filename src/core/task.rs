use super::task_context::TaskContext;
use super::task_state::TaskState;
use crate::config::TaskConfig;
use crate::constants::MAX_PROPOSER_FEEDBACK_COUNT;
use crate::core::process_task_context;
use crate::db::Task as DbTask;
use crate::llm::ChatMessage;
use serde_json::Value;
use std::str::FromStr;

/// Represents a task with its associated state and context
#[derive(Debug, Clone)]
pub struct Task {
    /// ID of the task
    pub task_id: String,
    /// Name of the task
    pub name: String,
    /// Description of the task
    pub description: String,
    /// Current state of the task
    pub state: TaskState,
    /// Task context containing input data
    pub context: TaskContext,
    /// History of all proposals made for this task
    pub proposal_history: Vec<String>,
    /// Current active proposal being considered
    pub current_proposal: Option<String>,
    /// Final output of the task
    pub final_output: Option<Value>,
    /// Feedback history
    pub feedback_history: Vec<String>,
    /// Module execution history
    pub module_execution_history: Vec<String>,
    /// Conversation history
    pub conversation: Vec<ChatMessage>,
}

impl Task {
    /// Creates a new Task with the given name and context
    ///
    /// # Arguments
    ///
    /// * `name` - Name of the task
    /// * `context` - Initial task context
    pub fn new(task_id: String, name: String, description: String, context: TaskContext) -> Self {
        Self {
            task_id,
            name,
            description,
            state: TaskState::New,
            context,
            proposal_history: Vec::new(),
            current_proposal: None,
            final_output: None,
            feedback_history: Vec::new(),
            module_execution_history: Vec::new(),
            conversation: Vec::new(),
        }
    }

    /// Adds a new proposal to the task's history and sets it as current
    ///
    /// # Arguments
    ///
    /// * `proposal` - The proposal text to add
    pub fn add_proposal(&mut self, proposal: String) {
        self.proposal_history.push(proposal.clone());
        self.current_proposal = Some(proposal);
    }

    /// Sets the current feedback for the task
    ///
    /// # Arguments
    ///
    /// * `feedback` - Optional feedback text
    pub fn set_feedback(&mut self, feedback: Option<String>) {
        self.feedback_history
            .push(feedback.clone().unwrap_or_default());
        while self.feedback_history.len() > MAX_PROPOSER_FEEDBACK_COUNT {
            self.feedback_history.remove(0);
        }
    }

    /// Returns all feedback history joined by newlines
    ///
    /// # Returns
    ///
    /// A string containing all feedback messages separated by newlines
    pub fn feedback_for_prompt(&self) -> String {
        self.feedback_history.join("\n")
    }

    /// Adds a new module execution to the task's history
    ///
    /// # Arguments
    ///
    /// * `execution` - The execution text to add
    pub fn add_module_execution(&mut self, execution: String) {
        self.module_execution_history.push(execution);
    }

    /// Returns all module executions joined by newlines
    ///
    /// # Returns
    ///
    /// A string containing all module execution messages separated by newlines
    pub fn module_execution_for_prompt(&self) -> String {
        self.module_execution_history.join("\n")
    }
}

impl From<DbTask> for Task {
    fn from(db_task: DbTask) -> Self {
        Self {
            task_id: db_task.task_id,
            name: db_task.name.unwrap_or("".to_string()),
            description: db_task.description.unwrap_or("".to_string()),
            state: TaskState::from_str(&db_task.state).unwrap_or(TaskState::New),
            context: TaskContext::new(),
            proposal_history: Vec::new(),
            current_proposal: None,
            final_output: None,
            feedback_history: Vec::new(),
            module_execution_history: Vec::new(),
            conversation: Vec::new(),
        }
    }
}

impl From<(DbTask, TaskConfig)> for Task {
    fn from((db_task, task_config): (DbTask, TaskConfig)) -> Self {
        let context = process_task_context(&task_config);
        Self {
            task_id: db_task.task_id,
            name: db_task.name.unwrap_or("".to_string()),
            description: db_task.description.unwrap_or("".to_string()),
            state: TaskState::from_str(&db_task.state).unwrap_or(TaskState::New),
            context,
            proposal_history: Vec::new(),
            current_proposal: None,
            final_output: None,
            feedback_history: Vec::new(),
            module_execution_history: Vec::new(),
            conversation: Vec::new(),
        }
    }
}
