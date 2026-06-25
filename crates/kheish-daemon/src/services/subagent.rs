use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;

use anyhow::{Result, anyhow};
use kheish_session::write_json_pretty_atomically;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::warn;

use kheish_agent::{AgentId, AgentSupervisor};

use crate::runs::now_ms;
use crate::state_files::read_json_or_quarantine;
use crate::{
    SubagentPolicyConfig, SubagentPolicyDecisionView, SubagentPolicyEstimateView,
    SubagentPolicyScopeView, SubagentPolicyStatusView, SubagentPolicyUsageView,
    SubagentReservationStatusView,
};

#[derive(Default)]
struct SpawnReservationState {
    by_parent: BTreeMap<String, usize>,
    by_root: BTreeMap<String, usize>,
    by_session: BTreeMap<String, usize>,
    global_live: usize,
    by_run: BTreeMap<String, usize>,
    by_request: BTreeSet<String>,
    by_conversation: BTreeSet<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct SpawnPolicyLedger {
    #[serde(default)]
    entries: Vec<SpawnPolicyLedgerEntry>,
    #[serde(default)]
    denied_by_reason: BTreeMap<String, u64>,
    #[serde(default)]
    idempotent_replays: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct SpawnPolicyLedgerEntry {
    key: String,
    parent_agent_id: String,
    root_agent_id: String,
    session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    project_ids: Vec<String>,
    request_fingerprint_hash: String,
    charged_at_ms: u64,
    input_tokens: u64,
    output_tokens: u64,
    cost_microusd: u64,
    cpu_ms: u64,
}

/// Tracks one in-flight spawn reservation until the sidechain creation attempt settles.
#[derive(Debug)]
pub(crate) struct SpawnReservation {
    parent_id: String,
    root_id: String,
    session_id: Option<String>,
    run_id: Option<String>,
    request_key: Option<String>,
    conversation_key: String,
    counted_towards_limits: bool,
}

/// Tracks one in-flight idempotent spawn request before its child conversation is known.
#[derive(Debug)]
pub(crate) struct SpawnRequestReservation {
    request_key: String,
}

/// Owns daemon-local subagent coordination state such as spawn reservations and mailbox run gates.
pub(crate) struct SubagentService {
    mailbox_scheduler: Mutex<BTreeSet<String>>,
    spawn_reservations: StdMutex<SpawnReservationState>,
    spawn_policy_ledger_path: Option<PathBuf>,
    spawn_policy_ledger: StdMutex<SpawnPolicyLedger>,
}

impl SubagentService {
    /// Creates a new subagent service with empty reservation state and a durable quota ledger.
    pub(crate) fn new(state_root: PathBuf) -> Self {
        let ledger_path = state_root.join("spawn-policy-ledger.json");
        let ledger = load_spawn_policy_ledger(&ledger_path).unwrap_or_else(|error| {
            warn!(
                error = %error,
                path = %ledger_path.display(),
                "failed to load subagent spawn-policy ledger; starting with an empty ledger"
            );
            SpawnPolicyLedger::default()
        });
        Self {
            mailbox_scheduler: Mutex::new(BTreeSet::new()),
            spawn_reservations: StdMutex::new(SpawnReservationState::default()),
            spawn_policy_ledger_path: Some(ledger_path),
            spawn_policy_ledger: StdMutex::new(ledger),
        }
    }

    #[cfg(test)]
    fn new_ephemeral() -> Self {
        Self {
            mailbox_scheduler: Mutex::new(BTreeSet::new()),
            spawn_reservations: StdMutex::new(SpawnReservationState::default()),
            spawn_policy_ledger_path: None,
            spawn_policy_ledger: StdMutex::new(SpawnPolicyLedger::default()),
        }
    }

    /// Clears tracked per-run spawn counts after the owning run settles.
    pub(crate) fn clear_run_spawn_count(&self, run_id: &str) {
        self.spawn_reservations
            .lock()
            .expect("spawn reservation mutex poisoned")
            .by_run
            .remove(run_id);
    }

    /// Attempts to reserve one child spawn under the configured daemon policy.
    pub(crate) fn reserve_subagent_spawn<F, G>(
        &self,
        supervisor: &AgentSupervisor,
        parent: &AgentId,
        effective_run_id: Option<&str>,
        request_key: Option<&str>,
        conversation_key: &str,
        enforce_policy_limits: bool,
        policy: &SubagentPolicyConfig,
        has_runtime: F,
        count_spawned_children_for_run: G,
    ) -> Result<SpawnReservation>
    where
        F: Fn(&AgentId) -> bool,
        G: Fn(&str) -> usize,
    {
        self.reserve_subagent_spawn_inner(
            supervisor,
            parent,
            effective_run_id,
            request_key,
            conversation_key,
            enforce_policy_limits,
            policy,
            None,
            None,
            None,
            false,
            has_runtime,
            count_spawned_children_for_run,
        )
    }

    /// Attempts to reserve one child spawn with full scoped policy and durable quotas.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reserve_subagent_spawn_with_policy<F, G>(
        &self,
        supervisor: &AgentSupervisor,
        parent: &AgentId,
        effective_run_id: Option<&str>,
        request_key: Option<&str>,
        conversation_key: &str,
        enforce_policy_limits: bool,
        policy: &SubagentPolicyConfig,
        policy_scope: &SubagentPolicyScopeView,
        estimate: &SubagentPolicyEstimateView,
        request_fingerprint: &str,
        has_runtime: F,
        count_spawned_children_for_run: G,
    ) -> Result<SpawnReservation>
    where
        F: Fn(&AgentId) -> bool,
        G: Fn(&str) -> usize,
    {
        self.reserve_subagent_spawn_inner(
            supervisor,
            parent,
            effective_run_id,
            request_key,
            conversation_key,
            enforce_policy_limits,
            policy,
            Some(policy_scope),
            Some(estimate),
            Some(request_fingerprint),
            false,
            has_runtime,
            count_spawned_children_for_run,
        )
    }

    /// Reserves one spawn request id before its conversation key is known.
    pub(crate) fn reserve_spawn_request(
        &self,
        request_key: &str,
    ) -> Result<SpawnRequestReservation> {
        let mut reservations = self
            .spawn_reservations
            .lock()
            .expect("spawn reservation mutex poisoned");
        anyhow::ensure!(
            !reservations.by_request.contains(request_key),
            "spawn request {request_key} is already in progress"
        );
        reservations.by_request.insert(request_key.to_string());
        Ok(SpawnRequestReservation {
            request_key: request_key.to_string(),
        })
    }

    /// Releases one previously acquired request-id reservation.
    pub(crate) fn release_spawn_request_reservation(&self, reservation: SpawnRequestReservation) {
        self.spawn_reservations
            .lock()
            .expect("spawn reservation mutex poisoned")
            .by_request
            .remove(&reservation.request_key);
    }

    /// Attempts to reserve one child spawn while the request key is already held by the caller.
    pub(crate) fn reserve_subagent_spawn_with_held_request<F, G>(
        &self,
        supervisor: &AgentSupervisor,
        parent: &AgentId,
        effective_run_id: Option<&str>,
        request_key: &str,
        conversation_key: &str,
        enforce_policy_limits: bool,
        policy: &SubagentPolicyConfig,
        has_runtime: F,
        count_spawned_children_for_run: G,
    ) -> Result<SpawnReservation>
    where
        F: Fn(&AgentId) -> bool,
        G: Fn(&str) -> usize,
    {
        self.reserve_subagent_spawn_inner(
            supervisor,
            parent,
            effective_run_id,
            Some(request_key),
            conversation_key,
            enforce_policy_limits,
            policy,
            None,
            None,
            None,
            true,
            has_runtime,
            count_spawned_children_for_run,
        )
    }

    /// Attempts to reserve one scoped child spawn while the request key is already held.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reserve_subagent_spawn_with_policy_and_held_request<F, G>(
        &self,
        supervisor: &AgentSupervisor,
        parent: &AgentId,
        effective_run_id: Option<&str>,
        request_key: &str,
        conversation_key: &str,
        enforce_policy_limits: bool,
        policy: &SubagentPolicyConfig,
        policy_scope: &SubagentPolicyScopeView,
        estimate: &SubagentPolicyEstimateView,
        request_fingerprint: &str,
        has_runtime: F,
        count_spawned_children_for_run: G,
    ) -> Result<SpawnReservation>
    where
        F: Fn(&AgentId) -> bool,
        G: Fn(&str) -> usize,
    {
        self.reserve_subagent_spawn_inner(
            supervisor,
            parent,
            effective_run_id,
            Some(request_key),
            conversation_key,
            enforce_policy_limits,
            policy,
            Some(policy_scope),
            Some(estimate),
            Some(request_fingerprint),
            true,
            has_runtime,
            count_spawned_children_for_run,
        )
    }

    /// Explains one child-spawn policy decision without reserving or charging quota.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn explain_subagent_spawn_policy<F, G>(
        &self,
        supervisor: &AgentSupervisor,
        parent: &AgentId,
        effective_run_id: Option<&str>,
        request_key: Option<&str>,
        conversation_key: &str,
        policy: &SubagentPolicyConfig,
        policy_scope: &SubagentPolicyScopeView,
        estimate: &SubagentPolicyEstimateView,
        request_fingerprint: &str,
        idempotent_replay: bool,
        has_runtime: F,
        count_spawned_children_for_run: G,
    ) -> SubagentPolicyDecisionView
    where
        F: Fn(&AgentId) -> bool,
        G: Fn(&str) -> usize,
    {
        let mut reservations = self
            .spawn_reservations
            .lock()
            .expect("spawn reservation mutex poisoned");
        self.evaluate_subagent_spawn_policy(
            supervisor,
            parent,
            effective_run_id,
            request_key,
            conversation_key,
            policy,
            policy_scope,
            estimate,
            request_fingerprint,
            idempotent_replay,
            false,
            &mut reservations,
            has_runtime,
            count_spawned_children_for_run,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn reserve_subagent_spawn_inner<F, G>(
        &self,
        supervisor: &AgentSupervisor,
        parent: &AgentId,
        effective_run_id: Option<&str>,
        request_key: Option<&str>,
        conversation_key: &str,
        enforce_policy_limits: bool,
        policy: &SubagentPolicyConfig,
        policy_scope: Option<&SubagentPolicyScopeView>,
        estimate: Option<&SubagentPolicyEstimateView>,
        request_fingerprint: Option<&str>,
        request_key_already_reserved: bool,
        has_runtime: F,
        count_spawned_children_for_run: G,
    ) -> Result<SpawnReservation>
    where
        F: Fn(&AgentId) -> bool,
        G: Fn(&str) -> usize,
    {
        let root = supervisor
            .root_of(parent)
            .ok_or_else(|| anyhow!("unknown agent {}", parent.0))?;
        let mut reservations = self
            .spawn_reservations
            .lock()
            .expect("spawn reservation mutex poisoned");

        if let Some(request_key) = request_key {
            if request_key_already_reserved {
                anyhow::ensure!(
                    reservations.by_request.contains(request_key),
                    "spawn request {request_key} reservation is missing"
                );
            } else {
                anyhow::ensure!(
                    !reservations.by_request.contains(request_key),
                    "spawn request {request_key} is already in progress"
                );
            }
        }
        anyhow::ensure!(
            !reservations.by_conversation.contains(conversation_key),
            "spawn conversation {conversation_key} is already in progress"
        );

        if enforce_policy_limits {
            let fallback_scope;
            let policy_scope = if let Some(policy_scope) = policy_scope {
                policy_scope
            } else {
                fallback_scope = SubagentPolicyScopeView {
                    parent_agent_id: parent.0.clone(),
                    root_agent_id: root.id.0.clone(),
                    session_id: root.conversation.session_id.clone(),
                    profile: None,
                    project_ids: Vec::new(),
                };
                &fallback_scope
            };
            let fallback_estimate;
            let estimate = if let Some(estimate) = estimate {
                estimate
            } else {
                fallback_estimate = SubagentPolicyEstimateView::default();
                &fallback_estimate
            };
            let request_fingerprint = request_fingerprint.unwrap_or(conversation_key);
            let decision = self.evaluate_subagent_spawn_policy(
                supervisor,
                parent,
                effective_run_id,
                request_key,
                conversation_key,
                policy,
                policy_scope,
                estimate,
                request_fingerprint,
                false,
                true,
                &mut reservations,
                &has_runtime,
                &count_spawned_children_for_run,
            );
            if !decision.allowed {
                self.record_spawn_policy_denial(decision.reason_code.as_deref());
                anyhow::bail!(
                    "subagent spawn policy denied: {}",
                    decision
                        .reason
                        .unwrap_or_else(|| "spawn policy rejected the request".to_string())
                );
            }
        }

        if let Some(request_key) = request_key
            && !request_key_already_reserved
        {
            reservations.by_request.insert(request_key.to_string());
        }
        reservations
            .by_conversation
            .insert(conversation_key.to_string());
        if enforce_policy_limits {
            *reservations.by_parent.entry(parent.0.clone()).or_insert(0) += 1;
            *reservations.by_root.entry(root.id.0.clone()).or_insert(0) += 1;
            *reservations
                .by_session
                .entry(
                    policy_scope
                        .map(|scope| scope.session_id.clone())
                        .unwrap_or_else(|| root.conversation.session_id.clone()),
                )
                .or_insert(0) += 1;
            reservations.global_live += 1;
            if let Some(run_id) = effective_run_id {
                *reservations.by_run.entry(run_id.to_string()).or_insert(0) += 1;
            }
        }

        Ok(SpawnReservation {
            parent_id: parent.0.clone(),
            root_id: root.id.0.clone(),
            session_id: enforce_policy_limits.then(|| {
                policy_scope
                    .map(|scope| scope.session_id.clone())
                    .unwrap_or_else(|| root.conversation.session_id.clone())
            }),
            run_id: enforce_policy_limits
                .then(|| effective_run_id.map(str::to_string))
                .flatten(),
            request_key: (!request_key_already_reserved)
                .then(|| request_key.map(str::to_string))
                .flatten(),
            conversation_key: conversation_key.to_string(),
            counted_towards_limits: enforce_policy_limits,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_subagent_spawn_policy<F, G>(
        &self,
        supervisor: &AgentSupervisor,
        parent: &AgentId,
        effective_run_id: Option<&str>,
        request_key: Option<&str>,
        _conversation_key: &str,
        policy: &SubagentPolicyConfig,
        scope: &SubagentPolicyScopeView,
        estimate: &SubagentPolicyEstimateView,
        request_fingerprint: &str,
        idempotent_replay: bool,
        charge_quota: bool,
        reservations: &mut SpawnReservationState,
        has_runtime: F,
        count_spawned_children_for_run: G,
    ) -> SubagentPolicyDecisionView
    where
        F: Fn(&AgentId) -> bool,
        G: Fn(&str) -> usize,
    {
        let limits = policy.effective_limits(scope);
        let child_depth = supervisor.depth_of(parent).unwrap_or(0) + 1;
        let live_children = supervisor
            .children_of(parent)
            .into_iter()
            .filter(|record| has_runtime(&record.id))
            .count()
            + reservations.by_parent.get(&parent.0).copied().unwrap_or(0);
        let live_descendants = supervisor
            .descendants_of(&AgentId(scope.root_agent_id.clone()))
            .into_iter()
            .filter(|record| has_runtime(&record.id))
            .count()
            + reservations
                .by_root
                .get(&scope.root_agent_id)
                .copied()
                .unwrap_or(0);
        let live_global =
            live_global_sidechains(supervisor, &has_runtime) + reservations.global_live;
        let live_session = live_session_sidechains(supervisor, &scope.session_id, &has_runtime)
            + reservations
                .by_session
                .get(&scope.session_id)
                .copied()
                .unwrap_or(0);
        let run_spawn_count = effective_run_id.map(|run_id| {
            count_spawned_children_for_run(run_id)
                + reservations.by_run.get(run_id).copied().unwrap_or(0)
        });
        let mut usage = vec![
            usage_view(
                "child_depth",
                child_depth as u64,
                limits.max_child_depth as u64,
                None,
            ),
            usage_view(
                "live_children_per_parent",
                live_children as u64,
                limits.max_live_children_per_parent as u64,
                None,
            ),
            usage_view(
                "live_descendants_per_root",
                live_descendants as u64,
                limits.max_live_descendants_per_root as u64,
                None,
            ),
            usage_view(
                "live_sidechains_global",
                live_global as u64,
                limits.max_live_sidechains_global as u64,
                None,
            ),
            usage_view(
                "live_sidechains_per_session",
                live_session as u64,
                limits.max_live_sidechains_per_session as u64,
                None,
            ),
        ];
        if let Some(spawn_count) = run_spawn_count {
            usage.push(usage_view(
                "spawns_per_run",
                spawn_count as u64,
                limits.max_spawns_per_run as u64,
                None,
            ));
        }

        let denied = |code: &str, reason: String, usage: Vec<SubagentPolicyUsageView>| {
            SubagentPolicyDecisionView {
                allowed: false,
                reason_code: Some(code.to_string()),
                reason: Some(reason),
                dry_run: !charge_quota,
                idempotent_replay,
                scopes: scope.clone(),
                limits: limits.clone(),
                estimate: estimate.clone(),
                usage,
            }
        };

        if child_depth > limits.max_child_depth {
            return denied(
                "max_child_depth",
                format!(
                    "spawn depth {child_depth} exceeds daemon limit {}",
                    limits.max_child_depth
                ),
                usage,
            );
        }
        if live_children >= limits.max_live_children_per_parent {
            return denied(
                "live_children_per_parent",
                format!(
                    "live child limit exceeded for {} ({} >= {})",
                    parent.0, live_children, limits.max_live_children_per_parent
                ),
                usage,
            );
        }
        if live_descendants >= limits.max_live_descendants_per_root {
            return denied(
                "live_descendants_per_root",
                format!(
                    "live descendant limit exceeded for {} ({} >= {})",
                    scope.root_agent_id, live_descendants, limits.max_live_descendants_per_root
                ),
                usage,
            );
        }
        if live_global >= limits.max_live_sidechains_global {
            return denied(
                "live_sidechains_global",
                format!(
                    "global live sidechain limit exceeded ({} >= {})",
                    live_global, limits.max_live_sidechains_global
                ),
                usage,
            );
        }
        if live_session >= limits.max_live_sidechains_per_session {
            return denied(
                "live_sidechains_per_session",
                format!(
                    "session live sidechain limit exceeded for {} ({} >= {})",
                    scope.session_id, live_session, limits.max_live_sidechains_per_session
                ),
                usage,
            );
        }
        if let (Some(run_id), Some(spawn_count)) = (effective_run_id, run_spawn_count)
            && spawn_count >= limits.max_spawns_per_run
        {
            return denied(
                "spawns_per_run",
                format!(
                    "spawn limit exceeded for run {run_id} ({} >= {})",
                    spawn_count, limits.max_spawns_per_run
                ),
                usage,
            );
        }
        if estimate.input_tokens > limits.max_spawn_input_tokens_per_request {
            return denied(
                "input_tokens_per_request",
                format!(
                    "spawn input token estimate {} exceeds request limit {}",
                    estimate.input_tokens, limits.max_spawn_input_tokens_per_request
                ),
                usage,
            );
        }
        if estimate.output_tokens > limits.max_spawn_output_tokens_per_request {
            return denied(
                "output_tokens_per_request",
                format!(
                    "spawn output token reservation {} exceeds request limit {}",
                    estimate.output_tokens, limits.max_spawn_output_tokens_per_request
                ),
                usage,
            );
        }

        let now = now_ms();
        let charge_key = spawn_policy_charge_key(request_key, request_fingerprint);
        let fingerprint_hash = spawn_policy_fingerprint_hash(request_fingerprint);
        let mut ledger = self
            .spawn_policy_ledger
            .lock()
            .expect("spawn policy ledger mutex poisoned");
        prune_spawn_policy_ledger(&mut ledger, now, policy.max_quota_window_ms());
        if ledger.entries.iter().any(|entry| entry.key == charge_key) {
            if charge_quota {
                ledger.idempotent_replays += 1;
                if let Err(error) = self.persist_spawn_policy_ledger(&ledger) {
                    warn!(error = %error, "failed to persist subagent policy idempotent replay");
                }
            }
            return SubagentPolicyDecisionView {
                allowed: true,
                reason_code: None,
                reason: None,
                dry_run: !charge_quota,
                idempotent_replay: true,
                scopes: scope.clone(),
                limits,
                estimate: estimate.clone(),
                usage,
            };
        }

        let active_entries = quota_window_entries(&ledger, now, limits.spawn_rate_window_ms);
        let global_window_spawns = active_entries.len() as u64;
        let session_window_spawns = active_entries
            .iter()
            .filter(|entry| entry.session_id == scope.session_id)
            .count() as u64;
        let profile_window_spawns = scope.profile.as_ref().map(|profile| {
            active_entries
                .iter()
                .filter(|entry| entry.profile.as_ref() == Some(profile))
                .count() as u64
        });
        let project_window_spawns = scope
            .project_ids
            .iter()
            .map(|project_id| {
                (
                    project_id,
                    active_entries
                        .iter()
                        .filter(|entry| {
                            entry
                                .project_ids
                                .iter()
                                .any(|project| project == project_id)
                        })
                        .count() as u64,
                )
            })
            .collect::<Vec<_>>();
        let global_window_cost = active_entries
            .iter()
            .map(|entry| entry.cost_microusd)
            .sum::<u64>();
        let global_window_cpu = active_entries.iter().map(|entry| entry.cpu_ms).sum::<u64>();
        usage.push(usage_view(
            "global_window_spawns",
            global_window_spawns,
            limits.max_spawns_global_window as u64,
            Some(limits.spawn_rate_window_ms),
        ));
        usage.push(usage_view(
            "session_window_spawns",
            session_window_spawns,
            limits.max_spawns_per_session_window as u64,
            Some(limits.spawn_rate_window_ms),
        ));
        if let Some(profile_window_spawns) = profile_window_spawns {
            usage.push(usage_view(
                "profile_window_spawns",
                profile_window_spawns,
                limits.max_spawns_per_profile_window as u64,
                Some(limits.spawn_rate_window_ms),
            ));
        }
        for (project_id, project_count) in &project_window_spawns {
            usage.push(usage_view(
                &format!("project_window_spawns:{project_id}"),
                *project_count,
                limits.max_spawns_per_project_window as u64,
                Some(limits.spawn_rate_window_ms),
            ));
        }
        usage.push(usage_view(
            "global_window_cost_microusd",
            global_window_cost,
            limits.max_spawn_cost_microusd_per_window,
            Some(limits.spawn_rate_window_ms),
        ));
        usage.push(usage_view(
            "global_window_cpu_ms",
            global_window_cpu,
            limits.max_spawn_cpu_ms_per_window,
            Some(limits.spawn_rate_window_ms),
        ));

        if global_window_spawns >= limits.max_spawns_global_window as u64 {
            return denied(
                "global_window_spawns",
                format!(
                    "global spawn quota exceeded ({} >= {})",
                    global_window_spawns, limits.max_spawns_global_window
                ),
                usage,
            );
        }
        if session_window_spawns >= limits.max_spawns_per_session_window as u64 {
            return denied(
                "session_window_spawns",
                format!(
                    "session spawn quota exceeded for {} ({} >= {})",
                    scope.session_id, session_window_spawns, limits.max_spawns_per_session_window
                ),
                usage,
            );
        }
        if let Some(profile_window_spawns) = profile_window_spawns
            && profile_window_spawns >= limits.max_spawns_per_profile_window as u64
        {
            return denied(
                "profile_window_spawns",
                format!(
                    "profile spawn quota exceeded for {} ({} >= {})",
                    scope.profile.as_deref().unwrap_or("default"),
                    profile_window_spawns,
                    limits.max_spawns_per_profile_window
                ),
                usage,
            );
        }
        for (project_id, project_count) in project_window_spawns {
            if project_count >= limits.max_spawns_per_project_window as u64 {
                return denied(
                    "project_window_spawns",
                    format!(
                        "project spawn quota exceeded for {} ({} >= {})",
                        project_id, project_count, limits.max_spawns_per_project_window
                    ),
                    usage,
                );
            }
        }
        if global_window_cost.saturating_add(estimate.cost_microusd)
            > limits.max_spawn_cost_microusd_per_window
        {
            return denied(
                "global_window_cost_microusd",
                format!(
                    "global spawn cost budget exceeded ({} + {} > {})",
                    global_window_cost,
                    estimate.cost_microusd,
                    limits.max_spawn_cost_microusd_per_window
                ),
                usage,
            );
        }
        if global_window_cpu.saturating_add(estimate.cpu_ms) > limits.max_spawn_cpu_ms_per_window {
            return denied(
                "global_window_cpu_ms",
                format!(
                    "global spawn CPU budget exceeded ({} + {} > {})",
                    global_window_cpu, estimate.cpu_ms, limits.max_spawn_cpu_ms_per_window
                ),
                usage,
            );
        }

        if charge_quota && !idempotent_replay {
            ledger.entries.push(SpawnPolicyLedgerEntry {
                key: charge_key,
                parent_agent_id: scope.parent_agent_id.clone(),
                root_agent_id: scope.root_agent_id.clone(),
                session_id: scope.session_id.clone(),
                profile: scope.profile.clone(),
                project_ids: scope.project_ids.clone(),
                request_fingerprint_hash: fingerprint_hash,
                charged_at_ms: now,
                input_tokens: estimate.input_tokens,
                output_tokens: estimate.output_tokens,
                cost_microusd: estimate.cost_microusd,
                cpu_ms: estimate.cpu_ms,
            });
            if let Err(error) = self.persist_spawn_policy_ledger(&ledger) {
                return SubagentPolicyDecisionView {
                    allowed: false,
                    reason_code: Some("quota_persistence_failed".to_string()),
                    reason: Some(format!("failed to persist spawn quota ledger: {error}")),
                    dry_run: false,
                    idempotent_replay: false,
                    scopes: scope.clone(),
                    limits,
                    estimate: estimate.clone(),
                    usage,
                };
            }
        }

        SubagentPolicyDecisionView {
            allowed: true,
            reason_code: None,
            reason: None,
            dry_run: !charge_quota,
            idempotent_replay,
            scopes: scope.clone(),
            limits,
            estimate: estimate.clone(),
            usage,
        }
    }

    /// Releases one previously acquired child spawn reservation.
    pub(crate) fn release_spawn_reservation(&self, reservation: SpawnReservation) {
        let mut reservations = self
            .spawn_reservations
            .lock()
            .expect("spawn reservation mutex poisoned");
        if reservation.counted_towards_limits {
            if let Some(count) = reservations.by_parent.get_mut(&reservation.parent_id) {
                if *count <= 1 {
                    reservations.by_parent.remove(&reservation.parent_id);
                } else {
                    *count -= 1;
                }
            }
            if let Some(count) = reservations.by_root.get_mut(&reservation.root_id) {
                if *count <= 1 {
                    reservations.by_root.remove(&reservation.root_id);
                } else {
                    *count -= 1;
                }
            }
            if let Some(session_id) = reservation.session_id.as_ref()
                && let Some(count) = reservations.by_session.get_mut(session_id)
            {
                if *count <= 1 {
                    reservations.by_session.remove(session_id);
                } else {
                    *count -= 1;
                }
            }
            reservations.global_live = reservations.global_live.saturating_sub(1);
        }
        if let Some(run_id) = reservation.run_id {
            if let Some(count) = reservations.by_run.get_mut(&run_id) {
                if *count <= 1 {
                    reservations.by_run.remove(&run_id);
                } else {
                    *count -= 1;
                }
            }
        }
        if let Some(request_key) = reservation.request_key {
            reservations.by_request.remove(&request_key);
        }
        reservations
            .by_conversation
            .remove(&reservation.conversation_key);
    }

    /// Returns an operator-facing snapshot of subagent policy quotas and live reservations.
    pub(crate) fn spawn_policy_status(
        &self,
        policy: &SubagentPolicyConfig,
        now_ms: u64,
    ) -> SubagentPolicyStatusView {
        let reservations = self
            .spawn_reservations
            .lock()
            .expect("spawn reservation mutex poisoned");
        let reservation_status = reservation_status_view(&reservations);
        drop(reservations);

        let mut ledger = self
            .spawn_policy_ledger
            .lock()
            .expect("spawn policy ledger mutex poisoned");
        prune_spawn_policy_ledger(&mut ledger, now_ms, policy.max_quota_window_ms());
        let limits = policy.base_limits();
        let active_entries = quota_window_entries(&ledger, now_ms, limits.spawn_rate_window_ms);
        let usage = vec![
            usage_view(
                "global_window_spawns",
                active_entries.len() as u64,
                limits.max_spawns_global_window as u64,
                Some(limits.spawn_rate_window_ms),
            ),
            usage_view(
                "global_window_cost_microusd",
                active_entries.iter().map(|entry| entry.cost_microusd).sum(),
                limits.max_spawn_cost_microusd_per_window,
                Some(limits.spawn_rate_window_ms),
            ),
            usage_view(
                "global_window_cpu_ms",
                active_entries.iter().map(|entry| entry.cpu_ms).sum(),
                limits.max_spawn_cpu_ms_per_window,
                Some(limits.spawn_rate_window_ms),
            ),
        ];
        SubagentPolicyStatusView {
            policy: policy.clone(),
            reservations: reservation_status,
            active_quota_entries: active_entries.len(),
            denied_by_reason: ledger.denied_by_reason.clone(),
            idempotent_replays: ledger.idempotent_replays,
            usage,
        }
    }

    /// Acquires one mailbox scheduling slot and returns `false` when work is already scheduled.
    pub(crate) async fn try_acquire_mailbox_slot(&self, mailbox_key: &str) -> bool {
        self.mailbox_scheduler
            .lock()
            .await
            .insert(mailbox_key.to_string())
    }

    /// Releases one mailbox scheduling slot after the mailbox-delivery attempt settles.
    pub(crate) async fn release_mailbox_slot(&self, mailbox_key: &str) {
        self.mailbox_scheduler.lock().await.remove(mailbox_key);
    }

    fn persist_spawn_policy_ledger(&self, ledger: &SpawnPolicyLedger) -> Result<()> {
        if let Some(path) = self.spawn_policy_ledger_path.as_ref() {
            write_json_pretty_atomically(path, ledger)?;
        }
        Ok(())
    }

    fn record_spawn_policy_denial(&self, reason_code: Option<&str>) {
        let reason_code = reason_code.unwrap_or("unknown").to_string();
        let mut ledger = self
            .spawn_policy_ledger
            .lock()
            .expect("spawn policy ledger mutex poisoned");
        *ledger.denied_by_reason.entry(reason_code).or_insert(0) += 1;
        if let Err(error) = self.persist_spawn_policy_ledger(&ledger) {
            warn!(error = %error, "failed to persist subagent policy denial");
        }
    }
}

fn load_spawn_policy_ledger(path: &PathBuf) -> Result<SpawnPolicyLedger> {
    if !path.exists() {
        return Ok(SpawnPolicyLedger::default());
    }
    Ok(read_json_or_quarantine(path, "subagent spawn-policy ledger")?.unwrap_or_default())
}

fn prune_spawn_policy_ledger(ledger: &mut SpawnPolicyLedger, now_ms: u64, max_window_ms: u64) {
    let cutoff = now_ms.saturating_sub(max_window_ms);
    ledger.entries.retain(|entry| entry.charged_at_ms >= cutoff);
}

fn quota_window_entries(
    ledger: &SpawnPolicyLedger,
    now_ms: u64,
    window_ms: u64,
) -> Vec<&SpawnPolicyLedgerEntry> {
    let cutoff = now_ms.saturating_sub(window_ms);
    ledger
        .entries
        .iter()
        .filter(|entry| entry.charged_at_ms >= cutoff)
        .collect()
}

fn usage_view(
    scope: impl Into<String>,
    used: u64,
    limit: u64,
    window_ms: Option<u64>,
) -> SubagentPolicyUsageView {
    SubagentPolicyUsageView {
        scope: scope.into(),
        used,
        limit,
        remaining: limit.saturating_sub(used),
        window_ms,
    }
}

fn reservation_status_view(reservations: &SpawnReservationState) -> SubagentReservationStatusView {
    SubagentReservationStatusView {
        active_by_parent: reservations.by_parent.clone(),
        active_by_root: reservations.by_root.clone(),
        active_by_session: reservations.by_session.clone(),
        active_global: reservations.global_live,
        in_flight_request_count: reservations.by_request.len(),
        in_flight_conversation_count: reservations.by_conversation.len(),
    }
}

fn spawn_policy_charge_key(request_key: Option<&str>, request_fingerprint: &str) -> String {
    request_key
        .map(|request_key| format!("request:{request_key}"))
        .unwrap_or_else(|| {
            format!(
                "fingerprint:{}",
                spawn_policy_fingerprint_hash(request_fingerprint)
            )
        })
}

fn spawn_policy_fingerprint_hash(request_fingerprint: &str) -> String {
    hex::encode(Sha256::digest(request_fingerprint.as_bytes()))
}

fn live_global_sidechains<F>(supervisor: &AgentSupervisor, has_runtime: F) -> usize
where
    F: Fn(&AgentId) -> bool,
{
    supervisor
        .list()
        .into_iter()
        .filter(|record| record.parent.is_some() && has_runtime(&record.id))
        .count()
}

fn live_session_sidechains<F>(
    supervisor: &AgentSupervisor,
    session_id: &str,
    has_runtime: F,
) -> usize
where
    F: Fn(&AgentId) -> bool,
{
    supervisor
        .list()
        .into_iter()
        .filter(|record| record.parent.is_some() && has_runtime(&record.id))
        .filter(|record| {
            supervisor
                .root_of(&record.id)
                .is_some_and(|root| root.conversation.session_id == session_id)
        })
        .count()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;

    use super::SubagentService;
    use crate::SubagentPolicyConfig;
    use kheish_agent::{AgentSupervisor, ChildRetentionPolicy};
    use kheish_runtime::NoopObserver;
    use kheish_types::ConversationKey;

    #[tokio::test]
    async fn mailbox_slots_are_deduplicated_until_release() {
        let service = SubagentService::new_ephemeral();

        assert!(service.try_acquire_mailbox_slot("agent-1").await);
        assert!(!service.try_acquire_mailbox_slot("agent-1").await);
        service.release_mailbox_slot("agent-1").await;
        assert!(service.try_acquire_mailbox_slot("agent-1").await);
    }

    #[test]
    fn spawn_request_reservations_are_exclusive_until_release() -> Result<()> {
        let service = SubagentService::new_ephemeral();

        let reservation = service.reserve_spawn_request("parent:req-1")?;
        let error = service
            .reserve_spawn_request("parent:req-1")
            .expect_err("duplicate spawn request reservation should be rejected");
        assert!(error.to_string().contains("already in progress"));
        service.release_spawn_request_reservation(reservation);
        service.reserve_spawn_request("parent:req-1")?;

        Ok(())
    }

    #[test]
    fn spawn_reservations_enforce_per_run_limits_and_can_be_cleared() -> Result<()> {
        let service = SubagentService::new_ephemeral();
        let supervisor = AgentSupervisor::new(Arc::new(NoopObserver));
        let parent = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "root".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        let policy = SubagentPolicyConfig {
            max_child_depth: 4,
            max_live_children_per_parent: 8,
            max_live_descendants_per_root: 8,
            max_spawns_per_run: 1,
            ..SubagentPolicyConfig::default()
        };

        let reservation = service.reserve_subagent_spawn(
            &supervisor,
            &parent.id,
            Some("run-1"),
            Some("parent:req-1"),
            "session-a::thread-a",
            true,
            &policy,
            |_| false,
            |_| 0,
        )?;
        let error = service
            .reserve_subagent_spawn(
                &supervisor,
                &parent.id,
                Some("run-1"),
                Some("parent:req-2"),
                "session-a::thread-b",
                true,
                &policy,
                |_| false,
                |_| 0,
            )
            .expect_err("second reservation should exceed the per-run limit");
        assert!(error.to_string().contains("spawn limit exceeded"));

        service.clear_run_spawn_count("run-1");
        service.release_spawn_reservation(reservation);
        service.reserve_subagent_spawn(
            &supervisor,
            &parent.id,
            Some("run-1"),
            Some("parent:req-3"),
            "session-a::thread-a",
            true,
            &policy,
            |_| false,
            |_| 0,
        )?;
        Ok(())
    }

    #[test]
    fn spawn_reservations_reject_duplicate_request_keys_until_release() -> Result<()> {
        let service = SubagentService::new_ephemeral();
        let supervisor = AgentSupervisor::new(Arc::new(NoopObserver));
        let parent = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "root".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        let policy = SubagentPolicyConfig {
            max_child_depth: 4,
            max_live_children_per_parent: 8,
            max_live_descendants_per_root: 8,
            max_spawns_per_run: 8,
            ..SubagentPolicyConfig::default()
        };

        let reservation = service.reserve_subagent_spawn(
            &supervisor,
            &parent.id,
            Some("run-1"),
            Some("parent:req-1"),
            "session-a::thread-a",
            true,
            &policy,
            |_| false,
            |_| 0,
        )?;
        let error = service
            .reserve_subagent_spawn(
                &supervisor,
                &parent.id,
                Some("run-2"),
                Some("parent:req-1"),
                "session-a::thread-b",
                true,
                &policy,
                |_| false,
                |_| 0,
            )
            .expect_err("duplicate request key should be rejected");
        assert!(error.to_string().contains("already in progress"));

        service.release_spawn_reservation(reservation);
        service.reserve_subagent_spawn(
            &supervisor,
            &parent.id,
            Some("run-2"),
            Some("parent:req-1"),
            "session-a::thread-b",
            true,
            &policy,
            |_| false,
            |_| 0,
        )?;
        Ok(())
    }

    #[test]
    fn spawn_reservations_reject_duplicate_conversations_until_release() -> Result<()> {
        let service = SubagentService::new_ephemeral();
        let supervisor = AgentSupervisor::new(Arc::new(NoopObserver));
        let parent = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "root".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        let policy = SubagentPolicyConfig {
            max_child_depth: 4,
            max_live_children_per_parent: 8,
            max_live_descendants_per_root: 8,
            max_spawns_per_run: 8,
            ..SubagentPolicyConfig::default()
        };

        let reservation = service.reserve_subagent_spawn(
            &supervisor,
            &parent.id,
            Some("run-1"),
            Some("parent:req-1"),
            "shared-session::shared-thread",
            true,
            &policy,
            |_| false,
            |_| 0,
        )?;
        let error = service
            .reserve_subagent_spawn(
                &supervisor,
                &parent.id,
                Some("run-2"),
                Some("parent:req-2"),
                "shared-session::shared-thread",
                true,
                &policy,
                |_| false,
                |_| 0,
            )
            .expect_err("duplicate conversation should be rejected");
        assert!(error.to_string().contains("already in progress"));

        service.release_spawn_reservation(reservation);
        service.reserve_subagent_spawn(
            &supervisor,
            &parent.id,
            Some("run-2"),
            Some("parent:req-2"),
            "shared-session::shared-thread",
            true,
            &policy,
            |_| false,
            |_| 0,
        )?;
        Ok(())
    }

    #[test]
    fn spawn_retry_reservations_skip_live_child_and_run_quotas() -> Result<()> {
        let service = SubagentService::new_ephemeral();
        let supervisor = AgentSupervisor::new(Arc::new(NoopObserver));
        let parent = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "root".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        let child = supervisor.spawn(
            Some(parent.id.clone()),
            ConversationKey {
                session_id: "child".to_string(),
                thread_id: Some("thread".to_string()),
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            Some("run-1".to_string()),
        )?;
        let policy = SubagentPolicyConfig {
            max_child_depth: 4,
            max_live_children_per_parent: 1,
            max_live_descendants_per_root: 1,
            max_spawns_per_run: 1,
            ..SubagentPolicyConfig::default()
        };

        let reservation = service.reserve_subagent_spawn(
            &supervisor,
            &parent.id,
            Some("run-1"),
            Some("parent:req-retry"),
            "child::thread",
            false,
            &policy,
            |agent_id| agent_id == &child.id,
            |_| 1,
        )?;
        service.release_spawn_reservation(reservation);
        Ok(())
    }
}
