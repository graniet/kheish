//! Durable transcript jobs derived from daemon-owned observations.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kheish_session::{prepare_storage_path_for_write, write_json_pretty_atomically};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::assets::StoredAssetRecord;
use crate::observations::{ObservationRetentionState, ObservationView};
use crate::runs::now_ms;
use crate::state_files::read_json_or_quarantine;

pub(crate) const DEFAULT_TRANSCRIPT_CHUNK_SECONDS: u64 = 60;
pub(crate) const DEFAULT_TRANSCRIPT_CHUNK_MAX_BYTES: u64 = 10 * 1024 * 1024;
const MAX_TRANSCRIPT_IDEMPOTENCY_KEY_BYTES: usize = 256;
const MAX_TRANSCRIPT_CAPTURE_GROUP_ID_BYTES: usize = 256;
const MAX_TRANSCRIPT_ROLE_BYTES: usize = 64;

/// Durable lifecycle state for one observation transcript job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationTranscriptStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    Skipped,
}

impl ObservationTranscriptStatus {
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }
}

/// Coarse phase used by clients to display processing progress.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationTranscriptPhase {
    Selecting,
    Chunking,
    Transcribing,
    Finalizing,
    Completed,
    Failed,
    Cancelled,
}

/// Selection frozen when creating one transcript job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationTranscriptSelection {
    pub capture_group_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    #[serde(default = "default_include_purged")]
    pub include_purged: bool,
}

fn default_include_purged() -> bool {
    true
}

impl ObservationTranscriptSelection {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_transcript_identifier(
            "capture_group_id",
            &self.capture_group_id,
            MAX_TRANSCRIPT_CAPTURE_GROUP_ID_BYTES,
        )?;
        if let (Some(after_ms), Some(before_ms)) = (self.after_ms, self.before_ms) {
            anyhow::ensure!(after_ms <= before_ms, "after_ms must be <= before_ms");
        }
        if let Some(recording_id) = self.recording_id.as_deref() {
            validate_transcript_identifier("recording_id", recording_id, 256)?;
        }
        let mut roles = BTreeSet::new();
        for role in &self.roles {
            validate_transcript_identifier("role", role, MAX_TRANSCRIPT_ROLE_BYTES)?;
            anyhow::ensure!(roles.insert(role), "roles must not contain duplicates");
        }
        Ok(())
    }

    pub(crate) fn matches(&self, observation: &ObservationView) -> bool {
        if !self.include_purged && observation.retention_state != ObservationRetentionState::Active
        {
            return false;
        }
        if self
            .after_ms
            .is_some_and(|after_ms| observation.captured_at_ms < after_ms)
        {
            return false;
        }
        if self
            .before_ms
            .is_some_and(|before_ms| observation.captured_at_ms > before_ms)
        {
            return false;
        }
        let Some(metadata) = observation.metadata.as_object() else {
            return false;
        };
        if metadata
            .get("capture_group_id")
            .and_then(|value| value.as_str())
            != Some(self.capture_group_id.as_str())
        {
            return false;
        }
        if let Some(recording_id) = self.recording_id.as_deref()
            && metadata
                .get("recording_id")
                .and_then(|value| value.as_str())
                != Some(recording_id)
        {
            return false;
        }
        if self.roles.is_empty() {
            return true;
        }
        let Some(role) = metadata.get("role").and_then(|value| value.as_str()) else {
            return false;
        };
        self.roles.iter().any(|candidate| candidate == role)
    }
}

/// Backend/chunking options for one transcript job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationTranscriptTranscriptionOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
    #[serde(default = "default_transcript_chunk_seconds")]
    pub target_chunk_seconds: u64,
    #[serde(default = "default_transcript_chunk_max_bytes")]
    pub max_chunk_bytes: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub diarization: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_transcript_chunk_seconds() -> u64 {
    DEFAULT_TRANSCRIPT_CHUNK_SECONDS
}

fn default_transcript_chunk_max_bytes() -> u64 {
    DEFAULT_TRANSCRIPT_CHUNK_MAX_BYTES
}

impl Default for ObservationTranscriptTranscriptionOptions {
    fn default() -> Self {
        Self {
            route_id: None,
            target_chunk_seconds: DEFAULT_TRANSCRIPT_CHUNK_SECONDS,
            max_chunk_bytes: DEFAULT_TRANSCRIPT_CHUNK_MAX_BYTES,
            diarization: false,
        }
    }
}

impl ObservationTranscriptTranscriptionOptions {
    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(route_id) = self.route_id.as_deref() {
            validate_transcript_identifier("route_id", route_id, 256)?;
        }
        anyhow::ensure!(
            (5..=10 * 60).contains(&self.target_chunk_seconds),
            "target_chunk_seconds must be between 5 and 600"
        );
        anyhow::ensure!(
            (64 * 1024..=12 * 1024 * 1024).contains(&self.max_chunk_bytes),
            "max_chunk_bytes must be between 65536 and 12582912"
        );
        Ok(())
    }
}

/// Request used to create or replay one observation transcript job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObservationTranscriptCreateRequest {
    pub idempotency_key: String,
    pub selection: ObservationTranscriptSelection,
    #[serde(default)]
    pub transcription: ObservationTranscriptTranscriptionOptions,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

impl ObservationTranscriptCreateRequest {
    pub(crate) fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.idempotency_key.trim().is_empty(),
            "idempotency_key is required"
        );
        anyhow::ensure!(
            self.idempotency_key.trim().len() <= MAX_TRANSCRIPT_IDEMPOTENCY_KEY_BYTES,
            "idempotency_key must not exceed {MAX_TRANSCRIPT_IDEMPOTENCY_KEY_BYTES} bytes"
        );
        anyhow::ensure!(
            !self.idempotency_key.trim().chars().any(char::is_control),
            "idempotency_key must not contain control characters"
        );
        self.selection.validate()?;
        self.transcription.validate()?;
        Ok(())
    }

    pub(crate) fn normalized(mut self) -> Self {
        self.idempotency_key = self.idempotency_key.trim().to_string();
        self.selection.capture_group_id = self.selection.capture_group_id.trim().to_string();
        self.selection.recording_id = self
            .selection
            .recording_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self.selection.roles = self
            .selection
            .roles
            .into_iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect();
        self.transcription.route_id = self
            .transcription
            .route_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self
    }

    pub(crate) fn fingerprint(&self) -> Result<String> {
        let encoded = serde_json::to_vec(self)?;
        Ok(hex::encode(Sha256::digest(encoded)))
    }
}

/// Progress counters for one transcript job.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationTranscriptProgress {
    pub total_observations: u64,
    pub selected_audio_observations: u64,
    pub skipped_observations: u64,
    pub total_chunks: u64,
    pub completed_chunks: u64,
    pub skipped_chunks: u64,
    pub failed_chunks: u64,
    pub completed_observations: u64,
    pub failed_observations: u64,
}

/// One durable transcript output artifact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationTranscriptArtifactView {
    pub kind: String,
    pub asset_id: String,
    pub media_type: String,
    pub byte_length: u64,
}

impl ObservationTranscriptArtifactView {
    pub(crate) fn new(kind: impl Into<String>, asset: &StoredAssetRecord) -> Self {
        Self {
            kind: kind.into(),
            asset_id: asset.id.clone(),
            media_type: asset.media_type.clone(),
            byte_length: asset.byte_length,
        }
    }
}

/// One transcript segment, usually a 30-60s audio chunk mapped to source observations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObservationTranscriptSegmentView {
    pub segment_id: String,
    pub status: ObservationTranscriptStatus,
    pub role: String,
    pub captured_at_ms: u64,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq_no_start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq_no_end: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observation_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_asset_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_asset_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_asset_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Externally visible transcript job view.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObservationTranscriptJobView {
    pub transcript_job_id: String,
    pub idempotency_key: String,
    pub status: ObservationTranscriptStatus,
    pub phase: ObservationTranscriptPhase,
    pub selection: ObservationTranscriptSelection,
    pub transcription: ObservationTranscriptTranscriptionOptions,
    pub progress: ObservationTranscriptProgress,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ObservationTranscriptArtifactView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<ObservationTranscriptSegmentView>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
}

/// Persisted transcript job record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ObservationTranscriptJobRecord {
    pub view: ObservationTranscriptJobView,
    pub request_fingerprint: String,
    #[serde(default)]
    pub attempt_id: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_observation_ids: Vec<String>,
}

impl ObservationTranscriptJobRecord {
    pub(crate) fn queued(
        transcript_job_id: String,
        request: ObservationTranscriptCreateRequest,
        request_fingerprint: String,
    ) -> Self {
        let now = now_ms();
        Self {
            view: ObservationTranscriptJobView {
                transcript_job_id,
                idempotency_key: request.idempotency_key,
                status: ObservationTranscriptStatus::Queued,
                phase: ObservationTranscriptPhase::Selecting,
                selection: request.selection,
                transcription: request.transcription,
                progress: ObservationTranscriptProgress::default(),
                artifacts: Vec::new(),
                segments: Vec::new(),
                metadata: request.metadata,
                error: None,
                created_at_ms: now,
                updated_at_ms: now,
                started_at_ms: None,
                finished_at_ms: None,
            },
            request_fingerprint,
            attempt_id: 0,
            selected_observation_ids: Vec::new(),
        }
    }
}

/// File-backed transcript job store.
#[derive(Clone, Debug)]
pub(crate) struct FileObservationTranscriptStore {
    root: PathBuf,
}

impl FileObservationTranscriptStore {
    pub(crate) fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            root: state_root.into().join("observation-transcripts"),
        }
    }

    fn jobs_root(&self) -> PathBuf {
        self.root.join("jobs")
    }

    fn job_path(&self, transcript_job_id: &str) -> Result<PathBuf> {
        prepare_storage_path_for_write(&self.jobs_root(), transcript_job_id, "json")
    }

    pub(crate) fn load_jobs(&self) -> Result<BTreeMap<String, ObservationTranscriptJobRecord>> {
        let mut records = BTreeMap::new();
        self.walk_job_dir(&self.jobs_root(), &mut records)?;
        Ok(records)
    }

    fn walk_job_dir(
        &self,
        root: &Path,
        records: &mut BTreeMap<String, ObservationTranscriptJobRecord>,
    ) -> Result<()> {
        if !root.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                self.walk_job_dir(&path, records)?;
                continue;
            }
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(record) =
                read_json_or_quarantine::<ObservationTranscriptJobRecord>(&path, "transcript job")?
            else {
                continue;
            };
            records.insert(record.view.transcript_job_id.clone(), record);
        }
        Ok(())
    }

    pub(crate) fn save_job(&self, record: &ObservationTranscriptJobRecord) -> Result<()> {
        let path = self.job_path(&record.view.transcript_job_id)?;
        write_json_pretty_atomically(&path, record)
    }

    pub(crate) fn next_job_seed(&self) -> u64 {
        self.job_file_paths(&self.jobs_root())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|path| transcript_job_id_from_path(&path))
            .filter_map(|id| {
                id.strip_prefix("transcript-job-")
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }

    fn job_file_paths(&self, root: &Path) -> Result<Vec<PathBuf>> {
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                paths.extend(self.job_file_paths(&path)?);
            } else if path.is_file() {
                paths.push(path);
            }
        }
        Ok(paths)
    }
}

fn transcript_job_id_from_path(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    let base_name = file_name.split(".corrupt-").next().unwrap_or(file_name);
    let stem = base_name.strip_suffix(".json")?;
    kheish_session::decode_safe_storage_name(stem).or_else(|| Some(stem.to_string()))
}

fn validate_transcript_identifier(label: &str, value: &str, max_bytes: usize) -> Result<()> {
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
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct AudioTranscriptChunk {
    pub(crate) segment_id: String,
    pub(crate) role: String,
    pub(crate) observations: Vec<ObservationView>,
    pub(crate) source_asset_ids: Vec<String>,
    pub(crate) payloads: Vec<Vec<u8>>,
    pub(crate) captured_at_ms: u64,
    pub(crate) duration_ms: u64,
    pub(crate) seq_no_start: Option<u64>,
    pub(crate) seq_no_end: Option<u64>,
}

#[cfg(test)]
pub(crate) fn build_audio_transcript_chunks(
    observations: Vec<(ObservationView, StoredAssetRecord, Vec<u8>)>,
    target_chunk_seconds: u64,
    max_chunk_bytes: u64,
) -> Result<Vec<AudioTranscriptChunk>> {
    let mut by_role = BTreeMap::<String, Vec<(ObservationView, StoredAssetRecord, Vec<u8>)>>::new();
    for item in observations {
        let role = observation_role(&item.0).unwrap_or("audio").to_string();
        by_role.entry(role).or_default().push(item);
    }

    let mut chunks = Vec::new();
    let target_duration_ms = target_chunk_seconds.saturating_mul(1000);
    for (role, mut items) in by_role {
        items.sort_by(|left, right| {
            left.0
                .captured_at_ms
                .cmp(&right.0.captured_at_ms)
                .then_with(|| left.0.observation_id.cmp(&right.0.observation_id))
        });
        let mut current = Vec::<(ObservationView, StoredAssetRecord, Vec<u8>, u64)>::new();
        let mut current_bytes = 0u64;
        let mut current_duration_ms = 0u64;
        for (observation, asset, bytes) in items {
            let duration_ms = wav_duration_ms(&bytes).unwrap_or(5_000);
            let can_concat = asset.media_type == "audio/wav" && parse_pcm_wav(&bytes).is_ok();
            let current_can_concat = current.iter().all(|(_, asset, bytes, _)| {
                asset.media_type == "audio/wav" && parse_pcm_wav(bytes).is_ok()
            });
            if !current.is_empty() && (!can_concat || !current_can_concat) {
                chunks.push(audio_transcript_chunk_from_items(
                    &role,
                    chunks.len() + 1,
                    std::mem::take(&mut current),
                    current_duration_ms,
                ));
                current_bytes = 0;
                current_duration_ms = 0;
            }
            if !can_concat {
                chunks.push(audio_transcript_chunk_from_items(
                    &role,
                    chunks.len() + 1,
                    vec![(observation, asset, bytes, duration_ms)],
                    duration_ms,
                ));
                continue;
            }
            let projected_bytes = current_bytes.saturating_add(bytes.len() as u64);
            let projected_duration = current_duration_ms.saturating_add(duration_ms);
            if !current.is_empty()
                && (projected_bytes > max_chunk_bytes || projected_duration > target_duration_ms)
            {
                chunks.push(audio_transcript_chunk_from_items(
                    &role,
                    chunks.len() + 1,
                    std::mem::take(&mut current),
                    current_duration_ms,
                ));
                current_bytes = 0;
                current_duration_ms = 0;
            }
            current_bytes = current_bytes.saturating_add(bytes.len() as u64);
            current_duration_ms = current_duration_ms.saturating_add(duration_ms);
            current.push((observation, asset, bytes, duration_ms));
        }
        if !current.is_empty() {
            chunks.push(audio_transcript_chunk_from_items(
                &role,
                chunks.len() + 1,
                current,
                current_duration_ms,
            ));
        }
    }
    chunks.sort_by(|left, right| {
        left.captured_at_ms
            .cmp(&right.captured_at_ms)
            .then_with(|| left.role.cmp(&right.role))
            .then_with(|| left.segment_id.cmp(&right.segment_id))
    });
    for (index, chunk) in chunks.iter_mut().enumerate() {
        chunk.segment_id = format!("segment-{}", index + 1);
    }
    Ok(chunks)
}

pub(crate) fn audio_transcript_chunk_from_items(
    role: &str,
    index: usize,
    items: Vec<(ObservationView, StoredAssetRecord, Vec<u8>, u64)>,
    duration_ms: u64,
) -> AudioTranscriptChunk {
    let captured_at_ms = items
        .iter()
        .map(|(observation, _, _, _)| observation.captured_at_ms)
        .min()
        .unwrap_or(0);
    let seq_no_start = items
        .iter()
        .filter_map(|(observation, _, _, _)| observation.seq_no)
        .min();
    let seq_no_end = items
        .iter()
        .filter_map(|(observation, _, _, _)| observation.seq_no)
        .max();
    AudioTranscriptChunk {
        segment_id: format!("segment-{index}"),
        role: role.to_string(),
        observations: items
            .iter()
            .map(|(observation, _, _, _)| observation.clone())
            .collect(),
        source_asset_ids: items
            .iter()
            .map(|(_, asset, _, _)| asset.id.clone())
            .collect(),
        payloads: items.into_iter().map(|(_, _, bytes, _)| bytes).collect(),
        captured_at_ms,
        duration_ms,
        seq_no_start,
        seq_no_end,
    }
}

pub(crate) fn split_pcm_wav_payload(
    payload: &[u8],
    target_chunk_seconds: u64,
    max_chunk_bytes: u64,
) -> Result<Vec<Vec<u8>>> {
    let wav = parse_pcm_wav(payload)?;
    let byte_rate = u32::from_le_bytes(
        wav.format
            .get(8..12)
            .context("WAV fmt missing byte rate")?
            .try_into()?,
    ) as usize;
    let block_align = u16::from_le_bytes(
        wav.format
            .get(12..14)
            .context("WAV fmt missing block align")?
            .try_into()?,
    ) as usize;
    anyhow::ensure!(byte_rate > 0, "WAV byte rate must be positive");
    anyhow::ensure!(block_align > 0, "WAV block align must be positive");

    let header_bytes = 20usize
        .checked_add(wav.format.len())
        .and_then(|value| value.checked_add(8))
        .context("WAV header length overflow")?;
    let size_limit = (max_chunk_bytes as usize)
        .saturating_sub(header_bytes)
        .max(block_align);
    let duration_limit = byte_rate
        .saturating_mul(target_chunk_seconds as usize)
        .max(block_align);
    let mut data_limit = size_limit.min(duration_limit);
    data_limit -= data_limit % block_align;
    data_limit = data_limit.max(block_align);
    if wav.data.len() <= data_limit {
        return Ok(vec![payload.to_vec()]);
    }

    let mut chunks = Vec::new();
    let mut offset = 0usize;
    while offset < wav.data.len() {
        let mut end = offset.saturating_add(data_limit).min(wav.data.len());
        if end < wav.data.len() {
            end -= (end - offset) % block_align;
            if end == offset {
                end = offset.saturating_add(block_align).min(wav.data.len());
            }
        }
        chunks.push(write_pcm_wav_payload(wav.format, &wav.data[offset..end])?);
        offset = end;
    }
    Ok(chunks)
}

pub(crate) fn observation_role(observation: &ObservationView) -> Option<&str> {
    observation
        .metadata
        .get("role")
        .and_then(|value| value.as_str())
}

pub(crate) fn is_audio_observation(observation: &ObservationView) -> bool {
    observation.media_type.starts_with("audio/")
        || matches!(
            observation_role(observation),
            Some("microphone" | "conversation")
        )
}

/// Concatenates simple PCM WAV payloads emitted by Aurora.
pub(crate) fn concatenate_pcm_wav_payloads(payloads: &[Vec<u8>]) -> Result<Vec<u8>> {
    anyhow::ensure!(
        !payloads.is_empty(),
        "cannot concatenate empty WAV payload list"
    );
    let mut parsed = Vec::with_capacity(payloads.len());
    for payload in payloads {
        parsed.push(parse_pcm_wav(payload)?);
    }
    let first = &parsed[0];
    for wav in &parsed[1..] {
        anyhow::ensure!(
            wav.format == first.format,
            "cannot concatenate WAV payloads with different PCM formats"
        );
    }
    let data_len = parsed
        .iter()
        .map(|wav| wav.data.len())
        .try_fold(0usize, |acc, len| acc.checked_add(len))
        .context("concatenated WAV payload is too large")?;
    let riff_size = 36usize
        .checked_add(data_len)
        .context("concatenated WAV payload is too large")?;
    anyhow::ensure!(
        riff_size <= u32::MAX as usize,
        "concatenated WAV payload is too large"
    );
    anyhow::ensure!(
        data_len <= u32::MAX as usize,
        "concatenated WAV data is too large"
    );

    write_pcm_wav_payload(
        first.format,
        &parsed
            .iter()
            .flat_map(|wav| wav.data)
            .copied()
            .collect::<Vec<_>>(),
    )
}

fn write_pcm_wav_payload(format: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let riff_size = 20usize
        .checked_add(format.len())
        .and_then(|value| value.checked_add(data.len()))
        .context("WAV payload is too large")?;
    anyhow::ensure!(riff_size <= u32::MAX as usize, "WAV payload is too large");
    anyhow::ensure!(data.len() <= u32::MAX as usize, "WAV data is too large");

    let mut output = Vec::with_capacity(20 + format.len() + 8 + data.len());
    output.extend_from_slice(b"RIFF");
    output.extend_from_slice(&(riff_size as u32).to_le_bytes());
    output.extend_from_slice(b"WAVE");
    output.extend_from_slice(b"fmt ");
    output.extend_from_slice(&(format.len() as u32).to_le_bytes());
    output.extend_from_slice(format);
    output.extend_from_slice(b"data");
    output.extend_from_slice(&(data.len() as u32).to_le_bytes());
    output.extend_from_slice(data);
    Ok(output)
}

pub(crate) fn wav_duration_ms(payload: &[u8]) -> Option<u64> {
    let wav = parse_pcm_wav(payload).ok()?;
    let byte_rate = u32::from_le_bytes(wav.format.get(8..12)?.try_into().ok()?) as u64;
    if byte_rate == 0 {
        return None;
    }
    Some((wav.data.len() as u64).saturating_mul(1000) / byte_rate)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedPcmWav<'a> {
    format: &'a [u8],
    data: &'a [u8],
}

fn parse_pcm_wav(payload: &[u8]) -> Result<ParsedPcmWav<'_>> {
    anyhow::ensure!(payload.len() >= 44, "WAV payload is too short");
    anyhow::ensure!(&payload[0..4] == b"RIFF", "WAV payload missing RIFF header");
    anyhow::ensure!(
        &payload[8..12] == b"WAVE",
        "WAV payload missing WAVE header"
    );
    let mut cursor = 12usize;
    let mut format = None;
    let mut data = None;
    while cursor.saturating_add(8) <= payload.len() {
        let chunk_id = &payload[cursor..cursor + 4];
        let chunk_len = u32::from_le_bytes(payload[cursor + 4..cursor + 8].try_into()?) as usize;
        cursor += 8;
        let end = cursor
            .checked_add(chunk_len)
            .context("WAV chunk length overflow")?;
        anyhow::ensure!(end <= payload.len(), "WAV chunk exceeds payload length");
        match chunk_id {
            b"fmt " => format = Some(&payload[cursor..end]),
            b"data" => data = Some(&payload[cursor..end]),
            _ => {}
        }
        cursor = end + (chunk_len % 2);
    }
    let format = format.context("WAV payload missing fmt chunk")?;
    let data = data.context("WAV payload missing data chunk")?;
    anyhow::ensure!(format.len() >= 16, "WAV fmt chunk is too short");
    let audio_format = u16::from_le_bytes(format[0..2].try_into()?);
    let bits_per_sample = u16::from_le_bytes(format[14..16].try_into()?);
    anyhow::ensure!(
        audio_format == 1,
        "only PCM WAV payloads can be concatenated"
    );
    anyhow::ensure!(
        bits_per_sample == 16,
        "only 16-bit PCM WAV payloads can be concatenated"
    );
    Ok(ParsedPcmWav { format, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(
        id: &str,
        asset_id: &str,
        media_type: &str,
        role: &str,
        captured_at_ms: u64,
    ) -> ObservationView {
        ObservationView {
            observation_id: id.to_string(),
            source_id: "source".to_string(),
            kind: crate::observations::ObservationSourceKind::MicrophoneSegment,
            sensitivity: crate::observations::ObservationSensitivity::Standard,
            retention_state: ObservationRetentionState::Active,
            asset_id: asset_id.to_string(),
            canonical_text_asset_id: None,
            media_type: media_type.to_string(),
            sha256: "sha".to_string(),
            byte_length: 0,
            captured_at_ms,
            received_at_ms: captured_at_ms,
            stream_id: None,
            seq_no: Some(captured_at_ms / 1000),
            idempotency_key: id.to_string(),
            request_fingerprint: format!("{id}-fingerprint"),
            metadata: serde_json::json!({ "capture_group_id": "group", "role": role }),
        }
    }

    fn asset(id: &str, media_type: &str, byte_length: u64) -> StoredAssetRecord {
        StoredAssetRecord {
            id: id.to_string(),
            media_type: media_type.to_string(),
            file_name: format!("{id}.bin"),
            sha256: "sha".to_string(),
            byte_length,
            uri: format!("file:///tmp/{id}"),
            text_uri: None,
            text_sha256: None,
            text_byte_length: None,
            preview_image_uri: None,
            preview_image_media_type: None,
            preview_image_sha256: None,
            preview_image_byte_length: None,
            derivation_ids: Vec::new(),
            provenance: Vec::new(),
            created_at_ms: 1,
        }
    }

    fn wav(samples: usize, sample_rate: u32, channels: u16) -> Vec<u8> {
        let bytes_per_sample = 2u16;
        let block_align = channels * bytes_per_sample;
        let byte_rate = sample_rate * u32::from(block_align);
        let data_len = samples * usize::from(bytes_per_sample);
        let riff_size = 36 + data_len;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(riff_size as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&byte_rate.to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
        bytes.resize(bytes.len() + data_len, 0);
        bytes
    }

    #[test]
    fn concatenate_pcm_wav_payloads_preserves_format_and_combines_data() -> Result<()> {
        let first = wav(16_000, 16_000, 1);
        let second = wav(8_000, 16_000, 1);
        let combined = concatenate_pcm_wav_payloads(&[first, second])?;
        let parsed = parse_pcm_wav(&combined)?;
        assert_eq!(parsed.data.len(), 48_000);
        assert_eq!(wav_duration_ms(&combined), Some(1_500));
        Ok(())
    }

    #[test]
    fn transcript_request_rejects_invalid_time_window() {
        let request = ObservationTranscriptCreateRequest {
            idempotency_key: "key".to_string(),
            selection: ObservationTranscriptSelection {
                capture_group_id: "group".to_string(),
                recording_id: None,
                after_ms: Some(2),
                before_ms: Some(1),
                roles: Vec::new(),
                include_purged: true,
            },
            transcription: ObservationTranscriptTranscriptionOptions::default(),
            metadata: Value::Null,
        };
        assert!(request.validate().is_err());
    }

    #[test]
    fn audio_chunks_keep_non_concat_formats_as_single_segments() -> Result<()> {
        let first_wav = wav(16_000, 16_000, 1);
        let webm = b"webm-payload".to_vec();
        let second_wav = wav(16_000, 16_000, 1);
        let chunks = build_audio_transcript_chunks(
            vec![
                (
                    observation("obs-1", "asset-1", "audio/wav", "microphone", 1_000),
                    asset("asset-1", "audio/wav", first_wav.len() as u64),
                    first_wav,
                ),
                (
                    observation("obs-2", "asset-2", "audio/webm", "microphone", 2_000),
                    asset("asset-2", "audio/webm", webm.len() as u64),
                    webm,
                ),
                (
                    observation("obs-3", "asset-3", "audio/wav", "microphone", 3_000),
                    asset("asset-3", "audio/wav", second_wav.len() as u64),
                    second_wav,
                ),
            ],
            60,
            DEFAULT_TRANSCRIPT_CHUNK_MAX_BYTES,
        )?;

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].source_asset_ids, vec!["asset-1"]);
        assert_eq!(chunks[1].source_asset_ids, vec!["asset-2"]);
        assert_eq!(chunks[2].source_asset_ids, vec!["asset-3"]);
        Ok(())
    }

    #[test]
    fn split_pcm_wav_payload_respects_duration_window() -> Result<()> {
        let payload = wav(32_000, 16_000, 1);
        let chunks = split_pcm_wav_payload(&payload, 1, DEFAULT_TRANSCRIPT_CHUNK_MAX_BYTES)?;

        assert_eq!(chunks.len(), 2);
        assert_eq!(wav_duration_ms(&chunks[0]), Some(1_000));
        assert_eq!(wav_duration_ms(&chunks[1]), Some(1_000));
        Ok(())
    }
}
