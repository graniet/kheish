//! Batch provisioning for host-local `kheish-capture` runtimes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use kheish_session::{prepare_storage_path_for_write, write_json_pretty_atomically};
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::state_files::read_json_or_quarantine;
use crate::{CreateObservationSourceRequest, ObservationSensitivity, ObservationSourceKind};

const DEFAULT_INTERVAL_MS: u64 = 5_000;
const DEFAULT_RETENTION_SECONDS: u64 = 7 * 24 * 60 * 60;
const DEFAULT_MAX_ACTIVE_OBSERVATIONS: u64 = 256;
const DEFAULT_MAX_ACTIVE_BYTES: u64 = 128 * 1024 * 1024;
const DEFAULT_TOKEN_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const DEFAULT_HEARTBEAT_INTERVAL_MS: u64 = 30_000;
const DEFAULT_HEARTBEAT_GRACE_MS: u64 = 120_000;
const MAX_PROVISION_AGENTS: usize = 1_000;

/// The host operating-system profile used to render capture-agent config.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureOsProfile {
    /// Native macOS capture drivers.
    #[default]
    Macos,
    /// Portable Linux profile. Currently supports microphone capture.
    Linux,
    /// Portable Windows profile. Currently supports microphone capture.
    Windows,
}

impl CaptureOsProfile {
    fn as_str(self) -> &'static str {
        match self {
            Self::Macos => "macos",
            Self::Linux => "linux",
            Self::Windows => "windows",
        }
    }

    fn spool_root(self) -> &'static str {
        match self {
            Self::Macos => "~/Library/Application Support/Kheish Capture/spool",
            Self::Linux => "~/.local/state/kheish-capture/spool",
            Self::Windows => "~/AppData/Local/Kheish Capture/spool",
        }
    }

    fn metadata_schema(self) -> &'static str {
        match self {
            Self::Macos => "kheish.macos.capture.v1",
            Self::Linux => "kheish.linux.capture.v1",
            Self::Windows => "kheish.windows.capture.v1",
        }
    }

    fn supports_screen(self) -> bool {
        matches!(self, Self::Macos)
    }

    fn supports_camera(self) -> bool {
        matches!(self, Self::Macos)
    }

    fn supports_system_audio(self) -> bool {
        matches!(self, Self::Macos)
    }
}

/// Durable capture-agent lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureAgentStatus {
    /// The agent can heartbeat and upload through active leases.
    Active,
    /// The agent has been revoked and all leases should be rejected.
    Revoked,
}

/// Operator-visible heartbeat state for one capture agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureHeartbeatState {
    /// The agent was provisioned but has not heartbeated yet.
    Pending,
    /// The daemon received a heartbeat before the deadline.
    Healthy,
    /// The heartbeat deadline has passed.
    Missing,
    /// The agent was revoked.
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureAgentProvisionRequest {
    pub batch_id: String,
    pub daemon_base_url: String,
    #[serde(default)]
    pub os_profile: CaptureOsProfile,
    pub agents: Vec<CaptureAgentProvisionTarget>,
    #[serde(default)]
    pub sources: CaptureAgentProvisionSources,
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default = "default_retention_seconds")]
    pub retention_seconds: u64,
    #[serde(default = "default_max_active_observations")]
    pub max_active_observations: u64,
    #[serde(default = "default_max_active_bytes")]
    pub max_active_bytes: u64,
    #[serde(default = "default_token_ttl_ms")]
    pub token_ttl_ms: u64,
    #[serde(default = "default_heartbeat_interval_ms")]
    pub heartbeat_interval_ms: u64,
    #[serde(default = "default_heartbeat_grace_ms")]
    pub heartbeat_grace_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureAgentProvisionTarget {
    pub machine_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_profile: Option<CaptureOsProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_unique_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureAgentProvisionSources {
    #[serde(default = "default_true")]
    pub screen: bool,
    #[serde(default)]
    pub camera: bool,
    #[serde(default)]
    pub system_audio: bool,
    #[serde(default)]
    pub microphone: bool,
}

impl Default for CaptureAgentProvisionSources {
    fn default() -> Self {
        Self {
            screen: true,
            camera: false,
            system_audio: false,
            microphone: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureAgentProvisionResponse {
    pub batch_id: String,
    pub agents: Vec<CaptureAgentProvisionedAgent>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureAgentProvisionedAgent {
    pub machine_id: String,
    pub os_profile: CaptureOsProfile,
    pub heartbeat_interval_ms: u64,
    pub heartbeat_grace_ms: u64,
    pub heartbeat_deadline_ms: u64,
    pub heartbeat_token: String,
    pub token_expires_at_ms: u64,
    pub config_toml: String,
    pub sources: Vec<CaptureProvisionedSource>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureProvisionedSource {
    pub source_id: String,
    pub kind: ObservationSourceKind,
    pub lease_id: String,
    pub expires_at_ms: u64,
    pub upload_token: String,
}

pub(crate) struct CaptureAgentProvisionPlan {
    pub response: CaptureAgentProvisionResponse,
    pub source_requests: Vec<CreateObservationSourceRequest>,
    pub agent_records: Vec<CaptureAgentRecord>,
}

/// One sanitized view of a source token lease. The raw token digest is never exposed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureSourceLeaseView {
    pub lease_id: String,
    pub source_id: String,
    pub kind: ObservationSourceKind,
    pub upload_token_version: u64,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
}

/// One durable source token lease. Only the digest is persisted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CaptureSourceLeaseRecord {
    pub(crate) view: CaptureSourceLeaseView,
    pub(crate) upload_token_sha256: String,
}

/// Operator-visible capture-agent state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureAgentView {
    pub machine_id: String,
    pub batch_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision_fingerprint_sha256: Option<String>,
    pub os_profile: CaptureOsProfile,
    pub status: CaptureAgentStatus,
    pub source_ids: Vec<String>,
    pub leases: Vec<CaptureSourceLeaseView>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub heartbeat_interval_ms: u64,
    pub heartbeat_grace_ms: u64,
    pub heartbeat_deadline_ms: u64,
    #[serde(default = "default_heartbeat_token_version")]
    pub heartbeat_token_version: u64,
    pub heartbeat_state: CaptureHeartbeatState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_agent_version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_heartbeat_observed_source_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_heartbeat_unobserved_source_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_missing_since_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_expires_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_reason: Option<String>,
}

/// One persisted capture-agent record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CaptureAgentRecord {
    pub(crate) view: CaptureAgentView,
    pub(crate) leases: Vec<CaptureSourceLeaseRecord>,
    #[serde(default)]
    pub(crate) heartbeat_token_sha256: String,
}

/// Heartbeat payload accepted from a host-local capture agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureAgentHeartbeatRequest {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_source_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
}

/// Response returned after one accepted heartbeat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureAgentHeartbeatResponse {
    pub machine_id: String,
    pub status: CaptureAgentStatus,
    pub heartbeat_state: CaptureHeartbeatState,
    pub last_heartbeat_at_ms: u64,
    pub heartbeat_deadline_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_source_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unobserved_source_ids: Vec<String>,
}

/// Request used to revoke one capture agent and all of its leases.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeCaptureAgentRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Operator-visible missing-heartbeat alert.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureAgentAlertView {
    pub machine_id: String,
    pub kind: String,
    pub severity: String,
    pub message: String,
    pub detected_at_ms: u64,
    pub heartbeat_deadline_ms: u64,
}

/// Filesystem-backed capture-agent storage rooted under one daemon state directory.
#[derive(Clone, Debug)]
pub(crate) struct FileCaptureAgentStore {
    root: PathBuf,
}

impl FileCaptureAgentStore {
    /// Creates a capture-agent store under one daemon state root.
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn agents_root(&self) -> PathBuf {
        self.root.join("capture-agents")
    }

    /// Loads every persisted capture-agent record, quarantining corrupted files.
    pub(crate) fn load_agents(&self) -> Result<BTreeMap<String, CaptureAgentRecord>> {
        let mut records = BTreeMap::new();
        for root in [self.agents_root(), self.agents_root().join("__safe")] {
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
                let Some(record) =
                    read_json_or_quarantine::<CaptureAgentRecord>(&path, "capture agent")?
                else {
                    continue;
                };
                records.insert(record.view.machine_id.clone(), record);
            }
        }
        Ok(records)
    }

    /// Persists one capture-agent record atomically.
    pub(crate) fn save_agent(&self, record: &CaptureAgentRecord) -> Result<()> {
        let path =
            prepare_storage_path_for_write(&self.agents_root(), &record.view.machine_id, "json")?;
        write_json_pretty_atomically(&path, record)
    }

    /// Deletes one capture-agent record if it exists.
    pub(crate) fn delete_agent(&self, machine_id: &str) -> Result<()> {
        let path = prepare_storage_path_for_write(&self.agents_root(), machine_id, "json")?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("failed to delete capture agent {}", path.display())),
        }
    }
}

impl CaptureAgentProvisionRequest {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure_identifier(&self.batch_id, "batch_id")?;
        let daemon_url = reqwest::Url::parse(&self.daemon_base_url)
            .map_err(|_| anyhow::anyhow!("daemon_base_url must be a valid http(s) URL"))?;
        anyhow::ensure!(
            matches!(daemon_url.scheme(), "http" | "https") && daemon_url.has_host(),
            "daemon_base_url must be a valid http(s) URL with a host"
        );
        anyhow::ensure!(!self.agents.is_empty(), "agents must not be empty");
        anyhow::ensure!(
            self.agents.len() <= MAX_PROVISION_AGENTS,
            "agents cannot contain more than {MAX_PROVISION_AGENTS} entries"
        );
        anyhow::ensure!(
            self.sources.screen
                || self.sources.camera
                || self.sources.system_audio
                || self.sources.microphone,
            "at least one source kind must be enabled"
        );
        anyhow::ensure!(
            self.interval_ms > 0,
            "interval_ms must be greater than zero"
        );
        if let Some(max_runs) = self.max_runs {
            anyhow::ensure!(max_runs > 0, "max_runs must be greater than zero");
        }
        if let Some(duration_ms) = self.duration_ms {
            anyhow::ensure!(duration_ms > 0, "duration_ms must be greater than zero");
        }
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
            self.token_ttl_ms > 0,
            "token_ttl_ms must be greater than zero"
        );
        anyhow::ensure!(
            self.heartbeat_interval_ms > 0,
            "heartbeat_interval_ms must be greater than zero"
        );
        anyhow::ensure!(
            self.heartbeat_grace_ms > 0,
            "heartbeat_grace_ms must be greater than zero"
        );
        let mut machine_ids = BTreeSet::new();
        for agent in &self.agents {
            ensure_identifier(&agent.machine_id, "machine_id")?;
            let os_profile = agent.os_profile.unwrap_or(self.os_profile);
            ensure_sources_supported(os_profile, &self.sources)?;
            let sanitized_machine_id = sanitize_identifier(&agent.machine_id)?;
            anyhow::ensure!(
                machine_ids.insert(sanitized_machine_id.clone()),
                "duplicate machine_id after normalization: {sanitized_machine_id}"
            );
            if agent.camera_unique_id.is_some() && agent.camera_name.is_some() {
                bail!(
                    "agent {} cannot set both camera_unique_id and camera_name",
                    agent.machine_id
                );
            }
            if self.sources.camera
                && agent.camera_unique_id.as_deref().is_none_or(str::is_empty)
                && agent.camera_name.as_deref().is_none_or(str::is_empty)
            {
                bail!(
                    "agent {} requires camera_unique_id or camera_name when camera source is enabled",
                    agent.machine_id
                );
            }
        }
        Ok(())
    }
}

pub(crate) fn build_capture_agent_provision_plan(
    request: CaptureAgentProvisionRequest,
    issued_at_ms: u64,
) -> Result<CaptureAgentProvisionPlan> {
    request.validate()?;
    let provision_fingerprint_sha256 = capture_agent_provision_request_fingerprint(&request)?;
    let token_expires_at_ms = issued_at_ms.saturating_add(request.token_ttl_ms);
    let mut source_requests = Vec::new();
    let mut agents = Vec::new();
    let mut agent_records = Vec::new();
    for agent in &request.agents {
        let machine_id = sanitize_identifier(&agent.machine_id)?;
        let os_profile = agent.os_profile.unwrap_or(request.os_profile);
        let profile_name = os_profile.as_str();
        let prefix = format!("{profile_name}-{machine_id}");
        let capture_group_id = format!("{machine_id}-capture");
        let heartbeat_token = random_token();
        let mut provisioned_sources = Vec::new();
        let mut config_sources = Vec::new();
        let mut leases = Vec::new();
        let heartbeat_deadline_ms = issued_at_ms
            .saturating_add(request.heartbeat_interval_ms)
            .saturating_add(request.heartbeat_grace_ms);

        if request.sources.screen {
            let token = random_token();
            let source_id = format!("{prefix}-screen");
            let lease_id = random_lease_id(&source_id);
            source_requests.push(source_request(
                &source_id,
                &format!("{machine_id} Main Display"),
                ObservationSourceKind::ScreenSnapshot,
                &token,
                &request,
            ));
            provisioned_sources.push(CaptureProvisionedSource {
                source_id: source_id.clone(),
                kind: ObservationSourceKind::ScreenSnapshot,
                lease_id: lease_id.clone(),
                expires_at_ms: token_expires_at_ms,
                upload_token: token.clone(),
            });
            leases.push(source_lease(
                &lease_id,
                &source_id,
                ObservationSourceKind::ScreenSnapshot,
                &token,
                issued_at_ms,
                token_expires_at_ms,
            ));
            config_sources.push(capture_source(
                &source_id,
                &format!("{machine_id} Main Display"),
                "screen_snapshot",
                "screen:display:display-main",
                &token,
                trigger(&request),
                json!({
                    "driver": "macos_screen_snapshot",
                    "target": { "target": "main_display" },
                    "output_format": "png",
                    "file_name_prefix": format!("{machine_id}-screen"),
                    "metadata": metadata(os_profile, &machine_id, &capture_group_id, "screenshot"),
                }),
                &request,
            ));
        }

        if request.sources.camera {
            let token = random_token();
            let source_id = format!("{prefix}-camera");
            let lease_id = random_lease_id(&source_id);
            source_requests.push(source_request(
                &source_id,
                &format!("{machine_id} External Webcam"),
                ObservationSourceKind::WebcamSnapshot,
                &token,
                &request,
            ));
            provisioned_sources.push(CaptureProvisionedSource {
                source_id: source_id.clone(),
                kind: ObservationSourceKind::WebcamSnapshot,
                lease_id: lease_id.clone(),
                expires_at_ms: token_expires_at_ms,
                upload_token: token.clone(),
            });
            leases.push(source_lease(
                &lease_id,
                &source_id,
                ObservationSourceKind::WebcamSnapshot,
                &token,
                issued_at_ms,
                token_expires_at_ms,
            ));
            let target = match (&agent.camera_unique_id, &agent.camera_name) {
                (Some(unique_id), None) => json!({"target": "unique_id", "unique_id": unique_id}),
                (None, Some(name)) => json!({"target": "name", "name": name}),
                _ => unreachable!("validated above"),
            };
            config_sources.push(capture_source(
                &source_id,
                &format!("{machine_id} External Webcam"),
                "webcam_snapshot",
                "camera:external:primary",
                &token,
                trigger(&request),
                json!({
                    "driver": "macos_camera_snapshot",
                    "target": target,
                    "output_format": "jpeg",
                    "jpeg_quality": 92,
                    "external_only": true,
                    "reject_virtual": true,
                    "file_name_prefix": format!("{machine_id}-camera"),
                    "metadata": metadata(os_profile, &machine_id, &capture_group_id, "webcam"),
                }),
                &request,
            ));
        }

        if request.sources.system_audio || request.sources.microphone {
            let token = random_token();
            let source_id = format!("{prefix}-call-audio");
            let lease_id = random_lease_id(&source_id);
            source_requests.push(source_request(
                &source_id,
                &format!("{machine_id} Call Audio"),
                ObservationSourceKind::MicrophoneSegment,
                &token,
                &request,
            ));
            provisioned_sources.push(CaptureProvisionedSource {
                source_id: source_id.clone(),
                kind: ObservationSourceKind::MicrophoneSegment,
                lease_id: lease_id.clone(),
                expires_at_ms: token_expires_at_ms,
                upload_token: token.clone(),
            });
            leases.push(source_lease(
                &lease_id,
                &source_id,
                ObservationSourceKind::MicrophoneSegment,
                &token,
                issued_at_ms,
                token_expires_at_ms,
            ));
            if request.sources.system_audio {
                config_sources.push(capture_source(
                    &source_id,
                    &format!("{machine_id} Call Audio"),
                    "microphone_segment",
                    &format!("call:{capture_group_id}:conversation"),
                    &token,
                    trigger(&request),
                    json!({
                        "driver": "macos_system_audio",
                        "segment_duration_ms": request.interval_ms,
                        "sample_rate_hz": 48000,
                        "channel_count": 2,
                        "buffer_capacity_ms": request.interval_ms.saturating_mul(2).max(request.interval_ms),
                        "exclude_current_process": true,
                        "file_name_prefix": format!("{machine_id}-conversation"),
                        "metadata": metadata(os_profile, &machine_id, &capture_group_id, "conversation"),
                    }),
                    &request,
                ));
            }
            if request.sources.microphone {
                let mut driver = json!({
                    "driver": "microphone",
                    "segment_duration_ms": request.interval_ms,
                    "sample_rate_hz": 48000,
                    "channel_count": 1,
                    "file_name_prefix": format!("{machine_id}-microphone"),
                    "metadata": metadata(os_profile, &machine_id, &capture_group_id, "microphone"),
                });
                if let Some(microphone_name) = agent.microphone_name.as_deref() {
                    driver["device_name"] = json!(microphone_name);
                }
                config_sources.push(capture_source(
                    &source_id,
                    &format!("{machine_id} Call Audio"),
                    "microphone_segment",
                    &format!("call:{capture_group_id}:microphone"),
                    &token,
                    trigger(&request),
                    driver,
                    &request,
                ));
            }
        }

        let config = CaptureConfigToml {
            capture_agent: CaptureAgentToml {
                machine_id: machine_id.clone(),
                batch_id: request.batch_id.clone(),
                os_profile: os_profile.as_str().to_string(),
                heartbeat_url: format!(
                    "{}/v1/capture-agents/{machine_id}/heartbeat",
                    request.daemon_base_url.trim_end_matches('/')
                ),
                heartbeat_token: heartbeat_token.clone(),
                heartbeat_interval_ms: request.heartbeat_interval_ms,
                heartbeat_grace_ms: request.heartbeat_grace_ms,
                heartbeat_deadline_ms,
                token_expires_at_ms,
            },
            daemon: CaptureDaemonToml {
                base_url: request.daemon_base_url.clone(),
                request_timeout_ms: 30_000,
                user_agent: format!("kheish-capture-agent/{machine_id}"),
            },
            spool: CaptureSpoolToml {
                root_dir: os_profile.spool_root().to_string(),
                max_pending_items: 2_048,
                max_pending_bytes: 512 * 1024 * 1024,
                upload_batch_size: 8,
                retry_initial_delay_ms: 500,
                retry_max_delay_ms: 30_000,
            },
            sources: config_sources,
        };
        let source_ids = leases
            .iter()
            .map(|lease| lease.view.source_id.clone())
            .collect::<Vec<_>>();
        agent_records.push(CaptureAgentRecord {
            view: CaptureAgentView {
                machine_id: machine_id.clone(),
                batch_id: request.batch_id.clone(),
                provision_fingerprint_sha256: Some(provision_fingerprint_sha256.clone()),
                os_profile,
                status: CaptureAgentStatus::Active,
                source_ids,
                leases: leases.iter().map(|lease| lease.view.clone()).collect(),
                created_at_ms: issued_at_ms,
                updated_at_ms: issued_at_ms,
                heartbeat_interval_ms: request.heartbeat_interval_ms,
                heartbeat_grace_ms: request.heartbeat_grace_ms,
                heartbeat_deadline_ms,
                heartbeat_token_version: 1,
                heartbeat_state: CaptureHeartbeatState::Pending,
                last_heartbeat_at_ms: None,
                last_heartbeat_agent_version: None,
                last_heartbeat_observed_source_ids: Vec::new(),
                last_heartbeat_unobserved_source_ids: Vec::new(),
                heartbeat_missing_since_ms: None,
                token_expires_at_ms: Some(token_expires_at_ms),
                revoked_at_ms: None,
                revoked_reason: None,
            },
            leases,
            heartbeat_token_sha256: hex::encode(Sha256::digest(heartbeat_token.trim().as_bytes())),
        });
        agents.push(CaptureAgentProvisionedAgent {
            machine_id,
            os_profile,
            heartbeat_interval_ms: request.heartbeat_interval_ms,
            heartbeat_grace_ms: request.heartbeat_grace_ms,
            heartbeat_deadline_ms,
            heartbeat_token,
            token_expires_at_ms,
            config_toml: toml::to_string_pretty(&config)?,
            sources: provisioned_sources,
        });
    }
    Ok(CaptureAgentProvisionPlan {
        response: CaptureAgentProvisionResponse {
            batch_id: request.batch_id,
            agents,
        },
        source_requests,
        agent_records,
    })
}

pub(crate) fn capture_agent_provision_request_fingerprint(
    request: &CaptureAgentProvisionRequest,
) -> Result<String> {
    request.validate()?;
    let mut agents = Vec::with_capacity(request.agents.len());
    for agent in &request.agents {
        agents.push(json!({
            "machine_id": sanitize_identifier(&agent.machine_id)?,
            "os_profile": agent.os_profile.unwrap_or(request.os_profile).as_str(),
            "camera_unique_id": normalized_optional(&agent.camera_unique_id),
            "camera_name": normalized_optional(&agent.camera_name),
            "microphone_name": normalized_optional(&agent.microphone_name),
        }));
    }
    agents.sort_by(|left, right| {
        left["machine_id"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["machine_id"].as_str().unwrap_or_default())
    });
    let canonical = json!({
        "batch_id": sanitize_identifier(&request.batch_id)?,
        "daemon_base_url": request.daemon_base_url.trim().trim_end_matches('/'),
        "os_profile": request.os_profile.as_str(),
        "agents": agents,
        "sources": {
            "screen": request.sources.screen,
            "camera": request.sources.camera,
            "system_audio": request.sources.system_audio,
            "microphone": request.sources.microphone,
        },
        "interval_ms": request.interval_ms,
        "max_runs": request.max_runs,
        "duration_ms": request.duration_ms,
        "retention_seconds": request.retention_seconds,
        "max_active_observations": request.max_active_observations,
        "max_active_bytes": request.max_active_bytes,
        "token_ttl_ms": request.token_ttl_ms,
        "heartbeat_interval_ms": request.heartbeat_interval_ms,
        "heartbeat_grace_ms": request.heartbeat_grace_ms,
    });
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&canonical)?)))
}

#[derive(Serialize)]
struct CaptureConfigToml {
    capture_agent: CaptureAgentToml,
    daemon: CaptureDaemonToml,
    spool: CaptureSpoolToml,
    sources: Vec<CaptureSourceToml>,
}

#[derive(Serialize)]
struct CaptureAgentToml {
    machine_id: String,
    batch_id: String,
    os_profile: String,
    heartbeat_url: String,
    heartbeat_token: String,
    heartbeat_interval_ms: u64,
    heartbeat_grace_ms: u64,
    heartbeat_deadline_ms: u64,
    token_expires_at_ms: u64,
}

#[derive(Serialize)]
struct CaptureDaemonToml {
    base_url: String,
    request_timeout_ms: u64,
    user_agent: String,
}

#[derive(Serialize)]
struct CaptureSpoolToml {
    root_dir: String,
    max_pending_items: u64,
    max_pending_bytes: u64,
    upload_batch_size: u64,
    retry_initial_delay_ms: u64,
    retry_max_delay_ms: u64,
}

#[derive(Serialize)]
struct CaptureSourceToml {
    source_id: String,
    display_name: String,
    kind: String,
    enabled: bool,
    stream_id: String,
    sensitivity: String,
    trigger: serde_json::Value,
    remote: CaptureRemoteToml,
    driver: serde_json::Value,
}

#[derive(Serialize)]
struct CaptureRemoteToml {
    ensure_exists: bool,
    upload_token: String,
    retention_seconds: u64,
    max_active_observations: u64,
    max_active_bytes: u64,
    allow_materialization: bool,
    allow_output_delivery: bool,
}

fn capture_source(
    source_id: &str,
    display_name: &str,
    kind: &str,
    stream_id: &str,
    token: &str,
    trigger: serde_json::Value,
    driver: serde_json::Value,
    request: &CaptureAgentProvisionRequest,
) -> CaptureSourceToml {
    CaptureSourceToml {
        source_id: source_id.to_string(),
        display_name: display_name.to_string(),
        kind: kind.to_string(),
        enabled: true,
        stream_id: stream_id.to_string(),
        sensitivity: "sensitive".to_string(),
        trigger,
        remote: CaptureRemoteToml {
            ensure_exists: false,
            upload_token: token.to_string(),
            retention_seconds: request.retention_seconds,
            max_active_observations: request.max_active_observations,
            max_active_bytes: request.max_active_bytes,
            allow_materialization: true,
            allow_output_delivery: false,
        },
        driver,
    }
}

fn source_request(
    source_id: &str,
    display_name: &str,
    kind: ObservationSourceKind,
    upload_token: &str,
    request: &CaptureAgentProvisionRequest,
) -> CreateObservationSourceRequest {
    CreateObservationSourceRequest {
        source_id: Some(source_id.to_string()),
        display_name: display_name.to_string(),
        kind,
        upload_token: upload_token.to_string(),
        sensitivity: ObservationSensitivity::Sensitive,
        retention_seconds: request.retention_seconds,
        max_active_observations: request.max_active_observations,
        max_active_bytes: request.max_active_bytes,
        ingest_rate_limit_window_ms: 60_000,
        ingest_rate_limit_burst: 120,
        purge_raw_on_retention: false,
        allow_materialization: true,
        allow_output_delivery: false,
    }
}

fn trigger(request: &CaptureAgentProvisionRequest) -> serde_json::Value {
    let mut value = json!({
        "mode": "interval",
        "every_ms": request.interval_ms,
    });
    if let Some(max_runs) = request.max_runs {
        value["max_runs"] = json!(max_runs);
    }
    if let Some(duration_ms) = request.duration_ms {
        value["duration_ms"] = json!(duration_ms);
    }
    value
}

fn metadata(
    os_profile: CaptureOsProfile,
    machine_id: &str,
    capture_group_id: &str,
    role: &str,
) -> serde_json::Value {
    json!({
        "schema": os_profile.metadata_schema(),
        "machine_id": machine_id,
        "os_profile": os_profile.as_str(),
        "capture_group_id": capture_group_id,
        "capture_group_kind": "call_context",
        "role": role,
    })
}

fn ensure_sources_supported(
    os_profile: CaptureOsProfile,
    sources: &CaptureAgentProvisionSources,
) -> Result<()> {
    if sources.screen && !os_profile.supports_screen() {
        bail!(
            "os_profile {} does not support screen capture provisioning",
            os_profile.as_str()
        );
    }
    if sources.camera && !os_profile.supports_camera() {
        bail!(
            "os_profile {} does not support camera capture provisioning",
            os_profile.as_str()
        );
    }
    if sources.system_audio && !os_profile.supports_system_audio() {
        bail!(
            "os_profile {} does not support system audio capture provisioning",
            os_profile.as_str()
        );
    }
    Ok(())
}

fn source_lease(
    lease_id: &str,
    source_id: &str,
    kind: ObservationSourceKind,
    upload_token: &str,
    issued_at_ms: u64,
    expires_at_ms: u64,
) -> CaptureSourceLeaseRecord {
    CaptureSourceLeaseRecord {
        view: CaptureSourceLeaseView {
            lease_id: lease_id.to_string(),
            source_id: source_id.to_string(),
            kind,
            upload_token_version: 1,
            issued_at_ms,
            expires_at_ms,
            revoked_at_ms: None,
            superseded_by: None,
        },
        upload_token_sha256: hex::encode(Sha256::digest(upload_token.trim().as_bytes())),
    }
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    format!("khc_{}", hex::encode(bytes))
}

fn random_lease_id(source_id: &str) -> String {
    let mut bytes = [0u8; 8];
    OsRng.fill_bytes(&mut bytes);
    format!("lease-{}-{}", source_id, hex::encode(bytes))
}

fn ensure_identifier(value: &str, label: &str) -> Result<()> {
    let trimmed = value.trim();
    anyhow::ensure!(!trimmed.is_empty(), "{label} must not be empty");
    for ch in trimmed.chars() {
        anyhow::ensure!(
            ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') || ch.is_whitespace(),
            "{label} may contain only ASCII letters, digits, dashes, underscores, dots, or whitespace"
        );
    }
    let sanitized = sanitize_identifier(trimmed)?;
    anyhow::ensure!(
        !sanitized.is_empty(),
        "{label} must contain an ASCII letter or digit"
    );
    Ok(())
}

fn sanitize_identifier(value: &str) -> Result<String> {
    let mut output = String::new();
    let mut previous_dash = false;
    for ch in value.trim().chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else if matches!(ch, '-' | '_' | '.') || ch.is_whitespace() {
            Some('-')
        } else {
            None
        };
        if let Some(ch) = mapped {
            if ch == '-' {
                if previous_dash {
                    continue;
                }
                previous_dash = true;
            } else {
                previous_dash = false;
            }
            output.push(ch);
        }
    }
    let output = output.trim_matches('-').to_string();
    if output.is_empty() {
        bail!("identifier must contain an ASCII letter or digit");
    }
    Ok(output)
}

fn normalized_optional(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn default_true() -> bool {
    true
}

fn default_interval_ms() -> u64 {
    DEFAULT_INTERVAL_MS
}

fn default_retention_seconds() -> u64 {
    DEFAULT_RETENTION_SECONDS
}

fn default_max_active_observations() -> u64 {
    DEFAULT_MAX_ACTIVE_OBSERVATIONS
}

fn default_max_active_bytes() -> u64 {
    DEFAULT_MAX_ACTIVE_BYTES
}

fn default_token_ttl_ms() -> u64 {
    DEFAULT_TOKEN_TTL_MS
}

fn default_heartbeat_interval_ms() -> u64 {
    DEFAULT_HEARTBEAT_INTERVAL_MS
}

fn default_heartbeat_grace_ms() -> u64 {
    DEFAULT_HEARTBEAT_GRACE_MS
}

fn default_heartbeat_token_version() -> u64 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provision_plan_creates_runtime_only_config_for_fleet() -> Result<()> {
        let request = CaptureAgentProvisionRequest {
            batch_id: "mac-2026q2".to_string(),
            daemon_base_url: "http://127.0.0.1:4000".to_string(),
            agents: (0..50)
                .map(|index| CaptureAgentProvisionTarget {
                    machine_id: format!("mac-{index:03}"),
                    os_profile: None,
                    camera_unique_id: Some(format!("camera-{index:03}")),
                    camera_name: None,
                    microphone_name: None,
                })
                .collect(),
            sources: CaptureAgentProvisionSources {
                screen: true,
                camera: true,
                system_audio: true,
                microphone: true,
            },
            interval_ms: 5_000,
            max_runs: Some(12),
            duration_ms: Some(60_000),
            retention_seconds: DEFAULT_RETENTION_SECONDS,
            max_active_observations: DEFAULT_MAX_ACTIVE_OBSERVATIONS,
            max_active_bytes: DEFAULT_MAX_ACTIVE_BYTES,
            os_profile: CaptureOsProfile::Macos,
            token_ttl_ms: DEFAULT_TOKEN_TTL_MS,
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
            heartbeat_grace_ms: DEFAULT_HEARTBEAT_GRACE_MS,
        };
        let plan = build_capture_agent_provision_plan(request, 1_000)?;
        assert_eq!(plan.response.agents.len(), 50);
        assert_eq!(plan.source_requests.len(), 150);
        assert_eq!(plan.agent_records.len(), 50);
        assert_eq!(plan.response.agents[0].os_profile, CaptureOsProfile::Macos);
        assert_eq!(
            plan.response.agents[0].token_expires_at_ms,
            1_000 + DEFAULT_TOKEN_TTL_MS
        );
        assert!(
            !plan.response.agents[0]
                .config_toml
                .contains("admin_bearer_token")
        );
        assert!(
            plan.response.agents[0]
                .config_toml
                .contains("ensure_exists = false")
        );
        Ok(())
    }

    #[test]
    fn provision_request_rejects_machine_ids_that_collide_after_normalization() {
        let request = CaptureAgentProvisionRequest {
            batch_id: "mac-batch".to_string(),
            daemon_base_url: "http://127.0.0.1:4000".to_string(),
            agents: vec![
                CaptureAgentProvisionTarget {
                    machine_id: "Mac 001".to_string(),
                    os_profile: None,
                    camera_unique_id: None,
                    camera_name: None,
                    microphone_name: None,
                },
                CaptureAgentProvisionTarget {
                    machine_id: "mac-001".to_string(),
                    os_profile: None,
                    camera_unique_id: None,
                    camera_name: None,
                    microphone_name: None,
                },
            ],
            sources: CaptureAgentProvisionSources {
                screen: true,
                camera: false,
                system_audio: false,
                microphone: false,
            },
            interval_ms: 5_000,
            max_runs: None,
            duration_ms: None,
            retention_seconds: DEFAULT_RETENTION_SECONDS,
            max_active_observations: DEFAULT_MAX_ACTIVE_OBSERVATIONS,
            max_active_bytes: DEFAULT_MAX_ACTIVE_BYTES,
            os_profile: CaptureOsProfile::Macos,
            token_ttl_ms: DEFAULT_TOKEN_TTL_MS,
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
            heartbeat_grace_ms: DEFAULT_HEARTBEAT_GRACE_MS,
        };

        let error = request.validate().expect_err("collision must fail");
        assert!(error.to_string().contains("duplicate machine_id"));
    }

    #[test]
    fn provision_request_rejects_dropped_identifier_characters() {
        let error = ensure_identifier("mac-001!", "machine_id").expect_err("invalid char");
        assert!(error.to_string().contains("may contain only"));
    }

    #[test]
    fn provision_request_rejects_unsupported_source_for_os_profile() {
        let request = CaptureAgentProvisionRequest {
            batch_id: "linux-batch".to_string(),
            daemon_base_url: "http://127.0.0.1:4000".to_string(),
            os_profile: CaptureOsProfile::Linux,
            agents: vec![CaptureAgentProvisionTarget {
                machine_id: "linux-001".to_string(),
                os_profile: None,
                camera_unique_id: None,
                camera_name: None,
                microphone_name: None,
            }],
            sources: CaptureAgentProvisionSources {
                screen: true,
                camera: false,
                system_audio: false,
                microphone: false,
            },
            interval_ms: 5_000,
            max_runs: None,
            duration_ms: None,
            retention_seconds: DEFAULT_RETENTION_SECONDS,
            max_active_observations: DEFAULT_MAX_ACTIVE_OBSERVATIONS,
            max_active_bytes: DEFAULT_MAX_ACTIVE_BYTES,
            token_ttl_ms: DEFAULT_TOKEN_TTL_MS,
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
            heartbeat_grace_ms: DEFAULT_HEARTBEAT_GRACE_MS,
        };

        let error = request
            .validate()
            .expect_err("linux screen capture should be rejected");
        assert!(error.to_string().contains("does not support screen"));
    }

    #[test]
    fn provision_request_rejects_url_without_host() {
        let request = CaptureAgentProvisionRequest {
            batch_id: "mac-batch".to_string(),
            daemon_base_url: "http://".to_string(),
            agents: vec![CaptureAgentProvisionTarget {
                machine_id: "mac-001".to_string(),
                os_profile: None,
                camera_unique_id: None,
                camera_name: None,
                microphone_name: None,
            }],
            sources: CaptureAgentProvisionSources {
                screen: true,
                camera: false,
                system_audio: false,
                microphone: false,
            },
            interval_ms: 5_000,
            max_runs: None,
            duration_ms: None,
            retention_seconds: DEFAULT_RETENTION_SECONDS,
            max_active_observations: DEFAULT_MAX_ACTIVE_OBSERVATIONS,
            max_active_bytes: DEFAULT_MAX_ACTIVE_BYTES,
            os_profile: CaptureOsProfile::Macos,
            token_ttl_ms: DEFAULT_TOKEN_TTL_MS,
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
            heartbeat_grace_ms: DEFAULT_HEARTBEAT_GRACE_MS,
        };

        let error = request.validate().expect_err("invalid URL must fail");
        assert!(error.to_string().contains("daemon_base_url"));
    }
}
