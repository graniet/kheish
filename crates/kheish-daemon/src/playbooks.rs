//! Durable Playbook definitions and Flow projections.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use kheish_session::write_json_pretty_atomically;
use kheish_types::{CapabilityScope, CredentialScope};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::runs::{DaemonRunKind, RunView};
use crate::state_files::read_json_or_quarantine;

/// Daemon-owned metadata key used to correlate normal Kheish runs with one Flow.
pub const KHEISH_FLOW_METADATA_KEY: &str = "kheish_flow";

/// Mutable release state for one immutable Playbook version.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybookReleaseStatus {
    /// The version is stored but not approved for execution.
    #[default]
    Draft,
    /// The version has evidence-backed verification but is not the default active version.
    Verified,
    /// The version is approved for limited rollout.
    Canary,
    /// The version is the active operator version.
    Active,
    /// The version must no longer be started.
    Revoked,
}

impl PlaybookReleaseStatus {
    pub(crate) fn is_startable(&self) -> bool {
        matches!(
            self,
            PlaybookReleaseStatus::Verified
                | PlaybookReleaseStatus::Canary
                | PlaybookReleaseStatus::Active
        )
    }

    pub(crate) fn requires_evidence(&self) -> bool {
        matches!(
            self,
            PlaybookReleaseStatus::Verified
                | PlaybookReleaseStatus::Canary
                | PlaybookReleaseStatus::Active
        )
    }
}

/// One evidence reference used by Playbook release review or Flow execution review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowEvidenceRef {
    /// The daemon primitive kind, such as `run`, `debug_artifact`, `task`, or `doc_review`.
    pub kind: String,
    /// The referenced primitive identifier.
    pub id: String,
    /// Optional human-readable note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Default runtime route preferences carried by a Playbook manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookRuntimeDefaults {
    /// Optional provider or route identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Optional model identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Declarative tool policy carried by a Playbook manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookToolPolicy {
    /// Whether this policy is enforced against scoped Flow tool calls.
    #[serde(default)]
    pub enforce: bool,
    /// Tool names explicitly allowed by this Playbook.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Tool names explicitly blocked by this Playbook.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

/// Capability and credential scope hints carried by a Playbook manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookScopePolicy {
    /// Human-readable capability scope guidance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_scope: Option<String>,
    /// Human-readable credential scope guidance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_scope: Option<String>,
    /// Required session capability scope. Flow start fails if the session is wider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_capability_scope: Option<CapabilityScope>,
    /// Required session credential scope. Flow start fails if the session is wider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_credential_scope: Option<CredentialScope>,
}

/// One role expected by a Playbook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookRole {
    /// Stable role identifier inside the Playbook.
    pub role_id: String,
    /// Human-readable purpose.
    pub purpose: String,
}

/// One explicit evidence requirement used by Flow contract validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookEvidenceRequirement {
    /// Required evidence kind, usually a daemon primitive kind such as `run` or `task`.
    pub kind: String,
    /// Required evidence id. Use the exact primitive id or an operator-defined evidence id.
    pub id: String,
    /// Optional human-readable note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One phase expected by a Playbook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookPhase {
    /// Stable phase identifier inside the Playbook.
    pub phase_id: String,
    /// Human-readable phase objective.
    pub objective: String,
    /// Optional acceptance criteria scoped to this phase.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_criteria: Vec<String>,
    /// Evidence required before this phase gate is considered satisfied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_evidence: Vec<PlaybookEvidenceRequirement>,
}

/// One input expected by a Playbook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookInputSpec {
    /// Stable input name.
    pub name: String,
    /// Human-readable input description.
    pub description: String,
    /// Whether the caller must supply this input.
    #[serde(default)]
    pub required: bool,
}

/// Immutable, digest-covered Playbook version body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlaybookManifest {
    /// Stable Playbook identifier.
    pub playbook_id: String,
    /// Stable immutable version string.
    pub version: String,
    /// User-visible name.
    pub title: String,
    /// The operator objective the Playbook is meant to achieve.
    pub objective: String,
    /// Optional longer description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Expected inputs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<PlaybookInputSpec>,
    /// Preconditions that should be true before starting a Flow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<String>,
    /// Expected roles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<PlaybookRole>,
    /// Expected phases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<PlaybookPhase>,
    /// Top-level acceptance criteria.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_criteria: Vec<String>,
    /// Evidence that should be collected before the Flow is considered complete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_expectations: Vec<String>,
    /// Evidence required before a completed Flow can report success.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_evidence: Vec<PlaybookEvidenceRequirement>,
    /// Declarative tool policy.
    #[serde(default)]
    pub tools: PlaybookToolPolicy,
    /// Default route/model hints.
    #[serde(default)]
    pub runtime_defaults: PlaybookRuntimeDefaults,
    /// Scope guidance.
    #[serde(default)]
    pub scopes: PlaybookScopePolicy,
    /// Optional caller metadata included in the immutable digest.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// Immutable stored Playbook version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlaybookVersionRecord {
    /// Stable Playbook identifier.
    pub playbook_id: String,
    /// Stable immutable version string.
    pub version: String,
    /// SHA-256 digest of the manifest body.
    pub digest: String,
    /// Immutable manifest.
    pub manifest: PlaybookManifest,
    /// Creation timestamp.
    pub created_at_ms: u64,
}

/// Mutable release metadata for one immutable Playbook version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookReleaseRecord {
    /// Current release lifecycle state.
    pub status: PlaybookReleaseStatus,
    /// Last release metadata update.
    pub updated_at_ms: u64,
    /// Evidence justifying non-draft release states.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<FlowEvidenceRef>,
    /// Optional revocation reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_reason: Option<String>,
}

/// Durable Playbook catalog record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlaybookRecord {
    /// Stable Playbook identifier.
    pub playbook_id: String,
    /// Immutable versions keyed by version.
    #[serde(default)]
    pub versions: BTreeMap<String, PlaybookVersionRecord>,
    /// Mutable release metadata keyed by version.
    #[serde(default)]
    pub releases: BTreeMap<String, PlaybookReleaseRecord>,
    /// Creation timestamp.
    pub created_at_ms: u64,
    /// Last update timestamp.
    pub updated_at_ms: u64,
}

/// One immutable Playbook version reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookVersionRef {
    /// Stable Playbook identifier.
    pub playbook_id: String,
    /// Immutable version string.
    pub version: String,
    /// Expected manifest digest.
    pub digest: String,
}

/// Compact Playbook version summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookVersionSummary {
    /// Immutable version string.
    pub version: String,
    /// Manifest digest.
    pub digest: String,
    /// Mutable release lifecycle state.
    pub status: PlaybookReleaseStatus,
    /// Creation timestamp.
    pub created_at_ms: u64,
    /// Last release metadata update.
    pub updated_at_ms: u64,
    /// Evidence attached to the current release state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<FlowEvidenceRef>,
    /// Revocation reason when the version is revoked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_reason: Option<String>,
}

/// Playbook view returned by list and detail APIs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlaybookView {
    /// Stable Playbook identifier.
    pub playbook_id: String,
    /// Latest created version when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    /// Active version when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_version: Option<String>,
    /// Known immutable versions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub versions: Vec<PlaybookVersionSummary>,
    /// Full selected version when returning detail views.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_version: Option<PlaybookVersionRecord>,
}

/// Structural validation result for a Playbook manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookValidationResult {
    /// Whether the manifest is structurally valid.
    pub valid: bool,
    /// Digest of the submitted manifest when serialization succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Blocking validation errors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    /// Non-blocking validation warnings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Create or idempotently return one immutable Playbook version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreatePlaybookRequest {
    /// Immutable manifest body.
    pub manifest: PlaybookManifest,
}

/// Validate one Playbook manifest without storing it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ValidatePlaybookRequest {
    /// Immutable manifest body.
    pub manifest: PlaybookManifest,
}

/// Publish one immutable Playbook version into a startable release state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishPlaybookRequest {
    /// Target version.
    pub version: String,
    /// Expected manifest digest.
    pub digest: String,
    /// Release state to apply. Defaults to `active`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<PlaybookReleaseStatus>,
    /// Evidence justifying this release.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<FlowEvidenceRef>,
}

/// Revoke one immutable Playbook version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokePlaybookRequest {
    /// Target version.
    pub version: String,
    /// Expected manifest digest.
    pub digest: String,
    /// Optional reason recorded with the release metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional evidence justifying revocation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<FlowEvidenceRef>,
}

/// Flow status derived from referenced daemon primitives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowStatus {
    /// The Flow exists but has no run reference yet.
    Pending,
    /// The referenced run is queued or running.
    Running,
    /// The referenced run is waiting on approvals or user questions.
    Waiting,
    /// The referenced run completed.
    Succeeded,
    /// The referenced run failed.
    Failed,
    /// The referenced run was cancelled.
    Cancelled,
    /// The referenced run was interrupted.
    Interrupted,
    /// The Flow references a run that is not currently available.
    Unknown,
}

/// Derived phase gate status for one Flow view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowPhaseStatus {
    /// Earlier Flow work is still running or this phase has no terminal evidence yet.
    Pending,
    /// All requirements for the phase are satisfied.
    Satisfied,
    /// The Flow reached terminal success but this phase still misses required evidence.
    Blocked,
}

/// Derived phase projection for one Flow view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowPhaseState {
    /// Stable phase identifier from the Playbook manifest.
    pub phase_id: String,
    /// Derived phase gate status.
    pub status: FlowPhaseStatus,
    /// Missing evidence references for this phase.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_evidence: Vec<PlaybookEvidenceRequirement>,
}

/// One contract check used by generic Flow validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowContractCheck {
    pub name: String,
    pub passed: bool,
    pub details: String,
}

/// Generic Flow contract validation summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowContractValidation {
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<FlowContractCheck>,
}

impl Default for FlowContractValidation {
    fn default() -> Self {
        Self {
            passed: true,
            checks: Vec::new(),
        }
    }
}

/// Aggregated primitive references for one Flow.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowPrimitiveRefs {
    /// Referenced runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub run_ids: Vec<String>,
    /// Referenced session tasks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_ids: Vec<String>,
    /// Referenced agents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_ids: Vec<String>,
    /// Referenced approvals.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approval_ids: Vec<String>,
    /// Referenced structured user questions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub question_ids: Vec<String>,
    /// Referenced schedules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedule_ids: Vec<String>,
    /// Referenced output records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_ids: Vec<String>,
}

/// Durable Flow record. Lifecycle status is intentionally derived on read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FlowRecord {
    /// Stable Flow identifier.
    pub flow_id: String,
    /// Optional caller idempotency key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Immutable Playbook version reference.
    pub playbook_ref: PlaybookVersionRef,
    /// Target session.
    pub session_id: String,
    /// Digest of the original SubmitInputRequest used to start the Flow.
    pub input_digest: String,
    /// Daemon-only nonce used to prove run correlation came from Flow start.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub correlation_nonce: String,
    /// Referenced run once scheduling succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Creation timestamp.
    pub created_at_ms: u64,
    /// Last projection update timestamp.
    pub updated_at_ms: u64,
    /// First terminal projection boundary. Later child-session activity is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    /// Cancellation marker used only when no run was scheduled yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_at_ms: Option<u64>,
    /// Optional caller metadata for this Flow record.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
    /// Evidence collected for this Flow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<FlowEvidenceRef>,
}

/// Flow view returned by APIs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FlowView {
    /// Stable Flow identifier.
    pub flow_id: String,
    /// Derived lifecycle status.
    pub status: FlowStatus,
    /// Immutable Playbook version reference.
    pub playbook_ref: PlaybookVersionRef,
    /// Target session.
    pub session_id: String,
    /// Referenced run once scheduling succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Aggregated daemon primitive references.
    pub primitive_refs: FlowPrimitiveRefs,
    /// Derived phase gate states from the Playbook manifest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phase_states: Vec<FlowPhaseState>,
    /// Generic contract validation summary for phases, evidence, and tool policy.
    #[serde(default)]
    pub contract: FlowContractValidation,
    /// Full run view when the referenced run is currently available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunView>,
    /// Thin run-stream proxy path when a run exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_stream_url: Option<String>,
    /// Creation timestamp.
    pub created_at_ms: u64,
    /// Last update timestamp.
    pub updated_at_ms: u64,
    /// First terminal projection boundary, when sealed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    /// Optional caller metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
    /// Evidence collected for this Flow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<FlowEvidenceRef>,
}

/// Product-view Flow verifier request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductViewFlowVerificationRequest {
    /// Workspace-relative report path that must exist.
    pub report_path: String,
    /// Required report section labels. Defaults are applied when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_sections: Vec<String>,
    /// Tool names that must not appear in scoped session journals.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbidden_tools: Vec<String>,
}

/// Append evidence references to one Flow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppendFlowEvidenceRequest {
    /// Evidence references to append. Existing `kind + id` pairs are idempotent.
    pub evidence_refs: Vec<FlowEvidenceRef>,
}

/// One verifier check result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowVerificationCheck {
    pub name: String,
    pub passed: bool,
    pub details: String,
}

/// Product-view verifier verdict based on daemon/workspace evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProductViewFlowVerificationVerdict {
    pub flow_id: String,
    pub passed: bool,
    pub checks: Vec<FlowVerificationCheck>,
    pub evidence: Value,
}

/// Start or idempotently recover one Flow by scheduling a normal session run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StartFlowRequest {
    /// Optional caller-provided Flow id. Supplying this makes start idempotent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_id: Option<String>,
    /// Optional caller idempotency key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Immutable Playbook version reference.
    pub playbook_ref: PlaybookVersionRef,
    /// Target session id.
    pub session_id: String,
    /// Normal Kheish input request used for the underlying run.
    pub request: crate::SubmitInputRequest,
    /// Optional caller metadata for the Flow record.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
    /// Optional evidence refs known at start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<FlowEvidenceRef>,
}

/// Flow list query.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowListQuery {
    /// Restricts results to one Playbook.
    pub playbook_id: Option<String>,
    /// Restricts results to one session.
    pub session_id: Option<String>,
    /// Restricts results to one derived status.
    pub status: Option<FlowStatus>,
}

/// Playbook list query.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookListQuery {
    /// Filters by Playbook id, title, objective, or digest substring.
    pub query: Option<String>,
    /// Restricts results to records with at least one version in this release state.
    pub status: Option<PlaybookReleaseStatus>,
}

/// Result of reserving a Flow start.
#[derive(Clone, Debug)]
pub(crate) struct FlowStartReservation {
    pub record: FlowRecord,
    pub should_submit_run: bool,
}

/// Filesystem-backed Playbook/Flow store.
#[derive(Clone, Debug)]
pub(crate) struct FilePlaybookStore {
    root: PathBuf,
}

impl FilePlaybookStore {
    /// Creates a new store rooted under one daemon state directory.
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn playbooks_path(&self) -> PathBuf {
        self.root.join("playbooks.json")
    }

    fn flows_path(&self) -> PathBuf {
        self.root.join("flows.json")
    }

    /// Loads all Playbook records.
    pub(crate) fn load_playbooks(&self) -> Result<BTreeMap<String, PlaybookRecord>> {
        read_map_or_default(&self.playbooks_path(), "playbook catalog")
    }

    /// Loads all Flow records.
    pub(crate) fn load_flows(&self) -> Result<BTreeMap<String, FlowRecord>> {
        read_map_or_default(&self.flows_path(), "flow catalog")
    }

    /// Saves all Playbook records atomically.
    pub(crate) fn save_playbooks(
        &self,
        playbooks: &BTreeMap<String, PlaybookRecord>,
    ) -> Result<()> {
        write_json_pretty_atomically(&self.playbooks_path(), playbooks)
    }

    /// Saves all Flow records atomically.
    pub(crate) fn save_flows(&self, flows: &BTreeMap<String, FlowRecord>) -> Result<()> {
        write_json_pretty_atomically(&self.flows_path(), flows)
    }

    /// Returns the next numeric Flow id seed.
    pub(crate) fn next_flow_seed(&self) -> u64 {
        self.load_flows()
            .map(|flows| next_seed(flows.keys().map(String::as_str), "flow-"))
            .unwrap_or(1)
    }
}

fn read_map_or_default<T>(path: &Path, label: &'static str) -> Result<BTreeMap<String, T>>
where
    T: for<'de> Deserialize<'de>,
{
    Ok(read_json_or_quarantine(path, label)?.unwrap_or_default())
}

fn next_seed<'a>(ids: impl Iterator<Item = &'a str>, prefix: &str) -> u64 {
    ids.filter_map(|id| id.strip_prefix(prefix))
        .filter_map(|suffix| suffix.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        + 1
}

/// Computes the digest of an immutable Playbook manifest.
pub(crate) fn playbook_manifest_digest(manifest: &PlaybookManifest) -> Result<String> {
    let payload = serde_json::to_vec(manifest)?;
    Ok(hex::encode(Sha256::digest(payload)))
}

/// Computes a digest of the normal Kheish input request used to start one Flow.
pub(crate) fn flow_input_digest(request: &crate::SubmitInputRequest) -> Result<String> {
    let payload = serde_json::to_vec(request)?;
    Ok(hex::encode(Sha256::digest(payload)))
}

/// Builds the daemon-owned run metadata object used for Flow correlation.
pub(crate) fn flow_correlation_metadata(record: &FlowRecord) -> Value {
    json!({
        "flow_id": record.flow_id,
        "playbook_id": record.playbook_ref.playbook_id,
        "version": record.playbook_ref.version,
        "digest": record.playbook_ref.digest,
        "nonce": record.correlation_nonce,
    })
}

/// Merges one daemon-owned metadata entry into a normal request metadata object.
pub(crate) fn insert_daemon_metadata(
    metadata: &mut Option<Value>,
    key: &str,
    value: Value,
) -> Result<()> {
    match metadata.take().unwrap_or(Value::Null) {
        Value::Null => {
            *metadata = Some(json!({ key: value }));
            Ok(())
        }
        Value::Object(mut map) => {
            if map.contains_key(key) {
                bail!("metadata key `{key}` is daemon-owned");
            }
            map.insert(key.to_string(), value);
            *metadata = Some(Value::Object(map));
            Ok(())
        }
        other => {
            *metadata = Some(other);
            bail!("metadata must be an object when daemon metadata is attached")
        }
    }
}

/// Returns whether caller metadata attempts to set daemon-owned Flow correlation.
pub(crate) fn contains_flow_metadata(metadata: &Option<Value>) -> bool {
    metadata
        .as_ref()
        .and_then(Value::as_object)
        .is_some_and(|map| map.contains_key(KHEISH_FLOW_METADATA_KEY))
}

/// Extracts a Flow id from run metadata.
pub(crate) fn flow_id_from_run_metadata(run: &RunView) -> Option<&str> {
    run.input_metadata
        .as_ref()
        .and_then(|metadata| metadata.get(KHEISH_FLOW_METADATA_KEY))
        .and_then(|value| value.get("flow_id"))
        .and_then(Value::as_str)
}

/// Returns whether a run carries daemon-owned metadata for a specific Flow record.
pub(crate) fn run_matches_flow_record(run: &RunView, record: &FlowRecord) -> bool {
    if !matches!(
        run.kind,
        DaemonRunKind::Input | DaemonRunKind::ScheduledInput
    ) {
        return false;
    }
    if flow_id_from_run_metadata(run) != Some(record.flow_id.as_str()) {
        return false;
    }
    let Some(metadata) = run
        .input_metadata
        .as_ref()
        .and_then(|metadata| metadata.get(KHEISH_FLOW_METADATA_KEY))
    else {
        return false;
    };
    let fields_match = metadata.get("playbook_id").and_then(Value::as_str)
        == Some(record.playbook_ref.playbook_id.as_str())
        && metadata.get("version").and_then(Value::as_str)
            == Some(record.playbook_ref.version.as_str())
        && metadata.get("digest").and_then(Value::as_str)
            == Some(record.playbook_ref.digest.as_str());
    if !fields_match {
        return false;
    }
    record.correlation_nonce.is_empty()
        || metadata.get("nonce").and_then(Value::as_str) == Some(record.correlation_nonce.as_str())
}

/// Validates a Playbook manifest structurally.
pub(crate) fn validate_playbook_manifest(manifest: &PlaybookManifest) -> PlaybookValidationResult {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    validate_identifier("playbook_id", &manifest.playbook_id, &mut errors);
    validate_identifier("version", &manifest.version, &mut errors);
    require_text("title", &manifest.title, &mut errors);
    require_text("objective", &manifest.objective, &mut errors);
    validate_unique_ids(
        "input name",
        manifest.inputs.iter().map(|input| input.name.as_str()),
        &mut errors,
    );
    validate_unique_ids(
        "role_id",
        manifest.roles.iter().map(|role| role.role_id.as_str()),
        &mut errors,
    );
    validate_unique_ids(
        "phase_id",
        manifest.phases.iter().map(|phase| phase.phase_id.as_str()),
        &mut errors,
    );
    for phase in &manifest.phases {
        validate_identifier("phase_id", &phase.phase_id, &mut errors);
        require_text("phase objective", &phase.objective, &mut errors);
        validate_evidence_requirements(
            &format!("phase {}", phase.phase_id),
            &phase.required_evidence,
            &mut errors,
        );
    }
    validate_evidence_requirements("manifest", &manifest.required_evidence, &mut errors);
    validate_tool_policy(&manifest.tools, &mut errors);
    if manifest.phases.is_empty() {
        warnings.push("manifest has no phases".to_string());
    }
    if manifest.acceptance_criteria.is_empty() {
        warnings.push("manifest has no top-level acceptance criteria".to_string());
    }
    let digest = playbook_manifest_digest(manifest).ok();
    PlaybookValidationResult {
        valid: errors.is_empty(),
        digest,
        errors,
        warnings,
    }
}

fn validate_unique_ids<'a>(
    label: &str,
    values: impl Iterator<Item = &'a str>,
    errors: &mut Vec<String>,
) {
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        if value.trim().is_empty() {
            errors.push(format!("{label} is required"));
            continue;
        }
        if !seen.insert(value.to_string()) {
            errors.push(format!("duplicate {label} `{value}`"));
        }
    }
}

fn validate_evidence_requirements(
    label: &str,
    requirements: &[PlaybookEvidenceRequirement],
    errors: &mut Vec<String>,
) {
    let mut seen = std::collections::BTreeSet::new();
    for requirement in requirements {
        if requirement.kind.trim().is_empty() {
            errors.push(format!("{label} evidence kind is required"));
        }
        if requirement.id.trim().is_empty() {
            errors.push(format!("{label} evidence id is required"));
        }
        let key = (requirement.kind.clone(), requirement.id.clone());
        if !seen.insert(key) {
            errors.push(format!(
                "{label} has duplicate evidence requirement {}:{}",
                requirement.kind, requirement.id
            ));
        }
    }
}

fn validate_tool_policy(policy: &PlaybookToolPolicy, errors: &mut Vec<String>) {
    let allow = policy
        .allow
        .iter()
        .map(|tool| tool.trim())
        .filter(|tool| !tool.is_empty())
        .collect::<std::collections::BTreeSet<_>>();
    let deny = policy
        .deny
        .iter()
        .map(|tool| tool.trim())
        .filter(|tool| !tool.is_empty())
        .collect::<std::collections::BTreeSet<_>>();
    for tool in allow.intersection(&deny) {
        errors.push(format!("tool `{tool}` cannot be both allowed and denied"));
    }
    if policy.enforce && policy.allow.is_empty() && policy.deny.is_empty() {
        errors.push("enforced tool policy requires allow or deny entries".to_string());
    }
}

fn validate_identifier(field: &str, value: &str, errors: &mut Vec<String>) {
    if let Err(error) = ensure_control_identifier(field, value) {
        errors.push(error.to_string());
    }
}

pub(crate) fn ensure_control_identifier(field: &str, value: &str) -> Result<()> {
    if value.chars().any(char::is_whitespace) {
        bail!("{field} must not contain whitespace");
    }
    if value.contains('/') || value.contains('\\') {
        bail!("{field} must not contain path separators");
    }
    if value.trim().is_empty() {
        bail!("{field} is required");
    }
    Ok(())
}

fn require_text(field: &str, value: &str, errors: &mut Vec<String>) {
    if value.trim().is_empty() {
        errors.push(format!("{field} is required"));
    }
}

pub(crate) fn record_to_view(record: &PlaybookRecord, selected: Option<&str>) -> PlaybookView {
    let mut versions = record
        .versions
        .values()
        .map(|version| {
            let release = record.releases.get(&version.version);
            PlaybookVersionSummary {
                version: version.version.clone(),
                digest: version.digest.clone(),
                status: release
                    .map(|release| release.status.clone())
                    .unwrap_or_default(),
                created_at_ms: version.created_at_ms,
                updated_at_ms: release
                    .map(|release| release.updated_at_ms)
                    .unwrap_or(version.created_at_ms),
                evidence_refs: release
                    .map(|release| release.evidence_refs.clone())
                    .unwrap_or_default(),
                revoked_reason: release.and_then(|release| release.revoked_reason.clone()),
            }
        })
        .collect::<Vec<_>>();
    versions.sort_by(|left, right| left.version.cmp(&right.version));
    let latest_version = record
        .versions
        .values()
        .max_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.version.cmp(&right.version))
        })
        .map(|version| version.version.clone());
    let active_version = versions
        .iter()
        .find(|version| version.status == PlaybookReleaseStatus::Active)
        .map(|version| version.version.clone());
    let selected_version = selected
        .and_then(|version| record.versions.get(version))
        .cloned()
        .or_else(|| {
            latest_version
                .as_ref()
                .and_then(|version| record.versions.get(version))
                .cloned()
        });
    PlaybookView {
        playbook_id: record.playbook_id.clone(),
        latest_version,
        active_version,
        versions,
        selected_version,
    }
}

pub(crate) fn require_matching_version<'a>(
    record: &'a PlaybookRecord,
    playbook_id: &str,
    version: &str,
    digest: &str,
) -> Result<&'a PlaybookVersionRecord> {
    let version_record = record
        .versions
        .get(version)
        .ok_or_else(|| anyhow!("unknown playbook version {playbook_id}@{version}"))?;
    if version_record.digest != digest {
        bail!(
            "digest mismatch for playbook {playbook_id}@{version}: expected {}, got {digest}",
            version_record.digest
        );
    }
    Ok(version_record)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::Result;
    use serde_json::{Value, json};
    use tempfile::tempdir;

    use super::*;
    use crate::runs::{DaemonRunStatus, RunRequestSummary, now_ms};

    #[test]
    fn file_playbook_store_quarantines_corrupt_catalogs() -> Result<()> {
        let root = tempdir()?;
        let store = FilePlaybookStore::new(root.path());
        fs::write(root.path().join("playbooks.json"), b"{not-json")?;
        fs::write(root.path().join("flows.json"), b"{not-json")?;

        assert!(store.load_playbooks()?.is_empty());
        assert!(store.load_flows()?.is_empty());

        let sibling_names = fs::read_dir(root.path())?
            .map(|entry| {
                entry
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .map_err(Into::into)
            })
            .collect::<Result<Vec<_>>>()?;
        assert!(
            sibling_names
                .iter()
                .any(|name| name.starts_with("playbooks.json.corrupt-")),
            "missing quarantined playbooks catalog: {sibling_names:?}"
        );
        assert!(
            sibling_names
                .iter()
                .any(|name| name.starts_with("flows.json.corrupt-")),
            "missing quarantined flows catalog: {sibling_names:?}"
        );
        Ok(())
    }

    #[test]
    fn run_matching_requires_daemon_nonce_and_flow_capable_run_kind() {
        let record = FlowRecord {
            flow_id: "flow-1".to_string(),
            idempotency_key: Some("flow-1".to_string()),
            playbook_ref: PlaybookVersionRef {
                playbook_id: "ops".to_string(),
                version: "1".to_string(),
                digest: "digest-1".to_string(),
            },
            session_id: "session-1".to_string(),
            input_digest: "input-digest".to_string(),
            correlation_nonce: "nonce-1".to_string(),
            run_id: None,
            created_at_ms: now_ms(),
            updated_at_ms: now_ms(),
            completed_at_ms: None,
            cancelled_at_ms: None,
            metadata: Value::Null,
            evidence_refs: Vec::new(),
        };
        let mut run = matching_run(&record, DaemonRunKind::Input, "nonce-1");
        assert!(run_matches_flow_record(&run, &record));

        run.kind = DaemonRunKind::ScheduledInput;
        assert!(run_matches_flow_record(&run, &record));

        run.kind = DaemonRunKind::MailboxDelivery;
        assert!(!run_matches_flow_record(&run, &record));

        let run = matching_run(&record, DaemonRunKind::Input, "forged");
        assert!(!run_matches_flow_record(&run, &record));
    }

    fn matching_run(record: &FlowRecord, kind: DaemonRunKind, nonce: &str) -> RunView {
        RunView {
            run_id: "run-1".to_string(),
            session_id: record.session_id.clone(),
            agent_id: "agent-1".to_string(),
            kind,
            status: DaemonRunStatus::Completed,
            submitted_at_ms: now_ms(),
            updated_at_ms: now_ms(),
            started_at_ms: None,
            finished_at_ms: None,
            queued_position: None,
            request: RunRequestSummary {
                source_plugin: "daemon".to_string(),
                source_kind: "api".to_string(),
                actor_id: "operator".to_string(),
                text_preview: Some("run".to_string()),
                provider: None,
                model: None,
                approval_count: None,
                question_count: None,
            },
            input_attachments: Vec::new(),
            input_metadata: Some(json!({
                KHEISH_FLOW_METADATA_KEY: {
                    "flow_id": record.flow_id,
                    "playbook_id": record.playbook_ref.playbook_id,
                    "version": record.playbook_ref.version,
                    "digest": record.playbook_ref.digest,
                    "nonce": nonce,
                }
            })),
            pending_approval_ids: Vec::new(),
            pending_approvals: Vec::new(),
            pending_question_ids: Vec::new(),
            pending_questions: Vec::new(),
            outputs: Vec::new(),
            deliveries: Vec::new(),
            error: None,
        }
    }
}
