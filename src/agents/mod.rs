mod agent_manager;
mod formatter;
mod proposer;
mod reviewer;
mod validator;

pub use formatter::*;
pub use proposer::*;
pub use reviewer::*;
pub use validator::*;

use tracing::info;

pub enum AgentOutcome {
    ProposalGenerated,
    RevisionRequested,
    Approved,
    Validated,
    Exported,
    ModuleRequest(String, String, Vec<String>),
    Failed(String),
}

#[async_trait::async_trait]
pub trait AgentBehavior {
    async fn execute_step(&self, task: &mut crate::core::Task) -> AgentOutcome;

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

                    info!(
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
