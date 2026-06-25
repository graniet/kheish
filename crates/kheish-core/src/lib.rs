//! Core agent loop primitives for Kheish.

mod approvals;
mod compaction;
mod engine;
mod hooks;
mod microcompact;
mod snip;
mod tokens;
mod user_questions;

pub use approvals::{
    apply_approval_resolutions, pending_approval_requests, pending_approval_requests_from_decisions,
};
pub use compaction::{
    build_compaction_resume_message, build_compaction_system_prompt, build_compaction_user_prompt,
    format_compaction_summary,
};
pub use engine::{
    AgentEngine, AllowAllPermissions, LoopPolicy, ModelDriver, ModelRequest, ModelRequestKind,
    ModelTurn, PermissionGate, PostCompactRestorationProvider, RunOutcome, ToolCatalog,
    ToolExecutor,
};
pub use hooks::{HookDispatcher, NoopHookDispatcher};
pub use microcompact::{
    CLEARED_TOOL_RESULT_MESSAGE, COMPACTABLE_TOOLS, MicrocompactResult, microcompact_tool_results,
};
pub use snip::{SnipResult, snip_if_needed};
pub use tokens::{
    calibrated_prompt_token_count, calibrated_token_count, latest_api_usage, rough_token_estimate,
    rough_token_estimate_all, rough_token_estimate_message, rough_token_estimate_value,
};
pub use user_questions::{
    UserQuestionValidationCode, UserQuestionValidationError, render_user_question_resolution,
};
