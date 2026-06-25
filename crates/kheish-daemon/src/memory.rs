use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use kheish_runtime::redact_text;
use kheish_session::{
    decode_safe_storage_name, prepare_storage_path_for_write, resolve_storage_path_for_read,
    write_json_pretty_atomically,
};
use kheish_types::ContentPart;
use kheish_types::RecoveredMemoryEntry;
use serde::{Deserialize, Serialize};

use crate::runs::truncate_preview;
use crate::{DaemonRunStatus, RunRecord, RunRequestPayload, SubmitInputItemRequest};

const MAX_MEMORY_SUMMARY_CHARS: usize = 480;
pub(crate) const DEFAULT_RUN_MEMORY_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;
pub(crate) const DEFAULT_MAX_RECOVERED_RUN_MEMORIES: usize = 3;
pub(crate) const DEFAULT_MAX_TRACKED_RUN_MEMORIES_PER_SESSION: usize = 32;
#[allow(dead_code)]
pub(crate) const RUN_MEMORY_RETENTION_MS: u64 = DEFAULT_RUN_MEMORY_RETENTION_MS;
#[allow(dead_code)]
pub(crate) const MAX_RECOVERED_RUN_MEMORIES: usize = DEFAULT_MAX_RECOVERED_RUN_MEMORIES;
#[allow(dead_code)]
pub(crate) const MAX_TRACKED_RUN_MEMORIES_PER_SESSION: usize =
    DEFAULT_MAX_TRACKED_RUN_MEMORIES_PER_SESSION;

fn default_run_memory_enabled() -> bool {
    true
}

fn default_run_memory_retention_ms() -> u64 {
    DEFAULT_RUN_MEMORY_RETENTION_MS
}

fn default_max_tracked_run_memories_per_session() -> usize {
    DEFAULT_MAX_TRACKED_RUN_MEMORIES_PER_SESSION
}

fn default_max_recovered_run_memories() -> usize {
    DEFAULT_MAX_RECOVERED_RUN_MEMORIES
}

fn default_run_memory_redact_pii() -> bool {
    true
}

fn default_run_memory_search_visibility() -> RunMemorySearchVisibility {
    RunMemorySearchVisibility::SessionOnly
}

/// Visibility policy for recovered-run records returned by session memory search.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMemorySearchVisibility {
    /// Search only recovered runs that originated in the requested session.
    #[default]
    SessionOnly,
    /// Search recovered runs visible through the requested session's learning scopes.
    LearningScopes,
}

/// Runtime policy for daemon-owned recovered run memory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMemoryPolicyConfig {
    /// Whether terminal runs should produce recovered run-memory records.
    #[serde(default = "default_run_memory_enabled")]
    pub enabled: bool,
    /// Time-to-live for one run-memory record.
    #[serde(default = "default_run_memory_retention_ms")]
    pub retention_ms: u64,
    /// Maximum tracked run-memory pointers retained per session.
    #[serde(default = "default_max_tracked_run_memories_per_session")]
    pub max_tracked_per_session: usize,
    /// Maximum recovered run-memory entries eligible for one prompt.
    #[serde(default = "default_max_recovered_run_memories")]
    pub max_prompt_entries: usize,
    /// Whether common PII patterns are scrubbed before run memory is persisted.
    #[serde(default = "default_run_memory_redact_pii")]
    pub redact_pii: bool,
    /// Scope boundary used by `/v1/sessions/{session_id}/memory-search` for recovered runs.
    #[serde(default = "default_run_memory_search_visibility")]
    pub search_visibility: RunMemorySearchVisibility,
}

impl Default for RunMemoryPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: default_run_memory_enabled(),
            retention_ms: default_run_memory_retention_ms(),
            max_tracked_per_session: default_max_tracked_run_memories_per_session(),
            max_prompt_entries: default_max_recovered_run_memories(),
            redact_pii: default_run_memory_redact_pii(),
            search_visibility: default_run_memory_search_visibility(),
        }
    }
}

impl RunMemoryPolicyConfig {
    /// Validates one operator-supplied run-memory policy.
    pub fn validate(&self) -> Result<()> {
        if self.enabled {
            if self.retention_ms == 0 {
                bail!("run memory retention_ms must be greater than zero when enabled");
            }
            if self.max_tracked_per_session == 0 {
                bail!("run memory max_tracked_per_session must be greater than zero when enabled");
            }
            if self.max_prompt_entries == 0 {
                bail!("run memory max_prompt_entries must be greater than zero when enabled");
            }
        }
        if self.max_prompt_entries > self.max_tracked_per_session {
            bail!(
                "run memory max_prompt_entries must be lower than or equal to max_tracked_per_session"
            );
        }
        Ok(())
    }
}

/// Monotonic operator counters for recovered run memory.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMemoryMetricsSnapshot {
    #[serde(default)]
    pub stored_total: u64,
    #[serde(default)]
    pub injected_total: u64,
    #[serde(default)]
    pub skipped_unreadable_total: u64,
    #[serde(default)]
    pub pruned_ttl_total: u64,
    #[serde(default)]
    pub pruned_overflow_total: u64,
    #[serde(default)]
    pub pruned_orphan_total: u64,
    #[serde(default)]
    pub redacted_fields_total: u64,
    #[serde(default)]
    pub ranked_candidates_total: u64,
    #[serde(default)]
    pub prompt_limit_omitted_total: u64,
}

#[derive(Debug, Default)]
struct RunMemoryMetrics {
    stored_total: AtomicU64,
    injected_total: AtomicU64,
    skipped_unreadable_total: AtomicU64,
    pruned_ttl_total: AtomicU64,
    pruned_overflow_total: AtomicU64,
    pruned_orphan_total: AtomicU64,
    redacted_fields_total: AtomicU64,
    ranked_candidates_total: AtomicU64,
    prompt_limit_omitted_total: AtomicU64,
}

impl RunMemoryMetrics {
    fn snapshot(&self) -> RunMemoryMetricsSnapshot {
        RunMemoryMetricsSnapshot {
            stored_total: self.stored_total.load(Ordering::Relaxed),
            injected_total: self.injected_total.load(Ordering::Relaxed),
            skipped_unreadable_total: self.skipped_unreadable_total.load(Ordering::Relaxed),
            pruned_ttl_total: self.pruned_ttl_total.load(Ordering::Relaxed),
            pruned_overflow_total: self.pruned_overflow_total.load(Ordering::Relaxed),
            pruned_orphan_total: self.pruned_orphan_total.load(Ordering::Relaxed),
            redacted_fields_total: self.redacted_fields_total.load(Ordering::Relaxed),
            ranked_candidates_total: self.ranked_candidates_total.load(Ordering::Relaxed),
            prompt_limit_omitted_total: self.prompt_limit_omitted_total.load(Ordering::Relaxed),
        }
    }
}

const RUN_MEMORY_MAINTENANCE_DIAGNOSTIC_LIMIT: usize = 32;

/// Last bounded run-memory maintenance report exposed through daemon status.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMemoryMaintenanceStatusView {
    #[serde(default)]
    pub checked_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub index_rebuilt: bool,
    #[serde(default)]
    pub pruned_ttl_count: usize,
    #[serde(default)]
    pub pruned_overflow_count: usize,
    #[serde(default)]
    pub pruned_orphan_file_count: usize,
    #[serde(default)]
    pub scan_error_count: usize,
    #[serde(default)]
    pub prune_error_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<RunMemoryMaintenanceDiagnosticView>,
}

/// One bounded run-memory maintenance diagnostic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMemoryMaintenanceDiagnosticView {
    pub action: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub message: String,
}

impl RunMemoryMaintenanceStatusView {
    pub(crate) fn from_rebuild(
        source: impl Into<String>,
        checked_at_ms: u64,
        index_rebuilt: bool,
        rebuilt: &RunMemoryIndexRebuild,
    ) -> Self {
        let mut report = Self {
            checked_at_ms,
            source: Some(source.into()),
            index_rebuilt,
            pruned_ttl_count: rebuilt.pruned_ttl_run_ids.len(),
            pruned_overflow_count: rebuilt.pruned_overflow_run_ids.len(),
            pruned_orphan_file_count: rebuilt.pruned_orphan_files.len(),
            scan_error_count: 0,
            prune_error_count: 0,
            diagnostics: Vec::new(),
        };
        for run_id in &rebuilt.pruned_ttl_run_ids {
            report.push_diagnostic(
                "delete_run_memory",
                "ttl_expired",
                Some(run_id.clone()),
                None,
                "expired run-memory record deleted",
            );
        }
        for run_id in &rebuilt.pruned_overflow_run_ids {
            report.push_diagnostic(
                "delete_run_memory",
                "session_overflow",
                Some(run_id.clone()),
                None,
                "overflow run-memory record deleted",
            );
        }
        for path in &rebuilt.pruned_orphan_files {
            report.push_diagnostic(
                "delete_run_memory_file",
                "orphan_file",
                None,
                Some(path.display().to_string()),
                "orphan run-memory file deleted",
            );
        }
        report
    }

    pub(crate) fn scan_error(
        source: impl Into<String>,
        checked_at_ms: u64,
        message: impl ToString,
    ) -> Self {
        let mut report = Self {
            checked_at_ms,
            source: Some(source.into()),
            scan_error_count: 1,
            ..Self::default()
        };
        report.push_diagnostic(
            "scan_run_memory_store",
            "scan_failed",
            None,
            None,
            message.to_string(),
        );
        report
    }

    pub(crate) fn record_prune_error(
        &mut self,
        action: &'static str,
        reason: &'static str,
        run_id: Option<String>,
        path: Option<String>,
        message: impl ToString,
    ) {
        self.prune_error_count = self.prune_error_count.saturating_add(1);
        self.push_diagnostic(action, reason, run_id, path, message.to_string());
    }

    pub fn repair_count(&self) -> usize {
        self.pruned_ttl_count
            .saturating_add(self.pruned_overflow_count)
            .saturating_add(self.pruned_orphan_file_count)
    }

    fn push_diagnostic(
        &mut self,
        action: impl Into<String>,
        reason: impl Into<String>,
        run_id: Option<String>,
        path: Option<String>,
        message: impl Into<String>,
    ) {
        if self.diagnostics.len() < RUN_MEMORY_MAINTENANCE_DIAGNOSTIC_LIMIT {
            self.diagnostics.push(RunMemoryMaintenanceDiagnosticView {
                action: action.into(),
                reason: reason.into(),
                run_id,
                path,
                message: message.into(),
            });
        }
    }
}

/// Cheap operator status for recovered run memory.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMemoryStatusView {
    #[serde(default)]
    pub policy: RunMemoryPolicyConfig,
    #[serde(default)]
    pub maintenance: RunMemoryMaintenanceStatusView,
    #[serde(default)]
    pub indexed_session_count: usize,
    #[serde(default)]
    pub indexed_record_count: usize,
    #[serde(default)]
    pub indexed_scope_count: usize,
    #[serde(default)]
    pub stale_indexed_record_count: usize,
    #[serde(default)]
    pub metrics: RunMemoryMetricsSnapshot,
}

/// Shared runtime control for recovered run memory.
#[derive(Clone, Debug)]
pub struct RunMemoryControl {
    policy: Arc<RwLock<RunMemoryPolicyConfig>>,
    metrics: Arc<RunMemoryMetrics>,
    maintenance: Arc<RwLock<RunMemoryMaintenanceStatusView>>,
}

impl Default for RunMemoryControl {
    fn default() -> Self {
        Self::new(RunMemoryPolicyConfig::default())
    }
}

impl RunMemoryControl {
    /// Creates one run-memory control with an initial policy.
    pub fn new(policy: RunMemoryPolicyConfig) -> Self {
        policy
            .validate()
            .expect("default run-memory policy should be valid");
        Self {
            policy: Arc::new(RwLock::new(policy)),
            metrics: Arc::new(RunMemoryMetrics::default()),
            maintenance: Arc::new(RwLock::new(RunMemoryMaintenanceStatusView::default())),
        }
    }

    /// Returns the current policy snapshot.
    pub fn policy(&self) -> RunMemoryPolicyConfig {
        self.policy
            .read()
            .expect("run-memory policy rwlock poisoned")
            .clone()
    }

    /// Replaces the current policy after validation.
    pub fn set_policy(&self, policy: RunMemoryPolicyConfig) -> Result<()> {
        policy.validate()?;
        *self
            .policy
            .write()
            .expect("run-memory policy rwlock poisoned") = policy;
        Ok(())
    }

    /// Returns the current metric snapshot.
    pub fn metrics(&self) -> RunMemoryMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Returns the last bounded maintenance report.
    pub fn maintenance(&self) -> RunMemoryMaintenanceStatusView {
        self.maintenance
            .read()
            .expect("run-memory maintenance rwlock poisoned")
            .clone()
    }

    pub(crate) fn record_maintenance(&self, report: RunMemoryMaintenanceStatusView) {
        *self
            .maintenance
            .write()
            .expect("run-memory maintenance rwlock poisoned") = report;
    }

    pub(crate) fn record_stored(&self) {
        self.metrics.stored_total.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_skipped_unreadable(&self) {
        self.metrics
            .skipped_unreadable_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_pruned_ttl(&self, count: usize) {
        if count > 0 {
            self.metrics
                .pruned_ttl_total
                .fetch_add(count as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_pruned_overflow(&self, count: usize) {
        if count > 0 {
            self.metrics
                .pruned_overflow_total
                .fetch_add(count as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_pruned_orphan(&self, count: usize) {
        if count > 0 {
            self.metrics
                .pruned_orphan_total
                .fetch_add(count as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_redacted_fields(&self, count: usize) {
        if count > 0 {
            self.metrics
                .redacted_fields_total
                .fetch_add(count as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_ranked_candidates(&self, count: usize) {
        if count > 0 {
            self.metrics
                .ranked_candidates_total
                .fetch_add(count as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_prompt_limit_omitted(&self, count: usize) {
        if count > 0 {
            self.metrics
                .prompt_limit_omitted_total
                .fetch_add(count as u64, Ordering::Relaxed);
        }
    }
}

/// Durable semantic-capture replay state stored alongside one run-memory record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMemorySemanticCaptureState {
    /// Semantic capture still needs to be attempted or replayed after a crash.
    #[default]
    Pending,
    /// Semantic capture already finished for this run, including abstentions.
    Completed,
    /// Semantic capture was intentionally skipped for this run.
    Skipped,
}

/// One durable run-memory record owned by the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMemoryRecord {
    /// The session that owns the originating run.
    pub session_id: String,
    /// Visible learning scopes captured with the run for later episodic retrieval.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope_keys: Vec<String>,
    /// Durable replay state for daemon-owned semantic capture on this run.
    #[serde(default)]
    pub semantic_capture: RunMemorySemanticCaptureState,
    /// The recovered-memory payload prepared for prompt injection.
    pub memory: RecoveredMemoryEntry,
}

/// One persisted run-memory pointer retained in the daemon topology index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMemoryIndexEntry {
    /// The referenced run identifier.
    pub run_id: String,
    /// The original capture timestamp used for retention ordering.
    pub recorded_at_ms: u64,
}

/// One session-scoped index of tracked run-memory records.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMemoryIndex {
    /// The tracked run-memory pointers grouped by session, newest first.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_session: BTreeMap<String, Vec<RunMemoryIndexEntry>>,
    /// The tracked run-memory pointers grouped by visible learning scope.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_scope: BTreeMap<String, Vec<RunMemoryIndexEntry>>,
}

impl RunMemoryIndex {
    /// Returns true when the index carries no tracked run-memory pointers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_session.is_empty() && self.by_scope.is_empty()
    }
}

/// The result of rebuilding the session run-memory index from persisted daemon state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RunMemoryIndexRebuild {
    pub(crate) index: RunMemoryIndex,
    pub(crate) pruned_run_ids: Vec<String>,
    pub(crate) pruned_ttl_run_ids: Vec<String>,
    pub(crate) pruned_overflow_run_ids: Vec<String>,
    pub(crate) pruned_orphan_files: Vec<PathBuf>,
}

/// The result of updating one session-scoped run-memory index in place.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RunMemoryIndexUpdate {
    pub(crate) changed: bool,
    pub(crate) pruned_run_ids: Vec<String>,
    pub(crate) pruned_ttl_run_ids: Vec<String>,
    pub(crate) pruned_overflow_run_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RunMemoryPruneResult {
    ttl_run_ids: Vec<String>,
    overflow_run_ids: Vec<String>,
}

impl RunMemoryPruneResult {
    fn all_run_ids(&self) -> Vec<String> {
        self.ttl_run_ids
            .iter()
            .chain(self.overflow_run_ids.iter())
            .cloned()
            .collect()
    }
}

/// Filesystem-backed persistence for compact run-memory records.
#[derive(Clone, Debug)]
pub struct FileRunMemoryStore {
    root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunMemoryStoredFile {
    pub(crate) run_id: Option<String>,
    pub(crate) path: PathBuf,
}

impl FileRunMemoryStore {
    /// Creates a new run-memory store rooted at the provided directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the filesystem path for one run-memory record.
    pub fn run_memory_path(&self, run_id: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.root.join("run-memories"), run_id, "json")
    }

    /// Returns true when one run-memory record exists on disk.
    pub fn has_run_memory(&self, run_id: &str) -> bool {
        self.run_memory_path(run_id).exists()
    }

    /// Saves or replaces one run-memory record.
    pub fn save_run_memory(&self, record: &RunMemoryRecord) -> Result<()> {
        let path = prepare_storage_path_for_write(
            &self.root.join("run-memories"),
            &record.memory.run_id,
            "json",
        )?;
        write_json_pretty_atomically(&path, record)
            .with_context(|| format!("failed to write {}", path.display()))
    }

    /// Loads one run-memory record when present.
    pub fn load_run_memory(&self, run_id: &str) -> Result<Option<RunMemoryRecord>> {
        let path = self.run_memory_path(run_id);
        if !path.exists() {
            return Ok(None);
        }
        let record: RunMemoryRecord = serde_json::from_slice(&fs::read(&path)?)?;
        if record.memory.run_id != run_id {
            bail!(
                "run-memory record id mismatch: requested {}, found {}",
                run_id,
                record.memory.run_id
            );
        }
        Ok(Some(record))
    }

    /// Lists stored run-memory JSON files in both current and legacy namespaces.
    pub(crate) fn list_run_memory_files(&self) -> Result<Vec<RunMemoryStoredFile>> {
        let root = self.root.join("run-memories");
        let mut files = Vec::new();
        collect_legacy_run_memory_files(&root, &mut files)?;
        collect_safe_run_memory_files(&root.join("__safe"), &mut files)?;
        files.sort_by(|left, right| left.path.cmp(&right.path));
        files.dedup_by(|left, right| left.path == right.path);
        Ok(files)
    }

    /// Deletes one run-memory record when present.
    pub fn delete_run_memory(&self, run_id: &str) -> Result<()> {
        let path = self.run_memory_path(run_id);
        let mut paths = vec![path.clone()];
        for file in self.list_run_memory_files()? {
            if file.run_id.as_deref() == Some(run_id) && file.path != path {
                paths.push(file.path);
            }
        }
        for path in paths {
            self.delete_run_memory_file(&path)?;
        }
        Ok(())
    }

    /// Deletes one stored run-memory file discovered during repair.
    pub(crate) fn delete_run_memory_file(&self, path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("failed to delete {}", path.display()))
            }
        }
    }
}

fn collect_legacy_run_memory_files(
    root: &Path,
    files: &mut Vec<RunMemoryStoredFile>,
) -> Result<()> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to scan {}", root.display()));
        }
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let run_id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|stem| !stem.is_empty())
            .map(str::to_string);
        files.push(RunMemoryStoredFile { run_id, path });
    }
    Ok(())
}

fn collect_safe_run_memory_files(root: &Path, files: &mut Vec<RunMemoryStoredFile>) -> Result<()> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to scan {}", root.display()));
        }
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() {
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let run_id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .and_then(decode_safe_storage_name);
        files.push(RunMemoryStoredFile { run_id, path });
    }
    Ok(())
}

/// Rebuilds the session run-memory index from persisted runs and run-memory files.
#[allow(dead_code)]
pub(crate) fn rebuild_run_memory_index(
    runs: &BTreeMap<String, RunRecord>,
    store: &FileRunMemoryStore,
    now_ms: u64,
) -> Result<RunMemoryIndexRebuild> {
    rebuild_run_memory_index_with_policy(runs, store, now_ms, &RunMemoryPolicyConfig::default())
}

/// Rebuilds the session run-memory index using the provided runtime policy.
pub(crate) fn rebuild_run_memory_index_with_policy(
    runs: &BTreeMap<String, RunRecord>,
    store: &FileRunMemoryStore,
    now_ms: u64,
    policy: &RunMemoryPolicyConfig,
) -> Result<RunMemoryIndexRebuild> {
    let mut index = RunMemoryIndex::default();
    let mut records_by_run = BTreeMap::new();
    let mut pruned_run_ids = Vec::new();
    let mut pruned_ttl_run_ids = Vec::new();
    let mut pruned_overflow_run_ids = Vec::new();
    let mut pruned_orphan_files = Vec::new();
    let files = store.list_run_memory_files()?;
    for file in files {
        match file.run_id.as_ref() {
            Some(run_id) => {
                let Some(record) = runs.get(run_id) else {
                    pruned_orphan_files.push(file.path);
                    continue;
                };
                if !record.view.status.is_terminal() || file.path != store.run_memory_path(run_id) {
                    pruned_orphan_files.push(file.path);
                }
            }
            None => pruned_orphan_files.push(file.path),
        }
    }
    if !policy.enabled {
        for record in runs.values() {
            let run_id = &record.view.run_id;
            if store.has_run_memory(run_id) {
                pruned_run_ids.push(run_id.clone());
                pruned_ttl_run_ids.push(run_id.clone());
            }
        }
        return Ok(RunMemoryIndexRebuild {
            index,
            pruned_run_ids,
            pruned_ttl_run_ids,
            pruned_overflow_run_ids,
            pruned_orphan_files,
        });
    }
    for record in runs.values() {
        let run_id = &record.view.run_id;
        if !record.view.status.is_terminal() || !store.has_run_memory(run_id) {
            continue;
        }
        let Some(memory_record) = (match store.load_run_memory(run_id) {
            Ok(memory_record) => memory_record,
            Err(_) => {
                pruned_run_ids.push(run_id.clone());
                continue;
            }
        }) else {
            continue;
        };
        if memory_record.session_id != record.view.session_id {
            pruned_run_ids.push(run_id.clone());
            continue;
        }
        records_by_run.insert(run_id.clone(), memory_record.clone());
        index
            .by_session
            .entry(record.view.session_id.clone())
            .or_default()
            .push(RunMemoryIndexEntry {
                run_id: run_id.clone(),
                recorded_at_ms: record
                    .view
                    .finished_at_ms
                    .unwrap_or(record.view.updated_at_ms),
            });
    }

    for entries in index.by_session.values_mut() {
        let pruned = prune_run_memory_entries(entries, now_ms, policy);
        pruned_ttl_run_ids.extend(pruned.ttl_run_ids.clone());
        pruned_overflow_run_ids.extend(pruned.overflow_run_ids.clone());
        pruned_run_ids.extend(pruned.all_run_ids());
    }
    index.by_session.retain(|_, entries| !entries.is_empty());
    let retained_run_ids = index
        .by_session
        .values()
        .flat_map(|entries| entries.iter().map(|entry| entry.run_id.clone()))
        .collect::<Vec<_>>();
    for run_id in retained_run_ids {
        let Some(record) = records_by_run.get(&run_id) else {
            continue;
        };
        let recorded_at_ms = record.memory.recorded_at_ms;
        for scope_key in scope_keys_for_record(record) {
            index
                .by_scope
                .entry(scope_key)
                .or_default()
                .push(RunMemoryIndexEntry {
                    run_id: run_id.clone(),
                    recorded_at_ms,
                });
        }
    }
    for entries in index.by_scope.values_mut() {
        entries.sort_by(|left, right| {
            right
                .recorded_at_ms
                .cmp(&left.recorded_at_ms)
                .then_with(|| right.run_id.cmp(&left.run_id))
        });
        entries.dedup_by(|left, right| left.run_id == right.run_id);
    }
    index.by_scope.retain(|_, entries| !entries.is_empty());

    Ok(RunMemoryIndexRebuild {
        index,
        pruned_run_ids,
        pruned_ttl_run_ids,
        pruned_overflow_run_ids,
        pruned_orphan_files,
    })
}

/// Records one newly persisted run-memory entry in the daemon topology index.
#[allow(dead_code)]
pub(crate) fn remember_run_memory(
    index: &mut RunMemoryIndex,
    record: &RunMemoryRecord,
    now_ms: u64,
) -> RunMemoryIndexUpdate {
    remember_run_memory_with_policy(index, record, now_ms, &RunMemoryPolicyConfig::default())
}

/// Records one run-memory entry using the provided runtime policy.
pub(crate) fn remember_run_memory_with_policy(
    index: &mut RunMemoryIndex,
    record: &RunMemoryRecord,
    now_ms: u64,
    policy: &RunMemoryPolicyConfig,
) -> RunMemoryIndexUpdate {
    let (pruned, retained) = {
        let entries = index
            .by_session
            .entry(record.session_id.clone())
            .or_default();
        entries.retain(|entry| entry.run_id != record.memory.run_id);
        entries.push(RunMemoryIndexEntry {
            run_id: record.memory.run_id.clone(),
            recorded_at_ms: record.memory.recorded_at_ms,
        });
        let pruned = prune_run_memory_entries(entries, now_ms, policy);
        let retained = entries
            .iter()
            .any(|entry| entry.run_id == record.memory.run_id);
        (pruned, retained)
    };
    remove_run_id_from_scope_index(index, &record.memory.run_id);
    for run_id in pruned
        .ttl_run_ids
        .iter()
        .chain(pruned.overflow_run_ids.iter())
    {
        remove_run_id_from_scope_index(index, run_id);
    }
    if retained {
        for scope_key in scope_keys_for_record(record) {
            index
                .by_scope
                .entry(scope_key)
                .or_default()
                .push(RunMemoryIndexEntry {
                    run_id: record.memory.run_id.clone(),
                    recorded_at_ms: record.memory.recorded_at_ms,
                });
        }
        for entries in index.by_scope.values_mut() {
            entries.sort_by(|left, right| {
                right
                    .recorded_at_ms
                    .cmp(&left.recorded_at_ms)
                    .then_with(|| right.run_id.cmp(&left.run_id))
            });
            entries.dedup_by(|left, right| left.run_id == right.run_id);
        }
    }
    index.by_scope.retain(|_, entries| !entries.is_empty());
    if index
        .by_session
        .get(&record.session_id)
        .is_some_and(|entries| entries.is_empty())
    {
        index.by_session.remove(&record.session_id);
    }
    RunMemoryIndexUpdate {
        changed: true,
        pruned_run_ids: pruned.all_run_ids(),
        pruned_ttl_run_ids: pruned.ttl_run_ids,
        pruned_overflow_run_ids: pruned.overflow_run_ids,
    }
}

/// Removes one stale or unreadable run-memory pointer from the daemon topology index.
pub(crate) fn forget_run_memory(
    index: &mut RunMemoryIndex,
    session_id: &str,
    run_id: &str,
) -> bool {
    let mut changed = remove_run_id_from_scope_index(index, run_id);
    if let Some(entries) = index.by_session.get_mut(session_id) {
        let len_before = entries.len();
        entries.retain(|entry| entry.run_id != run_id);
        changed |= entries.len() != len_before;
        if entries.is_empty() {
            index.by_session.remove(session_id);
        }
    }
    changed
}

fn prune_run_memory_entries(
    entries: &mut Vec<RunMemoryIndexEntry>,
    now_ms: u64,
    policy: &RunMemoryPolicyConfig,
) -> RunMemoryPruneResult {
    entries.sort_by(|left, right| {
        right
            .recorded_at_ms
            .cmp(&left.recorded_at_ms)
            .then_with(|| right.run_id.cmp(&left.run_id))
    });

    let mut retained = Vec::with_capacity(entries.len().min(policy.max_tracked_per_session));
    let mut pruned = RunMemoryPruneResult::default();
    for entry in entries.drain(..) {
        let expired = run_memory_entry_expired(entry.recorded_at_ms, now_ms, policy);
        let overflow = retained.len() >= policy.max_tracked_per_session;
        if expired || overflow {
            if expired {
                pruned.ttl_run_ids.push(entry.run_id);
            } else {
                pruned.overflow_run_ids.push(entry.run_id);
            }
        } else {
            retained.push(entry);
        }
    }
    *entries = retained;
    pruned
}

/// Returns true when the supplied timestamp is outside the configured run-memory TTL.
pub(crate) fn run_memory_entry_expired(
    recorded_at_ms: u64,
    now_ms: u64,
    policy: &RunMemoryPolicyConfig,
) -> bool {
    !policy.enabled || now_ms.saturating_sub(recorded_at_ms) > policy.retention_ms
}

/// Builds one durable run-memory record from a terminal daemon run.
pub fn build_run_memory_record(record: &RunRecord) -> Option<RunMemoryRecord> {
    build_run_memory_record_with_policy(record, &RunMemoryPolicyConfig::default())
        .map(|(record, _)| record)
}

/// Builds one durable run-memory record plus the number of scrubbed fields.
pub(crate) fn build_run_memory_record_with_policy(
    record: &RunRecord,
    policy: &RunMemoryPolicyConfig,
) -> Option<(RunMemoryRecord, usize)> {
    if !policy.enabled {
        return None;
    }
    if !record.view.status.is_terminal() {
        return None;
    }
    let mut redaction_count = 0usize;
    let request_preview = request_preview(record).map(|value| {
        let redacted = redact_run_memory_text(&value, policy.redact_pii);
        redaction_count = redaction_count.saturating_add(redacted.redaction_count);
        truncate_preview(&redacted.text)
    });
    let outcome_preview = outcome_preview(record).map(|value| {
        let redacted = redact_run_memory_text(&value, policy.redact_pii);
        redaction_count = redaction_count.saturating_add(redacted.redaction_count);
        truncate_preview(&redacted.text)
    });
    let failure_markers = failure_markers(record)
        .into_iter()
        .map(|value| {
            let redacted = redact_run_memory_text(&value, policy.redact_pii);
            redaction_count = redaction_count.saturating_add(redacted.redaction_count);
            truncate_preview(&redacted.text)
        })
        .collect::<Vec<_>>();
    let summary = summarize_run_memory_parts(
        &record.view.status,
        record.view.outputs.is_empty(),
        request_preview.as_deref(),
        outcome_preview.as_deref(),
        &failure_markers,
    )?;
    Some((
        RunMemoryRecord {
            session_id: record.view.session_id.clone(),
            scope_keys: Vec::new(),
            semantic_capture: RunMemorySemanticCaptureState::Pending,
            memory: RecoveredMemoryEntry {
                run_id: record.view.run_id.clone(),
                recorded_at_ms: record
                    .view
                    .finished_at_ms
                    .unwrap_or(record.view.updated_at_ms),
                status: run_status_label(&record.view.status).to_string(),
                request_preview,
                outcome_preview,
                artifact_ids: artifact_ids(record),
                failure_markers,
                summary,
            },
        },
        redaction_count,
    ))
}

fn scope_keys_for_record(record: &RunMemoryRecord) -> Vec<String> {
    let mut scope_keys = record
        .scope_keys
        .iter()
        .map(|scope_key| scope_key.trim())
        .filter(|scope_key| !scope_key.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if scope_keys.is_empty() {
        scope_keys.push(format!("session:{}", record.session_id));
    }
    scope_keys.sort();
    scope_keys.dedup();
    scope_keys
}

fn remove_run_id_from_scope_index(index: &mut RunMemoryIndex, run_id: &str) -> bool {
    let mut changed = false;
    index.by_scope.retain(|_, entries| {
        let len_before = entries.len();
        entries.retain(|entry| entry.run_id != run_id);
        changed |= entries.len() != len_before;
        !entries.is_empty()
    });
    changed
}

/// Returns whether one persisted run-memory record can attest the provided candidate content.
pub(crate) fn run_memory_supports_candidate_content(
    record: &RunMemoryRecord,
    needle: &str,
) -> bool {
    let needle = needle.trim();
    if needle.is_empty() {
        return false;
    }
    let needle_terms = crate::learning::semantic_learning_evidence_terms(needle);
    record
        .memory
        .request_preview
        .as_deref()
        .into_iter()
        .chain(record.memory.outcome_preview.as_deref())
        .chain(std::iter::once(record.memory.summary.as_str()))
        .chain(record.memory.failure_markers.iter().map(String::as_str))
        .any(|value| {
            value.contains(needle)
                || (needle_terms.len() >= 2 && {
                    let value_terms = crate::learning::semantic_learning_evidence_terms(value);
                    needle_terms.iter().all(|term| value_terms.contains(term))
                })
        })
}

fn request_preview(record: &RunRecord) -> Option<String> {
    if let Some(preview) = raw_payload_request_preview(record) {
        return Some(preview);
    }
    record
        .view
        .request
        .text_preview
        .as_deref()
        .map(str::trim)
        .filter(|request| !request.is_empty())
        .map(str::to_string)
}

fn raw_payload_request_preview(record: &RunRecord) -> Option<String> {
    match &record.payload {
        RunRequestPayload::Input { request, .. }
        | RunRequestPayload::ScheduledInput { request, .. } => {
            submit_input_request_memory_preview(request)
        }
        RunRequestPayload::ObservationMaterialization { .. }
        | RunRequestPayload::ScheduledObservationMaterialization { .. }
        | RunRequestPayload::ChannelDelivery { .. }
        | RunRequestPayload::MailboxDelivery { .. }
        | RunRequestPayload::ParentClarification { .. } => None,
        RunRequestPayload::ApprovalResume {
            original_request, ..
        }
        | RunRequestPayload::UserQuestionResume {
            original_request, ..
        } => original_request
            .as_ref()
            .and_then(submit_input_request_memory_preview),
    }
}

fn submit_input_request_memory_preview(request: &crate::SubmitInputRequest) -> Option<String> {
    if !request.input_items.is_empty() {
        let preview = request
            .input_items
            .iter()
            .filter_map(submit_input_item_memory_preview)
            .collect::<Vec<_>>()
            .join(" ");
        return (!preview.trim().is_empty()).then_some(preview);
    }
    let content = request.content.trim();
    if content.is_empty() {
        return None;
    }
    if request.attachments.is_empty() {
        Some(content.to_string())
    } else {
        Some(format!(
            "{} [{} attachment(s)]",
            content,
            request.attachments.len()
        ))
    }
}

fn submit_input_item_memory_preview(item: &SubmitInputItemRequest) -> Option<String> {
    match item {
        SubmitInputItemRequest::Text { text } => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        SubmitInputItemRequest::AssetReference { asset_id } => Some(format!("[asset:{asset_id}]")),
        SubmitInputItemRequest::BoardReference {
            board_id,
            revision_id,
        } => {
            let suffix = revision_id
                .as_deref()
                .map(|revision_id| format!("@{revision_id}"))
                .unwrap_or_default();
            Some(format!("[board:{board_id}{suffix}]"))
        }
        SubmitInputItemRequest::InlineAsset(upload) => {
            let media_type = upload.media_type.as_deref().unwrap_or("unknown");
            Some(format!("[inline_asset:{} {media_type}]", upload.file_name))
        }
    }
}

fn outcome_preview(record: &RunRecord) -> Option<String> {
    record.view.outputs.iter().rev().find_map(|output| {
        let content = output.content.trim();
        (!content.is_empty()).then(|| content.to_string())
    })
}

fn artifact_ids(record: &RunRecord) -> Vec<String> {
    let mut collected = Vec::new();
    for attachment in &record.view.input_attachments {
        push_unique_artifact_id(&mut collected, &attachment.id);
    }
    for output in &record.view.outputs {
        for part in &output.parts {
            if let ContentPart::Attachment { attachment } = part {
                push_unique_artifact_id(&mut collected, &attachment.id);
            }
        }
        for artifact in &output.artifacts {
            push_unique_artifact_id(&mut collected, &artifact.id);
        }
    }
    collected
}

fn push_unique_artifact_id(collected: &mut Vec<String>, artifact_id: &str) {
    let normalized = artifact_id.trim();
    if normalized.is_empty() || collected.iter().any(|existing| existing == normalized) {
        return;
    }
    collected.push(normalized.to_string());
}

fn failure_markers(record: &RunRecord) -> Vec<String> {
    let mut markers = Vec::new();
    match record.view.status {
        DaemonRunStatus::Failed => markers.push("failed".to_string()),
        DaemonRunStatus::Interrupted => markers.push("interrupted".to_string()),
        DaemonRunStatus::Cancelled => markers.push("cancelled".to_string()),
        DaemonRunStatus::Completed
        | DaemonRunStatus::Queued
        | DaemonRunStatus::Running
        | DaemonRunStatus::WaitingForApproval
        | DaemonRunStatus::WaitingForUserQuestion => {}
    }
    if let Some(error) = record
        .view
        .error
        .as_deref()
        .map(str::trim)
        .filter(|error| !error.is_empty())
    {
        markers.push(format!("error: {error}"));
    }
    markers
}

fn summarize_run_memory_parts(
    status: &DaemonRunStatus,
    outputs_empty: bool,
    request_preview: Option<&str>,
    outcome_preview: Option<&str>,
    failure_markers: &[String],
) -> Option<String> {
    let mut lines = Vec::new();
    if let Some(request) = request_preview {
        lines.push(format!("Request: {request}"));
    }
    if let Some(output) = outcome_preview {
        lines.push(format!("Result: {output}"));
    }

    match status {
        DaemonRunStatus::Completed => {
            if outputs_empty {
                lines.push("Status: completed without recorded text output.".to_string());
            }
        }
        DaemonRunStatus::Failed => {
            if let Some(error) = failure_markers
                .iter()
                .find_map(|marker| marker.strip_prefix("error: "))
                .map(str::trim)
                .filter(|error| !error.is_empty())
            {
                lines.push(format!("Error: {error}"));
            } else {
                lines.push("Status: failed.".to_string());
            }
        }
        DaemonRunStatus::Interrupted => {
            lines.push("Status: interrupted before completion.".to_string());
        }
        DaemonRunStatus::Cancelled => {
            lines.push("Status: cancelled before completion.".to_string());
        }
        DaemonRunStatus::Queued
        | DaemonRunStatus::Running
        | DaemonRunStatus::WaitingForApproval
        | DaemonRunStatus::WaitingForUserQuestion => return None,
    }

    let summary = truncate_summary(&lines.join("\n"));
    (!summary.is_empty()).then_some(summary)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RedactedRunMemoryText {
    text: String,
    redaction_count: usize,
}

fn redact_run_memory_text(text: &str, redact_pii: bool) -> RedactedRunMemoryText {
    let secret_redacted = redact_text(text);
    let mut redaction_count = usize::from(secret_redacted != text);
    let (text, pii_redactions) = if redact_pii {
        redact_pii_tokens(&secret_redacted)
    } else {
        (secret_redacted, 0)
    };
    redaction_count = redaction_count.saturating_add(pii_redactions);
    RedactedRunMemoryText {
        text,
        redaction_count,
    }
}

fn redact_pii_tokens(text: &str) -> (String, usize) {
    let (text, mut redactions) = redact_pii_number_sequences(text);
    let mut rendered = String::with_capacity(text.len());
    let mut token = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            redactions =
                redactions.saturating_add(push_redacted_or_original_token(&mut rendered, &token));
            token.clear();
            rendered.push(ch);
        } else {
            token.push(ch);
        }
    }
    redactions = redactions.saturating_add(push_redacted_or_original_token(&mut rendered, &token));
    (rendered, redactions)
}

fn redact_pii_number_sequences(text: &str) -> (String, usize) {
    let mut rendered = String::with_capacity(text.len());
    let mut index = 0usize;
    let mut redactions = 0usize;
    while index < text.len() {
        let ch = text[index..]
            .chars()
            .next()
            .expect("index should be on a char boundary");
        if is_pii_number_sequence_start(ch) {
            let start = index;
            let mut end = index + ch.len_utf8();
            let mut last_digit_end = ch.is_ascii_digit().then_some(end);
            while end < text.len() {
                let next = text[end..]
                    .chars()
                    .next()
                    .expect("index should be on a char boundary");
                if next.is_ascii_digit() {
                    end += next.len_utf8();
                    last_digit_end = Some(end);
                } else if is_pii_number_sequence_separator(next) {
                    let next_end = end + next.len_utf8();
                    if next.is_whitespace() && !next_non_space_is_digit(text, next_end) {
                        break;
                    }
                    end = next_end;
                } else {
                    break;
                }
            }
            if let Some(candidate_end) = last_digit_end {
                let candidate = &text[start..candidate_end];
                if let Some((marker, redacted_end)) =
                    pii_number_sequence_redaction(candidate, start)
                {
                    rendered.push_str(marker);
                    redactions = redactions.saturating_add(1);
                    index = redacted_end;
                    continue;
                }
            }
        }
        rendered.push(ch);
        index += ch.len_utf8();
    }
    (rendered, redactions)
}

fn is_pii_number_sequence_start(ch: char) -> bool {
    ch.is_ascii_digit() || matches!(ch, '+' | '(')
}

fn is_pii_number_sequence_separator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '+' | '-' | '(' | ')' | '.')
}

fn next_non_space_is_digit(text: &str, index: usize) -> bool {
    text[index..]
        .chars()
        .find(|ch| !ch.is_whitespace())
        .is_some_and(|ch| ch.is_ascii_digit())
}

fn pii_number_sequence_redaction(
    candidate: &str,
    absolute_start: usize,
) -> Option<(&'static str, usize)> {
    if let Some(marker) = pii_number_sequence_marker(candidate) {
        return Some((marker, absolute_start + candidate.len()));
    }
    if candidate.starts_with('+') || candidate.starts_with('(') {
        let mut digit_count = 0usize;
        for (offset, ch) in candidate.char_indices() {
            if ch.is_ascii_digit() {
                digit_count = digit_count.saturating_add(1);
                if digit_count == 10 || digit_count == 11 {
                    let prefix_end = offset + ch.len_utf8();
                    let prefix = &candidate[..prefix_end];
                    if looks_like_phone_number(prefix) {
                        return Some(("<redacted:phone>", absolute_start + prefix_end));
                    }
                } else if digit_count > 11 {
                    break;
                }
            }
        }
    }
    None
}

fn pii_number_sequence_marker(candidate: &str) -> Option<&'static str> {
    if looks_like_credit_card(candidate) {
        Some("<redacted:card>")
    } else if looks_like_phone_number(candidate) {
        Some("<redacted:phone>")
    } else {
        None
    }
}

fn push_redacted_or_original_token(rendered: &mut String, token: &str) -> usize {
    if token.is_empty() {
        return 0;
    }
    if looks_like_email(token) {
        rendered.push_str("<redacted:email>");
        return 1;
    }
    if looks_like_ssn(token) {
        rendered.push_str("<redacted:ssn>");
        return 1;
    }
    if looks_like_credit_card(token) {
        rendered.push_str("<redacted:card>");
        return 1;
    }
    if looks_like_phone_number(token) {
        rendered.push_str("<redacted:phone>");
        return 1;
    }
    rendered.push_str(token);
    0
}

fn trimmed_pii_token(token: &str) -> &str {
    token.trim_matches(|ch: char| {
        matches!(
            ch,
            '.' | ','
                | ';'
                | ':'
                | '!'
                | '?'
                | '"'
                | '\''
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
        )
    })
}

fn looks_like_email(token: &str) -> bool {
    let token = trimmed_pii_token(token);
    let Some((local, domain)) = token.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '@' | '.' | '_' | '%' | '+' | '-'))
}

fn looks_like_ssn(token: &str) -> bool {
    let token = trimmed_pii_token(token);
    let bytes = token.as_bytes();
    bytes.len() == 11
        && bytes[3] == b'-'
        && bytes[6] == b'-'
        && bytes[..3].iter().all(u8::is_ascii_digit)
        && bytes[4..6].iter().all(u8::is_ascii_digit)
        && bytes[7..].iter().all(u8::is_ascii_digit)
}

fn looks_like_credit_card(token: &str) -> bool {
    let token = trimmed_pii_token(token);
    if !token
        .chars()
        .all(|ch| ch.is_ascii_digit() || ch == '-' || ch == ' ')
    {
        return false;
    }
    let digits = token
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    (13..=19).contains(&digits.len()) && luhn_valid(&digits)
}

fn looks_like_phone_number(token: &str) -> bool {
    let token = trimmed_pii_token(token);
    if !token.chars().all(|ch| {
        ch.is_ascii_digit() || ch.is_whitespace() || matches!(ch, '+' | '-' | '(' | ')' | '.')
    }) {
        return false;
    }
    if !token
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '+' | '-' | '(' | ')' | '.'))
    {
        return false;
    }
    let digits = token
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if token.starts_with('+') {
        digits.len() == 11 && digits.starts_with('1')
    } else {
        digits.len() == 10 || (digits.len() == 11 && digits.starts_with('1'))
    }
}

fn luhn_valid(digits: &str) -> bool {
    let mut sum = 0u32;
    let mut double = false;
    for ch in digits.chars().rev() {
        let Some(mut value) = ch.to_digit(10) else {
            return false;
        };
        if double {
            value *= 2;
            if value > 9 {
                value -= 9;
            }
        }
        sum += value;
        double = !double;
    }
    sum != 0 && sum % 10 == 0
}

/// Scores one recovered run-memory record against the pending input text.
pub(crate) fn rank_run_memory_record(record: &RunMemoryRecord, query: Option<&str>) -> u64 {
    let Some(query) = query.map(str::trim).filter(|query| !query.is_empty()) else {
        return 0;
    };
    let normalized_query = normalize_rank_text(query);
    let query_terms = memory_terms(&normalized_query);
    if normalized_query.is_empty() && query_terms.is_empty() {
        return 0;
    }
    let mut score = 0u64;
    score = score.saturating_add(score_rank_field(
        &record.memory.summary,
        &normalized_query,
        &query_terms,
        4,
    ));
    if let Some(request) = record.memory.request_preview.as_deref() {
        score = score.saturating_add(score_rank_field(
            request,
            &normalized_query,
            &query_terms,
            3,
        ));
    }
    if let Some(outcome) = record.memory.outcome_preview.as_deref() {
        score = score.saturating_add(score_rank_field(
            outcome,
            &normalized_query,
            &query_terms,
            3,
        ));
    }
    if !record.memory.failure_markers.is_empty() {
        score = score.saturating_add(score_rank_field(
            &record.memory.failure_markers.join(" "),
            &normalized_query,
            &query_terms,
            1,
        ));
    }
    score
}

fn score_rank_field(
    value: &str,
    normalized_query: &str,
    query_terms: &BTreeSet<String>,
    weight: u64,
) -> u64 {
    let normalized_value = normalize_rank_text(value);
    if normalized_value.is_empty() {
        return 0;
    }
    let mut score = 0u64;
    if !normalized_query.is_empty() && normalized_value.contains(normalized_query) {
        score = score.saturating_add(100 * weight);
    }
    let value_terms = memory_terms(&normalized_value);
    let matched_terms = query_terms
        .iter()
        .filter(|term| value_terms.contains(*term))
        .count() as u64;
    if matched_terms > 0 {
        score = score.saturating_add(matched_terms * 15 * weight);
        if matched_terms as usize == query_terms.len() {
            score = score.saturating_add(40 * weight);
        }
    }
    score
}

fn memory_terms(value: &str) -> BTreeSet<String> {
    normalize_rank_text(value)
        .split_whitespace()
        .filter(|term| term.chars().count() >= 3)
        .map(str::to_string)
        .collect()
}

fn normalize_rank_text(value: &str) -> String {
    let mut rendered = String::with_capacity(value.len());
    let mut previous_space = true;
    for ch in value.chars() {
        if ch.is_alphanumeric() {
            for lowered in ch.to_lowercase() {
                rendered.push(lowered);
            }
            previous_space = false;
        } else if !previous_space {
            rendered.push(' ');
            previous_space = true;
        }
    }
    rendered.trim().to_string()
}

fn truncate_summary(content: &str) -> String {
    let mut truncated = content
        .trim()
        .chars()
        .take(MAX_MEMORY_SUMMARY_CHARS)
        .collect::<String>();
    if content.trim().chars().count() > MAX_MEMORY_SUMMARY_CHARS {
        truncated.push('…');
    }
    truncated
}

fn run_status_label(status: &DaemonRunStatus) -> &'static str {
    match status {
        DaemonRunStatus::Queued => "queued",
        DaemonRunStatus::Running => "running",
        DaemonRunStatus::WaitingForApproval => "waiting_for_approval",
        DaemonRunStatus::WaitingForUserQuestion => "waiting_for_user_question",
        DaemonRunStatus::Completed => "completed",
        DaemonRunStatus::Failed => "failed",
        DaemonRunStatus::Interrupted => "interrupted",
        DaemonRunStatus::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use anyhow::Result;
    use kheish_session::safe_storage_name;

    use crate::{RunRequestPayload, RunRequestSummary, RunView, SubmitInputRequest};

    use super::*;

    fn sample_run_record(run_id: &str) -> RunRecord {
        RunRecord {
            view: RunView {
                run_id: run_id.to_string(),
                session_id: "session-a".to_string(),
                agent_id: "agent-1".to_string(),
                kind: crate::DaemonRunKind::Input,
                status: DaemonRunStatus::Completed,
                submitted_at_ms: 1,
                updated_at_ms: 2,
                started_at_ms: Some(1),
                finished_at_ms: Some(2),
                queued_position: None,
                request: RunRequestSummary {
                    source_plugin: "daemon".to_string(),
                    source_kind: "api".to_string(),
                    actor_id: "tester".to_string(),
                    text_preview: Some("inspect the repo".to_string()),
                    provider: Some("openai".to_string()),
                    model: Some("gpt-5.4".to_string()),
                    approval_count: None,
                    question_count: None,
                },
                input_attachments: Vec::new(),
                input_metadata: None,
                pending_approval_ids: Vec::new(),
                pending_approvals: Vec::new(),
                pending_question_ids: Vec::new(),
                pending_questions: Vec::new(),
                outputs: vec![crate::DaemonOutputRecord {
                    session_id: "session-a".to_string(),
                    run_id: Some(run_id.to_string()),
                    content: "done".to_string(),
                    parts: Vec::new(),
                    artifacts: Vec::new(),
                    source_kind: None,
                    plugin: Some("daemon".to_string()),
                    address: Some("session-a".to_string()),
                }],
                deliveries: Vec::new(),
                error: None,
            },
            reply_targets: Vec::new(),
            payload: RunRequestPayload::Input {
                request: SubmitInputRequest {
                    provider: None,
                    source_plugin: Some("daemon".to_string()),
                    source_kind: Some("api".to_string()),
                    actor_id: Some("tester".to_string()),
                    content: "inspect the repo".to_string(),
                    input_items: Vec::new(),
                    attachments: Vec::new(),
                    generation: None,
                    completion_requirements: None,
                    metadata: None,
                    binding_keys: Vec::new(),
                    reply_targets: Vec::new(),
                    reply_plugin: None,
                    reply_address: None,
                },
                idempotency: None,
            },
        }
    }

    fn sample_run_record_at(session_id: &str, run_id: &str, finished_at_ms: u64) -> RunRecord {
        let mut record = sample_run_record(run_id);
        record.view.session_id = session_id.to_string();
        record.view.updated_at_ms = finished_at_ms;
        record.view.finished_at_ms = Some(finished_at_ms);
        record
    }

    #[test]
    fn run_memory_store_uses_safe_filenames_for_hostile_run_ids() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileRunMemoryStore::new(root.path());
        let run_id = "../../../tmp/evil-run";
        let record = build_run_memory_record(&sample_run_record(run_id))
            .expect("terminal runs should produce memory");
        store.save_run_memory(&record)?;

        let expected = root
            .path()
            .join("run-memories")
            .join("__safe")
            .join(format!("{}.json", safe_storage_name(run_id)));
        assert!(expected.exists());
        assert!(
            !root
                .path()
                .join("run-memories/../../../tmp/evil-run.json")
                .exists()
        );
        assert_eq!(
            store
                .load_run_memory(run_id)?
                .map(|value| value.memory.run_id),
            Some(run_id.to_string())
        );
        Ok(())
    }

    #[test]
    fn run_memory_store_round_trips_after_restart() -> Result<()> {
        let root = tempfile::tempdir()?;
        let record = build_run_memory_record(&sample_run_record("run-1"))
            .expect("terminal runs should produce memory");
        FileRunMemoryStore::new(root.path()).save_run_memory(&record)?;

        let reloaded = FileRunMemoryStore::new(root.path())
            .load_run_memory("run-1")?
            .expect("memory should reload");
        assert_eq!(reloaded, record);
        Ok(())
    }

    #[test]
    fn run_memory_store_rejects_mismatched_record_run_id() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileRunMemoryStore::new(root.path());
        let record = build_run_memory_record(&sample_run_record("run-1"))
            .expect("terminal runs should produce memory");
        store.save_run_memory(&record)?;
        let mut mismatched = record;
        mismatched.memory.run_id = "run-evil".to_string();
        fs::write(
            store.run_memory_path("run-1"),
            serde_json::to_vec_pretty(&mismatched)?,
        )?;

        assert!(store.load_run_memory("run-1").is_err());
        Ok(())
    }

    #[test]
    fn run_memory_store_deletes_records() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileRunMemoryStore::new(root.path());
        let record = build_run_memory_record(&sample_run_record("run-delete"))
            .expect("terminal runs should produce memory");
        store.save_run_memory(&record)?;
        let legacy_path = root.path().join("run-memories").join("run-delete.json");
        fs::write(&legacy_path, serde_json::to_vec_pretty(&record)?)?;
        assert!(store.has_run_memory("run-delete"));
        store.delete_run_memory("run-delete")?;
        assert!(!store.has_run_memory("run-delete"));
        assert!(!legacy_path.exists());
        Ok(())
    }

    #[test]
    fn build_run_memory_record_captures_request_result_and_status() {
        let record = build_run_memory_record(&sample_run_record("run-2"))
            .expect("terminal runs should produce memory");
        assert_eq!(
            record.semantic_capture,
            RunMemorySemanticCaptureState::Pending
        );
        assert_eq!(record.memory.status, "completed");
        assert_eq!(
            record.memory.request_preview.as_deref(),
            Some("inspect the repo")
        );
        assert_eq!(record.memory.outcome_preview.as_deref(), Some("done"));
        assert!(record.memory.artifact_ids.is_empty());
        assert!(record.memory.failure_markers.is_empty());
        assert!(record.memory.summary.contains("Request: inspect the repo"));
        assert!(record.memory.summary.contains("Result: done"));
    }

    #[test]
    fn build_run_memory_record_redacts_secrets_and_common_pii_before_storage() {
        let mut source = sample_run_record("run-redact");
        let private_key_label = "PRIVATE KEY";
        let private_key_body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC";
        let token_secret = format!("{}{}", "sk-", "proj-secret");
        let api_key_secret = format!("{}{}", "sk-", "proj-colon-secret");
        let client_secret = format!("{}{}", "github", "_pat_client_secret");
        source.view.request.text_preview = Some(format!(
            "-----BEGIN {private_key_label}-----\n{private_key_body}\n-----END {private_key_label}-----\nEmail alice@example.com SSN 123-45-6789 phone 555-123-4567 card 4111-1111-1111-1111 token {token_secret} api-key: {api_key_secret} clientSecret: {client_secret} spaced +1 415 555 2671 card 4242 4242 4242 4242"
        ));
        if let RunRequestPayload::Input { request, .. } = &mut source.payload {
            request.content = source
                .view
                .request
                .text_preview
                .clone()
                .expect("request preview should be set");
        }
        let json_secret = format!("{}{}", "sk-", "ant-json-secret");
        source.view.outputs[0].content = format!(
            "Stored webhook https://hooks.example.test/incoming?token=hook-secret&signature=sig-secret jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.sflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c bob@example.com with Bearer raw-token and \"x-api-key\": \"{json_secret}\" plus +1 415 555 2671 4242 4242 4242 4242"
        );

        let record = build_run_memory_record(&source).expect("terminal runs should produce memory");
        let serialized = serde_json::to_string(&record).expect("record should serialize");
        for forbidden in [
            "alice@example.com".to_string(),
            "123-45-6789".to_string(),
            "555-123-4567".to_string(),
            "4111-1111-1111-1111".to_string(),
            "+1 415 555 2671".to_string(),
            "4242 4242 4242 4242".to_string(),
            token_secret,
            api_key_secret,
            client_secret,
            json_secret,
            "bob@example.com".to_string(),
            "raw-token".to_string(),
            "eyJhbGciOiJIUzI1NiJ9".to_string(),
            "hook-secret".to_string(),
            "sig-secret".to_string(),
            "MIIEvQIBADAN".to_string(),
        ] {
            assert!(
                !serialized.contains(forbidden.as_str()),
                "run memory leaked {forbidden}: {serialized}"
            );
        }
        assert!(serialized.contains("<redacted:email>"));
        assert!(serialized.contains("<redacted:ssn>"));
        assert!(serialized.contains("<redacted:phone>"));
        assert!(!serialized.contains("<redacted:phone>1"));
        assert!(serialized.contains("<redacted:card>"));
        assert!(serialized.contains("<redacted:private-key>"));
        assert!(serialized.contains("token=<redacted>"));
        assert!(serialized.contains("signature=<redacted>"));
        assert!(serialized.contains("<redacted>"));
    }

    #[test]
    fn run_memory_ranker_prefers_query_relevant_records() {
        let mut atlas = build_run_memory_record(&sample_run_record("run-atlas"))
            .expect("terminal runs should produce memory");
        atlas.memory.summary =
            "Request: remember Atlas launch codename\nResult: Atlas stored".to_string();
        let mut newer_irrelevant = build_run_memory_record(&sample_run_record("run-banana"))
            .expect("terminal runs should produce memory");
        newer_irrelevant.memory.summary =
            "Request: remember banana inventory\nResult: banana stored".to_string();

        assert!(
            rank_run_memory_record(&atlas, Some("What is the Atlas codename?"))
                > rank_run_memory_record(&newer_irrelevant, Some("What is the Atlas codename?"))
        );
    }

    #[test]
    fn run_memory_ranker_scores_unicode_exact_matches() {
        let mut tokyo = build_run_memory_record(&sample_run_record("run-tokyo"))
            .expect("terminal runs should produce memory");
        tokyo.memory.summary = "Request: remember 東京 office color\nResult: copper".to_string();
        let mut newer_irrelevant = build_run_memory_record(&sample_run_record("run-banana"))
            .expect("terminal runs should produce memory");
        newer_irrelevant.memory.summary =
            "Request: remember banana inventory\nResult: banana stored".to_string();

        assert!(
            rank_run_memory_record(&tokyo, Some("東京"))
                > rank_run_memory_record(&newer_irrelevant, Some("東京"))
        );
    }

    #[test]
    fn run_memory_policy_rejects_invalid_enabled_limits() {
        let invalid = RunMemoryPolicyConfig {
            retention_ms: 0,
            ..RunMemoryPolicyConfig::default()
        };
        assert!(invalid.validate().is_err());
        let invalid = RunMemoryPolicyConfig {
            max_prompt_entries: 2,
            max_tracked_per_session: 1,
            ..RunMemoryPolicyConfig::default()
        };
        assert!(invalid.validate().is_err());
        let invalid = RunMemoryPolicyConfig {
            max_prompt_entries: 0,
            ..RunMemoryPolicyConfig::default()
        };
        assert!(invalid.validate().is_err());
        let disabled = RunMemoryPolicyConfig {
            enabled: false,
            retention_ms: 0,
            max_tracked_per_session: 0,
            max_prompt_entries: 0,
            ..RunMemoryPolicyConfig::default()
        };
        assert!(disabled.validate().is_ok());
    }

    #[test]
    fn remember_run_memory_prunes_expired_and_excess_entries() {
        let now_ms = RUN_MEMORY_RETENTION_MS + 10_000;
        let mut index = RunMemoryIndex {
            by_session: BTreeMap::from([(
                "session-a".to_string(),
                vec![RunMemoryIndexEntry {
                    run_id: "expired".to_string(),
                    recorded_at_ms: 1,
                }],
            )]),
            by_scope: BTreeMap::new(),
        };

        for offset in 0..=MAX_TRACKED_RUN_MEMORIES_PER_SESSION {
            let record = RunMemoryRecord {
                session_id: "session-a".to_string(),
                scope_keys: vec!["session:session-a".to_string()],
                semantic_capture: RunMemorySemanticCaptureState::Completed,
                memory: RecoveredMemoryEntry {
                    run_id: format!("run-{offset}"),
                    recorded_at_ms: now_ms.saturating_sub(offset as u64),
                    status: "completed".to_string(),
                    request_preview: None,
                    outcome_preview: None,
                    artifact_ids: Vec::new(),
                    failure_markers: Vec::new(),
                    summary: format!("summary-{offset}"),
                },
            };
            let _ = remember_run_memory(&mut index, &record, now_ms);
        }

        let entries = index
            .by_session
            .get("session-a")
            .expect("session index should exist");
        assert_eq!(entries.len(), MAX_TRACKED_RUN_MEMORIES_PER_SESSION);
        assert_eq!(entries[0].run_id, "run-0");
        let by_scope = index
            .by_scope
            .get("session:session-a")
            .expect("scope index should exist");
        assert_eq!(by_scope.len(), MAX_TRACKED_RUN_MEMORIES_PER_SESSION);
        assert!(
            entries
                .iter()
                .all(|entry| now_ms.saturating_sub(entry.recorded_at_ms) <= RUN_MEMORY_RETENTION_MS)
        );
        assert!(!entries.iter().any(|entry| entry.run_id == "expired"));
    }

    #[test]
    fn run_memory_supports_candidate_content_checks_daemon_owned_fields() {
        let record = RunMemoryRecord {
            session_id: "session-1".to_string(),
            scope_keys: vec!["session:session-1".to_string()],
            semantic_capture: RunMemorySemanticCaptureState::Completed,
            memory: RecoveredMemoryEntry {
                run_id: "run-1".to_string(),
                recorded_at_ms: 42,
                status: "completed".to_string(),
                request_preview: Some("Preference: Preferred editor is Helix".to_string()),
                outcome_preview: Some("Remembered Preferred editor is Helix".to_string()),
                artifact_ids: Vec::new(),
                failure_markers: vec!["marker".to_string()],
                summary: "Result: Preferred editor is Helix".to_string(),
            },
        };

        assert!(run_memory_supports_candidate_content(
            &record,
            "Preferred editor is Helix"
        ));
        assert!(run_memory_supports_candidate_content(
            &record,
            "The preferred editor: Helix"
        ));
        assert!(!run_memory_supports_candidate_content(&record, "Atlas"));
    }

    #[test]
    fn rebuild_run_memory_index_prunes_missing_stale_and_overflow_records() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileRunMemoryStore::new(root.path());
        let now_ms = RUN_MEMORY_RETENTION_MS + 20_000;
        let mut runs = BTreeMap::new();

        let stale = sample_run_record_at("session-a", "stale", 1);
        let stale_memory = build_run_memory_record(&stale).expect("stale run should persist");
        store.save_run_memory(&stale_memory)?;
        runs.insert("stale".to_string(), stale);

        for offset in 0..=MAX_TRACKED_RUN_MEMORIES_PER_SESSION {
            let record = sample_run_record_at(
                "session-a",
                &format!("run-{offset}"),
                now_ms.saturating_sub(offset as u64),
            );
            let memory = build_run_memory_record(&record).expect("terminal run should persist");
            store.save_run_memory(&memory)?;
            runs.insert(record.view.run_id.clone(), record);
        }

        runs.insert(
            "missing".to_string(),
            sample_run_record_at("session-a", "missing", now_ms),
        );

        let rebuilt = rebuild_run_memory_index(&runs, &store, now_ms)?;
        let entries = rebuilt
            .index
            .by_session
            .get("session-a")
            .expect("session index should exist");
        assert_eq!(entries.len(), MAX_TRACKED_RUN_MEMORIES_PER_SESSION);
        assert_eq!(entries[0].run_id, "run-0");
        assert_eq!(
            rebuilt
                .index
                .by_scope
                .get("session:session-a")
                .expect("scope index should exist")
                .len(),
            MAX_TRACKED_RUN_MEMORIES_PER_SESSION
        );
        assert!(rebuilt.pruned_run_ids.contains(&"stale".to_string()));
        assert!(
            rebuilt
                .pruned_run_ids
                .contains(&format!("run-{}", MAX_TRACKED_RUN_MEMORIES_PER_SESSION))
        );
        assert!(!rebuilt.pruned_run_ids.contains(&"missing".to_string()));
        Ok(())
    }

    #[test]
    fn rebuild_run_memory_index_prunes_mismatched_session_records() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileRunMemoryStore::new(root.path());
        let now_ms = RUN_MEMORY_RETENTION_MS + 20_000;
        let record = sample_run_record_at("session-a", "run-1", now_ms);
        let mut memory = build_run_memory_record(&record).expect("terminal run should persist");
        memory.session_id = "session-b".to_string();
        store.save_run_memory(&memory)?;
        let runs = BTreeMap::from([("run-1".to_string(), record)]);

        let rebuilt = rebuild_run_memory_index(&runs, &store, now_ms)?;
        assert!(rebuilt.index.by_session.is_empty());
        assert!(rebuilt.index.by_scope.is_empty());
        assert!(rebuilt.pruned_run_ids.contains(&"run-1".to_string()));
        Ok(())
    }

    #[test]
    fn rebuild_run_memory_index_reports_orphaned_store_files() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileRunMemoryStore::new(root.path());
        let now_ms = RUN_MEMORY_RETENTION_MS + 20_000;

        let live = sample_run_record_at("session-a", "run-live", now_ms);
        let live_memory = build_run_memory_record(&live).expect("terminal run should persist");
        store.save_run_memory(&live_memory)?;

        let orphan = sample_run_record_at("session-a", "run-orphan", now_ms);
        let orphan_memory = build_run_memory_record(&orphan).expect("terminal run should persist");
        store.save_run_memory(&orphan_memory)?;

        let invalid_safe_path = root
            .path()
            .join("run-memories")
            .join("__safe")
            .join("not-a-safe-id.json");
        fs::write(&invalid_safe_path, "{}")?;

        let runs = BTreeMap::from([("run-live".to_string(), live)]);
        let rebuilt = rebuild_run_memory_index(&runs, &store, now_ms)?;
        assert_eq!(
            rebuilt.index.by_session.get("session-a").map(Vec::len),
            Some(1)
        );
        assert!(
            rebuilt
                .pruned_orphan_files
                .iter()
                .any(|path| path == &store.run_memory_path("run-orphan"))
        );
        assert!(
            rebuilt
                .pruned_orphan_files
                .iter()
                .any(|path| path == &invalid_safe_path)
        );
        assert!(
            !rebuilt
                .pruned_orphan_files
                .iter()
                .any(|path| path == &store.run_memory_path("run-live"))
        );

        for path in &rebuilt.pruned_orphan_files {
            store.delete_run_memory_file(path)?;
        }
        assert!(store.has_run_memory("run-live"));
        assert!(!store.has_run_memory("run-orphan"));
        assert!(!invalid_safe_path.exists());
        Ok(())
    }

    #[test]
    fn rebuild_run_memory_index_reports_non_terminal_store_files() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileRunMemoryStore::new(root.path());
        let now_ms = RUN_MEMORY_RETENTION_MS + 20_000;

        let mut run = sample_run_record_at("session-a", "run-pending", now_ms);
        let memory = build_run_memory_record(&run).expect("terminal run should persist");
        store.save_run_memory(&memory)?;
        run.view.status = DaemonRunStatus::Running;
        run.view.finished_at_ms = None;

        let runs = BTreeMap::from([("run-pending".to_string(), run)]);
        let rebuilt = rebuild_run_memory_index(&runs, &store, now_ms)?;
        assert!(rebuilt.index.by_session.is_empty());
        assert!(rebuilt.index.by_scope.is_empty());
        assert!(
            rebuilt
                .pruned_orphan_files
                .iter()
                .any(|path| path == &store.run_memory_path("run-pending"))
        );
        Ok(())
    }

    #[test]
    fn rebuild_run_memory_index_reports_duplicate_legacy_store_files() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileRunMemoryStore::new(root.path());
        let now_ms = RUN_MEMORY_RETENTION_MS + 20_000;

        let live = sample_run_record_at("session-a", "run-live", now_ms);
        let live_memory = build_run_memory_record(&live).expect("terminal run should persist");
        store.save_run_memory(&live_memory)?;
        let safe_path = store.run_memory_path("run-live");
        let legacy_path = root.path().join("run-memories").join("run-live.json");
        fs::write(&legacy_path, serde_json::to_vec_pretty(&live_memory)?)?;

        let runs = BTreeMap::from([("run-live".to_string(), live)]);
        let rebuilt = rebuild_run_memory_index(&runs, &store, now_ms)?;
        assert_eq!(
            rebuilt.index.by_session.get("session-a").map(Vec::len),
            Some(1)
        );
        assert!(
            rebuilt
                .pruned_orphan_files
                .iter()
                .any(|path| path == &legacy_path)
        );
        assert!(
            !rebuilt
                .pruned_orphan_files
                .iter()
                .any(|path| path == &safe_path)
        );

        for path in &rebuilt.pruned_orphan_files {
            store.delete_run_memory_file(path)?;
        }
        assert!(store.has_run_memory("run-live"));
        assert!(safe_path.exists());
        assert!(!legacy_path.exists());
        Ok(())
    }

    #[test]
    fn rebuild_run_memory_index_reports_store_scan_errors() -> Result<()> {
        let root = tempfile::tempdir()?;
        let run_memories_dir = root.path().join("run-memories");
        fs::create_dir_all(&run_memories_dir)?;
        fs::write(run_memories_dir.join("__safe"), "not a directory")?;
        let store = FileRunMemoryStore::new(root.path());
        let runs = BTreeMap::new();

        let error = rebuild_run_memory_index(&runs, &store, RUN_MEMORY_RETENTION_MS + 20_000)
            .expect_err("broken store scan should be reported");
        assert!(
            error.to_string().contains("run-memories/__safe"),
            "unexpected error: {error:?}"
        );
        Ok(())
    }

    #[test]
    fn remember_run_memory_indexes_visible_scope_keys() {
        let mut index = RunMemoryIndex::default();
        let record = RunMemoryRecord {
            session_id: "session-a".to_string(),
            scope_keys: vec![
                "session:session-a".to_string(),
                "project:proj-1".to_string(),
                "workspace:workspace".to_string(),
            ],
            semantic_capture: RunMemorySemanticCaptureState::Completed,
            memory: RecoveredMemoryEntry {
                run_id: "run-scope".to_string(),
                recorded_at_ms: 42,
                status: "completed".to_string(),
                request_preview: None,
                outcome_preview: None,
                artifact_ids: Vec::new(),
                failure_markers: Vec::new(),
                summary: "scoped".to_string(),
            },
        };

        let _ = remember_run_memory(&mut index, &record, 100);

        assert!(
            index
                .by_scope
                .get("project:proj-1")
                .is_some_and(|entries| entries.iter().any(|entry| entry.run_id == "run-scope"))
        );
        assert!(
            index
                .by_scope
                .get("workspace:workspace")
                .is_some_and(|entries| entries.iter().any(|entry| entry.run_id == "run-scope"))
        );
    }
}
