use crate::{agents::AgentOutcome, core::Task};

#[derive(Debug, Clone)]
pub enum Event {
    NewRequest(String, Task),                  // role, task
    AgentResponse(String, AgentOutcome, Task), // role, outcome, task
}
