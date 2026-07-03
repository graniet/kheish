//! Internal daemon services used to incrementally decompose `DaemonState`.

mod board;
mod channel;
mod connector;
mod connector_ingress;
mod delivery;
mod derivation;
mod external_action;
mod goal;
mod learning;
mod learning_extraction;
mod learning_judge;
mod learning_policy;
mod observation;
mod persona;
mod playbook;
mod procedural_skills;
mod project;
mod run;
mod runtime_config;
mod schedule;
mod session;
mod subagent;
mod task;

pub(crate) use board::BoardService;
pub(crate) use channel::{
    ChannelReactionMutation, ChannelService, CreateChannelMessageRecord, CreateChannelRecord,
};
pub(crate) use connector::{ConnectorConfigRecord, ConnectorConfigSource, ConnectorService};
pub(crate) use connector_ingress::ConnectorIngressService;
pub(crate) use delivery::DeliveryService;
pub(crate) use derivation::DerivationService;
pub use external_action::ExternalActionAuditRecord;
pub(crate) use external_action::ExternalActionService;
pub(crate) use goal::{GoalService, SessionGoalPatch};
pub(crate) use learning::{
    LearningCandidateListFilter, LearningListFilter, LearningService, learning_is_prompt_visible,
    learning_is_session_memory_search_visible, rank_learning_record,
};
pub(crate) use learning_extraction::LearningExtractionService;
pub(crate) use learning_judge::LearningJudgeService;
pub(crate) use learning_policy::{
    LearningAutomationEvaluation, LearningMutationMode, LearningPolicyService,
};
pub(crate) use observation::{
    ObservationIngressRateLimitDecision, ObservationService, ObservationUploadAuthorization,
};
pub(crate) use persona::PersonaService;
pub(crate) use playbook::PlaybookService;
pub(crate) use procedural_skills::LearningSkillService;
pub(crate) use project::ProjectService;
pub(crate) use run::{AgentRunSummaryOverlay, RunService};
pub(crate) use runtime_config::RuntimeConfigService;
pub(crate) use schedule::{
    ScheduleDueCompletion, ScheduleDueDecision, ScheduleService, SchedulerSnapshot,
};
pub(crate) use session::{
    ArchivedTaskIndex, SessionService, archived_terminal_tasks, latest_archived_terminal_task,
};
pub(crate) use subagent::{SpawnRequestReservation, SpawnReservation, SubagentService};
pub(crate) use task::{
    BackgroundShellTaskFinalState, BackgroundShellTaskHandle, FinalizedBackgroundShellTask,
    TaskService, apply_background_shell_shutdown_outcome, background_shell_task_shutdown_unsettled,
};
