/// Module containing agent-related functionality
mod agent_manager;
mod formatter;
mod proposer;
mod reviewer;
mod validator;

pub use formatter::*;
pub use proposer::*;
pub use reviewer::*;
pub use validator::*;

use tracing::debug;

/// Represents the possible outcomes of an agent's execution
pub enum AgentOutcome {
    /// A proposal was successfully generated
    ProposalGenerated,
    /// A revision was requested
    RevisionRequested,
    /// The proposal was approved
    Approved,
    /// The proposal was validated
    Validated,
    /// The result was exported
    Exported,
    /// A module request was made with module name, action and parameters
    ModuleRequest(String, String, Vec<String>),
    /// The execution failed with an error message
    Failed(String),
}

/// Defines the behavior that all agents must implement
#[async_trait::async_trait]
pub trait AgentBehavior {
    /// Executes a single step of the agent's workflow
    ///
    /// # Arguments
    /// * `task` - The task being processed
    ///
    /// # Returns
    /// The outcome of executing the step
    async fn execute_step(&self, task: &mut crate::core::Task) -> AgentOutcome;

    /// Parses a response string to check for module requests
    ///
    /// # Arguments
    /// * `resp` - The response string to parse
    ///
    /// # Returns
    /// Some(AgentOutcome::ModuleRequest) if a module request is found, None otherwise
    fn parse_module_request(&self, resp: &str) -> Option<AgentOutcome> {
        for line in resp.lines() {
            if line.trim().contains("MODULE_REQUEST:") {
                let parts: Vec<_> = line["MODULE_REQUEST:".len()..]
                    .split_whitespace()
                    .map(|s| s.to_string())
                    .collect();
                if parts.len() >= 2 {
                    let module_name = parts[0].clone();
                    let action = parts[1].clone();
                    let params = if parts.len() > 2 {
                        parts[2..].to_vec()
                    } else {
                        vec![]
                    };

                    debug!(
                        "executing module request: {} {} {:?}",
                        module_name, action, params
                    );
                    return Some(AgentOutcome::ModuleRequest(module_name, action, params));
                }
            }
        }
        None
    }
}

impl AgentOutcome {
    /// Converts the outcome to its string condition representation
    ///
    /// # Returns
    /// A string slice representing the condition for this outcome
    pub fn as_condition(&self) -> &str {
        match self {
            AgentOutcome::ProposalGenerated => "proposal_generated",
            AgentOutcome::RevisionRequested => "revision_requested",
            AgentOutcome::Approved => "approved",
            AgentOutcome::Validated => "validated",
            AgentOutcome::Exported => "exported",
            AgentOutcome::Failed(_) => "failed",
            AgentOutcome::ModuleRequest(_, _, _) => "module_request",
        }
    }
}
