//! Daemon-owned procedural skills promoted from reviewed procedure learnings.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use kheish_session::{atomic_write, write_json_pretty_atomically};
use kheish_skills::SkillRuntimeConfig;
use kheish_types::{
    LearningEvidenceRef, LearningScope, LearningVerificationStatus, SkillExecutionContext,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::learning::learning_content_has_secret_material;
use crate::state_files::read_json_or_quarantine;

const PROMOTED_SKILL_DIR_NAME: &str = "procedural";

/// The durable publication state of one promoted procedural skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningSkillStatus {
    /// The promoted skill draft is stored durably but not yet ready for execution.
    Draft,
    /// The promoted skill passed sterile verification but is not yet rolled out.
    Verified,
    /// The promoted skill is limited to canary evaluation and stays out of the visible catalog.
    Canary,
    /// The promoted skill is currently available in the daemon skill catalog.
    Active,
    /// The promoted skill was revoked and removed from the catalog.
    Revoked,
}

/// The daemon-validated rollout evidence kind for one promoted procedural skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningSkillRolloutKind {
    /// A sterile verification run used before the skill can enter rollout.
    Verification,
    /// A canary run used before the skill can become visible as active.
    Canary,
}

/// One structured lifecycle event retained with a promoted procedural skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningSkillLifecycleEvent {
    /// The stable event name, such as `promote`, `rollout_result`, `revoke`, or `rollback`.
    pub event: String,
    /// The event timestamp in milliseconds since the Unix epoch.
    pub recorded_at_ms: u64,
    /// The previous skill status when this event changed status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_status: Option<LearningSkillStatus>,
    /// The skill status after this event.
    pub to_status: LearningSkillStatus,
    /// The rollout kind for rollout-result events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_kind: Option<LearningSkillRolloutKind>,
    /// The daemon run that produced rollout evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The daemon session that owned the rollout evidence run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Whether rollout evidence succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    /// The promoted-skill definition fingerprint observed by this event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_fingerprint: Option<String>,
    /// Optional sanitized operator or daemon reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One validated rollout result retained against the current promoted-skill definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LearningSkillRolloutResult {
    /// Whether the result verifies the draft or canary rollout.
    pub(crate) kind: LearningSkillRolloutKind,
    /// The daemon run that produced the evidence.
    pub(crate) run_id: String,
    /// The session that owned the evidence run.
    pub(crate) session_id: String,
    /// Whether the run completed and matched the expected marker.
    pub(crate) success: bool,
    /// Optional caller-supplied current definition fingerprint guard.
    pub(crate) definition_fingerprint: Option<String>,
    /// The audit timestamp in milliseconds since the Unix epoch.
    pub(crate) recorded_at_ms: u64,
}

/// One daemon-owned procedural skill promoted from a reviewed learning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningSkillView {
    /// The stable skill name exposed through the daemon skill catalog.
    pub skill_name: String,
    /// The reviewed procedure learning that owns this promoted skill.
    pub source_learning_id: String,
    /// The durable learning scope inherited from the source learning.
    pub source_scope: LearningScope,
    /// The current publication state of the promoted skill.
    pub status: LearningSkillStatus,
    /// The human-readable skill description.
    pub description: String,
    /// Optional when-to-use guidance surfaced in the catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    /// Optional version string pinned with the promoted skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The rendered instructions persisted in the daemon-owned skill file.
    pub instructions: String,
    /// The canonical skill markdown path.
    pub skill_path: String,
    /// The canonical skill directory.
    pub skill_root: String,
    /// The current content digest loaded from the skill catalog.
    pub digest: String,
    /// The fingerprint of the current prompt-visible and runtime skill definition.
    #[serde(default)]
    pub definition_fingerprint: String,
    /// The runtime configuration persisted with the promoted skill.
    pub runtime: SkillRuntimeConfig,
    /// Immutable evidence references retained with this promoted skill record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<LearningEvidenceRef>,
    /// Structured lifecycle events retained for rollout, activation, revoke, and rollback audit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lifecycle_events: Vec<LearningSkillLifecycleEvent>,
    /// The current verification state retained with the promoted skill.
    #[serde(default)]
    pub verification_status: LearningVerificationStatus,
    /// The count of successful verification or rollout runs retained with this record.
    #[serde(default)]
    pub successful_run_count: u32,
    /// The count of distinct sessions observed in successful evidence.
    #[serde(default)]
    pub distinct_session_count: u32,
    /// Verification run identifiers retained for audit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verifier_run_ids: Vec<String>,
    /// Whether at least one verification run executed on a real daemon path.
    #[serde(default)]
    pub real_daemon_verified: bool,
    /// Optional workspace digest pinned when the latest verification succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified_workspace_digest: Option<String>,
    /// The count of successful canary executions retained with this record.
    #[serde(default)]
    pub canary_success_count: u32,
    /// The count of failed canary executions retained with this record.
    #[serde(default)]
    pub canary_failure_count: u32,
    /// The promotion timestamp in milliseconds since the Unix epoch.
    pub promoted_at_ms: u64,
    /// The revocation timestamp in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at_ms: Option<u64>,
    /// The optional human-readable revocation reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_reason: Option<String>,
}

/// Operator-provided input used when promoting one procedure learning to a skill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LearningSkillDraft {
    /// The stable skill name that should become visible in the catalog.
    pub(crate) skill_name: String,
    /// Optional replacement description. When omitted, the procedure learning content is used.
    pub(crate) description: Option<String>,
    /// Optional when-to-use guidance stored in the skill frontmatter.
    pub(crate) when_to_use: Option<String>,
    /// Optional version string stored in the skill frontmatter.
    pub(crate) version: Option<String>,
    /// The rendered instructions stored in the promoted skill.
    pub(crate) instructions: String,
    /// The runtime configuration used when the promoted skill is activated.
    pub(crate) runtime: SkillRuntimeConfig,
    /// The initial durable rollout state that should be assigned to the promoted skill.
    pub(crate) status: LearningSkillStatus,
    /// Immutable evidence references retained with the promoted skill.
    pub(crate) evidence_refs: Vec<LearningEvidenceRef>,
    /// Verification status retained with the promoted skill.
    pub(crate) verification_status: LearningVerificationStatus,
    /// Successful verification or rollout run count retained with the promoted skill.
    pub(crate) successful_run_count: u32,
    /// Distinct successful session count retained with the promoted skill.
    pub(crate) distinct_session_count: u32,
    /// Verification run identifiers retained with the promoted skill.
    pub(crate) verifier_run_ids: Vec<String>,
    /// Whether at least one verification run executed on a real daemon path.
    pub(crate) real_daemon_verified: bool,
    /// Optional workspace digest pinned when verification succeeded.
    pub(crate) last_verified_workspace_digest: Option<String>,
    /// Successful canary execution count retained with the promoted skill.
    pub(crate) canary_success_count: u32,
    /// Failed canary execution count retained with the promoted skill.
    pub(crate) canary_failure_count: u32,
}

impl LearningSkillView {
    /// Validates one promoted skill record before persistence.
    pub(crate) fn validate(&self) -> Result<()> {
        validate_skill_name(&self.skill_name)?;
        validate_single_line("description", &self.description)?;
        if let Some(value) = self.when_to_use.as_deref() {
            validate_single_line("when_to_use", value)?;
        }
        if let Some(value) = self.version.as_deref() {
            validate_single_line("version", value)?;
        }
        validate_skill_instructions(&self.instructions)?;
        validate_runtime_config(&self.runtime)?;
        validate_learning_skill_lifecycle_events(self)?;
        validate_learning_skill_status_consistency(self)?;
        if !self.definition_fingerprint.is_empty()
            && self.definition_fingerprint != learning_skill_definition_fingerprint(self)
        {
            bail!("promoted skill definition fingerprint is stale");
        }
        Ok(())
    }
}

/// Normalizes optional operator-facing one-line text fields used by promoted skills.
pub(crate) fn normalize_learning_skill_text(value: &str) -> Option<String> {
    let normalized = normalize_single_line(value);
    (!normalized.is_empty()).then_some(normalized)
}

fn validate_learning_skill_status_consistency(skill: &LearningSkillView) -> Result<()> {
    match skill.status {
        LearningSkillStatus::Draft => {}
        LearningSkillStatus::Verified
        | LearningSkillStatus::Canary
        | LearningSkillStatus::Active => {
            if skill.verification_status != LearningVerificationStatus::Verified {
                bail!(
                    "promoted skills in {:?} status must use verified verification status",
                    skill.status
                );
            }
            if !skill.real_daemon_verified
                || skill.successful_run_count == 0
                || skill.verifier_run_ids.is_empty()
            {
                bail!(
                    "promoted skills in {:?} status require daemon-validated verification evidence",
                    skill.status
                );
            }
            if skill.status == LearningSkillStatus::Active {
                if skill.canary_success_count == 0 {
                    bail!("active promoted skills require at least one successful canary rollout");
                }
                if skill.canary_failure_count > 0 {
                    bail!("active promoted skills require zero canary failures");
                }
            }
        }
        LearningSkillStatus::Revoked => {}
    }
    Ok(())
}

fn validate_learning_skill_lifecycle_events(skill: &LearningSkillView) -> Result<()> {
    for event in &skill.lifecycle_events {
        validate_single_line("lifecycle event", &event.event)?;
        if let Some(reason) = event.reason.as_deref() {
            validate_single_line("lifecycle reason", reason)?;
            if learning_content_has_secret_material(reason) {
                bail!("lifecycle reason appears to contain secret material");
            }
        }
        if let Some(fingerprint) = event.definition_fingerprint.as_deref() {
            validate_single_line("definition_fingerprint", fingerprint)?;
        }
        if let Some(run_id) = event.run_id.as_deref() {
            validate_single_line("run_id", run_id)?;
        }
        if let Some(session_id) = event.session_id.as_deref() {
            validate_single_line("session_id", session_id)?;
        }
    }
    Ok(())
}

/// Filesystem-backed store for promoted procedural skill records and skill files.
#[derive(Clone, Debug)]
pub(crate) struct FileLearningSkillStore {
    root: PathBuf,
}

impl FileLearningSkillStore {
    /// Creates a new promoted-skill store rooted under one daemon state directory.
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn records_root(&self) -> PathBuf {
        self.root.join("learning-skills")
    }

    fn history_root(&self) -> PathBuf {
        self.root.join("learning-skills-history")
    }

    fn catalog_root(&self) -> PathBuf {
        self.root.join("skills")
    }

    fn promoted_skill_root(&self, learning_id: &str) -> PathBuf {
        self.catalog_root()
            .join(PROMOTED_SKILL_DIR_NAME)
            .join(learning_id)
    }

    /// Returns the canonical daemon-owned root used for one promoted skill.
    pub(crate) fn expected_skill_root(&self, learning_id: &str) -> PathBuf {
        self.promoted_skill_root(learning_id)
    }

    /// Returns the canonical daemon-owned markdown path used for one promoted skill.
    pub(crate) fn expected_skill_path(&self, learning_id: &str) -> PathBuf {
        self.expected_skill_root(learning_id).join("SKILL.md")
    }

    fn record_path(&self, skill_name: &str) -> PathBuf {
        self.records_root()
            .join(format!("{}.json", skill_name_record_key(skill_name)))
    }

    fn history_dir(&self, skill_name: &str) -> PathBuf {
        self.history_root().join(skill_name_record_key(skill_name))
    }

    fn history_path(&self, record: &LearningSkillView, recorded_at_ms: u64) -> Result<PathBuf> {
        let encoded = serde_json::to_vec(record)?;
        let digest = Sha256::digest(encoded);
        let digest_hex = format!("{:x}", digest);
        Ok(self.history_dir(&record.skill_name).join(format!(
            "{recorded_at_ms:020}-{}-{}.json",
            status_history_name(&record.status),
            &digest_hex[..16]
        )))
    }

    /// Loads every persisted promoted-skill record, quarantining corrupted files.
    pub(crate) fn load(&self) -> Result<BTreeMap<String, LearningSkillView>> {
        let root = self.records_root();
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
            let Some(record) =
                read_json_or_quarantine::<LearningSkillView>(&path, "learning skill record")?
            else {
                continue;
            };
            if let Err(error) = record.validate() {
                warn!(
                    path = %path.display(),
                    error = %error,
                    "ignoring invalid learning skill record"
                );
                continue;
            }
            records.insert(record.skill_name.clone(), record);
        }
        Ok(records)
    }

    /// Persists one promoted-skill record atomically.
    pub(crate) fn save(&self, record: &LearningSkillView) -> Result<()> {
        record.validate()?;
        write_json_pretty_atomically(&self.record_path(&record.skill_name), record)
    }

    /// Saves one immutable audit snapshot for a promoted skill before mutation.
    pub(crate) fn save_history(
        &self,
        record: &LearningSkillView,
        recorded_at_ms: u64,
    ) -> Result<()> {
        record.validate()?;
        let path = self.history_path(record, recorded_at_ms)?;
        write_json_pretty_atomically(&path, record)
    }

    /// Loads the newest historical active snapshot for one promoted skill.
    pub(crate) fn load_latest_active_history(
        &self,
        skill_name: &str,
    ) -> Result<Option<LearningSkillView>> {
        let root = self.history_dir(skill_name);
        if !root.exists() {
            return Ok(None);
        }
        let mut paths = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                paths.push(path);
            }
        }
        paths.sort();
        paths.reverse();
        for path in paths {
            let Some(record) =
                read_json_or_quarantine::<LearningSkillView>(&path, "learning skill history")?
            else {
                continue;
            };
            if let Err(error) = record.validate() {
                warn!(
                    path = %path.display(),
                    error = %error,
                    "ignoring invalid learning skill history record"
                );
                continue;
            }
            if record.skill_name == skill_name && record.status == LearningSkillStatus::Active {
                return Ok(Some(record));
            }
        }
        Ok(None)
    }

    /// Deletes one persisted promoted-skill record when it exists.
    pub(crate) fn delete(&self, skill_name: &str) -> Result<()> {
        let path = self.record_path(skill_name);
        if !path.exists() {
            return Ok(());
        }
        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))
    }

    /// Writes the daemon-owned skill files for one promoted record atomically.
    pub(crate) fn write_skill_files(&self, record: &LearningSkillView) -> Result<PathBuf> {
        record.validate()?;
        let skill_root = self.promoted_skill_root(&record.source_learning_id);
        fs::create_dir_all(skill_root.join("agents"))
            .with_context(|| format!("failed to create {}", skill_root.display()))?;
        let skill_markdown = render_skill_markdown(record)?;
        atomic_write(&skill_root.join("SKILL.md"), skill_markdown.as_bytes())?;
        let runtime_yaml = render_skill_runtime_config(&record.runtime)?;
        atomic_write(
            &skill_root.join("agents").join("kheish.yaml"),
            runtime_yaml.as_bytes(),
        )?;
        Ok(skill_root)
    }

    /// Removes the daemon-owned skill files for one promoted record.
    pub(crate) fn remove_skill_files(&self, record: &LearningSkillView) -> Result<()> {
        let skill_root = self.promoted_skill_root(&record.source_learning_id);
        if !skill_root.exists() {
            return Ok(());
        }
        fs::remove_dir_all(&skill_root)
            .with_context(|| format!("failed to remove {}", skill_root.display()))
    }

    /// Lists every promoted-learning directory currently mounted under the daemon-owned catalog.
    pub(crate) fn list_catalog_learning_ids(&self) -> Result<Vec<String>> {
        let root = self.catalog_root().join(PROMOTED_SKILL_DIR_NAME);
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut learning_ids = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                learning_ids.push(name.to_string());
            }
        }
        learning_ids.sort();
        Ok(learning_ids)
    }

    /// Removes one orphaned promoted-learning directory when it still exists.
    pub(crate) fn remove_skill_files_by_learning_id(&self, learning_id: &str) -> Result<()> {
        let root = self.promoted_skill_root(learning_id);
        if !root.exists() {
            return Ok(());
        }
        fs::remove_dir_all(&root).with_context(|| format!("failed to remove {}", root.display()))
    }
}

fn validate_skill_name(value: &str) -> Result<()> {
    let normalized = normalize_single_line(value);
    if normalized.is_empty() {
        bail!("skill_name is required");
    }
    if normalized != value.trim() {
        bail!("skill_name must not contain leading, trailing, or repeated whitespace");
    }
    if !normalized
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '_' | '-'))
    {
        bail!("skill_name may contain only ASCII letters, digits, ':', '_' and '-'");
    }
    Ok(())
}

fn validate_single_line(field: &str, value: &str) -> Result<()> {
    let normalized = normalize_single_line(value);
    if normalized.is_empty() {
        bail!("{field} is required");
    }
    if normalized != value.trim() {
        bail!("{field} must not contain leading, trailing, or repeated whitespace");
    }
    Ok(())
}

fn validate_skill_instructions(value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("instructions are required");
    }
    Ok(())
}

fn validate_runtime_config(runtime: &SkillRuntimeConfig) -> Result<()> {
    if runtime.context == SkillExecutionContext::Inline
        && (runtime.agent_profile.is_some()
            || runtime.provider.is_some()
            || runtime.model.is_some()
            || runtime.fallback_model.is_some())
    {
        bail!("inline promoted skills cannot declare child-only runtime overrides");
    }
    Ok(())
}

fn normalize_single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn skill_name_record_key(skill_name: &str) -> String {
    format!("{:x}", Sha256::digest(skill_name.as_bytes()))
}

fn status_history_name(status: &LearningSkillStatus) -> &'static str {
    match status {
        LearningSkillStatus::Draft => "draft",
        LearningSkillStatus::Verified => "verified",
        LearningSkillStatus::Canary => "canary",
        LearningSkillStatus::Active => "active",
        LearningSkillStatus::Revoked => "revoked",
    }
}

/// Computes the fingerprint for the current promoted-skill definition.
pub(crate) fn learning_skill_definition_fingerprint(record: &LearningSkillView) -> String {
    let runtime = serde_json::to_vec(&record.runtime).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(record.skill_name.as_bytes());
    hasher.update(b"\0description\0");
    hasher.update(record.description.as_bytes());
    hasher.update(b"\0when_to_use\0");
    if let Some(value) = record.when_to_use.as_deref() {
        hasher.update(value.as_bytes());
    }
    hasher.update(b"\0version\0");
    if let Some(value) = record.version.as_deref() {
        hasher.update(value.as_bytes());
    }
    hasher.update(b"\0instructions\0");
    hasher.update(record.instructions.trim().as_bytes());
    hasher.update(b"\0runtime\0");
    hasher.update(&runtime);
    format!("{:x}", hasher.finalize())
}

#[derive(Serialize)]
struct SkillFrontmatterFile<'a> {
    name: &'a str,
    description: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    when_to_use: Option<&'a str>,
}

#[derive(Serialize)]
struct SkillRuntimeConfigFile<'a> {
    #[serde(skip_serializing_if = "slice_is_empty")]
    allowed_tools: &'a [String],
    #[serde(skip_serializing_if = "slice_is_empty")]
    blocked_tools: &'a [String],
    context: SkillExecutionContext,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_profile: Option<&'a String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<&'a String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback_model: Option<&'a String>,
}

fn slice_is_empty<T>(value: &&[T]) -> bool {
    value.is_empty()
}

fn render_skill_markdown(record: &LearningSkillView) -> Result<String> {
    let frontmatter = SkillFrontmatterFile {
        name: &record.skill_name,
        description: &record.description,
        version: record.version.as_deref(),
        when_to_use: record.when_to_use.as_deref(),
    };
    let yaml = serde_yaml::to_string(&frontmatter)?;
    Ok(format!(
        "---\n{}---\n{}\n",
        yaml,
        record.instructions.trim()
    ))
}

fn render_skill_runtime_config(runtime: &SkillRuntimeConfig) -> Result<String> {
    let yaml = serde_yaml::to_string(&SkillRuntimeConfigFile {
        allowed_tools: &runtime.allowed_tools,
        blocked_tools: &runtime.blocked_tools,
        context: runtime.context,
        agent_profile: runtime.agent_profile.as_ref(),
        provider: runtime.provider.as_ref(),
        model: runtime.model.as_ref(),
        fallback_model: runtime.fallback_model.as_ref(),
    })?;
    Ok(yaml)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn build_learning_skill_record(
    learning_id: &str,
    scope: &LearningScope,
    promoted_at_ms: u64,
    draft: LearningSkillDraft,
    loaded_skill: &kheish_skills::SkillDefinition,
) -> Result<LearningSkillView> {
    let description = draft
        .description
        .as_deref()
        .map(normalize_single_line)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("description is required"))?;
    let when_to_use = draft
        .when_to_use
        .as_deref()
        .map(normalize_single_line)
        .filter(|value| !value.is_empty());
    let version = draft
        .version
        .as_deref()
        .map(normalize_single_line)
        .filter(|value| !value.is_empty());
    let record = LearningSkillView {
        skill_name: draft.skill_name,
        source_learning_id: learning_id.to_string(),
        source_scope: scope.clone(),
        status: draft.status,
        description,
        when_to_use,
        version,
        instructions: draft.instructions.trim().to_string(),
        skill_path: loaded_skill.skill_path.display().to_string(),
        skill_root: loaded_skill.skill_root.display().to_string(),
        digest: loaded_skill.digest.clone(),
        definition_fingerprint: String::new(),
        runtime: draft.runtime,
        evidence_refs: draft.evidence_refs,
        lifecycle_events: Vec::new(),
        verification_status: draft.verification_status,
        successful_run_count: draft.successful_run_count,
        distinct_session_count: draft.distinct_session_count,
        verifier_run_ids: draft.verifier_run_ids,
        real_daemon_verified: draft.real_daemon_verified,
        last_verified_workspace_digest: draft.last_verified_workspace_digest,
        canary_success_count: draft.canary_success_count,
        canary_failure_count: draft.canary_failure_count,
        promoted_at_ms,
        revoked_at_ms: None,
        revoked_reason: None,
    };
    let record = LearningSkillView {
        definition_fingerprint: learning_skill_definition_fingerprint(&record),
        ..record
    };
    record.validate()?;
    if loaded_skill.name != record.skill_name {
        bail!(
            "loaded promoted skill name mismatch: expected {}, got {}",
            record.skill_name,
            loaded_skill.name
        );
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use kheish_skills::{SharedSkillRegistry, SkillRuntimeConfig, SkillScope};
    use kheish_types::{LearningScopeKind, LearningVerificationStatus, SkillExecutionContext};
    use tempfile::tempdir;

    use super::{
        FileLearningSkillStore, LearningSkillDraft, LearningSkillStatus, LearningSkillView,
        build_learning_skill_record,
    };

    fn sample_record() -> LearningSkillView {
        LearningSkillView {
            skill_name: "learning:review".to_string(),
            source_learning_id: "learning-1".to_string(),
            source_scope: kheish_types::LearningScope {
                kind: LearningScopeKind::Session,
                id: "demo".to_string(),
            },
            status: LearningSkillStatus::Active,
            description: "Review the touched files before you patch them.".to_string(),
            when_to_use: Some("Use when the user explicitly asks for learning:review.".to_string()),
            version: Some("1".to_string()),
            instructions: "Reply with exactly `LEARNING_SKILL_OK`.".to_string(),
            skill_path: "/tmp/skill/SKILL.md".to_string(),
            skill_root: "/tmp/skill".to_string(),
            digest: "digest".to_string(),
            definition_fingerprint: String::new(),
            runtime: SkillRuntimeConfig {
                context: SkillExecutionContext::Fork,
                agent_profile: Some("verification".to_string()),
                ..SkillRuntimeConfig::default()
            },
            evidence_refs: Vec::new(),
            lifecycle_events: Vec::new(),
            verification_status: LearningVerificationStatus::Verified,
            successful_run_count: 2,
            distinct_session_count: 1,
            verifier_run_ids: vec!["run-verify-1".to_string(), "run-canary-1".to_string()],
            real_daemon_verified: true,
            last_verified_workspace_digest: None,
            canary_success_count: 1,
            canary_failure_count: 0,
            promoted_at_ms: 10,
            revoked_at_ms: None,
            revoked_reason: None,
        }
    }

    #[test]
    fn promoted_skill_files_round_trip_through_shared_registry_reload() -> Result<()> {
        let temp = tempdir()?;
        let store = FileLearningSkillStore::new(temp.path());
        let record = sample_record();
        store.write_skill_files(&record)?;

        let registry = SharedSkillRegistry::load_from_roots(vec![kheish_skills::SkillRoot {
            path: temp.path().join("skills"),
            scope: SkillScope::Explicit,
        }]);
        let loaded = registry
            .get("learning:review")
            .expect("promoted skill should load");
        assert_eq!(loaded.runtime.context, SkillExecutionContext::Fork);
        assert_eq!(
            loaded.runtime.agent_profile.as_deref(),
            Some("verification")
        );
        assert!(loaded.instructions.contains("LEARNING_SKILL_OK"));
        Ok(())
    }

    #[test]
    fn build_learning_skill_record_uses_loaded_catalog_metadata() -> Result<()> {
        let temp = tempdir()?;
        let store = FileLearningSkillStore::new(temp.path());
        let record = sample_record();
        store.write_skill_files(&record)?;
        let registry = SharedSkillRegistry::load_from_roots(vec![kheish_skills::SkillRoot {
            path: temp.path().join("skills"),
            scope: SkillScope::Explicit,
        }]);
        let loaded = registry
            .get("learning:review")
            .expect("promoted skill should load");
        let built = build_learning_skill_record(
            "learning-1",
            &kheish_types::LearningScope {
                kind: LearningScopeKind::Session,
                id: "demo".to_string(),
            },
            10,
            LearningSkillDraft {
                skill_name: "learning:review".to_string(),
                description: Some("Review the touched files before you patch them.".to_string()),
                when_to_use: Some(
                    "Use when the user explicitly asks for learning:review.".to_string(),
                ),
                version: Some("1".to_string()),
                instructions: "Reply with exactly `LEARNING_SKILL_OK`.".to_string(),
                runtime: SkillRuntimeConfig {
                    context: SkillExecutionContext::Fork,
                    agent_profile: Some("verification".to_string()),
                    ..SkillRuntimeConfig::default()
                },
                status: LearningSkillStatus::Active,
                evidence_refs: Vec::new(),
                verification_status: LearningVerificationStatus::Verified,
                successful_run_count: 2,
                distinct_session_count: 1,
                verifier_run_ids: vec!["run-verify-1".to_string(), "run-canary-1".to_string()],
                real_daemon_verified: true,
                last_verified_workspace_digest: None,
                canary_success_count: 1,
                canary_failure_count: 0,
            },
            &loaded,
        )?;

        assert_eq!(built.skill_name, "learning:review");
        assert_eq!(built.digest, loaded.digest);
        assert!(built.skill_path.ends_with("SKILL.md"));
        Ok(())
    }

    #[test]
    fn learning_skill_store_uses_collision_safe_record_paths() -> Result<()> {
        let temp = tempdir()?;
        let store = FileLearningSkillStore::new(temp.path());
        let mut left = sample_record();
        left.skill_name = "learning:foo".to_string();
        let mut right = sample_record();
        right.skill_name = "learning-foo".to_string();

        store.save(&left)?;
        store.save(&right)?;

        let loaded = store.load()?;
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains_key("learning:foo"));
        assert!(loaded.contains_key("learning-foo"));
        Ok(())
    }

    #[test]
    fn learning_skill_store_skips_invalid_active_rollout_records() -> Result<()> {
        let temp = tempdir()?;
        let store = FileLearningSkillStore::new(temp.path());
        let mut invalid = sample_record();
        invalid.skill_name = "learning:invalid-active".to_string();
        invalid.successful_run_count = 0;
        invalid.distinct_session_count = 0;
        invalid.verifier_run_ids.clear();
        invalid.real_daemon_verified = false;
        invalid.canary_success_count = 0;
        let path = temp.path().join("learning-skills").join(format!(
            "{}.json",
            super::skill_name_record_key(&invalid.skill_name)
        ));
        std::fs::create_dir_all(path.parent().expect("record parent"))?;
        std::fs::write(&path, serde_json::to_vec_pretty(&invalid)?)?;

        let loaded = store.load()?;
        assert!(loaded.is_empty());
        Ok(())
    }

    #[test]
    fn learning_skill_validation_rejects_path_unsafe_names() {
        let mut record = sample_record();
        record.skill_name = "learning/bad".to_string();
        let error = record
            .validate()
            .expect_err("promoted skill names should stay safe in path parameters");
        assert!(error.to_string().contains("may contain only"));
    }
}
