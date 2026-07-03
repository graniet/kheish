//! Configurable daemon hook execution and persistence.

use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use kheish_core::{
    AgentEngine, AllowAllPermissions, HookDispatcher, LoopPolicy, ModelDriver, ModelRequest,
    ModelRequestKind, ToolCatalog,
};
use kheish_runtime::{
    MetricsSnapshot, RuntimeObserver, SystemPromptBuilder, ToolChoice, ToolRuntime,
    current_cancellation_token, current_execution_scope, external_action_trace,
    failed_external_action_outcome, redact_text, scope_execution,
};
use kheish_session::{append_json_line_sync, atomic_write, write_json_pretty_atomically};
use kheish_types::{
    ConversationKey, HOOK_CONTRACT_VERSION, HookDecision, HookDispatchOutcome, HookExecutorConfig,
    HookFailureMode, HookFailurePolicy, HookInvocation, HookModelConfig, HookPermissionBehavior,
    HookSettings, InputEnvelope, ModelGenerationConfig, PromptProjection, ProviderInputItem,
    ProviderPrompt, ReplyHandle, Role, SessionControlState, SourceRef, StructuredFieldSchema,
    StructuredValueKind, ToolSurfaceFilter,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, timeout};

use crate::problems::DaemonProblem;
use crate::state_files::read_json_or_quarantine;
use crate::{DaemonModelControl, HookDeadLetterView, HookStatusView, now_ms};

const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_HTTP_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_PROMPT_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_AGENT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_HOOK_MAX_OUTPUT_TOKENS: u32 = 1_024;
const HOOK_SETTINGS_FILENAME: &str = "hooks.json";
const MAX_COMMAND_HOOK_OUTPUT_BYTES: usize = 128 * 1024;
const MAX_HTTP_HOOK_BODY_BYTES: usize = 128 * 1024;
const DEFAULT_COMMAND_HOOK_CONCURRENCY: usize = 64;
const DEFAULT_HTTP_HOOK_CONCURRENCY: usize = 64;
const DEFAULT_PROMPT_HOOK_CONCURRENCY: usize = 16;
const DEFAULT_AGENT_HOOK_CONCURRENCY: usize = 8;
const DEFAULT_CALLBACK_HOOK_CONCURRENCY: usize = 128;
const HOOK_DEAD_LETTER_DIR: &str = "hook-dlq";
const HOOK_RESOLVED_DEAD_LETTER_FILENAME: &str = "resolved-dead-letter.jsonl";
const MAX_HOOK_RETRIES: u8 = 3;
const MAX_HOOK_DEAD_LETTER_API_RECORDS: usize = 1_000;
const MAX_HOOK_DEAD_LETTER_RECORDS: usize = 5_000;
const MAX_HOOK_DEAD_LETTER_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_CONFIGURED_HOOK_TIMEOUT_MS: u64 = 30_000;
const HOOK_SETTINGS_SCHEMA_VERSION: u32 = 1;
const HOOK_ATTEMPT_TIMEOUT_GRACE_MS: u64 = 250;

type HookCallbackFuture = Pin<Box<dyn Future<Output = Result<HookDispatchOutcome>> + Send>>;
type HookCallbackFn = Arc<dyn Fn(HookInvocation) -> HookCallbackFuture + Send + Sync>;

#[derive(Clone, Debug, Serialize)]
struct HookSettingsEnvelope {
    schema_version: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    hooks: BTreeMap<kheish_types::HookEventName, Vec<kheish_types::HookDefinition>>,
}

impl From<&HookSettings> for HookSettingsEnvelope {
    fn from(settings: &HookSettings) -> Self {
        Self {
            schema_version: HOOK_SETTINGS_SCHEMA_VERSION,
            hooks: settings.hooks.clone(),
        }
    }
}

fn parse_persisted_hook_settings(value: Value) -> Result<HookSettings> {
    let Some(object) = value.as_object() else {
        bail!("hook settings must be a JSON object");
    };
    if let Some(raw_version) = object.get("schema_version") {
        let Some(version) = raw_version.as_u64() else {
            bail!("hook settings schema_version must be an unsigned integer");
        };
        if version != HOOK_SETTINGS_SCHEMA_VERSION as u64 {
            bail!(
                "unsupported hook settings schema_version {version}; expected {HOOK_SETTINGS_SCHEMA_VERSION}"
            );
        }
        let hooks = object.get("hooks").cloned().unwrap_or_else(|| json!({}));
        return Ok(HookSettings {
            hooks: serde_json::from_value(hooks)
                .context("failed to parse versioned hook settings hooks map")?,
        });
    }
    serde_json::from_value(value).context("failed to parse legacy hook settings")
}

/// Stores daemon hook settings on disk.
#[derive(Clone, Debug)]
pub struct FileHookSettingsStore {
    path: PathBuf,
}

impl FileHookSettingsStore {
    /// Creates one hook settings store rooted in the daemon state directory.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            path: root.as_ref().join(HOOK_SETTINGS_FILENAME),
        }
    }

    /// Loads hook settings from disk, returning empty settings when no file exists yet.
    pub fn load(&self) -> Result<HookSettings> {
        let Some(value) = read_json_or_quarantine::<Value>(&self.path, "hook settings")? else {
            return Ok(HookSettings::default());
        };
        match parse_persisted_hook_settings(value) {
            Ok(settings) => Ok(settings),
            Err(error) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = ?error,
                    "persisted hook settings are incompatible; starting with hooks disabled"
                );
                Ok(HookSettings::default())
            }
        }
    }

    /// Saves hook settings atomically.
    pub fn save(&self, settings: &HookSettings) -> Result<()> {
        write_json_pretty_atomically(&self.path, &HookSettingsEnvelope::from(settings))
    }
}

/// Stores failed hook executions for operator inspection and incident resolution.
#[derive(Clone, Debug)]
struct FileHookDeadLetterStore {
    root: PathBuf,
}

impl FileHookDeadLetterStore {
    fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().join(HOOK_DEAD_LETTER_DIR),
        }
    }

    fn append(&self, record: &HookDeadLetterRecord) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let digest = kheish_codec::digest_serialize(record)?;
        let short = digest.get(..16).unwrap_or(digest.as_str());
        let hook_name = safe_hook_file_component(&record.hook_name);
        let path = self
            .root
            .join(format!("{}-{hook_name}-{short}.json", record.at_ms));
        write_json_pretty_atomically(&path, record)?;
        if let Err(error) = self.prune() {
            tracing::warn!(error = ?error, "failed to prune hook dead-letter records");
        }
        Ok(())
    }

    fn append_resolution(&self, dead_letter_id: &str, reason: &str) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        append_json_line_sync(
            &self.resolved_dead_letter_path(),
            &HookDeadLetterResolutionRecord {
                dead_letter_id: dead_letter_id.to_string(),
                resolved_at_ms: now_ms(),
                reason: normalize_hook_dead_letter_resolution_reason(reason),
            },
        )?;
        if let Err(error) = self.prune() {
            tracing::warn!(error = ?error, "failed to prune hook dead-letter records after resolution");
        }
        Ok(())
    }

    fn load_recent(&self, max_records: usize) -> Result<Vec<HookDeadLetterRecord>> {
        let mut paths = self.sorted_record_paths_desc()?;
        paths.truncate(max_records);
        let mut records = Vec::with_capacity(paths.len());
        for path in paths {
            match std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))
                .and_then(|content| {
                    serde_json::from_str::<HookDeadLetterRecord>(&content)
                        .with_context(|| format!("failed to parse {}", path.display()))
                }) {
                Ok(record) => records.push(record),
                Err(error) => {
                    tracing::warn!(error = ?error, path = %path.display(), "skipping unreadable hook dead-letter record");
                }
            }
        }
        Ok(records)
    }

    fn load_resolutions(&self) -> Result<Vec<HookDeadLetterResolutionRecord>> {
        let path = self.resolved_dead_letter_path();
        if path
            .metadata()
            .map(|metadata| metadata.len() > MAX_HOOK_DEAD_LETTER_BYTES)
            .unwrap_or(false)
        {
            bail!(
                "hook dead-letter resolution ledger exceeded {} bytes",
                MAX_HOOK_DEAD_LETTER_BYTES
            );
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut records = Vec::new();
        for (index, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<HookDeadLetterResolutionRecord>(line) {
                Ok(mut record) => {
                    record.reason = normalize_hook_dead_letter_resolution_reason(&record.reason);
                    records.push(record);
                }
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        line = index + 1,
                        error = ?error,
                        "skipping corrupt hook dead-letter resolution ledger line"
                    );
                }
            }
        }
        Ok(records)
    }

    #[cfg(test)]
    fn load_all(&self) -> Result<Vec<HookDeadLetterRecord>> {
        self.load_recent(usize::MAX)
    }

    fn summary(&self) -> HookDeadLetterSummary {
        let paths = match self.sorted_record_paths_desc() {
            Ok(paths) => paths,
            Err(error) => {
                return HookDeadLetterSummary {
                    read_error: Some(bounded_hook_dead_letter_error(&error.to_string())),
                    ..HookDeadLetterSummary::default()
                };
            }
        };
        let resolutions = match self.load_resolutions() {
            Ok(resolutions) => resolutions,
            Err(error) => {
                return HookDeadLetterSummary {
                    count: paths.len(),
                    read_error: Some(bounded_hook_dead_letter_error(&error.to_string())),
                    ..HookDeadLetterSummary::default()
                };
            }
        };
        let resolved = resolved_hook_dead_letter_ids(&resolutions);
        let mut summary = HookDeadLetterSummary {
            count: paths.len(),
            ..HookDeadLetterSummary::default()
        };
        if paths.is_empty() {
            return summary;
        }
        for path in paths {
            if summary.last_at_ms.is_none()
                && let Some(timestamp) = dead_letter_path_timestamp(&path)
            {
                summary.last_at_ms = Some(timestamp);
            }
            match std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))
                .and_then(|content| {
                    serde_json::from_str::<HookDeadLetterRecord>(&content)
                        .with_context(|| format!("failed to parse {}", path.display()))
                }) {
                Ok(record) => {
                    if summary.last_hook.is_none() {
                        summary.last_at_ms = Some(record.at_ms);
                        summary.last_hook = Some(record.hook_name.clone());
                    }
                    if !resolved.contains(&record.id) {
                        summary.unresolved_count = summary.unresolved_count.saturating_add(1);
                        if summary.last_unresolved_hook.is_none() {
                            summary.last_unresolved_at_ms = Some(record.at_ms);
                            summary.last_unresolved_hook = Some(record.hook_name);
                        }
                    }
                }
                Err(error) => {
                    summary.read_error = Some(bounded_hook_dead_letter_error(&error.to_string()));
                }
            }
        }
        summary
    }

    fn sorted_record_paths_desc(&self) -> Result<Vec<PathBuf>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut paths = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let path = entry.path();
                    if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
                        paths.push(path);
                    }
                }
                Err(error) => {
                    tracing::warn!(error = ?error, "skipping unreadable hook dead-letter directory entry");
                }
            }
        }
        paths.sort_by(|left, right| {
            dead_letter_path_timestamp(right)
                .cmp(&dead_letter_path_timestamp(left))
                .then_with(|| right.file_name().cmp(&left.file_name()))
        });
        Ok(paths)
    }

    fn prune(&self) -> Result<()> {
        self.prune_with_limits(MAX_HOOK_DEAD_LETTER_RECORDS, MAX_HOOK_DEAD_LETTER_BYTES)
    }

    fn prune_with_limits(&self, max_records: usize, max_bytes: u64) -> Result<()> {
        let paths = self.sorted_record_paths_desc()?;
        let mut kept_bytes = 0u64;
        let mut kept_paths = Vec::new();
        for (index, path) in paths.into_iter().enumerate() {
            let len = path.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            let should_prune = index >= max_records || kept_bytes.saturating_add(len) > max_bytes;
            if should_prune {
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        tracing::warn!(error = ?error, path = %path.display(), "failed to remove old hook dead-letter record");
                    }
                }
            } else {
                kept_bytes = kept_bytes.saturating_add(len);
                kept_paths.push(path);
            }
        }
        self.compact_resolutions_for_record_paths(&kept_paths)?;
        Ok(())
    }

    fn compact_resolutions_for_record_paths(&self, record_paths: &[PathBuf]) -> Result<()> {
        let resolutions = self.load_resolutions()?;
        if resolutions.is_empty() {
            return Ok(());
        }
        let mut live_ids = BTreeSet::new();
        for path in record_paths {
            match std::fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))
                .and_then(|content| {
                    serde_json::from_str::<HookDeadLetterRecord>(&content)
                        .with_context(|| format!("failed to parse {}", path.display()))
                }) {
                Ok(record) => {
                    live_ids.insert(record.id);
                }
                Err(error) => {
                    tracing::warn!(error = ?error, path = %path.display(), "skipping unreadable hook dead-letter record during resolution compaction");
                }
            }
        }
        let mut compacted = BTreeMap::new();
        for resolution in resolutions {
            if live_ids.contains(&resolution.dead_letter_id) {
                compacted.insert(resolution.dead_letter_id.clone(), resolution);
            }
        }
        self.write_resolutions_atomically(&compacted.into_values().collect::<Vec<_>>())
    }

    fn write_resolutions_atomically(
        &self,
        records: &[HookDeadLetterResolutionRecord],
    ) -> Result<()> {
        let path = self.resolved_dead_letter_path();
        if records.is_empty() {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            return Ok(());
        }
        std::fs::create_dir_all(&self.root)?;
        let mut body = String::new();
        for record in records.iter().take(MAX_HOOK_DEAD_LETTER_RECORDS) {
            body.push_str(&serde_json::to_string(record)?);
            body.push('\n');
        }
        atomic_write(&path, body.as_bytes())
    }

    fn display_path(&self) -> String {
        self.root.display().to_string()
    }

    fn resolved_dead_letter_path(&self) -> PathBuf {
        self.root.join(HOOK_RESOLVED_DEAD_LETTER_FILENAME)
    }
}

#[derive(Default)]
struct HookDeadLetterSummary {
    count: usize,
    unresolved_count: usize,
    last_at_ms: Option<u64>,
    last_hook: Option<String>,
    last_unresolved_at_ms: Option<u64>,
    last_unresolved_hook: Option<String>,
    read_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct HookDeadLetterRecord {
    id: String,
    at_ms: u64,
    hook_name: String,
    event: kheish_types::HookEventName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
    target: String,
    attempt_count: u8,
    #[serde(default)]
    failure_mode: HookFailureMode,
    #[serde(default = "hook_contract_version_default")]
    contract_version: u32,
    #[serde(default)]
    invocation_digest: String,
    #[serde(default)]
    definition_digest: String,
    error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolution_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct HookDeadLetterResolutionRecord {
    dead_letter_id: String,
    resolved_at_ms: u64,
    reason: String,
}

impl From<HookDeadLetterRecord> for HookDeadLetterView {
    fn from(record: HookDeadLetterRecord) -> Self {
        Self {
            id: record.id,
            at_ms: record.at_ms,
            hook_name: record.hook_name,
            event: record.event,
            subject: record.subject,
            session_id: record.session_id,
            run_id: record.run_id,
            target: record.target,
            attempt_count: record.attempt_count,
            failure_mode: record.failure_mode,
            contract_version: record.contract_version,
            invocation_digest: record.invocation_digest,
            definition_digest: record.definition_digest,
            error: bounded_hook_dead_letter_error(&record.error),
            resolved_at_ms: record.resolved_at_ms,
            resolution_reason: record
                .resolution_reason
                .as_deref()
                .map(normalize_hook_dead_letter_resolution_reason),
        }
    }
}

fn counter_value(metrics: &MetricsSnapshot, name: &str) -> u64 {
    metrics.counters.get(name).copied().unwrap_or_default()
}

fn hook_contract_version_default() -> u32 {
    HOOK_CONTRACT_VERSION
}

/// One in-process callback registry used by callback-style hooks.
#[derive(Clone, Default)]
pub struct HookCallbackRegistry {
    callbacks: Arc<RwLock<BTreeMap<String, HookCallbackFn>>>,
}

impl HookCallbackRegistry {
    /// Registers one asynchronous callback by name.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn register<F, Fut>(&self, name: impl Into<String>, callback: F)
    where
        F: Fn(HookInvocation) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<HookDispatchOutcome>> + Send + 'static,
    {
        let callback = Arc::new(move |invocation: HookInvocation| {
            Box::pin(callback(invocation)) as HookCallbackFuture
        }) as HookCallbackFn;
        self.callbacks.write().insert(name.into(), callback);
    }

    async fn call(&self, name: &str, invocation: HookInvocation) -> Result<HookDispatchOutcome> {
        let callback = self
            .callbacks
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown hook callback {name}"))?;
        callback(invocation).await
    }
}

#[async_trait]
trait HookHttpResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>>;
}

#[derive(Clone, Debug, Default)]
struct SystemHookHttpResolver;

#[async_trait]
impl HookHttpResolver for SystemHookHttpResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        Ok(tokio::net::lookup_host((host, port))
            .await?
            .collect::<Vec<_>>())
    }
}

/// One daemon-scoped hook dispatcher with configurable executors.
#[derive(Clone)]
pub struct DaemonHookDispatcher {
    settings: Arc<RwLock<HookSettings>>,
    callbacks: HookCallbackRegistry,
    store: FileHookSettingsStore,
    dead_letters: FileHookDeadLetterStore,
    dead_letter_write_lock: Arc<Mutex<()>>,
    model: Arc<dyn ModelDriver>,
    model_control: Option<Arc<dyn DaemonModelControl>>,
    default_provider: Option<String>,
    system_prompt: Arc<SystemPromptBuilder>,
    tools: Arc<ToolRuntime>,
    observer: Arc<dyn RuntimeObserver>,
    workspace_root: PathBuf,
    http_resolver: Arc<dyn HookHttpResolver>,
    command_permits: Arc<Semaphore>,
    http_permits: Arc<Semaphore>,
    prompt_permits: Arc<Semaphore>,
    agent_permits: Arc<Semaphore>,
    callback_permits: Arc<Semaphore>,
}

impl DaemonHookDispatcher {
    /// Creates a new daemon hook dispatcher and loads persisted settings.
    pub fn new(
        state_root: impl AsRef<Path>,
        model: Arc<dyn ModelDriver>,
        model_control: Option<Arc<dyn DaemonModelControl>>,
        default_provider: Option<String>,
        system_prompt: Arc<SystemPromptBuilder>,
        tools: Arc<ToolRuntime>,
        observer: Arc<dyn RuntimeObserver>,
        workspace_root: impl Into<PathBuf>,
    ) -> Result<Self> {
        let state_root = state_root.as_ref();
        let store = FileHookSettingsStore::new(state_root);
        let loaded_settings = store.load()?;
        let settings = match validate_hook_settings(&loaded_settings) {
            Ok(()) => loaded_settings,
            Err(error) => {
                tracing::warn!(
                    error = ?error,
                    "persisted hook settings are invalid for this daemon build; starting with hooks disabled"
                );
                HookSettings::default()
            }
        };
        Ok(Self {
            settings: Arc::new(RwLock::new(settings)),
            callbacks: HookCallbackRegistry::default(),
            store,
            dead_letters: FileHookDeadLetterStore::new(state_root),
            dead_letter_write_lock: Arc::new(Mutex::new(())),
            model,
            model_control,
            default_provider,
            system_prompt,
            tools,
            observer,
            workspace_root: workspace_root.into(),
            http_resolver: Arc::new(SystemHookHttpResolver),
            command_permits: Arc::new(Semaphore::new(DEFAULT_COMMAND_HOOK_CONCURRENCY)),
            http_permits: Arc::new(Semaphore::new(DEFAULT_HTTP_HOOK_CONCURRENCY)),
            prompt_permits: Arc::new(Semaphore::new(DEFAULT_PROMPT_HOOK_CONCURRENCY)),
            agent_permits: Arc::new(Semaphore::new(DEFAULT_AGENT_HOOK_CONCURRENCY)),
            callback_permits: Arc::new(Semaphore::new(DEFAULT_CALLBACK_HOOK_CONCURRENCY)),
        })
    }

    #[cfg(test)]
    fn set_http_resolver_for_test(&mut self, resolver: Arc<dyn HookHttpResolver>) {
        self.http_resolver = resolver;
    }

    /// Returns the current persisted hook settings.
    pub fn settings(&self) -> HookSettings {
        self.settings.read().clone()
    }

    /// Replaces and persists the daemon hook settings.
    pub fn set_settings(&self, settings: HookSettings) -> Result<HookSettings> {
        validate_hook_settings(&settings)?;
        self.store.save(&settings)?;
        *self.settings.write() = settings.clone();
        Ok(settings)
    }

    /// Returns a cheap operator status snapshot for configured hooks and dead letters.
    pub(crate) fn status_snapshot(&self, metrics: &MetricsSnapshot) -> HookStatusView {
        let configured_count = self.settings().hooks.values().map(Vec::len).sum::<usize>();
        let mut view = HookStatusView {
            configured_count,
            dead_letter_store_path: Some(self.dead_letters.display_path()),
            execution_count: counter_value(metrics, "hook.executions"),
            failure_count: counter_value(metrics, "hook.failures"),
            retry_count: counter_value(metrics, "hook.retries"),
            dead_letter_persist_failure_count: counter_value(
                metrics,
                "hook.dead_letter_persist_failures",
            ),
            ..HookStatusView::default()
        };
        let summary = self.dead_letters.summary();
        view.dead_lettered_count = summary.count;
        view.unresolved_dead_lettered_count = summary.unresolved_count;
        view.last_dead_letter_at_ms = summary.last_at_ms;
        view.last_dead_letter_hook = summary.last_hook;
        view.last_unresolved_dead_letter_at_ms = summary.last_unresolved_at_ms;
        view.last_unresolved_dead_letter_hook = summary.last_unresolved_hook;
        if let Some(error) = summary.read_error {
            view.dead_letter_read_error = Some(error);
        }
        view
    }

    /// Returns redacted hook dead-letter records for operator inspection.
    pub(crate) fn dead_letter_views(&self) -> Result<Vec<HookDeadLetterView>> {
        let mut records = self
            .dead_letters
            .load_recent(MAX_HOOK_DEAD_LETTER_API_RECORDS)?;
        apply_hook_dead_letter_resolutions(&mut records, &self.dead_letters.load_resolutions()?);
        records.sort_by_key(|record| (record.at_ms, record.id.clone()));
        Ok(records.into_iter().map(HookDeadLetterView::from).collect())
    }

    /// Marks one hook dead-letter record as operator-resolved without deleting audit evidence.
    pub(crate) fn resolve_dead_letter(
        &self,
        dead_letter_id: &str,
        reason: &str,
    ) -> Result<Option<HookDeadLetterView>> {
        let _guard = self.dead_letter_write_lock.lock();
        let mut records = self.dead_letters.load_recent(usize::MAX)?;
        let Some(source_index) = records
            .iter()
            .position(|record| record.id == dead_letter_id)
        else {
            return Ok(None);
        };
        let mut resolutions = self.dead_letters.load_resolutions()?;
        if !resolutions
            .iter()
            .any(|record| record.dead_letter_id == dead_letter_id)
        {
            self.dead_letters
                .append_resolution(dead_letter_id, reason)?;
            resolutions = self.dead_letters.load_resolutions()?;
        }
        apply_hook_dead_letter_resolutions(&mut records, &resolutions);
        Ok(Some(HookDeadLetterView::from(records.remove(source_index))))
    }

    /// Executes one daemon-owned structured prompt and returns the parsed JSON payload.
    pub async fn run_structured_prompt_json(
        &self,
        session_id: Option<&str>,
        run_id: Option<&str>,
        prompt: &str,
        system_prompt: Option<&str>,
        model: Option<&HookModelConfig>,
        timeout_ms: Option<u64>,
        schema: StructuredFieldSchema,
    ) -> Result<Value> {
        let _permit = self
            .prompt_permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("hook execution semaphore closed"))?;
        let generation = self.resolved_structured_prompt_generation(model, schema)?;
        let invocation = HookInvocation {
            event: kheish_types::HookEventName::Notification,
            subject: Some("daemon_structured_prompt".to_string()),
            session_id: session_id.map(ToOwned::to_owned),
            agent_id: None,
            run_id: run_id.map(ToOwned::to_owned),
            payload: Value::Null,
        };
        let request = self.build_prompt_model_request(
            &invocation,
            prompt.to_string(),
            system_prompt,
            generation,
            Vec::new(),
        );
        let turn = timeout_or_run(
            timeout_ms.unwrap_or(DEFAULT_PROMPT_TIMEOUT_MS),
            self.model.next_turn(request),
        )
        .await?;
        parse_structured_json_text(turn.assistant_message.content.trim())
    }

    /// Registers one in-process callback executor.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn register_callback<F, Fut>(&self, name: impl Into<String>, callback: F)
    where
        F: Fn(HookInvocation) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<HookDispatchOutcome>> + Send + 'static,
    {
        self.callbacks.register(name, callback);
    }

    async fn execute_definition(
        &self,
        definition: HookDefinitionView,
        invocation: HookInvocation,
    ) -> Result<HookDispatchOutcome> {
        let _permit = self.acquire_permit(&definition.executor).await?;
        match definition.executor {
            HookExecutorConfig::Command {
                command,
                shell,
                timeout_ms,
            } => {
                self.run_command_hook(
                    &definition.name,
                    &command,
                    shell.as_deref(),
                    timeout_ms,
                    &invocation,
                )
                .await
            }
            HookExecutorConfig::Http { url, timeout_ms } => {
                self.run_http_hook(&definition.name, &url, timeout_ms, &invocation)
                    .await
            }
            HookExecutorConfig::Prompt {
                template,
                system_prompt,
                model,
                timeout_ms,
            } => {
                self.run_prompt_hook(
                    &template,
                    system_prompt.as_deref(),
                    model.as_ref(),
                    timeout_ms,
                    &invocation,
                )
                .await
            }
            HookExecutorConfig::Agent {
                template,
                system_prompt,
                model,
                tool_surface,
                max_turns,
                timeout_ms,
            } => {
                self.run_agent_hook(
                    &template,
                    system_prompt.as_deref(),
                    model.as_ref(),
                    tool_surface,
                    max_turns,
                    timeout_ms,
                    &invocation,
                )
                .await
            }
            HookExecutorConfig::Callback { name, timeout_ms } => {
                let future = self.callbacks.call(&name, invocation);
                timeout_or_run(timeout_ms.unwrap_or(DEFAULT_PROMPT_TIMEOUT_MS), future).await
            }
        }
    }

    async fn execute_definition_with_policy(
        &self,
        definition: HookDefinitionView,
        invocation: HookInvocation,
    ) -> Result<HookDispatchOutcome> {
        let max_retries = definition.failure_policy.max_retries.min(MAX_HOOK_RETRIES);
        let total_timeout_ms = definition.timeout_ms();
        let deadline = Instant::now() + Duration::from_millis(total_timeout_ms);
        let mut attempt_count = 0u8;
        let mut last_failure: Option<String> = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let message = last_failure.unwrap_or_else(|| {
                    format!("hook execution timed out after {total_timeout_ms}ms")
                });
                return self.failed_definition_with_policy(
                    &definition,
                    &invocation,
                    attempt_count.max(1),
                    &message,
                );
            }
            attempt_count = attempt_count.saturating_add(1);
            match timeout_or_run(
                duration_millis_ceil(remaining).saturating_add(HOOK_ATTEMPT_TIMEOUT_GRACE_MS),
                self.execute_definition(definition.clone(), invocation.clone()),
            )
            .await
            {
                Ok(outcome) => return Ok(sanitize_hook_outcome(outcome)),
                Err(error) => {
                    let message = error.to_string();
                    last_failure = Some(message.clone());
                    if hook_failure_allows_retry(&error) && attempt_count <= max_retries {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            return self.failed_definition_with_policy(
                                &definition,
                                &invocation,
                                attempt_count,
                                &message,
                            );
                        }
                        self.observer.increment_counter("hook.retries", 1);
                        let delay = hook_failure_retry_after(&error)
                            .unwrap_or_else(|| hook_retry_backoff(attempt_count))
                            .min(remaining);
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return self.failed_definition_with_policy(
                        &definition,
                        &invocation,
                        attempt_count,
                        &message,
                    );
                }
            }
        }
    }

    fn record_dead_letter(
        &self,
        definition: &HookDefinitionView,
        invocation: &HookInvocation,
        attempt_count: u8,
        error: &str,
    ) -> Result<()> {
        let at_ms = now_ms();
        let invocation_digest = kheish_codec::digest_serialize(invocation)?;
        let definition_digest = kheish_codec::digest_serialize(&json!({
            "name": &definition.name,
            "failure_policy": &definition.failure_policy,
            "executor": &definition.executor,
        }))?;
        let redacted_error = bounded_hook_dead_letter_error(error);
        let digest = kheish_codec::digest_serialize(&json!({
            "at_ms": at_ms,
            "hook_name": &definition.name,
            "event": &invocation.event,
            "subject": &invocation.subject,
            "session_id": &invocation.session_id,
            "run_id": &invocation.run_id,
            "error": &redacted_error,
        }))?;
        let invocation_short = invocation_digest
            .get(..8)
            .unwrap_or(invocation_digest.as_str());
        let definition_short = definition_digest
            .get(..8)
            .unwrap_or(definition_digest.as_str());
        let id = format!(
            "hook-dlq-{at_ms}-{invocation_short}-{definition_short}-{}",
            digest.get(..16).unwrap_or(digest.as_str())
        );
        let record = HookDeadLetterRecord {
            id,
            at_ms,
            hook_name: definition.name.clone(),
            event: invocation.event.clone(),
            subject: invocation.subject.clone(),
            session_id: invocation.session_id.clone(),
            run_id: invocation.run_id.clone(),
            target: hook_definition_safe_target(definition),
            attempt_count,
            failure_mode: definition.failure_policy.mode.clone(),
            contract_version: HOOK_CONTRACT_VERSION,
            invocation_digest,
            definition_digest,
            error: redacted_error,
            resolved_at_ms: None,
            resolution_reason: None,
        };
        let _guard = self.dead_letter_write_lock.lock();
        self.dead_letters.append(&record)
    }

    fn failed_definition_with_policy(
        &self,
        definition: &HookDefinitionView,
        invocation: &HookInvocation,
        attempt_count: u8,
        message: &str,
    ) -> Result<HookDispatchOutcome> {
        self.observer.increment_counter("hook.failures", 1);
        if let Err(dlq_error) =
            self.record_dead_letter(definition, invocation, attempt_count, message)
        {
            self.observer
                .increment_counter("hook.dead_letter_persist_failures", 1);
            tracing::warn!(
                hook_name = %definition.name,
                event = ?invocation.event,
                error = ?dlq_error,
                "failed to persist hook dead-letter record"
            );
        }
        if matches!(definition.failure_policy.mode, HookFailureMode::Closed) {
            let safe_message = bounded_hook_dead_letter_error(message);
            return Ok(HookDispatchOutcome {
                continue_execution: false,
                decision: Some(HookDecision::Block),
                stop_reason: Some(format!(
                    "hook `{}` failed after {} attempt(s): {}",
                    definition.name, attempt_count, safe_message
                )),
                ..HookDispatchOutcome::default()
            });
        }
        Err(anyhow!(message.to_string()))
    }

    async fn acquire_permit(&self, executor: &HookExecutorConfig) -> Result<OwnedSemaphorePermit> {
        let semaphore = match executor {
            HookExecutorConfig::Command { .. } => self.command_permits.clone(),
            HookExecutorConfig::Http { .. } => self.http_permits.clone(),
            HookExecutorConfig::Prompt { .. } => self.prompt_permits.clone(),
            HookExecutorConfig::Agent { .. } => self.agent_permits.clone(),
            HookExecutorConfig::Callback { .. } => self.callback_permits.clone(),
        };
        semaphore
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("hook execution semaphore closed"))
    }

    async fn run_command_hook(
        &self,
        hook_name: &str,
        command: &str,
        shell: Option<&str>,
        timeout_ms: Option<u64>,
        invocation: &HookInvocation,
    ) -> Result<HookDispatchOutcome> {
        let shell = shell.unwrap_or("/bin/bash");
        let audit = HookAuditSpan::start(
            self.observer.clone(),
            format!(
                "hook_command:{:?}:{}",
                invocation.event,
                summarize_hook_target(hook_name)
            ),
            invocation,
        )?;
        let executed = async {
            let mut process = Command::new(shell);
            process
                .arg("-lc")
                .arg(command)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            configure_command_hook_process_group(&mut process);
            let mut child = process
                .spawn()
                .with_context(|| format!("failed to spawn hook command via {shell}"))?;
            let process_group_id = child.id();
            let payload = serde_json::to_vec(&hook_invocation_contract(invocation)?)?;
            if let Some(mut stdin) = child.stdin.take() {
                if let Err(error) = stdin.write_all(&payload).await {
                    if !matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ) {
                        terminate_command_hook_process_group(process_group_id, &mut child).await;
                        return Err(error.into());
                    }
                }
            }
            let stdout = match child.stdout.take() {
                Some(stdout) => stdout,
                None => {
                    terminate_command_hook_process_group(process_group_id, &mut child).await;
                    bail!("hook command stdout was not captured");
                }
            };
            let stderr = match child.stderr.take() {
                Some(stderr) => stderr,
                None => {
                    terminate_command_hook_process_group(process_group_id, &mut child).await;
                    bail!("hook command stderr was not captured");
                }
            };
            let wait_timeout = timeout_ms.unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS);
            let mut stdout_task =
                tokio::spawn(read_stream_capped(stdout, MAX_COMMAND_HOOK_OUTPUT_BYTES));
            let mut stderr_task =
                tokio::spawn(read_stream_capped(stderr, MAX_COMMAND_HOOK_OUTPUT_BYTES));
            let result = timeout(Duration::from_millis(wait_timeout), async {
                let status = child.wait().await?;
                let stdout = (&mut stdout_task).await??;
                let stderr = (&mut stderr_task).await??;
                Ok::<_, anyhow::Error>((status, stdout, stderr))
            })
            .await;
            let (status, stdout, stderr) = match result {
                Ok(Ok(value)) => value,
                Ok(Err(error)) => {
                    terminate_command_hook_process_group(process_group_id, &mut child).await;
                    stdout_task.abort();
                    stderr_task.abort();
                    return Err(error);
                }
                Err(_) => {
                    terminate_command_hook_process_group(process_group_id, &mut child).await;
                    stdout_task.abort();
                    stderr_task.abort();
                    bail!("hook execution timed out after {wait_timeout}ms");
                }
            };
            let stdout = String::from_utf8_lossy(&stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
            let outcome = if status.code() == Some(2) && stdout.is_empty() {
                Ok(HookDispatchOutcome {
                    continue_execution: false,
                    decision: Some(HookDecision::Block),
                    stop_reason: Some(format!(
                        "hook command blocked execution{}",
                        (!stderr.is_empty())
                            .then_some(format!(": {}", bounded_hook_stop_reason(&stderr)))
                            .unwrap_or_default()
                    )),
                    ..HookDispatchOutcome::default()
                })
            } else if !status.success() {
                Err(anyhow!(
                    "hook command exited with status {}{}",
                    status,
                    (!stderr.is_empty())
                        .then_some(format!(": {stderr}"))
                        .unwrap_or_default()
                ))
            } else {
                parse_hook_outcome_text(&stdout)
                    .map_err(|error| terminal_hook_failure(error.to_string()))
            };
            let response_digest = kheish_codec::digest_serialize(&json!({
                "status": status.code(),
                "stdout": stdout,
                "stderr": stderr,
            }))?;
            Ok::<_, anyhow::Error>((outcome, response_digest))
        }
        .await;
        match executed {
            Ok((outcome, response_digest)) => {
                audit.record_response_digest(
                    Some(response_digest),
                    match &outcome {
                        Ok(value) if !value.continue_execution => "blocked".to_string(),
                        Ok(_) => "ok".to_string(),
                        Err(error) => failed_external_action_outcome(error.to_string()),
                    },
                )?;
                outcome
            }
            Err(error) => {
                audit.record_failure(&error)?;
                Err(error)
            }
        }
    }

    async fn run_http_hook(
        &self,
        hook_name: &str,
        url: &str,
        timeout_ms: Option<u64>,
        invocation: &HookInvocation,
    ) -> Result<HookDispatchOutcome> {
        let audit = HookAuditSpan::start(
            self.observer.clone(),
            format!(
                "hook_http:{:?}:{}:{}",
                invocation.event,
                summarize_hook_target(hook_name),
                sanitized_hook_http_target_from_str(url)
            ),
            invocation,
        )?;
        let url = match reqwest::Url::parse(url) {
            Ok(url) => url,
            Err(error) => {
                audit.record_failure_outcome("failed:invalid_url")?;
                return Err(terminal_hook_failure(error.to_string()));
            }
        };
        if !matches!(url.scheme(), "http" | "https") {
            audit.record_failure_outcome("failed:unsupported_scheme")?;
            return Err(terminal_hook_failure(
                "hook HTTP executors only support http and https",
            ));
        }
        let resolved_addrs = match validate_hook_http_url(&url, self.http_resolver.as_ref()).await {
            Ok(addrs) => addrs,
            Err(error) => {
                audit.record_failure_outcome("failed:rejected_target")?;
                return Err(terminal_hook_failure(error.to_string()));
            }
        };
        let executed = execute_http_hook_request(
            hook_name,
            url,
            &resolved_addrs,
            timeout_ms.unwrap_or(DEFAULT_HTTP_TIMEOUT_MS),
            invocation,
        )
        .await;
        match executed {
            Ok((outcome, response_digest)) => {
                audit.record_response_digest(
                    Some(response_digest),
                    match &outcome {
                        Ok(value) if !value.continue_execution => "blocked".to_string(),
                        Ok(_) => "ok".to_string(),
                        Err(error) => failed_external_action_outcome(error.to_string()),
                    },
                )?;
                outcome
            }
            Err(error) => {
                audit.record_failure_outcome(&safe_hook_failure_outcome(&error))?;
                Err(error)
            }
        }
    }

    async fn run_prompt_hook(
        &self,
        template: &str,
        system_prompt: Option<&str>,
        model: Option<&HookModelConfig>,
        timeout_ms: Option<u64>,
        invocation: &HookInvocation,
    ) -> Result<HookDispatchOutcome> {
        let rendered = render_hook_template(template, invocation);
        let generation = self.resolved_hook_generation(model, false)?;
        let request = self.build_prompt_model_request(
            invocation,
            rendered,
            system_prompt,
            generation,
            Vec::new(),
        );
        let turn = timeout_or_run(
            timeout_ms.unwrap_or(DEFAULT_PROMPT_TIMEOUT_MS),
            self.model.next_turn(request),
        )
        .await?;
        parse_hook_outcome_text(turn.assistant_message.content.trim())
            .map_err(|error| terminal_hook_failure(error.to_string()))
    }

    async fn run_agent_hook(
        &self,
        template: &str,
        system_prompt: Option<&str>,
        model: Option<&HookModelConfig>,
        tool_surface: Option<ToolSurfaceFilter>,
        max_turns: Option<usize>,
        timeout_ms: Option<u64>,
        invocation: &HookInvocation,
    ) -> Result<HookDispatchOutcome> {
        let rendered = render_hook_template(template, invocation);
        let generation = self.resolved_hook_generation(model, true)?;
        let tool_surface = tool_surface
            .map(|surface| surface.normalized())
            .unwrap_or_else(ToolSurfaceFilter::deny_all);
        let tools =
            Arc::new(self.tools.as_ref().clone_without_hook_dispatcher()).scoped(tool_surface);
        let mut sections = self.system_prompt.build_sections(
            &tools.definitions(),
            None,
            None,
            &[],
            &SessionControlState::default(),
            None,
        );
        if let Some(system_prompt) = system_prompt {
            sections.push(kheish_types::SystemPromptSection {
                name: "hook_agent".to_string(),
                content: system_prompt.to_string(),
            });
        }
        sections.push(kheish_types::SystemPromptSection {
            name: "hook_response_contract".to_string(),
            content: "Return only a JSON object matching the requested schema when you finish."
                .to_string(),
        });

        let conversation = ConversationKey {
            session_id: invocation
                .session_id
                .clone()
                .unwrap_or_else(|| format!("hook-agent-{}", now_ms())),
            thread_id: invocation.run_id.clone(),
        };
        let mut engine = AgentEngine::new(
            conversation.clone(),
            LoopPolicy {
                max_turns: max_turns.unwrap_or(4),
                ..LoopPolicy::default()
            },
        );
        engine.set_system_sections(sections);
        let reply = ReplyHandle {
            plugin: "hook".to_string(),
            address: self.workspace_root.display().to_string(),
        };
        let input = InputEnvelope {
            source: SourceRef {
                plugin: "hook".to_string(),
                kind: "agent".to_string(),
            },
            conversation,
            actor: kheish_types::ActorRef {
                id: "hook-agent".to_string(),
                display_name: None,
            },
            payload: kheish_types::InputPayload::Text { content: rendered },
            attachments: Vec::new(),
            metadata: Value::Null,
            reply_targets: vec![reply.clone()],
            reply: Some(reply),
        };
        let outcome = timeout_or_run(
            timeout_ms.unwrap_or(DEFAULT_AGENT_TIMEOUT_MS),
            engine.run_input_with_generation(
                input,
                generation,
                self.model.as_ref(),
                &tools,
                &AllowAllPermissions,
            ),
        )
        .await?;
        if outcome.status != kheish_types::RunStatus::Completed {
            bail!("hook agent did not reach a completed state");
        }
        let final_message = engine
            .replay_from_journal()
            .messages
            .into_iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .ok_or_else(|| anyhow!("hook agent produced no assistant message"))?;
        parse_hook_outcome_text(final_message.content.trim())
            .map_err(|error| terminal_hook_failure(error.to_string()))
    }

    fn resolved_generation_base(
        &self,
        model: Option<&HookModelConfig>,
        allow_tools: bool,
    ) -> Result<(ModelGenerationConfig, Option<String>)> {
        let provider = model.and_then(|config| config.provider.as_deref());
        let mut generation = model
            .and_then(|config| config.generation.clone())
            .unwrap_or_default();
        let mut resolved_provider = self.default_provider.clone();
        if let Some(control) = self.model_control.as_ref() {
            let resolved = control.resolve_route(provider, generation.model.as_deref())?;
            generation.model = Some(resolved.model.clone());
            resolved_provider = Some(resolved.provider.clone());
            if let Some(fallback_model) = generation.fallback_model.clone() {
                let fallback =
                    control.resolve_route(Some(&resolved.route_id), Some(&fallback_model))?;
                generation.fallback_model = Some(fallback.model);
            }
        } else if provider.is_some() {
            bail!("hook provider overrides require multi-provider daemon routing");
        }
        generation
            .max_output_tokens
            .get_or_insert(DEFAULT_HOOK_MAX_OUTPUT_TOKENS);
        if !allow_tools {
            generation.tool_choice = ToolChoice::None;
            generation.allow_parallel_tool_calls = false;
        }
        Ok((generation, resolved_provider))
    }

    fn resolved_hook_generation(
        &self,
        model: Option<&HookModelConfig>,
        allow_tools: bool,
    ) -> Result<ModelGenerationConfig> {
        let (mut generation, resolved_provider) =
            self.resolved_generation_base(model, allow_tools)?;
        generation.response_format = if resolved_provider.as_deref() == Some("openai") {
            kheish_types::ResponseFormat::Text
        } else {
            kheish_types::ResponseFormat::StructuredJson {
                schema: hook_outcome_schema(),
            }
        };
        Ok(generation)
    }

    fn resolved_structured_prompt_generation(
        &self,
        model: Option<&HookModelConfig>,
        schema: StructuredFieldSchema,
    ) -> Result<ModelGenerationConfig> {
        let (mut generation, resolved_provider) = self.resolved_generation_base(model, false)?;
        generation.response_format = if resolved_provider.as_deref() == Some("openai") {
            kheish_types::ResponseFormat::Text
        } else {
            kheish_types::ResponseFormat::StructuredJson { schema }
        };
        Ok(generation)
    }

    fn build_prompt_model_request(
        &self,
        invocation: &HookInvocation,
        rendered: String,
        system_prompt: Option<&str>,
        generation: ModelGenerationConfig,
        available_tools: Vec<kheish_types::ToolDefinition>,
    ) -> ModelRequest {
        let system_sections = system_prompt
            .into_iter()
            .map(|content| kheish_types::SystemPromptSection {
                name: "hook".to_string(),
                content: content.to_string(),
            })
            .collect::<Vec<_>>();
        let message = kheish_types::MessageRecord::new("hook-user-1", Role::User, rendered.clone());
        ModelRequest {
            kind: ModelRequestKind::MainLoop,
            conversation: ConversationKey {
                session_id: invocation
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "hook".to_string()),
                thread_id: invocation.run_id.clone(),
            },
            turn: 1,
            prompt: PromptProjection {
                summary: None,
                system_sections: system_sections.clone(),
                messages: vec![message.clone()],
                open_tool_calls: Vec::new(),
                restoration: None,
            },
            provider_prompt: ProviderPrompt {
                instructions: system_sections
                    .iter()
                    .map(|section| section.content.clone())
                    .collect(),
                force_synthetic_user_prefix: false,
                items: vec![ProviderInputItem::Message {
                    id: message.id,
                    role: message.role,
                    content: message.content,
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: None,
                    provider_context: None,
                }],
            },
            available_tools,
            generation,
        }
    }
}

#[async_trait]
impl HookDispatcher for DaemonHookDispatcher {
    async fn dispatch(&self, invocation: HookInvocation) -> Result<HookDispatchOutcome> {
        let hooks = self
            .settings
            .read()
            .event_hooks(invocation.event.clone())
            .iter()
            .filter(|hook| matcher_matches(hook.matcher.as_deref(), invocation.subject.as_deref()))
            .map(HookDefinitionView::from)
            .collect::<Vec<_>>();
        if hooks.is_empty() {
            return Ok(HookDispatchOutcome::default());
        }

        if hook_event_short_circuits(&invocation.event) {
            let execution_scope = current_execution_scope();
            let cancellation = current_cancellation_token();
            let mut aggregated = HookDispatchOutcome::default();
            let mut watch_paths = BTreeSet::new();
            for definition in hooks {
                let name = definition.name.clone();
                let dispatcher = self.clone();
                let task_definition = definition.clone();
                let task_invocation = invocation.clone();
                let task_scope = execution_scope.clone();
                let task_cancellation = cancellation.clone();
                let handle = tokio::spawn(async move {
                    let run = async {
                        dispatcher
                            .execute_definition_with_policy(task_definition, task_invocation)
                            .await
                    };
                    if let Some(scope) = task_scope {
                        scope_execution(
                            scope,
                            task_cancellation
                                .unwrap_or_else(tokio_util::sync::CancellationToken::new),
                            run,
                        )
                        .await
                    } else {
                        run.await
                    }
                });
                let result = match handle.await {
                    Ok(result) => result,
                    Err(error) => self.failed_definition_with_policy(
                        &definition,
                        &invocation,
                        1,
                        if error.is_panic() {
                            "hook task panicked"
                        } else {
                            "hook task was cancelled"
                        },
                    ),
                };
                let Ok(outcome) = result else {
                    continue;
                };
                self.observer.increment_counter("hook.executions", 1);
                merge_hook_outcome(
                    &mut aggregated,
                    &mut watch_paths,
                    name,
                    sanitize_hook_outcome(outcome),
                );
                if hook_outcome_stops_event(&invocation.event, &aggregated) {
                    break;
                }
            }
            aggregated.watch_paths = watch_paths.into_iter().collect();
            return Ok(aggregated);
        }

        let tasks = hooks
            .into_iter()
            .enumerate()
            .map(|(index, definition)| {
                let dispatcher = self.clone();
                let invocation = invocation.clone();
                let task_definition = definition.clone();
                let execution_scope = current_execution_scope();
                let cancellation = current_cancellation_token();
                let handle = tokio::spawn(async move {
                    let run = async {
                        dispatcher
                            .execute_definition_with_policy(task_definition.clone(), invocation)
                            .await
                    };
                    if let Some(scope) = execution_scope {
                        scope_execution(
                            scope,
                            cancellation.unwrap_or_else(tokio_util::sync::CancellationToken::new),
                            run,
                        )
                        .await
                    } else {
                        run.await
                    }
                });
                (index, definition, handle)
            })
            .collect::<Vec<_>>();

        let mut completed = Vec::with_capacity(tasks.len());
        for (index, definition, task) in tasks {
            let result = match task.await {
                Ok(result) => result,
                Err(error) => self.failed_definition_with_policy(
                    &definition,
                    &invocation,
                    1,
                    if error.is_panic() {
                        "hook task panicked"
                    } else {
                        "hook task was cancelled"
                    },
                ),
            };
            let name = definition.name;
            completed.push((index, name, result));
        }
        completed.sort_by_key(|(index, _, _)| *index);

        let mut aggregated = HookDispatchOutcome::default();
        let mut watch_paths = BTreeSet::new();
        for (_, name, result) in completed {
            let Ok(outcome) = result else {
                continue;
            };
            self.observer.increment_counter("hook.executions", 1);
            merge_hook_outcome(
                &mut aggregated,
                &mut watch_paths,
                name,
                sanitize_hook_outcome(outcome),
            );
        }
        aggregated.watch_paths = watch_paths.into_iter().collect();
        Ok(aggregated)
    }
}

#[derive(Clone)]
struct HookDefinitionView {
    name: String,
    failure_policy: HookFailurePolicy,
    executor: HookExecutorConfig,
}

impl From<&kheish_types::HookDefinition> for HookDefinitionView {
    fn from(value: &kheish_types::HookDefinition) -> Self {
        Self {
            name: value.name.clone(),
            failure_policy: value.failure_policy.clone(),
            executor: value.executor.clone(),
        }
    }
}

impl HookDefinitionView {
    fn timeout_ms(&self) -> u64 {
        let timeout_ms = match &self.executor {
            HookExecutorConfig::Command { timeout_ms, .. } => {
                timeout_ms.unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS)
            }
            HookExecutorConfig::Http { timeout_ms, .. } => {
                timeout_ms.unwrap_or(DEFAULT_HTTP_TIMEOUT_MS)
            }
            HookExecutorConfig::Prompt { timeout_ms, .. } => {
                timeout_ms.unwrap_or(DEFAULT_PROMPT_TIMEOUT_MS)
            }
            HookExecutorConfig::Agent { timeout_ms, .. } => {
                timeout_ms.unwrap_or(DEFAULT_AGENT_TIMEOUT_MS)
            }
            HookExecutorConfig::Callback { timeout_ms, .. } => {
                timeout_ms.unwrap_or(DEFAULT_PROMPT_TIMEOUT_MS)
            }
        };
        timeout_ms.min(MAX_CONFIGURED_HOOK_TIMEOUT_MS)
    }
}

/// Validates hook settings before accepting them into runtime configuration.
pub fn validate_hook_settings(settings: &HookSettings) -> Result<()> {
    for (event, hooks) in &settings.hooks {
        for (index, hook) in hooks.iter().enumerate() {
            let hook_id = format!("{event:?}[{index}]:{}", hook.name);
            if hook.name.trim().is_empty() {
                invalid_hook_settings(format!("{hook_id}: hook name must not be empty"))?;
            }
            if hook.failure_policy.max_retries > MAX_HOOK_RETRIES {
                invalid_hook_settings(format!(
                    "{hook_id}: hook max_retries must not exceed {MAX_HOOK_RETRIES}"
                ))?;
            }
            validate_hook_timeout(hook_id.as_str(), hook_timeout_ms(&hook.executor))?;
            match &hook.executor {
                HookExecutorConfig::Command { command, shell, .. } => {
                    if command.trim().is_empty() {
                        invalid_hook_settings(format!("{hook_id}: command hook command is empty"))?;
                    }
                    if shell
                        .as_deref()
                        .is_some_and(|shell| shell.trim().is_empty())
                    {
                        invalid_hook_settings(format!("{hook_id}: command hook shell is empty"))?;
                    }
                }
                HookExecutorConfig::Http { url, .. } => {
                    if let Err(error) = validate_hook_http_target_static(url) {
                        invalid_hook_settings(format!("{hook_id}: {error}"))?;
                    }
                }
                HookExecutorConfig::Prompt { template, .. } => {
                    if template.trim().is_empty() {
                        invalid_hook_settings(format!("{hook_id}: prompt hook template is empty"))?;
                    }
                }
                HookExecutorConfig::Agent {
                    template,
                    tool_surface,
                    max_turns,
                    ..
                } => {
                    if template.trim().is_empty() {
                        invalid_hook_settings(format!("{hook_id}: agent hook template is empty"))?;
                    }
                    if let Some(tool_surface) = tool_surface {
                        let normalized = tool_surface.normalized();
                        if normalized.allowlist.is_empty() && !normalized.is_deny_all() {
                            invalid_hook_settings(format!(
                                "{hook_id}: agent hook tool_surface must include an allowlist"
                            ))?;
                        }
                    }
                    if max_turns == &Some(0) {
                        invalid_hook_settings(format!("{hook_id}: agent hook max_turns is zero"))?;
                    }
                }
                HookExecutorConfig::Callback { name, .. } => {
                    if name.trim().is_empty() {
                        invalid_hook_settings(format!("{hook_id}: callback hook name is empty"))?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn hook_timeout_ms(executor: &HookExecutorConfig) -> Option<u64> {
    match executor {
        HookExecutorConfig::Command { timeout_ms, .. }
        | HookExecutorConfig::Http { timeout_ms, .. }
        | HookExecutorConfig::Prompt { timeout_ms, .. }
        | HookExecutorConfig::Agent { timeout_ms, .. }
        | HookExecutorConfig::Callback { timeout_ms, .. } => *timeout_ms,
    }
}

fn validate_hook_timeout(hook_id: &str, timeout_ms: Option<u64>) -> Result<()> {
    let Some(timeout_ms) = timeout_ms else {
        return Ok(());
    };
    if timeout_ms == 0 {
        invalid_hook_settings(format!(
            "{hook_id}: hook timeout_ms must be greater than zero"
        ))?;
    }
    if timeout_ms > MAX_CONFIGURED_HOOK_TIMEOUT_MS {
        invalid_hook_settings(format!(
            "{hook_id}: hook timeout_ms must not exceed {MAX_CONFIGURED_HOOK_TIMEOUT_MS}"
        ))?;
    }
    Ok(())
}

fn validate_hook_http_target_static(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        invalid_hook_settings("HTTP hook URL must use http or https")?;
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        invalid_hook_settings("HTTP hook URL must not include userinfo")?;
    }
    let host = parsed.host_str().ok_or_else(|| {
        DaemonProblem::bad_request(
            "runtime",
            "invalid_hook_settings",
            "HTTP hook URL requires a concrete host",
        )
    })?;
    if is_localhost_hook_name(host) {
        invalid_hook_settings("HTTP hook URL must not target localhost")?;
    }
    if hook_host_has_zone_identifier(host) {
        invalid_hook_settings("HTTP hook URL must not include IPv6 zone identifiers")?;
    }
    if let Some(address) = parse_hook_ip_host(host)
        && is_blocked_hook_ip(address)
    {
        invalid_hook_settings("HTTP hook URL must not target private or local addresses")?;
    }
    Ok(())
}

fn is_localhost_hook_name(host: &str) -> bool {
    let lower_host = host.trim_end_matches('.').to_ascii_lowercase();
    lower_host == "localhost" || lower_host.ends_with(".localhost")
}

fn invalid_hook_settings<T>(detail: impl Into<String>) -> Result<T> {
    Err(DaemonProblem::bad_request("runtime", "invalid_hook_settings", detail).into())
}

fn hook_retry_backoff(attempt_count: u8) -> Duration {
    let shift = u32::from(attempt_count.saturating_sub(1).min(2));
    let base = 50u64.saturating_mul(1u64 << shift);
    let jitter = now_ms() % 25;
    Duration::from_millis(base + jitter)
}

#[derive(Debug)]
struct ClassifiedHookFailure {
    message: String,
    retryable: bool,
    retry_after: Option<Duration>,
}

impl fmt::Display for ClassifiedHookFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClassifiedHookFailure {}

fn terminal_hook_failure(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ClassifiedHookFailure {
        message: message.into(),
        retryable: false,
        retry_after: None,
    })
}

fn retryable_hook_failure(
    message: impl Into<String>,
    retry_after: Option<Duration>,
) -> anyhow::Error {
    anyhow::Error::new(ClassifiedHookFailure {
        message: message.into(),
        retryable: true,
        retry_after,
    })
}

fn hook_failure_allows_retry(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ClassifiedHookFailure>())
        .map(|failure| failure.retryable)
        .unwrap_or(true)
}

fn hook_failure_retry_after(error: &anyhow::Error) -> Option<Duration> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ClassifiedHookFailure>())
        .and_then(|failure| failure.retry_after)
}

fn is_retryable_hook_http_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn hook_retry_after_header(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds).min(Duration::from_secs(30)));
    }
    let retry_at = httpdate::parse_http_date(value).ok()?;
    let now = std::time::SystemTime::now();
    retry_at
        .duration_since(now)
        .ok()
        .map(|duration| duration.min(Duration::from_secs(30)))
}

fn duration_millis_ceil(duration: Duration) -> u64 {
    let millis = duration.as_millis();
    let rounded = if duration.subsec_nanos() % 1_000_000 == 0 {
        millis
    } else {
        millis.saturating_add(1)
    };
    u64::try_from(rounded).unwrap_or(u64::MAX).max(1)
}

fn aggregate_decision(
    current: Option<HookDecision>,
    next: Option<HookDecision>,
) -> Option<HookDecision> {
    match (current, next) {
        (Some(HookDecision::Block), _) | (_, Some(HookDecision::Block)) => {
            Some(HookDecision::Block)
        }
        (Some(HookDecision::Approve), _) => Some(HookDecision::Approve),
        (None, value) => value,
    }
}

fn aggregate_permission(
    current: Option<HookPermissionBehavior>,
    next: Option<HookPermissionBehavior>,
) -> Option<HookPermissionBehavior> {
    let rank = |value: &HookPermissionBehavior| match value {
        HookPermissionBehavior::Deny => 3,
        HookPermissionBehavior::Ask => 2,
        HookPermissionBehavior::Allow => 1,
    };
    match (current, next) {
        (Some(current), Some(next)) => {
            if rank(&next) >= rank(&current) {
                Some(next)
            } else {
                Some(current)
            }
        }
        (Some(current), None) => Some(current),
        (None, value) => value,
    }
}

fn hook_event_short_circuits(event: &kheish_types::HookEventName) -> bool {
    matches!(
        event,
        kheish_types::HookEventName::PreToolUse
            | kheish_types::HookEventName::PostToolUse
            | kheish_types::HookEventName::PostToolUseFailure
            | kheish_types::HookEventName::PreCompact
            | kheish_types::HookEventName::PermissionRequest
            | kheish_types::HookEventName::Setup
            | kheish_types::HookEventName::SessionStart
            | kheish_types::HookEventName::Stop
            | kheish_types::HookEventName::UserPromptSubmit
            | kheish_types::HookEventName::SubagentStart
            | kheish_types::HookEventName::ConfigChange
            | kheish_types::HookEventName::WorktreeCreate
            | kheish_types::HookEventName::InstructionsLoaded
    )
}

fn hook_outcome_stops_event(
    event: &kheish_types::HookEventName,
    outcome: &HookDispatchOutcome,
) -> bool {
    matches!(outcome.decision, Some(HookDecision::Block))
        || !outcome.continue_execution
        || (matches!(event, kheish_types::HookEventName::PermissionRequest)
            && matches!(outcome.permission, Some(HookPermissionBehavior::Deny)))
}

fn merge_hook_outcome(
    aggregated: &mut HookDispatchOutcome,
    watch_paths: &mut BTreeSet<String>,
    name: String,
    outcome: HookDispatchOutcome,
) {
    aggregated.matched_hooks.push(name);
    aggregated.continue_execution &= outcome.continue_execution;
    if aggregated.stop_reason.is_none() {
        aggregated.stop_reason = outcome.stop_reason;
    }
    aggregated.decision = aggregate_decision(aggregated.decision.take(), outcome.decision);
    aggregated.permission = aggregate_permission(aggregated.permission.take(), outcome.permission);
    if let Some(updated_input) = outcome.updated_input {
        aggregated.updated_input = Some(updated_input);
    }
    if let Some(updated_output) = outcome.updated_output {
        aggregated.updated_output = Some(updated_output);
    }
    if !outcome.updated_permissions.is_empty() {
        aggregated
            .updated_permissions
            .extend(outcome.updated_permissions);
    }
    if aggregated.initial_user_message.is_none() {
        aggregated.initial_user_message = outcome.initial_user_message;
    }
    aggregated.retry |= outcome.retry;
    aggregated
        .additional_contexts
        .extend(outcome.additional_contexts);
    for path in outcome.watch_paths {
        watch_paths.insert(path);
    }
}

fn matcher_matches(matcher: Option<&str>, subject: Option<&str>) -> bool {
    let Some(matcher) = matcher.filter(|matcher| !matcher.is_empty()) else {
        return true;
    };
    wildcard_matches(matcher, subject.unwrap_or_default())
}

fn wildcard_matches(pattern: &str, candidate: &str) -> bool {
    let pattern = pattern.as_bytes();
    let candidate = candidate.as_bytes();
    let (mut p, mut c, mut star, mut last_match) = (0usize, 0usize, None, 0usize);
    while c < candidate.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == candidate[c]) {
            p += 1;
            c += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            last_match = c;
        } else if let Some(star_index) = star {
            p = star_index + 1;
            last_match += 1;
            c = last_match;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn render_hook_template(template: &str, invocation: &HookInvocation) -> String {
    let invocation_json = hook_invocation_contract(invocation)
        .and_then(|value| serde_json::to_string_pretty(&value).map_err(Into::into))
        .unwrap_or_else(|_| "{}".to_string());
    template
        .replace("{{event}}", &format!("{:?}", invocation.event))
        .replace(
            "{{subject}}",
            invocation.subject.as_deref().unwrap_or_default(),
        )
        .replace(
            "{{session_id}}",
            invocation.session_id.as_deref().unwrap_or_default(),
        )
        .replace(
            "{{agent_id}}",
            invocation.agent_id.as_deref().unwrap_or_default(),
        )
        .replace(
            "{{run_id}}",
            invocation.run_id.as_deref().unwrap_or_default(),
        )
        .replace("{{payload}}", &invocation.payload.to_string())
        .replace("{{invocation_json}}", &invocation_json)
}

fn hook_invocation_contract(invocation: &HookInvocation) -> Result<Value> {
    let mut value = serde_json::to_value(invocation)?;
    if let Value::Object(fields) = &mut value {
        fields.insert("contract_version".to_string(), json!(HOOK_CONTRACT_VERSION));
        fields.insert(
            "invocation_key".to_string(),
            json!(hook_invocation_key(invocation)?),
        );
    }
    Ok(value)
}

fn hook_invocation_key(invocation: &HookInvocation) -> Result<String> {
    let digest = kheish_codec::digest_serialize(invocation)?;
    Ok(format!(
        "hook-invocation-{}",
        digest.get(..32).unwrap_or(digest.as_str())
    ))
}

fn hook_execution_idempotency_key(hook_name: &str, invocation: &HookInvocation) -> Result<String> {
    let digest = kheish_codec::digest_serialize(&json!({
        "contract_version": HOOK_CONTRACT_VERSION,
        "hook_name": hook_name,
        "invocation": invocation,
    }))?;
    Ok(format!(
        "hook-execution-{}",
        digest.get(..32).unwrap_or(digest.as_str())
    ))
}

fn parse_hook_outcome_text(text: &str) -> Result<HookDispatchOutcome> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(HookDispatchOutcome::default());
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
        return parse_hook_outcome_value(parsed);
    }
    if let Some(candidate) = extract_json_payload(trimmed) {
        return parse_hook_outcome_value(serde_json::from_str(candidate)?);
    }
    parse_hook_outcome_value(serde_json::from_str(trimmed)?)
}

fn parse_hook_outcome_value(value: Value) -> Result<HookDispatchOutcome> {
    if let Some(raw_version) = value.get("contract_version") {
        let Some(version) = raw_version.as_u64() else {
            bail!("malformed hook outcome contract_version");
        };
        if version > HOOK_CONTRACT_VERSION as u64 {
            bail!("unsupported hook outcome contract_version {version}");
        }
    }
    Ok(sanitize_hook_outcome(serde_json::from_value(value)?))
}

fn sanitize_hook_outcome(mut outcome: HookDispatchOutcome) -> HookDispatchOutcome {
    outcome.stop_reason = outcome.stop_reason.as_deref().map(bounded_hook_stop_reason);
    outcome
}

fn parse_structured_json_text(text: &str) -> Result<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("structured prompt returned an empty response");
    }
    if let Ok(parsed) = serde_json::from_str(trimmed) {
        return Ok(parsed);
    }
    if let Some(candidate) = extract_json_payload(trimmed) {
        return Ok(serde_json::from_str(candidate)?);
    }
    Ok(serde_json::from_str(trimmed)?)
}

fn extract_json_payload(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let fenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|value| value.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    extract_balanced_json(fenced, '{', '}').or_else(|| extract_balanced_json(fenced, '[', ']'))
}

fn extract_balanced_json(text: &str, open: char, close: char) -> Option<&str> {
    let start = text.find(open)?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in text.char_indices().skip_while(|(index, _)| *index < start) {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            value if value == open => depth = depth.saturating_add(1),
            value if value == close => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&text[start..=index]);
                }
            }
            _ => {}
        }
    }
    None
}

fn configure_command_hook_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }
}

async fn terminate_command_hook_process_group(
    process_group_id: Option<u32>,
    child: &mut tokio::process::Child,
) {
    #[cfg(unix)]
    {
        if let Some(pid) = process_group_id {
            let _ = signal_command_hook_process_group(pid, libc::SIGTERM);
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = signal_command_hook_process_group(pid, libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = timeout(Duration::from_millis(500), child.wait()).await;
}

#[cfg(unix)]
fn signal_command_hook_process_group(pid: u32, signal: i32) -> std::io::Result<()> {
    let result = unsafe { libc::kill(-(pid as i32), signal) };
    if result == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}

async fn read_stream_capped<R>(mut reader: R, limit: usize) -> Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = Vec::with_capacity(limit.min(8 * 1024));
    let mut chunk = [0u8; 8 * 1024];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        if buffer.len() + read > limit {
            bail!("hook output exceeded {limit} bytes");
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    Ok(buffer)
}

async fn read_response_body_capped(response: reqwest::Response, limit: usize) -> Result<String> {
    if let Some(length) = response.content_length() {
        if length > limit as u64 {
            bail!("hook HTTP response exceeded {limit} bytes");
        }
    }
    let mut body = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > limit {
            bail!("hook HTTP response exceeded {limit} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8(body)?)
}

async fn execute_http_hook_request(
    hook_name: &str,
    url: reqwest::Url,
    resolved_addrs: &[SocketAddr],
    request_timeout_ms: u64,
    invocation: &HookInvocation,
) -> Result<(Result<HookDispatchOutcome>, String)> {
    let client = pinned_hook_http_client(&url, resolved_addrs, request_timeout_ms)?;
    let idempotency_key = hook_execution_idempotency_key(hook_name, invocation)?;
    let response = timeout_reqwest_result(
        request_timeout_ms,
        client
            .post(url)
            .header("Idempotency-Key", idempotency_key)
            .json(&hook_invocation_contract(invocation)?)
            .send(),
    )
    .await?;
    let status = response.status();
    if status.is_redirection() {
        return Err(terminal_hook_failure("hook HTTP redirects are not allowed"));
    }
    if is_retryable_hook_http_status(status) {
        return Err(retryable_hook_failure(
            format!("hook HTTP request failed with status {}", status.as_u16()),
            hook_retry_after_header(response.headers()),
        ));
    }
    if status.is_client_error() {
        return Err(terminal_hook_failure(format!(
            "hook HTTP request failed with status {}",
            status.as_u16()
        )));
    }
    let body = timeout_or_run(request_timeout_ms, async {
        read_response_body_capped(response, MAX_HTTP_HOOK_BODY_BYTES).await
    })
    .await?;
    let parsed = parse_hook_outcome_text(body.trim())
        .map_err(|error| terminal_hook_failure(error.to_string()));
    let response_digest = kheish_codec::digest_serialize(&json!({
        "status": status.as_u16(),
        "body": body,
    }))?;
    Ok((parsed, response_digest))
}

async fn validate_hook_http_url(
    url: &reqwest::Url,
    resolver: &dyn HookHttpResolver,
) -> Result<Vec<SocketAddr>> {
    validate_hook_http_target_static(url.as_str())?;
    let Some(host) = url.host_str() else {
        bail!("hook HTTP executors require a concrete host");
    };
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("hook HTTP executors require an explicit or default port"))?;
    if let Some(address) = parse_hook_ip_host(host) {
        if is_blocked_hook_ip(address) {
            bail!("hook HTTP executors cannot target private or local addresses");
        }
        return Ok(vec![SocketAddr::new(address, port)]);
    }
    let resolved = resolver.resolve(host, port).await?;
    validate_resolved_hook_http_addresses(&resolved)?;
    Ok(resolved)
}

fn validate_resolved_hook_http_addresses(resolved: &[SocketAddr]) -> Result<()> {
    if resolved.is_empty() {
        bail!("hook HTTP executor hostname resolved no addresses");
    }
    for address in resolved {
        if is_blocked_hook_ip(address.ip()) {
            bail!(
                "hook HTTP executors cannot target hostnames resolving to private or local addresses"
            );
        }
    }
    Ok(())
}

fn parse_hook_ip_host(host: &str) -> Option<IpAddr> {
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    host.parse::<IpAddr>().ok()
}

fn hook_host_has_zone_identifier(host: &str) -> bool {
    host.contains('%')
}

fn pinned_hook_http_client(
    url: &reqwest::Url,
    resolved_addrs: &[SocketAddr],
    timeout_ms: u64,
) -> Result<reqwest::Client> {
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("hook HTTP executors require a concrete host"))?;
    let timeout = Duration::from_millis(timeout_ms);
    let connect_timeout = timeout.min(Duration::from_secs(3));
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(timeout)
        .connect_timeout(connect_timeout)
        .resolve_to_addrs(host, resolved_addrs)
        .build()
        .map_err(Into::into)
}

pub fn hook_http_target_blocks_ip(address: IpAddr) -> bool {
    is_blocked_hook_ip(address)
}

fn is_blocked_hook_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => is_blocked_hook_ipv4(ip),
        IpAddr::V6(ip) => is_blocked_hook_ipv6(ip),
    }
}

fn is_blocked_hook_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.octets()[0] == 0
        || is_ipv4_ietf_protocol_assignment(ip)
        || is_ipv4_6to4_relay_anycast(ip)
        || is_ipv4_shared_address(ip)
        || is_ipv4_benchmark_address(ip)
        || is_ipv4_reserved_address(ip)
}

fn is_blocked_hook_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_blocked_hook_ipv4(mapped);
    }
    if let Some(compatible) = ipv6_to_ipv4_compatible(ip) {
        return is_blocked_hook_ipv4(compatible);
    }
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || is_ipv6_site_local(ip)
        || is_ipv6_documentation(ip)
        || is_ipv6_nat64_translation(ip)
        || is_ipv6_6to4(ip)
        || is_ipv6_teredo(ip)
        || is_ipv6_discard_only(ip)
        || is_ipv6_benchmark(ip)
        || is_ipv6_orchid(ip)
}

fn ipv6_to_ipv4_compatible(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = ip.segments();
    if segments[..6] != [0, 0, 0, 0, 0, 0] {
        return None;
    }
    let [a, b] = segments[6].to_be_bytes();
    let [c, d] = segments[7].to_be_bytes();
    let mapped = Ipv4Addr::new(a, b, c, d);
    if mapped == Ipv4Addr::UNSPECIFIED || mapped == Ipv4Addr::new(0, 0, 0, 1) {
        None
    } else {
        Some(mapped)
    }
}

fn is_ipv6_site_local(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfec0
}

fn is_ipv6_documentation(ip: Ipv6Addr) -> bool {
    ip.segments()[0] == 0x2001 && ip.segments()[1] == 0x0db8
}

fn is_ipv6_nat64_translation(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    (segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 0)
        || (segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 0x0001)
}

fn is_ipv6_6to4(ip: Ipv6Addr) -> bool {
    ip.segments()[0] == 0x2002
}

fn is_ipv6_teredo(ip: Ipv6Addr) -> bool {
    ip.segments()[0] == 0x2001 && ip.segments()[1] == 0
}

fn is_ipv6_discard_only(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    segments[0] == 0x0100 && segments[1] == 0 && segments[2] == 0 && segments[3] == 0
}

fn is_ipv6_benchmark(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    segments[0] == 0x2001 && segments[1] == 0x0002 && segments[2] == 0
}

fn is_ipv6_orchid(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    segments[0] == 0x2001 && ((segments[1] & 0xfff0) == 0x0010 || (segments[1] & 0xfff0) == 0x0020)
}

fn is_ipv4_ietf_protocol_assignment(ip: Ipv4Addr) -> bool {
    let [first, second, third, _] = ip.octets();
    first == 192 && second == 0 && third == 0
}

fn is_ipv4_6to4_relay_anycast(ip: Ipv4Addr) -> bool {
    let [first, second, third, _] = ip.octets();
    first == 192 && second == 88 && third == 99
}

fn is_ipv4_shared_address(ip: Ipv4Addr) -> bool {
    let [first, second, _, _] = ip.octets();
    first == 100 && (64..=127).contains(&second)
}

fn is_ipv4_benchmark_address(ip: Ipv4Addr) -> bool {
    let [first, second, _, _] = ip.octets();
    first == 198 && (18..=19).contains(&second)
}

fn is_ipv4_reserved_address(ip: Ipv4Addr) -> bool {
    ip.octets()[0] >= 240
}

struct HookAuditSpan {
    observer: Arc<dyn RuntimeObserver>,
    target: String,
    request_digest: String,
}

impl HookAuditSpan {
    fn start(
        observer: Arc<dyn RuntimeObserver>,
        target: String,
        invocation: &HookInvocation,
    ) -> Result<Self> {
        let request_digest = kheish_codec::digest_serialize(invocation)?;
        observer.record_external_action(external_action_trace(
            "request",
            "hook",
            target.clone(),
            Some(request_digest.clone()),
            None,
            None,
        ))?;
        Ok(Self {
            observer,
            target,
            request_digest,
        })
    }

    fn record_response_digest(
        &self,
        response_digest: Option<String>,
        outcome: String,
    ) -> Result<()> {
        self.observer.record_external_action(external_action_trace(
            "response",
            "hook",
            self.target.clone(),
            Some(self.request_digest.clone()),
            response_digest,
            Some(outcome),
        ))
    }

    fn record_failure(&self, error: &anyhow::Error) -> Result<()> {
        self.record_failure_outcome(&safe_hook_failure_outcome(error))
    }

    fn record_failure_outcome(&self, outcome: &str) -> Result<()> {
        self.record_response_digest(None, outcome.to_string())
    }
}

fn sanitized_hook_http_target_from_str(url: &str) -> String {
    reqwest::Url::parse(url)
        .map(|url| sanitized_hook_http_target(&url))
        .unwrap_or_else(|_| {
            let digest = kheish_codec::digest_text(url);
            format!(
                "invalid_url_sha256:{}",
                digest.get(..16).unwrap_or(digest.as_str())
            )
        })
}

fn sanitized_hook_http_target(url: &reqwest::Url) -> String {
    let Some(host) = url.host_str() else {
        return "unknown-host".to_string();
    };
    let mut target = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        target.push(':');
        target.push_str(&port.to_string());
    }
    target
}

fn safe_hook_failure_outcome(error: &anyhow::Error) -> String {
    if let Some(reqwest_error) = error.downcast_ref::<reqwest::Error>() {
        let class = if reqwest_error.is_timeout() {
            "timeout"
        } else if reqwest_error.is_connect() {
            "connect"
        } else if reqwest_error.is_status() {
            "status"
        } else if reqwest_error.is_decode() {
            "decode"
        } else if reqwest_error.is_request() {
            "request"
        } else {
            "transport"
        };
        return format!("failed:{class}");
    }
    let message = error.to_string();
    if message.contains("timed out") {
        return "failed:timeout".to_string();
    }
    if message.contains("unsupported") {
        return "failed:unsupported".to_string();
    }
    if message.contains("private or local addresses") || message.contains("localhost") {
        return "failed:rejected_target".to_string();
    }
    if message.contains("redirect") {
        return "failed:redirect".to_string();
    }
    if message.contains("status") {
        return "failed:status".to_string();
    }
    if message.contains("decode") {
        return "failed:decode".to_string();
    }
    "failed:internal".to_string()
}

fn bounded_hook_dead_letter_error(error: &str) -> String {
    redact_hook_inline_secret_assignments(&redact_text(error))
        .chars()
        .take(2_048)
        .collect()
}

fn bounded_hook_stop_reason(reason: &str) -> String {
    bounded_hook_dead_letter_error(reason)
}

fn normalize_hook_dead_letter_resolution_reason(reason: &str) -> String {
    let redacted = bounded_hook_dead_letter_error(reason.trim());
    if redacted.is_empty() {
        return "operator resolved".to_string();
    }
    redacted.chars().take(512).collect()
}

fn resolved_hook_dead_letter_ids(
    resolutions: &[HookDeadLetterResolutionRecord],
) -> BTreeSet<String> {
    resolutions
        .iter()
        .map(|record| record.dead_letter_id.clone())
        .collect()
}

fn hook_dead_letter_resolutions_by_id(
    resolutions: &[HookDeadLetterResolutionRecord],
) -> BTreeMap<String, HookDeadLetterResolutionRecord> {
    let mut by_id = BTreeMap::new();
    for record in resolutions {
        by_id.insert(record.dead_letter_id.clone(), record.clone());
    }
    by_id
}

fn apply_hook_dead_letter_resolutions(
    records: &mut [HookDeadLetterRecord],
    resolutions: &[HookDeadLetterResolutionRecord],
) {
    let by_id = hook_dead_letter_resolutions_by_id(resolutions);
    for record in records {
        if let Some(resolution) = by_id.get(&record.id) {
            record.resolved_at_ms = Some(resolution.resolved_at_ms);
            record.resolution_reason = Some(normalize_hook_dead_letter_resolution_reason(
                &resolution.reason,
            ));
        }
    }
}

fn redact_hook_inline_secret_assignments(text: &str) -> String {
    let mut rendered = String::with_capacity(text.len());
    let mut word = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            rendered.push_str(&redact_hook_inline_secret_word(&word));
            word.clear();
            rendered.push(ch);
        } else {
            word.push(ch);
        }
    }
    rendered.push_str(&redact_hook_inline_secret_word(&word));
    rendered
}

fn redact_hook_inline_secret_word(word: &str) -> String {
    let Some(separator_index) = word.find(['=', ':']) else {
        return word.to_string();
    };
    let key = &word[..separator_index];
    let value = &word[separator_index + 1..];
    if value.is_empty() || !is_sensitive_hook_inline_key(key) {
        return word.to_string();
    }
    let separator = &word[separator_index..=separator_index];
    format!("{key}{separator}[REDACTED]")
}

fn is_sensitive_hook_inline_key(key: &str) -> bool {
    let normalized = key
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .to_ascii_lowercase();
    normalized.contains("token")
        || normalized.contains("secret")
        || normalized.contains("api_key")
        || normalized.contains("apikey")
        || normalized.contains("password")
        || normalized.contains("authorization")
        || normalized == "auth"
}

fn dead_letter_path_timestamp(path: &Path) -> Option<u64> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.split_once('-').map(|(timestamp, _)| timestamp))
        .and_then(|timestamp| timestamp.parse::<u64>().ok())
}

/// Returns a hook settings view safe for read-only operator surfaces.
pub fn redacted_hook_settings(settings: &HookSettings) -> HookSettings {
    HookSettings {
        hooks: settings
            .hooks
            .iter()
            .map(|(event, hooks)| {
                (
                    event.clone(),
                    hooks
                        .iter()
                        .map(redacted_hook_definition)
                        .collect::<Vec<_>>(),
                )
            })
            .collect(),
    }
}

fn redacted_hook_definition(hook: &kheish_types::HookDefinition) -> kheish_types::HookDefinition {
    let mut hook = hook.clone();
    hook.executor = redacted_hook_executor(&hook.executor);
    hook
}

fn redacted_hook_executor(executor: &HookExecutorConfig) -> HookExecutorConfig {
    match executor {
        HookExecutorConfig::Command {
            shell, timeout_ms, ..
        } => HookExecutorConfig::Command {
            command: "[redacted hook command]".to_string(),
            shell: shell.clone(),
            timeout_ms: *timeout_ms,
        },
        HookExecutorConfig::Http { url, timeout_ms } => HookExecutorConfig::Http {
            url: sanitized_hook_http_target_from_str(url),
            timeout_ms: *timeout_ms,
        },
        HookExecutorConfig::Prompt {
            model, timeout_ms, ..
        } => HookExecutorConfig::Prompt {
            template: "[redacted hook prompt template]".to_string(),
            system_prompt: None,
            model: model.clone(),
            timeout_ms: *timeout_ms,
        },
        HookExecutorConfig::Agent {
            model,
            tool_surface,
            max_turns,
            timeout_ms,
            ..
        } => HookExecutorConfig::Agent {
            template: "[redacted hook agent template]".to_string(),
            system_prompt: None,
            model: model.clone(),
            tool_surface: tool_surface.clone(),
            max_turns: *max_turns,
            timeout_ms: *timeout_ms,
        },
        HookExecutorConfig::Callback { name, timeout_ms } => HookExecutorConfig::Callback {
            name: name.clone(),
            timeout_ms: *timeout_ms,
        },
    }
}

fn hook_definition_safe_target(definition: &HookDefinitionView) -> String {
    match &definition.executor {
        HookExecutorConfig::Command { .. } => {
            format!("command:{}", summarize_hook_target(&definition.name))
        }
        HookExecutorConfig::Http { url, .. } => {
            format!(
                "http:{}:{}",
                summarize_hook_target(&definition.name),
                sanitized_hook_http_target_from_str(url)
            )
        }
        HookExecutorConfig::Prompt { .. } => {
            format!("prompt:{}", summarize_hook_target(&definition.name))
        }
        HookExecutorConfig::Agent { .. } => {
            format!("agent:{}", summarize_hook_target(&definition.name))
        }
        HookExecutorConfig::Callback { name, .. } => {
            format!(
                "callback:{}:{}",
                summarize_hook_target(&definition.name),
                summarize_hook_target(name)
            )
        }
    }
}

fn safe_hook_file_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "hook".to_string()
    } else {
        trimmed.chars().take(64).collect()
    }
}

fn summarize_hook_target(value: &str) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= 160 {
        return normalized;
    }
    let truncated = normalized.chars().take(160).collect::<String>();
    format!("{truncated}...")
}

fn hook_outcome_schema() -> StructuredFieldSchema {
    let mut schema = StructuredFieldSchema::new(StructuredValueKind::Object);
    schema.optional_fields.insert(
        "contract_version".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::Number),
    );
    schema.optional_fields.insert(
        "decision".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    schema.optional_fields.insert(
        "permission".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    schema.optional_fields.insert(
        "continue_execution".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::Boolean),
    );
    schema.optional_fields.insert(
        "stop_reason".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    schema.optional_fields.insert(
        "updated_input".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::Any),
    );
    schema.optional_fields.insert(
        "updated_output".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::Any),
    );
    let mut updated_permission = StructuredFieldSchema::new(StructuredValueKind::Object);
    updated_permission.fields.insert(
        "scope".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    updated_permission.fields.insert(
        "tool_name_pattern".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    updated_permission.fields.insert(
        "behavior".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    updated_permission.optional_fields.insert(
        "reason".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    let mut updated_permissions = StructuredFieldSchema::new(StructuredValueKind::Array);
    updated_permissions.items = Some(Box::new(updated_permission));
    schema
        .optional_fields
        .insert("updated_permissions".to_string(), updated_permissions);
    schema.optional_fields.insert(
        "initial_user_message".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    schema.optional_fields.insert(
        "retry".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::Boolean),
    );
    let mut string_array = StructuredFieldSchema::new(StructuredValueKind::Array);
    string_array.items = Some(Box::new(StructuredFieldSchema::new(
        StructuredValueKind::String,
    )));
    schema
        .optional_fields
        .insert("additional_contexts".to_string(), string_array.clone());
    schema
        .optional_fields
        .insert("watch_paths".to_string(), string_array);
    schema
}

async fn timeout_or_run<T, F>(timeout_ms: u64, future: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    timeout(Duration::from_millis(timeout_ms), future)
        .await
        .map_err(|_| anyhow!("hook execution timed out after {timeout_ms}ms"))?
}

async fn timeout_reqwest_result<T, F>(timeout_ms: u64, future: F) -> Result<T>
where
    F: Future<Output = std::result::Result<T, reqwest::Error>>,
{
    timeout(Duration::from_millis(timeout_ms), future)
        .await
        .map_err(|_| anyhow!("hook execution timed out after {timeout_ms}ms"))?
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::collections::{BTreeSet, VecDeque};
    use std::net::{IpAddr, SocketAddr};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        DaemonHookDispatcher, FileHookSettingsStore, HookDefinitionView, aggregate_permission,
        matcher_matches, wildcard_matches,
    };
    use crate::now_ms;
    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::{Json, Router, routing::post};
    use kheish_core::{HookDispatcher, ModelDriver, ModelRequest, ModelTurn};
    use kheish_runtime::{
        ExecutionScope, InMemoryObserver, SystemPromptBuilder, SystemPromptEnvironment,
        SystemPromptSettings, ToolRuntime, TraceEventKind, scope_execution,
    };
    use kheish_types::{
        HookDecision, HookDefinition, HookDispatchOutcome, HookExecutorConfig, HookInvocation,
        HookPermissionBehavior, HookPermissionUpdate, HookPermissionUpdateBehavior,
        HookPermissionUpdateScope, HookSettings, MessageRecord, ModelFinishReason,
        StructuredValueKind, ToolCallRecord, ToolSurfaceFilter,
    };
    use serde_json::{Value, json};
    use tempfile::tempdir;
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;

    struct ScriptedModel {
        responses: Mutex<VecDeque<String>>,
    }

    struct StaticHookHttpResolver {
        addrs: Vec<SocketAddr>,
    }

    #[async_trait]
    impl super::HookHttpResolver for StaticHookHttpResolver {
        async fn resolve(&self, _host: &str, _port: u16) -> Result<Vec<SocketAddr>> {
            Ok(self.addrs.clone())
        }
    }

    #[async_trait]
    impl ModelDriver for ScriptedModel {
        async fn next_turn(&self, _request: ModelRequest) -> Result<ModelTurn> {
            let content = self
                .responses
                .lock()
                .pop_front()
                .ok_or_else(|| anyhow!("no scripted response remaining"))?;
            Ok(ModelTurn {
                assistant_message: MessageRecord::new(
                    "hook-assistant".to_string(),
                    kheish_types::Role::Assistant,
                    content,
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            })
        }
    }

    fn test_dispatcher() -> Result<DaemonHookDispatcher> {
        test_dispatcher_with_responses(Vec::new())
    }

    fn test_dispatcher_with_responses(responses: Vec<String>) -> Result<DaemonHookDispatcher> {
        Ok(test_dispatcher_with_observer(responses)?.0)
    }

    fn test_dispatcher_with_observer(
        responses: Vec<String>,
    ) -> Result<(DaemonHookDispatcher, std::sync::Arc<InMemoryObserver>)> {
        let root = tempdir()?.keep();
        let observer = InMemoryObserver::shared();
        let model: std::sync::Arc<dyn ModelDriver> = std::sync::Arc::new(ScriptedModel {
            responses: Mutex::new(VecDeque::from(responses)),
        });
        let system_prompt = std::sync::Arc::new(SystemPromptBuilder::new(
            SystemPromptEnvironment::new(&root, "/bin/bash"),
            SystemPromptSettings::default(),
        ));
        let hook_tools = std::sync::Arc::new(ToolRuntime::new(observer.clone()));
        let dispatcher = DaemonHookDispatcher::new(
            &root,
            model,
            None,
            None,
            system_prompt,
            hook_tools,
            observer.clone(),
            &root,
        )?;
        Ok((dispatcher, observer))
    }

    #[test]
    fn wildcard_match_supports_star_and_question() {
        assert!(wildcard_matches("bash*", "bash"));
        assert!(wildcard_matches("bash*", "bash:rm"));
        assert!(wildcard_matches("file-?.txt", "file-a.txt"));
        assert!(!wildcard_matches("file-?.txt", "file-aa.txt"));
    }

    #[test]
    fn matcher_defaults_to_match_all() {
        assert!(matcher_matches(None, Some("bash")));
        assert!(matcher_matches(Some(""), None));
        assert!(!matcher_matches(Some("write_*"), Some("bash")));
    }

    #[test]
    fn permission_aggregation_prefers_deny_then_ask_then_allow() {
        assert_eq!(
            aggregate_permission(
                Some(HookPermissionBehavior::Allow),
                Some(HookPermissionBehavior::Deny)
            ),
            Some(HookPermissionBehavior::Deny)
        );
        assert_eq!(
            aggregate_permission(
                Some(HookPermissionBehavior::Allow),
                Some(HookPermissionBehavior::Ask)
            ),
            Some(HookPermissionBehavior::Ask)
        );
    }

    #[tokio::test]
    async fn callback_hooks_can_mutate_input() -> Result<()> {
        let dispatcher = test_dispatcher()?;
        dispatcher.register_callback("rewrite", |invocation| async move {
            assert_eq!(invocation.event, kheish_types::HookEventName::PreToolUse);
            Ok(HookDispatchOutcome {
                updated_input: Some(json!({"path": "rewritten.txt"})),
                additional_contexts: vec!["callback-ran".to_string()],
                ..HookDispatchOutcome::default()
            })
        });
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::PreToolUse,
                vec![HookDefinition {
                    name: "rewrite".to_string(),
                    matcher: Some("write_file".to_string()),
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Callback {
                        name: "rewrite".to_string(),
                        timeout_ms: None,
                    },
                }],
            ))
            .collect(),
        })?;

        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::PreToolUse,
                subject: Some("write_file".to_string()),
                session_id: Some("s".to_string()),
                agent_id: None,
                run_id: None,
                payload: json!({
                    "tool_call": ToolCallRecord {
                        id: "call-1".to_string(),
                        name: "write_file".to_string(),
                        input: json!({"path": "original.txt"}),
                        assistant_message_id: None,
                        assistant_provider_response_id: None,
                    }
                }),
            })
            .await?;
        assert_eq!(
            outcome.updated_input,
            Some(json!({"path": "rewritten.txt"}))
        );
        assert_eq!(
            outcome.additional_contexts,
            vec!["callback-ran".to_string()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn command_hook_exit_code_two_blocks() -> Result<()> {
        let (dispatcher, observer) = test_dispatcher_with_observer(Vec::new())?;
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::UserPromptSubmit,
                vec![HookDefinition {
                    name: "block".to_string(),
                    matcher: None,
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Command {
                        command: "exit 2".to_string(),
                        shell: Some("/bin/sh".to_string()),
                        timeout_ms: Some(1_000),
                    },
                }],
            ))
            .collect(),
        })?;
        let outcome = scope_execution(
            ExecutionScope {
                session_id: "hook-session".to_string(),
                run_id: Some("hook-run".to_string()),
                principal_id: Some("agent:hook-agent".to_string()),
                ..ExecutionScope::default()
            },
            CancellationToken::new(),
            dispatcher.dispatch(HookInvocation {
                event: kheish_types::HookEventName::UserPromptSubmit,
                subject: Some("actor".to_string()),
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            }),
        )
        .await?;
        assert_eq!(outcome.decision, Some(HookDecision::Block));
        assert!(!outcome.continue_execution);

        let external_actions = observer
            .traces()
            .into_iter()
            .filter_map(|event| match event.kind {
                TraceEventKind::ExternalAction {
                    phase,
                    kind,
                    target,
                    outcome,
                    ..
                } if kind == "hook" => Some((phase, target, outcome, event.run_id)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(external_actions.len(), 2);
        assert_eq!(external_actions[0].0, "request");
        assert!(external_actions[0].1.starts_with("hook_command:"));
        assert!(!external_actions[0].1.contains("exit 2"));
        assert_eq!(external_actions[0].3.as_deref(), Some("hook-run"));
        assert_eq!(external_actions[1].0, "response");
        assert_eq!(external_actions[1].2.as_deref(), Some("blocked"));
        assert_eq!(external_actions[1].3.as_deref(), Some("hook-run"));
        Ok(())
    }

    #[tokio::test]
    async fn command_hook_spawn_failures_record_audit_response() -> Result<()> {
        let (dispatcher, observer) = test_dispatcher_with_observer(Vec::new())?;
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::UserPromptSubmit,
                vec![HookDefinition {
                    name: "missing-shell".to_string(),
                    matcher: None,
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Command {
                        command: "printf '{}'".to_string(),
                        shell: Some("/definitely/not/kheish-shell".to_string()),
                        timeout_ms: Some(1_000),
                    },
                }],
            ))
            .collect(),
        })?;

        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::UserPromptSubmit,
                subject: Some("actor".to_string()),
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert!(outcome.matched_hooks.is_empty());

        let external_actions = observer
            .traces()
            .into_iter()
            .filter_map(|event| match event.kind {
                TraceEventKind::ExternalAction {
                    phase,
                    kind,
                    target,
                    outcome,
                    ..
                } if kind == "hook" => Some((phase, target, outcome)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(external_actions.len(), 2);
        assert_eq!(external_actions[0].0, "request");
        assert_eq!(external_actions[1].0, "response");
        assert!(
            external_actions[1]
                .2
                .as_deref()
                .is_some_and(|outcome| outcome.starts_with("failed:"))
        );
        assert!(!external_actions[0].1.contains("printf"));
        let dead_letters = dispatcher.dead_letters.load_all()?;
        assert_eq!(dead_letters.len(), 1);
        assert_eq!(dead_letters[0].hook_name, "missing-shell");
        assert_eq!(dead_letters[0].attempt_count, 1);
        Ok(())
    }

    #[tokio::test]
    async fn hook_retries_failed_executor_before_dead_letter() -> Result<()> {
        let dispatcher = test_dispatcher()?;
        let attempts = std::sync::Arc::new(AtomicUsize::new(0));
        let attempts_for_hook = attempts.clone();
        dispatcher.register_callback("flaky", move |_invocation| {
            let attempts = attempts_for_hook.clone();
            async move {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(anyhow!("temporary hook failure"))
                } else {
                    Ok(HookDispatchOutcome {
                        additional_contexts: vec!["retried".to_string()],
                        ..HookDispatchOutcome::default()
                    })
                }
            }
        });
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![HookDefinition {
                    name: "flaky-hook".to_string(),
                    matcher: None,
                    failure_policy: kheish_types::HookFailurePolicy {
                        max_retries: 1,
                        ..Default::default()
                    },
                    executor: HookExecutorConfig::Callback {
                        name: "flaky".to_string(),
                        timeout_ms: Some(1_000),
                    },
                }],
            ))
            .collect(),
        })?;

        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::Notification,
                subject: None,
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(outcome.matched_hooks, vec!["flaky-hook".to_string()]);
        assert_eq!(outcome.additional_contexts, vec!["retried".to_string()]);
        assert!(dispatcher.dead_letters.load_all()?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn hook_fail_closed_blocks_and_records_dead_letter() -> Result<()> {
        let dispatcher = test_dispatcher()?;
        dispatcher.register_callback("broken", |_invocation| async {
            Err(anyhow!("closed hook failure"))
        });
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![HookDefinition {
                    name: "closed-hook".to_string(),
                    matcher: None,
                    failure_policy: kheish_types::HookFailurePolicy {
                        mode: kheish_types::HookFailureMode::Closed,
                        max_retries: 1,
                    },
                    executor: HookExecutorConfig::Callback {
                        name: "broken".to_string(),
                        timeout_ms: Some(1_000),
                    },
                }],
            ))
            .collect(),
        })?;

        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::Notification,
                subject: Some("notify".to_string()),
                session_id: Some("session-hook".to_string()),
                agent_id: None,
                run_id: Some("run-hook".to_string()),
                payload: Value::Null,
            })
            .await?;
        assert_eq!(outcome.matched_hooks, vec!["closed-hook".to_string()]);
        assert_eq!(outcome.decision, Some(HookDecision::Block));
        assert!(!outcome.continue_execution);
        assert!(
            outcome
                .stop_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("closed hook failure"))
        );
        let dead_letters = dispatcher.dead_letters.load_all()?;
        assert_eq!(dead_letters.len(), 1);
        assert_eq!(dead_letters[0].hook_name, "closed-hook");
        assert_eq!(dead_letters[0].attempt_count, 2);
        assert_eq!(dead_letters[0].session_id.as_deref(), Some("session-hook"));
        assert_eq!(dead_letters[0].run_id.as_deref(), Some("run-hook"));
        Ok(())
    }

    #[tokio::test]
    async fn hook_fail_closed_blocks_even_when_dead_letter_write_fails() -> Result<()> {
        let (dispatcher, observer) = test_dispatcher_with_observer(Vec::new())?;
        std::fs::write(&dispatcher.dead_letters.root, "not a directory")?;
        dispatcher.register_callback("broken", |_invocation| async {
            Err(anyhow!("clientSecret: closed-hook-secret"))
        });
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![HookDefinition {
                    name: "closed-hook".to_string(),
                    matcher: None,
                    failure_policy: kheish_types::HookFailurePolicy {
                        mode: kheish_types::HookFailureMode::Closed,
                        max_retries: 0,
                    },
                    executor: HookExecutorConfig::Callback {
                        name: "broken".to_string(),
                        timeout_ms: Some(1_000),
                    },
                }],
            ))
            .collect(),
        })?;

        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::Notification,
                subject: Some("notify".to_string()),
                session_id: Some("session-hook".to_string()),
                agent_id: None,
                run_id: Some("run-hook".to_string()),
                payload: Value::Null,
            })
            .await?;
        assert_eq!(outcome.matched_hooks, vec!["closed-hook".to_string()]);
        assert_eq!(outcome.decision, Some(HookDecision::Block));
        assert!(!outcome.continue_execution);
        assert_eq!(
            observer
                .metrics()
                .counters
                .get("hook.dead_letter_persist_failures")
                .copied(),
            Some(1)
        );
        assert!(
            outcome
                .stop_reason
                .as_deref()
                .is_some_and(|reason| !reason.contains("closed-hook-secret"))
        );
        assert_eq!(
            observer.metrics().counters.get("hook.failures").copied(),
            Some(1)
        );
        Ok(())
    }

    #[tokio::test]
    async fn hook_panic_honors_fail_open_and_fail_closed_policy() -> Result<()> {
        let (dispatcher, observer) = test_dispatcher_with_observer(Vec::new())?;
        dispatcher.register_callback("panic-open", |_invocation| async {
            panic!("open hook panic")
        });
        dispatcher.register_callback("panic-closed", |_invocation| async {
            panic!("closed hook panic")
        });
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![
                    HookDefinition {
                        name: "panic-open".to_string(),
                        matcher: Some("open".to_string()),
                        failure_policy: Default::default(),
                        executor: HookExecutorConfig::Callback {
                            name: "panic-open".to_string(),
                            timeout_ms: Some(1_000),
                        },
                    },
                    HookDefinition {
                        name: "panic-closed".to_string(),
                        matcher: Some("closed".to_string()),
                        failure_policy: kheish_types::HookFailurePolicy {
                            mode: kheish_types::HookFailureMode::Closed,
                            max_retries: 0,
                        },
                        executor: HookExecutorConfig::Callback {
                            name: "panic-closed".to_string(),
                            timeout_ms: Some(1_000),
                        },
                    },
                ],
            ))
            .collect(),
        })?;

        let open = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::Notification,
                subject: Some("open".to_string()),
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert!(open.continue_execution);
        assert!(open.matched_hooks.is_empty());

        let closed = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::Notification,
                subject: Some("closed".to_string()),
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert_eq!(closed.matched_hooks, vec!["panic-closed".to_string()]);
        assert_eq!(closed.decision, Some(HookDecision::Block));
        assert!(
            closed
                .stop_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("hook task panicked"))
        );
        assert_eq!(
            observer.metrics().counters.get("hook.failures").copied(),
            Some(2)
        );
        assert_eq!(dispatcher.dead_letters.load_all()?.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn hook_stop_reason_is_redacted_before_aggregation() -> Result<()> {
        let dispatcher = test_dispatcher()?;
        dispatcher.register_callback("secret-block", |_invocation| async {
            Ok(HookDispatchOutcome {
                continue_execution: false,
                decision: Some(HookDecision::Block),
                stop_reason: Some("api_key=sk-secret-stop-reason".to_string()),
                ..HookDispatchOutcome::default()
            })
        });
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![HookDefinition {
                    name: "secret-block".to_string(),
                    matcher: None,
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Callback {
                        name: "secret-block".to_string(),
                        timeout_ms: Some(1_000),
                    },
                }],
            ))
            .collect(),
        })?;

        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::Notification,
                subject: None,
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        let reason = outcome.stop_reason.expect("redacted stop reason");
        assert!(!reason.contains("sk-secret-stop-reason"));
        Ok(())
    }

    #[tokio::test]
    async fn hook_retry_timeout_uses_total_budget() -> Result<()> {
        let dispatcher = test_dispatcher()?;
        dispatcher.register_callback("slow", |_invocation| async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            Ok(HookDispatchOutcome::default())
        });
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![HookDefinition {
                    name: "slow".to_string(),
                    matcher: None,
                    failure_policy: kheish_types::HookFailurePolicy {
                        max_retries: super::MAX_HOOK_RETRIES,
                        ..Default::default()
                    },
                    executor: HookExecutorConfig::Callback {
                        name: "slow".to_string(),
                        timeout_ms: Some(120),
                    },
                }],
            ))
            .collect(),
        })?;

        let started = tokio::time::Instant::now();
        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::Notification,
                subject: None,
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        assert!(outcome.matched_hooks.is_empty());
        let dead_letters = dispatcher.dead_letters.load_all()?;
        assert_eq!(dead_letters.len(), 1);
        assert_eq!(dead_letters[0].attempt_count, 1);
        assert!(dead_letters[0].error.contains("timed out"));
        Ok(())
    }

    #[tokio::test]
    async fn prompt_and_agent_hook_contract_parse_errors_are_terminal() -> Result<()> {
        let prompt = test_dispatcher_with_responses(vec![
            r#"{"contract_version":999}"#.to_string(),
            r#"{"additional_contexts":["should-not-retry"]}"#.to_string(),
        ])?;
        prompt.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![HookDefinition {
                    name: "prompt-future-contract".to_string(),
                    matcher: Some("prompt".to_string()),
                    failure_policy: kheish_types::HookFailurePolicy {
                        max_retries: 1,
                        ..Default::default()
                    },
                    executor: HookExecutorConfig::Prompt {
                        template: "Return a hook outcome.".to_string(),
                        system_prompt: None,
                        model: None,
                        timeout_ms: Some(1_000),
                    },
                }],
            ))
            .collect(),
        })?;
        let prompt_outcome = prompt
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::Notification,
                subject: Some("prompt".to_string()),
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert!(prompt_outcome.matched_hooks.is_empty());
        let prompt_dead_letters = prompt.dead_letters.load_all()?;
        assert_eq!(prompt_dead_letters.len(), 1);
        assert_eq!(prompt_dead_letters[0].attempt_count, 1);
        assert!(
            prompt_dead_letters[0]
                .error
                .contains("unsupported hook outcome contract_version")
        );

        let agent = test_dispatcher_with_responses(vec![
            r#"{"contract_version":999}"#.to_string(),
            r#"{"additional_contexts":["should-not-retry"]}"#.to_string(),
        ])?;
        agent.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![HookDefinition {
                    name: "agent-future-contract".to_string(),
                    matcher: Some("agent".to_string()),
                    failure_policy: kheish_types::HookFailurePolicy {
                        max_retries: 1,
                        ..Default::default()
                    },
                    executor: HookExecutorConfig::Agent {
                        template: "Return a hook outcome.".to_string(),
                        system_prompt: None,
                        model: None,
                        tool_surface: None,
                        max_turns: Some(1),
                        timeout_ms: Some(1_000),
                    },
                }],
            ))
            .collect(),
        })?;
        let agent_outcome = agent
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::Notification,
                subject: Some("agent".to_string()),
                session_id: Some("agent-contract-session".to_string()),
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert!(agent_outcome.matched_hooks.is_empty());
        let agent_dead_letters = agent.dead_letters.load_all()?;
        assert_eq!(agent_dead_letters.len(), 1);
        assert_eq!(agent_dead_letters[0].attempt_count, 1);
        assert!(
            agent_dead_letters[0]
                .error
                .contains("unsupported hook outcome contract_version")
        );
        Ok(())
    }

    #[tokio::test]
    async fn gating_hooks_short_circuit_after_block() -> Result<()> {
        let dispatcher = test_dispatcher()?;
        let side_effects = Arc::new(AtomicUsize::new(0));
        dispatcher.register_callback("block", |_invocation| async {
            Ok(HookDispatchOutcome {
                continue_execution: false,
                decision: Some(HookDecision::Block),
                stop_reason: Some("blocked first".to_string()),
                ..HookDispatchOutcome::default()
            })
        });
        let side_effects_for_hook = side_effects.clone();
        dispatcher.register_callback("side-effect", move |_invocation| {
            let side_effects_for_hook = side_effects_for_hook.clone();
            async move {
                side_effects_for_hook.fetch_add(1, Ordering::SeqCst);
                Ok(HookDispatchOutcome::default())
            }
        });
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::UserPromptSubmit,
                vec![
                    HookDefinition {
                        name: "block".to_string(),
                        matcher: None,
                        failure_policy: Default::default(),
                        executor: HookExecutorConfig::Callback {
                            name: "block".to_string(),
                            timeout_ms: Some(1_000),
                        },
                    },
                    HookDefinition {
                        name: "side-effect".to_string(),
                        matcher: None,
                        failure_policy: Default::default(),
                        executor: HookExecutorConfig::Callback {
                            name: "side-effect".to_string(),
                            timeout_ms: Some(1_000),
                        },
                    },
                ],
            ))
            .collect(),
        })?;

        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::UserPromptSubmit,
                subject: Some("actor".to_string()),
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert_eq!(outcome.matched_hooks, vec!["block".to_string()]);
        assert_eq!(outcome.decision, Some(HookDecision::Block));
        assert_eq!(side_effects.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn pre_compact_hooks_short_circuit_after_block() -> Result<()> {
        let dispatcher = test_dispatcher()?;
        let side_effects = Arc::new(AtomicUsize::new(0));
        dispatcher.register_callback("block", |_invocation| async {
            Ok(HookDispatchOutcome {
                continue_execution: false,
                decision: Some(HookDecision::Block),
                stop_reason: Some("compaction blocked first".to_string()),
                ..HookDispatchOutcome::default()
            })
        });
        let side_effects_for_hook = side_effects.clone();
        dispatcher.register_callback("side-effect", move |_invocation| {
            let side_effects_for_hook = side_effects_for_hook.clone();
            async move {
                side_effects_for_hook.fetch_add(1, Ordering::SeqCst);
                Ok(HookDispatchOutcome::default())
            }
        });
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::PreCompact,
                vec![
                    HookDefinition {
                        name: "block".to_string(),
                        matcher: None,
                        failure_policy: Default::default(),
                        executor: HookExecutorConfig::Callback {
                            name: "block".to_string(),
                            timeout_ms: Some(1_000),
                        },
                    },
                    HookDefinition {
                        name: "side-effect".to_string(),
                        matcher: None,
                        failure_policy: Default::default(),
                        executor: HookExecutorConfig::Callback {
                            name: "side-effect".to_string(),
                            timeout_ms: Some(1_000),
                        },
                    },
                ],
            ))
            .collect(),
        })?;

        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::PreCompact,
                subject: None,
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert_eq!(outcome.matched_hooks, vec!["block".to_string()]);
        assert_eq!(outcome.decision, Some(HookDecision::Block));
        assert_eq!(side_effects.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn permission_request_denial_short_circuits_later_hooks() -> Result<()> {
        let dispatcher = test_dispatcher()?;
        let side_effects = Arc::new(AtomicUsize::new(0));
        dispatcher.register_callback("deny", |_invocation| async {
            Ok(HookDispatchOutcome {
                permission: Some(HookPermissionBehavior::Deny),
                ..HookDispatchOutcome::default()
            })
        });
        let side_effects_for_hook = side_effects.clone();
        dispatcher.register_callback("side-effect", move |_invocation| {
            let side_effects_for_hook = side_effects_for_hook.clone();
            async move {
                side_effects_for_hook.fetch_add(1, Ordering::SeqCst);
                Ok(HookDispatchOutcome::default())
            }
        });
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::PermissionRequest,
                vec![
                    HookDefinition {
                        name: "deny".to_string(),
                        matcher: None,
                        failure_policy: Default::default(),
                        executor: HookExecutorConfig::Callback {
                            name: "deny".to_string(),
                            timeout_ms: Some(1_000),
                        },
                    },
                    HookDefinition {
                        name: "side-effect".to_string(),
                        matcher: None,
                        failure_policy: Default::default(),
                        executor: HookExecutorConfig::Callback {
                            name: "side-effect".to_string(),
                            timeout_ms: Some(1_000),
                        },
                    },
                ],
            ))
            .collect(),
        })?;

        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::PermissionRequest,
                subject: Some("write_file".to_string()),
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert_eq!(outcome.matched_hooks, vec!["deny".to_string()]);
        assert_eq!(outcome.permission, Some(HookPermissionBehavior::Deny));
        assert_eq!(side_effects.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn permission_denied_hooks_preserve_retry_aggregation() -> Result<()> {
        let dispatcher = test_dispatcher()?;
        dispatcher.register_callback("notify", |_invocation| async {
            Ok(HookDispatchOutcome {
                continue_execution: false,
                decision: Some(HookDecision::Block),
                ..HookDispatchOutcome::default()
            })
        });
        dispatcher.register_callback("retry", |_invocation| async {
            Ok(HookDispatchOutcome {
                retry: true,
                additional_contexts: vec!["retry with context".to_string()],
                ..HookDispatchOutcome::default()
            })
        });
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::PermissionDenied,
                vec![
                    HookDefinition {
                        name: "notify".to_string(),
                        matcher: None,
                        failure_policy: Default::default(),
                        executor: HookExecutorConfig::Callback {
                            name: "notify".to_string(),
                            timeout_ms: Some(1_000),
                        },
                    },
                    HookDefinition {
                        name: "retry".to_string(),
                        matcher: None,
                        failure_policy: Default::default(),
                        executor: HookExecutorConfig::Callback {
                            name: "retry".to_string(),
                            timeout_ms: Some(1_000),
                        },
                    },
                ],
            ))
            .collect(),
        })?;

        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::PermissionDenied,
                subject: Some("write_file".to_string()),
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert_eq!(
            outcome.matched_hooks,
            vec!["notify".to_string(), "retry".to_string()]
        );
        assert!(outcome.retry);
        assert_eq!(
            outcome.additional_contexts,
            vec!["retry with context".to_string()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn gating_hook_panic_honors_fail_open_policy() -> Result<()> {
        let dispatcher = test_dispatcher()?;
        dispatcher.register_callback("panic-open", |_invocation| async {
            panic!("gating hook panic")
        });
        dispatcher.register_callback("after", |_invocation| async {
            Ok(HookDispatchOutcome {
                additional_contexts: vec!["after-panic".to_string()],
                ..HookDispatchOutcome::default()
            })
        });
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::UserPromptSubmit,
                vec![
                    HookDefinition {
                        name: "panic-open".to_string(),
                        matcher: None,
                        failure_policy: Default::default(),
                        executor: HookExecutorConfig::Callback {
                            name: "panic-open".to_string(),
                            timeout_ms: Some(1_000),
                        },
                    },
                    HookDefinition {
                        name: "after".to_string(),
                        matcher: None,
                        failure_policy: Default::default(),
                        executor: HookExecutorConfig::Callback {
                            name: "after".to_string(),
                            timeout_ms: Some(1_000),
                        },
                    },
                ],
            ))
            .collect(),
        })?;

        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::UserPromptSubmit,
                subject: Some("actor".to_string()),
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert_eq!(outcome.matched_hooks, vec!["after".to_string()]);
        assert_eq!(outcome.additional_contexts, vec!["after-panic".to_string()]);
        let dead_letters = dispatcher.dead_letters.load_all()?;
        assert_eq!(dead_letters.len(), 1);
        assert_eq!(dead_letters[0].hook_name, "panic-open");
        assert!(dead_letters[0].error.contains("hook task panicked"));
        Ok(())
    }

    #[tokio::test]
    async fn http_hook_rejects_private_dns_through_configured_dispatch() -> Result<()> {
        let mut dispatcher = test_dispatcher()?;
        dispatcher.set_http_resolver_for_test(Arc::new(StaticHookHttpResolver {
            addrs: vec![SocketAddr::from(([127, 0, 0, 1], 8443))],
        }));
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![HookDefinition {
                    name: "private-dns".to_string(),
                    matcher: None,
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Http {
                        url: "https://hooks.example.invalid/hook".to_string(),
                        timeout_ms: Some(1_000),
                    },
                }],
            ))
            .collect(),
        })?;

        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::Notification,
                subject: None,
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert!(outcome.matched_hooks.is_empty());
        let dead_letters = dispatcher.dead_letters.load_all()?;
        assert_eq!(dead_letters.len(), 1);
        assert_eq!(dead_letters[0].hook_name, "private-dns");
        assert!(
            dead_letters[0]
                .error
                .contains("hostnames resolving to private or local addresses")
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_hook_timeout_kills_descendant_process_group_and_audits_failure() -> Result<()>
    {
        let (dispatcher, observer) = test_dispatcher_with_observer(Vec::new())?;
        let temp = tempdir()?;
        let marker = temp.path().join("orphan-marker");
        let command = format!(
            "(sleep 1; printf orphan >> '{}') & sleep 5",
            marker.display()
        );
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![HookDefinition {
                    name: "timeout-tree".to_string(),
                    matcher: None,
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Command {
                        command,
                        shell: None,
                        timeout_ms: Some(100),
                    },
                }],
            ))
            .collect(),
        })?;

        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::Notification,
                subject: None,
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert!(outcome.matched_hooks.is_empty());
        tokio::time::sleep(std::time::Duration::from_millis(1_300)).await;
        assert!(
            !marker.exists(),
            "timed-out command hook descendant wrote {}",
            marker.display()
        );
        let dead_letters = dispatcher.dead_letters.load_all()?;
        assert_eq!(dead_letters.len(), 1);
        assert_eq!(dead_letters[0].hook_name, "timeout-tree");
        assert!(dead_letters[0].error.contains("timed out"));

        let external_actions = observer
            .traces()
            .into_iter()
            .filter_map(|event| match event.kind {
                TraceEventKind::ExternalAction {
                    phase,
                    kind,
                    outcome,
                    ..
                } if kind == "hook" => Some((phase, outcome)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(external_actions.len(), 2);
        assert_eq!(external_actions[0].0, "request");
        assert_eq!(external_actions[1].0, "response");
        assert!(
            external_actions[1]
                .1
                .as_deref()
                .is_some_and(|outcome| outcome.starts_with("failed:"))
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_hook_timeout_kills_child_that_keeps_stdio_after_shell_exit() -> Result<()> {
        let dispatcher = test_dispatcher()?;
        let temp = tempdir()?;
        let marker = temp.path().join("stdio-orphan-marker");
        let command = format!(
            "(sleep 1; printf orphan >> '{}') & printf '{{}}'",
            marker.display()
        );
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![HookDefinition {
                    name: "stdio-orphan".to_string(),
                    matcher: None,
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Command {
                        command,
                        shell: None,
                        timeout_ms: Some(100),
                    },
                }],
            ))
            .collect(),
        })?;

        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::Notification,
                subject: None,
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert!(outcome.matched_hooks.is_empty());
        tokio::time::sleep(std::time::Duration::from_millis(1_300)).await;
        assert!(
            !marker.exists(),
            "timed-out command hook descendant wrote {}",
            marker.display()
        );
        let dead_letters = dispatcher.dead_letters.load_all()?;
        assert_eq!(dead_letters.len(), 1);
        assert_eq!(dead_letters[0].hook_name, "stdio-orphan");
        assert!(dead_letters[0].error.contains("timed out"));
        Ok(())
    }

    #[test]
    fn hook_outcome_schema_includes_contract_version() {
        let schema = super::hook_outcome_schema();
        assert_eq!(
            schema
                .optional_fields
                .get("contract_version")
                .map(|field| &field.kind),
            Some(&StructuredValueKind::Number)
        );
    }

    #[test]
    fn hook_http_audit_target_redacts_path_query_and_userinfo() -> Result<()> {
        let url = reqwest::Url::parse(
            "https://user:secret@example.com:8443/path/with-token?token=secret#fragment",
        )?;
        assert_eq!(
            super::sanitized_hook_http_target(&url),
            "https://example.com:8443"
        );
        let invalid = super::sanitized_hook_http_target_from_str("not a url with secret-token");
        assert!(invalid.starts_with("invalid_url_sha256:"));
        assert!(!invalid.contains("secret"));
        Ok(())
    }

    #[test]
    fn hook_definition_serde_accepts_legacy_without_failure_policy() -> Result<()> {
        let definition: HookDefinition = serde_json::from_value(json!({
            "name": "legacy",
            "matcher": "notification",
            "executor": {
                "type": "command",
                "command": "printf '{}'"
            }
        }))?;
        assert_eq!(definition.failure_policy, Default::default());
        Ok(())
    }

    #[test]
    fn hook_settings_validation_rejects_dangerous_http_and_unbounded_timeouts() -> Result<()> {
        let invalid = HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![HookDefinition {
                    name: "bad-http".to_string(),
                    matcher: None,
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Http {
                        url: "http://localhost:9/hook".to_string(),
                        timeout_ms: Some(1_000),
                    },
                }],
            ))
            .collect(),
        };
        let error = super::validate_hook_settings(&invalid)
            .expect_err("localhost HTTP hook should not validate");
        assert_eq!(
            error
                .chain()
                .find_map(|cause| cause.downcast_ref::<crate::problems::DaemonProblem>())
                .map(|problem| problem.code),
            Some("invalid_hook_settings")
        );
        assert!(error.to_string().contains("localhost"));

        let too_long = HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![HookDefinition {
                    name: "slow".to_string(),
                    matcher: None,
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Command {
                        command: "true".to_string(),
                        shell: None,
                        timeout_ms: Some(super::MAX_CONFIGURED_HOOK_TIMEOUT_MS + 1),
                    },
                }],
            ))
            .collect(),
        };
        assert!(
            super::validate_hook_settings(&too_long)
                .expect_err("unbounded hook timeout should not validate")
                .to_string()
                .contains("must not exceed")
        );

        let broad_agent_tools = HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![HookDefinition {
                    name: "broad-agent".to_string(),
                    matcher: None,
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Agent {
                        template: "Inspect this event".to_string(),
                        system_prompt: None,
                        model: None,
                        tool_surface: Some(ToolSurfaceFilter {
                            allowlist: Vec::new(),
                            denylist: vec!["bash".to_string()],
                        }),
                        max_turns: Some(2),
                        timeout_ms: Some(1_000),
                    },
                }],
            ))
            .collect(),
        };
        assert!(
            super::validate_hook_settings(&broad_agent_tools)
                .expect_err("agent hook denylist-only tool surface should not validate")
                .to_string()
                .contains("tool_surface must include an allowlist")
        );
        Ok(())
    }

    #[test]
    fn hook_outcome_contract_version_is_checked() -> Result<()> {
        let current = super::parse_hook_outcome_text(
            r#"{"contract_version":1,"additional_contexts":["ok"]}"#,
        )?;
        assert_eq!(current.additional_contexts, vec!["ok".to_string()]);
        let future = super::parse_hook_outcome_text(r#"{"contract_version":999}"#)
            .expect_err("future hook contracts should be rejected");
        assert!(
            future
                .to_string()
                .contains("unsupported hook outcome contract_version")
        );
        for malformed in [
            r#"{"contract_version":"999"}"#,
            r#"{"contract_version":-1}"#,
            r#"{"contract_version":1.5}"#,
            r#"{"contract_version":{}}"#,
        ] {
            let error = super::parse_hook_outcome_text(malformed)
                .expect_err("malformed hook contract versions should be rejected");
            assert!(
                error
                    .to_string()
                    .contains("malformed hook outcome contract_version"),
                "{malformed}: {error}"
            );
        }
        Ok(())
    }

    #[test]
    fn hook_invocation_contract_includes_version_and_stable_key() -> Result<()> {
        let invocation = HookInvocation {
            event: kheish_types::HookEventName::Notification,
            subject: Some("subject".to_string()),
            session_id: Some("session".to_string()),
            agent_id: None,
            run_id: Some("run".to_string()),
            payload: json!({"message": "hello"}),
        };
        let contract = super::hook_invocation_contract(&invocation)?;
        assert_eq!(
            contract["contract_version"],
            json!(kheish_types::HOOK_CONTRACT_VERSION)
        );
        let key = contract["invocation_key"]
            .as_str()
            .expect("invocation key string");
        assert!(key.starts_with("hook-invocation-"));
        assert_eq!(contract, super::hook_invocation_contract(&invocation)?);
        Ok(())
    }

    #[tokio::test]
    async fn http_hook_validation_failures_record_audit_response() -> Result<()> {
        let (dispatcher, observer) = test_dispatcher_with_observer(Vec::new())?;
        let error = dispatcher
            .execute_definition(
                HookDefinitionView {
                    name: "bad-url".to_string(),
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Http {
                        url: "not a url with secret-token".to_string(),
                        timeout_ms: Some(1_000),
                    },
                },
                HookInvocation {
                    event: kheish_types::HookEventName::Notification,
                    subject: None,
                    session_id: None,
                    agent_id: None,
                    run_id: None,
                    payload: Value::Null,
                },
            )
            .await
            .expect_err("invalid HTTP hook URL should fail");
        assert!(error.to_string().contains("relative URL"));

        let external_actions = observer
            .traces()
            .into_iter()
            .filter_map(|event| match event.kind {
                TraceEventKind::ExternalAction {
                    phase,
                    kind,
                    target,
                    outcome,
                    ..
                } if kind == "hook" => Some((phase, target, outcome)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(external_actions.len(), 2);
        assert_eq!(external_actions[0].0, "request");
        assert!(external_actions[0].1.contains("invalid_url_sha256:"));
        assert!(!external_actions[0].1.contains("secret-token"));
        assert_eq!(external_actions[1].0, "response");
        assert_eq!(external_actions[1].2.as_deref(), Some("failed:invalid_url"));
        Ok(())
    }

    #[tokio::test]
    async fn http_hook_rejects_local_targets() -> Result<()> {
        async fn handle(Json(_payload): Json<HookInvocation>) -> Json<HookDispatchOutcome> {
            Json(HookDispatchOutcome {
                additional_contexts: vec!["http-hook".to_string()],
                ..HookDispatchOutcome::default()
            })
        }

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address: SocketAddr = listener.local_addr()?;
        tokio::spawn(async move {
            let app = Router::new().route("/", post(handle));
            axum::serve(listener, app).await.expect("http hook server");
        });

        let dispatcher = test_dispatcher()?;
        let rejected = dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![HookDefinition {
                    name: "http".to_string(),
                    matcher: None,
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Http {
                        url: format!("http://{address}/"),
                        timeout_ms: Some(2_000),
                    },
                }],
            ))
            .collect(),
        });
        assert!(
            rejected
                .expect_err("localhost HTTP hook should be rejected before persistence")
                .to_string()
                .contains("private or local addresses")
        );
        let error = dispatcher
            .execute_definition(
                HookDefinitionView {
                    name: "http".to_string(),
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Http {
                        url: format!("http://{address}/"),
                        timeout_ms: Some(2_000),
                    },
                },
                HookInvocation {
                    event: kheish_types::HookEventName::Notification,
                    subject: None,
                    session_id: None,
                    agent_id: None,
                    run_id: None,
                    payload: json!({"message": "hello"}),
                },
            )
            .await
            .expect_err("localhost HTTP hook should be rejected");
        assert!(error.to_string().contains("private or local addresses"));
        Ok(())
    }

    #[tokio::test]
    async fn http_hook_rejects_dns_targets_resolving_to_local_addresses() -> Result<()> {
        let error = super::validate_resolved_hook_http_addresses(&[SocketAddr::from((
            [127, 0, 0, 1],
            8080,
        ))])
        .expect_err("hostnames resolving to local addresses should be rejected");
        assert!(error.to_string().contains("private or local addresses"));
        Ok(())
    }

    #[tokio::test]
    async fn http_hook_request_uses_pinned_address_and_sends_v1_contract() -> Result<()> {
        let seen_payload = Arc::new(Mutex::new(None::<Value>));
        let seen_idempotency_key = Arc::new(Mutex::new(None::<String>));
        let payload_for_handler = seen_payload.clone();
        let key_for_handler = seen_idempotency_key.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            let app = Router::new().route(
                "/",
                post(move |headers: HeaderMap, Json(payload): Json<Value>| {
                    let payload_for_handler = payload_for_handler.clone();
                    let key_for_handler = key_for_handler.clone();
                    async move {
                        *payload_for_handler.lock() = Some(payload);
                        *key_for_handler.lock() = headers
                            .get("Idempotency-Key")
                            .and_then(|value| value.to_str().ok())
                            .map(ToOwned::to_owned);
                        Json(HookDispatchOutcome::default())
                    }
                }),
            );
            axum::serve(listener, app).await.expect("http hook server");
        });

        let invocation = HookInvocation {
            event: kheish_types::HookEventName::Notification,
            subject: Some("notify".to_string()),
            session_id: Some("session-http".to_string()),
            agent_id: None,
            run_id: Some("run-http".to_string()),
            payload: json!({"message": "hello"}),
        };
        let url = reqwest::Url::parse("http://hooks.example.invalid/")?;
        let (outcome, _digest) =
            super::execute_http_hook_request("contract", url, &[address], 1_000, &invocation)
                .await?;
        assert!(outcome?.continue_execution);

        let payload = seen_payload
            .lock()
            .clone()
            .expect("hook server saw payload");
        assert_eq!(
            payload["contract_version"],
            json!(kheish_types::HOOK_CONTRACT_VERSION)
        );
        assert!(
            payload["invocation_key"]
                .as_str()
                .is_some_and(|value| value.starts_with("hook-invocation-"))
        );
        assert!(
            seen_idempotency_key
                .lock()
                .as_deref()
                .is_some_and(|value| value.starts_with("hook-execution-"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn http_hook_request_rejects_redirect_without_following() -> Result<()> {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_handler = hits.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            let app = Router::new().route(
                "/",
                post(move || {
                    let hits_for_handler = hits_for_handler.clone();
                    async move {
                        hits_for_handler.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::FOUND,
                            [(header::LOCATION, "http://127.0.0.1:1/private")],
                            "",
                        )
                    }
                }),
            );
            axum::serve(listener, app).await.expect("http hook server");
        });

        let error = super::execute_http_hook_request(
            "redirect",
            reqwest::Url::parse("http://hooks.example.invalid/")?,
            &[address],
            1_000,
            &HookInvocation {
                event: kheish_types::HookEventName::Notification,
                subject: None,
                session_id: None,
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            },
        )
        .await
        .expect_err("redirects should be terminal");
        assert!(error.to_string().contains("redirects are not allowed"));
        assert!(!super::hook_failure_allows_retry(&error));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn http_hook_retry_classification_distinguishes_4xx_and_retry_after() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            let app = Router::new()
                .route(
                    "/client",
                    post(|| async { (StatusCode::BAD_REQUEST, "bad hook payload") }),
                )
                .route(
                    "/busy",
                    post(|| async {
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            [(header::RETRY_AFTER, "1")],
                            "retry later",
                        )
                    }),
                );
            axum::serve(listener, app).await.expect("http hook server");
        });
        let invocation = HookInvocation {
            event: kheish_types::HookEventName::Notification,
            subject: None,
            session_id: None,
            agent_id: None,
            run_id: None,
            payload: Value::Null,
        };

        let client_error = super::execute_http_hook_request(
            "client",
            reqwest::Url::parse("http://hooks.example.invalid/client")?,
            &[address],
            1_000,
            &invocation,
        )
        .await
        .expect_err("4xx should be terminal");
        assert!(!super::hook_failure_allows_retry(&client_error));

        let busy_error = super::execute_http_hook_request(
            "busy",
            reqwest::Url::parse("http://hooks.example.invalid/busy")?,
            &[address],
            1_000,
            &invocation,
        )
        .await
        .expect_err("429 should be retryable");
        assert!(super::hook_failure_allows_retry(&busy_error));
        assert_eq!(
            super::hook_failure_retry_after(&busy_error),
            Some(std::time::Duration::from_secs(1))
        );
        Ok(())
    }

    #[test]
    fn http_hook_blocks_special_literal_ips_and_ipv4_mapped_loopback() {
        for address in [
            IpAddr::from([0, 1, 2, 3]),
            IpAddr::from([100, 64, 0, 1]),
            IpAddr::from([169, 254, 169, 254]),
            IpAddr::from([192, 0, 0, 1]),
            IpAddr::from([192, 0, 2, 10]),
            IpAddr::from([192, 88, 99, 1]),
            IpAddr::from([198, 18, 0, 1]),
            IpAddr::from([240, 0, 0, 1]),
            IpAddr::V6("::ffff:127.0.0.1".parse().expect("mapped loopback")),
            IpAddr::V6("::127.0.0.1".parse().expect("compatible loopback")),
            IpAddr::V6("64:ff9b::127.0.0.1".parse().expect("nat64 loopback")),
            IpAddr::V6("64:ff9b:1::".parse().expect("local-use nat64")),
            IpAddr::V6("2002:7f00:1::".parse().expect("6to4 loopback")),
            IpAddr::V6("2001::1".parse().expect("teredo")),
            IpAddr::V6("2001:db8::1".parse().expect("documentation")),
            IpAddr::V6("2001:20::1".parse().expect("orchidv2")),
        ] {
            assert!(
                super::is_blocked_hook_ip(address),
                "{address} should be blocked for HTTP hooks"
            );
        }
    }

    #[test]
    fn hook_dead_letter_views_redact_legacy_errors() {
        let view = crate::HookDeadLetterView::from(super::HookDeadLetterRecord {
            id: "legacy".to_string(),
            at_ms: 1,
            hook_name: "legacy".to_string(),
            event: kheish_types::HookEventName::Notification,
            subject: None,
            session_id: None,
            run_id: None,
            target: "command:legacy".to_string(),
            attempt_count: 1,
            failure_mode: Default::default(),
            contract_version: kheish_types::HOOK_CONTRACT_VERSION,
            invocation_digest: String::new(),
            definition_digest: String::new(),
            error: format!(
                "hook stderr api_key={}{}",
                "sk-", "legacy-dead-letter-secret"
            ),
            resolved_at_ms: Some(2),
            resolution_reason: Some(format!(
                "operator reason api_key={}{}",
                "sk-", "legacy-resolution-secret"
            )),
        });
        let dead_letter_secret = format!("{}{}", "sk-", "legacy-dead-letter-secret");
        let resolution_secret = format!("{}{}", "sk-", "legacy-resolution-secret");
        assert!(!view.error.contains(&dead_letter_secret));
        assert!(view.error.contains("api_key=[REDACTED]"));
        assert!(
            view.resolution_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("api_key=[REDACTED]"))
        );
        assert!(
            !view
                .resolution_reason
                .as_deref()
                .unwrap_or_default()
                .contains(&resolution_secret)
        );
    }

    #[test]
    fn hook_dead_letter_summary_tolerates_corrupt_records() -> Result<()> {
        let root = tempdir()?.keep();
        let store = super::FileHookDeadLetterStore::new(&root);
        std::fs::create_dir_all(&store.root)?;
        std::fs::write(store.root.join("100-bad.json"), "{not-json")?;
        std::fs::write(
            store.root.join("200-good.json"),
            serde_json::to_string(&super::HookDeadLetterRecord {
                id: "good".to_string(),
                at_ms: 200,
                hook_name: "good-hook".to_string(),
                event: kheish_types::HookEventName::Notification,
                subject: None,
                session_id: None,
                run_id: None,
                target: "callback:good".to_string(),
                attempt_count: 1,
                failure_mode: Default::default(),
                contract_version: kheish_types::HOOK_CONTRACT_VERSION,
                invocation_digest: String::new(),
                definition_digest: String::new(),
                error: "safe".to_string(),
                resolved_at_ms: None,
                resolution_reason: None,
            })?,
        )?;

        let summary = store.summary();
        assert_eq!(summary.count, 2);
        assert_eq!(summary.last_at_ms, Some(200));
        assert_eq!(summary.last_hook.as_deref(), Some("good-hook"));
        let records = store.load_recent(10)?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].hook_name, "good-hook");
        Ok(())
    }

    #[test]
    fn hook_dead_letter_prune_keeps_newest_records_within_limits() -> Result<()> {
        let root = tempdir()?.keep();
        let store = super::FileHookDeadLetterStore::new(&root);
        std::fs::create_dir_all(&store.root)?;
        std::fs::write(store.root.join("100-old.json"), "{}")?;
        std::fs::write(store.root.join("200-mid.json"), "{}")?;
        std::fs::write(store.root.join("300-new.json"), "{}")?;

        store.prune_with_limits(2, u64::MAX)?;

        assert!(!store.root.join("100-old.json").exists());
        assert!(store.root.join("200-mid.json").exists());
        assert!(store.root.join("300-new.json").exists());
        Ok(())
    }

    #[test]
    fn hook_dead_letter_prune_compacts_resolution_ledger_to_live_records() -> Result<()> {
        let root = tempdir()?.keep();
        let store = super::FileHookDeadLetterStore::new(&root);
        std::fs::create_dir_all(&store.root)?;
        for (id, at_ms) in [("old", 100), ("mid", 200), ("new", 300)] {
            std::fs::write(
                store.root.join(format!("{at_ms}-{id}.json")),
                serde_json::to_string(&super::HookDeadLetterRecord {
                    id: id.to_string(),
                    at_ms,
                    hook_name: id.to_string(),
                    event: kheish_types::HookEventName::Notification,
                    subject: None,
                    session_id: None,
                    run_id: None,
                    target: format!("callback:{id}"),
                    attempt_count: 1,
                    failure_mode: Default::default(),
                    contract_version: kheish_types::HOOK_CONTRACT_VERSION,
                    invocation_digest: String::new(),
                    definition_digest: String::new(),
                    error: "safe".to_string(),
                    resolved_at_ms: None,
                    resolution_reason: None,
                })?,
            )?;
        }
        let resolutions = [
            super::HookDeadLetterResolutionRecord {
                dead_letter_id: "old".to_string(),
                resolved_at_ms: 1,
                reason: "old".to_string(),
            },
            super::HookDeadLetterResolutionRecord {
                dead_letter_id: "mid".to_string(),
                resolved_at_ms: 2,
                reason: "token=sk-mid-secret".to_string(),
            },
            super::HookDeadLetterResolutionRecord {
                dead_letter_id: "new".to_string(),
                resolved_at_ms: 3,
                reason: "new".to_string(),
            },
        ];
        let mut body = String::new();
        for resolution in resolutions {
            body.push_str(&serde_json::to_string(&resolution)?);
            body.push('\n');
        }
        std::fs::write(store.resolved_dead_letter_path(), body)?;

        store.prune_with_limits(2, u64::MAX)?;

        let compacted = store.load_resolutions()?;
        let ids = compacted
            .iter()
            .map(|record| record.dead_letter_id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(ids, BTreeSet::from(["mid", "new"]));
        assert!(
            compacted
                .iter()
                .find(|record| record.dead_letter_id == "mid")
                .is_some_and(|record| record.reason.contains("token=[REDACTED]"))
        );
        Ok(())
    }

    #[test]
    fn redacted_hook_settings_keep_shape_without_executor_secrets() {
        let settings = HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::Notification,
                vec![
                    HookDefinition {
                        name: "command".to_string(),
                        matcher: None,
                        failure_policy: Default::default(),
                        executor: HookExecutorConfig::Command {
                            command: "curl https://example.com?token=secret".to_string(),
                            shell: None,
                            timeout_ms: Some(1_000),
                        },
                    },
                    HookDefinition {
                        name: "http".to_string(),
                        matcher: None,
                        failure_policy: Default::default(),
                        executor: HookExecutorConfig::Http {
                            url: "https://example.com/hook?token=secret".to_string(),
                            timeout_ms: Some(1_000),
                        },
                    },
                ],
            ))
            .collect(),
        };
        let redacted = super::redacted_hook_settings(&settings);
        let rendered = serde_json::to_string(&redacted).expect("redacted hooks serialize");
        assert!(!rendered.contains("curl"));
        assert!(!rendered.contains("token=secret"));
        assert!(rendered.contains("https://example.com"));
    }

    #[tokio::test]
    async fn hook_settings_store_roundtrips() -> Result<()> {
        let root = std::env::temp_dir().join(format!("kheish-hook-store-test-{}", now_ms()));
        std::fs::create_dir_all(&root)?;
        let store = FileHookSettingsStore::new(&root);
        let settings = HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::ConfigChange,
                vec![HookDefinition {
                    name: "audit".to_string(),
                    matcher: Some("runtime_api".to_string()),
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Command {
                        command: "printf '{}'".to_string(),
                        shell: None,
                        timeout_ms: None,
                    },
                }],
            ))
            .collect(),
        };
        store.save(&settings)?;
        let raw: Value = serde_json::from_slice(&std::fs::read(&store.path)?)?;
        assert_eq!(
            raw["schema_version"],
            json!(super::HOOK_SETTINGS_SCHEMA_VERSION)
        );
        assert!(raw["hooks"].is_object());
        assert_eq!(store.load()?, settings);

        let legacy = serde_json::to_vec_pretty(&settings)?;
        std::fs::write(&store.path, legacy)?;
        assert_eq!(store.load()?, settings);

        std::fs::write(
            &store.path,
            serde_json::to_vec_pretty(&json!({
                "schema_version": 999,
                "hooks": settings.hooks,
            }))?,
        )?;
        assert_eq!(store.load()?, HookSettings::default());
        Ok(())
    }

    #[tokio::test]
    async fn prompt_hook_returns_structured_outcome() -> Result<()> {
        let dispatcher = test_dispatcher_with_responses(vec![
            r#"{"additional_contexts":["prompt-hook"],"continue_execution":true}"#.to_string(),
        ])?;
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::SessionStart,
                vec![HookDefinition {
                    name: "prompt".to_string(),
                    matcher: None,
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Prompt {
                        template: "Check {{subject}}".to_string(),
                        system_prompt: None,
                        model: None,
                        timeout_ms: Some(2_000),
                    },
                }],
            ))
            .collect(),
        })?;
        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::SessionStart,
                subject: Some("session-a".to_string()),
                session_id: Some("session-a".to_string()),
                agent_id: None,
                run_id: None,
                payload: Value::Null,
            })
            .await?;
        assert_eq!(outcome.additional_contexts, vec!["prompt-hook".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn agent_hook_returns_structured_outcome() -> Result<()> {
        let dispatcher = test_dispatcher_with_responses(vec![
            r#"{"additional_contexts":["agent-hook"],"continue_execution":true}"#.to_string(),
        ])?;
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::PostCompact,
                vec![HookDefinition {
                    name: "agent".to_string(),
                    matcher: None,
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Agent {
                        template: "Summarize {{subject}}".to_string(),
                        system_prompt: Some("Return valid hook JSON.".to_string()),
                        model: None,
                        tool_surface: None,
                        max_turns: Some(2),
                        timeout_ms: Some(2_000),
                    },
                }],
            ))
            .collect(),
        })?;
        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::PostCompact,
                subject: Some("compact".to_string()),
                session_id: Some("session-b".to_string()),
                agent_id: None,
                run_id: Some("run-1".to_string()),
                payload: json!({"summary": "done"}),
            })
            .await?;
        assert_eq!(outcome.additional_contexts, vec!["agent-hook".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn callback_hooks_can_emit_permission_updates() -> Result<()> {
        let dispatcher = test_dispatcher()?;
        dispatcher.register_callback("permissions", |_invocation| async move {
            Ok(HookDispatchOutcome {
                updated_permissions: vec![HookPermissionUpdate {
                    scope: HookPermissionUpdateScope::Session,
                    tool_name_pattern: "write_file".to_string(),
                    behavior: HookPermissionUpdateBehavior::Allow,
                    reason: Some("trusted by hook".to_string()),
                }],
                permission: Some(HookPermissionBehavior::Allow),
                ..HookDispatchOutcome::default()
            })
        });
        dispatcher.set_settings(HookSettings {
            hooks: std::iter::once((
                kheish_types::HookEventName::PermissionRequest,
                vec![HookDefinition {
                    name: "permissions".to_string(),
                    matcher: Some("write_file".to_string()),
                    failure_policy: Default::default(),
                    executor: HookExecutorConfig::Callback {
                        name: "permissions".to_string(),
                        timeout_ms: None,
                    },
                }],
            ))
            .collect(),
        })?;

        let outcome = dispatcher
            .dispatch(HookInvocation {
                event: kheish_types::HookEventName::PermissionRequest,
                subject: Some("write_file".to_string()),
                session_id: Some("session-a".to_string()),
                agent_id: None,
                run_id: None,
                payload: json!({"tool_call": {"name": "write_file"}}),
            })
            .await?;

        assert_eq!(outcome.permission, Some(HookPermissionBehavior::Allow));
        assert_eq!(
            outcome.updated_permissions,
            vec![HookPermissionUpdate {
                scope: HookPermissionUpdateScope::Session,
                tool_name_pattern: "write_file".to_string(),
                behavior: HookPermissionUpdateBehavior::Allow,
                reason: Some("trusted by hook".to_string()),
            }]
        );
        Ok(())
    }

    #[test]
    fn hook_outcome_parser_accepts_fenced_json() -> Result<()> {
        let outcome = super::parse_hook_outcome_text(
            "```json\n{\"decision\":\"block\",\"continue_execution\":false}\n```",
        )?;
        assert_eq!(outcome.decision, Some(HookDecision::Block));
        assert!(!outcome.continue_execution);
        Ok(())
    }
}
