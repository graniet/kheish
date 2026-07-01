//! Multi-agent supervision and orchestration for Kheish runtimes.

mod orchestrator;
mod snapshot;
mod supervisor;
mod tests;
mod types;

pub use orchestrator::AgentOrchestrator;
pub use supervisor::{AgentSupervisor, AgentSupervisorAuditSink};
pub use types::{
    AgentId, AgentRecord, AgentStatus, AgentSupervisorAuditEntry, AgentSupervisorSnapshot,
    AgentSupervisorStatusSnapshot, ChildRetentionPolicy, DaemonOwnedWorktree, ForkContext,
    InterruptResult, MailboxMessage, MailboxMessageState, ManagedAgentSnapshot, SubtaskSpec,
};
