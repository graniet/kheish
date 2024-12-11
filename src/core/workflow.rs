use crate::config::WorkflowStep;

/// Represents a workflow that defines transitions between different roles based on conditions
#[derive(Debug, Clone)]
pub struct Workflow {
    /// The list of workflow steps defining the transitions
    pub steps: Vec<WorkflowStep>,
}

impl Workflow {
    /// Creates a new Workflow instance
    ///
    /// # Arguments
    ///
    /// * `steps` - Vector of workflow steps defining the role transitions
    ///
    /// # Returns
    ///
    /// A new Workflow instance initialized with the provided steps
    pub fn new(steps: Vec<WorkflowStep>) -> Self {
        Workflow { steps }
    }

    /// Determines the next role based on the current role and condition
    ///
    /// # Arguments
    ///
    /// * `from` - The current role
    /// * `condition` - The condition that triggered the transition
    ///
    /// # Returns
    ///
    /// The next role as a String if a matching transition is found, None otherwise
    pub fn next_role(&self, from: &str, condition: &str) -> Option<String> {
        for step in &self.steps {
            if step.from == from && step.condition == condition {
                return Some(step.to.clone());
            }
        }
        None
    }
}
