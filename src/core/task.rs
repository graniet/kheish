use super::task_context::TaskContext;
use super::task_state::TaskState;
use crate::constants::MAX_PROPOSER_FEEDBACK_COUNT;
use crate::llm::ChatMessage;

/// Represents a task with its associated state and context
#[derive(Debug, Clone)]
pub struct Task {
    /// Name of the task
    pub name: String,
    /// Current state of the task
    pub state: TaskState,
    /// Task context containing input data
    pub context: TaskContext,
    /// History of all proposals made for this task
    pub proposal_history: Vec<String>,
    /// Current active proposal being considered
    pub current_proposal: Option<String>,
    /// Final output of the task
    pub final_output: Option<String>,
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
    pub fn new(name: String, context: TaskContext) -> Self {
        Self {
            name,
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
