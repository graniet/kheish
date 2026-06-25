//! Durable daemon-owned derivation records and profile definitions.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Result, bail};
use kheish_session::{
    decode_safe_storage_name, prepare_storage_path_for_write, write_json_pretty_atomically,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::state_files::read_json_or_quarantine;

/// The deterministic derivation profile executed by the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivationProfile {
    /// Materializes the canonical plain-text representation of one subject.
    CanonicalText,
    /// Materializes one visual preview image for subjects that expose one.
    VisualPreview,
}

impl DerivationProfile {
    /// Returns the stable profile identifier used in API payloads and cache keys.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CanonicalText => "canonical_text",
            Self::VisualPreview => "visual_preview",
        }
    }

    /// Returns the deterministic profile implementation version used in cache keys.
    pub(crate) fn version(&self) -> u32 {
        match self {
            Self::CanonicalText | Self::VisualPreview => 1,
        }
    }
}

impl FromStr for DerivationProfile {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "canonical_text" | "canonical-text" => Ok(Self::CanonicalText),
            "visual_preview" | "visual-preview" => Ok(Self::VisualPreview),
            other => bail!("unsupported derivation profile '{other}'"),
        }
    }
}

/// One daemon-owned subject that can be derived into a durable artifact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DerivationSubject {
    /// Derives from one daemon-owned asset.
    Asset { asset_id: String },
    /// Derives from one daemon-owned observation record.
    Observation { observation_id: String },
    /// Derives from one persisted `InputReceived` event referenced by journal offset.
    SessionInput { session_id: String, offset: u64 },
}

impl DerivationSubject {
    /// Returns one stable cache key fragment for the subject.
    pub(crate) fn stable_key(&self) -> String {
        match self {
            Self::Asset { asset_id } => format!("asset:{asset_id}"),
            Self::Observation { observation_id } => format!("observation:{observation_id}"),
            Self::SessionInput { session_id, offset } => {
                format!("session_input:{session_id}:{offset}")
            }
        }
    }

    /// Validates that the subject contains the required identifiers.
    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Self::Asset { asset_id } => {
                anyhow::ensure!(!asset_id.trim().is_empty(), "asset_id is required");
            }
            Self::Observation { observation_id } => {
                anyhow::ensure!(
                    !observation_id.trim().is_empty(),
                    "observation_id is required"
                );
            }
            Self::SessionInput { session_id, offset } => {
                anyhow::ensure!(!session_id.trim().is_empty(), "session_id is required");
                let _ = offset;
            }
        }
        Ok(())
    }
}

/// The API payload used to create or fetch one derivation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivationCreateRequest {
    /// The deterministic derivation profile that should run.
    pub profile: DerivationProfile,
    /// The subject that should be transformed into a durable artifact.
    pub subject: DerivationSubject,
    /// Optional speech-to-text controls for audio-backed canonical text derivations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcription: Option<DerivationTranscriptionOptions>,
}

/// Request-scoped cache controls for derivation creation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DerivationCreateControls {
    /// Recompute even when a completed or failed derivation already exists for the cache key.
    pub force_refresh: bool,
    /// Recompute only when the existing derivation for the cache key is failed.
    pub retry_failed: bool,
}

impl DerivationCreateRequest {
    /// Validates the request payload before execution.
    pub(crate) fn validate(&self) -> Result<()> {
        self.subject.validate()
    }

    /// Returns normalized speech-to-text controls after validating profile-level compatibility.
    pub(crate) fn normalized_transcription_options(
        &self,
    ) -> Result<Option<NormalizedDerivationTranscriptionOptions>> {
        let Some(options) = self.transcription.as_ref() else {
            return Ok(None);
        };
        anyhow::ensure!(
            self.profile == DerivationProfile::CanonicalText,
            "transcription options are only supported for canonical_text derivations"
        );
        let normalized = options.normalized()?;
        Ok((!normalized.is_empty()).then_some(normalized))
    }
}

/// Optional speech-to-text controls for audio-backed canonical text derivations.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivationTranscriptionOptions {
    /// Optional provider prompt used to guide audio transcription.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Optional BCP-47-style language hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Reserved for future structured timestamp output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timestamp_granularities: Vec<String>,
    /// Requests provider speaker diarization when the selected transcription backend supports it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub diarization: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl DerivationTranscriptionOptions {
    fn normalized(&self) -> Result<NormalizedDerivationTranscriptionOptions> {
        let (prompt, language) = crate::transcription::validate_transcription_hints(
            self.prompt.as_deref(),
            self.language.as_deref(),
        )?;
        let mut timestamp_granularities = Vec::new();
        for granularity in &self.timestamp_granularities {
            let normalized = granularity.trim().to_ascii_lowercase();
            anyhow::ensure!(
                normalized == "word" || normalized == "segment",
                "transcription timestamp granularity must be `word` or `segment`"
            );
            anyhow::ensure!(
                !timestamp_granularities.contains(&normalized),
                "transcription timestamp granularities must be unique"
            );
            timestamp_granularities.push(normalized);
        }
        timestamp_granularities.sort();
        anyhow::ensure!(
            !(self.diarization && !timestamp_granularities.is_empty()),
            "transcription diarization does not support timestamp granularities"
        );
        anyhow::ensure!(
            !(self.diarization && prompt.is_some()),
            "transcription diarization does not support prompts"
        );
        Ok(NormalizedDerivationTranscriptionOptions {
            prompt,
            language,
            timestamp_granularities,
            diarization: self.diarization,
        })
    }
}

/// Normalized speech-to-text controls used internally for execution and cache identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NormalizedDerivationTranscriptionOptions {
    pub(crate) prompt: Option<String>,
    pub(crate) language: Option<String>,
    pub(crate) timestamp_granularities: Vec<String>,
    pub(crate) diarization: bool,
}

impl NormalizedDerivationTranscriptionOptions {
    pub(crate) fn is_empty(&self) -> bool {
        self.prompt.is_none()
            && self.language.is_none()
            && self.timestamp_granularities.is_empty()
            && !self.diarization
    }

    pub(crate) fn prompt(&self) -> Option<&str> {
        self.prompt.as_deref()
    }

    pub(crate) fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }

    pub(crate) fn timestamp_granularities(&self) -> &[String] {
        &self.timestamp_granularities
    }

    pub(crate) fn diarization(&self) -> bool {
        self.diarization
    }

    pub(crate) fn cache_digest_with_backend(
        &self,
        route_id: &str,
        provider: &str,
        model: &str,
    ) -> String {
        let prompt_sha256 = self
            .prompt
            .as_deref()
            .map(|value| hex::encode(Sha256::digest(value.as_bytes())));
        let payload = serde_json::json!({
            "version": 1,
            "route_id": route_id,
            "provider": provider,
            "model": model,
            "prompt_sha256": prompt_sha256,
            "prompt_chars": self
                .prompt
                .as_deref()
                .map(|value| value.chars().count())
                .unwrap_or(0),
            "language": self.language.as_deref(),
            "timestamp_granularities": self.timestamp_granularities,
            "diarization": self.diarization,
        });
        hex::encode(Sha256::digest(
            serde_json::to_vec(&payload)
                .expect("normalized transcription cache payload should serialize"),
        ))
    }
}

/// Provider/backend metadata captured for derivations that use an external backend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivationBackendProvenance {
    /// Backend class used by the derivation.
    pub kind: String,
    /// Daemon route/backend identifier selected for the backend call.
    pub route_id: String,
    /// Provider reported by the backend response.
    pub provider: String,
    /// Model reported by the backend response.
    pub model: String,
    /// Version of the daemon transcription pipeline used for this backend result.
    #[serde(default = "default_transcription_pipeline_version")]
    pub pipeline_version: u32,
    /// Strategy used to produce the transcription text from one or more audio parts.
    #[serde(default = "default_transcription_stitching_strategy")]
    pub stitching_strategy: String,
    /// Number of audio parts consumed by the transcription pipeline.
    #[serde(default = "default_transcription_part_count")]
    pub part_count: u32,
    /// Optional daemon asset containing structured transcription timestamps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_asset_id: Option<String>,
}

fn default_transcription_pipeline_version() -> u32 {
    crate::transcription::TRANSCRIPTION_PIPELINE_VERSION
}

fn default_transcription_stitching_strategy() -> String {
    crate::transcription::TRANSCRIPTION_STITCHING_STRATEGY_SINGLE_PART.to_string()
}

fn default_transcription_part_count() -> u32 {
    crate::transcription::TRANSCRIPTION_SINGLE_PART_COUNT
}

/// The durable lifecycle status for one derivation attempt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivationStatus {
    /// The profile completed and produced or reused a result asset.
    #[default]
    Completed,
    /// The profile failed after the subject and cache key were resolved.
    Failed,
}

impl DerivationStatus {
    /// Returns the stable status string used in API filters and docs.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

/// Whether one create request reused an existing derivation or created a new one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivationCacheStatus {
    /// The create request returned an existing durable derivation record.
    Hit,
    /// The create request created a new durable derivation record.
    Miss,
}

fn default_profile_version() -> u32 {
    1
}

/// The externally visible derivation record returned by list and detail APIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivationView {
    /// The stable daemon-owned derivation identifier.
    pub derivation_id: String,
    /// The deterministic derivation profile that produced the result.
    pub profile: DerivationProfile,
    /// The deterministic profile implementation version included in the cache key.
    #[serde(default = "default_profile_version")]
    pub profile_version: u32,
    /// The source subject used to build the derived artifact.
    pub subject: DerivationSubject,
    /// The stable source fingerprint used for cache invalidation.
    pub source_fingerprint: String,
    /// The durable derivation status.
    #[serde(default)]
    pub status: DerivationStatus,
    /// The stable daemon-owned result asset identifier.
    #[serde(default)]
    pub result_asset_id: String,
    /// Whether the result reuses the original subject asset instead of producing a new one.
    #[serde(default)]
    pub reused_subject_asset: bool,
    /// The terminal error for failed derivations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Provider/backend metadata when the derivation used an external backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<DerivationBackendProvenance>,
    /// Request-scoped cache outcome, present on create responses and omitted from persisted views.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_status: Option<DerivationCacheStatus>,
    /// The creation timestamp in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
}

impl DerivationView {
    /// Returns this view with one request-scoped cache status.
    pub(crate) fn with_cache_status(mut self, cache_status: DerivationCacheStatus) -> Self {
        self.cache_status = Some(cache_status);
        self
    }
}

/// One persisted derivation record stored under the daemon state root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredDerivationRecord {
    /// The stable daemon-owned derivation identifier.
    pub(crate) derivation_id: String,
    /// The deterministic derivation profile that produced the result.
    pub(crate) profile: DerivationProfile,
    /// The deterministic profile implementation version included in the cache key.
    #[serde(default = "default_profile_version")]
    pub(crate) profile_version: u32,
    /// The source subject used to build the derived artifact.
    pub(crate) subject: DerivationSubject,
    /// One stable fingerprint of the source input used for cache invalidation.
    pub(crate) source_fingerprint: String,
    /// The durable derivation status.
    #[serde(default)]
    pub(crate) status: DerivationStatus,
    /// The stable daemon-owned result asset identifier.
    #[serde(default)]
    pub(crate) result_asset_id: String,
    /// Whether the result reuses the original subject asset instead of producing a new one.
    #[serde(default)]
    pub(crate) reused_subject_asset: bool,
    /// The terminal error for failed derivations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    /// Provider/backend metadata when the derivation used an external backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) backend: Option<DerivationBackendProvenance>,
    /// The creation timestamp in milliseconds since the Unix epoch.
    pub(crate) created_at_ms: u64,
}

impl StoredDerivationRecord {
    /// Returns the stable cache key used for deduplication.
    pub(crate) fn cache_key(&self) -> String {
        derivation_cache_key(
            &self.profile,
            self.profile_version,
            &self.subject,
            &self.source_fingerprint,
        )
    }
}

/// Builds the stable cache key used for deduplication.
pub(crate) fn derivation_cache_key(
    profile: &DerivationProfile,
    profile_version: u32,
    subject: &DerivationSubject,
    source_fingerprint: &str,
) -> String {
    format!(
        "{}@v{}:{}:{}",
        profile.as_str(),
        profile_version,
        subject.stable_key(),
        source_fingerprint
    )
}

impl From<&StoredDerivationRecord> for DerivationView {
    fn from(value: &StoredDerivationRecord) -> Self {
        Self {
            derivation_id: value.derivation_id.clone(),
            profile: value.profile.clone(),
            profile_version: value.profile_version,
            subject: value.subject.clone(),
            source_fingerprint: value.source_fingerprint.clone(),
            status: value.status.clone(),
            result_asset_id: value.result_asset_id.clone(),
            reused_subject_asset: value.reused_subject_asset,
            error: value.error.clone(),
            backend: value.backend.clone(),
            cache_status: None,
            created_at_ms: value.created_at_ms,
        }
    }
}

impl From<StoredDerivationRecord> for DerivationView {
    fn from(value: StoredDerivationRecord) -> Self {
        Self::from(&value)
    }
}

/// Filesystem-backed derivation storage rooted under one daemon state directory.
#[derive(Clone, Debug)]
pub(crate) struct FileDerivationStore {
    root: PathBuf,
}

impl FileDerivationStore {
    /// Creates a new derivation store rooted under one daemon state directory.
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn derivations_root(&self) -> PathBuf {
        self.root.join("derivations")
    }

    #[cfg(test)]
    fn derivation_path(&self, derivation_id: &str) -> PathBuf {
        kheish_session::resolve_storage_path_for_read(
            &self.derivations_root(),
            derivation_id,
            "json",
        )
    }

    /// Loads every persisted derivation record, quarantining corrupted files.
    pub(crate) fn load_derivations(&self) -> Result<BTreeMap<String, StoredDerivationRecord>> {
        let root = self.derivations_root();
        if !root.exists() {
            return Ok(BTreeMap::new());
        }
        let mut records = BTreeMap::new();
        for scan_root in derivation_storage_scan_roots(&root) {
            if !scan_root.exists() {
                continue;
            }
            for entry in fs::read_dir(scan_root)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                let Some(record) =
                    read_json_or_quarantine::<StoredDerivationRecord>(&path, "derivation record")?
                else {
                    continue;
                };
                records.insert(record.derivation_id.clone(), record);
            }
        }
        Ok(records)
    }

    /// Repairs loaded derivations whose terminal state no longer matches available assets.
    pub(crate) fn repair_loaded_derivations(
        &self,
        records: &mut BTreeMap<String, StoredDerivationRecord>,
        asset_exists: impl Fn(&str) -> bool,
    ) -> Result<usize> {
        let mut repaired = 0usize;
        for record in records.values_mut() {
            if record.status != DerivationStatus::Completed {
                continue;
            }
            let result_asset_id = record.result_asset_id.trim();
            let detail = if result_asset_id.is_empty() {
                "completed derivation is missing a result asset id after daemon startup".to_string()
            } else if !asset_exists(result_asset_id) {
                format!(
                    "completed derivation result asset {result_asset_id} is unavailable after daemon startup"
                )
            } else if let Some(timestamp_asset_id) = record
                .backend
                .as_ref()
                .and_then(|backend| backend.timestamp_asset_id.as_deref())
                && !asset_exists(timestamp_asset_id)
            {
                format!(
                    "completed derivation timestamp asset {timestamp_asset_id} is unavailable after daemon startup"
                )
            } else {
                continue;
            };
            record.status = DerivationStatus::Failed;
            record.result_asset_id = String::new();
            record.reused_subject_asset = false;
            record.error = Some(detail);
            self.save_derivation(record)?;
            repaired += 1;
        }
        Ok(repaired)
    }

    /// Persists one derivation record atomically.
    pub(crate) fn save_derivation(&self, record: &StoredDerivationRecord) -> Result<()> {
        let path = prepare_storage_path_for_write(
            &self.derivations_root(),
            &record.derivation_id,
            "json",
        )?;
        write_json_pretty_atomically(&path, record)
    }

    /// Returns the next numeric derivation identifier seed.
    pub(crate) fn next_seed(&self) -> u64 {
        self.derivation_file_paths()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|path| self.derivation_id_from_path(&path))
            .filter_map(|id| {
                id.strip_prefix("derivation-")
                    .and_then(|value| value.parse().ok())
            })
            .max()
            .unwrap_or(0u64)
            .saturating_add(1)
    }
}

impl FileDerivationStore {
    fn derivation_file_paths(&self) -> Result<Vec<PathBuf>> {
        let root = self.derivations_root();
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        for scan_root in derivation_storage_scan_roots(&root) {
            if !scan_root.exists() {
                continue;
            }
            for entry in fs::read_dir(scan_root)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() {
                    paths.push(path);
                }
            }
        }
        Ok(paths)
    }

    fn derivation_id_from_path(&self, path: &std::path::Path) -> Option<String> {
        let file_name = path.file_name()?.to_str()?;
        let base_name = file_name.split(".corrupt-").next().unwrap_or(file_name);
        let stem = base_name.strip_suffix(".json")?;
        decode_safe_storage_name(stem).or_else(|| Some(stem.to_string()))
    }
}

fn derivation_storage_scan_roots(root: &std::path::Path) -> [PathBuf; 2] {
    [root.to_path_buf(), root.join("__safe")]
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::*;

    #[test]
    fn derivation_profile_accepts_cli_friendly_aliases() -> Result<()> {
        assert_eq!(
            DerivationProfile::from_str("canonical-text")?,
            DerivationProfile::CanonicalText
        );
        assert_eq!(
            DerivationProfile::from_str("visual_preview")?,
            DerivationProfile::VisualPreview
        );
        Ok(())
    }

    #[test]
    fn derivation_store_next_seed_preserves_quarantined_highest_derivation_id() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileDerivationStore::new(root.path());
        fs::create_dir_all(store.derivations_root())?;
        fs::write(store.derivations_root().join("derivation-1.json"), "{}")?;
        fs::write(store.derivations_root().join("derivation-2.json"), "{}")?;

        let derivation_two = store.derivations_root().join("derivation-2.json");
        let quarantined = derivation_two.with_file_name(format!(
            "{}.corrupt-test",
            derivation_two
                .file_name()
                .expect("derivation-2 file")
                .to_string_lossy()
        ));
        fs::rename(&derivation_two, &quarantined)?;

        assert_eq!(store.next_seed(), 3);
        Ok(())
    }

    #[test]
    fn derivation_store_writes_hostile_ids_in_safe_namespace() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileDerivationStore::new(root.path());
        let record = StoredDerivationRecord {
            derivation_id: "../outside".to_string(),
            profile: DerivationProfile::CanonicalText,
            profile_version: 1,
            subject: DerivationSubject::Asset {
                asset_id: "asset-source".to_string(),
            },
            source_fingerprint: "source".to_string(),
            status: DerivationStatus::Failed,
            result_asset_id: String::new(),
            reused_subject_asset: false,
            error: Some("test failure".to_string()),
            backend: None,
            created_at_ms: 1,
        };

        store.save_derivation(&record)?;

        assert!(
            !root.path().join("outside.json").exists(),
            "hostile derivation id must not escape the derivations root"
        );
        let stored_path = store.derivation_path("../outside");
        assert!(stored_path.starts_with(store.derivations_root().join("__safe")));
        let loaded = store.load_derivations()?;
        assert_eq!(loaded.get("../outside"), Some(&record));
        Ok(())
    }

    #[test]
    fn derivation_subject_validation_rejects_empty_identifiers() {
        let error = DerivationSubject::Asset {
            asset_id: "   ".to_string(),
        }
        .validate()
        .expect_err("blank asset ids should be rejected");
        assert!(error.to_string().contains("asset_id is required"));
    }

    #[test]
    fn derivation_cache_key_includes_profile_version() {
        let subject = DerivationSubject::Asset {
            asset_id: "asset-1".to_string(),
        };
        let v1 = derivation_cache_key(
            &DerivationProfile::CanonicalText,
            1,
            &subject,
            "source-fingerprint",
        );
        let v2 = derivation_cache_key(
            &DerivationProfile::CanonicalText,
            2,
            &subject,
            "source-fingerprint",
        );

        assert_ne!(v1, v2);
        assert!(v1.starts_with("canonical_text@v1:asset:asset-1:"));
        assert!(v2.starts_with("canonical_text@v2:asset:asset-1:"));
    }

    #[test]
    fn derivation_backend_provenance_deserializes_legacy_transcription_records() -> Result<()> {
        let legacy = serde_json::json!({
            "kind": "transcription",
            "route_id": "openai",
            "provider": "openai",
            "model": "gpt-4o-transcribe",
            "timestamp_asset_id": "asset-timestamps"
        });

        let provenance = serde_json::from_value::<DerivationBackendProvenance>(legacy)?;

        assert_eq!(
            provenance.pipeline_version,
            crate::transcription::TRANSCRIPTION_PIPELINE_VERSION
        );
        assert_eq!(
            provenance.stitching_strategy,
            crate::transcription::TRANSCRIPTION_STITCHING_STRATEGY_SINGLE_PART
        );
        assert_eq!(
            provenance.part_count,
            crate::transcription::TRANSCRIPTION_SINGLE_PART_COUNT
        );
        assert_eq!(
            provenance.timestamp_asset_id.as_deref(),
            Some("asset-timestamps")
        );
        Ok(())
    }

    #[test]
    fn derivation_store_repairs_completed_records_with_missing_result_assets() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileDerivationStore::new(root.path());
        let subject = DerivationSubject::Asset {
            asset_id: "asset-source".to_string(),
        };
        let mut records = BTreeMap::new();
        records.insert(
            "derivation-1".to_string(),
            StoredDerivationRecord {
                derivation_id: "derivation-1".to_string(),
                profile: DerivationProfile::CanonicalText,
                profile_version: 1,
                subject: subject.clone(),
                source_fingerprint: "source".to_string(),
                status: DerivationStatus::Completed,
                result_asset_id: "asset-missing".to_string(),
                reused_subject_asset: false,
                error: None,
                backend: None,
                created_at_ms: 1,
            },
        );
        records.insert(
            "derivation-2".to_string(),
            StoredDerivationRecord {
                derivation_id: "derivation-2".to_string(),
                profile: DerivationProfile::CanonicalText,
                profile_version: 1,
                subject,
                source_fingerprint: "source-2".to_string(),
                status: DerivationStatus::Completed,
                result_asset_id: "asset-ok".to_string(),
                reused_subject_asset: false,
                error: None,
                backend: None,
                created_at_ms: 2,
            },
        );

        let repaired =
            store.repair_loaded_derivations(&mut records, |asset_id| asset_id == "asset-ok")?;

        assert_eq!(repaired, 1);
        let repaired_record = records.get("derivation-1").expect("repaired record");
        assert_eq!(repaired_record.status, DerivationStatus::Failed);
        assert_eq!(repaired_record.result_asset_id, "");
        assert!(
            repaired_record
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("asset-missing")
        );
        let persisted = store.load_derivations()?;
        assert_eq!(
            persisted
                .get("derivation-1")
                .expect("persisted repaired record")
                .status,
            DerivationStatus::Failed
        );
        assert_eq!(
            records.get("derivation-2").expect("valid record").status,
            DerivationStatus::Completed
        );
        Ok(())
    }

    #[test]
    fn derivation_store_repairs_completed_records_with_missing_timestamp_assets() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileDerivationStore::new(root.path());
        let mut records = BTreeMap::from([(
            "derivation-1".to_string(),
            StoredDerivationRecord {
                derivation_id: "derivation-1".to_string(),
                profile: DerivationProfile::CanonicalText,
                profile_version: 1,
                subject: DerivationSubject::Asset {
                    asset_id: "asset-source".to_string(),
                },
                source_fingerprint: "source".to_string(),
                status: DerivationStatus::Completed,
                result_asset_id: "asset-result".to_string(),
                reused_subject_asset: false,
                error: None,
                backend: Some(DerivationBackendProvenance {
                    kind: "transcription".to_string(),
                    route_id: "openai".to_string(),
                    provider: "openai".to_string(),
                    model: "gpt-4o-transcribe".to_string(),
                    pipeline_version: crate::transcription::TRANSCRIPTION_PIPELINE_VERSION,
                    stitching_strategy:
                        crate::transcription::TRANSCRIPTION_STITCHING_STRATEGY_SINGLE_PART
                            .to_string(),
                    part_count: crate::transcription::TRANSCRIPTION_SINGLE_PART_COUNT,
                    timestamp_asset_id: Some("asset-timestamps".to_string()),
                }),
                created_at_ms: 1,
            },
        )]);

        let repaired =
            store.repair_loaded_derivations(&mut records, |asset_id| asset_id == "asset-result")?;

        assert_eq!(repaired, 1);
        let repaired_record = records.get("derivation-1").expect("repaired record");
        assert_eq!(repaired_record.status, DerivationStatus::Failed);
        assert_eq!(repaired_record.result_asset_id, "");
        assert!(
            repaired_record
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("asset-timestamps")
        );
        Ok(())
    }
}
