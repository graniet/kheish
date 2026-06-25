//! Daemon-owned policy settings, evaluation, and queueing for learning publication workflows.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Result, bail};
use kheish_types::{
    LearningPolicyDecision, LearningPublishTier, LearningStatus, LearningVerificationStatus,
};
use tokio::sync::{Mutex, Notify};

use crate::learning::{
    LearningAutomationMode, LearningAutomationPolicyConfig, LearningAutomationReview,
    LearningCandidateOrigin, LearningCandidateView, LearningJudgeReview, LearningPublicationAction,
    LearningPublicationPolicy, LearningPublicationRule, LearningView,
    learning_content_has_secret_material,
};
use crate::procedural_skills::LearningSkillDraft;
use crate::state_files::{quarantine_corrupt_state_file, read_json_or_quarantine};

const LEARNING_POLICY_FILENAME: &str = "learning-policy.json";

/// The authority that initiated one durable learning-plane mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum LearningMutationMode {
    /// The mutation came from an operator-facing API or CLI request.
    Manual,
    /// The mutation came from a daemon-owned autonomous worker.
    Automatic,
}

/// One persisted policy-evaluation result returned by the daemon-owned learning worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LearningAutomationEvaluation {
    pub(crate) mode: LearningAutomationMode,
    pub(crate) action: LearningPublicationAction,
    pub(crate) matched_rule_name: Option<String>,
    pub(crate) judge_review: Option<LearningJudgeReview>,
    pub(crate) reason: String,
    pub(crate) expires_at_ms: Option<u64>,
}

impl LearningAutomationEvaluation {
    /// Builds one candidate review payload for audit and diagnostics.
    pub(crate) fn review(&self, reviewed_at_ms: u64) -> LearningAutomationReview {
        LearningAutomationReview {
            mode: self.mode.clone(),
            action: self.action.clone(),
            reviewed_at_ms,
            matched_rule_name: self.matched_rule_name.clone(),
            judge: self.judge_review.clone(),
            reason: self.reason.clone(),
        }
    }
}

/// Stores daemon-owned learning automation settings on disk.
#[derive(Clone, Debug)]
struct FileLearningPolicyStore {
    path: PathBuf,
}

impl FileLearningPolicyStore {
    fn new(root: impl AsRef<Path>) -> Self {
        Self {
            path: root.as_ref().join(LEARNING_POLICY_FILENAME),
        }
    }

    fn load(&self) -> Result<LearningAutomationPolicyConfig> {
        let Some(settings) = read_json_or_quarantine(&self.path, "learning policy")? else {
            return Ok(LearningAutomationPolicyConfig::default());
        };
        if let Err(error) = validate_learning_policy_config(&settings) {
            let quarantined_path = quarantine_corrupt_state_file(&self.path)?;
            tracing::warn!(
                path = %self.path.display(),
                quarantined_path = quarantined_path.as_ref().map(|value| value.display().to_string()),
                error = %error,
                "ignoring invalid legacy learning policy state file"
            );
            return Ok(LearningAutomationPolicyConfig::default());
        }
        Ok(settings)
    }
}

/// Normalizes durable learning mutations and owns autonomous publication settings.
#[derive(Clone)]
pub(crate) struct LearningPolicyService {
    settings: Arc<RwLock<LearningAutomationPolicyConfig>>,
    queued_candidate_ids: Arc<Mutex<BTreeSet<String>>>,
    queue: Arc<Mutex<VecDeque<String>>>,
    notify: Arc<Notify>,
}

impl LearningPolicyService {
    /// Creates a new daemon-owned learning policy service rooted in one state directory.
    pub(crate) fn new(state_root: impl AsRef<Path>) -> Result<Self> {
        let store = FileLearningPolicyStore::new(state_root);
        let settings = store.load()?;
        Ok(Self {
            settings: Arc::new(RwLock::new(settings)),
            queued_candidate_ids: Arc::new(Mutex::new(BTreeSet::new())),
            queue: Arc::new(Mutex::new(VecDeque::new())),
            notify: Arc::new(Notify::new()),
        })
    }

    /// Returns the currently persisted automation settings.
    pub(crate) fn settings(&self) -> LearningAutomationPolicyConfig {
        self.settings
            .read()
            .expect("learning policy settings rwlock poisoned")
            .clone()
    }

    /// Validates one replacement automation policy without activating it.
    pub(crate) fn validate_settings(
        &self,
        settings: &LearningAutomationPolicyConfig,
    ) -> Result<()> {
        validate_learning_policy_config(settings)
    }

    /// Activates one replacement automation policy without writing the legacy sidecar file.
    pub(crate) async fn activate_settings(
        &self,
        settings: LearningAutomationPolicyConfig,
    ) -> Result<LearningAutomationPolicyConfig> {
        validate_learning_policy_config(&settings)?;
        *self
            .settings
            .write()
            .expect("learning policy settings rwlock poisoned") = settings.clone();
        self.notify.notify_waiters();
        Ok(settings)
    }

    /// Returns whether terminal runs should emit session-scoped run-summary candidates.
    pub(crate) async fn capture_run_summary_candidates(&self) -> bool {
        self.settings().capture.run_summary_candidates
    }

    /// Returns the current semantic extraction settings when daemon-owned capture is enabled.
    pub(crate) fn semantic_capture_settings(
        &self,
    ) -> Option<crate::learning::LearningSemanticCaptureConfig> {
        let settings = self.settings();
        settings
            .capture
            .semantic_candidates
            .enabled
            .then_some(settings.capture.semantic_candidates)
    }

    /// Queues one candidate for asynchronous policy evaluation.
    pub(crate) async fn enqueue_candidate(&self, candidate_id: impl Into<String>) {
        let candidate_id = candidate_id.into();
        let mut queued = self.queued_candidate_ids.lock().await;
        if !queued.insert(candidate_id.clone()) {
            return;
        }
        drop(queued);
        self.queue.lock().await.push_back(candidate_id);
        self.notify.notify_one();
    }

    /// Returns one queued candidate identifier when available.
    pub(crate) async fn dequeue_candidate(&self) -> Option<String> {
        let candidate_id = self.queue.lock().await.pop_front();
        if let Some(candidate_id) = candidate_id {
            self.queued_candidate_ids.lock().await.remove(&candidate_id);
            return Some(candidate_id);
        }
        None
    }

    /// Returns the queue notifier used by the publication worker.
    pub(crate) fn notify(&self) -> Arc<Notify> {
        self.notify.clone()
    }

    /// Evaluates one candidate against the current autonomous publication policy.
    pub(crate) async fn evaluate_candidate(
        &self,
        candidate: &LearningCandidateView,
        reviewed_at_ms: u64,
    ) -> Option<LearningAutomationEvaluation> {
        let settings = self.settings();
        match settings.mode {
            LearningAutomationMode::ManualOnly => None,
            LearningAutomationMode::Shadow | LearningAutomationMode::Enabled => {
                Some(evaluate_candidate_against_policy(
                    candidate,
                    &settings.publication,
                    settings.mode,
                    reviewed_at_ms,
                ))
            }
        }
    }

    /// Builds one automatic learning publication from a candidate and policy evaluation.
    pub(crate) fn build_automatic_learning(
        &self,
        candidate: &LearningCandidateView,
        learning_id: String,
        published_at_ms: u64,
        evaluation: &LearningAutomationEvaluation,
    ) -> Result<Option<LearningView>> {
        let publish_tier = match evaluation.action {
            LearningPublicationAction::PublishProvisional => Some(LearningPublishTier::Provisional),
            LearningPublicationAction::PublishActive => Some(LearningPublishTier::Active),
            LearningPublicationAction::ManualReview | LearningPublicationAction::Reject => None,
        };
        let Some(publish_tier) = publish_tier else {
            return Ok(None);
        };
        let learning = LearningView {
            learning_id,
            scope: candidate.scope.clone(),
            kind: candidate.kind.clone(),
            sensitivity: candidate.sensitivity.clone(),
            content: candidate.content.clone(),
            confidence: candidate.confidence,
            source: candidate.source.clone(),
            evidence_refs: candidate.evidence_refs.clone(),
            source_candidate_id: None,
            created_at_ms: candidate.created_at_ms,
            published_at_ms,
            expires_at_ms: evaluation.expires_at_ms,
            status: status_for_tier(publish_tier.clone()),
            publish_tier,
            policy_decision: Some(LearningPolicyDecision::Automatic),
            policy_actor: Some(policy_actor_for_mode(LearningMutationMode::Automatic).to_string()),
            verification_status: LearningVerificationStatus::Unverified,
            supersedes: None,
            superseded_by: None,
            revoked_at_ms: None,
            revoked_reason: None,
        };
        learning.validate()?;
        Ok(Some(learning))
    }

    /// Normalizes one candidate publication before it enters the durable learning store.
    pub(crate) fn prepare_published_learning(
        &self,
        candidate: &LearningCandidateView,
        mut learning: LearningView,
        mode: LearningMutationMode,
    ) -> Result<LearningView> {
        if learning.evidence_refs.is_empty() {
            learning.evidence_refs = candidate.evidence_refs.clone();
        }
        learning.policy_decision = Some(policy_decision_for_mode(mode));
        learning.policy_actor = Some(policy_actor_for_mode(mode).to_string());
        learning.status = status_for_tier(learning.publish_tier.clone());
        learning.validate()?;
        Ok(learning)
    }

    /// Normalizes one replacement learning before supersession.
    pub(crate) fn prepare_superseding_learning(
        &self,
        current: &LearningView,
        mut replacement: LearningView,
        mode: LearningMutationMode,
    ) -> Result<LearningView> {
        if replacement.evidence_refs.is_empty() {
            replacement.evidence_refs = current.evidence_refs.clone();
        }
        replacement.policy_decision = Some(policy_decision_for_mode(mode));
        replacement.policy_actor = Some(policy_actor_for_mode(mode).to_string());
        replacement.status = status_for_tier(replacement.publish_tier.clone());
        replacement.validate()?;
        Ok(replacement)
    }

    /// Normalizes one promoted-skill draft before it is persisted or exposed in the catalog.
    pub(crate) fn prepare_promoted_skill(
        &self,
        mut draft: LearningSkillDraft,
        learning: &LearningView,
        _mode: LearningMutationMode,
    ) -> Result<LearningSkillDraft> {
        if learning_content_has_secret_material(&draft.skill_name) {
            bail!("promoted skill name appears to contain secret material");
        }
        if learning_content_has_secret_material(&draft.instructions) {
            bail!("promoted skill instructions appear to contain secret material");
        }
        if draft
            .description
            .as_deref()
            .is_some_and(learning_content_has_secret_material)
        {
            bail!("promoted skill description appears to contain secret material");
        }
        if draft
            .when_to_use
            .as_deref()
            .is_some_and(learning_content_has_secret_material)
        {
            bail!("promoted skill when_to_use appears to contain secret material");
        }
        if draft
            .version
            .as_deref()
            .is_some_and(learning_content_has_secret_material)
        {
            bail!("promoted skill version appears to contain secret material");
        }
        for value in &draft.runtime.allowed_tools {
            reject_secret_like_promoted_skill_runtime_field("allowed_tools", value)?;
        }
        for value in &draft.runtime.blocked_tools {
            reject_secret_like_promoted_skill_runtime_field("blocked_tools", value)?;
        }
        if let Some(value) = draft.runtime.agent_profile.as_deref() {
            reject_secret_like_promoted_skill_runtime_field("agent_profile", value)?;
        }
        if let Some(value) = draft.runtime.provider.as_deref() {
            reject_secret_like_promoted_skill_runtime_field("provider", value)?;
        }
        if let Some(value) = draft.runtime.model.as_deref() {
            reject_secret_like_promoted_skill_runtime_field("model", value)?;
        }
        if let Some(value) = draft.runtime.fallback_model.as_deref() {
            reject_secret_like_promoted_skill_runtime_field("fallback_model", value)?;
        }
        if draft.evidence_refs.is_empty() {
            draft.evidence_refs = learning.evidence_refs.clone();
        }
        Ok(draft)
    }
}

fn reject_secret_like_promoted_skill_runtime_field(field: &str, value: &str) -> Result<()> {
    if learning_content_has_secret_material(value) {
        bail!("promoted skill runtime {field} appears to contain secret material");
    }
    Ok(())
}

fn evaluate_candidate_against_policy(
    candidate: &LearningCandidateView,
    policy: &LearningPublicationPolicy,
    mode: LearningAutomationMode,
    reviewed_at_ms: u64,
) -> LearningAutomationEvaluation {
    if matches!(
        candidate.kind,
        kheish_types::LearningKind::Fact
            | kheish_types::LearningKind::Preference
            | kheish_types::LearningKind::Decision
    ) && learning_content_has_secret_material(&candidate.content)
    {
        return LearningAutomationEvaluation {
            mode,
            action: LearningPublicationAction::Reject,
            matched_rule_name: None,
            judge_review: None,
            reason: "candidate content matched the anti-secret learning classifier".to_string(),
            expires_at_ms: candidate.expires_at_ms,
        };
    }

    let quarantined_rule_names = policy
        .quarantined_rule_names
        .iter()
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect::<BTreeSet<_>>();
    let matched = policy.rules.iter().find_map(|rule| {
        let quarantined = rule
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .is_some_and(|name| quarantined_rule_names.contains(&name.to_ascii_lowercase()));
        (!quarantined && rule_matches_candidate(rule, candidate)).then(|| {
            (
                rule.action.clone(),
                rule.name.clone(),
                rule_reason(rule),
                resolved_expiration(
                    candidate.expires_at_ms,
                    rule.expires_after_ms,
                    reviewed_at_ms,
                ),
            )
        })
    });
    let (action, matched_rule_name, reason, expires_at_ms) = matched.unwrap_or_else(|| {
        (
            policy.default_action.clone(),
            None,
            format!(
                "no publication rule matched; falling back to {:?}",
                policy.default_action
            ),
            candidate.expires_at_ms,
        )
    });
    let (action, matched_rule_name, reason) = if candidate.kind
        == kheish_types::LearningKind::Procedure
        && matches!(action, LearningPublicationAction::PublishActive)
    {
        (
            LearningPublicationAction::ManualReview,
            matched_rule_name,
            "procedure learnings cannot be auto-published with the active tier".to_string(),
        )
    } else {
        (action, matched_rule_name, reason)
    };
    let (action, matched_rule_name, reason) = if matches!(
        (&candidate.origin, &action),
        (
            LearningCandidateOrigin::Api,
            LearningPublicationAction::PublishActive
        )
    ) && !policy.allow_api_origin_active_publication
    {
        (
            LearningPublicationAction::PublishProvisional,
            matched_rule_name,
            "api-origin candidates cannot be auto-published with the active tier; retaining provisional tier"
                .to_string(),
        )
    } else {
        (action, matched_rule_name, reason)
    };
    LearningAutomationEvaluation {
        mode,
        action,
        matched_rule_name,
        judge_review: None,
        reason,
        expires_at_ms,
    }
}

fn validate_learning_policy_config(settings: &LearningAutomationPolicyConfig) -> Result<()> {
    if matches!(
        settings.publication.default_action,
        LearningPublicationAction::PublishActive
    ) {
        bail!("learning publication default_action cannot be publish_active");
    }
    let mut quarantined_rule_names = BTreeSet::new();
    for name in &settings.publication.quarantined_rule_names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            bail!("learning publication quarantine names must not be empty");
        }
        if !quarantined_rule_names.insert(trimmed.to_ascii_lowercase()) {
            bail!("learning publication quarantine names must be unique");
        }
    }
    if settings.judge.enabled && matches!(settings.judge.timeout_ms, Some(0)) {
        bail!("learning judge timeout_ms must be greater than zero");
    }
    if settings.capture.semantic_candidates.enabled {
        if matches!(settings.capture.semantic_candidates.timeout_ms, Some(0)) {
            bail!("semantic capture timeout_ms must be greater than zero");
        }
        if settings.capture.semantic_candidates.max_candidates_per_run == 0 {
            bail!("semantic capture max_candidates_per_run must be greater than zero");
        }
        if settings.capture.semantic_candidates.max_candidates_per_run > 8 {
            bail!("semantic capture max_candidates_per_run must be 8 or lower");
        }
    }
    for (index, rule) in settings.publication.rules.iter().enumerate() {
        if let Some(scope_id) = rule.scope_id.as_deref()
            && scope_id.trim().is_empty()
        {
            bail!("learning publication rule #{index} uses an empty scope_id");
        }
        if let Some(min_confidence) = rule.min_confidence
            && min_confidence > 100
        {
            bail!("learning publication rule #{index} min_confidence must be between 0 and 100");
        }
        if matches!(rule.action, LearningPublicationAction::PublishActive)
            && matches!(rule.kind, Some(kheish_types::LearningKind::Procedure))
        {
            bail!("automatic active publication is not supported for procedure learnings");
        }
        if matches!(rule.action, LearningPublicationAction::PublishActive) && rule.kind.is_none() {
            bail!(
                "learning publication rule #{index} cannot use publish_active without an explicit kind filter"
            );
        }
    }
    Ok(())
}

fn rule_matches_candidate(
    rule: &LearningPublicationRule,
    candidate: &LearningCandidateView,
) -> bool {
    let trusted_policy_inputs = matches!(candidate.origin, LearningCandidateOrigin::Daemon);
    rule.scope_kind
        .as_ref()
        .is_none_or(|kind| &candidate.scope.kind == kind)
        && rule
            .scope_id
            .as_deref()
            .is_none_or(|scope_id| candidate.scope.id == scope_id)
        && rule
            .kind
            .as_ref()
            .is_none_or(|kind| &candidate.kind == kind)
        && rule
            .sensitivity
            .as_ref()
            .is_none_or(|sensitivity| &candidate.sensitivity == sensitivity)
        && rule
            .min_confidence
            .is_none_or(|min_confidence| candidate.confidence >= min_confidence)
        && (!rule.require_evidence
            || (trusted_policy_inputs && !candidate.evidence_refs.is_empty()))
        && (!rule.require_source_run
            || (trusted_policy_inputs && candidate.source.run_id.is_some()))
        && (!rule.require_source_session
            || (trusted_policy_inputs && candidate.source.session_id.is_some()))
}

fn resolved_expiration(
    candidate_expires_at_ms: Option<u64>,
    expires_after_ms: Option<u64>,
    reviewed_at_ms: u64,
) -> Option<u64> {
    match (candidate_expires_at_ms, expires_after_ms) {
        (Some(existing), Some(relative)) => {
            Some(existing.min(reviewed_at_ms.saturating_add(relative)))
        }
        (Some(existing), None) => Some(existing),
        (None, Some(relative)) => Some(reviewed_at_ms.saturating_add(relative)),
        (None, None) => None,
    }
}

fn rule_reason(rule: &LearningPublicationRule) -> String {
    match rule.name.as_deref() {
        Some(name) if !name.trim().is_empty() => format!("matched publication rule `{name}`"),
        _ => "matched unnamed publication rule".to_string(),
    }
}

fn policy_decision_for_mode(mode: LearningMutationMode) -> LearningPolicyDecision {
    match mode {
        LearningMutationMode::Manual => LearningPolicyDecision::Manual,
        LearningMutationMode::Automatic => LearningPolicyDecision::Automatic,
    }
}

fn status_for_tier(tier: LearningPublishTier) -> LearningStatus {
    match tier {
        LearningPublishTier::Provisional => LearningStatus::Provisional,
        LearningPublishTier::Active => LearningStatus::Active,
    }
}

fn policy_actor_for_mode(mode: LearningMutationMode) -> &'static str {
    match mode {
        LearningMutationMode::Manual => "operator",
        LearningMutationMode::Automatic => "daemon",
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use kheish_skills::SkillRuntimeConfig;
    use tempfile::tempdir;

    use super::{LearningMutationMode, LearningPolicyService};
    use crate::learning::{
        LearningAutomationMode, LearningAutomationPolicyConfig, LearningCandidateOrigin,
        LearningCandidateState, LearningCandidateView, LearningPublicationAction,
        LearningPublicationPolicy, LearningPublicationRule, LearningView,
    };
    use crate::procedural_skills::{LearningSkillDraft, LearningSkillStatus};
    use kheish_types::{
        LearningEvidenceRef, LearningKind, LearningPolicyDecision, LearningPublishTier,
        LearningScope, LearningScopeKind, LearningSensitivity, LearningSourceRef, LearningStatus,
        LearningVerificationStatus, SkillExecutionContext,
    };

    fn sample_candidate() -> LearningCandidateView {
        LearningCandidateView {
            candidate_id: "learning-candidate-1".to_string(),
            origin: LearningCandidateOrigin::Daemon,
            scope: LearningScope {
                kind: LearningScopeKind::Session,
                id: "demo".to_string(),
            },
            kind: LearningKind::Fact,
            sensitivity: LearningSensitivity::Scoped,
            content: "Remember the demo token.".to_string(),
            confidence: 96,
            source: LearningSourceRef {
                session_id: Some("demo".to_string()),
                run_id: Some("run-1".to_string()),
                ..LearningSourceRef::default()
            },
            evidence_refs: vec![LearningEvidenceRef {
                run_id: Some("run-1".to_string()),
                artifact_id: Some("provider-request".to_string()),
                note: None,
            }],
            created_at_ms: 5,
            expires_at_ms: None,
            state: LearningCandidateState::Pending,
            automation_review: None,
            published_learning_id: None,
        }
    }

    #[tokio::test]
    async fn learning_policy_service_evaluates_matching_rule() -> Result<()> {
        let temp = tempdir()?;
        let service = LearningPolicyService::new(temp.path())?;
        service
            .activate_settings(LearningAutomationPolicyConfig {
                mode: LearningAutomationMode::Enabled,
                publication: LearningPublicationPolicy {
                    default_action: LearningPublicationAction::ManualReview,
                    allow_api_origin_active_publication: false,
                    quarantined_rule_names: Vec::new(),
                    rules: vec![LearningPublicationRule {
                        name: Some("session-fact".to_string()),
                        scope_kind: Some(LearningScopeKind::Session),
                        scope_id: None,
                        kind: Some(LearningKind::Fact),
                        sensitivity: Some(LearningSensitivity::Scoped),
                        min_confidence: Some(95),
                        require_evidence: true,
                        require_source_run: true,
                        require_source_session: true,
                        action: LearningPublicationAction::PublishProvisional,
                        expires_after_ms: Some(100),
                    }],
                },
                ..LearningAutomationPolicyConfig::default()
            })
            .await?;

        let evaluation = service
            .evaluate_candidate(&sample_candidate(), 100)
            .await
            .expect("evaluation");

        assert_eq!(
            evaluation.action,
            LearningPublicationAction::PublishProvisional
        );
        assert_eq!(
            evaluation.matched_rule_name.as_deref(),
            Some("session-fact")
        );
        assert_eq!(evaluation.expires_at_ms, Some(200));
        Ok(())
    }

    #[tokio::test]
    async fn learning_policy_service_builds_automatic_learning_with_daemon_metadata() -> Result<()>
    {
        let temp = tempdir()?;
        let service = LearningPolicyService::new(temp.path())?;
        let candidate = sample_candidate();
        let learning = service
            .build_automatic_learning(
                &candidate,
                "learning-1".to_string(),
                100,
                &super::LearningAutomationEvaluation {
                    mode: LearningAutomationMode::Enabled,
                    action: LearningPublicationAction::PublishActive,
                    matched_rule_name: Some("session-fact".to_string()),
                    judge_review: None,
                    reason: "matched publication rule".to_string(),
                    expires_at_ms: None,
                },
            )?
            .expect("learning");

        assert_eq!(learning.publish_tier, LearningPublishTier::Active);
        assert_eq!(learning.status, LearningStatus::Active);
        assert_eq!(
            learning.policy_decision,
            Some(LearningPolicyDecision::Automatic)
        );
        assert_eq!(learning.policy_actor.as_deref(), Some("daemon"));
        assert_eq!(
            learning.verification_status,
            LearningVerificationStatus::Unverified
        );
        Ok(())
    }

    #[tokio::test]
    async fn api_candidates_cannot_satisfy_trusted_source_or_evidence_rules() -> Result<()> {
        let temp = tempdir()?;
        let service = LearningPolicyService::new(temp.path())?;
        service
            .activate_settings(LearningAutomationPolicyConfig {
                mode: LearningAutomationMode::Enabled,
                publication: LearningPublicationPolicy {
                    default_action: LearningPublicationAction::ManualReview,
                    allow_api_origin_active_publication: false,
                    quarantined_rule_names: Vec::new(),
                    rules: vec![LearningPublicationRule {
                        name: Some("trusted-only".to_string()),
                        scope_kind: Some(LearningScopeKind::Session),
                        scope_id: None,
                        kind: Some(LearningKind::Fact),
                        sensitivity: Some(LearningSensitivity::Scoped),
                        min_confidence: Some(95),
                        require_evidence: true,
                        require_source_run: true,
                        require_source_session: true,
                        action: LearningPublicationAction::PublishProvisional,
                        expires_after_ms: None,
                    }],
                },
                ..LearningAutomationPolicyConfig::default()
            })
            .await?;

        let mut candidate = sample_candidate();
        candidate.origin = LearningCandidateOrigin::Api;
        let evaluation = service
            .evaluate_candidate(&candidate, 100)
            .await
            .expect("evaluation");

        assert_eq!(evaluation.action, LearningPublicationAction::ManualReview);
        assert_eq!(evaluation.matched_rule_name, None);
        Ok(())
    }

    #[tokio::test]
    async fn api_origin_active_rules_downgrade_to_provisional_by_default() -> Result<()> {
        let temp = tempdir()?;
        let service = LearningPolicyService::new(temp.path())?;
        service
            .activate_settings(LearningAutomationPolicyConfig {
                mode: LearningAutomationMode::Enabled,
                publication: LearningPublicationPolicy {
                    default_action: LearningPublicationAction::ManualReview,
                    allow_api_origin_active_publication: false,
                    quarantined_rule_names: Vec::new(),
                    rules: vec![LearningPublicationRule {
                        name: Some("session-fact".to_string()),
                        scope_kind: Some(LearningScopeKind::Session),
                        scope_id: None,
                        kind: Some(LearningKind::Fact),
                        sensitivity: Some(LearningSensitivity::Scoped),
                        min_confidence: Some(95),
                        require_evidence: false,
                        require_source_run: false,
                        require_source_session: false,
                        action: LearningPublicationAction::PublishActive,
                        expires_after_ms: None,
                    }],
                },
                ..LearningAutomationPolicyConfig::default()
            })
            .await?;

        let mut candidate = sample_candidate();
        candidate.origin = LearningCandidateOrigin::Api;
        let evaluation = service
            .evaluate_candidate(&candidate, 100)
            .await
            .expect("evaluation");

        assert_eq!(
            evaluation.action,
            LearningPublicationAction::PublishProvisional
        );
        assert_eq!(
            evaluation.matched_rule_name.as_deref(),
            Some("session-fact")
        );
        Ok(())
    }

    #[tokio::test]
    async fn api_origin_active_rules_can_be_opted_in() -> Result<()> {
        let temp = tempdir()?;
        let service = LearningPolicyService::new(temp.path())?;
        service
            .activate_settings(LearningAutomationPolicyConfig {
                mode: LearningAutomationMode::Enabled,
                publication: LearningPublicationPolicy {
                    default_action: LearningPublicationAction::ManualReview,
                    allow_api_origin_active_publication: true,
                    quarantined_rule_names: Vec::new(),
                    rules: vec![LearningPublicationRule {
                        name: Some("session-fact".to_string()),
                        scope_kind: Some(LearningScopeKind::Session),
                        scope_id: None,
                        kind: Some(LearningKind::Fact),
                        sensitivity: Some(LearningSensitivity::Scoped),
                        min_confidence: Some(95),
                        require_evidence: false,
                        require_source_run: false,
                        require_source_session: false,
                        action: LearningPublicationAction::PublishActive,
                        expires_after_ms: None,
                    }],
                },
                ..LearningAutomationPolicyConfig::default()
            })
            .await?;

        let mut candidate = sample_candidate();
        candidate.origin = LearningCandidateOrigin::Api;
        let evaluation = service
            .evaluate_candidate(&candidate, 100)
            .await
            .expect("evaluation");

        assert_eq!(evaluation.action, LearningPublicationAction::PublishActive);
        assert_eq!(
            evaluation.matched_rule_name.as_deref(),
            Some("session-fact")
        );
        Ok(())
    }

    #[tokio::test]
    async fn quarantined_rule_names_skip_matching_rules() -> Result<()> {
        let temp = tempdir()?;
        let service = LearningPolicyService::new(temp.path())?;
        service
            .activate_settings(LearningAutomationPolicyConfig {
                mode: LearningAutomationMode::Enabled,
                publication: LearningPublicationPolicy {
                    default_action: LearningPublicationAction::ManualReview,
                    allow_api_origin_active_publication: false,
                    quarantined_rule_names: vec!["session-fact".to_string()],
                    rules: vec![LearningPublicationRule {
                        name: Some("session-fact".to_string()),
                        scope_kind: Some(LearningScopeKind::Session),
                        scope_id: None,
                        kind: Some(LearningKind::Fact),
                        sensitivity: Some(LearningSensitivity::Scoped),
                        min_confidence: Some(95),
                        require_evidence: false,
                        require_source_run: false,
                        require_source_session: false,
                        action: LearningPublicationAction::PublishProvisional,
                        expires_after_ms: None,
                    }],
                },
                ..LearningAutomationPolicyConfig::default()
            })
            .await?;

        let evaluation = service
            .evaluate_candidate(&sample_candidate(), 100)
            .await
            .expect("evaluation");

        assert_eq!(evaluation.action, LearningPublicationAction::ManualReview);
        assert_eq!(evaluation.matched_rule_name, None);
        Ok(())
    }

    #[tokio::test]
    async fn learning_policy_rejects_secret_like_candidates_before_publication_rules() -> Result<()>
    {
        let temp = tempdir()?;
        let service = LearningPolicyService::new(temp.path())?;
        service
            .activate_settings(LearningAutomationPolicyConfig {
                mode: LearningAutomationMode::Enabled,
                publication: LearningPublicationPolicy {
                    default_action: LearningPublicationAction::ManualReview,
                    allow_api_origin_active_publication: false,
                    quarantined_rule_names: Vec::new(),
                    rules: vec![LearningPublicationRule {
                        name: Some("session-fact".to_string()),
                        scope_kind: Some(LearningScopeKind::Session),
                        scope_id: None,
                        kind: Some(LearningKind::Fact),
                        sensitivity: Some(LearningSensitivity::Scoped),
                        min_confidence: Some(95),
                        require_evidence: true,
                        require_source_run: true,
                        require_source_session: true,
                        action: LearningPublicationAction::PublishActive,
                        expires_after_ms: None,
                    }],
                },
                ..LearningAutomationPolicyConfig::default()
            })
            .await?;

        let mut secret = sample_candidate();
        secret.content = "The API key is sk-proj-secret.".to_string();
        let rejected = service
            .evaluate_candidate(&secret, 100)
            .await
            .expect("evaluation");
        assert_eq!(rejected.action, LearningPublicationAction::Reject);
        assert_eq!(rejected.matched_rule_name, None);
        assert!(rejected.reason.contains("anti-secret"));

        let mut non_secret = sample_candidate();
        non_secret.content = "Token budget is 1000.".to_string();
        let accepted = service
            .evaluate_candidate(&non_secret, 100)
            .await
            .expect("evaluation");
        assert_eq!(accepted.action, LearningPublicationAction::PublishActive);
        assert_eq!(accepted.matched_rule_name.as_deref(), Some("session-fact"));
        Ok(())
    }

    #[test]
    fn prepare_promoted_skill_rejects_secret_like_visible_metadata() -> Result<()> {
        let temp = tempdir()?;
        let service = LearningPolicyService::new(temp.path())?;
        let learning = LearningView {
            learning_id: "learning-1".to_string(),
            scope: LearningScope {
                kind: LearningScopeKind::Workspace,
                id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
            },
            kind: LearningKind::Procedure,
            sensitivity: LearningSensitivity::Scoped,
            content: "Run the procedure.".to_string(),
            confidence: 95,
            source: LearningSourceRef::default(),
            evidence_refs: Vec::new(),
            source_candidate_id: None,
            created_at_ms: 1,
            published_at_ms: 2,
            expires_at_ms: None,
            status: LearningStatus::Active,
            publish_tier: LearningPublishTier::Active,
            policy_decision: Some(LearningPolicyDecision::Manual),
            policy_actor: Some("operator".to_string()),
            verification_status: LearningVerificationStatus::Verified,
            supersedes: None,
            superseded_by: None,
            revoked_at_ms: None,
            revoked_reason: None,
        };
        let base_draft = LearningSkillDraft {
            skill_name: "learning:safe-procedure".to_string(),
            description: Some("Run the safe procedure.".to_string()),
            when_to_use: Some("Use for safe procedure checks.".to_string()),
            version: Some("1".to_string()),
            instructions: "Reply with exactly `OK`.".to_string(),
            runtime: SkillRuntimeConfig {
                context: SkillExecutionContext::Fork,
                agent_profile: Some("verification".to_string()),
                ..SkillRuntimeConfig::default()
            },
            status: LearningSkillStatus::Draft,
            evidence_refs: Vec::new(),
            verification_status: LearningVerificationStatus::Unverified,
            successful_run_count: 0,
            distinct_session_count: 0,
            verifier_run_ids: Vec::new(),
            real_daemon_verified: false,
            last_verified_workspace_digest: None,
            canary_success_count: 0,
            canary_failure_count: 0,
        };

        for (field, draft) in [
            (
                "name",
                LearningSkillDraft {
                    skill_name: "api-key:sk-proj-secret".to_string(),
                    ..base_draft.clone()
                },
            ),
            (
                "version",
                LearningSkillDraft {
                    version: Some("clientSecret: hunter2hunter2".to_string()),
                    ..base_draft.clone()
                },
            ),
            (
                "description",
                LearningSkillDraft {
                    description: Some("secret_token=abc123456789".to_string()),
                    ..base_draft.clone()
                },
            ),
            (
                "when_to_use",
                LearningSkillDraft {
                    when_to_use: Some(format!("personal access token: {}{}", "gh", "p_1234567890")),
                    ..base_draft.clone()
                },
            ),
            (
                "instructions",
                LearningSkillDraft {
                    instructions: "x-api-key: abc123456789".to_string(),
                    ..base_draft.clone()
                },
            ),
            (
                "allowed_tools",
                LearningSkillDraft {
                    runtime: SkillRuntimeConfig {
                        allowed_tools: vec!["api-key:abc123456789".to_string()],
                        ..base_draft.runtime.clone()
                    },
                    ..base_draft.clone()
                },
            ),
            (
                "blocked_tools",
                LearningSkillDraft {
                    runtime: SkillRuntimeConfig {
                        blocked_tools: vec!["clientSecret: hunter2hunter2".to_string()],
                        ..base_draft.runtime.clone()
                    },
                    ..base_draft.clone()
                },
            ),
            (
                "agent_profile",
                LearningSkillDraft {
                    runtime: SkillRuntimeConfig {
                        agent_profile: Some("secret_token=abc123456789".to_string()),
                        ..base_draft.runtime.clone()
                    },
                    ..base_draft.clone()
                },
            ),
            (
                "provider",
                LearningSkillDraft {
                    runtime: SkillRuntimeConfig {
                        provider: Some(format!(
                            "personal access token: {}{}",
                            "gh", "p_1234567890"
                        )),
                        ..base_draft.runtime.clone()
                    },
                    ..base_draft.clone()
                },
            ),
            (
                "model",
                LearningSkillDraft {
                    runtime: SkillRuntimeConfig {
                        model: Some("x-api-key: abc123456789".to_string()),
                        ..base_draft.runtime.clone()
                    },
                    ..base_draft.clone()
                },
            ),
            (
                "fallback_model",
                LearningSkillDraft {
                    runtime: SkillRuntimeConfig {
                        fallback_model: Some("api-key: abc123456789".to_string()),
                        ..base_draft.runtime.clone()
                    },
                    ..base_draft.clone()
                },
            ),
        ] {
            let error = service
                .prepare_promoted_skill(draft, &learning, LearningMutationMode::Manual)
                .expect_err("secret-like promoted skill metadata should be rejected");
            assert!(
                error.to_string().contains("secret material"),
                "unexpected {field} error: {error:?}"
            );
        }

        let prepared =
            service.prepare_promoted_skill(base_draft, &learning, LearningMutationMode::Manual)?;
        assert_eq!(prepared.skill_name, "learning:safe-procedure");
        Ok(())
    }

    #[test]
    fn prepare_published_learning_sets_policy_actor_for_manual_and_automatic_modes() -> Result<()> {
        let temp = tempdir()?;
        let service = LearningPolicyService::new(temp.path())?;
        let candidate = sample_candidate();
        let manual = service.prepare_published_learning(
            &candidate,
            service
                .build_automatic_learning(
                    &candidate,
                    "learning-1".to_string(),
                    100,
                    &super::LearningAutomationEvaluation {
                        mode: LearningAutomationMode::Enabled,
                        action: LearningPublicationAction::PublishActive,
                        matched_rule_name: None,
                        judge_review: None,
                        reason: "matched rule".to_string(),
                        expires_at_ms: None,
                    },
                )?
                .expect("learning"),
            LearningMutationMode::Manual,
        )?;
        let automatic = service.prepare_published_learning(
            &candidate,
            manual.clone(),
            LearningMutationMode::Automatic,
        )?;

        assert_eq!(manual.policy_actor.as_deref(), Some("operator"));
        assert_eq!(automatic.policy_actor.as_deref(), Some("daemon"));
        Ok(())
    }
}
