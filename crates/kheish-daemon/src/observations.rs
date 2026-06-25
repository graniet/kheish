//! Durable daemon-owned observation sources, observations, and materialization requests.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use kheish_session::{
    append_json_line_sync, prepare_storage_path_for_write, write_json_pretty_atomically,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::state_files::read_json_or_quarantine;
use crate::{RunRequestSummary, SubmitInputRequest, summarize_input_request};

const DEFAULT_SOURCE_RETENTION_SECONDS: u64 = 7 * 24 * 60 * 60;
const DEFAULT_SOURCE_MAX_ACTIVE_OBSERVATIONS: u64 = 512;
const DEFAULT_SOURCE_MAX_ACTIVE_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_SOURCE_INGEST_RATE_LIMIT_WINDOW_MS: u64 = 60_000;
const DEFAULT_SOURCE_INGEST_RATE_LIMIT_BURST: u64 = 120;
const MAX_OBSERVATION_SOURCE_ID_BYTES: usize = 128;
const MAX_OBSERVATION_STREAM_ID_BYTES: usize = 128;
const MAX_OBSERVATION_IDEMPOTENCY_KEY_BYTES: usize = 256;

/// Validates one daemon-owned observation source identifier.
pub(crate) fn validate_observation_source_id(source_id: &str) -> Result<()> {
    validate_observation_identifier(
        "source_id",
        source_id,
        MAX_OBSERVATION_SOURCE_ID_BYTES,
        false,
    )
}

/// Validates one source-scoped observation stream identifier.
pub(crate) fn validate_observation_stream_id(stream_id: &str) -> Result<()> {
    validate_observation_identifier(
        "stream_id",
        stream_id,
        MAX_OBSERVATION_STREAM_ID_BYTES,
        true,
    )
}

fn validate_observation_identifier(
    label: &str,
    value: &str,
    max_bytes: usize,
    allow_colon: bool,
) -> Result<()> {
    anyhow::ensure!(!value.trim().is_empty(), "{label} is required");
    anyhow::ensure!(
        value == value.trim(),
        "{label} must not contain leading or trailing whitespace"
    );
    anyhow::ensure!(
        value.len() <= max_bytes,
        "{label} must not exceed {max_bytes} bytes"
    );
    anyhow::ensure!(
        !value.chars().any(char::is_control),
        "{label} must not contain control characters"
    );
    anyhow::ensure!(
        value != "." && value != "..",
        "{label} cannot be '.' or '..'"
    );
    let allowed_message = if allow_colon {
        format!(
            "{label} may contain only ASCII letters, digits, dots, underscores, dashes, or colons"
        )
    } else {
        format!("{label} may contain only ASCII letters, digits, dots, underscores, or dashes")
    };
    anyhow::ensure!(
        value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(ch, '-' | '_' | '.')
                || (allow_colon && ch == ':')
        }),
        "{allowed_message}"
    );
    Ok(())
}

/// The durable kind of external observation source managed by the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSourceKind {
    /// One user-defined screen snapshot source.
    ScreenSnapshot,
    /// One user-defined webcam snapshot source.
    WebcamSnapshot,
    /// One user-defined microphone segment source.
    MicrophoneSegment,
}

impl ObservationSourceKind {
    /// Returns the MIME types accepted by this source kind.
    pub fn allowed_media_types(&self) -> &'static [&'static str] {
        match self {
            Self::ScreenSnapshot | Self::WebcamSnapshot => &["image/png", "image/jpeg"],
            Self::MicrophoneSegment => &["audio/wav", "audio/webm"],
        }
    }

    /// Returns whether raw source assets should be attached during materialization by default.
    pub fn include_raw_asset_by_default(&self) -> bool {
        matches!(self, Self::ScreenSnapshot | Self::WebcamSnapshot)
    }
}

/// Controls how raw observation assets are attached during materialization.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationRawAssetPolicy {
    /// Attaches raw assets only for source kinds that enable them by default.
    #[default]
    Auto,
    /// Never attaches raw assets, even when the source kind allows them by default.
    Never,
    /// Always attaches raw assets when the observation still resolves one.
    Always,
}

impl ObservationRawAssetPolicy {
    /// Returns whether one raw source asset should be attached for the provided source kind.
    pub fn should_attach(self, source_kind: &ObservationSourceKind) -> bool {
        match self {
            Self::Auto => source_kind.include_raw_asset_by_default(),
            Self::Never => false,
            Self::Always => true,
        }
    }
}

/// The durable lifecycle state of one observation source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSourceStatus {
    /// The source accepts new uploads and materialization.
    Active,
    /// The source rejects new uploads until re-enabled.
    Paused,
    /// The source is disabled and should not produce or materialize new work.
    Disabled,
}

impl ObservationSourceStatus {
    pub(crate) fn accepts_ingest(&self) -> bool {
        matches!(self, Self::Active)
    }
}

/// The sensitivity class attached to one observation source or record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSensitivity {
    /// The observation may flow through normal daemon surfaces.
    Standard,
    /// The observation must remain deny-by-default for external reply routing.
    Sensitive,
}

/// The durable retention state of one observation record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationRetentionState {
    /// The observation is active and available for listing and materialization.
    Active,
    /// The observation exceeded source retention limits and is no longer materializable.
    Purged,
}

/// The externally visible view of one observation source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationSourceView {
    /// The stable daemon-owned source identifier.
    pub source_id: String,
    /// The user-visible source name.
    pub display_name: String,
    /// The source kind.
    pub kind: ObservationSourceKind,
    /// The current source lifecycle state.
    pub status: ObservationSourceStatus,
    /// The source sensitivity class.
    pub sensitivity: ObservationSensitivity,
    /// The maximum age in seconds for active observations under this source.
    pub retention_seconds: u64,
    /// The maximum number of active observations retained for this source.
    pub max_active_observations: u64,
    /// The maximum active raw-byte budget retained for this source.
    pub max_active_bytes: u64,
    /// Rolling window duration used for source-scoped upload rate limiting.
    #[serde(default = "default_source_ingest_rate_limit_window_ms")]
    pub ingest_rate_limit_window_ms: u64,
    /// Maximum successful uploads accepted per source-scoped rate-limit window.
    #[serde(default = "default_source_ingest_rate_limit_burst")]
    pub ingest_rate_limit_burst: u64,
    /// Whether retention purges also remove daemon-owned raw/canonical assets when unreferenced.
    #[serde(default)]
    pub purge_raw_on_retention: bool,
    /// Whether this source may be materialized into agent inputs.
    pub allow_materialization: bool,
    /// Whether this source may inherit or target external reply routes.
    pub allow_output_delivery: bool,
    /// The creation timestamp in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// The last update timestamp in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
    /// The latest successful source-authenticated upload timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_authenticated_at_ms: Option<u64>,
    /// Monotonic upload-token generation for this source.
    #[serde(default = "default_upload_token_version")]
    pub upload_token_version: u64,
    /// The latest token creation or rotation timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_token_rotated_at_ms: Option<u64>,
    /// The current source upload token revocation timestamp, when revoked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_token_revoked_at_ms: Option<u64>,
    /// The latest ingested observation timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_observed_at_ms: Option<u64>,
    /// The current number of active observations tracked for this source.
    #[serde(default)]
    pub active_observation_count: u64,
    /// The current number of active raw bytes tracked for this source.
    #[serde(default)]
    pub active_byte_length: u64,
}

/// One persisted observation source record stored under the daemon state root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ObservationSourceRecord {
    /// The externally visible source view.
    pub(crate) view: ObservationSourceView,
    /// The SHA-256 digest of the upload bearer token when one is configured.
    pub(crate) upload_token_sha256: String,
    /// Previous upload tokens accepted until their grace window expires.
    #[serde(default)]
    pub(crate) previous_upload_tokens: Vec<ObservationSourceUploadTokenRecord>,
    /// Owning capture agent, when this source is managed by capture provisioning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) capture_owner_machine_id: Option<String>,
    /// Expiry timestamp for the current capture-owned source lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) capture_lease_expires_at_ms: Option<u64>,
}

/// One prior upload token retained for a bounded rotation grace window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ObservationSourceUploadTokenRecord {
    /// SHA-256 digest of the previous token.
    pub(crate) sha256: String,
    /// Token generation this digest belonged to.
    pub(crate) version: u64,
    /// Last timestamp at which this token may authenticate uploads.
    pub(crate) expires_at_ms: u64,
    /// Revocation timestamp, when explicitly revoked before expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) revoked_at_ms: Option<u64>,
}

/// One sanitized append-only observation audit record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationAuditRecord {
    /// The daemon timestamp in milliseconds since the Unix epoch.
    pub recorded_at_ms: u64,
    /// Stable event kind such as `source_created`, `source_rotated`, or `upload_succeeded`.
    pub event: String,
    /// The source identifier associated with the event.
    pub source_id: String,
    /// The observation identifier when the event refers to one record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_id: Option<String>,
    /// Sanitized reason code for rejected or rate-limited uploads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Upload-token generation in effect for this event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_token_version: Option<u64>,
    /// Digest of the caller idempotency key, never the key itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key_sha256: Option<String>,
    /// Request fingerprint when an accepted/replayed upload reached idempotency handling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_fingerprint: Option<String>,
    /// Observation media type when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Observation byte length when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_length: Option<u64>,
    /// Milliseconds until retry for rate-limited uploads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    /// Asset ids physically removed by retention purge.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub purged_asset_ids: Vec<String>,
}

/// One request used to rotate a source-scoped observation upload token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotateObservationSourceTokenRequest {
    /// New bearer token accepted by the upload endpoint.
    pub upload_token: String,
    /// Milliseconds during which the previous token remains accepted.
    #[serde(default)]
    pub grace_period_ms: u64,
}

impl RotateObservationSourceTokenRequest {
    /// Validates one source-token rotation request.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.upload_token.trim().is_empty(),
            "upload_token is required"
        );
        anyhow::ensure!(
            self.grace_period_ms <= 24 * 60 * 60 * 1_000,
            "grace_period_ms cannot exceed 86400000"
        );
        Ok(())
    }
}

/// One request used to revoke a source-scoped observation upload token.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeObservationSourceTokenRequest {
    /// Optional sanitized operator reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl RevokeObservationSourceTokenRequest {
    /// Validates one source-token revocation request.
    pub fn validate(&self) -> Result<()> {
        if let Some(reason) = self.reason.as_deref() {
            anyhow::ensure!(
                reason.chars().count() <= 512,
                "reason cannot exceed 512 characters"
            );
        }
        Ok(())
    }
}

/// The externally visible view of one observation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationView {
    /// The stable daemon-owned observation identifier.
    pub observation_id: String,
    /// The owning source identifier.
    pub source_id: String,
    /// The owning source kind.
    pub kind: ObservationSourceKind,
    /// The source sensitivity class captured at ingest time.
    pub sensitivity: ObservationSensitivity,
    /// The current retention state.
    pub retention_state: ObservationRetentionState,
    /// The daemon-owned raw asset identifier.
    pub asset_id: String,
    /// The daemon-owned canonical-text asset identifier when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_text_asset_id: Option<String>,
    /// The normalized MIME type of the raw observation payload.
    pub media_type: String,
    /// The normalized SHA-256 digest of the raw observation payload.
    pub sha256: String,
    /// The raw observation payload size in bytes.
    pub byte_length: u64,
    /// The original capture timestamp in milliseconds since the Unix epoch.
    pub captured_at_ms: u64,
    /// The daemon receive timestamp in milliseconds since the Unix epoch.
    pub received_at_ms: u64,
    /// The source stream identifier when provided by the uploader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    /// The source sequence number when provided by the uploader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq_no: Option<u64>,
    /// The uploader-supplied idempotency key.
    pub idempotency_key: String,
    /// The stable request fingerprint bound to the idempotency key.
    pub request_fingerprint: String,
    /// Optional caller-supplied observation metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

impl ObservationView {
    /// Returns whether this observation is still materializable.
    pub fn is_active(&self) -> bool {
        self.retention_state == ObservationRetentionState::Active
    }
}

/// One materialization selection over existing observations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ObservationSelection {
    /// Materializes the provided stable observation identifiers.
    ObservationIds { observation_ids: Vec<String> },
    /// Materializes the most recent active observations sharing one capture group identifier.
    ObservationGroup {
        capture_group_id: String,
        #[serde(default = "default_materialization_max_observations")]
        max_observations: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lookback_seconds: Option<u64>,
    },
    /// Materializes the most recent observations for one source.
    LatestFromSource {
        source_id: String,
        #[serde(default = "default_materialization_max_observations")]
        max_observations: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lookback_seconds: Option<u64>,
    },
    /// Materializes the most recent observations for one source stream.
    LatestFromStream {
        source_id: String,
        stream_id: String,
        #[serde(default = "default_materialization_max_observations")]
        max_observations: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lookback_seconds: Option<u64>,
    },
}

fn default_materialization_max_observations() -> u64 {
    3
}

/// One request to materialize observations into a standard daemon input run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObservationMaterializationRequest {
    /// The target session that should receive the materialized input.
    pub target_session_id: String,
    /// The observation selection resolved at execution time.
    pub selection: ObservationSelection,
    /// The base input request that will be augmented with selected observations.
    pub request: SubmitInputRequest,
    /// The legacy raw-asset toggle preserved for backward-compatible callers.
    #[serde(default = "default_include_raw_assets")]
    pub include_raw_assets: bool,
    /// The explicit raw-asset policy override when callers need behavior beyond the legacy flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_asset_policy: Option<ObservationRawAssetPolicy>,
    /// Whether execution should fail when no active observations match the selection.
    #[serde(default = "default_fail_when_empty")]
    pub fail_when_empty: bool,
}

fn default_include_raw_assets() -> bool {
    true
}

fn default_fail_when_empty() -> bool {
    true
}

impl ObservationMaterializationRequest {
    /// Validates the request payload before the daemon schedules or executes it.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.target_session_id.trim().is_empty(),
            "target_session_id is required"
        );
        match &self.selection {
            ObservationSelection::ObservationIds { observation_ids } => {
                anyhow::ensure!(
                    !observation_ids.is_empty(),
                    "observation_ids must contain at least one identifier"
                );
                for observation_id in observation_ids {
                    anyhow::ensure!(
                        !observation_id.trim().is_empty(),
                        "observation_ids cannot contain empty identifiers"
                    );
                }
            }
            ObservationSelection::ObservationGroup {
                capture_group_id,
                max_observations,
                ..
            } => {
                anyhow::ensure!(
                    !capture_group_id.trim().is_empty(),
                    "capture_group_id is required"
                );
                anyhow::ensure!(
                    *max_observations > 0,
                    "max_observations must be greater than zero"
                );
            }
            ObservationSelection::LatestFromSource {
                source_id,
                max_observations,
                ..
            } => {
                anyhow::ensure!(!source_id.trim().is_empty(), "source_id is required");
                validate_observation_source_id(source_id)?;
                anyhow::ensure!(
                    *max_observations > 0,
                    "max_observations must be greater than zero"
                );
            }
            ObservationSelection::LatestFromStream {
                source_id,
                stream_id,
                max_observations,
                ..
            } => {
                anyhow::ensure!(!source_id.trim().is_empty(), "source_id is required");
                anyhow::ensure!(!stream_id.trim().is_empty(), "stream_id is required");
                validate_observation_source_id(source_id)?;
                validate_observation_stream_id(stream_id)?;
                anyhow::ensure!(
                    *max_observations > 0,
                    "max_observations must be greater than zero"
                );
            }
        }
        Ok(())
    }

    /// Resolves the effective raw-asset policy, preserving the legacy boolean as fallback.
    pub fn resolved_raw_asset_policy(&self) -> ObservationRawAssetPolicy {
        self.raw_asset_policy.unwrap_or(if self.include_raw_assets {
            ObservationRawAssetPolicy::Auto
        } else {
            ObservationRawAssetPolicy::Never
        })
    }
}

/// Summarizes one observation materialization request for run and schedule views.
pub(crate) fn summarize_observation_materialization_request(
    request: &ObservationMaterializationRequest,
) -> RunRequestSummary {
    let mut summary = summarize_input_request(&request.request);
    summary.source_plugin = "daemon".to_string();
    summary.source_kind = "observation_materialization".to_string();
    summary.actor_id = match &request.selection {
        ObservationSelection::ObservationIds { observation_ids } => observation_ids
            .first()
            .cloned()
            .unwrap_or_else(|| "observation-selection".to_string()),
        ObservationSelection::ObservationGroup {
            capture_group_id, ..
        } => format!("observation-group:{capture_group_id}"),
        ObservationSelection::LatestFromSource { source_id, .. } => source_id.clone(),
        ObservationSelection::LatestFromStream {
            source_id,
            stream_id,
            ..
        } => format!("{source_id}:{stream_id}"),
    };
    if summary.text_preview.is_none() {
        summary.text_preview = Some(match &request.selection {
            ObservationSelection::ObservationIds { observation_ids } => format!(
                "Materialize {} observations into session {}",
                observation_ids.len(),
                request.target_session_id
            ),
            ObservationSelection::ObservationGroup {
                capture_group_id,
                max_observations,
                lookback_seconds,
            } => match lookback_seconds {
                Some(lookback_seconds) => format!(
                    "Materialize up to {max_observations} recent observations from capture group {capture_group_id} over the last {lookback_seconds}s"
                ),
                None => format!(
                    "Materialize up to {max_observations} recent observations from capture group {capture_group_id}"
                ),
            },
            ObservationSelection::LatestFromSource {
                source_id,
                max_observations,
                lookback_seconds,
            } => match lookback_seconds {
                Some(lookback_seconds) => format!(
                    "Materialize up to {max_observations} recent observations from {source_id} over the last {lookback_seconds}s"
                ),
                None => format!(
                    "Materialize up to {max_observations} recent observations from {source_id}"
                ),
            },
            ObservationSelection::LatestFromStream {
                source_id,
                stream_id,
                max_observations,
                lookback_seconds,
            } => match lookback_seconds {
                Some(lookback_seconds) => format!(
                    "Materialize up to {max_observations} recent observations from {source_id} stream {stream_id} over the last {lookback_seconds}s"
                ),
                None => format!(
                    "Materialize up to {max_observations} recent observations from {source_id} stream {stream_id}"
                ),
            },
        });
    }
    summary
}

/// Filesystem-backed observation storage rooted under one daemon state directory.
#[derive(Clone, Debug)]
pub(crate) struct FileObservationStore {
    root: PathBuf,
}

impl FileObservationStore {
    /// Creates a new observation store rooted under one daemon state directory.
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn sources_root(&self) -> PathBuf {
        self.root.join("observation-sources")
    }

    fn observations_root(&self) -> PathBuf {
        self.root.join("observations")
    }

    fn observation_partition_root(&self, source_id: &str, captured_at_ms: u64) -> PathBuf {
        let source_partition = hex::encode(Sha256::digest(source_id.as_bytes()));
        let bucket = Utc
            .timestamp_millis_opt(captured_at_ms as i64)
            .single()
            .unwrap_or_else(Utc::now)
            .format("%Y-%m-%d")
            .to_string();
        self.observations_root()
            .join(&source_partition[..16])
            .join(bucket)
    }

    fn observation_path(
        &self,
        source_id: &str,
        observation_id: &str,
        captured_at_ms: u64,
    ) -> Result<PathBuf> {
        prepare_storage_path_for_write(
            &self.observation_partition_root(source_id, captured_at_ms),
            observation_id,
            "json",
        )
    }

    /// Loads every persisted observation source record, quarantining corrupted files.
    pub(crate) fn load_sources(&self) -> Result<BTreeMap<String, ObservationSourceRecord>> {
        let mut records = BTreeMap::new();
        for root in [self.sources_root(), self.sources_root().join("__safe")] {
            if !root.exists() {
                continue;
            }
            for entry in fs::read_dir(&root)? {
                let entry = entry?;
                let path = entry.path();
                if !path.is_file()
                    || path.extension().and_then(|value| value.to_str()) != Some("json")
                {
                    continue;
                }
                let Some(record) = read_json_or_quarantine::<ObservationSourceRecord>(
                    &path,
                    "observation source",
                )?
                else {
                    continue;
                };
                records.insert(record.view.source_id.clone(), record);
            }
        }
        Ok(records)
    }

    /// Persists one observation source record atomically.
    pub(crate) fn save_source(&self, record: &ObservationSourceRecord) -> Result<()> {
        let path =
            prepare_storage_path_for_write(&self.sources_root(), &record.view.source_id, "json")?;
        write_json_pretty_atomically(&path, record)
    }

    /// Deletes one persisted observation source record when present.
    pub(crate) fn delete_source(&self, source_id: &str) -> Result<()> {
        let path = prepare_storage_path_for_write(&self.sources_root(), source_id, "json")?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("failed to delete observation source {}", path.display())),
        }
    }

    /// Loads every persisted observation record, quarantining corrupted files.
    pub(crate) fn load_observations(&self) -> Result<BTreeMap<String, ObservationView>> {
        let mut records = BTreeMap::new();
        self.walk_observation_dir(&self.observations_root(), &mut records)?;
        Ok(records)
    }

    fn walk_observation_dir(
        &self,
        root: &Path,
        records: &mut BTreeMap<String, ObservationView>,
    ) -> Result<()> {
        if !root.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                self.walk_observation_dir(&path, records)?;
                continue;
            }
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(record) =
                read_json_or_quarantine::<ObservationView>(&path, "observation record")?
            else {
                continue;
            };
            records.insert(record.observation_id.clone(), record);
        }
        Ok(())
    }

    /// Persists one observation record atomically.
    pub(crate) fn save_observation(&self, record: &ObservationView) -> Result<()> {
        let path = self.observation_path(
            &record.source_id,
            &record.observation_id,
            record.captured_at_ms,
        )?;
        write_json_pretty_atomically(&path, record)
    }

    /// Appends one sanitized observation audit record.
    pub(crate) fn append_audit(&self, record: &ObservationAuditRecord) -> Result<()> {
        let path = prepare_storage_path_for_write(
            &self.root.join("observation-audit"),
            "events",
            "jsonl",
        )?;
        append_json_line_sync(&path, record)
    }

    /// Loads sanitized observation audit records in append order.
    pub(crate) fn load_audit_records(&self) -> Result<Vec<ObservationAuditRecord>> {
        let path = prepare_storage_path_for_write(
            &self.root.join("observation-audit"),
            "events",
            "jsonl",
        )?;
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read observation audit {}", path.display())
                });
            }
        };
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str::<ObservationAuditRecord>(line)
                    .context("failed to parse observation audit record")
            })
            .collect()
    }

    /// Returns the next numeric source identifier seed.
    pub(crate) fn next_source_seed(&self) -> u64 {
        self.source_file_paths(&self.sources_root())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|path| source_id_from_path(&path))
            .filter_map(|id| {
                id.strip_prefix("source-")
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }

    /// Returns the next numeric observation identifier seed.
    pub(crate) fn next_observation_seed(&self) -> u64 {
        self.observation_file_paths(&self.observations_root())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|path| observation_id_from_path(&path))
            .filter_map(|id| {
                id.strip_prefix("observation-")
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }

    fn source_file_paths(&self, root: &Path) -> Result<Vec<PathBuf>> {
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                paths.extend(self.source_file_paths(&path)?);
            } else if path.is_file() {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    fn observation_file_paths(&self, root: &Path) -> Result<Vec<PathBuf>> {
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                paths.extend(self.observation_file_paths(&path)?);
            } else if path.is_file() {
                paths.push(path);
            }
        }
        Ok(paths)
    }
}

fn source_id_from_path(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    let base_name = file_name.split(".corrupt-").next().unwrap_or(file_name);
    let stem = base_name.strip_suffix(".json")?;
    kheish_session::decode_safe_storage_name(stem).or_else(|| Some(stem.to_string()))
}

fn observation_id_from_path(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    let base_name = file_name.split(".corrupt-").next().unwrap_or(file_name);
    let stem = base_name.strip_suffix(".json")?;
    kheish_session::decode_safe_storage_name(stem).or_else(|| Some(stem.to_string()))
}

#[cfg(test)]
fn source_path_for_test(root: &Path, source_id: &str) -> PathBuf {
    prepare_storage_path_for_write(root, source_id, "json").expect("test source path")
}

/// One request used to create or rotate an observation source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateObservationSourceRequest {
    /// The optional caller-supplied source identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// The user-visible source name.
    pub display_name: String,
    /// The durable source kind.
    pub kind: ObservationSourceKind,
    /// The bearer token accepted by the upload endpoint for this source.
    pub upload_token: String,
    /// The source sensitivity class.
    #[serde(default = "default_source_sensitivity")]
    pub sensitivity: ObservationSensitivity,
    /// The maximum age in seconds for active observations retained by this source.
    #[serde(default = "default_source_retention_seconds")]
    pub retention_seconds: u64,
    /// The maximum number of active observations retained by this source.
    #[serde(default = "default_source_max_active_observations")]
    pub max_active_observations: u64,
    /// The maximum number of active raw bytes retained by this source.
    #[serde(default = "default_source_max_active_bytes")]
    pub max_active_bytes: u64,
    /// Rolling window duration used for source-scoped upload rate limiting.
    #[serde(default = "default_source_ingest_rate_limit_window_ms")]
    pub ingest_rate_limit_window_ms: u64,
    /// Maximum successful uploads accepted per source-scoped rate-limit window.
    #[serde(default = "default_source_ingest_rate_limit_burst")]
    pub ingest_rate_limit_burst: u64,
    /// Whether retention purges should also remove unreferenced raw/canonical assets.
    #[serde(default)]
    pub purge_raw_on_retention: bool,
    /// Whether this source may be materialized into agent inputs.
    #[serde(default = "default_allow_materialization")]
    pub allow_materialization: bool,
    /// Whether this source may inherit or target external reply routes.
    #[serde(default)]
    pub allow_output_delivery: bool,
}

fn default_source_sensitivity() -> ObservationSensitivity {
    ObservationSensitivity::Sensitive
}

fn default_source_retention_seconds() -> u64 {
    DEFAULT_SOURCE_RETENTION_SECONDS
}

fn default_source_max_active_observations() -> u64 {
    DEFAULT_SOURCE_MAX_ACTIVE_OBSERVATIONS
}

fn default_source_max_active_bytes() -> u64 {
    DEFAULT_SOURCE_MAX_ACTIVE_BYTES
}

fn default_source_ingest_rate_limit_window_ms() -> u64 {
    DEFAULT_SOURCE_INGEST_RATE_LIMIT_WINDOW_MS
}

fn default_source_ingest_rate_limit_burst() -> u64 {
    DEFAULT_SOURCE_INGEST_RATE_LIMIT_BURST
}

fn default_upload_token_version() -> u64 {
    1
}

fn default_allow_materialization() -> bool {
    true
}

impl CreateObservationSourceRequest {
    /// Validates the create-source payload before persistence.
    pub fn validate(&self) -> Result<()> {
        if let Some(source_id) = self.source_id.as_deref() {
            let source_id = source_id.trim();
            anyhow::ensure!(!source_id.is_empty(), "source_id cannot be empty");
            validate_observation_source_id(source_id)?;
        }
        anyhow::ensure!(
            !self.display_name.trim().is_empty(),
            "display_name is required"
        );
        anyhow::ensure!(
            !self.upload_token.trim().is_empty(),
            "upload_token is required"
        );
        anyhow::ensure!(
            self.retention_seconds > 0,
            "retention_seconds must be greater than zero"
        );
        anyhow::ensure!(
            self.max_active_observations > 0,
            "max_active_observations must be greater than zero"
        );
        anyhow::ensure!(
            self.max_active_bytes > 0,
            "max_active_bytes must be greater than zero"
        );
        anyhow::ensure!(
            self.ingest_rate_limit_window_ms > 0,
            "ingest_rate_limit_window_ms must be greater than zero"
        );
        anyhow::ensure!(
            self.ingest_rate_limit_burst > 0,
            "ingest_rate_limit_burst must be greater than zero"
        );
        Ok(())
    }
}

/// One upload request accepted by the source-scoped observation ingest endpoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreateObservationRequest {
    /// The inline asset payload uploaded for this observation.
    pub upload: crate::InlineAssetUpload,
    /// The stable idempotency key for this source event.
    pub idempotency_key: String,
    /// The original capture timestamp in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at_ms: Option<u64>,
    /// The source stream identifier when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    /// The monotonically increasing source sequence number when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq_no: Option<u64>,
    /// Optional caller-supplied canonical text such as OCR or STT output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_text: Option<String>,
    /// Optional caller-supplied metadata persisted beside the observation.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

impl CreateObservationRequest {
    /// Validates the ingest payload before the daemon stores it.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.idempotency_key.trim().is_empty(),
            "idempotency_key is required"
        );
        anyhow::ensure!(
            self.idempotency_key.trim().len() <= MAX_OBSERVATION_IDEMPOTENCY_KEY_BYTES,
            "idempotency_key must not exceed {MAX_OBSERVATION_IDEMPOTENCY_KEY_BYTES} bytes"
        );
        anyhow::ensure!(
            !self.idempotency_key.trim().chars().any(char::is_control),
            "idempotency_key must not contain control characters"
        );
        anyhow::ensure!(
            !self.upload.file_name.trim().is_empty(),
            "upload.file_name is required"
        );
        anyhow::ensure!(
            !self.upload.content_base64.trim().is_empty(),
            "upload.content_base64 is required"
        );
        if let Some(stream_id) = self.stream_id.as_deref() {
            validate_observation_stream_id(stream_id)?;
        }
        Ok(())
    }

    /// Returns one deterministic fingerprint bound to the idempotency key.
    pub fn fingerprint(&self) -> Result<String> {
        let encoded = serde_json::to_vec(self)?;
        Ok(hex::encode(Sha256::digest(encoded)))
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serde_json::json;

    use super::*;

    #[test]
    fn observation_identifiers_accept_path_safe_ascii() -> Result<()> {
        validate_observation_source_id("screen-materialize")?;
        validate_observation_stream_id("screen:display:display-main")?;
        validate_observation_stream_id("call_1.segment-2")?;
        assert!(
            validate_observation_stream_id("bad\nstream")
                .expect_err("control char should fail")
                .to_string()
                .contains("control characters")
        );
        assert!(
            validate_observation_source_id("bad/source")
                .expect_err("slash should fail")
                .to_string()
                .contains("may contain only")
        );
        Ok(())
    }

    #[test]
    fn observation_materialization_request_requires_one_valid_selection() -> Result<()> {
        let request = ObservationMaterializationRequest {
            target_session_id: "session-1".to_string(),
            selection: ObservationSelection::LatestFromStream {
                source_id: "screen-1".to_string(),
                stream_id: "call-1".to_string(),
                max_observations: 2,
                lookback_seconds: Some(600),
            },
            request: SubmitInputRequest {
                provider: None,
                source_plugin: None,
                source_kind: None,
                actor_id: None,
                content: "Analyze the latest snapshots.".to_string(),
                input_items: Vec::new(),
                attachments: Vec::new(),
                generation: None,
                completion_requirements: None,
                metadata: Some(json!({})),
                binding_keys: Vec::new(),
                reply_targets: Vec::new(),
                reply_plugin: None,
                reply_address: None,
            },
            include_raw_assets: true,
            raw_asset_policy: None,
            fail_when_empty: true,
        };
        request.validate()?;
        Ok(())
    }

    #[test]
    fn observation_materialization_request_rejects_empty_stream_id() {
        let request = ObservationMaterializationRequest {
            target_session_id: "session-1".to_string(),
            selection: ObservationSelection::LatestFromStream {
                source_id: "screen-1".to_string(),
                stream_id: "  ".to_string(),
                max_observations: 1,
                lookback_seconds: None,
            },
            request: SubmitInputRequest {
                provider: None,
                source_plugin: None,
                source_kind: None,
                actor_id: None,
                content: String::new(),
                input_items: Vec::new(),
                attachments: Vec::new(),
                generation: None,
                completion_requirements: None,
                metadata: Some(json!({})),
                binding_keys: Vec::new(),
                reply_targets: Vec::new(),
                reply_plugin: None,
                reply_address: None,
            },
            include_raw_assets: true,
            raw_asset_policy: None,
            fail_when_empty: true,
        };
        let error = request.validate().expect_err("empty stream_id should fail");
        assert!(error.to_string().contains("stream_id is required"));
    }

    #[test]
    fn source_store_next_seed_ignores_quarantine_suffixes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileObservationStore::new(root.path());
        fs::create_dir_all(store.sources_root())?;
        let source_one = source_path_for_test(&store.sources_root(), "source-1");
        let source_two = source_path_for_test(&store.sources_root(), "source-2");
        fs::create_dir_all(source_one.parent().expect("source-1 parent"))?;
        fs::create_dir_all(source_two.parent().expect("source-2 parent"))?;
        fs::write(&source_one, "{}")?;
        fs::write(&source_two, "{}")?;
        let quarantined = source_two.with_file_name(format!(
            "{}.corrupt-test",
            source_two
                .file_name()
                .expect("source-2 file")
                .to_string_lossy()
        ));
        fs::rename(&source_two, quarantined)?;
        assert_eq!(store.next_source_seed(), 3);
        Ok(())
    }

    #[test]
    fn observation_store_next_seed_decodes_safe_storage_names() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileObservationStore::new(root.path());
        let captured_at_ms = Utc
            .with_ymd_and_hms(2026, 5, 7, 9, 0, 0)
            .single()
            .expect("valid timestamp")
            .timestamp_millis() as u64;
        let observation_one =
            store.observation_path("screen-1", "observation-237", captured_at_ms)?;
        let observation_two =
            store.observation_path("screen-1", "observation-238", captured_at_ms)?;
        fs::create_dir_all(observation_one.parent().expect("observation-237 parent"))?;
        fs::create_dir_all(observation_two.parent().expect("observation-238 parent"))?;
        fs::write(&observation_one, "{}")?;
        fs::write(&observation_two, "{}")?;

        assert_eq!(store.next_observation_seed(), 239);
        Ok(())
    }
}
