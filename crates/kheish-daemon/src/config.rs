//! Daemon configuration and output record types.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Result, bail};
use kheish_types::{AttachmentRef, ContentPart};
use serde::{Deserialize, Serialize};

use crate::SchedulerPolicyConfig;

pub const DEFAULT_EVENT_HISTORY_CAPACITY: usize = 2_048;

/// Built-in control-plane authentication settings for the daemon HTTP API.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlPlaneAuthConfig {
    /// Bearer token that grants full read/write access to `/v1/*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_token: Option<String>,
    /// Optional bearer token that grants read-only access to safe GET endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_token: Option<String>,
}

impl ControlPlaneAuthConfig {
    /// Returns a disabled auth configuration.
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Returns true when at least one bearer token is configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.admin_token.is_some() || self.read_only_token.is_some()
    }

    #[must_use]
    fn has_admin_token(&self) -> bool {
        self.admin_token.is_some()
    }

    #[must_use]
    fn has_read_only_token(&self) -> bool {
        self.read_only_token.is_some()
    }
}

/// Token-file sources for control-plane bearer auth.
///
/// When configured, the daemon reloads file-backed token digests during auth
/// decisions, allowing token rotation without restarting the process.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlPlaneAuthTokenFiles {
    /// File containing the full-access bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_token_file: Option<PathBuf>,
    /// File containing the optional read-only bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_token_file: Option<PathBuf>,
}

impl ControlPlaneAuthTokenFiles {
    /// Returns true when no token file sources are configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.admin_token_file.is_none() && self.read_only_token_file.is_none()
    }

    #[must_use]
    fn has_admin_token_file(&self) -> bool {
        self.admin_token_file.is_some()
    }

    #[must_use]
    fn has_read_only_token_file(&self) -> bool {
        self.read_only_token_file.is_some()
    }
}

/// Browser CORS policy for the daemon control-plane API.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlPlaneCorsConfig {
    /// Exact browser origins allowed to call `/v1/*`.
    ///
    /// An empty list preserves the default development policy: any HTTP(S)
    /// origin whose host is the local loopback machine is accepted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
}

impl ControlPlaneCorsConfig {
    /// Returns the default local-loopback browser policy.
    #[must_use]
    pub fn loopback() -> Self {
        Self::default()
    }

    /// Returns an exact allowlist policy.
    #[must_use]
    pub fn exact(allowed_origins: Vec<String>) -> Self {
        Self { allowed_origins }
    }

    /// Returns true when the default loopback policy is active.
    #[must_use]
    pub fn is_loopback_policy(&self) -> bool {
        self.allowed_origins.is_empty()
    }
}

/// Accepts browser origins that resolve to the same local operator machine.
#[must_use]
pub fn is_loopback_control_plane_origin(origin: &str) -> bool {
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    if authority.is_empty()
        || authority.contains('/')
        || authority.contains('?')
        || authority.contains('#')
    {
        return false;
    }

    let Some(host) = origin_host(authority) else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

fn origin_host(authority: &str) -> Option<&str> {
    if let Some(rest) = authority.strip_prefix('[') {
        let host_end = rest.find(']')?;
        let host = &rest[..host_end];
        let port = &rest[host_end + 1..];
        return valid_bracketed_optional_port(port).then_some(host);
    }

    if let Some((host, port)) = authority.split_once(':') {
        if host.is_empty() || host.contains(':') || !valid_port(port) {
            return None;
        }
        return Some(host);
    }

    if authority.is_empty() || authority.contains(':') {
        return None;
    }
    Some(authority)
}

fn valid_bracketed_optional_port(port: &str) -> bool {
    port.is_empty() || port.strip_prefix(':').is_some_and(valid_port)
}

fn valid_port(port: &str) -> bool {
    !port.is_empty() && port.chars().all(|ch| ch.is_ascii_digit())
}

/// Daemon-owned safety bounds for child-agent fan-out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentPolicyConfig {
    /// Maximum parent/child depth below one root agent.
    pub max_child_depth: usize,
    /// Maximum number of live direct children under one parent.
    pub max_live_children_per_parent: usize,
    /// Maximum number of live descendants below one root agent.
    pub max_live_descendants_per_root: usize,
    /// Maximum number of child spawns one daemon run may request.
    pub max_spawns_per_run: usize,
    /// Maximum number of live sidechain runtime handles across the daemon.
    #[serde(default = "default_max_live_sidechains_global")]
    pub max_live_sidechains_global: usize,
    /// Maximum number of live sidechain runtime handles below one root session.
    #[serde(default = "default_max_live_sidechains_per_session")]
    pub max_live_sidechains_per_session: usize,
    /// Rolling quota window used for durable spawn budgets.
    #[serde(default = "default_spawn_rate_window_ms")]
    pub spawn_rate_window_ms: u64,
    /// Maximum accepted child spawns per parent session in one quota window.
    #[serde(default = "default_max_spawns_per_session_window")]
    pub max_spawns_per_session_window: usize,
    /// Maximum accepted child spawns per sidechain profile in one quota window.
    #[serde(default = "default_max_spawns_per_profile_window")]
    pub max_spawns_per_profile_window: usize,
    /// Maximum accepted child spawns per project in one quota window.
    #[serde(default = "default_max_spawns_per_project_window")]
    pub max_spawns_per_project_window: usize,
    /// Maximum accepted child spawns across the daemon in one quota window.
    #[serde(default = "default_max_spawns_global_window")]
    pub max_spawns_global_window: usize,
    /// Maximum estimated input tokens for one child-spawn request.
    #[serde(default = "default_max_spawn_input_tokens_per_request")]
    pub max_spawn_input_tokens_per_request: u64,
    /// Maximum reserved output tokens for one child-spawn request.
    #[serde(default = "default_max_spawn_output_tokens_per_request")]
    pub max_spawn_output_tokens_per_request: u64,
    /// Maximum estimated spawn cost, in micro-USD, across one quota window.
    #[serde(default = "default_max_spawn_cost_microusd_per_window")]
    pub max_spawn_cost_microusd_per_window: u64,
    /// Maximum estimated child CPU budget, in milliseconds, across one quota window.
    #[serde(default = "default_max_spawn_cpu_ms_per_window")]
    pub max_spawn_cpu_ms_per_window: u64,
    /// Static cost estimate charged to the quota ledger for one accepted spawn.
    #[serde(default = "default_estimated_spawn_cost_microusd")]
    pub estimated_spawn_cost_microusd: u64,
    /// Static CPU estimate charged to the quota ledger for one accepted spawn.
    #[serde(default = "default_estimated_spawn_cpu_ms")]
    pub estimated_spawn_cpu_ms: u64,
    /// Ordered JSON policy rules. Later matching rules override earlier limits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<SubagentPolicyRule>,
}

impl Default for SubagentPolicyConfig {
    fn default() -> Self {
        Self {
            max_child_depth: 4,
            max_live_children_per_parent: 6,
            max_live_descendants_per_root: 24,
            max_spawns_per_run: 6,
            max_live_sidechains_global: default_max_live_sidechains_global(),
            max_live_sidechains_per_session: default_max_live_sidechains_per_session(),
            spawn_rate_window_ms: default_spawn_rate_window_ms(),
            max_spawns_per_session_window: default_max_spawns_per_session_window(),
            max_spawns_per_profile_window: default_max_spawns_per_profile_window(),
            max_spawns_per_project_window: default_max_spawns_per_project_window(),
            max_spawns_global_window: default_max_spawns_global_window(),
            max_spawn_input_tokens_per_request: default_max_spawn_input_tokens_per_request(),
            max_spawn_output_tokens_per_request: default_max_spawn_output_tokens_per_request(),
            max_spawn_cost_microusd_per_window: default_max_spawn_cost_microusd_per_window(),
            max_spawn_cpu_ms_per_window: default_max_spawn_cpu_ms_per_window(),
            estimated_spawn_cost_microusd: default_estimated_spawn_cost_microusd(),
            estimated_spawn_cpu_ms: default_estimated_spawn_cpu_ms(),
            rules: Vec::new(),
        }
    }
}

fn default_max_live_sidechains_global() -> usize {
    128
}

fn default_max_live_sidechains_per_session() -> usize {
    24
}

fn default_spawn_rate_window_ms() -> u64 {
    60_000
}

fn default_max_spawns_per_session_window() -> usize {
    60
}
fn default_model_budget_max_total_output_tokens() -> u64 {
    1_000_000
}

fn default_model_budget_max_total_cost_usd() -> f64 {
    500.0
}

fn default_max_spawns_per_profile_window() -> usize {
    60
}

fn default_max_spawns_per_project_window() -> usize {
    120
}

fn default_max_spawns_global_window() -> usize {
    240
}

fn default_max_spawn_input_tokens_per_request() -> u64 {
    128_000
}

fn default_max_spawn_output_tokens_per_request() -> u64 {
    64_000
}

fn default_max_spawn_cost_microusd_per_window() -> u64 {
    1_000_000
}

fn default_max_spawn_cpu_ms_per_window() -> u64 {
    240_000
}

fn default_estimated_spawn_cost_microusd() -> u64 {
    1_000
}

fn default_estimated_spawn_cpu_ms() -> u64 {
    1_000
}

/// Effective child-agent policy limits after applying matching rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentPolicyLimits {
    pub max_child_depth: usize,
    pub max_live_children_per_parent: usize,
    pub max_live_descendants_per_root: usize,
    pub max_spawns_per_run: usize,
    pub max_live_sidechains_global: usize,
    pub max_live_sidechains_per_session: usize,
    pub spawn_rate_window_ms: u64,
    pub max_spawns_per_session_window: usize,
    pub max_spawns_per_profile_window: usize,
    pub max_spawns_per_project_window: usize,
    pub max_spawns_global_window: usize,
    pub max_spawn_input_tokens_per_request: u64,
    pub max_spawn_output_tokens_per_request: u64,
    pub max_spawn_cost_microusd_per_window: u64,
    pub max_spawn_cpu_ms_per_window: u64,
    pub estimated_spawn_cost_microusd: u64,
    pub estimated_spawn_cpu_ms: u64,
}

/// Optional override fields used by one JSON subagent-policy rule.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentPolicyLimitPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_child_depth: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_live_children_per_parent: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_live_descendants_per_root: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_spawns_per_run: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_live_sidechains_global: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_live_sidechains_per_session: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_rate_window_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_spawns_per_session_window: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_spawns_per_profile_window: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_spawns_per_project_window: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_spawns_global_window: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_spawn_input_tokens_per_request: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_spawn_output_tokens_per_request: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_spawn_cost_microusd_per_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_spawn_cpu_ms_per_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_spawn_cost_microusd: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_spawn_cpu_ms: Option<u64>,
}

/// Selector for one JSON subagent-policy rule.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentPolicySelector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// One ordered subagent-policy rule loaded from JSON.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentPolicyRule {
    #[serde(default)]
    pub selector: SubagentPolicySelector,
    #[serde(default)]
    pub limits: SubagentPolicyLimitPatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Scope values used to evaluate and explain one child-spawn policy decision.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentPolicyScopeView {
    pub parent_agent_id: String,
    pub root_agent_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_ids: Vec<String>,
}

/// Estimated resources reserved before accepting a child-spawn request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentPolicyEstimateView {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_microusd: u64,
    pub cpu_ms: u64,
}

/// One quota or live-limit usage row in policy status and explain output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentPolicyUsageView {
    pub scope: String,
    pub used: u64,
    pub limit: u64,
    pub remaining: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_ms: Option<u64>,
}

/// Active in-process spawn reservations.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentReservationStatusView {
    pub active_by_parent: BTreeMap<String, usize>,
    pub active_by_root: BTreeMap<String, usize>,
    pub active_by_session: BTreeMap<String, usize>,
    pub active_global: usize,
    pub in_flight_request_count: usize,
    pub in_flight_conversation_count: usize,
}

/// Policy decision for one sidechain spawn, including dry-run explanations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentPolicyDecisionView {
    pub allowed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub dry_run: bool,
    pub idempotent_replay: bool,
    pub scopes: SubagentPolicyScopeView,
    pub limits: SubagentPolicyLimits,
    pub estimate: SubagentPolicyEstimateView,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usage: Vec<SubagentPolicyUsageView>,
}

/// Operator-visible spawn-policy status snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentPolicyStatusView {
    pub policy: SubagentPolicyConfig,
    pub reservations: SubagentReservationStatusView,
    pub active_quota_entries: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub denied_by_reason: BTreeMap<String, u64>,
    #[serde(default)]
    pub idempotent_replays: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usage: Vec<SubagentPolicyUsageView>,
}

impl SubagentPolicyConfig {
    /// Returns the base limits before scoped rules are applied.
    pub fn base_limits(&self) -> SubagentPolicyLimits {
        SubagentPolicyLimits {
            max_child_depth: self.max_child_depth,
            max_live_children_per_parent: self.max_live_children_per_parent,
            max_live_descendants_per_root: self.max_live_descendants_per_root,
            max_spawns_per_run: self.max_spawns_per_run,
            max_live_sidechains_global: self.max_live_sidechains_global,
            max_live_sidechains_per_session: self.max_live_sidechains_per_session,
            spawn_rate_window_ms: self.spawn_rate_window_ms,
            max_spawns_per_session_window: self.max_spawns_per_session_window,
            max_spawns_per_profile_window: self.max_spawns_per_profile_window,
            max_spawns_per_project_window: self.max_spawns_per_project_window,
            max_spawns_global_window: self.max_spawns_global_window,
            max_spawn_input_tokens_per_request: self.max_spawn_input_tokens_per_request,
            max_spawn_output_tokens_per_request: self.max_spawn_output_tokens_per_request,
            max_spawn_cost_microusd_per_window: self.max_spawn_cost_microusd_per_window,
            max_spawn_cpu_ms_per_window: self.max_spawn_cpu_ms_per_window,
            estimated_spawn_cost_microusd: self.estimated_spawn_cost_microusd,
            estimated_spawn_cpu_ms: self.estimated_spawn_cpu_ms,
        }
    }

    /// Applies matching JSON policy rules in order and returns the effective limits.
    pub fn effective_limits(&self, scope: &SubagentPolicyScopeView) -> SubagentPolicyLimits {
        let mut limits = self.base_limits();
        for rule in &self.rules {
            if rule.selector.matches(scope) {
                rule.limits.apply_to(&mut limits);
            }
        }
        limits
    }

    /// Returns the largest quota window that can keep a durable entry relevant.
    pub fn max_quota_window_ms(&self) -> u64 {
        self.rules
            .iter()
            .filter_map(|rule| rule.limits.spawn_rate_window_ms)
            .fold(self.spawn_rate_window_ms, u64::max)
    }
}

impl SubagentPolicyLimitPatch {
    fn apply_to(&self, limits: &mut SubagentPolicyLimits) {
        if let Some(value) = self.max_child_depth {
            limits.max_child_depth = value;
        }
        if let Some(value) = self.max_live_children_per_parent {
            limits.max_live_children_per_parent = value;
        }
        if let Some(value) = self.max_live_descendants_per_root {
            limits.max_live_descendants_per_root = value;
        }
        if let Some(value) = self.max_spawns_per_run {
            limits.max_spawns_per_run = value;
        }
        if let Some(value) = self.max_live_sidechains_global {
            limits.max_live_sidechains_global = value;
        }
        if let Some(value) = self.max_live_sidechains_per_session {
            limits.max_live_sidechains_per_session = value;
        }
        if let Some(value) = self.spawn_rate_window_ms {
            limits.spawn_rate_window_ms = value;
        }
        if let Some(value) = self.max_spawns_per_session_window {
            limits.max_spawns_per_session_window = value;
        }
        if let Some(value) = self.max_spawns_per_profile_window {
            limits.max_spawns_per_profile_window = value;
        }
        if let Some(value) = self.max_spawns_per_project_window {
            limits.max_spawns_per_project_window = value;
        }
        if let Some(value) = self.max_spawns_global_window {
            limits.max_spawns_global_window = value;
        }
        if let Some(value) = self.max_spawn_input_tokens_per_request {
            limits.max_spawn_input_tokens_per_request = value;
        }
        if let Some(value) = self.max_spawn_output_tokens_per_request {
            limits.max_spawn_output_tokens_per_request = value;
        }
        if let Some(value) = self.max_spawn_cost_microusd_per_window {
            limits.max_spawn_cost_microusd_per_window = value;
        }
        if let Some(value) = self.max_spawn_cpu_ms_per_window {
            limits.max_spawn_cpu_ms_per_window = value;
        }
        if let Some(value) = self.estimated_spawn_cost_microusd {
            limits.estimated_spawn_cost_microusd = value;
        }
        if let Some(value) = self.estimated_spawn_cpu_ms {
            limits.estimated_spawn_cpu_ms = value;
        }
    }
}

impl SubagentPolicySelector {
    fn matches(&self, scope: &SubagentPolicyScopeView) -> bool {
        self.parent_agent_id
            .as_deref()
            .is_none_or(|value| value == scope.parent_agent_id)
            && self
                .session_id
                .as_deref()
                .is_none_or(|value| value == scope.session_id)
            && self
                .profile
                .as_deref()
                .is_none_or(|value| scope.profile.as_deref() == Some(value))
            && self
                .project_id
                .as_deref()
                .is_none_or(|value| scope.project_ids.iter().any(|project| project == value))
    }
}

fn default_true() -> bool {
    true
}

/// Runtime configuration for the Kheish daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// The address to bind.
    pub bind: SocketAddr,
    /// The filesystem root used for daemon state and session persistence.
    pub state_root: PathBuf,
    /// The workspace root exposed to default coding tools.
    pub workspace_root: PathBuf,
    /// Optional Codex-compatible MCP config path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_config_path: Option<PathBuf>,
    /// Optional Codex-compatible MCP credentials path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_credentials_path: Option<PathBuf>,
    /// Built-in MCP catalog profiles selected for this daemon.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_catalog_profiles: Vec<String>,
    /// Optional connector configuration file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connectors_config_path: Option<PathBuf>,
    /// Optional explicit skill roots loaded in precedence order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_roots: Vec<PathBuf>,
    /// Built-in bearer auth for the private control-plane API.
    #[serde(default, skip_serializing_if = "ControlPlaneAuthConfig::is_enabled")]
    pub control_plane_auth: ControlPlaneAuthConfig,
    /// File-backed bearer-token sources used for live token rotation.
    #[serde(default, skip_serializing_if = "ControlPlaneAuthTokenFiles::is_empty")]
    pub control_plane_auth_token_files: ControlPlaneAuthTokenFiles,
    /// Browser CORS policy for the private control-plane API.
    #[serde(
        default,
        skip_serializing_if = "ControlPlaneCorsConfig::is_loopback_policy"
    )]
    pub control_plane_cors: ControlPlaneCorsConfig,
    /// Whether the serving process acquired the daemon state-root lock.
    #[serde(skip)]
    pub state_root_lock_held: bool,
    /// Whether the daemon may boot with no model routes (onboarding `up` mode).
    ///
    /// A per-boot toggle, not durable configuration, so it is never serialized.
    #[serde(skip)]
    pub allow_empty_routes: bool,
    /// Daemon-owned child-agent bounds.
    #[serde(default)]
    pub subagent_policy: SubagentPolicyConfig,
    /// Daemon scheduler retry/backoff behavior.
    #[serde(default)]
    pub scheduler_policy: SchedulerPolicyConfig,
    /// Whether the background scheduler worker dispatches due schedules.
    #[serde(default = "default_true")]
    pub scheduler_enabled: bool,
    /// Number of daemon events retained for SSE replay and live backpressure buffering.
    #[serde(default = "default_event_history_capacity")]
    pub event_history_capacity: usize,
    /// Maximum cumulative output tokens one model runtime may emit.
    #[serde(default = "default_model_budget_max_total_output_tokens")]
    pub model_budget_max_total_output_tokens: u64,
    /// Maximum cumulative model cost in USD one model runtime may incur.
    #[serde(default = "default_model_budget_max_total_cost_usd")]
    pub model_budget_max_total_cost_usd: f64,
}

impl DaemonConfig {
    /// Creates a new daemon configuration.
    pub fn new(
        bind: SocketAddr,
        state_root: impl Into<PathBuf>,
        workspace_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            bind,
            state_root: state_root.into(),
            workspace_root: workspace_root.into(),
            mcp_config_path: None,
            mcp_credentials_path: None,
            mcp_catalog_profiles: Vec::new(),
            connectors_config_path: None,
            skill_roots: Vec::new(),
            control_plane_auth: ControlPlaneAuthConfig::disabled(),
            control_plane_auth_token_files: ControlPlaneAuthTokenFiles::default(),
            control_plane_cors: ControlPlaneCorsConfig::loopback(),
            state_root_lock_held: false,
            allow_empty_routes: false,
            subagent_policy: SubagentPolicyConfig::default(),
            scheduler_policy: SchedulerPolicyConfig::default(),
            scheduler_enabled: true,
            event_history_capacity: default_event_history_capacity(),
            model_budget_max_total_output_tokens: default_model_budget_max_total_output_tokens(),
            model_budget_max_total_cost_usd: default_model_budget_max_total_cost_usd(),
        }
    }

    /// Validates daemon-level safety invariants for the HTTP control plane.
    pub fn validate_control_plane_boundary(&self) -> Result<()> {
        let has_admin_token = self.control_plane_auth.has_admin_token()
            || self.control_plane_auth_token_files.has_admin_token_file();
        let has_read_only_token = self.control_plane_auth.has_read_only_token()
            || self
                .control_plane_auth_token_files
                .has_read_only_token_file();

        if has_read_only_token && !has_admin_token {
            bail!("control-plane read-only auth requires an admin bearer token source");
        }
        if !self.bind.ip().is_loopback() && !has_admin_token {
            bail!(
                "refusing to expose daemon control-plane on non-loopback bind {} without an admin bearer token source",
                self.bind
            );
        }
        if matches!(
            (
                self.control_plane_auth.admin_token.as_deref(),
                self.control_plane_auth.read_only_token.as_deref()
            ),
            (Some(admin_token), Some(read_only_token)) if admin_token == read_only_token
        ) {
            bail!(
                "control-plane admin and read-only bearer tokens must be distinct; identical tokens make authorization ambiguous"
            );
        }
        if matches!(
            (
                self.control_plane_auth_token_files.admin_token_file.as_ref(),
                self.control_plane_auth_token_files
                    .read_only_token_file
                    .as_ref()
            ),
            (Some(admin_file), Some(read_only_file)) if admin_file == read_only_file
        ) {
            bail!(
                "control-plane admin and read-only token files must be distinct; identical files make authorization ambiguous"
            );
        }
        for origin in &self.control_plane_cors.allowed_origins {
            if !is_loopback_control_plane_origin(origin) {
                bail!(
                    "control-plane CORS allowed origin must be an exact http(s) loopback origin without a path: {origin}"
                );
            }
        }
        Ok(())
    }
}

#[must_use]
pub const fn default_event_history_capacity() -> usize {
    DEFAULT_EVENT_HISTORY_CAPACITY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_plane_boundary_rejects_non_loopback_without_admin_source() {
        let config = DaemonConfig::new(
            "0.0.0.0:4000".parse::<SocketAddr>().expect("bind"),
            "state",
            "workspace",
        );
        let error = config
            .validate_control_plane_boundary()
            .expect_err("non-loopback without admin auth should fail");
        assert!(error.to_string().contains("non-loopback bind"));
    }

    #[test]
    fn control_plane_boundary_rejects_read_only_without_admin_source() {
        let mut config = DaemonConfig::new(
            "127.0.0.1:4000".parse::<SocketAddr>().expect("bind"),
            "state",
            "workspace",
        );
        config.control_plane_auth.read_only_token = Some("readonly-secret".to_string());
        let error = config
            .validate_control_plane_boundary()
            .expect_err("read-only without admin should fail");
        assert!(error.to_string().contains("requires an admin bearer token"));
    }

    #[test]
    fn control_plane_boundary_rejects_ambiguous_admin_and_read_only_tokens() {
        let mut config = DaemonConfig::new(
            "127.0.0.1:4000".parse::<SocketAddr>().expect("bind"),
            "state",
            "workspace",
        );
        config.control_plane_auth.admin_token = Some("same-secret".to_string());
        config.control_plane_auth.read_only_token = Some("same-secret".to_string());
        let error = config
            .validate_control_plane_boundary()
            .expect_err("duplicate tokens should fail");
        assert!(error.to_string().contains("must be distinct"));
    }

    #[test]
    fn control_plane_boundary_rejects_unsafe_cors_origin() {
        let mut config = DaemonConfig::new(
            "127.0.0.1:4000".parse::<SocketAddr>().expect("bind"),
            "state",
            "workspace",
        );
        config.control_plane_cors =
            ControlPlaneCorsConfig::exact(vec!["https://example.com".to_string()]);
        let error = config
            .validate_control_plane_boundary()
            .expect_err("unsafe origin should fail");
        assert!(error.to_string().contains("CORS allowed origin"));
    }
}

/// One output record captured by the daemon output plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonOutputSourceKind {
    /// The output came from the model's plain assistant text fallback.
    AssistantText,
    /// The output came from an explicit `emit_output` tool call.
    EmitOutput,
    /// The output was emitted directly by daemon control-plane code.
    DaemonEmitOutput,
    /// The output is a model-requested notification to the configured operator.
    OperatorNotification,
}

impl DaemonOutputSourceKind {
    /// Decodes one runtime metadata string into a durable output source kind.
    pub fn from_metadata_str(value: &str) -> Option<Self> {
        match value {
            "assistant_text" => Some(Self::AssistantText),
            "emit_output" => Some(Self::EmitOutput),
            "daemon_emit_output" => Some(Self::DaemonEmitOutput),
            "operator_notification" => Some(Self::OperatorNotification),
            _ => None,
        }
    }
}

/// One output record captured by the daemon output plugin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonOutputRecord {
    /// The destination session identifier.
    pub session_id: String,
    /// The detached run identifier when this output belongs to one background run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The output content.
    pub content: String,
    /// The ordered rich output parts shown inline to the user.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<ContentPart>,
    /// Additional generated assets associated with the output but not shown inline.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<AttachmentRef>,
    /// The logical source of the visible output payload when the daemon knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<DaemonOutputSourceKind>,
    /// The optional output plugin that received this dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// The optional routing address.
    pub address: Option<String>,
}
