use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::compaction::{
    CompactBoundaryMetadata, CompactionBoundary, PostCompactRestoration, SummaryBlock,
};
use crate::interaction::{PermissionDecision, UserQuestionRequest, UserQuestionResolution};
use crate::learning::LearnedContextBundle;
use crate::memory::RecoveredMemoryBundle;
use crate::model::{CompletionRequirement, ModelGenerationConfig, ModelUsage};
use crate::routing::{InputContentPart, InputEnvelope};
use crate::tools::{MessageRecord, Role, ToolCallRecord, ToolResultRecord};
use crate::{ActiveSkillSnapshot, CapabilityScope, CredentialScope};

/// Stable metadata key used to persist session control state.
pub const SESSION_CONTROL_STATE_METADATA_KEY: &str = "session_control_state";
/// Stable metadata key used to persist one long-running session goal.
pub const SESSION_GOAL_METADATA_KEY: &str = "session_goal";
/// Stable metadata key used to persist one session route policy.
pub const SESSION_ROUTE_POLICY_METADATA_KEY: &str = "session_route_policy";
/// Stable metadata key used to persist one bound session persona snapshot.
pub const SESSION_PERSONA_BINDING_METADATA_KEY: &str = "session_persona_binding";
/// Stable metadata key used to persist one session capability scope override.
pub const SESSION_CAPABILITY_SCOPE_METADATA_KEY: &str = "session_capability_scope";
/// Stable metadata key used to persist one session credential scope override.
pub const SESSION_CREDENTIAL_SCOPE_METADATA_KEY: &str = "session_credential_scope";
/// Stable metadata key used to persist one session execution identity snapshot.
pub const SESSION_EXECUTION_IDENTITY_METADATA_KEY: &str = "session_execution_identity";
/// Stable metadata key used to persist explicit session reply-target defaults.
pub const SESSION_REPLY_TARGETS_METADATA_KEY: &str = "session_reply_targets";
/// Stable metadata key used to persist one model-facing operator contact policy.
pub const SESSION_OPERATOR_CONFIG_METADATA_KEY: &str = "session_operator_config";

/// Stable metadata key carrying per-session native tool surface overrides.
pub const SESSION_TOOL_OVERRIDES_METADATA_KEY: &str = "session_tool_overrides";
/// Stable metadata key carrying one session's structured output contract.
pub const SESSION_OUTPUT_CONTRACT_METADATA_KEY: &str = "session_output_contract";
/// Session metadata key persisting the structured input contract.
pub const SESSION_INPUT_CONTRACT_METADATA_KEY: &str = "session_input_contract";
/// Stable metadata key used to persist hook runtime state.
pub const HOOK_RUNTIME_STATE_METADATA_KEY: &str = "hook_runtime_state";
/// Stable metadata key used to persist one session's private social ledger.
pub const SESSION_SOCIAL_LEDGER_METADATA_KEY: &str = "session_social_ledger";
/// Sentinel value for an unbounded autonomous-agent turn policy.
pub const UNBOUNDED_AGENT_MAX_TURNS: usize = 0;
/// Default turn ceiling for long-running autonomous agents. Unbounded loops
/// (`UNBOUNDED_AGENT_MAX_TURNS`) stay available as an explicit operator opt-in
/// so a misconfigured or looping run cannot burn tokens forever by default.
pub const DEFAULT_AGENT_MAX_TURNS: usize = 500;

/// Stores one durable plan artifact captured while exiting plan mode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanArtifact {
    /// Stable plan identifier.
    pub id: String,
    /// The plan body approved or awaiting review.
    pub content: String,
    /// Optional short summary for operator surfaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Creation timestamp in milliseconds.
    pub created_at_ms: u64,
    /// Last update timestamp in milliseconds.
    pub updated_at_ms: u64,
}

/// One todo item persisted with one session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// Stable todo identifier.
    pub id: String,
    /// The user-visible todo content.
    pub content: String,
    /// Whether the item is completed.
    #[serde(default)]
    pub completed: bool,
}

/// Lifecycle state for one tracked task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// The task exists but no agent started it yet.
    Pending,
    /// One agent is actively working on the task.
    InProgress,
    /// The task is blocked by dependencies or operator input.
    Blocked,
    /// The task completed successfully.
    Completed,
    /// The task failed and needs attention.
    Failed,
    /// The task was stopped intentionally.
    Cancelled,
}

/// One durable task record attached to one session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    /// Stable task identifier.
    pub id: String,
    /// Short task title.
    pub title: String,
    /// Longer operator/model-facing description.
    pub description: String,
    /// Current task lifecycle state.
    pub status: TaskStatus,
    /// Optional agent currently assigned to the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_agent_id: Option<String>,
    /// IDs of tasks blocking this task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
    /// IDs of tasks this task blocks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<String>,
    /// Optional current task output or conclusion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Optional structured task metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
    /// Creation timestamp in milliseconds.
    pub created_at_ms: u64,
    /// Last update timestamp in milliseconds.
    pub updated_at_ms: u64,
}

/// Why one task left the hot session control state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskArchiveReason {
    /// The task reached a terminal status (completed, failed, or cancelled).
    #[default]
    Terminal,
    /// The task was explicitly deleted; it stays hidden from task views.
    Deleted,
}

/// One task archived out of the hot session control state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivedTaskRecord {
    /// The task snapshot at archival time.
    pub task: TaskRecord,
    /// Archival timestamp in milliseconds.
    pub archived_at_ms: u64,
    /// Why the task was archived.
    #[serde(default)]
    pub reason: TaskArchiveReason,
}

/// Compact tally of terminal tasks archived out of the hot control state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivedTaskCounts {
    /// Archived tasks that completed successfully.
    #[serde(default)]
    pub completed: u64,
    /// Archived tasks that failed.
    #[serde(default)]
    pub failed: u64,
    /// Archived tasks that were cancelled.
    #[serde(default)]
    pub cancelled: u64,
}

impl ArchivedTaskCounts {
    /// Returns whether nothing has been archived yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// Returns the total number of archived terminal tasks.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.completed + self.failed + self.cancelled
    }

    /// Tallies one archived task status.
    pub fn record(&mut self, status: &TaskStatus) {
        match status {
            TaskStatus::Completed => self.completed += 1,
            TaskStatus::Failed => self.failed += 1,
            TaskStatus::Cancelled => self.cancelled += 1,
            TaskStatus::Pending | TaskStatus::InProgress | TaskStatus::Blocked => {}
        }
    }
}

/// Session-scoped control state surfaced to the agent loop.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionControlState {
    /// Whether the session is currently in plan mode.
    #[serde(default)]
    pub plan_mode: bool,
    /// The explicit session-scoped permission mode override when one is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_permission_mode: Option<String>,
    /// Session-scoped permission overrides previously emitted by hooks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_permission_updates: Vec<crate::HookPermissionUpdate>,
    /// The permission mode that was active before entering plan mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_plan_mode: Option<String>,
    /// The latest session plan artifact when plan mode produced one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_artifact: Option<PlanArtifact>,
    /// The current ordered todo list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<TodoItem>,
    /// The current tracked tasks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<TaskRecord>,
    /// Tally of terminal tasks archived out of this hot state.
    #[serde(default, skip_serializing_if = "ArchivedTaskCounts::is_empty")]
    pub archived_tasks: ArchivedTaskCounts,
}

/// Lifecycle state for one long-running session goal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionGoalStatus {
    /// The daemon should keep driving the goal when the session becomes idle.
    Active,
    /// User or system paused the goal; it should not auto-continue.
    Paused,
    /// The goal reached its token budget and should only wrap up.
    BudgetLimited,
    /// The goal was explicitly marked achieved.
    Complete,
}

impl SessionGoalStatus {
    /// Returns true when the goal cannot continue without user action.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::BudgetLimited | Self::Complete)
    }
}

/// One idempotently accounted usage unit for a session goal.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGoalUsageAccount {
    /// Non-cached input plus output tokens charged to the goal.
    #[serde(default)]
    pub tokens: u64,
    /// Active run time charged to the goal.
    #[serde(default)]
    pub time_ms: u64,
}

/// One durable long-running objective attached to a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGoal {
    /// Stable goal identifier. Changes when the objective is replaced.
    pub goal_id: String,
    /// Owning daemon session.
    pub session_id: String,
    /// User-provided objective text.
    pub objective: String,
    /// Current lifecycle state.
    pub status: SessionGoalStatus,
    /// Optional token ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    /// Tokens charged so far.
    #[serde(default)]
    pub tokens_used: u64,
    /// Active time charged so far.
    #[serde(default)]
    pub time_used_ms: u64,
    /// Creation timestamp in milliseconds.
    pub created_at_ms: u64,
    /// Last update timestamp in milliseconds.
    pub updated_at_ms: u64,
    /// Monotonic mutation version used for compare-and-set behavior.
    #[serde(default)]
    pub version: u64,
    /// Monotonic model-binding version. Unlike `version`, this excludes usage accounting.
    #[serde(default)]
    pub definition_version: u64,
    /// Run that created this goal through the model-facing tool, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_run_id: Option<String>,
    /// Idempotency ledger keyed by run/turn usage keys.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub accounted_usage: BTreeMap<String, SessionGoalUsageAccount>,
    /// The most recent continuation run scheduled by the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_continuation_run_id: Option<String>,
    /// The run that first moved this goal into `budget_limited`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_limited_by_run_id: Option<String>,
    /// The single wrap-up continuation scheduled after budget limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_wrapup_run_id: Option<String>,
    /// Run that explicitly completed this goal, used to account its final snapshot exactly once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_by_run_id: Option<String>,
}

impl SessionGoal {
    /// Returns the version used to bind model runs to the goal definition.
    pub fn binding_version(&self) -> u64 {
        if self.definition_version == 0 {
            self.version.max(1)
        } else {
            self.definition_version
        }
    }

    /// Returns the remaining token budget when one is configured.
    pub fn remaining_tokens(&self) -> Option<u64> {
        self.token_budget
            .map(|budget| budget.saturating_sub(self.tokens_used))
    }

    /// Returns whether this goal should be driven by daemon continuations.
    pub fn should_continue(&self) -> bool {
        self.status == SessionGoalStatus::Active
    }
}

/// Session-scoped default routing preferences used when one run does not override them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionRoutePolicy {
    /// The preferred provider for new runs when none is specified explicitly.
    #[serde(default, alias = "route_id", skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The preferred generation settings merged into new runs when none is specified explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<ModelGenerationConfig>,
}

impl SessionRoutePolicy {
    /// Returns true when the policy does not pin any route information.
    pub fn is_empty(&self) -> bool {
        self.provider.is_none() && self.generation.is_none()
    }
}

/// One immutable persona snapshot bound to a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPersonaBinding {
    /// The stable persona identifier.
    pub persona_id: String,
    /// The latest persona version visible when the binding was created.
    pub persona_version: u64,
    /// The user-visible persona name captured with the binding.
    pub display_name: String,
    /// The exact persona instructions captured for the session.
    pub soul: String,
    /// The normalized SHA-256 digest of the captured persona instructions.
    pub soul_sha256: String,
    /// The persona-scoped baseline capability policy captured for the session.
    #[serde(default, skip_serializing_if = "CapabilityScope::is_empty")]
    pub capability_scope: CapabilityScope,
    /// The resolved inline skills activated by default through this persona.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_inline_skills: Vec<ActiveSkillSnapshot>,
    /// The timestamp when the session bound this persona snapshot.
    pub bound_at_ms: u64,
}

/// Session-scoped hook execution state persisted with runtime metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookRuntimeState {
    /// Whether Setup hooks have already been executed for this session.
    #[serde(default)]
    pub setup_completed: bool,
    /// Whether SessionStart hooks have already been executed for this session.
    #[serde(default)]
    pub session_started: bool,
    /// Dynamic watch paths registered by hooks for this session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watch_paths: Vec<String>,
}

/// Session-scoped execution identity persisted for delegated work such as sidechains.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionExecutionIdentity {
    /// The stable principal identifier restored for future runs in the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    /// The parent principal identifier when this session was created by delegation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_principal_id: Option<String>,
    /// The stable delegation identifier when this session was spawned explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_id: Option<String>,
}

impl SessionExecutionIdentity {
    /// Returns true when the execution identity does not constrain runtime lineage.
    pub fn is_empty(&self) -> bool {
        self.principal_id.is_none()
            && self.parent_principal_id.is_none()
            && self.delegation_id.is_none()
    }
}

/// Per-session adjustments to the agent's native tool surface.
///
/// The built-in agent profile stays the fail-closed default; overrides let a
/// stack opt one session into tools the profile denies (`ask_user_question`,
/// plan mode…) or retire tools it allows. Operator-contact tools keep their
/// own gating: enabling them here never bypasses the session operator config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionToolOverrides {
    /// Tool names added to the session's surface.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enable: Vec<String>,
    /// Tool names removed from the session's surface.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disable: Vec<String>,
}

impl SessionToolOverrides {
    /// True when the overrides change nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.enable.is_empty() && self.disable.is_empty()
    }
}

/// Session-scoped policy that tells the model how it may contact a human operator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionOperatorConfig {
    /// Whether model-initiated operator contact is enabled for this session.
    #[serde(default)]
    pub enabled: bool,
    /// Optional human-readable label for the operator audience.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Optional style guidance, usually aligned with the bound persona.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub communication_style: Option<String>,
    /// Whether the model may send non-blocking operator notifications.
    #[serde(default = "default_true")]
    pub allow_notify: bool,
    /// Whether the model may suspend the run with a structured operator question.
    #[serde(default = "default_true")]
    pub allow_questions: bool,
}

impl Default for SessionOperatorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            display_name: None,
            communication_style: None,
            allow_notify: true,
            allow_questions: true,
        }
    }
}

impl SessionOperatorConfig {
    /// Returns true when at least one model-initiated operator path is enabled.
    pub fn is_active(&self) -> bool {
        self.enabled && (self.allow_notify || self.allow_questions)
    }

    /// Returns true when operator contact should be omitted from compact views.
    pub fn is_inactive(&self) -> bool {
        !self.is_active()
    }
}

fn default_true() -> bool {
    true
}

/// Decodes persisted session control state from metadata.
pub fn session_control_state_from_metadata(
    metadata: &Value,
) -> serde_json::Result<SessionControlState> {
    metadata
        .get(SESSION_CONTROL_STATE_METADATA_KEY)
        .cloned()
        .map(serde_json::from_value)
        .unwrap_or_else(|| Ok(SessionControlState::default()))
}

/// Returns metadata with session control state merged under the stable key.
pub fn metadata_with_session_control_state(
    metadata: Value,
    state: &SessionControlState,
) -> serde_json::Result<Value> {
    let mut object = match metadata {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("user_metadata".to_string(), other);
            map
        }
    };
    object.insert(
        SESSION_CONTROL_STATE_METADATA_KEY.to_string(),
        serde_json::to_value(state)?,
    );
    Ok(Value::Object(object))
}

/// Decodes persisted session goal state from metadata.
pub fn session_goal_from_metadata(metadata: &Value) -> serde_json::Result<Option<SessionGoal>> {
    metadata
        .get(SESSION_GOAL_METADATA_KEY)
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
}

/// Returns metadata with session goal state merged under the stable key.
pub fn metadata_with_session_goal(
    metadata: Value,
    goal: Option<&SessionGoal>,
) -> serde_json::Result<Value> {
    let mut object = match metadata {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("user_metadata".to_string(), other);
            map
        }
    };
    object.insert(
        SESSION_GOAL_METADATA_KEY.to_string(),
        goal.map(serde_json::to_value)
            .transpose()?
            .unwrap_or(Value::Null),
    );
    Ok(Value::Object(object))
}

/// Maximum number of relationship edges kept per session ledger. Beyond this
/// the lowest-salience edges are evicted, keeping the ledger sparse and the
/// per-agent cost bounded at hundreds of concurrent agents.
pub const MAX_SOCIAL_LEDGER_EDGES: usize = 8;

/// One `remember_about` nudge moves a `trust`/`warmth` scalar by this much,
/// clamped to `[-1.0, 1.0]`. Sized so a single decisive impression crosses the
/// render cue threshold, and a few reinforce toward saturation.
pub const AFFINITY_DRIFT_STEP: f32 = 0.35;

/// Half-life for relaxing `trust`/`warmth` back toward baseline. An impression
/// left untouched this long has faded halfway to resting — the "impressions fade
/// unless reinforced" rule, applied as pure math at load time (never persisted).
pub const AFFINITY_DECAY_HALF_LIFE_MS: u64 = 3 * 24 * 60 * 60 * 1000;

/// A `trust`/`warmth` scalar this far from baseline (after decay) earns a
/// non-numeric cue in the rendered section; closer than this renders nothing.
pub const AFFINITY_CUE_THRESHOLD: f32 = 0.3;

/// Default lifetime of a tab opened via `remember_about`, after which it is swept.
pub const AFFINITY_TAB_TTL_MS: u64 = 14 * 24 * 60 * 60 * 1000;

/// Longest note / tab text kept on an edge, so one tool call can neither bloat
/// the per-turn prompt (rendered every turn, up to the cap) nor the sidecar.
const AFFINITY_TEXT_MAX_CHARS: usize = 280;

/// Longest display name kept on an edge.
const AFFINITY_NAME_MAX_CHARS: usize = 80;

/// Directional standing of the ledger holder relative to a peer. Optional and
/// defaults to `peer`, so a flat, non-hierarchical tenant never encodes rank.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AffinityStanding {
    /// The holder defers to this peer.
    Below,
    /// The holder and peer are equals.
    #[default]
    Peer,
    /// The holder carries authority over this peer.
    Above,
}

/// Direction of a single outstanding social debt or favor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AffinityTabDir {
    /// The peer owes the holder.
    OwedToMe,
    /// The holder owes the peer.
    IOwe,
}

/// Direction a `trust`/`warmth` scalar moves on one `remember_about` nudge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AffinityDrift {
    /// Nudge the scalar up (warmer / more trusting).
    Up,
    /// Nudge the scalar down (cooler / less trusting).
    Down,
}

/// One live, expiring debt/favor between the holder and a peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AffinityOpenTab {
    /// Human-legible description of the debt/favor, from the holder's seat.
    pub text: String,
    /// Who owes whom.
    pub dir: AffinityTabDir,
    /// Wall-clock expiry in milliseconds; expired tabs are swept on the
    /// write/refresh path (in P2), not at render time.
    pub expires_at_ms: u64,
}

/// One directed, private relationship edge from the ledger holder to a peer.
///
/// This is a generic, domain-agnostic primitive: the daemon ships the schema,
/// the tenant supplies the peers and any seed relationships. The `trust` and
/// `warmth` scalars are INTERNAL ranking signals in `[-1.0, 1.0]`; they are
/// never surfaced to the model as numbers and never fed to any business-decision
/// function. Edges are private to the holder — only the holder's own ledger is
/// ever written, so there is no shared graph and no cross-session contention.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AffinityEdge {
    /// Session id of the peer this edge points at.
    pub peer_id: String,
    /// Display name of the peer, as the holder refers to them.
    pub display_name: String,
    /// Internal trust signal in `[-1.0, 1.0]`. Never surfaced as a number.
    #[serde(default)]
    pub trust: f32,
    /// Internal warmth signal in `[-1.0, 1.0]`. Never surfaced as a number.
    #[serde(default)]
    pub warmth: f32,
    /// Directional standing (deference); defaults to `peer`.
    #[serde(default)]
    pub standing: AffinityStanding,
    /// Concrete facts about dealing with this peer, from the holder's seat —
    /// stored as facts/quotes, not a generated relationship summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The single live debt/favor, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_tab: Option<AffinityOpenTab>,
    /// Resting trust the edge decays toward (the persona-seeded baseline).
    #[serde(default)]
    pub baseline_trust: f32,
    /// Resting warmth the edge decays toward (the persona-seeded baseline).
    #[serde(default)]
    pub baseline_warmth: f32,
    /// Last update timestamp in milliseconds.
    #[serde(default)]
    pub updated_at_ms: u64,
}

/// One session's private, bounded social ledger of directed relationship edges.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionSocialLedger {
    /// Outbound directed edges from this session to peers (bounded, sparse).
    #[serde(default)]
    pub edges: Vec<AffinityEdge>,
}

/// A tab the holder wants to open on a peer, before an expiry is stamped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AffinityTabInput {
    /// Human-legible description of the debt/favor, from the holder's seat.
    pub text: String,
    /// Who owes whom.
    pub dir: AffinityTabDir,
}

/// One holder impression of a peer, applied to the ledger by `apply_impression`.
///
/// Every field except `peer_id` is optional and leaves the existing edge value
/// untouched when absent, so an agent records only what actually changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AffinityImpression {
    /// Who the impression is about — a peer session id or ANY actor id, humans
    /// included (the ledger is holder-private, so peers need not be agents).
    pub peer_id: String,
    /// How the holder refers to the peer; sets/updates the edge display name.
    pub display_name: Option<String>,
    /// Optional warmth nudge.
    pub warmth: Option<AffinityDrift>,
    /// Optional trust nudge.
    pub trust: Option<AffinityDrift>,
    /// Optional new standing toward the peer.
    pub standing: Option<AffinityStanding>,
    /// Optional replacement note (a concrete impression, trimmed; empty clears it).
    pub note: Option<String>,
    /// Optional tab to open; its expiry is stamped on apply.
    pub tab: Option<AffinityTabInput>,
    /// When true, clears any existing open tab (a debt/favor is settled).
    pub settle_tab: bool,
}

impl SessionSocialLedger {
    /// Returns true when the ledger carries no edges.
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// Returns the edge pointing at `peer_id`, if the holder knows them.
    pub fn edge_for(&self, peer_id: &str) -> Option<&AffinityEdge> {
        self.edges.iter().find(|edge| edge.peer_id == peer_id)
    }

    /// Applies one holder impression in place: find-or-insert the peer edge, nudge
    /// its scalars by a bounded step, update name/standing/note/tab, and stamp
    /// `updated_at_ms`. When inserting past `MAX_SOCIAL_LEDGER_EDGES`, evicts the
    /// stalest edge so the ledger stays sparse without ever erroring the caller.
    /// Returns the resulting edge.
    pub fn apply_impression(
        &mut self,
        impression: AffinityImpression,
        now_ms: u64,
    ) -> AffinityEdge {
        if let Some(edge) = self
            .edges
            .iter_mut()
            .find(|edge| edge.peer_id == impression.peer_id)
        {
            apply_impression_to_edge(edge, &impression, now_ms);
            return edge.clone();
        }

        if self.edges.len() >= MAX_SOCIAL_LEDGER_EDGES
            && let Some((index, _)) = self
                .edges
                .iter()
                .enumerate()
                .min_by_key(|(_, edge)| edge.updated_at_ms)
        {
            self.edges.remove(index);
        }

        let mut edge = AffinityEdge {
            peer_id: impression.peer_id.clone(),
            display_name: impression.peer_id.clone(),
            trust: 0.0,
            warmth: 0.0,
            standing: AffinityStanding::default(),
            note: None,
            open_tab: None,
            baseline_trust: 0.0,
            baseline_warmth: 0.0,
            updated_at_ms: now_ms,
        };
        apply_impression_to_edge(&mut edge, &impression, now_ms);
        self.edges.push(edge.clone());
        edge
    }

    /// Returns a display copy for the prompt: expired tabs dropped and each
    /// `trust`/`warmth` relaxed toward its baseline by elapsed time. Pure — the
    /// stored ledger is never mutated by rendering.
    pub fn for_prompt(&self, now_ms: u64) -> SessionSocialLedger {
        let edges = self
            .edges
            .iter()
            .map(|edge| {
                let mut edge = edge.clone();
                if edge
                    .open_tab
                    .as_ref()
                    .is_some_and(|tab| tab.expires_at_ms != 0 && tab.expires_at_ms <= now_ms)
                {
                    edge.open_tab = None;
                }
                edge.trust =
                    decay_toward(edge.trust, edge.baseline_trust, edge.updated_at_ms, now_ms);
                edge.warmth = decay_toward(
                    edge.warmth,
                    edge.baseline_warmth,
                    edge.updated_at_ms,
                    now_ms,
                );
                edge
            })
            .collect();
        SessionSocialLedger { edges }
    }
}

fn nudge_scalar(value: f32, drift: Option<AffinityDrift>) -> f32 {
    match drift {
        Some(AffinityDrift::Up) => (value + AFFINITY_DRIFT_STEP).clamp(-1.0, 1.0),
        Some(AffinityDrift::Down) => (value - AFFINITY_DRIFT_STEP).clamp(-1.0, 1.0),
        None => value,
    }
}

fn decay_toward(value: f32, baseline: f32, updated_at_ms: u64, now_ms: u64) -> f32 {
    if now_ms <= updated_at_ms {
        return value;
    }
    let elapsed = (now_ms - updated_at_ms) as f32;
    let factor = 0.5f32.powf(elapsed / AFFINITY_DECAY_HALF_LIFE_MS as f32);
    baseline + (value - baseline) * factor
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn apply_impression_to_edge(edge: &mut AffinityEdge, impression: &AffinityImpression, now_ms: u64) {
    if let Some(name) = impression
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        edge.display_name = truncate_chars(name, AFFINITY_NAME_MAX_CHARS);
    }
    edge.warmth = nudge_scalar(edge.warmth, impression.warmth);
    edge.trust = nudge_scalar(edge.trust, impression.trust);
    if let Some(standing) = impression.standing {
        edge.standing = standing;
    }
    if let Some(note) = impression.note.as_deref().map(str::trim) {
        edge.note = if note.is_empty() {
            None
        } else {
            Some(truncate_chars(note, AFFINITY_TEXT_MAX_CHARS))
        };
    }
    if impression.settle_tab {
        edge.open_tab = None;
    } else if let Some(tab) = &impression.tab {
        if !tab.text.trim().is_empty() {
            edge.open_tab = Some(AffinityOpenTab {
                text: truncate_chars(tab.text.trim(), AFFINITY_TEXT_MAX_CHARS),
                dir: tab.dir,
                expires_at_ms: now_ms.saturating_add(AFFINITY_TAB_TTL_MS),
            });
        }
    } else if edge
        .open_tab
        .as_ref()
        .is_some_and(|tab| tab.expires_at_ms != 0 && tab.expires_at_ms <= now_ms)
    {
        edge.open_tab = None;
    }
    edge.updated_at_ms = now_ms;
}

/// Decodes the persisted social ledger from metadata.
pub fn session_social_ledger_from_metadata(
    metadata: &Value,
) -> serde_json::Result<Option<SessionSocialLedger>> {
    metadata
        .get(SESSION_SOCIAL_LEDGER_METADATA_KEY)
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
}

/// Returns metadata with the social ledger merged under the stable key.
pub fn metadata_with_session_social_ledger(
    metadata: Value,
    ledger: Option<&SessionSocialLedger>,
) -> serde_json::Result<Value> {
    let mut object = match metadata {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("user_metadata".to_string(), other);
            map
        }
    };
    object.insert(
        SESSION_SOCIAL_LEDGER_METADATA_KEY.to_string(),
        ledger
            .map(serde_json::to_value)
            .transpose()?
            .unwrap_or(Value::Null),
    );
    Ok(Value::Object(object))
}

/// Decodes persisted session route policy from metadata.
pub fn session_route_policy_from_metadata(
    metadata: &Value,
) -> serde_json::Result<SessionRoutePolicy> {
    metadata
        .get(SESSION_ROUTE_POLICY_METADATA_KEY)
        .cloned()
        .map(serde_json::from_value)
        .unwrap_or_else(|| Ok(SessionRoutePolicy::default()))
}

/// Returns metadata with session route policy merged under the stable key.
pub fn metadata_with_session_route_policy(
    metadata: Value,
    policy: &SessionRoutePolicy,
) -> serde_json::Result<Value> {
    let mut object = match metadata {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("user_metadata".to_string(), other);
            map
        }
    };
    object.insert(
        SESSION_ROUTE_POLICY_METADATA_KEY.to_string(),
        serde_json::to_value(policy)?,
    );
    Ok(Value::Object(object))
}

/// Decodes one persisted session persona binding from metadata.
pub fn session_persona_binding_from_metadata(
    metadata: &Value,
) -> serde_json::Result<Option<SessionPersonaBinding>> {
    metadata
        .get(SESSION_PERSONA_BINDING_METADATA_KEY)
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
}

/// Returns metadata with a session persona binding merged under the stable key.
pub fn metadata_with_session_persona_binding(
    metadata: Value,
    binding: Option<&SessionPersonaBinding>,
) -> serde_json::Result<Value> {
    let mut object = match metadata {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("user_metadata".to_string(), other);
            map
        }
    };
    match binding {
        Some(binding) => {
            object.insert(
                SESSION_PERSONA_BINDING_METADATA_KEY.to_string(),
                serde_json::to_value(binding)?,
            );
        }
        None => {
            object.remove(SESSION_PERSONA_BINDING_METADATA_KEY);
        }
    }
    Ok(Value::Object(object))
}

/// Decodes persisted session capability scope from metadata.
pub fn session_capability_scope_from_metadata(
    metadata: &Value,
) -> serde_json::Result<CapabilityScope> {
    metadata
        .get(SESSION_CAPABILITY_SCOPE_METADATA_KEY)
        .cloned()
        .map(serde_json::from_value)
        .unwrap_or_else(|| Ok(CapabilityScope::default()))
}

/// Returns metadata with session capability scope merged under the stable key.
pub fn metadata_with_session_capability_scope(
    metadata: Value,
    scope: &CapabilityScope,
) -> serde_json::Result<Value> {
    let mut object = match metadata {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("user_metadata".to_string(), other);
            map
        }
    };
    object.insert(
        SESSION_CAPABILITY_SCOPE_METADATA_KEY.to_string(),
        serde_json::to_value(scope)?,
    );
    Ok(Value::Object(object))
}

/// Decodes persisted session credential scope from metadata.
pub fn session_credential_scope_from_metadata(
    metadata: &Value,
) -> serde_json::Result<CredentialScope> {
    metadata
        .get(SESSION_CREDENTIAL_SCOPE_METADATA_KEY)
        .cloned()
        .map(serde_json::from_value)
        .unwrap_or_else(|| Ok(CredentialScope::default()))
}

/// Returns metadata with session credential scope merged under the stable key.
pub fn metadata_with_session_credential_scope(
    metadata: Value,
    scope: &CredentialScope,
) -> serde_json::Result<Value> {
    let mut object = match metadata {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("user_metadata".to_string(), other);
            map
        }
    };
    object.insert(
        SESSION_CREDENTIAL_SCOPE_METADATA_KEY.to_string(),
        serde_json::to_value(scope)?,
    );
    Ok(Value::Object(object))
}

/// Decodes persisted session execution identity from metadata.
pub fn session_execution_identity_from_metadata(
    metadata: &Value,
) -> serde_json::Result<SessionExecutionIdentity> {
    metadata
        .get(SESSION_EXECUTION_IDENTITY_METADATA_KEY)
        .cloned()
        .map(serde_json::from_value)
        .unwrap_or_else(|| Ok(SessionExecutionIdentity::default()))
}

/// Returns metadata with session execution identity merged under the stable key.
pub fn metadata_with_session_execution_identity(
    metadata: Value,
    identity: &SessionExecutionIdentity,
) -> serde_json::Result<Value> {
    let mut object = match metadata {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("user_metadata".to_string(), other);
            map
        }
    };
    if identity.is_empty() {
        object.remove(SESSION_EXECUTION_IDENTITY_METADATA_KEY);
    } else {
        object.insert(
            SESSION_EXECUTION_IDENTITY_METADATA_KEY.to_string(),
            serde_json::to_value(identity)?,
        );
    }
    Ok(Value::Object(object))
}

/// Decodes persisted explicit session reply targets from metadata.
///
/// Returns `None` when the session has never stored explicit reply targets. Returns
/// `Some(Vec::new())` when the session explicitly cleared its defaults.
pub fn session_reply_targets_from_metadata(
    metadata: &Value,
) -> serde_json::Result<Option<Vec<crate::ReplyHandle>>> {
    match metadata.get(SESSION_REPLY_TARGETS_METADATA_KEY) {
        Some(value) if value.is_null() => Ok(Some(Vec::new())),
        Some(value) => serde_json::from_value(value.clone()).map(Some),
        None => Ok(None),
    }
}

/// Returns metadata with explicit session reply targets merged under the stable key.
///
/// Passing `Some([])` stores an explicit clear tombstone. Passing `None` removes the key.
pub fn metadata_with_session_reply_targets(
    metadata: Value,
    reply_targets: Option<&[crate::ReplyHandle]>,
) -> serde_json::Result<Value> {
    let mut object = match metadata {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("user_metadata".to_string(), other);
            map
        }
    };
    match reply_targets {
        Some(reply_targets) if reply_targets.is_empty() => {
            object.insert(SESSION_REPLY_TARGETS_METADATA_KEY.to_string(), Value::Null);
        }
        Some(reply_targets) => {
            object.insert(
                SESSION_REPLY_TARGETS_METADATA_KEY.to_string(),
                serde_json::to_value(reply_targets)?,
            );
        }
        None => {
            object.remove(SESSION_REPLY_TARGETS_METADATA_KEY);
        }
    }
    Ok(Value::Object(object))
}

/// Decodes the model-facing operator policy from persisted session metadata.
pub fn session_operator_config_from_metadata(
    metadata: &Value,
) -> serde_json::Result<SessionOperatorConfig> {
    metadata
        .get(SESSION_OPERATOR_CONFIG_METADATA_KEY)
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .unwrap_or_else(|| Ok(SessionOperatorConfig::default()))
}

/// Decodes the native tool surface overrides from persisted session metadata.
pub fn session_tool_overrides_from_metadata(
    metadata: &Value,
) -> serde_json::Result<SessionToolOverrides> {
    metadata
        .get(SESSION_TOOL_OVERRIDES_METADATA_KEY)
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .unwrap_or_else(|| Ok(SessionToolOverrides::default()))
}

/// Decodes the structured input contract from persisted session metadata.
pub fn session_input_contract_from_metadata(
    metadata: &Value,
) -> serde_json::Result<Option<crate::StructuredInputContract>> {
    metadata
        .get(SESSION_INPUT_CONTRACT_METADATA_KEY)
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
}

/// Decodes the structured output contract from persisted session metadata.
pub fn session_output_contract_from_metadata(
    metadata: &Value,
) -> serde_json::Result<Option<crate::StructuredOutputContract>> {
    metadata
        .get(SESSION_OUTPUT_CONTRACT_METADATA_KEY)
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
}

/// Returns metadata with the model-facing operator policy merged under the stable key.
pub fn metadata_with_session_operator_config(
    metadata: Value,
    config: Option<&SessionOperatorConfig>,
) -> serde_json::Result<Value> {
    let mut object = match metadata {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("user_metadata".to_string(), other);
            map
        }
    };
    match config {
        Some(config) if config.is_active() => {
            object.insert(
                SESSION_OPERATOR_CONFIG_METADATA_KEY.to_string(),
                serde_json::to_value(config)?,
            );
        }
        _ => {
            object.remove(SESSION_OPERATOR_CONFIG_METADATA_KEY);
        }
    }
    Ok(Value::Object(object))
}

/// Decodes persisted hook runtime state from metadata.
pub fn hook_runtime_state_from_metadata(metadata: &Value) -> serde_json::Result<HookRuntimeState> {
    metadata
        .get(HOOK_RUNTIME_STATE_METADATA_KEY)
        .cloned()
        .map(serde_json::from_value)
        .unwrap_or_else(|| Ok(HookRuntimeState::default()))
}

/// Returns metadata with hook runtime state merged under the stable key.
pub fn metadata_with_hook_runtime_state(
    metadata: Value,
    state: &HookRuntimeState,
) -> serde_json::Result<Value> {
    let mut object = match metadata {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("user_metadata".to_string(), other);
            map
        }
    };
    object.insert(
        HOOK_RUNTIME_STATE_METADATA_KEY.to_string(),
        serde_json::to_value(state)?,
    );
    Ok(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::{
        AFFINITY_DECAY_HALF_LIFE_MS, AFFINITY_DRIFT_STEP, AFFINITY_TAB_TTL_MS, AffinityDrift,
        AffinityEdge, AffinityImpression, AffinityOpenTab, AffinityStanding, AffinityTabDir,
        AffinityTabInput, MAX_SOCIAL_LEDGER_EDGES, SessionPersonaBinding, SessionSocialLedger,
        metadata_with_session_capability_scope, metadata_with_session_persona_binding,
        metadata_with_session_social_ledger, session_capability_scope_from_metadata,
        session_persona_binding_from_metadata, session_social_ledger_from_metadata,
    };
    use crate::CapabilityScope;
    use serde_json::json;

    #[test]
    fn session_social_ledger_round_trips_through_metadata() {
        let ledger = SessionSocialLedger {
            edges: vec![AffinityEdge {
                peer_id: "agent-peer".to_string(),
                display_name: "Peer".to_string(),
                trust: 0.5,
                warmth: -0.25,
                standing: AffinityStanding::Above,
                note: Some("covered your on-call swap last week".to_string()),
                open_tab: None,
                baseline_trust: 0.25,
                baseline_warmth: 0.0,
                updated_at_ms: 42,
            }],
        };

        let metadata =
            metadata_with_session_social_ledger(json!({ "custom": true }), Some(&ledger))
                .expect("metadata update should succeed");

        assert_eq!(
            session_social_ledger_from_metadata(&metadata).expect("metadata decode should succeed"),
            Some(ledger)
        );
        assert_eq!(metadata.get("custom"), Some(&json!(true)));
    }

    #[test]
    fn session_social_ledger_decode_treats_null_as_absent() {
        let metadata = metadata_with_session_social_ledger(json!({ "other": 1 }), None)
            .expect("metadata update should succeed");
        assert!(
            session_social_ledger_from_metadata(&metadata)
                .expect("metadata decode should succeed")
                .is_none()
        );
    }

    fn fixed_now() -> u64 {
        1_000_000_000_000
    }

    #[test]
    fn apply_impression_creates_edge_for_a_human_peer() {
        let mut ledger = SessionSocialLedger::default();
        let edge = ledger.apply_impression(
            AffinityImpression {
                peer_id: "operator".to_string(),
                display_name: Some("Operator".to_string()),
                warmth: Some(AffinityDrift::Down),
                note: Some("cassant quand c'est tard".to_string()),
                tab: Some(AffinityTabInput {
                    text: "je lui dois un correctif propre".to_string(),
                    dir: AffinityTabDir::IOwe,
                }),
                ..AffinityImpression::default()
            },
            fixed_now(),
        );

        assert_eq!(edge.peer_id, "operator");
        assert_eq!(edge.display_name, "Operator");
        assert!((edge.warmth - (-AFFINITY_DRIFT_STEP)).abs() < 1e-4);
        assert_eq!(edge.standing, AffinityStanding::Peer);
        assert_eq!(edge.note.as_deref(), Some("cassant quand c'est tard"));
        let tab = edge.open_tab.expect("tab set");
        assert_eq!(tab.dir, AffinityTabDir::IOwe);
        assert_eq!(tab.expires_at_ms, fixed_now() + AFFINITY_TAB_TTL_MS);
        assert_eq!(ledger.edges.len(), 1);
        assert!(ledger.edge_for("operator").is_some());
    }

    #[test]
    fn apply_impression_accumulates_and_settles_on_existing_edge() {
        let mut ledger = SessionSocialLedger::default();
        ledger.apply_impression(
            AffinityImpression {
                peer_id: "agent-karim".to_string(),
                warmth: Some(AffinityDrift::Up),
                tab: Some(AffinityTabInput {
                    text: "le compte-rendu Batignolles".to_string(),
                    dir: AffinityTabDir::IOwe,
                }),
                ..AffinityImpression::default()
            },
            fixed_now(),
        );
        let edge = ledger.apply_impression(
            AffinityImpression {
                peer_id: "agent-karim".to_string(),
                warmth: Some(AffinityDrift::Up),
                settle_tab: true,
                ..AffinityImpression::default()
            },
            fixed_now() + 1,
        );

        assert!((edge.warmth - 2.0 * AFFINITY_DRIFT_STEP).abs() < 1e-4);
        assert!(edge.open_tab.is_none(), "settle clears the tab");
        assert_eq!(ledger.edges.len(), 1, "same peer updates in place");
    }

    #[test]
    fn apply_impression_evicts_the_stalest_edge_at_cap() {
        let mut ledger = SessionSocialLedger::default();
        for index in 0..MAX_SOCIAL_LEDGER_EDGES {
            ledger.apply_impression(
                AffinityImpression {
                    peer_id: format!("agent-{index}"),
                    ..AffinityImpression::default()
                },
                fixed_now() + index as u64, // agent-0 is the stalest
            );
        }
        assert_eq!(ledger.edges.len(), MAX_SOCIAL_LEDGER_EDGES);

        ledger.apply_impression(
            AffinityImpression {
                peer_id: "agent-new".to_string(),
                ..AffinityImpression::default()
            },
            fixed_now() + 1000,
        );

        assert_eq!(ledger.edges.len(), MAX_SOCIAL_LEDGER_EDGES, "cap holds");
        assert!(ledger.edge_for("agent-0").is_none(), "stalest evicted");
        assert!(ledger.edge_for("agent-new").is_some(), "newest kept");
    }

    #[test]
    fn for_prompt_drops_expired_tabs_and_decays_toward_baseline() {
        let ledger = SessionSocialLedger {
            edges: vec![AffinityEdge {
                peer_id: "agent-peer".to_string(),
                display_name: "Peer".to_string(),
                trust: 0.0,
                warmth: 0.7,
                standing: AffinityStanding::Peer,
                note: None,
                open_tab: Some(AffinityOpenTab {
                    text: "old favor".to_string(),
                    dir: AffinityTabDir::OwedToMe,
                    expires_at_ms: 100,
                }),
                baseline_trust: 0.0,
                baseline_warmth: 0.1,
                updated_at_ms: 0,
            }],
        };

        let view = ledger.for_prompt(AFFINITY_DECAY_HALF_LIFE_MS);
        let edge = &view.edges[0];
        // one half-life: warmth relaxes halfway from 0.7 toward baseline 0.1 -> 0.4
        assert!((edge.warmth - 0.4).abs() < 1e-3, "warmth = {}", edge.warmth);
        assert!(edge.open_tab.is_none(), "expired tab dropped");
        // the stored ledger is left untouched by rendering
        assert!((ledger.edges[0].warmth - 0.7).abs() < 1e-6);
        assert!(ledger.edges[0].open_tab.is_some());
    }

    #[test]
    fn apply_impression_truncates_overlong_text() {
        let mut ledger = SessionSocialLedger::default();
        let edge = ledger.apply_impression(
            AffinityImpression {
                peer_id: "agent-p".to_string(),
                display_name: Some("N".repeat(500)),
                note: Some("x".repeat(1000)),
                tab: Some(AffinityTabInput {
                    text: "y".repeat(1000),
                    dir: AffinityTabDir::OwedToMe,
                }),
                ..AffinityImpression::default()
            },
            fixed_now(),
        );
        assert!(edge.display_name.chars().count() <= 80);
        assert!(edge.note.as_deref().unwrap().chars().count() <= 280);
        assert!(edge.open_tab.unwrap().text.chars().count() <= 280);
    }

    #[test]
    fn session_persona_binding_round_trips_through_metadata() {
        let binding = SessionPersonaBinding {
            persona_id: "persona-1".to_string(),
            persona_version: 3,
            display_name: "Reviewer".to_string(),
            soul: "Always review thoroughly.".to_string(),
            soul_sha256: "abc123".to_string(),
            capability_scope: CapabilityScope {
                skill_allow: vec!["review".to_string()],
                ..CapabilityScope::default()
            },
            default_inline_skills: Vec::new(),
            bound_at_ms: 42,
        };

        let metadata = metadata_with_session_persona_binding(
            json!({
                "custom": true,
            }),
            Some(&binding),
        )
        .expect("metadata update should succeed");

        assert_eq!(
            session_persona_binding_from_metadata(&metadata)
                .expect("metadata decode should succeed"),
            Some(binding)
        );
        assert_eq!(metadata.get("custom"), Some(&json!(true)));
    }

    #[test]
    fn metadata_with_session_persona_binding_clears_existing_binding() {
        let metadata = metadata_with_session_persona_binding(
            json!({
                "session_persona_binding": {
                    "persona_id": "persona-1",
                    "persona_version": 1,
                    "display_name": "Reviewer",
                    "soul": "Review code.",
                    "soul_sha256": "abc123",
                    "bound_at_ms": 1
                }
            }),
            None,
        )
        .expect("metadata update should succeed");

        assert!(
            session_persona_binding_from_metadata(&metadata)
                .expect("metadata decode should succeed")
                .is_none()
        );
    }

    #[test]
    fn session_persona_binding_decode_treats_null_as_absent() {
        let metadata = json!({
            "session_persona_binding": null,
            "custom": true,
        });

        assert_eq!(
            session_persona_binding_from_metadata(&metadata)
                .expect("metadata decode should succeed"),
            None
        );
    }

    #[test]
    fn session_capability_scope_round_trips_through_metadata() {
        let scope = CapabilityScope {
            skill_allow: vec!["review".to_string()],
            mcp_server_allow: vec!["openaiDeveloperDocs".to_string()],
            ..CapabilityScope::default()
        };
        let metadata = metadata_with_session_capability_scope(json!({"custom": true}), &scope)
            .expect("metadata update should succeed");

        assert_eq!(
            session_capability_scope_from_metadata(&metadata)
                .expect("metadata decode should succeed"),
            scope
        );
        assert_eq!(metadata.get("custom"), Some(&json!(true)));
    }
}

/// Represents the canonical in-memory state reconstructed from session events.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CanonicalStateSnapshot {
    /// Canonical conversation messages reconstructed from the journal.
    pub messages: Vec<MessageRecord>,
    /// Persisted user-message content parts keyed by canonical message identifier.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input_content_parts: BTreeMap<String, Vec<InputContentPart>>,
    /// Tool calls that were emitted but have not yet received a result.
    pub open_tool_calls: BTreeMap<String, ToolCallRecord>,
    /// Completed tool results replayed from the journal.
    pub completed_tool_results: Vec<ToolResultRecord>,
}

/// Captures one compaction checkpoint together with its canonical replay state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionCheckpoint {
    pub compacted_until_offset: u64,
    pub journal_digest: String,
    pub prompt_summary: SummaryBlock,
    /// Stable identifier for the active model-visible prompt window.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt_window_id: String,
    /// Monotonic generation incremented every time compaction replaces the prompt window.
    #[serde(default)]
    pub prompt_window_generation: u64,
    /// Provider continuation identifiers from entries at or before this offset belong to the
    /// previous provider-side context and must not be reused.
    #[serde(default)]
    pub prompt_window_started_after_offset: u64,
    /// Human-readable reason that created this prompt window.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt_window_created_by: String,
    #[serde(default)]
    pub compact_metadata: CompactBoundaryMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restoration: Option<PostCompactRestoration>,
    pub canonical: CanonicalStateSnapshot,
}

impl SessionCheckpoint {
    /// Returns the effective cutoff for provider continuation IDs.
    ///
    /// Older checkpoint files did not persist prompt-window metadata. For those
    /// checkpoints we fail closed and disable provider continuation until a new
    /// checkpoint is created by this version.
    pub fn effective_prompt_window_started_after_offset(&self) -> u64 {
        if self.prompt_window_generation == 0 && self.prompt_window_started_after_offset == 0 {
            u64::MAX
        } else {
            self.prompt_window_started_after_offset
        }
    }
}

/// Declares one append-only session event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    InputReceived { input: InputEnvelope },
    MessageAppended { message: MessageRecord },
    ToolCallStarted { call: ToolCallRecord },
    ToolCallFinished { result: ToolResultRecord },
    UserQuestionRequested { request: UserQuestionRequest },
    UserQuestionResolved { resolution: UserQuestionResolution },
    CompactionBoundary { boundary: CompactionBoundary },
}

/// Wraps one session event with its monotonic offset.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    pub offset: u64,
    #[serde(default)]
    pub timestamp_ms: u64,
    pub event: SessionEvent,
}

/// Tracks autocompact retries and progression for one in-flight run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutocompactTracking {
    /// The number of consecutive autocompact failures.
    pub consecutive_failures: u32,
    /// The last turn that compacted successfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compacted_turn: Option<u64>,
    /// The number of turns processed since the latest successful compaction.
    pub turn_counter: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptTrace {
    pub has_summary: bool,
    pub system_section_count: usize,
    pub message_count: usize,
    pub open_tool_call_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolExecutionTrace {
    pub call_id: String,
    pub tool_name: String,
    pub decision: PermissionDecision,
    pub result_is_error: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TurnTrace {
    pub turn: usize,
    pub prompt: PromptTrace,
    pub assistant_message_id: String,
    pub tool_executions: Vec<ToolExecutionTrace>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointTrace {
    pub compacted_until_offset: u64,
    pub journal_digest: String,
    pub summary_chars: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunTrace {
    pub turns: Vec<TurnTrace>,
    pub checkpoints: Vec<CheckpointTrace>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compaction_boundaries: Vec<CompactionBoundary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunPolicySnapshot {
    /// Maximum main-loop turn number. `0` means no hard turn ceiling.
    pub max_turns: usize,
    pub keep_last_messages: usize,
    pub snip_token_budget: usize,
    pub snip_keep_minimum_messages: usize,
    pub microcompact_keep_recent: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microcompact_idle_timeout_ms: Option<u64>,
    pub autocompact_threshold_tokens: usize,
    pub autocompact_buffer_tokens: usize,
    pub session_memory_min_tokens: usize,
    pub session_memory_max_tokens: usize,
}

impl Default for RunPolicySnapshot {
    fn default() -> Self {
        Self {
            max_turns: DEFAULT_AGENT_MAX_TURNS,
            keep_last_messages: 6,
            snip_token_budget: 120_000,
            snip_keep_minimum_messages: 10,
            microcompact_keep_recent: 5,
            microcompact_idle_timeout_ms: Some(15 * 60 * 1000),
            autocompact_threshold_tokens: 167_000,
            autocompact_buffer_tokens: 13_000,
            session_memory_min_tokens: 10_000,
            session_memory_max_tokens: 40_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMetaSnapshot {
    pub session_id: String,
    pub thread_id: Option<String>,
    pub input_source_plugin: String,
    pub input_source_kind: String,
    #[serde(default)]
    pub input_event_offset: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completion_requirements: Vec<CompletionRequirement>,
    #[serde(default)]
    pub completion_follow_up_count: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_contract: Option<crate::StructuredOutputContract>,
    #[serde(default)]
    pub output_contract_repair_count: u8,
    #[serde(default)]
    pub permission_denied_retry_count: u8,
    #[serde(default)]
    pub max_output_tokens_recovery_count: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovered_memory: Option<RecoveredMemoryBundle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learned_context: Option<LearnedContextBundle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_skills: Option<Vec<String>>,
    #[serde(default)]
    pub autocompact: AutocompactTracking,
    pub policy: RunPolicySnapshot,
    pub engine_version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptMessageSnapshot {
    pub id: String,
    pub role: Role,
    pub digest: String,
    pub content_digest: String,
}

/// Stores one digested system-prompt section visible to a turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemPromptSectionSnapshot {
    /// The stable section identifier.
    pub name: String,
    /// The digest of the full section record.
    pub digest: String,
    /// The digest of the raw section content.
    pub content_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSnapshot {
    pub checkpoint_offset_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_window_id: Option<String>,
    #[serde(default)]
    pub prompt_window_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_window_started_after_offset: Option<u64>,
    pub summary_digest: Option<String>,
    pub summary_text: Option<String>,
    pub system_sections: Vec<SystemPromptSectionSnapshot>,
    pub messages: Vec<PromptMessageSnapshot>,
    pub open_tool_call_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallSnapshot {
    pub call_id: String,
    pub tool_name: String,
    pub input_digest: String,
    pub input_canonical_json: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultSnapshot {
    pub call_id: String,
    pub decision: PermissionDecision,
    pub result_is_error: bool,
    pub result_digest: String,
    pub result_canonical_json: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TurnSnapshot {
    pub turn: usize,
    pub prompt: PromptSnapshot,
    pub assistant_message_id: String,
    pub assistant_message_digest: String,
    pub stop_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ModelUsage>,
    pub tool_calls: Vec<ToolCallSnapshot>,
    pub tool_results: Vec<ToolResultSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointSnapshot {
    pub compacted_until_offset: u64,
    pub journal_digest: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt_window_id: String,
    #[serde(default)]
    pub prompt_window_generation: u64,
    #[serde(default)]
    pub prompt_window_started_after_offset: u64,
    pub summary_digest: String,
    pub summary_text: String,
    pub canonical_state_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalStateSnapshot {
    pub messages: Vec<PromptMessageSnapshot>,
    pub message_digests: Vec<String>,
    pub message_roles: Vec<Role>,
    pub completed_tool_result_digests: Vec<String>,
    pub open_tool_call_ids: Vec<String>,
    pub canonical_state_digest: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub run_meta: RunMetaSnapshot,
    pub turns: Vec<TurnSnapshot>,
    pub checkpoints: Vec<CheckpointSnapshot>,
    pub final_state: FinalStateSnapshot,
}
