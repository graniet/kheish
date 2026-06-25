//! Durable daemon-owned learning candidates and published learning records.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Result, bail};
use kheish_runtime::redact_text;
use kheish_session::write_json_pretty_atomically;
use kheish_types::{
    DEFAULT_WORKSPACE_LEARNING_SCOPE_ID, HookModelConfig, LearningEvidenceRef, LearningKind,
    LearningPolicyDecision, LearningPublishTier, LearningScope, LearningSensitivity,
    LearningSourceRef, LearningStatus, LearningVerificationStatus,
};
use serde::{Deserialize, Serialize};

use crate::state_files::read_json_or_quarantine;

const MAX_LEARNING_CONTENT_CHARS: usize = 1_600;

/// The durable review state of one learning candidate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningCandidateState {
    /// The candidate still awaits operator review.
    Pending,
    /// The candidate was reviewed automatically and still needs operator attention.
    Escalated,
    /// The candidate already produced one published learning.
    Published,
    /// The candidate was explicitly rejected.
    Rejected,
}

/// The ingress origin retained for one learning candidate.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningCandidateOrigin {
    /// The candidate was created explicitly through the public API or CLI.
    #[default]
    Api,
    /// The candidate was materialized by one daemon-owned workflow.
    Daemon,
}

/// The automation mode currently applied to candidate publication.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningAutomationMode {
    /// Automatic publication is disabled and candidates stay operator-driven.
    #[default]
    ManualOnly,
    /// Candidates are evaluated automatically but stay pending until an operator acts.
    Shadow,
    /// Matching candidates may be published, escalated, or rejected automatically.
    Enabled,
}

/// The daemon-owned action selected for one candidate after policy evaluation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningPublicationAction {
    /// The candidate should remain available for explicit operator review.
    ManualReview,
    /// The candidate should be rejected automatically.
    Reject,
    /// The candidate should be published with the provisional tier.
    PublishProvisional,
    /// The candidate should be published with the active tier.
    PublishActive,
}

impl Default for LearningPublicationAction {
    fn default() -> Self {
        Self::ManualReview
    }
}

/// Model-backed semantic extraction settings used by daemon-owned learning capture.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LearningSemanticCaptureConfig {
    /// Whether completed runs should be reviewed for durable semantic candidates.
    #[serde(default)]
    pub enabled: bool,
    /// Optional provider and generation overrides for the extraction route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<HookModelConfig>,
    /// Optional timeout for one extraction request in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// The maximum number of semantic candidates retained from one run.
    #[serde(default = "default_semantic_capture_max_candidates_per_run")]
    pub max_candidates_per_run: usize,
}

impl Default for LearningSemanticCaptureConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            timeout_ms: None,
            max_candidates_per_run: default_semantic_capture_max_candidates_per_run(),
        }
    }
}

/// The minimal capture policy currently supported by daemon-owned learning automation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LearningCapturePolicy {
    /// Whether terminal runs should materialize session-scoped run-summary candidates.
    #[serde(default = "default_capture_run_summary_candidates")]
    pub run_summary_candidates: bool,
    /// Optional semantic extraction settings applied to completed runs.
    #[serde(default)]
    pub semantic_candidates: LearningSemanticCaptureConfig,
}

impl Default for LearningCapturePolicy {
    fn default() -> Self {
        Self {
            run_summary_candidates: default_capture_run_summary_candidates(),
            semantic_candidates: LearningSemanticCaptureConfig::default(),
        }
    }
}

/// One ordered publication rule used by daemon-owned learning automation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningPublicationRule {
    /// Optional stable display name used for logs and review traces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional scope-kind filter for the rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_kind: Option<kheish_types::LearningScopeKind>,
    /// Optional exact scope identifier filter for the rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,
    /// Optional kind filter for the rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<LearningKind>,
    /// Optional sensitivity filter for the rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitivity: Option<kheish_types::LearningSensitivity>,
    /// Optional minimum confidence required for the rule to match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_confidence: Option<u8>,
    /// Whether at least one evidence reference must be present.
    #[serde(default)]
    pub require_evidence: bool,
    /// Whether the candidate source must include a run identifier.
    #[serde(default)]
    pub require_source_run: bool,
    /// Whether the candidate source must include a session identifier.
    #[serde(default)]
    pub require_source_session: bool,
    /// The action produced when the rule matches.
    pub action: LearningPublicationAction,
    /// Optional relative expiration applied to auto-published records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_after_ms: Option<u64>,
}

/// Ordered publication policy used by daemon-owned learning automation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningPublicationPolicy {
    /// The fallback action used when no publication rule matches.
    #[serde(default = "default_publication_action")]
    pub default_action: LearningPublicationAction,
    /// Whether API-created candidates may be auto-published directly with the active tier.
    #[serde(default)]
    pub allow_api_origin_active_publication: bool,
    /// Named publication rules that should be ignored while automation is quarantined.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quarantined_rule_names: Vec<String>,
    /// Ordered publication rules evaluated from first to last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<LearningPublicationRule>,
}

impl Default for LearningPublicationPolicy {
    fn default() -> Self {
        Self {
            default_action: default_publication_action(),
            allow_api_origin_active_publication: false,
            quarantined_rule_names: Vec::new(),
            rules: Vec::new(),
        }
    }
}

/// Model-backed judge settings used by daemon-owned learning automation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LearningJudgeConfig {
    /// Whether the daemon should ask a structured model judge before auto-applying actions.
    #[serde(default)]
    pub enabled: bool,
    /// Optional provider and generation overrides for the judge route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<HookModelConfig>,
    /// Optional timeout for one judge request in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// The daemon-owned automation policy that governs candidate capture and publication.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LearningAutomationPolicyConfig {
    /// The current automation mode.
    #[serde(default)]
    pub mode: LearningAutomationMode,
    /// Capture policy applied while extracting new candidates.
    #[serde(default)]
    pub capture: LearningCapturePolicy,
    /// Publication policy applied by the autonomous worker.
    #[serde(default)]
    pub publication: LearningPublicationPolicy,
    /// Optional model-backed judge settings applied before automatic publication.
    #[serde(default)]
    pub judge: LearningJudgeConfig,
}

impl Default for LearningAutomationPolicyConfig {
    fn default() -> Self {
        Self {
            mode: LearningAutomationMode::Shadow,
            capture: LearningCapturePolicy::default(),
            publication: LearningPublicationPolicy::default(),
            judge: LearningJudgeConfig::default(),
        }
    }
}

/// One model-backed judge review retained with a candidate automation decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningJudgeReview {
    /// The action recommended by the model-backed judge after clamping.
    pub action: LearningPublicationAction,
    /// The timestamp in milliseconds since the Unix epoch when the judge completed.
    pub judged_at_ms: u64,
    /// Human-readable rationale retained for audit and debugging.
    pub reason: String,
}

/// One daemon-owned review decision recorded on a candidate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningAutomationReview {
    /// The automation mode that produced the review.
    pub mode: LearningAutomationMode,
    /// The action selected by daemon policy.
    pub action: LearningPublicationAction,
    /// The review timestamp in milliseconds since the Unix epoch.
    pub reviewed_at_ms: u64,
    /// Optional rule name that matched while evaluating the candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_rule_name: Option<String>,
    /// Optional model-backed judge review retained with the final automation decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<LearningJudgeReview>,
    /// Human-readable explanation retained for audit and debugging.
    pub reason: String,
}

/// One externally visible learning candidate owned by the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningCandidateView {
    /// The stable daemon-owned candidate identifier.
    pub candidate_id: String,
    /// The ingress origin retained for daemon policy decisions.
    #[serde(default)]
    pub origin: LearningCandidateOrigin,
    /// The owning learning scope.
    pub scope: LearningScope,
    /// The candidate kind.
    pub kind: LearningKind,
    /// The candidate visibility class.
    pub sensitivity: LearningSensitivity,
    /// The normalized candidate content.
    pub content: String,
    /// The coarse confidence score in the inclusive range `[0, 100]`.
    pub confidence: u8,
    /// One compact provenance pointer retained with the candidate.
    pub source: LearningSourceRef,
    /// Immutable evidence references retained with the candidate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<LearningEvidenceRef>,
    /// The creation timestamp in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// The optional expiration timestamp in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// The current review state of the candidate.
    pub state: LearningCandidateState,
    /// The latest daemon-owned automation review when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation_review: Option<LearningAutomationReview>,
    /// The published learning identifier when the candidate already produced one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_learning_id: Option<String>,
}

/// One externally visible published learning owned by the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningView {
    /// The stable daemon-owned learning identifier.
    pub learning_id: String,
    /// The owning learning scope.
    pub scope: LearningScope,
    /// The published learning kind.
    pub kind: LearningKind,
    /// The published visibility class.
    pub sensitivity: LearningSensitivity,
    /// The normalized durable content.
    pub content: String,
    /// The coarse confidence score in the inclusive range `[0, 100]`.
    pub confidence: u8,
    /// One compact provenance pointer retained with the learning.
    pub source: LearningSourceRef,
    /// Immutable evidence references retained with the learning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<LearningEvidenceRef>,
    /// The originating candidate identifier when the learning was published from review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_candidate_id: Option<String>,
    /// The creation timestamp inherited from the original candidate or manual draft.
    pub created_at_ms: u64,
    /// The publication timestamp in milliseconds since the Unix epoch.
    pub published_at_ms: u64,
    /// The optional expiration timestamp in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// The durable publication status.
    pub status: LearningStatus,
    /// The daemon-owned publication tier that governs prompt visibility.
    #[serde(default)]
    pub publish_tier: LearningPublishTier,
    /// The policy decision that produced the current learning when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_decision: Option<LearningPolicyDecision>,
    /// The durable actor label that authored the policy decision when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_actor: Option<String>,
    /// The current verification state retained with the learning.
    #[serde(default)]
    pub verification_status: LearningVerificationStatus,
    /// The older learning identifier superseded by this record when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// The newer learning identifier that replaced this record when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// The revocation timestamp in milliseconds since the Unix epoch when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at_ms: Option<u64>,
    /// The operator-provided revocation reason when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_reason: Option<String>,
}

/// Monotonic operator counters for prompt-visible session memory.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMemoryMetricsSnapshot {
    /// Learnings omitted by final runtime prompt-budget packing.
    #[serde(default)]
    pub prompt_limit_omitted_total: u64,
}

/// Cheap operator status for prompt-visible session memory.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMemoryStatusView {
    /// Runtime prompt-packing counters for learned context.
    #[serde(default)]
    pub metrics: SessionMemoryMetricsSnapshot,
}

impl LearningCandidateView {
    /// Validates the candidate payload before persistence.
    pub(crate) fn validate(&self) -> Result<()> {
        validate_learning_scope(&self.scope)?;
        validate_learning_content(&self.content)?;
        if self.kind == LearningKind::RunSummary
            && !matches!(self.origin, LearningCandidateOrigin::Daemon)
        {
            bail!("run_summary candidates are daemon-owned");
        }
        if learning_kind_uses_secret_classifier(&self.kind)
            && learning_content_has_secret_material(&self.content)
        {
            bail!("semantic learning content appears to contain secret material");
        }
        if let Some(review) = &self.automation_review
            && review.reason.trim().is_empty()
        {
            bail!("learning automation review reason is required");
        }
        Ok(validate_confidence(self.confidence)?)
    }
}

fn default_capture_run_summary_candidates() -> bool {
    true
}

fn default_semantic_capture_max_candidates_per_run() -> usize {
    2
}

fn default_publication_action() -> LearningPublicationAction {
    LearningPublicationAction::ManualReview
}

impl LearningView {
    /// Validates the published payload before persistence.
    pub(crate) fn validate(&self) -> Result<()> {
        validate_learning_scope(&self.scope)?;
        validate_learning_content(&self.content)?;
        if learning_kind_uses_secret_classifier(&self.kind)
            && learning_content_has_secret_material(&self.content)
        {
            bail!("semantic learning content appears to contain secret material");
        }
        validate_learning_status_consistency(self)?;
        Ok(validate_confidence(self.confidence)?)
    }
}

/// Filesystem-backed learning storage rooted under one daemon state directory.
#[derive(Clone, Debug)]
pub(crate) struct FileLearningStore {
    root: PathBuf,
}

impl FileLearningStore {
    /// Creates a new learning store rooted under one daemon state directory.
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn candidates_root(&self) -> PathBuf {
        self.root.join("learning-candidates")
    }

    fn records_root(&self) -> PathBuf {
        self.root.join("learnings")
    }

    fn candidate_path(&self, candidate_id: &str) -> PathBuf {
        self.candidates_root().join(format!("{candidate_id}.json"))
    }

    fn learning_path(&self, learning_id: &str) -> PathBuf {
        self.records_root().join(format!("{learning_id}.json"))
    }

    /// Loads every persisted learning candidate, quarantining corrupted files.
    pub(crate) fn load_candidates(&self) -> Result<BTreeMap<String, LearningCandidateView>> {
        self.load_views(self.candidates_root(), "learning candidate")
    }

    /// Loads every persisted learning record, quarantining corrupted files.
    pub(crate) fn load_records(&self) -> Result<BTreeMap<String, LearningView>> {
        self.load_views(self.records_root(), "learning record")
    }

    /// Persists one learning candidate atomically.
    pub(crate) fn save_candidate(&self, candidate: &LearningCandidateView) -> Result<()> {
        candidate.validate()?;
        write_json_pretty_atomically(&self.candidate_path(&candidate.candidate_id), candidate)
    }

    /// Persists one published learning atomically.
    pub(crate) fn save_learning(&self, learning: &LearningView) -> Result<()> {
        learning.validate()?;
        write_json_pretty_atomically(&self.learning_path(&learning.learning_id), learning)
    }

    /// Returns the next numeric candidate identifier seed.
    pub(crate) fn next_candidate_seed(&self) -> u64 {
        self.file_ids(self.candidates_root(), "learning-candidate-")
    }

    /// Returns the next numeric published learning identifier seed.
    pub(crate) fn next_learning_seed(&self) -> u64 {
        self.file_ids(self.records_root(), "learning-")
    }

    fn load_views<T>(&self, root: PathBuf, state_kind: &'static str) -> Result<BTreeMap<String, T>>
    where
        T: Clone + for<'de> Deserialize<'de> + LearningIdAccessor,
    {
        if !root.exists() {
            return Ok(BTreeMap::new());
        }
        let mut records = BTreeMap::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(record) = read_json_or_quarantine::<T>(&path, state_kind)? else {
                continue;
            };
            records.insert(record.learning_id().to_string(), record);
        }
        Ok(records)
    }

    fn file_ids(&self, root: PathBuf, prefix: &str) -> u64 {
        let Ok(entries) = fs::read_dir(&root) else {
            return 1;
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                let base_name = file_name.split(".corrupt-").next().unwrap_or(&file_name);
                base_name
                    .strip_suffix(".json")
                    .and_then(|value| value.strip_prefix(prefix))
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }
}

trait LearningIdAccessor {
    fn learning_id(&self) -> &str;
}

impl LearningIdAccessor for LearningCandidateView {
    fn learning_id(&self) -> &str {
        &self.candidate_id
    }
}

impl LearningIdAccessor for LearningView {
    fn learning_id(&self) -> &str {
        &self.learning_id
    }
}

pub(crate) fn validate_learning_scope(scope: &LearningScope) -> Result<()> {
    let trimmed = scope.id.trim();
    if trimmed.is_empty() {
        bail!("learning scope id is required");
    }
    if trimmed != scope.id {
        bail!("learning scope id must not contain leading or trailing whitespace");
    }
    if matches!(scope.kind, kheish_types::LearningScopeKind::Workspace)
        && trimmed != DEFAULT_WORKSPACE_LEARNING_SCOPE_ID
    {
        bail!(
            "workspace learning scope id must be `{}`",
            DEFAULT_WORKSPACE_LEARNING_SCOPE_ID
        );
    }
    Ok(())
}

fn validate_learning_content(content: &str) -> Result<()> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        bail!("learning content is required");
    }
    if trimmed.chars().count() > MAX_LEARNING_CONTENT_CHARS {
        bail!(
            "learning content exceeds the {} character limit",
            MAX_LEARNING_CONTENT_CHARS
        );
    }
    Ok(())
}

fn learning_kind_uses_secret_classifier(kind: &LearningKind) -> bool {
    matches!(
        kind,
        LearningKind::Fact | LearningKind::Preference | LearningKind::Decision
    )
}

/// Returns true when a learning candidate looks like it would persist secret material.
pub(crate) fn learning_content_has_secret_material(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lowered = trimmed.to_lowercase();
    if lowered.contains("<redacted") {
        return true;
    }
    if redact_text(trimmed) != trimmed {
        return true;
    }
    learning_content_has_secret_assignment(&lowered)
}

/// Stable deterministic key used for semantic dedup/conflict checks.
pub(crate) fn semantic_learning_content_key(content: &str) -> String {
    if let Some((subject, value)) = semantic_learning_parts(content) {
        format!("{subject}={value}")
    } else {
        semantic_learning_text_key(content)
    }
}

/// Stable deterministic subject key used for same-subject conflict checks.
pub(crate) fn semantic_learning_subject_key(content: &str) -> Option<String> {
    semantic_learning_parts(content).map(|(subject, _)| subject)
}

/// Token set used when checking whether daemon-owned run memory supports a candidate.
pub(crate) fn semantic_learning_evidence_terms(content: &str) -> BTreeSet<String> {
    semantic_learning_tokens(content)
        .into_iter()
        .filter(|term| !semantic_learning_evidence_stop_word(term))
        .collect()
}

fn semantic_learning_parts(content: &str) -> Option<(String, String)> {
    let normalized = content
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .trim_matches(|ch: char| matches!(ch, '.' | ';' | ','))
        .to_lowercase();
    if normalized.is_empty() {
        return None;
    }
    for delimiter in [" is ", " are ", " est ", " sont ", "=", ":"] {
        let Some(index) = normalized.find(delimiter) else {
            continue;
        };
        let subject = semantic_learning_subject_part_key(&normalized[..index]);
        let value = semantic_learning_text_key(&normalized[index + delimiter.len()..]);
        if semantic_subject_part_is_usable(&subject) && semantic_value_part_is_usable(&value) {
            return Some((subject, value));
        }
    }
    None
}

fn semantic_learning_subject_part_key(subject: &str) -> String {
    let keyed = semantic_learning_text_key(subject);
    keyed
        .strip_prefix("the ")
        .or_else(|| keyed.strip_prefix("le "))
        .or_else(|| keyed.strip_prefix("la "))
        .or_else(|| keyed.strip_prefix("les "))
        .unwrap_or(&keyed)
        .to_string()
}

fn semantic_learning_text_key(value: &str) -> String {
    semantic_learning_tokens(value).join(" ")
}

fn semantic_learning_tokens(value: &str) -> Vec<String> {
    value
        .chars()
        .flat_map(char::to_lowercase)
        .map(|ch| if ch.is_alphanumeric() { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn semantic_subject_part_is_usable(subject: &str) -> bool {
    subject.chars().filter(|ch| ch.is_alphanumeric()).count() >= 3
        && !matches!(
            subject,
            "a" | "an" | "it" | "the" | "this" | "that" | "ce" | "cela" | "il" | "elle"
        )
}

fn semantic_value_part_is_usable(value: &str) -> bool {
    value.chars().filter(|ch| ch.is_alphanumeric()).count() >= 2
}

fn semantic_learning_evidence_stop_word(term: &str) -> bool {
    matches!(
        term,
        "a" | "an"
            | "and"
            | "are"
            | "de"
            | "des"
            | "du"
            | "est"
            | "et"
            | "is"
            | "la"
            | "le"
            | "les"
            | "of"
            | "the"
            | "to"
            | "un"
            | "une"
    )
}

fn learning_content_has_secret_assignment(lowered: &str) -> bool {
    let compact_words = lowered
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let squashed = lowered
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>();

    const WORD_MARKERS: &[&str] = &[
        "access token is",
        "access token:",
        "access token =",
        "api key is",
        "api key:",
        "api key =",
        "authorization bearer",
        "authorization: bearer",
        "bearer token is",
        "bearer token:",
        "bearer token =",
        "clientsecret:",
        "clientsecret =",
        "clientsecret is",
        "client secret is",
        "client secret:",
        "client secret =",
        "password is",
        "password:",
        "password =",
        "passphrase is",
        "passphrase:",
        "passphrase =",
        "private key is",
        "private key:",
        "private key =",
        "personal access token is",
        "personal access token:",
        "personal access token =",
        "refresh token is",
        "refresh token:",
        "refresh token =",
        "secret key is",
        "secret key:",
        "secret key =",
        "secret token is",
        "secret token:",
        "secret token =",
        "token is",
        "token:",
        "token =",
    ];
    if WORD_MARKERS
        .iter()
        .any(|marker| compact_words.contains(marker))
    {
        return true;
    }

    const SQUASHED_MARKERS: &[&str] = &[
        "access_token=",
        "apikey=",
        "api_key=",
        "api-key:",
        "api-key=",
        "authorization:bearer",
        "bearer=",
        "clientsecret:",
        "clientsecret=",
        "client_secret=",
        "client-secret:",
        "client-secret=",
        "pat:",
        "pat=",
        "password=",
        "passphrase=",
        "personal_access_token=",
        "personal-access-token:",
        "personal-access-token=",
        "personalaccesstoken:",
        "personalaccesstoken=",
        "privatekey:",
        "privatekey=",
        "private_key=",
        "refresh_token=",
        "secret_key=",
        "secret_token=",
        "token=",
        "x-api-key:",
        "x-api-key=",
        "x_api_key=",
        "xapikey:",
        "xapikey=",
    ];
    if SQUASHED_MARKERS
        .iter()
        .any(|marker| squashed.contains(marker))
    {
        return true;
    }

    learning_content_has_structured_secret_assignment(lowered)
}

fn learning_content_has_structured_secret_assignment(lowered: &str) -> bool {
    lowered.lines().any(|line| {
        [':', '='].iter().any(|delimiter| {
            line.split_once(*delimiter).is_some_and(|(key, value)| {
                !value.trim().is_empty() && secret_assignment_key_looks_sensitive(key)
            })
        })
    })
}

fn secret_assignment_key_looks_sensitive(key: &str) -> bool {
    let normalized = key
        .chars()
        .map(|ch| if ch.is_alphanumeric() { ch } else { ' ' })
        .collect::<String>();
    let words = normalized
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if words.is_empty() {
        return false;
    }

    const SECRET_KEY_PHRASES: &[&[&str]] = &[
        &["access", "token"],
        &["api", "key"],
        &["x", "api", "key"],
        &["auth", "token"],
        &["bearer", "token"],
        &["client", "secret"],
        &["clientsecret"],
        &["password"],
        &["passphrase"],
        &["pat"],
        &["personal", "access", "token"],
        &["personalaccesstoken"],
        &["private", "key"],
        &["privatekey"],
        &["refresh", "token"],
        &["secret", "key"],
        &["secret", "token"],
        &["token"],
    ];
    SECRET_KEY_PHRASES
        .iter()
        .any(|phrase| words.ends_with(phrase))
}

fn validate_confidence(confidence: u8) -> Result<()> {
    if confidence > 100 {
        bail!("learning confidence must be between 0 and 100");
    }
    Ok(())
}

fn validate_learning_status_consistency(learning: &LearningView) -> Result<()> {
    if learning.policy_decision.is_some()
        && learning
            .policy_actor
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
    {
        bail!("learning policy_actor is required when policy_decision is set");
    }
    match (learning.status.clone(), learning.publish_tier.clone()) {
        (LearningStatus::Provisional, LearningPublishTier::Provisional)
        | (LearningStatus::Active, LearningPublishTier::Active)
        | (LearningStatus::Superseded | LearningStatus::Revoked, _) => {}
        (LearningStatus::Provisional, LearningPublishTier::Active) => {
            bail!("provisional learnings must use the provisional publish tier")
        }
        (LearningStatus::Active, LearningPublishTier::Provisional) => {
            bail!("active learnings must use the active publish tier")
        }
    }
    if learning.status == LearningStatus::Active
        && learning.verification_status == LearningVerificationStatus::Failed
    {
        bail!("active learnings cannot use failed verification status");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use kheish_types::{
        LearningPolicyDecision, LearningPublishTier, LearningScopeKind, LearningStatus,
        LearningVerificationStatus,
    };
    use tempfile::tempdir;

    use super::{
        FileLearningStore, LearningCandidateOrigin, LearningCandidateState, LearningCandidateView,
        LearningView,
    };
    use kheish_types::{LearningKind, LearningScope, LearningSensitivity, LearningSourceRef};

    fn sample_candidate(candidate_id: &str) -> LearningCandidateView {
        LearningCandidateView {
            candidate_id: candidate_id.to_string(),
            origin: LearningCandidateOrigin::Api,
            scope: LearningScope {
                kind: LearningScopeKind::Session,
                id: "demo".to_string(),
            },
            kind: LearningKind::Fact,
            sensitivity: LearningSensitivity::Scoped,
            content: "The workspace prefers concise JSON snapshots.".to_string(),
            confidence: 80,
            source: LearningSourceRef {
                run_id: Some("run-1".to_string()),
                session_id: Some("demo".to_string()),
                agent_id: Some("agent-1".to_string()),
                ..LearningSourceRef::default()
            },
            evidence_refs: Vec::new(),
            created_at_ms: 10,
            expires_at_ms: None,
            state: LearningCandidateState::Pending,
            automation_review: None,
            published_learning_id: None,
        }
    }

    fn sample_learning(learning_id: &str) -> LearningView {
        LearningView {
            learning_id: learning_id.to_string(),
            scope: LearningScope {
                kind: LearningScopeKind::Session,
                id: "demo".to_string(),
            },
            kind: LearningKind::Fact,
            sensitivity: LearningSensitivity::Scoped,
            content: "The workspace prefers concise JSON snapshots.".to_string(),
            confidence: 90,
            source: LearningSourceRef::default(),
            evidence_refs: Vec::new(),
            source_candidate_id: Some("learning-candidate-1".to_string()),
            created_at_ms: 10,
            published_at_ms: 11,
            expires_at_ms: None,
            status: LearningStatus::Active,
            publish_tier: LearningPublishTier::Active,
            policy_decision: Some(LearningPolicyDecision::Manual),
            policy_actor: Some("operator".to_string()),
            verification_status: LearningVerificationStatus::Unverified,
            supersedes: None,
            superseded_by: None,
            revoked_at_ms: None,
            revoked_reason: None,
        }
    }

    #[test]
    fn api_run_summary_candidates_are_rejected() {
        let mut candidate = sample_candidate("learning-candidate-run-summary");
        candidate.kind = LearningKind::RunSummary;
        let error = candidate
            .validate()
            .expect_err("candidate should be rejected");
        assert!(
            error
                .to_string()
                .contains("run_summary candidates are daemon-owned"),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn semantic_learning_validation_rejects_secret_material() {
        let mut candidate = sample_candidate("learning-candidate-secret");
        candidate.content = "The API key is sk-proj-secret.".to_string();
        let error = candidate
            .validate()
            .expect_err("semantic candidate should reject secret material");
        assert!(
            error.to_string().contains("secret material"),
            "unexpected error: {error:?}"
        );

        let mut learning = sample_learning("learning-secret");
        learning.content = "Access token is <redacted>.".to_string();
        let error = learning
            .validate()
            .expect_err("semantic learning should reject redaction markers");
        assert!(
            error.to_string().contains("secret material"),
            "unexpected error: {error:?}"
        );

        let mut run_summary = candidate;
        run_summary.origin = LearningCandidateOrigin::Daemon;
        run_summary.kind = LearningKind::RunSummary;
        run_summary.content = "Request mentioned <redacted>.".to_string();
        run_summary
            .validate()
            .expect("run summaries may retain already redacted text");
    }

    #[test]
    fn semantic_learning_secret_classifier_covers_common_key_shapes() {
        for content in [
            "api-key: abc123456789",
            "x-api-key: abc123456789",
            "clientSecret: hunter2hunter2",
            concat!("personal access token = ", "gh", "p_1234567890"),
            concat!("PRIVATE_KEY: -----BEGIN ", "PRIVATE KEY", "-----"),
            "secret_token=abc123456789",
            concat!("pat: ", "gh", "p_1234567890"),
            "{\"clientSecret\":\"hunter2hunter2\"}",
        ] {
            assert!(
                super::learning_content_has_secret_material(content),
                "expected secret-like learning content to be rejected: {content}"
            );
        }

        for content in [
            "Token budget is 1000.",
            "The client secretariat prefers written updates.",
            "Pat prefers concise reports.",
            "Authentication strategy is still undecided.",
        ] {
            assert!(
                !super::learning_content_has_secret_material(content),
                "expected benign learning content to remain allowed: {content}"
            );
        }
    }

    #[test]
    fn semantic_learning_keys_normalize_subject_value_forms() {
        assert_eq!(
            super::semantic_learning_content_key("The project codename is Atlas."),
            super::semantic_learning_content_key("project codename: atlas")
        );
        assert_eq!(
            super::semantic_learning_content_key("Preferred editor = Helix"),
            super::semantic_learning_content_key("preferred editor is Helix.")
        );
        assert_eq!(
            super::semantic_learning_subject_key("Le projet est Atlas").as_deref(),
            Some("projet")
        );

        let terms = super::semantic_learning_evidence_terms("Fact: the project codename is Atlas.");
        assert!(terms.contains("project"));
        assert!(terms.contains("codename"));
        assert!(terms.contains("atlas"));
        assert!(!terms.contains("the"));
        assert!(!terms.contains("is"));
    }

    #[test]
    fn semantic_learning_guard_corpus_meets_thresholds() {
        let secret_cases = [
            (
                concat!("The API key is ", "sk-", "proj-corpus-secret."),
                true,
            ),
            ("clientSecret: corpus-secret-value", true),
            (
                concat!("personal access token = ", "gh", "p_corpus_secret"),
                true,
            ),
            ("The project token budget is 1000.", false),
            ("Pat prefers compact status reports.", false),
            ("The private roadmap is still draft.", false),
        ];
        let mut true_positive = 0usize;
        let mut false_positive = 0usize;
        let mut false_negative = 0usize;
        for (content, expected_secret) in secret_cases {
            let actual_secret = super::learning_content_has_secret_material(content);
            match (actual_secret, expected_secret) {
                (true, true) => true_positive += 1,
                (true, false) => false_positive += 1,
                (false, true) => false_negative += 1,
                (false, false) => {}
            }
        }
        let precision = true_positive as f64 / (true_positive + false_positive).max(1) as f64;
        let recall = true_positive as f64 / (true_positive + false_negative).max(1) as f64;
        assert!(
            precision >= 0.99,
            "secret corpus precision dropped below threshold"
        );
        assert!(
            recall >= 0.99,
            "secret corpus recall dropped below threshold"
        );

        for (left, right) in [
            ("The project codename is Atlas.", "project codename: atlas"),
            ("Preferred editor = Helix", "preferred editor is Helix."),
            ("Le projet est Atlas", "projet: atlas"),
        ] {
            assert_eq!(
                super::semantic_learning_content_key(left),
                super::semantic_learning_content_key(right),
                "semantic duplicate key mismatch for `{left}` and `{right}`"
            );
        }

        for (left, right) in [
            (
                "Project codename is Atlas.",
                "Project codename is Borealis.",
            ),
            ("Preferred editor is Helix.", "Preferred editor is Vim."),
        ] {
            assert_eq!(
                super::semantic_learning_subject_key(left),
                super::semantic_learning_subject_key(right),
                "conflict corpus subject mismatch"
            );
            assert_ne!(
                super::semantic_learning_content_key(left),
                super::semantic_learning_content_key(right),
                "conflict corpus content key collapsed distinct values"
            );
        }
    }

    #[test]
    fn learning_store_round_trips_candidates_and_records() -> Result<()> {
        let temp = tempdir()?;
        let store = FileLearningStore::new(temp.path());
        let candidate = sample_candidate("learning-candidate-1");
        let learning = sample_learning("learning-1");
        store.save_candidate(&candidate)?;
        store.save_learning(&learning)?;

        assert_eq!(
            store
                .load_candidates()?
                .get("learning-candidate-1")
                .cloned()
                .expect("candidate"),
            candidate
        );
        assert_eq!(
            store
                .load_records()?
                .get("learning-1")
                .cloned()
                .expect("record"),
            learning
        );
        Ok(())
    }

    #[test]
    fn learning_store_next_seeds_skip_quarantined_suffixes() -> Result<()> {
        let temp = tempdir()?;
        let store = FileLearningStore::new(temp.path());
        store.save_candidate(&sample_candidate("learning-candidate-7"))?;
        store.save_learning(&sample_learning("learning-8"))?;
        std::fs::write(
            temp.path()
                .join("learning-candidates/learning-candidate-15.json.corrupt-1"),
            b"{}",
        )?;
        std::fs::write(
            temp.path().join("learnings/learning-12.json.corrupt-1"),
            b"{}",
        )?;

        assert_eq!(store.next_candidate_seed(), 16);
        assert_eq!(store.next_learning_seed(), 13);
        Ok(())
    }

    #[test]
    fn learning_scope_validation_requires_canonical_ids() {
        let mut candidate = sample_candidate("learning-candidate-1");
        candidate.scope.id = " demo ".to_string();
        assert!(
            candidate
                .validate()
                .expect_err("whitespace should be rejected")
                .to_string()
                .contains("leading or trailing whitespace")
        );

        candidate.scope.kind = LearningScopeKind::Workspace;
        candidate.scope.id = "custom".to_string();
        assert!(
            candidate
                .validate()
                .expect_err("custom workspace id should be rejected")
                .to_string()
                .contains("workspace learning scope id")
        );
    }
}
