use parking_lot::{Mutex, RwLock};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use kheish_core::PermissionGate;
use kheish_session::PermissionAuditRecord;
use kheish_types::{
    ApprovalRequest, HookPermissionUpdate, HookPermissionUpdateBehavior, HookPermissionUpdateScope,
    PermissionDecision, ToolCallRecord,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

use crate::current_execution_scope;
use crate::observability::{RuntimeObserver, TraceEvent, TraceEventKind};

#[async_trait::async_trait]
pub trait SessionPermissionUpdateStore: Send + Sync {
    async fn persist_session_rule_updates(
        &self,
        session_id: &str,
        updates: &[HookPermissionUpdate],
    ) -> Result<()>;
}

/// The behavior applied by a permission rule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionBehavior {
    /// Always allow the matching tool call.
    Allow,
    /// Always deny the matching tool call.
    Deny,
    /// Require explicit daemon approval before the tool may run.
    Ask,
}

/// The scope that owns a permission rule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionScope {
    /// User-wide configuration.
    User,
    /// Project-wide configuration.
    Project,
    /// Session-only configuration.
    Session,
}

/// The runtime-wide permission handling mode.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    /// Evaluate the configured rules as-is.
    #[default]
    #[serde(rename = "default")]
    Default,
    /// Auto-accept direct file edits while keeping other rules intact.
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    /// Bypass every permission prompt.
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
    /// Reject every tool execution to keep the daemon in planning mode.
    #[serde(rename = "plan")]
    Plan,
    /// Never ask interactively: deny anything that would otherwise require approval.
    #[serde(rename = "dontAsk")]
    DontAsk,
}

/// A rule that applies to a tool call by name pattern.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    /// The owning scope.
    pub scope: PermissionScope,
    /// The tool name pattern (`*` and prefix `foo*` are supported).
    pub tool_name_pattern: String,
    /// The behavior to apply.
    pub behavior: PermissionBehavior,
    /// The optional explanatory reason.
    pub reason: Option<String>,
}

/// Context extracted from a tool call before permission evaluation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionContext {
    /// The optional approval justification resolved by the daemon.
    pub justification: Option<String>,
}

/// The evaluated permission outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PermissionOutcome {
    /// The final low-level decision.
    pub decision: PermissionDecision,
    /// The owning scope.
    pub scope: PermissionScope,
    /// The audit record emitted by the evaluation.
    pub audit: PermissionAuditRecord,
}

/// A dry-run explanation of one tool permission decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PermissionExplanation {
    /// The evaluated tool call identifier.
    pub tool_call_id: String,
    /// The evaluated tool name.
    pub tool_name: String,
    /// The effective permission mode after session overrides are applied.
    pub effective_mode: PermissionMode,
    /// The final decision label: `allow`, `ask`, or `deny`.
    pub decision: String,
    /// The decision label before the effective permission mode was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_decision: Option<String>,
    /// Whether the effective permission mode changed the static rule semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_applied: Option<bool>,
    /// The concrete mode effect, when the effective permission mode changed semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_effect: Option<String>,
    /// Whether executing this tool would create an approval request.
    pub approval_required: bool,
    /// Whether hooks were executed while producing this dry-run explanation.
    #[serde(default)]
    pub hooks_evaluated: bool,
    /// The dry-run hook policy applied to this explanation.
    #[serde(default = "default_permission_explanation_hook_policy")]
    pub hook_policy: String,
    /// The owning rule scope for the final decision.
    pub scope: String,
    /// The human-readable reason for the final decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The rule selected by precedence before the effective mode was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_rule: Option<PermissionRule>,
    /// The selected rule provenance before the effective mode was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_rule_origin: Option<String>,
    /// The audit record that would be persisted if this were a real tool execution.
    pub audit_preview: PermissionAuditRecord,
}

/// A layered permission engine with auditing.
pub struct PermissionEngine {
    state: RwLock<PermissionState>,
    session_modes: RwLock<std::collections::BTreeMap<String, PermissionMode>>,
    session_rule_overrides: RwLock<std::collections::BTreeMap<String, Vec<PermissionRule>>>,
    session_rule_update_store: RwLock<Option<Arc<dyn SessionPermissionUpdateStore>>>,
    hook_update_persist_lock: AsyncMutex<()>,
    audits: Mutex<BTreeMap<String, Vec<PermissionAuditRecord>>>,
    observer: Arc<dyn RuntimeObserver>,
    next_request_id: AtomicU64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PermissionState {
    user_rules: Vec<PermissionRule>,
    project_rules: Vec<PermissionRule>,
    session_rules: Vec<PermissionRule>,
    mode: PermissionMode,
}

impl PermissionEngine {
    /// Creates a new layered permission engine.
    pub fn new(
        user_rules: Vec<PermissionRule>,
        project_rules: Vec<PermissionRule>,
        session_rules: Vec<PermissionRule>,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Self {
        Self {
            state: RwLock::new(PermissionState {
                user_rules,
                project_rules,
                session_rules,
                mode: PermissionMode::Default,
            }),
            session_modes: RwLock::new(std::collections::BTreeMap::new()),
            session_rule_overrides: RwLock::new(std::collections::BTreeMap::new()),
            session_rule_update_store: RwLock::new(None),
            hook_update_persist_lock: AsyncMutex::new(()),
            audits: Mutex::new(BTreeMap::new()),
            observer,
            next_request_id: AtomicU64::new(0),
        }
    }

    /// Returns the active permission mode.
    pub fn mode(&self) -> PermissionMode {
        self.state.read().mode.clone()
    }

    /// Updates the active permission mode in place.
    pub fn set_mode(&self, mode: PermissionMode) {
        self.state.write().mode = mode;
    }

    /// Returns the optional session-scoped permission mode override.
    pub fn session_mode(&self, session_id: &str) -> Option<PermissionMode> {
        self.session_modes.read().get(session_id).cloned()
    }

    /// Sets or clears the session-scoped permission mode override.
    pub fn set_session_mode(&self, session_id: impl Into<String>, mode: Option<PermissionMode>) {
        let session_id = session_id.into();
        let mut session_modes = self.session_modes.write();
        if let Some(mode) = mode {
            session_modes.insert(session_id, mode);
        } else {
            session_modes.remove(&session_id);
        }
    }

    /// Binds one async store used to persist session-scoped hook permission overrides.
    pub fn bind_session_rule_update_store(&self, store: Arc<dyn SessionPermissionUpdateStore>) {
        *self.session_rule_update_store.write() = Some(store);
    }

    /// Returns the current session-scoped hook permission overrides for one session.
    pub fn session_rule_updates(&self, session_id: &str) -> Vec<HookPermissionUpdate> {
        self.session_rule_overrides
            .read()
            .get(session_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(hook_update_from_permission_rule)
            .collect()
    }

    /// Replaces the session-scoped hook permission overrides for one session.
    pub fn replace_session_rule_updates(
        &self,
        session_id: impl Into<String>,
        updates: &[HookPermissionUpdate],
    ) {
        let session_id = session_id.into();
        let mut session_overrides = self.session_rule_overrides.write();
        if updates.is_empty() {
            session_overrides.remove(&session_id);
            return;
        }
        session_overrides.insert(
            session_id,
            updates
                .iter()
                .filter(|update| matches!(update.scope, HookPermissionUpdateScope::Session))
                .cloned()
                .map(permission_rule_from_hook_update)
                .collect(),
        );
    }

    fn apply_hook_permission_updates_in_place(
        &self,
        session_id: &str,
        updates: &[HookPermissionUpdate],
    ) {
        if updates.is_empty() {
            return;
        }

        let mut state = self.state.write();
        let mut session_overrides = self.session_rule_overrides.write();
        for update in updates {
            let rule = permission_rule_from_hook_update(update.clone());

            match update.scope {
                HookPermissionUpdateScope::User => {
                    upsert_permission_rule(&mut state.user_rules, rule)
                }
                HookPermissionUpdateScope::Project => {
                    upsert_permission_rule(&mut state.project_rules, rule)
                }
                HookPermissionUpdateScope::Session => {
                    let entry = session_overrides.entry(session_id.to_string()).or_default();
                    upsert_permission_rule(entry, rule);
                }
            }
        }
    }

    fn merged_session_rule_updates(
        &self,
        session_id: &str,
        updates: &[HookPermissionUpdate],
    ) -> Vec<HookPermissionUpdate> {
        let mut merged = self
            .session_rule_updates(session_id)
            .into_iter()
            .map(permission_rule_from_hook_update)
            .collect::<Vec<_>>();
        for update in updates
            .iter()
            .filter(|update| matches!(update.scope, HookPermissionUpdateScope::Session))
        {
            upsert_permission_rule(
                &mut merged,
                permission_rule_from_hook_update(update.clone()),
            );
        }
        merged
            .into_iter()
            .map(hook_update_from_permission_rule)
            .collect()
    }

    /// Evaluates a tool call and records the resulting audit event.
    pub fn evaluate(&self, call: &ToolCallRecord) -> PermissionOutcome {
        let context = PermissionContext::default();
        let evaluated = self.evaluate_inner(call, true);

        let (decision_label, approval_request_id) = decision_label_and_request(&evaluated.decision);

        let audit = PermissionAuditRecord {
            scope: format!("{:?}", evaluated.scope).to_lowercase(),
            tool_name: call.name.clone(),
            tool_call_id: Some(call.id.clone()),
            decision: decision_label.clone(),
            base_decision: Some(evaluated.base_decision.clone()),
            effective_mode: Some(permission_mode_label(&evaluated.mode)),
            mode_effect: evaluated.mode_effect.clone(),
            matched_rule_pattern: evaluated
                .matched_rule
                .as_ref()
                .map(|rule| rule.tool_name_pattern.clone()),
            matched_rule_origin: evaluated.matched_rule_origin.clone(),
            justification: context.justification,
            reason: evaluated.reason.clone(),
            approval_request_id,
        };
        self.observer
            .record(TraceEvent::new(TraceEventKind::PermissionEvaluated {
                tool_name: call.name.clone(),
                decision: decision_label,
            }));
        self.audits
            .lock()
            .entry(current_permission_session_id())
            .or_default()
            .push(audit.clone());
        PermissionOutcome {
            decision: evaluated.decision,
            scope: evaluated.scope,
            audit,
        }
    }

    /// Explains a tool call permission without creating approvals or audit records.
    pub fn explain(&self, call: &ToolCallRecord) -> PermissionExplanation {
        self.explain_with_mode(call, None)
    }

    /// Explains a tool call permission under an optional mode override without mutating runtime
    /// state, creating approvals, or writing audit records.
    pub fn explain_with_mode(
        &self,
        call: &ToolCallRecord,
        mode_override: Option<PermissionMode>,
    ) -> PermissionExplanation {
        let context = PermissionContext::default();
        let evaluated = self.evaluate_inner_with_mode(call, false, mode_override);
        let (decision_label, _) = decision_label_and_request(&evaluated.decision);
        let base_decision = evaluated.base_decision.clone();
        let mode_applied = evaluated.mode_effect.is_some();
        let approval_required = matches!(evaluated.decision, PermissionDecision::Ask { .. });
        let audit_preview = PermissionAuditRecord {
            scope: format!("{:?}", evaluated.scope).to_lowercase(),
            tool_name: call.name.clone(),
            tool_call_id: Some(call.id.clone()),
            decision: decision_label.clone(),
            base_decision: Some(base_decision.clone()),
            effective_mode: Some(permission_mode_label(&evaluated.mode)),
            mode_effect: evaluated.mode_effect.clone(),
            matched_rule_pattern: evaluated
                .matched_rule
                .as_ref()
                .map(|rule| rule.tool_name_pattern.clone()),
            matched_rule_origin: evaluated.matched_rule_origin.clone(),
            justification: context.justification,
            reason: evaluated.reason.clone(),
            approval_request_id: None,
        };
        PermissionExplanation {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            effective_mode: evaluated.mode,
            decision: decision_label,
            mode_applied: Some(mode_applied),
            mode_effect: evaluated.mode_effect,
            base_decision: Some(base_decision),
            approval_required,
            hooks_evaluated: false,
            hook_policy: "not_executed_in_dry_run".to_string(),
            scope: format!("{:?}", evaluated.scope).to_lowercase(),
            reason: evaluated.reason,
            matched_rule_origin: evaluated.matched_rule_origin,
            matched_rule: evaluated.matched_rule,
            audit_preview,
        }
    }

    fn evaluate_inner(
        &self,
        call: &ToolCallRecord,
        allocate_approval_id: bool,
    ) -> EvaluatedPermission {
        self.evaluate_inner_with_mode(call, allocate_approval_id, None)
    }

    fn evaluate_inner_with_mode(
        &self,
        call: &ToolCallRecord,
        allocate_approval_id: bool,
        mode_override: Option<PermissionMode>,
    ) -> EvaluatedPermission {
        let state = self.state.read();
        let scoped_session_rules = current_execution_scope()
            .and_then(|scope| {
                (!scope.session_id.is_empty()).then(|| {
                    self.session_rule_overrides
                        .read()
                        .get(&scope.session_id)
                        .cloned()
                        .unwrap_or_default()
                })
            })
            .unwrap_or_default();
        let session_hook_update_count = scoped_session_rules.len();
        let mut effective_session_rules = scoped_session_rules;
        effective_session_rules.extend(state.session_rules.clone());
        let matched = matching_rule_with_origin(
            &state.user_rules,
            &state.project_rules,
            &effective_session_rules,
            session_hook_update_count,
            &call.name,
        );
        let rule = matched.as_ref().map(|(rule, _)| *rule);
        let matched_rule = rule.cloned();
        let matched_rule_origin = matched.map(|(_, origin)| origin.label().to_string());
        let (base_decision, base_scope, base_reason) = match rule {
            Some(rule) => match rule.behavior {
                PermissionBehavior::Allow => (
                    PermissionDecision::Allow,
                    rule.scope.clone(),
                    rule.reason.clone(),
                ),
                PermissionBehavior::Deny => (
                    PermissionDecision::Deny {
                        reason: rule
                            .reason
                            .clone()
                            .unwrap_or_else(|| "permission denied".to_string()),
                    },
                    rule.scope.clone(),
                    rule.reason.clone(),
                ),
                PermissionBehavior::Ask => (
                    PermissionDecision::Ask {
                        request: ApprovalRequest {
                            id: if allocate_approval_id {
                                self.next_request_id(&call.name, &call.id)
                            } else {
                                "dry-run".to_string()
                            },
                            tool_call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            input: call.input.clone(),
                            scope: format!("{:?}", rule.scope).to_lowercase(),
                            reason: rule
                                .reason
                                .clone()
                                .unwrap_or_else(|| "approval required".to_string()),
                        },
                    },
                    rule.scope.clone(),
                    rule.reason
                        .clone()
                        .or_else(|| Some("approval required".to_string())),
                ),
            },
            None => (PermissionDecision::Allow, PermissionScope::Session, None),
        };
        let (base_decision_label, _) = decision_label_and_request(&base_decision);
        let mode = mode_override.unwrap_or_else(|| {
            current_execution_scope()
                .and_then(|scope| {
                    (!scope.session_id.is_empty()).then(|| self.session_mode(&scope.session_id))
                })
                .flatten()
                .unwrap_or_else(|| state.mode.clone())
        });
        let (decision, scope, reason, mode_effect) =
            apply_permission_mode(&mode, call, base_decision, base_scope, base_reason);
        EvaluatedPermission {
            decision,
            scope,
            reason,
            mode,
            base_decision: base_decision_label,
            matched_rule,
            matched_rule_origin,
            mode_effect,
        }
    }

    /// Checks a tool call permission using the public runtime API.
    pub async fn check(&self, call: &ToolCallRecord) -> Result<PermissionDecision> {
        <Self as PermissionGate>::check(self, call).await
    }

    /// Drains and returns the accumulated audit records.
    pub fn drain_audits(&self) -> Vec<PermissionAuditRecord> {
        let mut audits = self.audits.lock();
        audits
            .values_mut()
            .flat_map(std::mem::take)
            .collect::<Vec<_>>()
    }

    /// Drains and returns accumulated audit records for one session only.
    pub fn drain_audits_for_session(&self, session_id: &str) -> Vec<PermissionAuditRecord> {
        let mut audits = self.audits.lock();
        audits.remove(session_id).unwrap_or_default()
    }

    fn next_request_id(&self, tool_name: &str, tool_call_id: &str) -> String {
        let next = self.next_request_id.fetch_add(1, Ordering::Relaxed) + 1;
        let call_hash = hex::encode(Sha256::digest(tool_call_id.as_bytes()));
        format!("approval-{tool_name}-{next}-{}", &call_hash[..8])
    }
}

struct EvaluatedPermission {
    decision: PermissionDecision,
    scope: PermissionScope,
    reason: Option<String>,
    mode: PermissionMode,
    base_decision: String,
    matched_rule: Option<PermissionRule>,
    matched_rule_origin: Option<String>,
    mode_effect: Option<String>,
}

#[async_trait::async_trait]
impl PermissionGate for PermissionEngine {
    async fn check(&self, call: &ToolCallRecord) -> Result<PermissionDecision> {
        Ok(self.evaluate(call).decision)
    }

    async fn apply_hook_permission_updates(
        &self,
        session_id: &str,
        updates: &[HookPermissionUpdate],
    ) -> Result<()> {
        if let Some(update) = updates
            .iter()
            .find(|update| !matches!(update.scope, HookPermissionUpdateScope::Session))
        {
            anyhow::bail!(
                "hook permission updates only support session scope; {:?} update for {} is not durable",
                update.scope,
                update.tool_name_pattern
            );
        }
        let _persist_guard = self.hook_update_persist_lock.lock().await;
        let store = self.session_rule_update_store.read().clone();
        if updates
            .iter()
            .any(|update| matches!(update.scope, HookPermissionUpdateScope::Session))
            && let Some(store) = store
        {
            let persisted = self.merged_session_rule_updates(session_id, updates);
            store
                .persist_session_rule_updates(session_id, &persisted)
                .await?;
        }
        self.apply_hook_permission_updates_in_place(session_id, updates);
        Ok(())
    }

    async fn record_final_decision(
        &self,
        call: &ToolCallRecord,
        decision: &PermissionDecision,
        reason: Option<String>,
    ) -> Result<()> {
        let (decision_label, approval_request_id) = decision_label_and_request(decision);
        let session_id = current_permission_session_id();
        let mut audits = self.audits.lock();
        let entries = audits.entry(session_id).or_default();
        if let Some(existing) = entries
            .iter_mut()
            .rev()
            .find(|entry| entry.tool_call_id.as_deref() == Some(call.id.as_str()))
        {
            existing.decision = decision_label;
            existing.reason = reason
                .or_else(|| match decision {
                    PermissionDecision::Deny { reason } => Some(reason.clone()),
                    _ => None,
                })
                .or_else(|| existing.reason.clone());
            existing.approval_request_id = approval_request_id;
        } else {
            let evaluated = self.evaluate_inner(call, false);
            let audit = PermissionAuditRecord {
                scope: format!("{:?}", evaluated.scope).to_lowercase(),
                tool_name: call.name.clone(),
                tool_call_id: Some(call.id.clone()),
                decision: decision_label,
                base_decision: Some(evaluated.base_decision),
                effective_mode: Some(permission_mode_label(&evaluated.mode)),
                mode_effect: evaluated.mode_effect,
                matched_rule_pattern: evaluated
                    .matched_rule
                    .as_ref()
                    .map(|rule| rule.tool_name_pattern.clone()),
                matched_rule_origin: evaluated.matched_rule_origin,
                justification: None,
                reason: reason
                    .or_else(|| match decision {
                        PermissionDecision::Deny { reason } => Some(reason.clone()),
                        _ => None,
                    })
                    .or(evaluated.reason),
                approval_request_id,
            };
            entries.push(audit);
        }
        Ok(())
    }
}

fn permission_rule_from_hook_update(update: HookPermissionUpdate) -> PermissionRule {
    PermissionRule {
        scope: match update.scope {
            HookPermissionUpdateScope::User => PermissionScope::User,
            HookPermissionUpdateScope::Project => PermissionScope::Project,
            HookPermissionUpdateScope::Session => PermissionScope::Session,
        },
        tool_name_pattern: update.tool_name_pattern,
        behavior: match update.behavior {
            HookPermissionUpdateBehavior::Allow => PermissionBehavior::Allow,
            HookPermissionUpdateBehavior::Deny => PermissionBehavior::Deny,
            HookPermissionUpdateBehavior::Ask => PermissionBehavior::Ask,
        },
        reason: update.reason,
    }
}

fn hook_update_from_permission_rule(rule: PermissionRule) -> HookPermissionUpdate {
    HookPermissionUpdate {
        scope: match rule.scope {
            PermissionScope::User => HookPermissionUpdateScope::User,
            PermissionScope::Project => HookPermissionUpdateScope::Project,
            PermissionScope::Session => HookPermissionUpdateScope::Session,
        },
        tool_name_pattern: rule.tool_name_pattern,
        behavior: match rule.behavior {
            PermissionBehavior::Allow => HookPermissionUpdateBehavior::Allow,
            PermissionBehavior::Deny => HookPermissionUpdateBehavior::Deny,
            PermissionBehavior::Ask => HookPermissionUpdateBehavior::Ask,
        },
        reason: rule.reason,
    }
}

fn pattern_specificity(tool_name: &str, pattern: &str) -> Option<u16> {
    if pattern == tool_name {
        return Some(u16::MAX);
    }
    if pattern == "*" {
        return Some(1);
    }
    pattern
        .strip_suffix('*')
        .filter(|prefix| !prefix.is_empty() && tool_name.starts_with(*prefix))
        .map(|prefix| 100_u16.saturating_add(prefix.len() as u16))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PermissionRuleOrigin {
    Static,
    HookUpdate,
}

impl PermissionRuleOrigin {
    fn label(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::HookUpdate => "hook_update",
        }
    }
}

fn matching_rule_with_origin<'a>(
    user_rules: &'a [PermissionRule],
    project_rules: &'a [PermissionRule],
    session_rules: &'a [PermissionRule],
    session_hook_update_count: usize,
    tool_name: &str,
) -> Option<(&'a PermissionRule, PermissionRuleOrigin)> {
    best_matching_rule_index(session_rules, tool_name)
        .map(|(index, rule)| {
            let origin = if index < session_hook_update_count {
                PermissionRuleOrigin::HookUpdate
            } else {
                PermissionRuleOrigin::Static
            };
            (rule, origin)
        })
        .or_else(|| {
            best_matching_rule(project_rules, tool_name)
                .map(|rule| (rule, PermissionRuleOrigin::Static))
        })
        .or_else(|| {
            best_matching_rule(user_rules, tool_name)
                .map(|rule| (rule, PermissionRuleOrigin::Static))
        })
}

fn best_matching_rule<'a>(
    rules: &'a [PermissionRule],
    tool_name: &str,
) -> Option<&'a PermissionRule> {
    best_matching_rule_index(rules, tool_name).map(|(_, rule)| rule)
}

fn best_matching_rule_index<'a>(
    rules: &'a [PermissionRule],
    tool_name: &str,
) -> Option<(usize, &'a PermissionRule)> {
    let mut best = None;
    let mut best_specificity = 0_u16;
    for (index, rule) in rules.iter().enumerate() {
        let Some(specificity) = pattern_specificity(tool_name, &rule.tool_name_pattern) else {
            continue;
        };
        if specificity > best_specificity {
            best = Some((index, rule));
            best_specificity = specificity;
        }
    }
    best
}

fn upsert_permission_rule(rules: &mut Vec<PermissionRule>, update: PermissionRule) {
    if let Some(existing) = rules.iter_mut().find(|rule| {
        rule.scope == update.scope && rule.tool_name_pattern == update.tool_name_pattern
    }) {
        *existing = update;
    } else {
        rules.push(update);
    }
}

fn decision_label_and_request(decision: &PermissionDecision) -> (String, Option<String>) {
    match decision {
        PermissionDecision::Allow => ("allow".to_string(), None),
        PermissionDecision::Deny { .. } => ("deny".to_string(), None),
        PermissionDecision::Ask { request } => ("ask".to_string(), Some(request.id.clone())),
    }
}

fn current_permission_session_id() -> String {
    current_execution_scope()
        .map(|scope| scope.session_id)
        .filter(|session_id| !session_id.is_empty())
        .unwrap_or_default()
}

fn default_permission_explanation_hook_policy() -> String {
    "not_reported_by_older_daemon".to_string()
}

fn permission_mode_label(mode: &PermissionMode) -> String {
    serde_json::to_value(mode)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("{mode:?}"))
}

fn apply_permission_mode(
    mode: &PermissionMode,
    call: &ToolCallRecord,
    base_decision: PermissionDecision,
    base_scope: PermissionScope,
    base_reason: Option<String>,
) -> (
    PermissionDecision,
    PermissionScope,
    Option<String>,
    Option<String>,
) {
    match mode {
        PermissionMode::Default => (base_decision, base_scope, base_reason, None),
        PermissionMode::BypassPermissions => match base_decision {
            PermissionDecision::Ask { .. } => (
                PermissionDecision::Allow,
                PermissionScope::Session,
                Some("bypass permissions mode".to_string()),
                Some("bypass_permissions_auto_allowed_approval".to_string()),
            ),
            other => (other, base_scope, base_reason, None),
        },
        PermissionMode::Plan => match base_decision {
            PermissionDecision::Deny { .. } => (base_decision, base_scope, base_reason, None),
            PermissionDecision::Allow if is_plan_mode_allowed_tool(&call.name) => (
                PermissionDecision::Allow,
                PermissionScope::Session,
                Some("plan mode allowed a read-only or coordination tool".to_string()),
                Some("plan_mode_allowed_read_only_or_coordination_tool".to_string()),
            ),
            PermissionDecision::Ask { .. } if is_plan_mode_allowed_tool(&call.name) => {
                (base_decision, base_scope, base_reason, None)
            }
            _ => (
                PermissionDecision::Deny {
                    reason: "plan mode only allows read-only and coordination tools".to_string(),
                },
                PermissionScope::Session,
                Some("plan mode only allows read-only and coordination tools".to_string()),
                Some("plan_mode_denied_non_whitelisted_tool".to_string()),
            ),
        },
        PermissionMode::AcceptEdits if is_edit_tool(&call.name) => match base_decision {
            PermissionDecision::Ask { .. } => (
                PermissionDecision::Allow,
                PermissionScope::Session,
                Some("accept edits mode".to_string()),
                Some("accept_edits_auto_allowed_edit_approval".to_string()),
            ),
            other => (other, base_scope, base_reason, None),
        },
        PermissionMode::DontAsk => match base_decision {
            PermissionDecision::Ask { .. } => (
                PermissionDecision::Deny {
                    reason: "dontAsk mode blocked a tool requiring approval".to_string(),
                },
                base_scope,
                Some(base_reason.unwrap_or_else(|| {
                    "dontAsk mode blocked a tool requiring approval".to_string()
                })),
                Some("dont_ask_denied_approval".to_string()),
            ),
            other => (other, base_scope, base_reason, None),
        },
        _ => (base_decision, base_scope, base_reason, None),
    }
}

fn is_edit_tool(tool_name: &str) -> bool {
    matches!(tool_name, "write_file" | "edit_file" | "apply_patch")
}

fn is_plan_mode_allowed_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "read_file"
            | "list_files"
            | "glob_search"
            | "grep_search"
            | "web_fetch"
            | "web_search"
            | "list_mcp_resources"
            | "list_mcp_resource_templates"
            | "read_mcp_resource"
            | "list_skills"
            | "read_channel_thread"
            | "ask_operator"
            | "ask_user_question"
            | "request_parent_clarification"
            | "spawn_agent"
            | "list_agents"
            | "list_agent_summaries"
            | "message_agent"
            | "wait_agent"
            | "get_agent"
            | "enter_plan_mode"
            | "exit_plan_mode"
            | "task_get"
            | "task_list"
            | "task_output"
            | "schedule_get"
            | "schedule_list"
            | "get_goal"
    )
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::Result;
    use serde_json::json;

    use super::{
        PermissionBehavior, PermissionEngine, PermissionRule, PermissionScope,
        SessionPermissionUpdateStore,
    };
    use crate::observability::InMemoryObserver;

    #[derive(Default)]
    struct TestSessionPermissionStore {
        fail: bool,
        delay_ms: u64,
        persisted: Mutex<Vec<(String, Vec<kheish_types::HookPermissionUpdate>)>>,
    }

    #[async_trait::async_trait]
    impl SessionPermissionUpdateStore for TestSessionPermissionStore {
        async fn persist_session_rule_updates(
            &self,
            session_id: &str,
            updates: &[kheish_types::HookPermissionUpdate],
        ) -> Result<()> {
            if self.fail {
                anyhow::bail!("persist failed");
            }
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
            self.persisted
                .lock()
                .push((session_id.to_string(), updates.to_vec()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn session_scope_overrides_project_scope() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(
            vec![],
            vec![PermissionRule {
                scope: PermissionScope::Project,
                tool_name_pattern: "write*".to_string(),
                behavior: PermissionBehavior::Deny,
                reason: Some("project denies writes".to_string()),
            }],
            vec![PermissionRule {
                scope: PermissionScope::Session,
                tool_name_pattern: "write_file".to_string(),
                behavior: PermissionBehavior::Ask,
                reason: Some("session asks".to_string()),
            }],
            observer,
        );

        let denied = engine
            .check(&kheish_types::ToolCallRecord {
                id: "1".to_string(),
                name: "write_file".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;
        assert!(matches!(
            denied,
            kheish_types::PermissionDecision::Ask { .. }
        ));

        let still_asks = engine
            .check(&kheish_types::ToolCallRecord {
                id: "2".to_string(),
                name: "write_file".to_string(),
                input: json!({"justification": "user approved"}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;
        assert!(matches!(
            still_asks,
            kheish_types::PermissionDecision::Ask { .. }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn permission_precedence_is_scope_then_pattern_specificity() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(
            vec![PermissionRule {
                scope: PermissionScope::User,
                tool_name_pattern: "bash".to_string(),
                behavior: PermissionBehavior::Allow,
                reason: Some("user allows bash".to_string()),
            }],
            vec![
                PermissionRule {
                    scope: PermissionScope::Project,
                    tool_name_pattern: "*".to_string(),
                    behavior: PermissionBehavior::Deny,
                    reason: Some("project wildcard deny".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Project,
                    tool_name_pattern: "mcp__*".to_string(),
                    behavior: PermissionBehavior::Deny,
                    reason: Some("project prefix deny".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Project,
                    tool_name_pattern: "mcp__linear__*".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("project longest prefix ask".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Project,
                    tool_name_pattern: "bash".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("project exact ask".to_string()),
                },
            ],
            vec![],
            observer,
        );

        let bash = engine.explain(&kheish_types::ToolCallRecord {
            id: "precedence-bash".to_string(),
            name: "bash".to_string(),
            input: json!({}),
            assistant_message_id: None,
            assistant_provider_response_id: None,
        });
        assert_eq!(bash.scope, "project");
        assert_eq!(bash.decision, "ask");
        assert_eq!(bash.base_decision.as_deref(), Some("ask"));
        assert_eq!(bash.mode_applied, Some(false));
        assert_eq!(
            bash.matched_rule
                .as_ref()
                .map(|rule| rule.tool_name_pattern.as_str()),
            Some("bash")
        );
        assert_eq!(bash.reason.as_deref(), Some("project exact ask"));

        let read = engine.explain(&kheish_types::ToolCallRecord {
            id: "precedence-read".to_string(),
            name: "read_file".to_string(),
            input: json!({}),
            assistant_message_id: None,
            assistant_provider_response_id: None,
        });
        assert_eq!(read.scope, "project");
        assert_eq!(read.decision, "deny");
        assert_eq!(read.reason.as_deref(), Some("project wildcard deny"));

        let mcp = engine.explain(&kheish_types::ToolCallRecord {
            id: "precedence-mcp".to_string(),
            name: "mcp__linear__create_issue".to_string(),
            input: json!({}),
            assistant_message_id: None,
            assistant_provider_response_id: None,
        });
        assert_eq!(mcp.scope, "project");
        assert_eq!(mcp.decision, "ask");
        assert_eq!(
            mcp.matched_rule
                .as_ref()
                .map(|rule| rule.tool_name_pattern.as_str()),
            Some("mcp__linear__*")
        );
        assert_eq!(mcp.reason.as_deref(), Some("project longest prefix ask"));
        Ok(())
    }

    #[tokio::test]
    async fn bypass_permissions_preserves_explicit_denies() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(
            vec![],
            vec![],
            vec![PermissionRule {
                scope: PermissionScope::Session,
                tool_name_pattern: "bash".to_string(),
                behavior: PermissionBehavior::Deny,
                reason: Some("session denies bash".to_string()),
            }],
            observer,
        );
        engine.set_mode(super::PermissionMode::BypassPermissions);

        let denied = engine
            .check(&kheish_types::ToolCallRecord {
                id: "deny-1".to_string(),
                name: "bash".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;

        assert!(matches!(
            denied,
            kheish_types::PermissionDecision::Deny { .. }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn accept_edits_preserves_explicit_denies() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(
            vec![],
            vec![],
            vec![PermissionRule {
                scope: PermissionScope::Session,
                tool_name_pattern: "write_file".to_string(),
                behavior: PermissionBehavior::Deny,
                reason: Some("session denies writes".to_string()),
            }],
            observer,
        );
        engine.set_mode(super::PermissionMode::AcceptEdits);

        let denied = engine
            .check(&kheish_types::ToolCallRecord {
                id: "deny-edit-1".to_string(),
                name: "write_file".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;

        assert!(matches!(
            denied,
            kheish_types::PermissionDecision::Deny { .. }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn plan_mode_preserves_explicit_denies() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(
            vec![],
            vec![],
            vec![
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "bash".to_string(),
                    behavior: PermissionBehavior::Deny,
                    reason: Some("session denies bash".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "*".to_string(),
                    behavior: PermissionBehavior::Allow,
                    reason: None,
                },
            ],
            observer,
        );
        engine.set_mode(super::PermissionMode::Plan);

        let call = kheish_types::ToolCallRecord {
            id: "plan-explicit-deny".to_string(),
            name: "bash".to_string(),
            input: json!({}),
            assistant_message_id: None,
            assistant_provider_response_id: None,
        };
        let denied = engine.check(&call).await?;
        assert!(matches!(
            denied,
            kheish_types::PermissionDecision::Deny { .. }
        ));
        let explanation = engine.explain(&call);
        assert_eq!(explanation.decision, "deny");
        assert_eq!(explanation.base_decision.as_deref(), Some("deny"));
        assert_eq!(explanation.mode_applied, Some(false));
        assert_eq!(explanation.mode_effect, None);
        assert_eq!(explanation.scope, "session");
        assert_eq!(explanation.reason.as_deref(), Some("session denies bash"));
        assert_eq!(
            explanation.audit_preview.base_decision.as_deref(),
            Some("deny")
        );
        assert_eq!(
            explanation.audit_preview.effective_mode.as_deref(),
            Some("plan")
        );
        assert_eq!(
            explanation.audit_preview.matched_rule_pattern.as_deref(),
            Some("bash")
        );
        Ok(())
    }

    #[tokio::test]
    async fn accept_edits_auto_allows_edit_asks_only() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(
            vec![],
            vec![],
            vec![
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "write_file".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("session asks writes".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "bash".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("session asks bash".to_string()),
                },
            ],
            observer,
        );
        engine.set_mode(super::PermissionMode::AcceptEdits);

        let write = engine
            .check(&kheish_types::ToolCallRecord {
                id: "ask-edit-1".to_string(),
                name: "write_file".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;
        let bash = engine
            .check(&kheish_types::ToolCallRecord {
                id: "ask-bash-1".to_string(),
                name: "bash".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;

        assert!(matches!(write, kheish_types::PermissionDecision::Allow));
        assert!(matches!(bash, kheish_types::PermissionDecision::Ask { .. }));
        Ok(())
    }

    #[test]
    fn permission_explanation_decodes_legacy_dry_run_responses() -> Result<()> {
        let explanation: super::PermissionExplanation = serde_json::from_value(json!({
            "tool_call_id": "legacy-call",
            "tool_name": "bash",
            "effective_mode": "default",
            "decision": "ask",
            "approval_required": true,
            "hooks_evaluated": false,
            "scope": "session",
            "reason": "shell command requires approval",
            "audit_preview": {
                "scope": "session",
                "tool_name": "bash",
                "tool_call_id": "legacy-call",
                "decision": "ask",
                "justification": null,
                "reason": "shell command requires approval",
                "approval_request_id": null
            }
        }))?;

        assert_eq!(explanation.base_decision, None);
        assert_eq!(explanation.mode_applied, None);
        assert_eq!(explanation.mode_effect, None);
        assert_eq!(explanation.matched_rule, None);
        assert_eq!(explanation.hook_policy, "not_reported_by_older_daemon");
        assert_eq!(explanation.audit_preview.base_decision, None);
        assert_eq!(explanation.audit_preview.effective_mode, None);
        assert_eq!(explanation.audit_preview.mode_effect, None);
        assert_eq!(explanation.audit_preview.matched_rule_pattern, None);
        Ok(())
    }

    #[test]
    fn permission_modes_have_an_explicit_tool_matrix() {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(
            vec![],
            vec![],
            vec![
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "bash".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("shell command requires approval".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "write_file".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("file write requires approval".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "edit_file".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("file edit requires approval".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "apply_patch".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("file patch requires approval".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "mcp__*".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("MCP tool calls require approval".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "list_mcp_resources".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("MCP resource listing requires approval".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "list_mcp_resource_templates".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("MCP resource template listing requires approval".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "read_mcp_resource".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("MCP resource reads require approval".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "exit_plan_mode".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("leaving plan mode requires approval".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "*".to_string(),
                    behavior: PermissionBehavior::Allow,
                    reason: None,
                },
            ],
            observer,
        );

        let cases = [
            (
                super::PermissionMode::Default,
                [
                    ("bash", "ask"),
                    ("write_file", "ask"),
                    ("edit_file", "ask"),
                    ("apply_patch", "ask"),
                    ("read_file", "allow"),
                    ("task_get", "allow"),
                    ("task_create", "allow"),
                    ("mcp__linear__create_issue", "ask"),
                    ("list_mcp_resources", "ask"),
                    ("read_mcp_resource", "ask"),
                    ("exit_plan_mode", "ask"),
                ],
            ),
            (
                super::PermissionMode::BypassPermissions,
                [
                    ("bash", "allow"),
                    ("write_file", "allow"),
                    ("edit_file", "allow"),
                    ("apply_patch", "allow"),
                    ("read_file", "allow"),
                    ("task_get", "allow"),
                    ("task_create", "allow"),
                    ("mcp__linear__create_issue", "allow"),
                    ("list_mcp_resources", "allow"),
                    ("read_mcp_resource", "allow"),
                    ("exit_plan_mode", "allow"),
                ],
            ),
            (
                super::PermissionMode::AcceptEdits,
                [
                    ("bash", "ask"),
                    ("write_file", "allow"),
                    ("edit_file", "allow"),
                    ("apply_patch", "allow"),
                    ("read_file", "allow"),
                    ("task_get", "allow"),
                    ("task_create", "allow"),
                    ("mcp__linear__create_issue", "ask"),
                    ("list_mcp_resources", "ask"),
                    ("read_mcp_resource", "ask"),
                    ("exit_plan_mode", "ask"),
                ],
            ),
            (
                super::PermissionMode::DontAsk,
                [
                    ("bash", "deny"),
                    ("write_file", "deny"),
                    ("edit_file", "deny"),
                    ("apply_patch", "deny"),
                    ("read_file", "allow"),
                    ("task_get", "allow"),
                    ("task_create", "allow"),
                    ("mcp__linear__create_issue", "deny"),
                    ("list_mcp_resources", "deny"),
                    ("read_mcp_resource", "deny"),
                    ("exit_plan_mode", "deny"),
                ],
            ),
            (
                super::PermissionMode::Plan,
                [
                    ("bash", "deny"),
                    ("write_file", "deny"),
                    ("edit_file", "deny"),
                    ("apply_patch", "deny"),
                    ("read_file", "allow"),
                    ("task_get", "allow"),
                    ("task_create", "deny"),
                    ("mcp__linear__create_issue", "deny"),
                    ("list_mcp_resources", "ask"),
                    ("read_mcp_resource", "ask"),
                    ("exit_plan_mode", "ask"),
                ],
            ),
        ];

        for (mode, expected) in cases {
            engine.set_mode(mode.clone());
            for (tool_name, expected_decision) in expected {
                let explanation = engine.explain(&kheish_types::ToolCallRecord {
                    id: format!("{mode:?}-{tool_name}"),
                    name: tool_name.to_string(),
                    input: json!({}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                });
                assert_eq!(
                    explanation.decision, expected_decision,
                    "unexpected decision for mode {mode:?} and tool {tool_name}"
                );
                assert!(
                    !explanation.mode_applied.unwrap_or(false) || explanation.mode_effect.is_some(),
                    "mode-applied explanations must name the concrete effect"
                );
            }
        }
    }

    #[tokio::test]
    async fn explain_is_dry_run_and_does_not_consume_approval_ids() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(
            vec![],
            vec![],
            vec![PermissionRule {
                scope: PermissionScope::Session,
                tool_name_pattern: "bash".to_string(),
                behavior: PermissionBehavior::Ask,
                reason: Some("session asks bash".to_string()),
            }],
            observer,
        );

        let call = kheish_types::ToolCallRecord {
            id: "bash-dry-run".to_string(),
            name: "bash".to_string(),
            input: json!({"command": "touch should-not-run"}),
            assistant_message_id: None,
            assistant_provider_response_id: None,
        };
        let explanation = engine.explain(&call);
        assert_eq!(explanation.decision, "ask");
        assert_eq!(explanation.base_decision.as_deref(), Some("ask"));
        assert_eq!(explanation.mode_applied, Some(false));
        assert_eq!(
            explanation
                .matched_rule
                .as_ref()
                .map(|rule| rule.tool_name_pattern.as_str()),
            Some("bash")
        );
        assert!(explanation.approval_required);
        assert!(!explanation.hooks_evaluated);
        assert_eq!(explanation.hook_policy, "not_executed_in_dry_run");
        assert_eq!(explanation.audit_preview.approval_request_id, None);
        assert_eq!(
            explanation.audit_preview.tool_call_id.as_deref(),
            Some("bash-dry-run")
        );
        assert!(engine.drain_audits().is_empty());

        let real = engine.check(&call).await?;
        match real {
            kheish_types::PermissionDecision::Ask { request } => {
                assert!(request.id.starts_with("approval-bash-1-"));
            }
            other => panic!("unexpected decision after dry-run: {other:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn approval_ids_include_tool_call_identity_across_counter_restarts() -> Result<()> {
        let ask_rule = PermissionRule {
            scope: PermissionScope::Session,
            tool_name_pattern: "bash".to_string(),
            behavior: PermissionBehavior::Ask,
            reason: Some("ask".to_string()),
        };
        let decision_for = |tool_call_id: &str| {
            let engine = PermissionEngine::new(
                vec![],
                vec![],
                vec![ask_rule.clone()],
                InMemoryObserver::shared(),
            );
            let call = kheish_types::ToolCallRecord {
                id: tool_call_id.to_string(),
                name: "bash".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            };
            async move { engine.check(&call).await }
        };

        let first = match decision_for("provider-call-1").await? {
            kheish_types::PermissionDecision::Ask { request } => request.id,
            other => panic!("unexpected first decision: {other:?}"),
        };
        let second = match decision_for("provider-call-2").await? {
            kheish_types::PermissionDecision::Ask { request } => request.id,
            other => panic!("unexpected second decision: {other:?}"),
        };

        assert!(first.starts_with("approval-bash-1-"));
        assert!(second.starts_with("approval-bash-1-"));
        assert_ne!(first, second);
        Ok(())
    }

    #[tokio::test]
    async fn final_permission_audit_replaces_pre_hook_decision() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(
            vec![],
            vec![],
            vec![PermissionRule {
                scope: PermissionScope::Session,
                tool_name_pattern: "write_file".to_string(),
                behavior: PermissionBehavior::Ask,
                reason: Some("session asks writes".to_string()),
            }],
            observer,
        );
        let call = kheish_types::ToolCallRecord {
            id: "write-hook-allow".to_string(),
            name: "write_file".to_string(),
            input: json!({"path": "demo.txt", "content": "hello"}),
            assistant_message_id: None,
            assistant_provider_response_id: None,
        };

        crate::scope_execution(
            crate::ExecutionScope {
                session_id: "session-hook-audit".to_string(),
                ..crate::ExecutionScope::default()
            },
            tokio_util::sync::CancellationToken::new(),
            async {
                let base = engine.check(&call).await?;
                assert!(matches!(base, kheish_types::PermissionDecision::Ask { .. }));
                kheish_core::PermissionGate::record_final_decision(
                    &engine,
                    &call,
                    &kheish_types::PermissionDecision::Allow,
                    Some("allowed by permission hook".to_string()),
                )
                .await
            },
        )
        .await?;

        let audits = engine.drain_audits_for_session("session-hook-audit");
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].tool_call_id.as_deref(), Some("write-hook-allow"));
        assert_eq!(audits[0].tool_name, "write_file");
        assert_eq!(audits[0].decision, "allow");
        assert_eq!(audits[0].approval_request_id, None);
        assert_eq!(
            audits[0].reason.as_deref(),
            Some("allowed by permission hook")
        );
        Ok(())
    }

    #[tokio::test]
    async fn matched_rule_origin_distinguishes_hook_permission_updates() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(
            vec![],
            vec![],
            vec![PermissionRule {
                scope: PermissionScope::Session,
                tool_name_pattern: "write_file".to_string(),
                behavior: PermissionBehavior::Ask,
                reason: Some("static session asks".to_string()),
            }],
            observer,
        );

        let call = kheish_types::ToolCallRecord {
            id: "origin-write".to_string(),
            name: "write_file".to_string(),
            input: json!({"path": "demo.txt", "content": "hello"}),
            assistant_message_id: None,
            assistant_provider_response_id: None,
        };

        let before = engine.explain(&call);
        assert_eq!(before.matched_rule_origin.as_deref(), Some("static"));
        assert_eq!(
            before.audit_preview.matched_rule_origin.as_deref(),
            Some("static")
        );

        crate::scope_execution(
            crate::ExecutionScope {
                session_id: "session-origin".to_string(),
                ..crate::ExecutionScope::default()
            },
            tokio_util::sync::CancellationToken::new(),
            async {
                kheish_core::PermissionGate::apply_hook_permission_updates(
                    &engine,
                    "session-origin",
                    &[kheish_types::HookPermissionUpdate {
                        scope: kheish_types::HookPermissionUpdateScope::Session,
                        tool_name_pattern: "write_file".to_string(),
                        behavior: kheish_types::HookPermissionUpdateBehavior::Allow,
                        reason: Some("sticky allow".to_string()),
                    }],
                )
                .await?;
                let after = engine.explain(&call);
                assert_eq!(after.decision, "allow");
                assert_eq!(after.matched_rule_origin.as_deref(), Some("hook_update"));
                assert_eq!(
                    after.audit_preview.matched_rule_origin.as_deref(),
                    Some("hook_update")
                );
                Ok::<_, anyhow::Error>(())
            },
        )
        .await?;

        Ok(())
    }

    #[tokio::test]
    async fn explain_reports_accept_edits_without_allowing_bash() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(
            vec![],
            vec![],
            vec![
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "write_file".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("session asks writes".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "bash".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("session asks bash".to_string()),
                },
            ],
            observer,
        );
        engine.set_mode(super::PermissionMode::AcceptEdits);

        let write = engine.explain(&kheish_types::ToolCallRecord {
            id: "write-dry-run".to_string(),
            name: "write_file".to_string(),
            input: json!({"path": "demo.txt", "content": "hello"}),
            assistant_message_id: None,
            assistant_provider_response_id: None,
        });
        let bash = engine.explain(&kheish_types::ToolCallRecord {
            id: "bash-dry-run".to_string(),
            name: "bash".to_string(),
            input: json!({"command": "touch demo.txt"}),
            assistant_message_id: None,
            assistant_provider_response_id: None,
        });

        assert_eq!(write.effective_mode, super::PermissionMode::AcceptEdits);
        assert_eq!(write.decision, "allow");
        assert!(!write.approval_required);
        assert_eq!(write.reason.as_deref(), Some("accept edits mode"));
        assert_eq!(bash.effective_mode, super::PermissionMode::AcceptEdits);
        assert_eq!(bash.decision, "ask");
        assert!(bash.approval_required);
        Ok(())
    }

    #[test]
    fn plan_mode_allows_structured_user_questions() {
        assert!(super::is_plan_mode_allowed_tool("ask_operator"));
        assert!(super::is_plan_mode_allowed_tool("ask_user_question"));
        assert!(super::is_plan_mode_allowed_tool("read_channel_thread"));
        assert!(super::is_plan_mode_allowed_tool(
            "request_parent_clarification"
        ));
        assert!(!super::is_plan_mode_allowed_tool("list_and_delete"));
        assert!(!super::is_plan_mode_allowed_tool("read_then_write"));
    }

    #[tokio::test]
    async fn explain_with_mode_override_is_dry_run_only() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(
            vec![],
            vec![],
            vec![PermissionRule {
                scope: PermissionScope::Session,
                tool_name_pattern: "bash".to_string(),
                behavior: PermissionBehavior::Ask,
                reason: Some("session asks bash".to_string()),
            }],
            observer,
        );

        let call = kheish_types::ToolCallRecord {
            id: "mode-override-dry-run".to_string(),
            name: "bash".to_string(),
            input: json!({"command": "echo hi"}),
            assistant_message_id: None,
            assistant_provider_response_id: None,
        };
        let explanation =
            engine.explain_with_mode(&call, Some(super::PermissionMode::BypassPermissions));
        assert_eq!(
            explanation.effective_mode,
            super::PermissionMode::BypassPermissions
        );
        assert_eq!(explanation.decision, "allow");
        assert_eq!(engine.mode(), super::PermissionMode::Default);
        assert!(engine.drain_audits().is_empty());

        let real = engine.check(&call).await?;
        let request_id = match real {
            kheish_types::PermissionDecision::Ask { request } => request.id,
            other => panic!("expected real evaluation to ask, got {other:?}"),
        };
        assert!(request_id.starts_with("approval-bash-1-"));
        Ok(())
    }

    #[tokio::test]
    async fn plan_mode_preserves_exit_plan_approval_and_explicit_denies() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(
            vec![],
            vec![],
            vec![
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "exit_plan_mode".to_string(),
                    behavior: PermissionBehavior::Ask,
                    reason: Some("leaving plan mode requires approval".to_string()),
                },
                PermissionRule {
                    scope: PermissionScope::Session,
                    tool_name_pattern: "read_file".to_string(),
                    behavior: PermissionBehavior::Deny,
                    reason: Some("session denies read".to_string()),
                },
            ],
            observer,
        );
        engine.set_mode(super::PermissionMode::Plan);

        let exit = engine
            .check(&kheish_types::ToolCallRecord {
                id: "exit-plan".to_string(),
                name: "exit_plan_mode".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;
        let read = engine
            .check(&kheish_types::ToolCallRecord {
                id: "read".to_string(),
                name: "read_file".to_string(),
                input: json!({"path": "README.md"}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;

        assert!(matches!(exit, kheish_types::PermissionDecision::Ask { .. }));
        assert!(matches!(
            read,
            kheish_types::PermissionDecision::Deny { .. }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn plan_mode_denies_task_mutations_but_allows_task_reads() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(vec![], vec![], vec![], observer);
        engine.set_mode(super::PermissionMode::Plan);

        for tool_name in ["task_create", "task_update", "task_stop", "task_delete"] {
            let decision = engine
                .check(&kheish_types::ToolCallRecord {
                    id: format!("{tool_name}-dry-run"),
                    name: tool_name.to_string(),
                    input: json!({}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                })
                .await?;
            assert!(
                matches!(decision, kheish_types::PermissionDecision::Deny { .. }),
                "{tool_name} should be denied in plan mode"
            );
        }

        for tool_name in [
            "task_get",
            "task_list",
            "task_output",
            "schedule_get",
            "schedule_list",
            "get_goal",
        ] {
            let decision = engine
                .check(&kheish_types::ToolCallRecord {
                    id: format!("{tool_name}-dry-run"),
                    name: tool_name.to_string(),
                    input: json!({}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                })
                .await?;
            assert_eq!(decision, kheish_types::PermissionDecision::Allow);
        }
        Ok(())
    }

    #[tokio::test]
    async fn permission_audits_are_drained_by_session() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = Arc::new(PermissionEngine::new(
            vec![],
            vec![],
            vec![PermissionRule {
                scope: PermissionScope::Session,
                tool_name_pattern: "bash".to_string(),
                behavior: PermissionBehavior::Ask,
                reason: Some("session asks bash".to_string()),
            }],
            observer,
        ));
        for session_id in ["session-a", "session-b"] {
            let engine = engine.clone();
            crate::scope_execution(
                crate::ExecutionScope {
                    session_id: session_id.to_string(),
                    ..crate::ExecutionScope::default()
                },
                tokio_util::sync::CancellationToken::new(),
                async move {
                    engine
                        .check(&kheish_types::ToolCallRecord {
                            id: format!("{session_id}-bash"),
                            name: "bash".to_string(),
                            input: json!({"command": "echo hi"}),
                            assistant_message_id: None,
                            assistant_provider_response_id: None,
                        })
                        .await
                },
            )
            .await?;
        }

        let session_a = engine.drain_audits_for_session("session-a");
        assert_eq!(session_a.len(), 1);
        assert_eq!(session_a[0].tool_name, "bash");
        let session_a_again = engine.drain_audits_for_session("session-a");
        assert!(session_a_again.is_empty());
        let session_b = engine.drain_audits_for_session("session-b");
        assert_eq!(session_b.len(), 1);
        assert_eq!(session_b[0].tool_name, "bash");
        Ok(())
    }

    #[tokio::test]
    async fn session_rule_updates_roll_back_when_store_persist_fails() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(vec![], vec![], vec![], observer);
        engine.bind_session_rule_update_store(Arc::new(TestSessionPermissionStore {
            fail: true,
            delay_ms: 0,
            persisted: Mutex::new(Vec::new()),
        }));

        let error = kheish_core::PermissionGate::apply_hook_permission_updates(
            &engine,
            "session-a",
            &[kheish_types::HookPermissionUpdate {
                scope: kheish_types::HookPermissionUpdateScope::Session,
                tool_name_pattern: "echo".to_string(),
                behavior: kheish_types::HookPermissionUpdateBehavior::Allow,
                reason: Some("sticky".to_string()),
            }],
        )
        .await
        .expect_err("store failure should bubble up");

        assert!(error.to_string().contains("persist failed"));
        assert!(engine.session_rule_updates("session-a").is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn hook_permission_updates_reject_non_session_scopes() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let engine = PermissionEngine::new(vec![], vec![], vec![], observer);
        engine.bind_session_rule_update_store(Arc::new(TestSessionPermissionStore {
            fail: false,
            delay_ms: 0,
            persisted: Mutex::new(Vec::new()),
        }));

        let error = kheish_core::PermissionGate::apply_hook_permission_updates(
            &engine,
            "session-a",
            &[
                kheish_types::HookPermissionUpdate {
                    scope: kheish_types::HookPermissionUpdateScope::Project,
                    tool_name_pattern: "bash".to_string(),
                    behavior: kheish_types::HookPermissionUpdateBehavior::Deny,
                    reason: Some("deny bash".to_string()),
                },
                kheish_types::HookPermissionUpdate {
                    scope: kheish_types::HookPermissionUpdateScope::Session,
                    tool_name_pattern: "echo".to_string(),
                    behavior: kheish_types::HookPermissionUpdateBehavior::Allow,
                    reason: Some("allow echo".to_string()),
                },
            ],
        )
        .await
        .expect_err("non-session hook updates should be rejected");

        assert!(
            error.to_string().contains("only support session scope"),
            "{error}"
        );
        assert!(engine.session_rule_updates("session-a").is_empty());
        let bash = engine
            .check(&kheish_types::ToolCallRecord {
                id: "check-project-rollback".to_string(),
                name: "bash".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;
        assert_eq!(bash, kheish_types::PermissionDecision::Allow);
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_session_updates_persist_the_merged_snapshot() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let store = Arc::new(TestSessionPermissionStore {
            fail: false,
            delay_ms: 25,
            persisted: Mutex::new(Vec::new()),
        });
        let engine = Arc::new(PermissionEngine::new(vec![], vec![], vec![], observer));
        engine.bind_session_rule_update_store(store.clone());

        let first = {
            let engine = engine.clone();
            tokio::spawn(async move {
                kheish_core::PermissionGate::apply_hook_permission_updates(
                    engine.as_ref(),
                    "session-a",
                    &[kheish_types::HookPermissionUpdate {
                        scope: kheish_types::HookPermissionUpdateScope::Session,
                        tool_name_pattern: "echo".to_string(),
                        behavior: kheish_types::HookPermissionUpdateBehavior::Allow,
                        reason: Some("allow echo".to_string()),
                    }],
                )
                .await
            })
        };
        let second = {
            let engine = engine.clone();
            tokio::spawn(async move {
                kheish_core::PermissionGate::apply_hook_permission_updates(
                    engine.as_ref(),
                    "session-a",
                    &[kheish_types::HookPermissionUpdate {
                        scope: kheish_types::HookPermissionUpdateScope::Session,
                        tool_name_pattern: "bash".to_string(),
                        behavior: kheish_types::HookPermissionUpdateBehavior::Allow,
                        reason: Some("allow bash".to_string()),
                    }],
                )
                .await
            })
        };

        first.await??;
        second.await??;

        let persisted = store.persisted.lock();
        let (_, latest) = persisted.last().expect("expected persisted snapshots");
        assert_eq!(latest.len(), 2);
        assert!(
            latest
                .iter()
                .any(|update| update.tool_name_pattern == "echo")
        );
        assert!(
            latest
                .iter()
                .any(|update| update.tool_name_pattern == "bash")
        );
        let updates = engine.session_rule_updates("session-a");
        assert_eq!(updates.len(), 2);
        Ok(())
    }
}
