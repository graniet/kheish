use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::ModelGenerationConfig;
use crate::tools::ToolSurfaceFilter;

/// Current hook invocation/outcome wire-contract version.
pub const HOOK_CONTRACT_VERSION: u32 = 1;

/// Supported lifecycle events for external hook execution.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEventName {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionRequest,
    PermissionDenied,
    Setup,
    SessionStart,
    SessionEnd,
    Stop,
    StopFailure,
    UserPromptSubmit,
    PreCompact,
    PostCompact,
    SubagentStart,
    SubagentStop,
    TeammateIdle,
    TaskCreated,
    TaskCompleted,
    Elicitation,
    ElicitationResult,
    ConfigChange,
    WorktreeCreate,
    WorktreeRemove,
    FileChanged,
    InstructionsLoaded,
    Notification,
    CwdChanged,
}

/// A generic hook-level decision used outside permission-specific flows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookDecision {
    Approve,
    Block,
}

/// A permission override produced by a hook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPermissionBehavior {
    Allow,
    Deny,
    Ask,
}

/// The scope that owns one hook-emitted permission update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPermissionUpdateScope {
    User,
    Project,
    Session,
}

/// The behavior applied by one hook-emitted permission update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPermissionUpdateBehavior {
    Allow,
    Deny,
    Ask,
}

/// Whether hook executor failures should allow or block the triggering action.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookFailureMode {
    /// Preserve fail-open compatibility: record the failure and continue.
    #[default]
    Open,
    /// Fail closed: convert the failed hook into a blocking hook outcome.
    Closed,
}

/// Retry and failure behavior for one configured hook.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookFailurePolicy {
    /// Failure mode after all retry attempts are exhausted.
    #[serde(default)]
    pub mode: HookFailureMode,
    /// Number of retry attempts after the initial execution. Capped by the daemon.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub max_retries: u8,
}

/// One permission update emitted by a hook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookPermissionUpdate {
    /// The rule scope that should receive the update.
    pub scope: HookPermissionUpdateScope,
    /// The tool-name matcher used by the updated permission rule.
    pub tool_name_pattern: String,
    /// The resulting permission behavior.
    pub behavior: HookPermissionUpdateBehavior,
    /// Optional human-readable rationale stored with the update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One provider and generation override used by model-backed hooks.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HookModelConfig {
    /// Optional explicit provider name such as `anthropic` or `openai`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Optional generation overrides applied to the hook request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<ModelGenerationConfig>,
}

/// One configured hook executor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookExecutorConfig {
    /// Execute one local shell command with JSON input on stdin.
    Command {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    /// POST one JSON payload to an HTTP endpoint.
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    /// Run one lightweight model prompt and parse a structured JSON response.
    Prompt {
        template: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<HookModelConfig>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    /// Run one isolated ephemeral agent and parse a structured JSON response.
    Agent {
        template: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<HookModelConfig>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_surface: Option<ToolSurfaceFilter>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_turns: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    /// Call one in-process callback registered at runtime.
    Callback {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
}

/// One configured hook registration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookDefinition {
    /// Stable operator-facing hook name.
    pub name: String,
    /// Optional matcher applied to the invocation subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    /// Failure behavior for this hook. Defaults to fail-open with no retries.
    #[serde(default, skip_serializing_if = "HookFailurePolicy::is_default")]
    pub failure_policy: HookFailurePolicy,
    /// The configured execution backend.
    pub executor: HookExecutorConfig,
}

/// All configured hooks keyed by event name.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HookSettings {
    /// Registered hooks grouped by event.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<HookEventName, Vec<HookDefinition>>,
}

impl HookSettings {
    /// Returns the configured hooks for one event in declaration order.
    pub fn event_hooks(&self, event: HookEventName) -> &[HookDefinition] {
        self.hooks.get(&event).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// One concrete hook invocation emitted by the runtime.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookInvocation {
    /// The event that triggered the invocation.
    pub event: HookEventName,
    /// Optional subject used by matchers, such as a tool name or file path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Optional session identifier associated with the invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Optional agent identifier associated with the invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Optional run identifier associated with the invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Event-specific payload.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub payload: Value,
}

/// The aggregated outcome returned by one or more hook handlers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookDispatchOutcome {
    /// The hooks that matched this invocation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_hooks: Vec<String>,
    /// Whether execution should continue after the hook.
    #[serde(default = "default_true")]
    pub continue_execution: bool,
    /// Optional human-readable stop reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Optional generic approve/block decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<HookDecision>,
    /// Optional permission override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<HookPermissionBehavior>,
    /// Optional updated tool or permission input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<Value>,
    /// Optional updated tool output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_output: Option<Value>,
    /// Optional permission updates emitted by the hook.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub updated_permissions: Vec<HookPermissionUpdate>,
    /// Additional context strings to inject into the agent transcript.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_contexts: Vec<String>,
    /// Optional initial user message suggested by SessionStart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_user_message: Option<String>,
    /// Optional dynamic watch paths registered by the hook.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watch_paths: Vec<String>,
    /// Whether a permission-denied tool should be retried.
    #[serde(default)]
    pub retry: bool,
}

impl Default for HookDispatchOutcome {
    fn default() -> Self {
        Self {
            matched_hooks: Vec::new(),
            continue_execution: true,
            stop_reason: None,
            decision: None,
            permission: None,
            updated_input: None,
            updated_output: None,
            updated_permissions: Vec::new(),
            additional_contexts: Vec::new(),
            initial_user_message: None,
            watch_paths: Vec::new(),
            retry: false,
        }
    }
}

fn default_true() -> bool {
    true
}

fn is_zero_u8(value: &u8) -> bool {
    *value == 0
}

impl HookFailurePolicy {
    fn is_default(value: &Self) -> bool {
        value == &Self::default()
    }
}
