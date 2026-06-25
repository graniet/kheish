use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm_siv::aead::{Aead, KeyInit};
use aes_gcm_siv::{Aes256GcmSiv, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use kheish_runtime::{DebugArtifact, DebugArtifactFormat, DebugCaptureLevel};
use kheish_session::{
    atomic_write, decode_safe_storage_name, legacy_storage_name, prepare_storage_dir_for_write,
    resolve_storage_dir_for_read, safe_storage_name, write_json_pretty_atomically,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

/// Environment variable containing a 32-byte debug artifact encryption key.
pub const DEBUG_CAPTURE_KEY_ENV: &str = "KHEISH_DEBUG_CAPTURE_KEY";
/// Environment variable containing a file path with the debug artifact encryption key.
pub const DEBUG_CAPTURE_KEY_FILE_ENV: &str = "KHEISH_DEBUG_CAPTURE_KEY_FILE";
/// Environment variable overriding the maximum retained plaintext bytes per artifact.
pub const DEBUG_MAX_ARTIFACT_BYTES_ENV: &str = "KHEISH_DEBUG_MAX_ARTIFACT_BYTES";
/// Environment variable overriding the maximum retained artifact-body bytes per run.
pub const DEBUG_MAX_RUN_BYTES_ENV: &str = "KHEISH_DEBUG_MAX_RUN_BYTES";
/// Environment variable overriding the maximum retained artifacts per run.
pub const DEBUG_MAX_ARTIFACTS_PER_RUN_ENV: &str = "KHEISH_DEBUG_MAX_ARTIFACTS_PER_RUN";
/// Environment variable overriding automatic debug retention TTL.
pub const DEBUG_TTL_MS_ENV: &str = "KHEISH_DEBUG_TTL_MS";
/// Environment variable setting an optional global debug-store byte cap.
pub const DEBUG_MAX_STORE_BYTES_ENV: &str = "KHEISH_DEBUG_MAX_STORE_BYTES";
/// Environment variable overriding the periodic debug retention interval.
pub const DEBUG_GC_INTERVAL_MS_ENV: &str = "KHEISH_DEBUG_GC_INTERVAL_MS";

const DEBUG_ARTIFACT_ENVELOPE_KIND: &str = "kheish_debug_artifact_envelope";
const DEBUG_ARTIFACT_ENVELOPE_VERSION: u32 = 1;
const DEBUG_ARTIFACT_NONCE_LEN: usize = 12;
const DEFAULT_MAX_DEBUG_ARTIFACT_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_MAX_DEBUG_RUN_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_MAX_DEBUG_ARTIFACTS_PER_RUN: usize = 64;
const DEFAULT_DEBUG_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const DEFAULT_DEBUG_GC_INTERVAL_MS: u64 = 60 * 60 * 1_000;
const MIN_DEBUG_ARTIFACT_BYTES: u64 = 512;

#[cfg(test)]
pub(crate) fn debug_capture_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .expect("debug capture env lock poisoned")
}

/// One stored debug artifact descriptor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugArtifactSummary {
    /// The stable artifact identifier used by the API and CLI.
    pub artifact_id: String,
    /// The artifact format on disk.
    pub format: DebugArtifactFormat,
    /// The timestamp of the last write.
    pub updated_at_ms: u64,
    /// The artifact byte length on disk.
    pub bytes: u64,
    /// The retained plaintext byte length after any storage budget truncation.
    #[serde(default)]
    pub plaintext_bytes: u64,
    /// SHA-256 checksum of the retained plaintext bytes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    /// Whether the retained plaintext is a truncated representation of a larger artifact.
    #[serde(default)]
    pub truncated: bool,
    /// The original plaintext byte length before truncation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_bytes: Option<u64>,
    /// Whether the artifact body is encrypted at rest.
    #[serde(default)]
    pub encrypted: bool,
    /// Identifier of the configured encryption key used to write the artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption_key_id: Option<String>,
}

/// Operator-visible debug capture storage and scrubber policy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugCapturePolicyView {
    pub ttl_ms: u64,
    pub gc_interval_ms: u64,
    pub max_artifact_bytes: u64,
    pub max_run_bytes: u64,
    pub max_artifacts_per_run: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_store_bytes: Option<u64>,
    pub encryption_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption_key_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption_key_error: Option<String>,
    pub redaction_literal_token_count: usize,
    pub redaction_token_file_configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redaction_token_file_error: Option<String>,
}

/// One run-scoped debug bundle summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDebugView {
    /// The owning run identifier.
    pub run_id: String,
    /// The most recent debug level used for the run.
    pub level: DebugCaptureLevel,
    /// The known artifacts for the run.
    pub artifacts: Vec<DebugArtifactSummary>,
}

/// Filesystem-backed persistence for per-run debug artifacts.
#[derive(Clone, Debug)]
pub struct FileDebugStore {
    root: PathBuf,
    policy: DebugStorePolicy,
    encryption_key: Option<[u8; 32]>,
    encryption_key_id: Option<String>,
    encryption_key_error: Option<String>,
    lock: Arc<Mutex<()>>,
}

#[derive(Clone, Debug)]
struct DebugStorePolicy {
    max_artifact_bytes: u64,
    max_run_bytes: u64,
    max_artifacts_per_run: usize,
    ttl_ms: u64,
    max_store_bytes: Option<u64>,
    gc_interval_ms: u64,
}

impl Default for DebugStorePolicy {
    fn default() -> Self {
        Self {
            max_artifact_bytes: DEFAULT_MAX_DEBUG_ARTIFACT_BYTES,
            max_run_bytes: DEFAULT_MAX_DEBUG_RUN_BYTES,
            max_artifacts_per_run: DEFAULT_MAX_DEBUG_ARTIFACTS_PER_RUN,
            ttl_ms: DEFAULT_DEBUG_TTL_MS,
            max_store_bytes: None,
            gc_interval_ms: DEFAULT_DEBUG_GC_INTERVAL_MS,
        }
    }
}

impl DebugStorePolicy {
    fn from_env() -> Self {
        let mut policy = Self::default();
        policy.max_artifact_bytes =
            read_positive_u64_env(DEBUG_MAX_ARTIFACT_BYTES_ENV, policy.max_artifact_bytes)
                .max(MIN_DEBUG_ARTIFACT_BYTES);
        policy.max_run_bytes = read_positive_u64_env(DEBUG_MAX_RUN_BYTES_ENV, policy.max_run_bytes)
            .max(MIN_DEBUG_ARTIFACT_BYTES);
        policy.max_artifacts_per_run = read_positive_usize_env(
            DEBUG_MAX_ARTIFACTS_PER_RUN_ENV,
            policy.max_artifacts_per_run,
        );
        policy.ttl_ms = read_u64_env(DEBUG_TTL_MS_ENV, policy.ttl_ms);
        policy.max_store_bytes = read_optional_positive_u64_env(DEBUG_MAX_STORE_BYTES_ENV);
        policy.gc_interval_ms =
            read_positive_u64_env(DEBUG_GC_INTERVAL_MS_ENV, policy.gc_interval_ms);
        policy
    }
}

#[derive(Clone, Debug)]
struct PreparedArtifactBody {
    plaintext: Vec<u8>,
    sha256: String,
    plaintext_bytes: u64,
    truncated: bool,
    original_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct EncryptedDebugArtifactEnvelope {
    kind: String,
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_id: Option<String>,
    nonce: String,
    ciphertext: String,
    plaintext_sha256: String,
    plaintext_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DebugStorePruneResult {
    pub candidate_run_ids: Vec<String>,
    pub candidate_debug_bytes: u64,
    pub pruned_debug_run_ids: Vec<String>,
    pub pruned_debug_bytes: u64,
}

#[derive(Clone, Debug)]
struct DebugBundleRecord {
    run_id: String,
    latest_updated_at_ms: u64,
    path: PathBuf,
    bytes: u64,
}

impl FileDebugStore {
    /// Creates a new debug store rooted at the provided directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let (encryption_key, encryption_key_error) = match load_debug_capture_key_from_env() {
            Ok(key) => (key, None),
            Err(error) => (None, Some(error.to_string())),
        };
        let encryption_key_id = encryption_key.as_ref().map(debug_capture_key_id);
        let lock = debug_store_lock_for_root(&root);
        Self {
            root,
            policy: DebugStorePolicy::from_env(),
            encryption_key,
            encryption_key_id,
            encryption_key_error,
            lock,
        }
    }

    #[cfg(test)]
    fn with_policy(root: impl Into<PathBuf>, policy: DebugStorePolicy) -> Self {
        let root = root.into();
        let (encryption_key, encryption_key_error) = match load_debug_capture_key_from_env() {
            Ok(key) => (key, None),
            Err(error) => (None, Some(error.to_string())),
        };
        let encryption_key_id = encryption_key.as_ref().map(debug_capture_key_id);
        let lock = debug_store_lock_for_root(&root);
        Self {
            root,
            policy,
            encryption_key,
            encryption_key_id,
            encryption_key_error,
            lock,
        }
    }

    /// Returns the root directory used by the debug store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the automatic debug retention TTL.
    pub fn ttl_ms(&self) -> u64 {
        self.policy.ttl_ms
    }

    /// Returns the interval for periodic debug retention.
    pub fn gc_interval_ms(&self) -> u64 {
        self.policy.gc_interval_ms
    }

    /// Returns the optional global debug-store byte cap.
    pub fn max_store_bytes(&self) -> Option<u64> {
        self.policy.max_store_bytes
    }

    /// Returns a startup configuration error for debug encryption, when present.
    pub fn encryption_key_error(&self) -> Option<&str> {
        self.encryption_key_error.as_deref()
    }

    /// Returns the effective debug capture policy used by this store.
    pub fn policy_view(&self) -> DebugCapturePolicyView {
        let redaction = kheish_runtime::debug_redaction_config_status();
        DebugCapturePolicyView {
            ttl_ms: self.policy.ttl_ms,
            gc_interval_ms: self.policy.gc_interval_ms,
            max_artifact_bytes: self.policy.max_artifact_bytes,
            max_run_bytes: self.policy.max_run_bytes,
            max_artifacts_per_run: self.policy.max_artifacts_per_run,
            max_store_bytes: self.policy.max_store_bytes,
            encryption_enabled: self.encryption_key.is_some(),
            encryption_key_id: self.encryption_key_id.clone(),
            encryption_key_error: self.encryption_key_error.clone(),
            redaction_literal_token_count: redaction.literal_token_count,
            redaction_token_file_configured: redaction.token_file_configured,
            redaction_token_file_error: redaction.token_file_error,
        }
    }

    /// Appends or replaces one debug artifact and updates the run manifest.
    pub fn append_artifact(&self, artifact: &DebugArtifact) -> Result<()> {
        let Some(run_id) = artifact.run_id.as_deref() else {
            return Ok(());
        };
        let _guard = self
            .lock
            .lock()
            .expect("debug store mutex poisoned during artifact append");
        let run_root = self.prepare_run_root_for_write(run_id)?;
        let mut view = self.load_view_unlocked(run_id)?.unwrap_or(RunDebugView {
            run_id: run_id.to_string(),
            level: artifact.level,
            artifacts: Vec::new(),
        });
        let artifact_id = artifact_id_for_append(artifact, &view);
        let path =
            self.prepare_artifact_path_for_write(&run_root, &artifact_id, artifact.format)?;
        self.ensure_parent_dir(&path)?;
        let prepared = self.prepare_body(&path, artifact)?;
        let key = self.encryption_key()?;
        let encrypted = key.is_some();
        let encryption_key_id = encrypted.then(|| self.encryption_key_id.clone()).flatten();
        let stored = match key {
            Some(key) => self.encrypt_body(&prepared, &key)?,
            None => prepared.plaintext.clone(),
        };
        atomic_write(&path, &stored)
            .with_context(|| format!("failed to write {}", path.display()))?;

        view.level = artifact.level;
        let bytes = fs::metadata(&path)
            .with_context(|| format!("failed to read {}", path.display()))?
            .len();
        if let Some(entry) = view
            .artifacts
            .iter_mut()
            .find(|entry| entry.artifact_id == artifact_id)
        {
            entry.updated_at_ms = artifact.timestamp_ms;
            entry.bytes = bytes;
            entry.format = artifact.format;
            entry.plaintext_bytes = prepared.plaintext_bytes;
            entry.sha256 = prepared.sha256.clone();
            entry.truncated = prepared.truncated;
            entry.original_bytes = prepared.original_bytes;
            entry.encrypted = encrypted;
            entry.encryption_key_id = encryption_key_id.clone();
        } else {
            view.artifacts.push(DebugArtifactSummary {
                artifact_id: artifact_id.clone(),
                format: artifact.format,
                updated_at_ms: artifact.timestamp_ms,
                bytes,
                plaintext_bytes: prepared.plaintext_bytes,
                sha256: prepared.sha256.clone(),
                truncated: prepared.truncated,
                original_bytes: prepared.original_bytes,
                encrypted,
                encryption_key_id: encryption_key_id.clone(),
            });
            view.artifacts
                .sort_by(|left, right| left.artifact_id.cmp(&right.artifact_id));
        }
        self.enforce_run_budget(&run_root, &mut view, &artifact_id)?;
        self.save_view(&view)?;
        Ok(())
    }

    /// Loads one run debug summary when present.
    pub fn load_view(&self, run_id: &str) -> Result<Option<RunDebugView>> {
        let _guard = self
            .lock
            .lock()
            .expect("debug store mutex poisoned during manifest read");
        self.load_view_unlocked(run_id)
    }

    fn load_view_unlocked(&self, run_id: &str) -> Result<Option<RunDebugView>> {
        let path = self.view_path(run_id);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&fs::read(&path)?)?))
    }

    /// Returns true when one run has a debug bundle on disk.
    pub fn has_run(&self, run_id: &str) -> bool {
        let _guard = self
            .lock
            .lock()
            .expect("debug store mutex poisoned during existence check");
        self.view_path(run_id).exists()
    }

    /// Returns the current on-disk byte size of one run debug bundle.
    pub fn run_bytes(&self, run_id: &str) -> Result<u64> {
        let _guard = self
            .lock
            .lock()
            .expect("debug store mutex poisoned during byte scan");
        directory_bytes(&self.run_root_for_read(run_id))
    }

    /// Reads one stored artifact body as UTF-8 text.
    pub fn read_artifact(&self, run_id: &str, artifact_id: &str) -> Result<String> {
        let _guard = self
            .lock
            .lock()
            .expect("debug store mutex poisoned during artifact read");
        let view = self
            .load_view_unlocked(run_id)?
            .ok_or_else(|| anyhow!("debug capture is off for run {run_id}"))?;
        let summary = view
            .artifacts
            .iter()
            .find(|entry| entry.artifact_id == artifact_id)
            .ok_or_else(|| anyhow!("unknown debug artifact {artifact_id}"))?;
        let path = self.artifact_path_for_read(
            &self.run_root_for_read(run_id),
            artifact_id,
            summary.format,
        );
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        let plaintext = self.decode_stored_body(&bytes, &path)?;
        if !summary.sha256.is_empty() {
            let digest = sha256_hex(&plaintext);
            anyhow::ensure!(
                digest == summary.sha256,
                "debug artifact checksum mismatch for {}",
                path.display()
            );
        }
        String::from_utf8(plaintext)
            .with_context(|| format!("debug artifact {artifact_id} is not UTF-8"))
    }

    /// Deletes all debug artifacts for one run and reports whether a bundle existed.
    pub fn delete_run(&self, run_id: &str) -> Result<bool> {
        let _guard = self
            .lock
            .lock()
            .expect("debug store mutex poisoned during run delete");
        let path = self.run_root_for_read(run_id);
        remove_debug_bundle_dir(&path)
    }

    /// Deletes expired terminal/orphan debug bundles while callers protect non-terminal runs.
    pub(crate) fn prune_expired_bundles(
        &self,
        protected_run_ids: &BTreeSet<String>,
        expired_known_run_ids: &BTreeSet<String>,
        retained_known_run_ids: &BTreeSet<String>,
        now_ms: u64,
    ) -> Result<DebugStorePruneResult> {
        if self.policy.ttl_ms == 0 {
            return Ok(DebugStorePruneResult::default());
        }
        let cutoff_ms = now_ms.saturating_sub(self.policy.ttl_ms);
        let _guard = self
            .lock
            .lock()
            .expect("debug store mutex poisoned during expired bundle prune");
        let candidates = self
            .bundle_records_unlocked()?
            .into_iter()
            .filter(|record| {
                !protected_run_ids.contains(&record.run_id)
                    && !retained_known_run_ids.contains(&record.run_id)
                    && (expired_known_run_ids.contains(&record.run_id)
                        || record.latest_updated_at_ms <= cutoff_ms)
            })
            .collect::<Vec<_>>();
        self.prune_bundle_records_unlocked(candidates)
    }

    /// Deletes old unprotected debug bundles until the global store budget is satisfied.
    pub(crate) fn prune_over_budget(
        &self,
        protected_run_ids: &BTreeSet<String>,
        known_terminal_retention_ms: &BTreeMap<String, u64>,
    ) -> Result<Option<DebugStorePruneResult>> {
        let Some(max_store_bytes) = self.policy.max_store_bytes else {
            return Ok(None);
        };
        if max_store_bytes == 0 {
            return Ok(None);
        }
        let _guard = self
            .lock
            .lock()
            .expect("debug store mutex poisoned during store budget prune");
        let records = self.bundle_records_unlocked()?;
        let mut total_debug_bytes = records
            .iter()
            .map(|record| record.bytes)
            .fold(0_u64, u64::saturating_add);
        let mut candidates = records
            .into_iter()
            .filter(|record| !protected_run_ids.contains(&record.run_id))
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            let left_retention_ms = known_terminal_retention_ms
                .get(&left.run_id)
                .copied()
                .unwrap_or(left.latest_updated_at_ms);
            let right_retention_ms = known_terminal_retention_ms
                .get(&right.run_id)
                .copied()
                .unwrap_or(right.latest_updated_at_ms);
            left_retention_ms
                .cmp(&right_retention_ms)
                .then_with(|| left.run_id.cmp(&right.run_id))
                .then_with(|| left.path.cmp(&right.path))
        });
        if total_debug_bytes <= max_store_bytes {
            return Ok(Some(DebugStorePruneResult {
                candidate_run_ids: unique_record_run_ids(&candidates),
                candidate_debug_bytes: sum_record_bytes(&candidates),
                pruned_debug_run_ids: Vec::new(),
                pruned_debug_bytes: 0,
            }));
        }

        let mut to_prune = Vec::new();
        for record in candidates {
            if total_debug_bytes <= max_store_bytes {
                break;
            }
            total_debug_bytes = total_debug_bytes.saturating_sub(record.bytes);
            to_prune.push(record);
        }
        Ok(Some(self.prune_bundle_records_unlocked(to_prune)?))
    }

    fn save_view(&self, view: &RunDebugView) -> Result<()> {
        let path = self
            .prepare_run_root_for_write(&view.run_id)?
            .join("manifest.json");
        self.ensure_parent_dir(&path)?;
        write_json_pretty_atomically(&path, view)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    fn bundle_records_unlocked(&self) -> Result<Vec<DebugBundleRecord>> {
        let debug_root = self.root.join("debug");
        if !debug_root.exists() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for path in debug_run_roots(&debug_root)? {
            let manifest_path = path.join("manifest.json");
            let bytes = directory_bytes(&path)?;
            let record = match fs::read(&manifest_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<RunDebugView>(&bytes).ok())
            {
                Some(view) => DebugBundleRecord {
                    latest_updated_at_ms: view
                        .artifacts
                        .iter()
                        .map(|artifact| artifact.updated_at_ms)
                        .max()
                        .unwrap_or_else(|| modified_time_ms(&manifest_path).unwrap_or(0)),
                    run_id: view.run_id,
                    path,
                    bytes,
                },
                None => DebugBundleRecord {
                    run_id: debug_bundle_run_id_from_path(&debug_root, &path),
                    latest_updated_at_ms: directory_latest_modified_time_ms(&path).unwrap_or(0),
                    path,
                    bytes,
                },
            };
            records.push(record);
        }
        records.sort_by(|left, right| {
            left.latest_updated_at_ms
                .cmp(&right.latest_updated_at_ms)
                .then_with(|| left.run_id.cmp(&right.run_id))
                .then_with(|| left.path.cmp(&right.path))
        });
        Ok(records)
    }

    fn prune_bundle_records_unlocked(
        &self,
        records: Vec<DebugBundleRecord>,
    ) -> Result<DebugStorePruneResult> {
        let candidate_run_ids = unique_record_run_ids(&records);
        let candidate_debug_bytes = sum_record_bytes(&records);
        let mut pruned_records = Vec::new();
        let mut pruned_debug_bytes = 0_u64;
        for record in records {
            if remove_debug_bundle_dir(&record.path)? {
                pruned_debug_bytes = pruned_debug_bytes.saturating_add(record.bytes);
                pruned_records.push(record);
            }
        }
        Ok(DebugStorePruneResult {
            candidate_run_ids,
            candidate_debug_bytes,
            pruned_debug_run_ids: unique_record_run_ids(&pruned_records),
            pruned_debug_bytes,
        })
    }

    fn view_path(&self, run_id: &str) -> PathBuf {
        self.run_root_for_read(run_id).join("manifest.json")
    }

    fn artifact_path_for_read(
        &self,
        run_root: &Path,
        artifact_id: &str,
        format: DebugArtifactFormat,
    ) -> PathBuf {
        let extension = match format {
            DebugArtifactFormat::Json => "json",
            DebugArtifactFormat::JsonLines => "jsonl",
        };
        let safe = run_root.join("artifacts").join(format!(
            "{}.{}",
            safe_storage_name(artifact_id),
            extension
        ));
        if safe.exists() {
            return safe;
        }
        if let Some(legacy) = legacy_storage_name(artifact_id) {
            let legacy = run_root
                .join("artifacts")
                .join(format!("{}.{}", legacy, extension));
            if legacy.exists() {
                return legacy;
            }
        }
        safe
    }

    fn prepare_artifact_path_for_write(
        &self,
        run_root: &Path,
        artifact_id: &str,
        format: DebugArtifactFormat,
    ) -> Result<PathBuf> {
        let extension = match format {
            DebugArtifactFormat::Json => "json",
            DebugArtifactFormat::JsonLines => "jsonl",
        };
        let safe = run_root.join("artifacts").join(format!(
            "{}.{}",
            safe_storage_name(artifact_id),
            extension
        ));
        if !safe.exists()
            && let Some(legacy) = legacy_storage_name(artifact_id)
        {
            let legacy = run_root
                .join("artifacts")
                .join(format!("{}.{}", legacy, extension));
            if legacy.exists() {
                self.ensure_parent_dir(&safe)?;
                fs::rename(&legacy, &safe).with_context(|| {
                    format!(
                        "failed to migrate legacy debug artifact {} to {}",
                        legacy.display(),
                        safe.display()
                    )
                })?;
            }
        }
        Ok(safe)
    }

    fn prepare_run_root_for_write(&self, run_id: &str) -> Result<PathBuf> {
        prepare_storage_dir_for_write(&self.root.join("debug"), run_id)
    }

    fn run_root_for_read(&self, run_id: &str) -> PathBuf {
        resolve_storage_dir_for_read(&self.root.join("debug"), run_id)
    }

    fn ensure_parent_dir(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        Ok(())
    }

    fn prepare_body(&self, path: &Path, artifact: &DebugArtifact) -> Result<PreparedArtifactBody> {
        let plaintext = match artifact.format {
            DebugArtifactFormat::Json => serde_json::to_vec_pretty(&artifact.payload)?,
            DebugArtifactFormat::JsonLines => {
                let mut body = if path.exists() {
                    self.decode_stored_body(&fs::read(path)?, path)?
                } else {
                    Vec::new()
                };
                writeln!(body, "{}", serde_json::to_string(&artifact.payload)?)?;
                body
            }
        };
        self.apply_artifact_budget(artifact.format, plaintext)
    }

    fn apply_artifact_budget(
        &self,
        format: DebugArtifactFormat,
        plaintext: Vec<u8>,
    ) -> Result<PreparedArtifactBody> {
        let max_artifact_bytes = self
            .policy
            .max_artifact_bytes
            .min(self.policy.max_run_bytes);
        let original_bytes = plaintext.len() as u64;
        let original_sha256 = sha256_hex(&plaintext);
        if original_bytes <= max_artifact_bytes {
            return Ok(PreparedArtifactBody {
                plaintext,
                sha256: original_sha256,
                plaintext_bytes: original_bytes,
                truncated: false,
                original_bytes: None,
            });
        }

        let plaintext = match format {
            DebugArtifactFormat::Json => {
                truncate_json_artifact(&plaintext, max_artifact_bytes, &original_sha256)?
            }
            DebugArtifactFormat::JsonLines => {
                truncate_json_lines_artifact(&plaintext, max_artifact_bytes, &original_sha256)?
            }
        };
        Ok(PreparedArtifactBody {
            plaintext_bytes: plaintext.len() as u64,
            sha256: sha256_hex(&plaintext),
            plaintext,
            truncated: true,
            original_bytes: Some(original_bytes),
        })
    }

    fn enforce_run_budget(
        &self,
        run_root: &Path,
        view: &mut RunDebugView,
        current_artifact_id: &str,
    ) -> Result<()> {
        loop {
            let total_bytes = view
                .artifacts
                .iter()
                .map(|artifact| artifact.plaintext_bytes)
                .sum::<u64>();
            let over_count = view.artifacts.len() > self.policy.max_artifacts_per_run;
            let over_bytes = total_bytes > self.policy.max_run_bytes;
            if !over_count && !over_bytes {
                return Ok(());
            }
            let Some(remove_index) = view
                .artifacts
                .iter()
                .enumerate()
                .filter(|(_, artifact)| artifact.artifact_id != current_artifact_id)
                .min_by_key(|(_, artifact)| (artifact.updated_at_ms, artifact.artifact_id.clone()))
                .map(|(index, _)| index)
            else {
                return Ok(());
            };
            let removed = view.artifacts.remove(remove_index);
            let path = self.artifact_path_for_read(run_root, &removed.artifact_id, removed.format);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to delete {}", path.display()));
                }
            }
        }
    }

    #[cfg(test)]
    fn prune_expired(&self, now_ms: u64) -> Result<()> {
        if self.policy.ttl_ms == 0 {
            return Ok(());
        }
        let debug_root = self.root.join("debug");
        if !debug_root.exists() {
            return Ok(());
        }
        let cutoff_ms = now_ms.saturating_sub(self.policy.ttl_ms);
        for path in debug_run_roots(&debug_root)? {
            let manifest_path = path.join("manifest.json");
            let Ok(bytes) = fs::read(&manifest_path) else {
                continue;
            };
            let Ok(view) = serde_json::from_slice::<RunDebugView>(&bytes) else {
                continue;
            };
            let latest = view
                .artifacts
                .iter()
                .map(|artifact| artifact.updated_at_ms)
                .max()
                .unwrap_or(0);
            if latest <= cutoff_ms {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("failed to prune {}", path.display()))?;
            }
        }
        Ok(())
    }

    fn encryption_key(&self) -> Result<Option<[u8; 32]>> {
        if let Some(error) = &self.encryption_key_error {
            bail!("{error}");
        }
        Ok(self.encryption_key)
    }

    fn encrypt_body(&self, body: &PreparedArtifactBody, key: &[u8; 32]) -> Result<Vec<u8>> {
        let cipher = Aes256GcmSiv::new_from_slice(key).expect("32-byte key should be valid");
        let mut nonce_bytes = [0_u8; DEBUG_ARTIFACT_NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), body.plaintext.as_ref())
            .map_err(|error| anyhow!("failed to encrypt debug artifact: {error}"))?;
        let envelope = EncryptedDebugArtifactEnvelope {
            kind: DEBUG_ARTIFACT_ENVELOPE_KIND.to_string(),
            version: DEBUG_ARTIFACT_ENVELOPE_VERSION,
            key_id: self.encryption_key_id.clone(),
            nonce: BASE64_STANDARD.encode(nonce_bytes),
            ciphertext: BASE64_STANDARD.encode(ciphertext),
            plaintext_sha256: body.sha256.clone(),
            plaintext_bytes: body.plaintext_bytes,
        };
        serde_json::to_vec_pretty(&envelope).map_err(Into::into)
    }

    fn decode_stored_body(&self, bytes: &[u8], path: &Path) -> Result<Vec<u8>> {
        let Ok(envelope) = serde_json::from_slice::<EncryptedDebugArtifactEnvelope>(bytes) else {
            return Ok(bytes.to_vec());
        };
        if envelope.kind != DEBUG_ARTIFACT_ENVELOPE_KIND {
            return Ok(bytes.to_vec());
        }
        if envelope.version != DEBUG_ARTIFACT_ENVELOPE_VERSION {
            bail!(
                "unsupported encrypted debug artifact version {} in {}",
                envelope.version,
                path.display()
            );
        }
        let key = self.encryption_key()?.ok_or_else(|| {
            anyhow!(
                "debug artifact {} is encrypted; set {} or {} before reading it",
                path.display(),
                DEBUG_CAPTURE_KEY_ENV,
                DEBUG_CAPTURE_KEY_FILE_ENV
            )
        })?;
        if let (Some(expected), Some(actual)) = (
            envelope.key_id.as_deref(),
            self.encryption_key_id.as_deref(),
        ) {
            anyhow::ensure!(
                expected == actual,
                "debug artifact {} was encrypted with key id {expected}, but current key id is {actual}",
                path.display()
            );
        }
        let nonce = BASE64_STANDARD
            .decode(&envelope.nonce)
            .with_context(|| format!("invalid debug artifact nonce in {}", path.display()))?;
        if nonce.len() != DEBUG_ARTIFACT_NONCE_LEN {
            bail!("invalid debug artifact nonce length in {}", path.display());
        }
        let ciphertext = BASE64_STANDARD
            .decode(&envelope.ciphertext)
            .with_context(|| format!("invalid debug artifact ciphertext in {}", path.display()))?;
        let cipher = Aes256GcmSiv::new_from_slice(&key).expect("32-byte key should be valid");
        let plaintext = cipher
            .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
            .map_err(|error| {
                anyhow!(
                    "failed to decrypt debug artifact {}: {error}",
                    path.display()
                )
            })?;
        let digest = sha256_hex(&plaintext);
        anyhow::ensure!(
            digest == envelope.plaintext_sha256,
            "debug artifact checksum mismatch for {}",
            path.display()
        );
        Ok(plaintext)
    }
}

fn load_debug_capture_key_from_env() -> Result<Option<[u8; 32]>> {
    let raw = std::env::var_os(DEBUG_CAPTURE_KEY_ENV);
    let raw_file = std::env::var_os(DEBUG_CAPTURE_KEY_FILE_ENV);
    match (raw, raw_file) {
        (Some(_), Some(_)) => {
            bail!("{DEBUG_CAPTURE_KEY_ENV} and {DEBUG_CAPTURE_KEY_FILE_ENV} are mutually exclusive")
        }
        (Some(raw), None) => {
            let raw = raw
                .into_string()
                .map_err(|_| anyhow!("{DEBUG_CAPTURE_KEY_ENV} must be valid UTF-8"))?;
            parse_debug_capture_key(&raw, DEBUG_CAPTURE_KEY_ENV).map(Some)
        }
        (None, Some(path)) => {
            let path = PathBuf::from(path);
            let raw = fs::read_to_string(&path).with_context(|| {
                format!("failed to read debug capture key file {}", path.display())
            })?;
            parse_debug_capture_key(&raw, DEBUG_CAPTURE_KEY_FILE_ENV).map(Some)
        }
        (None, None) => Ok(None),
    }
}

fn parse_debug_capture_key(raw: &str, source: &str) -> Result<[u8; 32]> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("{source} cannot be empty");
    }
    if let Ok(decoded) = BASE64_STANDARD.decode(trimmed)
        && decoded.len() == 32
    {
        let mut key = [0_u8; 32];
        key.copy_from_slice(&decoded);
        return Ok(key);
    }
    if trimmed.as_bytes().len() == 32 {
        let mut key = [0_u8; 32];
        key.copy_from_slice(trimmed.as_bytes());
        return Ok(key);
    }
    bail!("{source} must be exactly 32 raw bytes or base64-encoded 32 bytes")
}

fn read_positive_u64_env(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => match value.trim().parse::<u64>() {
            Ok(parsed) if parsed > 0 => parsed,
            Ok(_) | Err(_) => {
                tracing::warn!(
                    env = name,
                    value = %value,
                    default,
                    "invalid debug capture numeric environment override; using default"
                );
                default
            }
        },
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => {
            tracing::warn!(
                env = name,
                error = ?error,
                default,
                "invalid debug capture environment override; using default"
            );
            default
        }
    }
}

fn read_positive_usize_env(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(value) => match value.trim().parse::<usize>() {
            Ok(parsed) if parsed > 0 => parsed,
            Ok(_) | Err(_) => {
                tracing::warn!(
                    env = name,
                    value = %value,
                    default,
                    "invalid debug capture numeric environment override; using default"
                );
                default
            }
        },
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => {
            tracing::warn!(
                env = name,
                error = ?error,
                default,
                "invalid debug capture environment override; using default"
            );
            default
        }
    }
}

fn read_u64_env(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => match value.trim().parse::<u64>() {
            Ok(parsed) => parsed,
            Err(_) => {
                tracing::warn!(
                    env = name,
                    value = %value,
                    default,
                    "invalid debug capture numeric environment override; using default"
                );
                default
            }
        },
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => {
            tracing::warn!(
                env = name,
                error = ?error,
                default,
                "invalid debug capture environment override; using default"
            );
            default
        }
    }
}

fn read_optional_positive_u64_env(name: &str) -> Option<u64> {
    match std::env::var(name) {
        Ok(value) => match value.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(parsed) => Some(parsed),
            Err(_) => {
                tracing::warn!(
                    env = name,
                    value = %value,
                    "invalid debug capture optional byte cap; ignoring override"
                );
                None
            }
        },
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => {
            tracing::warn!(
                env = name,
                error = ?error,
                "invalid debug capture optional byte cap; ignoring override"
            );
            None
        }
    }
}

fn truncate_json_artifact(
    plaintext: &[u8],
    max_bytes: u64,
    original_sha256: &str,
) -> Result<Vec<u8>> {
    let original_bytes = plaintext.len() as u64;
    let mut excerpt_len = plaintext.len().min(max_bytes as usize);
    loop {
        let excerpt = String::from_utf8_lossy(&plaintext[..excerpt_len]).to_string();
        let body = serde_json::to_vec_pretty(&json!({
            "debug_artifact_truncated": true,
            "original_bytes": original_bytes,
            "original_sha256": original_sha256,
            "excerpt": excerpt,
        }))?;
        if body.len() as u64 <= max_bytes || excerpt_len == 0 {
            return Ok(body);
        }
        excerpt_len /= 2;
    }
}

fn truncate_json_lines_artifact(
    plaintext: &[u8],
    max_bytes: u64,
    original_sha256: &str,
) -> Result<Vec<u8>> {
    let marker = serde_json::to_vec(&json!({
        "debug_artifact_truncated": true,
        "original_bytes": plaintext.len(),
        "original_sha256": original_sha256,
    }))?;
    let mut rendered = marker;
    rendered.push(b'\n');
    let budget = max_bytes.saturating_sub(rendered.len() as u64) as usize;
    let start = plaintext.len().saturating_sub(budget);
    let tail = &plaintext[start..];
    let aligned = tail
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|offset| &tail[offset + 1..])
        .unwrap_or(tail);
    rendered.extend_from_slice(aligned);
    if rendered.len() as u64 > max_bytes {
        let max_bytes = max_bytes as usize;
        rendered.truncate(max_bytes);
        if !rendered.ends_with(b"\n") && rendered.len() < max_bytes {
            rendered.push(b'\n');
        }
    }
    Ok(rendered)
}

fn debug_store_lock_for_root(root: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
    let key = normalized_debug_store_root(root);
    let mut locks = LOCKS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .expect("debug store lock registry poisoned");
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

fn normalized_debug_store_root(root: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(root) {
        return canonical;
    }
    if root.is_absolute() {
        return root.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(root))
        .unwrap_or_else(|_| root.to_path_buf())
}

fn debug_run_roots(debug_root: &Path) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    for entry in fs::read_dir(debug_root)
        .with_context(|| format!("failed to read {}", debug_root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if is_debug_bundle_dir(&path) {
            roots.push(path);
            continue;
        }
        for child in
            fs::read_dir(&path).with_context(|| format!("failed to read {}", path.display()))?
        {
            let child = child?;
            let child_path = child.path();
            if child_path.is_dir() && is_debug_bundle_dir(&child_path) {
                roots.push(child_path);
            }
        }
    }
    Ok(roots)
}

fn is_debug_bundle_dir(path: &Path) -> bool {
    path.join("manifest.json").exists() || path.join("artifacts").is_dir()
}

fn remove_debug_bundle_dir(path: &Path) -> Result<bool> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to delete {}", path.display())),
    }
}

fn unique_record_run_ids(records: &[DebugBundleRecord]) -> Vec<String> {
    let mut run_ids = records
        .iter()
        .map(|record| record.run_id.clone())
        .collect::<Vec<_>>();
    run_ids.sort();
    run_ids.dedup();
    run_ids
}

fn sum_record_bytes(records: &[DebugBundleRecord]) -> u64 {
    records
        .iter()
        .map(|record| record.bytes)
        .fold(0_u64, u64::saturating_add)
}

fn debug_bundle_run_id_from_path(debug_root: &Path, path: &Path) -> String {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown-debug-bundle");
    let safe_root = debug_root.join("__safe");
    if path.parent() == Some(safe_root.as_path())
        && let Some(decoded) = decode_safe_storage_name(file_name)
    {
        return decoded;
    }
    file_name.to_string()
}

fn modified_time_ms(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(system_time_ms)
}

fn directory_latest_modified_time_ms(path: &Path) -> Option<u64> {
    let metadata = fs::symlink_metadata(path).ok()?;
    let mut latest = metadata.modified().ok().and_then(system_time_ms);
    if metadata.is_dir() {
        let entries = fs::read_dir(path).ok()?;
        for entry in entries.flatten() {
            if let Some(child_latest) = directory_latest_modified_time_ms(&entry.path()) {
                latest = Some(latest.map_or(child_latest, |current| current.max(child_latest)));
            }
        }
    }
    latest
}

fn system_time_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
}

fn directory_bytes(path: &Path) -> Result<u64> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to stat {}", path.display()));
        }
    };
    if metadata.is_file() || metadata.file_type().is_symlink() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }
    let mut total = metadata.len();
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        total = total.saturating_add(directory_bytes(&entry.path())?);
    }
    Ok(total)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn debug_capture_key_id(key: &[u8; 32]) -> String {
    sha256_hex(key.as_slice()).chars().take(16).collect()
}

fn artifact_id(artifact: &DebugArtifact) -> String {
    let mut parts = Vec::new();
    if let Some(turn) = artifact.turn {
        parts.push(format!("turn-{turn:04}"));
    }
    if let Some(attempt) = artifact.attempt {
        parts.push(format!("attempt-{attempt:04}"));
    }
    parts.push(sanitize_fragment(&artifact.name));
    parts.join("-")
}

fn artifact_id_for_append(artifact: &DebugArtifact, view: &RunDebugView) -> String {
    let base = artifact_id(artifact);
    if artifact.format != DebugArtifactFormat::Json
        || artifact.turn.is_some()
        || artifact.attempt.is_some()
        || !view.artifacts.iter().any(|entry| entry.artifact_id == base)
    {
        return base;
    }

    let mut candidate = format!("{base}-{}", artifact.timestamp_ms);
    let mut suffix = 2_u64;
    while view
        .artifacts
        .iter()
        .any(|entry| entry.artifact_id == candidate)
    {
        candidate = format!("{base}-{}-{suffix}", artifact.timestamp_ms);
        suffix = suffix.saturating_add(1);
    }
    candidate
}

fn sanitize_fragment(fragment: &str) -> String {
    fragment
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use kheish_session::{safe_storage_dir, safe_storage_name};

    use super::*;

    fn without_debug_capture_key() -> std::sync::MutexGuard<'static, ()> {
        let guard = debug_capture_env_lock();
        unsafe {
            std::env::remove_var(DEBUG_CAPTURE_KEY_ENV);
            std::env::remove_var(DEBUG_CAPTURE_KEY_FILE_ENV);
            clear_debug_store_policy_env();
        }
        guard
    }

    unsafe fn clear_debug_store_policy_env() {
        unsafe {
            std::env::remove_var(DEBUG_MAX_ARTIFACT_BYTES_ENV);
            std::env::remove_var(DEBUG_MAX_RUN_BYTES_ENV);
            std::env::remove_var(DEBUG_MAX_ARTIFACTS_PER_RUN_ENV);
            std::env::remove_var(DEBUG_TTL_MS_ENV);
            std::env::remove_var(DEBUG_MAX_STORE_BYTES_ENV);
            std::env::remove_var(DEBUG_GC_INTERVAL_MS_ENV);
        }
    }

    fn sample_artifact(run_id: &str) -> DebugArtifact {
        DebugArtifact {
            timestamp_ms: 1,
            session_id: Some("session-a".to_string()),
            agent_id: Some("agent-1".to_string()),
            run_id: Some(run_id.to_string()),
            level: DebugCaptureLevel::On,
            name: "response".to_string(),
            payload: serde_json::json!({"ok": true}),
            turn: Some(1),
            attempt: Some(1),
            format: DebugArtifactFormat::Json,
        }
    }

    fn sample_named_artifact(run_id: &str, name: &str, timestamp_ms: u64) -> DebugArtifact {
        DebugArtifact {
            timestamp_ms,
            name: name.to_string(),
            ..sample_artifact(run_id)
        }
    }

    #[test]
    fn debug_store_uses_safe_run_directories_for_hostile_run_ids() -> Result<()> {
        let _guard = without_debug_capture_key();
        let root = tempfile::tempdir()?;
        let store = FileDebugStore::new(root.path());
        let run_id = "../../../tmp/evil-run";
        store.append_artifact(&sample_artifact(run_id))?;

        let safe_run_root = safe_storage_dir(&root.path().join("debug"), run_id);
        assert!(safe_run_root.join("manifest.json").exists());
        assert!(
            safe_run_root
                .join("artifacts")
                .join(format!(
                    "{}.json",
                    safe_storage_name("turn-0001-attempt-0001-response")
                ))
                .exists()
        );
        assert!(!root.path().join("debug/../../../tmp/evil-run").exists());
        assert!(store.load_view(run_id)?.is_some());
        Ok(())
    }

    #[test]
    fn debug_store_delete_run_reports_whether_bundle_existed() -> Result<()> {
        let _guard = without_debug_capture_key();
        let root = tempfile::tempdir()?;
        let store = FileDebugStore::new(root.path());
        store.append_artifact(&sample_artifact("run-delete"))?;

        assert!(store.delete_run("run-delete")?);
        assert!(!store.delete_run("run-delete")?);
        assert!(!store.delete_run("run-never-created")?);
        Ok(())
    }

    #[test]
    fn debug_store_records_checksums_and_truncates_oversized_json() -> Result<()> {
        let _guard = without_debug_capture_key();
        let root = tempfile::tempdir()?;
        let store = FileDebugStore::with_policy(
            root.path(),
            DebugStorePolicy {
                max_artifact_bytes: 512,
                max_run_bytes: 4_096,
                max_artifacts_per_run: 16,
                ttl_ms: 0,
                max_store_bytes: None,
                gc_interval_ms: DEFAULT_DEBUG_GC_INTERVAL_MS,
            },
        );
        let mut artifact = sample_artifact("run-big");
        artifact.payload = serde_json::json!({
            "large": "x".repeat(8_000),
            "tail": "visible",
        });

        store.append_artifact(&artifact)?;

        let view = store.load_view("run-big")?.expect("debug view");
        let summary = view.artifacts.first().expect("debug artifact summary");
        assert!(summary.truncated);
        assert!(summary.original_bytes.unwrap_or_default() > summary.plaintext_bytes);
        assert!(!summary.sha256.is_empty());
        assert!(summary.plaintext_bytes <= 512);
        let artifact_text = store.read_artifact("run-big", &summary.artifact_id)?;
        let value: serde_json::Value = serde_json::from_str(&artifact_text)?;
        assert_eq!(value["debug_artifact_truncated"], true);
        assert!(
            value["excerpt"]
                .as_str()
                .unwrap_or_default()
                .contains("large")
        );
        Ok(())
    }

    #[test]
    fn debug_store_caps_current_artifact_by_run_plaintext_budget() -> Result<()> {
        let _guard = without_debug_capture_key();
        let root = tempfile::tempdir()?;
        let store = FileDebugStore::with_policy(
            root.path(),
            DebugStorePolicy {
                max_artifact_bytes: 4_096,
                max_run_bytes: 768,
                max_artifacts_per_run: 16,
                ttl_ms: 0,
                max_store_bytes: None,
                gc_interval_ms: DEFAULT_DEBUG_GC_INTERVAL_MS,
            },
        );
        let mut artifact = sample_artifact("run-current-budget");
        artifact.payload = serde_json::json!({
            "large": "x".repeat(8_000),
            "tail": "visible",
        });

        store.append_artifact(&artifact)?;

        let view = store.load_view("run-current-budget")?.expect("debug view");
        let summary = view.artifacts.first().expect("debug artifact summary");
        assert!(summary.truncated);
        assert!(summary.plaintext_bytes <= 768);
        assert!(
            view.artifacts
                .iter()
                .map(|artifact| artifact.plaintext_bytes)
                .sum::<u64>()
                <= 768
        );
        let artifact_text = store.read_artifact("run-current-budget", &summary.artifact_id)?;
        let value: serde_json::Value = serde_json::from_str(&artifact_text)?;
        assert_eq!(value["debug_artifact_truncated"], true);
        Ok(())
    }

    #[test]
    fn debug_store_prunes_old_artifacts_by_run_budget() -> Result<()> {
        let _guard = without_debug_capture_key();
        let root = tempfile::tempdir()?;
        let store = FileDebugStore::with_policy(
            root.path(),
            DebugStorePolicy {
                max_artifact_bytes: 256,
                max_run_bytes: 700,
                max_artifacts_per_run: 2,
                ttl_ms: 0,
                max_store_bytes: None,
                gc_interval_ms: DEFAULT_DEBUG_GC_INTERVAL_MS,
            },
        );
        for index in 1..=3 {
            let mut artifact =
                sample_named_artifact("run-budget", &format!("artifact-{index}"), index);
            artifact.payload = serde_json::json!({"body": "z".repeat(180), "index": index});
            store.append_artifact(&artifact)?;
        }

        let view = store.load_view("run-budget")?.expect("debug view");
        assert!(view.artifacts.len() <= 2);
        assert!(
            view.artifacts
                .iter()
                .all(|artifact| artifact.artifact_id != "turn-0001-attempt-0001-artifact-1")
        );
        Ok(())
    }

    #[test]
    fn debug_store_preserves_duplicate_static_json_artifacts() -> Result<()> {
        let _guard = without_debug_capture_key();
        let root = tempfile::tempdir()?;
        let store = FileDebugStore::new(root.path());
        let mut first =
            sample_named_artifact("run-static", "openai-audio-speech-provider-request", 1);
        first.turn = None;
        first.attempt = None;
        first.payload = serde_json::json!({"request": 1});
        let mut second = first.clone();
        second.timestamp_ms = 2;
        second.payload = serde_json::json!({"request": 2});

        store.append_artifact(&first)?;
        store.append_artifact(&second)?;

        let view = store.load_view("run-static")?.expect("debug view");
        let ids = view
            .artifacts
            .iter()
            .map(|artifact| artifact.artifact_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                "openai-audio-speech-provider-request",
                "openai-audio-speech-provider-request-2"
            ]
        );
        assert!(
            store
                .read_artifact("run-static", "openai-audio-speech-provider-request")?
                .contains("\"request\": 1")
        );
        assert!(
            store
                .read_artifact("run-static", "openai-audio-speech-provider-request-2")?
                .contains("\"request\": 2")
        );
        Ok(())
    }

    #[test]
    fn debug_store_serializes_concurrent_artifact_appends() -> Result<()> {
        let _guard = without_debug_capture_key();
        let root = tempfile::tempdir()?;
        let store = FileDebugStore::new(root.path());
        let barrier = Arc::new(std::sync::Barrier::new(12));
        let mut handles = Vec::new();
        for index in 0..12 {
            let store = store.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || -> Result<()> {
                barrier.wait();
                let mut artifact =
                    sample_named_artifact("run-concurrent", &format!("artifact-{index}"), index);
                artifact.payload = serde_json::json!({"index": index});
                store.append_artifact(&artifact)
            }));
        }
        for handle in handles {
            handle
                .join()
                .expect("debug append worker should not panic")?;
        }

        let view = store.load_view("run-concurrent")?.expect("debug view");
        assert_eq!(view.artifacts.len(), 12);
        for index in 0..12 {
            assert!(view.artifacts.iter().any(|artifact| {
                artifact.artifact_id == format!("turn-0001-attempt-0001-artifact-{index}")
            }));
        }
        Ok(())
    }

    #[test]
    fn debug_store_serializes_independent_instances_for_same_root() -> Result<()> {
        let _guard = without_debug_capture_key();
        let root = tempfile::tempdir()?;
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for index in 0..8 {
            let root = root.path().to_path_buf();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || -> Result<()> {
                let store = FileDebugStore::new(&root);
                barrier.wait();
                let mut artifact =
                    sample_named_artifact("run-shared-root", &format!("artifact-{index}"), index);
                artifact.payload = serde_json::json!({"index": index});
                store.append_artifact(&artifact)
            }));
        }
        for handle in handles {
            handle
                .join()
                .expect("debug append worker should not panic")?;
        }

        let store = FileDebugStore::new(root.path());
        let view = store.load_view("run-shared-root")?.expect("debug view");
        assert_eq!(view.artifacts.len(), 8);
        Ok(())
    }

    #[test]
    fn debug_store_rejects_checksum_mismatched_artifacts() -> Result<()> {
        let _guard = without_debug_capture_key();
        let root = tempfile::tempdir()?;
        let store = FileDebugStore::new(root.path());
        store.append_artifact(&sample_artifact("run-corrupt"))?;
        let view = store.load_view("run-corrupt")?.expect("debug view");
        let summary = view.artifacts.first().expect("debug artifact summary");
        let path = safe_storage_dir(&root.path().join("debug"), "run-corrupt")
            .join("artifacts")
            .join(format!("{}.json", safe_storage_name(&summary.artifact_id)));
        fs::write(&path, br#"{"ok": false}"#)?;

        let error = store
            .read_artifact("run-corrupt", &summary.artifact_id)
            .expect_err("checksum mismatch should be rejected");
        assert!(
            error
                .to_string()
                .contains("debug artifact checksum mismatch")
        );
        Ok(())
    }

    #[test]
    fn debug_store_policy_reads_environment_overrides() -> Result<()> {
        let _guard = debug_capture_env_lock();
        unsafe {
            clear_debug_store_policy_env();
            std::env::remove_var(DEBUG_CAPTURE_KEY_ENV);
            std::env::remove_var(DEBUG_CAPTURE_KEY_FILE_ENV);
            std::env::set_var(DEBUG_MAX_ARTIFACT_BYTES_ENV, "1234");
            std::env::set_var(DEBUG_MAX_RUN_BYTES_ENV, "5678");
            std::env::set_var(DEBUG_MAX_ARTIFACTS_PER_RUN_ENV, "9");
            std::env::set_var(DEBUG_TTL_MS_ENV, "10");
            std::env::set_var(DEBUG_MAX_STORE_BYTES_ENV, "1112");
            std::env::set_var(DEBUG_GC_INTERVAL_MS_ENV, "1314");
        }

        let root = tempfile::tempdir()?;
        let store = FileDebugStore::new(root.path());
        assert_eq!(store.policy.max_artifact_bytes, 1_234);
        assert_eq!(store.policy.max_run_bytes, 5_678);
        assert_eq!(store.policy.max_artifacts_per_run, 9);
        assert_eq!(store.policy.ttl_ms, 10);
        assert_eq!(store.policy.max_store_bytes, Some(1_112));
        assert_eq!(store.policy.gc_interval_ms, 1_314);

        unsafe {
            clear_debug_store_policy_env();
        }
        Ok(())
    }

    #[test]
    fn debug_store_prunes_expired_run_bundles_when_requested() -> Result<()> {
        let _guard = without_debug_capture_key();
        let root = tempfile::tempdir()?;
        let store = FileDebugStore::with_policy(
            root.path(),
            DebugStorePolicy {
                max_artifact_bytes: 4_096,
                max_run_bytes: 16_384,
                max_artifacts_per_run: 16,
                ttl_ms: 100,
                max_store_bytes: None,
                gc_interval_ms: DEFAULT_DEBUG_GC_INTERVAL_MS,
            },
        );
        store.append_artifact(&sample_named_artifact("run-old", "old", 1))?;
        assert!(store.load_view("run-old")?.is_some());

        store.append_artifact(&sample_named_artifact("run-new", "new", 1_000))?;
        store.prune_expired(1_000)?;

        assert!(store.load_view("run-old")?.is_none());
        assert!(store.load_view("run-new")?.is_some());
        Ok(())
    }

    #[test]
    fn debug_store_orphan_bundle_age_uses_recursive_file_mtime() -> Result<()> {
        let _guard = without_debug_capture_key();
        let root = tempfile::tempdir()?;
        let bundle_root = safe_storage_dir(&root.path().join("debug"), "run-orphan-fresh-child");
        let artifacts_root = bundle_root.join("artifacts");
        fs::create_dir_all(&artifacts_root)?;
        fs::write(bundle_root.join("manifest.json"), b"{not-json")?;
        let bundle_mtime = modified_time_ms(&bundle_root).expect("bundle mtime");
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(artifacts_root.join("fresh.json"), br#"{"fresh":true}"#)?;
        let latest_mtime =
            directory_latest_modified_time_ms(&bundle_root).expect("recursive mtime");
        assert!(
            latest_mtime > bundle_mtime,
            "test filesystem should expose a newer child mtime"
        );

        let store = FileDebugStore::with_policy(
            root.path(),
            DebugStorePolicy {
                max_artifact_bytes: 4_096,
                max_run_bytes: 16_384,
                max_artifacts_per_run: 16,
                ttl_ms: latest_mtime.saturating_sub(bundle_mtime),
                max_store_bytes: None,
                gc_interval_ms: DEFAULT_DEBUG_GC_INTERVAL_MS,
            },
        );

        let pruned = store.prune_expired_bundles(
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            latest_mtime,
        )?;

        assert!(pruned.pruned_debug_run_ids.is_empty());
        assert!(bundle_root.exists());
        Ok(())
    }

    #[test]
    fn debug_store_encrypts_artifacts_when_key_is_configured() -> Result<()> {
        let _guard = debug_capture_env_lock();
        unsafe {
            clear_debug_store_policy_env();
            std::env::set_var(DEBUG_CAPTURE_KEY_ENV, "0123456789abcdef0123456789abcdef");
            std::env::remove_var(DEBUG_CAPTURE_KEY_FILE_ENV);
        }

        let root = tempfile::tempdir()?;
        let store = FileDebugStore::new(root.path());
        let mut artifact = sample_artifact("run-encrypted");
        artifact.payload = serde_json::json!({"message": "debug-secret-value"});
        store.append_artifact(&artifact)?;

        let view = store.load_view("run-encrypted")?.expect("debug view");
        let summary = view.artifacts.first().expect("debug artifact summary");
        assert!(summary.encrypted);
        assert!(summary.encryption_key_id.is_some());
        assert!(!summary.sha256.is_empty());
        let path = safe_storage_dir(&root.path().join("debug"), "run-encrypted")
            .join("artifacts")
            .join(format!("{}.json", safe_storage_name(&summary.artifact_id)));
        let raw = fs::read_to_string(&path)?;
        assert!(!raw.contains("debug-secret-value"));
        assert!(raw.contains(DEBUG_ARTIFACT_ENVELOPE_KIND));
        assert!(raw.contains("key_id"));

        let body = store.read_artifact("run-encrypted", &summary.artifact_id)?;
        assert!(body.contains("debug-secret-value"));
        let policy = store.policy_view();
        assert!(policy.encryption_enabled);
        assert_eq!(policy.encryption_key_id, summary.encryption_key_id);

        unsafe {
            std::env::remove_var(DEBUG_CAPTURE_KEY_ENV);
        }
        Ok(())
    }

    #[test]
    fn debug_store_invalid_encryption_key_fails_closed_without_writing_artifacts() -> Result<()> {
        let _guard = debug_capture_env_lock();
        unsafe {
            clear_debug_store_policy_env();
            std::env::set_var(DEBUG_CAPTURE_KEY_ENV, "too-short");
            std::env::remove_var(DEBUG_CAPTURE_KEY_FILE_ENV);
        }

        let root = tempfile::tempdir()?;
        let store = FileDebugStore::new(root.path());
        let mut artifact = sample_artifact("run-invalid-key");
        artifact.payload = serde_json::json!({"message": "debug-secret-value"});
        let error = store
            .append_artifact(&artifact)
            .expect_err("invalid encryption key should fail closed");
        assert!(error.to_string().contains(DEBUG_CAPTURE_KEY_ENV));
        assert!(store.load_view("run-invalid-key")?.is_none());

        let artifact_path = safe_storage_dir(&root.path().join("debug"), "run-invalid-key")
            .join("artifacts")
            .join(format!(
                "{}.json",
                safe_storage_name("turn-0001-attempt-0001-response")
            ));
        assert!(!artifact_path.exists());

        unsafe {
            std::env::remove_var(DEBUG_CAPTURE_KEY_ENV);
        }
        Ok(())
    }
}
