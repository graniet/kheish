use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use kheish_output::{OutputManifest, OutputPlugin, ResponseEnvelope};
use kheish_runtime::{
    ExecutionScope, failed_reqwest_external_action_outcome, redact_text, scope_execution,
};
use kheish_session::{append_json_line_sync, write_json_pretty_atomically};
use kheish_types::{AttachmentRef, ContentPart, ConversationKey, ReplyHandle};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::now_ms;
use crate::state_files::read_json_or_quarantine;

const INITIAL_RETRY_DELAY_MS_ENV: &str = "KHEISH_DELIVERY_INITIAL_RETRY_MS";
const MAX_RETRY_DELAY_MS_ENV: &str = "KHEISH_DELIVERY_MAX_RETRY_MS";
const MAX_RETRY_AFTER_DELAY_MS_ENV: &str = "KHEISH_DELIVERY_MAX_RETRY_AFTER_MS";
const MAX_DELIVERY_ATTEMPTS_ENV: &str = "KHEISH_DELIVERY_MAX_ATTEMPTS";
const TARGET_MIN_INTERVAL_MS_ENV: &str = "KHEISH_DELIVERY_TARGET_MIN_INTERVAL_MS";
const TARGET_CIRCUIT_FAILURE_THRESHOLD_ENV: &str = "KHEISH_DELIVERY_CIRCUIT_FAILURE_THRESHOLD";
const TARGET_CIRCUIT_COOLDOWN_MS_ENV: &str = "KHEISH_DELIVERY_CIRCUIT_COOLDOWN_MS";
const DEFAULT_INITIAL_RETRY_DELAY_MS: u64 = 1_000;
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;
const DEFAULT_MAX_RETRY_AFTER_DELAY_MS: u64 = 60 * 60 * 1_000;
const DEFAULT_MAX_DELIVERY_ATTEMPTS: u32 = 8;
const DEFAULT_TARGET_MIN_INTERVAL_MS: u64 = 0;
const DEFAULT_TARGET_CIRCUIT_FAILURE_THRESHOLD: u32 = 3;
const DEFAULT_TARGET_CIRCUIT_COOLDOWN_MS: u64 = 60_000;
const WORKER_ERROR_RETRY_DELAY: Duration = Duration::from_secs(1);
const DEFAULT_BULK_REPLAY_LIMIT: usize = 100;
const MAX_BULK_REPLAY_LIMIT: usize = 500;
const MAX_TERMINAL_DELIVERY_ERROR_DETAIL_CHARS: usize = 512;
const DELIVERY_IDEMPOTENCY_METADATA_KEY: &str = "delivery_idempotency_key";

#[derive(Debug)]
struct TerminalDeliveryError {
    detail: String,
}

impl std::fmt::Display for TerminalDeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for TerminalDeliveryError {}

pub(crate) fn terminal_delivery_error(detail: impl Into<String>) -> anyhow::Error {
    TerminalDeliveryError {
        detail: detail.into(),
    }
    .into()
}

pub(crate) fn is_terminal_delivery_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<TerminalDeliveryError>().is_some()
}

#[derive(Debug)]
struct RetryAfterDeliveryError {
    retry_after_ms: u64,
    detail: String,
}

impl std::fmt::Display for RetryAfterDeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for RetryAfterDeliveryError {}

pub(crate) fn retry_after_delivery_error(
    retry_after_ms: u64,
    detail: impl Into<String>,
) -> anyhow::Error {
    RetryAfterDeliveryError {
        retry_after_ms: retry_after_ms.max(1),
        detail: detail.into(),
    }
    .into()
}

pub(crate) fn retry_after_delivery_error_ms(error: &anyhow::Error) -> Option<u64> {
    error
        .downcast_ref::<RetryAfterDeliveryError>()
        .map(|error| error.retry_after_ms)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct PendingDeliveryRecord {
    pub id: String,
    pub conversation: ConversationKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub reply: ReplyHandle,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<AttachmentRef>,
    pub metadata: Value,
    pub attempts: u32,
    pub next_attempt_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Pending,
    Retrying,
    Delivered,
    DeadLettered,
}

impl DeliveryStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Retrying => "retrying",
            Self::Delivered => "delivered",
            Self::DeadLettered => "dead_lettered",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeliveryView {
    pub delivery_id: String,
    pub status: DeliveryStatus,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub plugin: String,
    pub target: String,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_attempt_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead_lettered_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead_letter_resolved_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead_letter_resolution_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replayed_from_delivery_id: Option<String>,
    pub content_size_bytes: usize,
    pub parts_count: usize,
    pub artifacts_count: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DeliveryListFilter {
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub plugin: Option<String>,
    pub status: Option<DeliveryStatus>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeliveryReplayResponse {
    pub replayed_from_delivery_id: String,
    pub delivery: DeliveryView,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryBulkReplayAction {
    WouldReplay,
    Replayed,
    ExistingReplay,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeliveryBulkReplayItem {
    pub replayed_from_delivery_id: String,
    pub source: DeliveryView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<DeliveryView>,
    pub action: DeliveryBulkReplayAction,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeliveryBulkReplayResponse {
    pub dry_run: bool,
    pub force: bool,
    pub unresolved_only: bool,
    pub limit: usize,
    pub matched: usize,
    pub selected: usize,
    pub replayed: usize,
    pub existing_replays: usize,
    pub truncated: usize,
    pub items: Vec<DeliveryBulkReplayItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryBackpressureResetResponse {
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    pub matched: usize,
    pub removed: usize,
    pub targets: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryQueueStatusView {
    pub pending: usize,
    pub retrying: usize,
    pub delivered: usize,
    pub dead_lettered: usize,
    #[serde(default)]
    pub unresolved_dead_lettered: usize,
    pub ready: usize,
    #[serde(default)]
    pub dispatchable: usize,
    pub target_count: usize,
    pub blocked_target_count: usize,
    #[serde(default)]
    pub throttled_target_count: usize,
    #[serde(default)]
    pub open_circuit_target_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_attempt_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_target_available_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_heartbeat_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_lag_ms: Option<u64>,
    #[serde(default)]
    pub status_error_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_error: Option<String>,
}

impl DeliveryQueueStatusView {
    pub fn render_prometheus_metrics(&self) -> String {
        let mut body = String::new();
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_pending",
            "Output deliveries that have never been attempted.",
            self.pending,
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_retrying",
            "Output deliveries waiting for a retry after at least one failed attempt.",
            self.retrying,
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_delivered",
            "Persisted delivered output delivery records.",
            self.delivered,
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_dead_lettered",
            "Persisted dead-lettered output delivery records.",
            self.dead_lettered,
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_unresolved_dead_lettered",
            "Dead-lettered output deliveries without a completed replay.",
            self.unresolved_dead_lettered,
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_ready",
            "Output deliveries whose own next attempt time has elapsed.",
            self.ready,
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_dispatchable",
            "Target-head output deliveries that can be dispatched now.",
            self.dispatchable,
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_targets",
            "Distinct delivery target heads currently tracked by the queue.",
            self.target_count,
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_targets_blocked",
            "Delivery targets with ready work blocked behind an earlier retry.",
            self.blocked_target_count,
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_targets_throttled",
            "Delivery target heads delayed by per-target backpressure.",
            self.throttled_target_count,
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_targets_open_circuit",
            "Delivery target circuits currently open after consecutive retryable failures.",
            self.open_circuit_target_count,
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_next_attempt_at_ms",
            "Unix timestamp in milliseconds for the next target-head delivery attempt, or zero.",
            self.next_attempt_at_ms.unwrap_or_default(),
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_next_target_available_at_ms",
            "Unix timestamp in milliseconds for the next target-head delivery after target backpressure, or zero.",
            self.next_target_available_at_ms.unwrap_or_default(),
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_worker_started_at_ms",
            "Unix timestamp in milliseconds for the current delivery worker start time, or zero.",
            self.worker_started_at_ms.unwrap_or_default(),
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_worker_heartbeat_at_ms",
            "Unix timestamp in milliseconds for the latest delivery worker heartbeat, or zero.",
            self.worker_heartbeat_at_ms.unwrap_or_default(),
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_worker_lag_ms",
            "Age in milliseconds of the latest delivery worker heartbeat, or zero.",
            self.worker_lag_ms.unwrap_or_default(),
        );
        push_prometheus_gauge(
            &mut body,
            "kheish_delivery_status_errors",
            "Delivery status ledger summary errors.",
            self.status_error_count,
        );
        body
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct DeliveryQueueSnapshot {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    pending: BTreeMap<String, PendingDeliveryRecord>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct DeliveryQueueMeta {
    #[serde(default)]
    next_id: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct TargetBackpressureSnapshot {
    #[serde(default)]
    targets: BTreeMap<String, TargetBackpressureRecord>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct TargetBackpressureRecord {
    target: String,
    #[serde(default)]
    last_attempt_at_ms: u64,
    #[serde(default)]
    consecutive_retryable_failures: u32,
    #[serde(default)]
    blocked_until_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    circuit_opened_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct DeadLetterDeliveryRecord {
    dropped_at_ms: u64,
    terminal_error: String,
    record: PendingDeliveryRecord,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ResolvedDeadLetterDeliveryRecord {
    delivery_id: String,
    resolved_at_ms: u64,
    reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct CompletedDeliveryRecord {
    delivered_at_ms: u64,
    record: PendingDeliveryRecord,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ResolvedDeadLetterSourceMarker {
    delivery_id: String,
    resolved_at_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct DeliveryLedgerStatusCounts {
    #[serde(default)]
    delivered: usize,
    #[serde(default)]
    dead_lettered: usize,
    #[serde(default)]
    unresolved_dead_lettered: usize,
    #[serde(default)]
    status_error_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status_error: Option<String>,
    // Append-only ledger byte offsets. They detect missed daemon appends without
    // re-scanning terminal ledgers on every status request.
    #[serde(default, alias = "completed_ledger_bytes")]
    completed_ledger_offset_bytes: u64,
    #[serde(default, alias = "dead_letter_ledger_bytes")]
    dead_letter_ledger_offset_bytes: u64,
    #[serde(default, alias = "resolved_dead_letter_ledger_bytes")]
    resolved_dead_letter_ledger_offset_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DeliveryLedgerScanStatus {
    parse_error_count: usize,
    first_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DeliveryRetryPolicy {
    initial_retry_delay_ms: u64,
    max_retry_delay_ms: u64,
    max_retry_after_delay_ms: u64,
    max_attempts: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DeliveryTargetBackpressurePolicy {
    min_interval_ms: u64,
    circuit_failure_threshold: u32,
    circuit_cooldown_ms: u64,
}

impl Default for DeliveryRetryPolicy {
    fn default() -> Self {
        Self {
            initial_retry_delay_ms: DEFAULT_INITIAL_RETRY_DELAY_MS,
            max_retry_delay_ms: DEFAULT_MAX_RETRY_DELAY_MS,
            max_retry_after_delay_ms: DEFAULT_MAX_RETRY_AFTER_DELAY_MS,
            max_attempts: DEFAULT_MAX_DELIVERY_ATTEMPTS,
        }
    }
}

impl Default for DeliveryTargetBackpressurePolicy {
    fn default() -> Self {
        Self {
            min_interval_ms: DEFAULT_TARGET_MIN_INTERVAL_MS,
            circuit_failure_threshold: DEFAULT_TARGET_CIRCUIT_FAILURE_THRESHOLD,
            circuit_cooldown_ms: DEFAULT_TARGET_CIRCUIT_COOLDOWN_MS,
        }
    }
}

impl DeliveryRetryPolicy {
    fn load_from_env() -> Result<Self> {
        let mut policy = Self::default();
        if let Some(value) = std::env::var_os(INITIAL_RETRY_DELAY_MS_ENV) {
            policy.initial_retry_delay_ms =
                parse_delivery_env_u64(INITIAL_RETRY_DELAY_MS_ENV, &value, 1)?;
        }
        if let Some(value) = std::env::var_os(MAX_RETRY_DELAY_MS_ENV) {
            policy.max_retry_delay_ms = parse_delivery_env_u64(MAX_RETRY_DELAY_MS_ENV, &value, 1)?;
        }
        if let Some(value) = std::env::var_os(MAX_RETRY_AFTER_DELAY_MS_ENV) {
            policy.max_retry_after_delay_ms =
                parse_delivery_env_u64(MAX_RETRY_AFTER_DELAY_MS_ENV, &value, 1)?;
        }
        if let Some(value) = std::env::var_os(MAX_DELIVERY_ATTEMPTS_ENV) {
            policy.max_attempts = parse_delivery_env_u32(MAX_DELIVERY_ATTEMPTS_ENV, &value, 1)?;
        }
        if policy.max_retry_delay_ms < policy.initial_retry_delay_ms {
            policy.max_retry_delay_ms = policy.initial_retry_delay_ms;
        }
        Ok(policy)
    }
}

impl DeliveryTargetBackpressurePolicy {
    fn load_from_env() -> Result<Self> {
        let mut policy = Self::default();
        if let Some(value) = std::env::var_os(TARGET_MIN_INTERVAL_MS_ENV) {
            policy.min_interval_ms = parse_delivery_env_u64(TARGET_MIN_INTERVAL_MS_ENV, &value, 0)?;
        }
        if let Some(value) = std::env::var_os(TARGET_CIRCUIT_FAILURE_THRESHOLD_ENV) {
            policy.circuit_failure_threshold =
                parse_delivery_env_u32(TARGET_CIRCUIT_FAILURE_THRESHOLD_ENV, &value, 0)?;
        }
        if let Some(value) = std::env::var_os(TARGET_CIRCUIT_COOLDOWN_MS_ENV) {
            policy.circuit_cooldown_ms =
                parse_delivery_env_u64(TARGET_CIRCUIT_COOLDOWN_MS_ENV, &value, 1)?;
        }
        Ok(policy)
    }
}

fn parse_delivery_env_u64(name: &str, value: &std::ffi::OsStr, min: u64) -> Result<u64> {
    let parsed = value
        .to_string_lossy()
        .parse::<u64>()
        .map_err(|_| anyhow!("{name} must be an unsigned integer"))?;
    anyhow::ensure!(parsed >= min, "{name} must be at least {min}");
    Ok(parsed)
}

fn parse_delivery_env_u32(name: &str, value: &std::ffi::OsStr, min: u32) -> Result<u32> {
    let parsed = value
        .to_string_lossy()
        .parse::<u32>()
        .map_err(|_| anyhow!("{name} must be an unsigned integer"))?;
    anyhow::ensure!(parsed >= min, "{name} must be at least {min}");
    Ok(parsed)
}

fn safe_delivery_error(error: &anyhow::Error) -> String {
    if is_terminal_delivery_error(error) {
        let lower = error.to_string().to_ascii_lowercase();
        if lower.contains("external fetch")
            && (lower.contains("allowlist")
                || lower.contains("non-public")
                || lower.contains("private")
                || lower.contains("loopback")
                || lower.contains("unsupported url scheme")
                || lower.contains("user info")
                || lower.contains("missing a hostname"))
        {
            return "failed:blocked_external_fetch".to_string();
        }
        return "failed:terminal".to_string();
    }
    if retry_after_delivery_error_ms(error).is_some() {
        return "failed:rate_limited".to_string();
    }
    if let Some(reqwest_error) = error.downcast_ref::<reqwest::Error>() {
        return failed_reqwest_external_action_outcome(reqwest_error);
    }
    let lower = error.to_string().to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        "failed:timeout".to_string()
    } else if lower.contains("status") || lower.contains("http ") {
        "failed:status".to_string()
    } else if lower.contains("decode") || lower.contains("json") || lower.contains("parse") {
        "failed:decode".to_string()
    } else if lower.contains("connect") || lower.contains("dns") || lower.contains("resolve") {
        "failed:connect".to_string()
    } else {
        "failed:internal".to_string()
    }
}

fn safe_terminal_delivery_error_detail(error: &anyhow::Error, fallback: &str) -> String {
    let Some(error) = error.downcast_ref::<TerminalDeliveryError>() else {
        return fallback.to_string();
    };
    let detail = redact_text(&error.detail)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if detail.is_empty() {
        fallback.to_string()
    } else {
        truncate_terminal_delivery_error_detail(&detail)
    }
}

fn truncate_terminal_delivery_error_detail(detail: &str) -> String {
    if detail.chars().count() <= MAX_TERMINAL_DELIVERY_ERROR_DETAIL_CHARS {
        return detail.to_string();
    }
    let mut truncated = detail
        .chars()
        .take(MAX_TERMINAL_DELIVERY_ERROR_DETAIL_CHARS)
        .collect::<String>();
    truncated.push_str("...");
    truncated
}

fn safe_reply_target(reply: &ReplyHandle) -> String {
    let digest = kheish_codec::digest_text(&reply.address);
    format!(
        "{}:address_sha256:{}",
        reply.plugin,
        digest.get(..16).unwrap_or(digest.as_str())
    )
}

struct FileDeliveryStore {
    legacy_path: PathBuf,
    root_dir: PathBuf,
}

impl FileDeliveryStore {
    fn new(root: impl Into<PathBuf>) -> Self {
        let legacy_path = root.into();
        let root_dir = delivery_store_root_dir(&legacy_path);
        Self {
            legacy_path,
            root_dir,
        }
    }

    fn meta_path(&self) -> PathBuf {
        self.root_dir.join("meta.json")
    }

    fn target_backpressure_path(&self) -> PathBuf {
        self.root_dir.join("target-backpressure.json")
    }

    fn pending_dir(&self) -> PathBuf {
        self.root_dir.join("pending")
    }

    fn resolved_sources_dir(&self) -> PathBuf {
        self.root_dir.join("resolved-sources")
    }

    fn pending_path(&self, delivery_id: &str) -> PathBuf {
        self.pending_dir().join(format!("{delivery_id}.json"))
    }

    fn settled_path(&self, delivery_id: &str) -> PathBuf {
        self.pending_dir()
            .join(format!(".settled-{delivery_id}.json"))
    }

    fn dead_letter_path(&self) -> PathBuf {
        self.root_dir.join("dead-letter.jsonl")
    }

    fn resolved_dead_letter_path(&self) -> PathBuf {
        self.root_dir.join("resolved-dead-letter.jsonl")
    }

    fn completed_path(&self) -> PathBuf {
        self.root_dir.join("completed.jsonl")
    }

    fn status_summary_path(&self) -> PathBuf {
        self.root_dir.join("status-summary.json")
    }

    fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(self.pending_dir())?;
        Ok(())
    }

    fn ensure_resolved_sources_dir(&self) -> Result<()> {
        fs::create_dir_all(self.resolved_sources_dir())?;
        Ok(())
    }

    fn load(&self) -> Result<DeliveryQueueSnapshot> {
        self.migrate_legacy_snapshot()?;
        let meta =
            read_json_or_quarantine::<DeliveryQueueMeta>(&self.meta_path(), "delivery metadata")?
                .unwrap_or_default();
        let pending = self.load_pending_records()?;
        let next_id = pending
            .values()
            .map(delivery_sequence)
            .max()
            .unwrap_or_default()
            .max(self.load_terminal_delivery_max_sequence()?)
            .max(meta.next_id);
        self.rebuild_status_counts()?;
        Ok(DeliveryQueueSnapshot { next_id, pending })
    }

    fn save_meta(&self, next_id: u64) -> Result<()> {
        self.ensure_dirs()?;
        write_json_pretty_atomically(&self.meta_path(), &DeliveryQueueMeta { next_id })
    }

    fn save_target_backpressure(&self, snapshot: &TargetBackpressureSnapshot) -> Result<()> {
        self.ensure_dirs()?;
        write_json_pretty_atomically(&self.target_backpressure_path(), snapshot)
    }

    fn save_pending(&self, record: &PendingDeliveryRecord) -> Result<()> {
        self.ensure_dirs()?;
        write_json_pretty_atomically(&self.pending_path(&record.id), record)
    }

    fn remove_pending(&self, delivery_id: &str) -> Result<()> {
        match fs::remove_file(self.pending_path(delivery_id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn mark_pending_settled(&self, delivery_id: &str) -> Result<bool> {
        match fs::rename(
            self.pending_path(delivery_id),
            self.settled_path(delivery_id),
        ) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn restore_settled(&self, delivery_id: &str) -> Result<()> {
        match fs::rename(
            self.settled_path(delivery_id),
            self.pending_path(delivery_id),
        ) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn cleanup_settled(&self, delivery_id: &str) -> Result<()> {
        match fs::remove_file(self.settled_path(delivery_id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn append_dead_letter(
        &self,
        record: &PendingDeliveryRecord,
        terminal_error: &str,
    ) -> Result<()> {
        self.ensure_dirs()?;
        let base_offsets = self.terminal_ledger_offsets()?;
        append_json_line_sync(
            &self.dead_letter_path(),
            &DeadLetterDeliveryRecord {
                dropped_at_ms: now_ms(),
                terminal_error: terminal_error.to_string(),
                record: record.clone(),
            },
        )?;
        if let Err(error) = self.update_status_counts_from_base(base_offsets, |summary| {
            summary.dead_lettered += 1;
            summary.unresolved_dead_lettered += 1;
        }) {
            warn!(
                error = ?error,
                "failed to update delivery status summary after dead-letter append"
            );
        }
        Ok(())
    }

    fn append_resolved_dead_letter(&self, delivery_id: &str, reason: &str) -> Result<()> {
        self.ensure_dirs()?;
        let base_offsets = self.terminal_ledger_offsets()?;
        let should_clear_unresolved = !self.dead_letter_source_is_marked_resolved(delivery_id);
        append_json_line_sync(
            &self.resolved_dead_letter_path(),
            &ResolvedDeadLetterDeliveryRecord {
                delivery_id: delivery_id.to_string(),
                resolved_at_ms: now_ms(),
                reason: normalize_dead_letter_resolution_reason(reason),
            },
        )?;
        let marker_result = should_clear_unresolved
            .then(|| self.mark_dead_letter_source_resolved(delivery_id))
            .transpose();
        let marker_error = marker_result.as_ref().err().map(ToString::to_string);
        if let Err(error) = &marker_result {
            warn!(
                error = ?error,
                delivery_id,
                "failed to update resolved delivery source marker after dead-letter resolution"
            );
        }
        let did_clear_unresolved = marker_result
            .as_ref()
            .ok()
            .and_then(|value| *value)
            .unwrap_or(false);
        if let Err(error) = self.update_status_counts_from_base(base_offsets, |summary| {
            if did_clear_unresolved {
                summary.unresolved_dead_lettered =
                    summary.unresolved_dead_lettered.saturating_sub(1);
            }
            if let Some(error) = marker_error {
                record_delivery_summary_error(
                    summary,
                    format!("resolved source marker update failed: {error}"),
                );
            }
        }) {
            warn!(
                error = ?error,
                "failed to update delivery status summary after dead-letter resolution"
            );
        }
        Ok(())
    }

    fn append_completed(&self, record: &PendingDeliveryRecord) -> Result<()> {
        self.ensure_dirs()?;
        let base_offsets = self.terminal_ledger_offsets()?;
        let replayed_from = metadata_string(&record.metadata, "replayed_from_delivery_id");
        let should_clear_unresolved = match replayed_from.as_deref() {
            Some(source_id) => !self.dead_letter_source_is_marked_resolved(source_id),
            None => false,
        };
        append_json_line_sync(
            &self.completed_path(),
            &CompletedDeliveryRecord {
                delivered_at_ms: now_ms(),
                record: record.clone(),
            },
        )?;
        let marker_result = replayed_from
            .as_deref()
            .filter(|_| should_clear_unresolved)
            .map(|source_id| self.mark_dead_letter_source_resolved(source_id))
            .transpose();
        let marker_error = marker_result.as_ref().err().map(ToString::to_string);
        if let Err(error) = &marker_result {
            warn!(
                error = ?error,
                delivery_id = %record.id,
                "failed to update resolved delivery source marker after completed replay"
            );
        }
        let did_clear_unresolved = marker_result
            .as_ref()
            .ok()
            .and_then(|value| *value)
            .unwrap_or(false);
        if let Err(error) = self.update_status_counts_from_base(base_offsets, |summary| {
            summary.delivered += 1;
            if did_clear_unresolved {
                summary.unresolved_dead_lettered =
                    summary.unresolved_dead_lettered.saturating_sub(1);
            }
            if let Some(error) = marker_error {
                record_delivery_summary_error(
                    summary,
                    format!("resolved source marker update failed: {error}"),
                );
            }
        }) {
            warn!(
                error = ?error,
                "failed to update delivery status summary after completed append"
            );
        }
        Ok(())
    }

    fn load_pending_records(&self) -> Result<BTreeMap<String, PendingDeliveryRecord>> {
        let terminal_ids = self.load_terminal_delivery_ids()?;
        let pending_dir = self.pending_dir();
        if !pending_dir.exists() {
            return Ok(BTreeMap::new());
        }
        let mut pending = BTreeMap::new();
        for entry in fs::read_dir(&pending_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_file() {
                continue;
            }
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".settled-"))
            {
                let name = entry.file_name();
                let delivery_id = name
                    .to_str()
                    .and_then(|value| value.strip_prefix(".settled-"))
                    .and_then(|value| value.strip_suffix(".json"));
                if delivery_id.is_some_and(|delivery_id| terminal_ids.contains(delivery_id)) {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                let Some(record) =
                    read_json_or_quarantine::<PendingDeliveryRecord>(&path, "settled delivery")?
                else {
                    continue;
                };
                self.restore_settled(&record.id)?;
                pending.insert(record.id.clone(), record);
                continue;
            }
            let Some(record) =
                read_json_or_quarantine::<PendingDeliveryRecord>(&path, "pending delivery")?
            else {
                continue;
            };
            pending.insert(record.id.clone(), record);
        }
        Ok(pending)
    }

    fn load_target_backpressure(&self) -> Result<TargetBackpressureSnapshot> {
        Ok(read_json_or_quarantine::<TargetBackpressureSnapshot>(
            &self.target_backpressure_path(),
            "delivery target backpressure",
        )?
        .unwrap_or_default())
    }

    fn load_dead_letters(&self) -> Result<Vec<DeadLetterDeliveryRecord>> {
        self.load_json_lines(&self.dead_letter_path(), "dead-letter delivery")
    }

    fn load_resolved_dead_letters(&self) -> Result<Vec<ResolvedDeadLetterDeliveryRecord>> {
        self.load_json_lines(
            &self.resolved_dead_letter_path(),
            "resolved dead-letter delivery",
        )
    }

    fn load_completed(&self) -> Result<Vec<CompletedDeliveryRecord>> {
        self.load_json_lines(&self.completed_path(), "completed delivery")
    }

    fn status_counts(&self) -> Result<DeliveryLedgerStatusCounts> {
        let mut summary = self.load_status_counts_raw()?;
        if self.status_summary_is_stale()? {
            summary.status_error_count = summary.status_error_count.saturating_add(1);
            if summary.status_error.is_none() {
                summary.status_error =
                    Some("delivery status summary is older than terminal ledgers".to_string());
            }
        }
        Ok(summary)
    }

    fn load_status_counts_raw(&self) -> Result<DeliveryLedgerStatusCounts> {
        read_json_or_quarantine::<DeliveryLedgerStatusCounts>(
            &self.status_summary_path(),
            "delivery status summary",
        )?
        .ok_or_else(|| anyhow!("delivery status summary is missing"))
    }

    fn save_status_counts(&self, summary: &DeliveryLedgerStatusCounts) -> Result<()> {
        self.ensure_dirs()?;
        write_json_pretty_atomically(&self.status_summary_path(), summary)
    }

    fn status_summary_is_stale(&self) -> Result<bool> {
        let summary = self.load_status_counts_raw()?;
        let offsets = self.terminal_ledger_offsets()?;
        Ok(
            summary.completed_ledger_offset_bytes != offsets.completed_ledger_offset_bytes
                || summary.dead_letter_ledger_offset_bytes
                    != offsets.dead_letter_ledger_offset_bytes
                || summary.resolved_dead_letter_ledger_offset_bytes
                    != offsets.resolved_dead_letter_ledger_offset_bytes,
        )
    }

    fn update_status_counts_from_base<F>(
        &self,
        base_offsets: DeliveryLedgerStatusCounts,
        update: F,
    ) -> Result<()>
    where
        F: FnOnce(&mut DeliveryLedgerStatusCounts),
    {
        let mut summary = match self.load_status_counts_raw() {
            Ok(summary) if delivery_ledger_offsets_match(&summary, &base_offsets) => summary,
            Ok(_) | Err(_) => {
                self.rebuild_status_counts()?;
                return Ok(());
            }
        };
        update(&mut summary);
        apply_delivery_ledger_offsets(&mut summary, self.terminal_ledger_offsets()?);
        self.save_status_counts(&summary)
    }

    fn rebuild_status_counts(&self) -> Result<DeliveryLedgerStatusCounts> {
        let mut summary = DeliveryLedgerStatusCounts::default();
        let mut resolved = BTreeSet::new();
        let completed_scan = self.for_each_json_line::<CompletedDeliveryRecord, _>(
            &self.completed_path(),
            "completed delivery",
            |record| {
                summary.delivered += 1;
                if let Some(replayed_from) =
                    metadata_string(&record.record.metadata, "replayed_from_delivery_id")
                {
                    resolved.insert(replayed_from);
                }
            },
        )?;
        merge_ledger_scan_status(&mut summary, completed_scan);
        let resolved_scan = self.for_each_json_line::<ResolvedDeadLetterDeliveryRecord, _>(
            &self.resolved_dead_letter_path(),
            "resolved dead-letter delivery",
            |record| {
                resolved.insert(record.delivery_id);
            },
        )?;
        merge_ledger_scan_status(&mut summary, resolved_scan);

        let dead_letter_scan = self.for_each_json_line::<DeadLetterDeliveryRecord, _>(
            &self.dead_letter_path(),
            "dead-letter delivery",
            |record| {
                summary.dead_lettered += 1;
                if !resolved.contains(&record.record.id) {
                    summary.unresolved_dead_lettered += 1;
                }
            },
        )?;
        merge_ledger_scan_status(&mut summary, dead_letter_scan);

        apply_delivery_ledger_offsets(&mut summary, self.terminal_ledger_offsets()?);
        self.rebuild_resolved_source_markers(&resolved)?;
        self.save_status_counts(&summary)?;
        Ok(summary)
    }

    fn terminal_ledger_offsets(&self) -> Result<DeliveryLedgerStatusCounts> {
        Ok(DeliveryLedgerStatusCounts {
            completed_ledger_offset_bytes: file_len_or_zero(&self.completed_path())?,
            dead_letter_ledger_offset_bytes: file_len_or_zero(&self.dead_letter_path())?,
            resolved_dead_letter_ledger_offset_bytes: file_len_or_zero(
                &self.resolved_dead_letter_path(),
            )?,
            ..Default::default()
        })
    }

    fn resolved_source_path(&self, delivery_id: &str) -> PathBuf {
        self.resolved_sources_dir()
            .join(format!("{}.json", kheish_codec::digest_text(delivery_id)))
    }

    fn dead_letter_source_is_marked_resolved(&self, delivery_id: &str) -> bool {
        self.resolved_source_path(delivery_id).exists()
    }

    fn mark_dead_letter_source_resolved(&self, delivery_id: &str) -> Result<bool> {
        let path = self.resolved_source_path(delivery_id);
        if path.exists() {
            return Ok(false);
        }
        self.ensure_resolved_sources_dir()?;
        write_json_pretty_atomically(
            &path,
            &ResolvedDeadLetterSourceMarker {
                delivery_id: delivery_id.to_string(),
                resolved_at_ms: now_ms(),
            },
        )?;
        Ok(true)
    }

    fn rebuild_resolved_source_markers(&self, resolved: &BTreeSet<String>) -> Result<()> {
        match fs::remove_dir_all(self.resolved_sources_dir()) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.ensure_resolved_sources_dir()?;
        for delivery_id in resolved {
            write_json_pretty_atomically(
                &self.resolved_source_path(delivery_id),
                &ResolvedDeadLetterSourceMarker {
                    delivery_id: delivery_id.clone(),
                    resolved_at_ms: now_ms(),
                },
            )?;
        }
        Ok(())
    }

    fn load_json_lines<T>(&self, path: &Path, label: &str) -> Result<Vec<T>>
    where
        T: DeserializeOwned,
    {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut records = Vec::new();
        for (index, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<T>(line) {
                Ok(record) => records.push(record),
                Err(error) => {
                    warn!(
                        path = %path.display(),
                        label,
                        line = index + 1,
                        error = ?error,
                        "skipping corrupt delivery ledger line"
                    );
                }
            }
        }
        Ok(records)
    }

    fn for_each_json_line<T, F>(
        &self,
        path: &Path,
        label: &str,
        mut visit: F,
    ) -> Result<DeliveryLedgerScanStatus>
    where
        T: DeserializeOwned,
        F: FnMut(T),
    {
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Default::default()),
            Err(error) => return Err(error.into()),
        };
        let reader = BufReader::new(file);
        let mut status = DeliveryLedgerScanStatus::default();
        for (index, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<T>(&line) {
                Ok(record) => {
                    visit(record);
                }
                Err(error) => {
                    status.parse_error_count += 1;
                    if status.first_error.is_none() {
                        status.first_error = Some(format!(
                            "{label} ledger line {} could not be parsed: {error}",
                            index + 1
                        ));
                    }
                    warn!(
                        path = %path.display(),
                        label,
                        line = index + 1,
                        error = ?error,
                        "skipping corrupt delivery ledger line"
                    );
                }
            }
        }
        Ok(status)
    }

    fn load_terminal_delivery_ids(&self) -> Result<BTreeSet<String>> {
        let mut ids = BTreeSet::new();
        for record in self.load_dead_letters()? {
            ids.insert(record.record.id);
        }
        for record in self.load_completed()? {
            ids.insert(record.record.id);
        }
        Ok(ids)
    }

    fn load_terminal_delivery_max_sequence(&self) -> Result<u64> {
        let mut max_sequence = 0;
        for record in self.load_dead_letters()? {
            max_sequence = max_sequence.max(delivery_sequence(&record.record));
        }
        for record in self.load_completed()? {
            max_sequence = max_sequence.max(delivery_sequence(&record.record));
        }
        Ok(max_sequence)
    }

    fn load_pending_record(&self, delivery_id: &str) -> Result<Option<PendingDeliveryRecord>> {
        let path = self.pending_path(delivery_id);
        if !path.exists() {
            return Ok(None);
        }
        read_json_or_quarantine(&path, "pending delivery")
    }

    fn migrate_legacy_snapshot(&self) -> Result<()> {
        if self.root_dir.exists() || !self.legacy_path.exists() {
            return Ok(());
        }
        let Some(snapshot) = read_json_or_quarantine::<DeliveryQueueSnapshot>(
            &self.legacy_path,
            "legacy delivery queue",
        )?
        else {
            return Ok(());
        };
        self.ensure_dirs()?;
        self.save_meta(snapshot.next_id)?;
        for record in snapshot.pending.values() {
            self.save_pending(record)?;
        }
        match fs::remove_file(&self.legacy_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn merge_ledger_scan_status(
    summary: &mut DeliveryLedgerStatusCounts,
    scan: DeliveryLedgerScanStatus,
) {
    if scan.parse_error_count > 0 {
        summary.status_error_count = summary
            .status_error_count
            .saturating_add(scan.parse_error_count);
        if summary.status_error.is_none() {
            summary.status_error = scan.first_error;
        }
    }
}

fn record_delivery_summary_error(
    summary: &mut DeliveryLedgerStatusCounts,
    error: impl Into<String>,
) {
    summary.status_error_count = summary.status_error_count.saturating_add(1);
    if summary.status_error.is_none() {
        summary.status_error = Some(error.into());
    }
}

fn apply_delivery_ledger_offsets(
    summary: &mut DeliveryLedgerStatusCounts,
    offsets: DeliveryLedgerStatusCounts,
) {
    summary.completed_ledger_offset_bytes = offsets.completed_ledger_offset_bytes;
    summary.dead_letter_ledger_offset_bytes = offsets.dead_letter_ledger_offset_bytes;
    summary.resolved_dead_letter_ledger_offset_bytes =
        offsets.resolved_dead_letter_ledger_offset_bytes;
}

fn delivery_ledger_offsets_match(
    summary: &DeliveryLedgerStatusCounts,
    offsets: &DeliveryLedgerStatusCounts,
) -> bool {
    summary.completed_ledger_offset_bytes == offsets.completed_ledger_offset_bytes
        && summary.dead_letter_ledger_offset_bytes == offsets.dead_letter_ledger_offset_bytes
        && summary.resolved_dead_letter_ledger_offset_bytes
            == offsets.resolved_dead_letter_ledger_offset_bytes
}

fn file_len_or_zero(path: &Path) -> Result<u64> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn load_pending_delivery_record(
    state_root: &Path,
    delivery_id: &str,
) -> Result<Option<PendingDeliveryRecord>> {
    FileDeliveryStore::new(state_root.join("daemon-deliveries.json"))
        .load_pending_record(delivery_id)
}

fn delivery_store_root_dir(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("deliveries");
    let stem = file_name.strip_suffix(".json").unwrap_or(file_name);
    path.with_file_name(format!("{stem}.d"))
}

#[async_trait]
pub(crate) trait DeliveryTransport: Send + Sync {
    async fn deliver(&self, response: ResponseEnvelope) -> Result<()>;
}

pub(crate) struct DeliveryDispatcher {
    transports: BTreeMap<String, Arc<dyn DeliveryTransport>>,
}

impl DeliveryDispatcher {
    pub(crate) fn new() -> Self {
        Self {
            transports: BTreeMap::new(),
        }
    }

    pub(crate) fn register<T>(&mut self, name: impl Into<String>, transport: T)
    where
        T: DeliveryTransport + 'static,
    {
        self.transports.insert(name.into(), Arc::new(transport));
    }

    async fn deliver(&self, envelope: ResponseEnvelope) -> Result<()> {
        let reply = envelope
            .reply
            .as_ref()
            .ok_or_else(|| anyhow!("delivery dispatcher requires a reply target"))?;
        let transport = self.transports.get(&reply.plugin).ok_or_else(|| {
            terminal_delivery_error(format!("unknown delivery transport {}", reply.plugin))
        })?;
        transport.deliver(envelope).await
    }
}

pub(crate) struct DeliveryQueue {
    store: FileDeliveryStore,
    snapshot: Mutex<DeliveryQueueSnapshot>,
    target_backpressure: Mutex<TargetBackpressureSnapshot>,
    notify: Notify,
    dispatcher: Arc<DeliveryDispatcher>,
    retry_policy: DeliveryRetryPolicy,
    target_backpressure_policy: DeliveryTargetBackpressurePolicy,
    worker_started_at_ms: AtomicU64,
    worker_heartbeat_at_ms: AtomicU64,
}

impl DeliveryQueue {
    pub(crate) fn load(
        root: impl Into<PathBuf>,
        dispatcher: Arc<DeliveryDispatcher>,
    ) -> Result<Self> {
        Self::load_with_policies(
            root,
            dispatcher,
            DeliveryRetryPolicy::load_from_env()?,
            DeliveryTargetBackpressurePolicy::load_from_env()?,
        )
    }

    fn load_with_policies(
        root: impl Into<PathBuf>,
        dispatcher: Arc<DeliveryDispatcher>,
        retry_policy: DeliveryRetryPolicy,
        target_backpressure_policy: DeliveryTargetBackpressurePolicy,
    ) -> Result<Self> {
        let store = FileDeliveryStore::new(root.into());
        let snapshot = store.load()?;
        let target_backpressure = store.load_target_backpressure()?;
        Ok(Self {
            store,
            snapshot: Mutex::new(snapshot),
            target_backpressure: Mutex::new(target_backpressure),
            notify: Notify::new(),
            dispatcher,
            retry_policy,
            target_backpressure_policy,
            worker_started_at_ms: AtomicU64::new(0),
            worker_heartbeat_at_ms: AtomicU64::new(0),
        })
    }

    pub(crate) async fn enqueue(&self, response: ResponseEnvelope) -> Result<()> {
        let reply = response
            .reply
            .clone()
            .ok_or_else(|| anyhow!("queued delivery requires one reply target"))?;
        let run_id = response
            .metadata
            .get("run_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let mut snapshot = self.snapshot.lock().await;
        if let Some(idempotency_key) =
            metadata_string(&response.metadata, DELIVERY_IDEMPOTENCY_METADATA_KEY)
        {
            if snapshot
                .pending
                .values()
                .any(|record| delivery_record_matches_idempotency(record, &reply, &idempotency_key))
                || self.store.load_completed()?.into_iter().any(|record| {
                    delivery_record_matches_idempotency(&record.record, &reply, &idempotency_key)
                })
                || self.store.load_dead_letters()?.into_iter().any(|record| {
                    delivery_record_matches_idempotency(&record.record, &reply, &idempotency_key)
                })
            {
                return Ok(());
            }
        }
        let next_id = snapshot.next_id + 1;
        let record = PendingDeliveryRecord {
            id: format!("delivery-{next_id}"),
            conversation: response.conversation,
            run_id,
            reply,
            content: response.content,
            parts: response.parts,
            artifacts: response.artifacts,
            metadata: response.metadata,
            attempts: 0,
            next_attempt_at_ms: now_ms(),
            last_error: None,
        };
        self.store.save_pending(&record)?;
        if let Err(error) = self.store.save_meta(next_id) {
            let _ = self.store.remove_pending(&record.id);
            return Err(error);
        }
        snapshot.next_id = next_id;
        snapshot.pending.insert(record.id.clone(), record);
        drop(snapshot);
        self.notify.notify_one();
        Ok(())
    }

    pub(crate) async fn list_views(&self, filter: DeliveryListFilter) -> Result<Vec<DeliveryView>> {
        let mut views = {
            let snapshot = self.snapshot.lock().await;
            snapshot
                .pending
                .values()
                .map(DeliveryView::from_pending)
                .collect::<Vec<_>>()
        };
        views.extend(
            self.store
                .load_completed()?
                .into_iter()
                .map(|record| DeliveryView::from_completed(&record)),
        );
        let resolved_dead_letters =
            resolved_dead_letter_by_delivery_id(&self.store.load_resolved_dead_letters()?);
        views.extend(self.store.load_dead_letters()?.into_iter().map(|record| {
            DeliveryView::from_dead_letter(&record, resolved_dead_letters.get(&record.record.id))
        }));
        views.retain(|view| delivery_view_matches(view, &filter));
        views.sort_by(|left, right| {
            delivery_sequence_from_id(&left.delivery_id)
                .cmp(&delivery_sequence_from_id(&right.delivery_id))
                .then_with(|| {
                    delivery_status_rank(left.status).cmp(&delivery_status_rank(right.status))
                })
        });
        Ok(views)
    }

    pub(crate) async fn get_view(&self, delivery_id: &str) -> Result<Option<DeliveryView>> {
        let mut views = self.list_views(DeliveryListFilter::default()).await?;
        views.retain(|view| view.delivery_id == delivery_id);
        views.sort_by_key(|view| delivery_status_rank(view.status));
        Ok(views.into_iter().next())
    }

    pub(crate) async fn reference_records(
        &self,
    ) -> Result<Vec<(DeliveryStatus, PendingDeliveryRecord)>> {
        let mut records = {
            let snapshot = self.snapshot.lock().await;
            snapshot
                .pending
                .values()
                .map(|record| {
                    let status = if record.attempts == 0 {
                        DeliveryStatus::Pending
                    } else {
                        DeliveryStatus::Retrying
                    };
                    (status, record.clone())
                })
                .collect::<Vec<_>>()
        };
        records.extend(
            self.store
                .load_completed()?
                .into_iter()
                .map(|record| (DeliveryStatus::Delivered, record.record)),
        );
        records.extend(
            self.store
                .load_dead_letters()?
                .into_iter()
                .map(|record| (DeliveryStatus::DeadLettered, record.record)),
        );
        records.sort_by(|left, right| {
            delivery_sequence_from_id(&left.1.id)
                .cmp(&delivery_sequence_from_id(&right.1.id))
                .then_with(|| delivery_status_rank(left.0).cmp(&delivery_status_rank(right.0)))
        });
        Ok(records)
    }

    pub(crate) async fn replay_dead_letter(
        &self,
        delivery_id: &str,
        force: bool,
    ) -> Result<Option<DeliveryReplayResponse>> {
        let mut snapshot = self.snapshot.lock().await;
        let dead_letters = self.store.load_dead_letters()?;
        let completed = if force {
            Vec::new()
        } else {
            self.store.load_completed()?
        };
        let Some(source) = dead_letters
            .iter()
            .rev()
            .find(|record| record.record.id == delivery_id)
            .cloned()
        else {
            return Ok(None);
        };
        if !force
            && let Some(existing) =
                existing_replay_response(delivery_id, &snapshot, &completed, &dead_letters)
        {
            return Ok(Some(existing));
        }
        let next_id = snapshot.next_id + 1;
        let replayed = PendingDeliveryRecord {
            id: format!("delivery-{next_id}"),
            attempts: 0,
            next_attempt_at_ms: now_ms(),
            last_error: None,
            metadata: metadata_with_replay_context(
                &source.record.metadata,
                &source.record.id,
                source.dropped_at_ms,
            ),
            ..source.record.clone()
        };
        self.store.save_pending(&replayed)?;
        if let Err(error) = self.store.save_meta(next_id) {
            let _ = self.store.remove_pending(&replayed.id);
            return Err(error);
        }
        snapshot.next_id = next_id;
        snapshot
            .pending
            .insert(replayed.id.clone(), replayed.clone());
        drop(snapshot);
        self.notify.notify_one();
        Ok(Some(DeliveryReplayResponse {
            replayed_from_delivery_id: delivery_id.to_string(),
            delivery: DeliveryView::from_pending(&replayed),
        }))
    }

    pub(crate) async fn resolve_dead_letter(
        &self,
        delivery_id: &str,
        reason: &str,
    ) -> Result<Option<DeliveryView>> {
        let _snapshot = self.snapshot.lock().await;
        let dead_letters = self.store.load_dead_letters()?;
        let Some(source) = dead_letters
            .iter()
            .rev()
            .find(|record| record.record.id == delivery_id)
            .cloned()
        else {
            return Ok(None);
        };
        let mut resolved_dead_letters = self.store.load_resolved_dead_letters()?;
        if !resolved_dead_letters
            .iter()
            .any(|record| record.delivery_id == delivery_id)
        {
            self.store
                .append_resolved_dead_letter(delivery_id, reason)?;
            resolved_dead_letters = self.store.load_resolved_dead_letters()?;
        }
        let resolved_by_id = resolved_dead_letter_by_delivery_id(&resolved_dead_letters);
        Ok(Some(DeliveryView::from_dead_letter(
            &source,
            resolved_by_id.get(delivery_id),
        )))
    }

    pub(crate) async fn bulk_replay_dead_letters(
        &self,
        filter: DeliveryListFilter,
        force: bool,
        dry_run: bool,
        unresolved_only: bool,
        limit: Option<usize>,
    ) -> Result<DeliveryBulkReplayResponse> {
        let limit = limit
            .unwrap_or(DEFAULT_BULK_REPLAY_LIMIT)
            .clamp(1, MAX_BULK_REPLAY_LIMIT);
        let mut snapshot = self.snapshot.lock().await;
        let dead_letters = self.store.load_dead_letters()?;
        let completed = self.store.load_completed()?;
        let resolved_dead_letters = self.store.load_resolved_dead_letters()?;
        let resolved_by_id = resolved_dead_letter_by_delivery_id(&resolved_dead_letters);
        let resolved_ids = resolved_dead_letter_ids(&completed, &resolved_dead_letters);
        let mut candidates = dead_letters
            .iter()
            .filter_map(|record| {
                if unresolved_only && resolved_ids.contains(&record.record.id) {
                    return None;
                }
                let view =
                    DeliveryView::from_dead_letter(record, resolved_by_id.get(&record.record.id));
                delivery_view_matches(&view, &filter).then_some((record.clone(), view))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            delivery_sequence_from_id(&left.0.record.id)
                .cmp(&delivery_sequence_from_id(&right.0.record.id))
        });
        let matched = candidates.len();
        candidates.truncate(limit);

        let mut items = Vec::new();
        for (source, source_view) in candidates {
            if dry_run {
                items.push(DeliveryBulkReplayItem {
                    replayed_from_delivery_id: source.record.id.clone(),
                    source: source_view,
                    replay: None,
                    action: DeliveryBulkReplayAction::WouldReplay,
                });
                continue;
            }
            if !force
                && let Some(existing) = existing_replay_response(
                    &source.record.id,
                    &snapshot,
                    &completed,
                    &dead_letters,
                )
            {
                items.push(DeliveryBulkReplayItem {
                    replayed_from_delivery_id: source.record.id.clone(),
                    source: source_view,
                    replay: Some(existing.delivery),
                    action: DeliveryBulkReplayAction::ExistingReplay,
                });
                continue;
            }

            let next_id = snapshot.next_id + 1;
            let replayed = PendingDeliveryRecord {
                id: format!("delivery-{next_id}"),
                attempts: 0,
                next_attempt_at_ms: now_ms(),
                last_error: None,
                metadata: metadata_with_replay_context(
                    &source.record.metadata,
                    &source.record.id,
                    source.dropped_at_ms,
                ),
                ..source.record.clone()
            };
            self.store.save_pending(&replayed)?;
            if let Err(error) = self.store.save_meta(next_id) {
                let _ = self.store.remove_pending(&replayed.id);
                return Err(error);
            }
            snapshot.next_id = next_id;
            snapshot
                .pending
                .insert(replayed.id.clone(), replayed.clone());
            items.push(DeliveryBulkReplayItem {
                replayed_from_delivery_id: source.record.id.clone(),
                source: source_view,
                replay: Some(DeliveryView::from_pending(&replayed)),
                action: DeliveryBulkReplayAction::Replayed,
            });
        }
        let replayed = items
            .iter()
            .filter(|item| item.action == DeliveryBulkReplayAction::Replayed)
            .count();
        let existing_replays = items
            .iter()
            .filter(|item| item.action == DeliveryBulkReplayAction::ExistingReplay)
            .count();
        if replayed > 0 {
            drop(snapshot);
            self.notify.notify_one();
        }
        Ok(DeliveryBulkReplayResponse {
            dry_run,
            force,
            unresolved_only,
            limit,
            matched,
            selected: items.len(),
            replayed,
            existing_replays,
            truncated: matched.saturating_sub(items.len()),
            items,
        })
    }

    pub(crate) async fn reset_target_backpressure(
        &self,
        target: Option<&str>,
        plugin: Option<&str>,
        dry_run: bool,
    ) -> Result<DeliveryBackpressureResetResponse> {
        anyhow::ensure!(
            target.is_some() || plugin.is_some(),
            "target or plugin is required"
        );
        let plugin_prefix = plugin.map(|plugin| format!("{plugin}:address_sha256:"));
        let mut target_backpressure = self.target_backpressure.lock().await;
        let mut targets = target_backpressure
            .targets
            .keys()
            .filter(|candidate| {
                target.is_none_or(|target| candidate.as_str() == target)
                    && plugin_prefix
                        .as_deref()
                        .is_none_or(|prefix| candidate.starts_with(prefix))
            })
            .cloned()
            .collect::<Vec<_>>();
        targets.sort();
        let matched = targets.len();
        let removed = if dry_run {
            0
        } else {
            for target in &targets {
                target_backpressure.targets.remove(target);
            }
            if matched > 0 {
                self.store.save_target_backpressure(&target_backpressure)?;
            }
            matched
        };
        drop(target_backpressure);
        if removed > 0 {
            self.notify.notify_one();
        }
        Ok(DeliveryBackpressureResetResponse {
            dry_run,
            target: target.map(ToOwned::to_owned),
            plugin: plugin.map(ToOwned::to_owned),
            matched,
            removed,
            targets,
        })
    }

    pub(crate) async fn status_snapshot(&self, now: u64) -> Result<DeliveryQueueStatusView> {
        let snapshot = self.snapshot.lock().await;
        let pending = snapshot
            .pending
            .values()
            .filter(|record| record.attempts == 0)
            .count();
        let retrying = snapshot.pending.len().saturating_sub(pending);
        let ready = snapshot
            .pending
            .values()
            .filter(|record| record.next_attempt_at_ms <= now)
            .count();
        let target_heads = delivery_target_heads(&snapshot);
        let target_backpressure = self.target_backpressure.lock().await;
        let dispatchable = target_heads
            .values()
            .filter(|record| delivery_target_available_at(record, &target_backpressure) <= now)
            .count();
        let blocked_target_count = target_heads
            .iter()
            .filter(|(target, head)| {
                delivery_target_available_at(head, &target_backpressure) > now
                    && snapshot.pending.values().any(|record| {
                        reply_target_key(&record.reply) == **target
                            && record.id != head.id
                            && record.next_attempt_at_ms <= now
                    })
            })
            .count();
        let next_attempt_at_ms = target_heads
            .values()
            .map(|record| record.next_attempt_at_ms)
            .min();
        let next_target_available_at_ms = target_heads
            .values()
            .map(|record| delivery_target_available_at(record, &target_backpressure))
            .min();
        let throttled_target_count = target_heads
            .values()
            .filter(|record| {
                target_backpressure
                    .targets
                    .get(&safe_reply_target(&record.reply))
                    .is_some_and(|state| state.blocked_until_ms > now)
            })
            .count();
        let open_circuit_target_count = target_heads
            .values()
            .filter(|record| {
                target_backpressure
                    .targets
                    .get(&safe_reply_target(&record.reply))
                    .is_some_and(|state| {
                        state.circuit_opened_at_ms.is_some() && state.blocked_until_ms > now
                    })
            })
            .count();
        drop(target_backpressure);
        drop(snapshot);

        let ledger_counts =
            self.store
                .status_counts()
                .unwrap_or_else(|error| DeliveryLedgerStatusCounts {
                    status_error_count: 1,
                    status_error: Some(error.to_string()),
                    ..Default::default()
                });
        let worker_started_at_ms = non_zero_u64(self.worker_started_at_ms.load(Ordering::Relaxed));
        let worker_heartbeat_at_ms =
            non_zero_u64(self.worker_heartbeat_at_ms.load(Ordering::Relaxed));
        let worker_lag_ms = worker_heartbeat_at_ms.map(|heartbeat| now.saturating_sub(heartbeat));
        Ok(DeliveryQueueStatusView {
            pending,
            retrying,
            delivered: ledger_counts.delivered,
            dead_lettered: ledger_counts.dead_lettered,
            unresolved_dead_lettered: ledger_counts.unresolved_dead_lettered,
            ready,
            dispatchable,
            target_count: target_heads.len(),
            blocked_target_count,
            throttled_target_count,
            open_circuit_target_count,
            next_attempt_at_ms,
            next_target_available_at_ms,
            worker_started_at_ms,
            worker_heartbeat_at_ms,
            worker_lag_ms,
            status_error_count: ledger_counts.status_error_count,
            status_error: ledger_counts.status_error,
        })
    }

    pub(crate) fn spawn_worker(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(error) = self.run_worker().await {
                error!(error = ?error, "delivery worker exited");
            }
        })
    }

    async fn run_worker(self: &Arc<Self>) -> Result<()> {
        let started_at_ms = now_ms();
        self.worker_started_at_ms
            .store(started_at_ms, Ordering::Relaxed);
        self.worker_heartbeat_at_ms
            .store(started_at_ms, Ordering::Relaxed);
        loop {
            self.worker_heartbeat_at_ms
                .store(now_ms(), Ordering::Relaxed);
            let (record, wait_for) = {
                let snapshot = self.snapshot.lock().await;
                let target_backpressure = self.target_backpressure.lock().await;
                next_delivery_candidate(&snapshot, &target_backpressure)
            };

            if let Some(record) = record {
                if let Err(error) = self.process_one(record).await {
                    error!(error = ?error, "delivery worker iteration failed");
                    tokio::time::sleep(WORKER_ERROR_RETRY_DELAY).await;
                }
                continue;
            }

            match wait_for {
                Some(delay_ms) if delay_ms > 0 => {
                    tokio::select! {
                        _ = self.notify.notified() => {}
                        _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                    }
                }
                Some(_) => {}
                None => self.notify.notified().await,
            }
        }
    }

    async fn process_one(&self, record: PendingDeliveryRecord) -> Result<()> {
        let attempt = record.attempts.saturating_add(1);
        let envelope = ResponseEnvelope {
            conversation: record.conversation.clone(),
            reply_targets: Vec::new(),
            reply: Some(record.reply.clone()),
            content: record.content.clone(),
            parts: record.parts.clone(),
            artifacts: record.artifacts.clone(),
            metadata: metadata_with_delivery_context(&record.metadata, &record.id, attempt),
        };
        let delivery = self.dispatcher.deliver(envelope);
        let result = match delivery_execution_scope(&record) {
            Some(scope) => scope_execution(scope, CancellationToken::new(), delivery).await,
            None => delivery.await,
        };
        match result {
            Ok(()) => {
                let settled = PendingDeliveryRecord {
                    attempts: attempt,
                    next_attempt_at_ms: now_ms(),
                    last_error: None,
                    ..record.clone()
                };
                let mut snapshot = self.snapshot.lock().await;
                anyhow::ensure!(
                    self.store.mark_pending_settled(&record.id)?,
                    "missing pending delivery file for {}",
                    record.id
                );
                if let Err(error) = self.store.append_completed(&settled) {
                    let _ = self.store.restore_settled(&record.id);
                    return Err(error);
                }
                snapshot.pending.remove(&record.id);
                if let Err(error) = self.store.cleanup_settled(&record.id) {
                    tracing::warn!(
                        delivery_id = %record.id,
                        error = ?error,
                        "failed to clean up settled delivery tombstone"
                    );
                }
                self.record_target_success(&settled).await?;
            }
            Err(error) => {
                let mut snapshot = self.snapshot.lock().await;
                if let Some(current) = snapshot.pending.get(&record.id).cloned() {
                    let terminal_error = error.downcast_ref::<TerminalDeliveryError>().is_some();
                    let retry_after_ms = error
                        .downcast_ref::<RetryAfterDeliveryError>()
                        .map(|error| error.retry_after_ms);
                    let safe_error = safe_delivery_error(&error);
                    let retry_delay_ms =
                        effective_retry_delay_ms(&self.retry_policy, attempt, retry_after_ms);
                    let updated = PendingDeliveryRecord {
                        attempts: attempt,
                        last_error: Some(safe_error.clone()),
                        next_attempt_at_ms: now_ms().saturating_add(retry_delay_ms),
                        ..current
                    };
                    if !terminal_error {
                        self.record_target_retryable_failure(&updated, retry_delay_ms)
                            .await?;
                    }
                    if terminal_error || updated.attempts >= self.retry_policy.max_attempts {
                        anyhow::ensure!(
                            self.store.mark_pending_settled(&record.id)?,
                            "missing pending delivery file for {}",
                            record.id
                        );
                        let terminal_error_detail = if terminal_error {
                            safe_terminal_delivery_error_detail(&error, &safe_error)
                        } else {
                            updated
                                .last_error
                                .clone()
                                .unwrap_or_else(|| "failed:unknown".to_string())
                        };
                        if let Err(dead_letter_error) = self
                            .store
                            .append_dead_letter(&updated, &terminal_error_detail)
                        {
                            let _ = self.store.restore_settled(&record.id);
                            return Err(dead_letter_error);
                        }
                        snapshot.pending.remove(&record.id);
                        if let Err(cleanup_error) = self.store.cleanup_settled(&record.id) {
                            tracing::warn!(
                                delivery_id = %updated.id,
                                error = ?cleanup_error,
                                "failed to clean up dead-letter delivery tombstone"
                            );
                        }
                        error!(
                            delivery_id = %updated.id,
                            plugin = %updated.reply.plugin,
                            address = %safe_reply_target(&updated.reply),
                            attempts = updated.attempts,
                            last_error = %safe_error,
                            "dropping external delivery after exhausting retry budget"
                        );
                        return Ok(());
                    }
                    self.store.save_pending(&updated)?;
                    if let Some(entry) = snapshot.pending.get_mut(&record.id) {
                        *entry = updated;
                    }
                }
            }
        }
        Ok(())
    }

    async fn record_target_success(&self, record: &PendingDeliveryRecord) -> Result<()> {
        let target = safe_reply_target(&record.reply);
        let now = now_ms();
        let mut target_backpressure = self.target_backpressure.lock().await;
        let blocked_until_ms = now.saturating_add(self.target_backpressure_policy.min_interval_ms);
        target_backpressure.targets.insert(
            target.clone(),
            TargetBackpressureRecord {
                target,
                last_attempt_at_ms: now,
                consecutive_retryable_failures: 0,
                blocked_until_ms,
                circuit_opened_at_ms: None,
            },
        );
        self.store.save_target_backpressure(&target_backpressure)
    }

    async fn record_target_retryable_failure(
        &self,
        record: &PendingDeliveryRecord,
        retry_delay_ms: u64,
    ) -> Result<()> {
        let target = safe_reply_target(&record.reply);
        let now = now_ms();
        let mut target_backpressure = self.target_backpressure.lock().await;
        let mut state = target_backpressure
            .targets
            .get(&target)
            .cloned()
            .unwrap_or_else(|| TargetBackpressureRecord {
                target: target.clone(),
                ..Default::default()
            });
        state.target = target.clone();
        state.last_attempt_at_ms = now;
        state.consecutive_retryable_failures =
            state.consecutive_retryable_failures.saturating_add(1);
        state.blocked_until_ms = state
            .blocked_until_ms
            .max(now.saturating_add(retry_delay_ms));
        if self.target_backpressure_policy.circuit_failure_threshold > 0
            && state.consecutive_retryable_failures
                >= self.target_backpressure_policy.circuit_failure_threshold
        {
            state.circuit_opened_at_ms.get_or_insert(now);
            state.blocked_until_ms = state
                .blocked_until_ms
                .max(now.saturating_add(self.target_backpressure_policy.circuit_cooldown_ms));
        }
        target_backpressure.targets.insert(target, state);
        self.store.save_target_backpressure(&target_backpressure)
    }
}

fn metadata_with_delivery_context(metadata: &Value, delivery_id: &str, attempt: u32) -> Value {
    let mut object = match metadata.clone() {
        Value::Object(map) => map,
        Value::Null => Map::new(),
        other => {
            let mut map = Map::new();
            map.insert("source_metadata".to_string(), other);
            map
        }
    };
    object.insert(
        "delivery_id".to_string(),
        Value::String(delivery_id.to_string()),
    );
    object.insert("delivery_attempt".to_string(), Value::from(attempt));
    Value::Object(object)
}

fn metadata_with_replay_context(
    metadata: &Value,
    replayed_from_delivery_id: &str,
    dead_lettered_at_ms: u64,
) -> Value {
    let mut object = match metadata.clone() {
        Value::Object(map) => map,
        Value::Null => Map::new(),
        other => {
            let mut map = Map::new();
            map.insert("source_metadata".to_string(), other);
            map
        }
    };
    object.insert(
        "replayed_from_delivery_id".to_string(),
        Value::String(replayed_from_delivery_id.to_string()),
    );
    object.insert(
        "replayed_from_dead_lettered_at_ms".to_string(),
        Value::from(dead_lettered_at_ms),
    );
    Value::Object(object)
}

impl DeliveryView {
    fn from_pending(record: &PendingDeliveryRecord) -> Self {
        Self::from_record(
            record,
            if record.attempts == 0 {
                DeliveryStatus::Pending
            } else {
                DeliveryStatus::Retrying
            },
            Some(record.next_attempt_at_ms),
            None,
            None,
            None,
            None,
            record.last_error.clone(),
            None,
        )
    }

    fn from_completed(record: &CompletedDeliveryRecord) -> Self {
        Self::from_record(
            &record.record,
            DeliveryStatus::Delivered,
            None,
            Some(record.delivered_at_ms),
            None,
            None,
            None,
            None,
            None,
        )
    }

    fn from_dead_letter(
        record: &DeadLetterDeliveryRecord,
        resolution: Option<&ResolvedDeadLetterDeliveryRecord>,
    ) -> Self {
        Self::from_record(
            &record.record,
            DeliveryStatus::DeadLettered,
            None,
            None,
            Some(record.dropped_at_ms),
            resolution.map(|record| record.resolved_at_ms),
            resolution.map(|record| record.reason.clone()),
            record.record.last_error.clone(),
            Some(record.terminal_error.clone()),
        )
    }

    fn from_record(
        record: &PendingDeliveryRecord,
        status: DeliveryStatus,
        next_attempt_at_ms: Option<u64>,
        delivered_at_ms: Option<u64>,
        dead_lettered_at_ms: Option<u64>,
        dead_letter_resolved_at_ms: Option<u64>,
        dead_letter_resolution_reason: Option<String>,
        last_error: Option<String>,
        terminal_error: Option<String>,
    ) -> Self {
        Self {
            delivery_id: record.id.clone(),
            status,
            session_id: record.conversation.session_id.clone(),
            thread_id: record.conversation.thread_id.clone(),
            run_id: record.run_id.clone(),
            plugin: record.reply.plugin.clone(),
            target: safe_reply_target(&record.reply),
            attempts: record.attempts,
            next_attempt_at_ms,
            delivered_at_ms,
            dead_lettered_at_ms,
            dead_letter_resolved_at_ms,
            dead_letter_resolution_reason,
            last_error,
            terminal_error,
            replayed_from_delivery_id: metadata_string(
                &record.metadata,
                "replayed_from_delivery_id",
            ),
            content_size_bytes: record.content.len(),
            parts_count: record.parts.len(),
            artifacts_count: record.artifacts.len(),
        }
    }
}

fn delivery_view_matches(view: &DeliveryView, filter: &DeliveryListFilter) -> bool {
    if let Some(session_id) = filter.session_id.as_deref()
        && view.session_id != session_id
    {
        return false;
    }
    if let Some(run_id) = filter.run_id.as_deref()
        && view.run_id.as_deref() != Some(run_id)
    {
        return false;
    }
    if let Some(plugin) = filter.plugin.as_deref()
        && view.plugin != plugin
    {
        return false;
    }
    if let Some(status) = filter.status
        && view.status != status
    {
        return false;
    }
    true
}

fn existing_replay_response(
    replayed_from_delivery_id: &str,
    snapshot: &DeliveryQueueSnapshot,
    completed: &[CompletedDeliveryRecord],
    dead_letters: &[DeadLetterDeliveryRecord],
) -> Option<DeliveryReplayResponse> {
    let mut views = snapshot
        .pending
        .values()
        .filter(|record| {
            metadata_string(&record.metadata, "replayed_from_delivery_id").as_deref()
                == Some(replayed_from_delivery_id)
        })
        .map(DeliveryView::from_pending)
        .collect::<Vec<_>>();
    views.extend(completed.iter().filter_map(|record| {
        (metadata_string(&record.record.metadata, "replayed_from_delivery_id").as_deref()
            == Some(replayed_from_delivery_id))
        .then(|| DeliveryView::from_completed(record))
    }));
    views.extend(dead_letters.iter().filter_map(|record| {
        (metadata_string(&record.record.metadata, "replayed_from_delivery_id").as_deref()
            == Some(replayed_from_delivery_id))
        .then(|| DeliveryView::from_dead_letter(record, None))
    }));
    views
        .into_iter()
        .max_by(|left, right| {
            delivery_sequence_from_id(&left.delivery_id)
                .cmp(&delivery_sequence_from_id(&right.delivery_id))
                .then_with(|| {
                    delivery_status_rank(left.status).cmp(&delivery_status_rank(right.status))
                })
        })
        .map(|delivery| DeliveryReplayResponse {
            replayed_from_delivery_id: replayed_from_delivery_id.to_string(),
            delivery,
        })
}

#[cfg(test)]
fn unresolved_dead_letter_count(
    dead_letters: &[DeadLetterDeliveryRecord],
    completed: &[CompletedDeliveryRecord],
    resolved_dead_letters: &[ResolvedDeadLetterDeliveryRecord],
) -> usize {
    let resolved = resolved_dead_letter_ids(completed, resolved_dead_letters);
    dead_letters
        .iter()
        .filter(|record| !resolved.contains(&record.record.id))
        .count()
}

fn resolved_dead_letter_ids(
    completed: &[CompletedDeliveryRecord],
    resolved_dead_letters: &[ResolvedDeadLetterDeliveryRecord],
) -> BTreeSet<String> {
    completed
        .iter()
        .filter_map(|record| metadata_string(&record.record.metadata, "replayed_from_delivery_id"))
        .chain(
            resolved_dead_letters
                .iter()
                .map(|record| record.delivery_id.clone()),
        )
        .collect()
}

fn resolved_dead_letter_by_delivery_id(
    resolved_dead_letters: &[ResolvedDeadLetterDeliveryRecord],
) -> BTreeMap<String, ResolvedDeadLetterDeliveryRecord> {
    let mut by_id = BTreeMap::new();
    for record in resolved_dead_letters {
        by_id.insert(record.delivery_id.clone(), record.clone());
    }
    by_id
}

fn normalize_dead_letter_resolution_reason(reason: &str) -> String {
    let redacted = kheish_runtime::redact_text(reason.trim());
    if redacted.is_empty() {
        return "operator resolved".to_string();
    }
    redacted.chars().take(512).collect()
}

fn push_prometheus_gauge(body: &mut String, name: &str, help: &str, value: impl std::fmt::Display) {
    let _ = writeln!(body, "# HELP {name} {help}");
    let _ = writeln!(body, "# TYPE {name} gauge");
    let _ = writeln!(body, "{name} {value}");
}

fn delivery_status_rank(status: DeliveryStatus) -> u8 {
    match status {
        DeliveryStatus::Pending => 0,
        DeliveryStatus::Retrying => 1,
        DeliveryStatus::Delivered => 2,
        DeliveryStatus::DeadLettered => 3,
    }
}

fn delivery_execution_scope(record: &PendingDeliveryRecord) -> Option<ExecutionScope> {
    if record.conversation.session_id.trim().is_empty() {
        return None;
    }
    Some(ExecutionScope {
        session_id: record.conversation.session_id.clone(),
        agent_id: metadata_string(&record.metadata, "agent_id"),
        run_id: record
            .run_id
            .clone()
            .or_else(|| metadata_string(&record.metadata, "run_id")),
        principal_id: metadata_string(&record.metadata, "principal_id"),
        parent_principal_id: metadata_string(&record.metadata, "parent_principal_id"),
        delegation_id: metadata_string(&record.metadata, "delegation_id"),
        grant_id: metadata_string(&record.metadata, "grant_id"),
        ..ExecutionScope::default()
    })
}

fn metadata_string(metadata: &Value, key: &str) -> Option<String> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn delivery_record_matches_idempotency(
    record: &PendingDeliveryRecord,
    reply: &ReplyHandle,
    idempotency_key: &str,
) -> bool {
    record.reply == *reply
        && metadata_string(&record.metadata, DELIVERY_IDEMPOTENCY_METADATA_KEY).as_deref()
            == Some(idempotency_key)
}

fn next_delivery_candidate(
    snapshot: &DeliveryQueueSnapshot,
    target_backpressure: &TargetBackpressureSnapshot,
) -> (Option<PendingDeliveryRecord>, Option<u64>) {
    let now = now_ms();
    let target_heads = delivery_target_heads(snapshot);

    let mut ready: Vec<PendingDeliveryRecord> = target_heads
        .values()
        .filter(|record| delivery_target_available_at(record, target_backpressure) <= now)
        .cloned()
        .collect();
    if !ready.is_empty() {
        ready.sort_by_key(|record| {
            (
                delivery_target_available_at(record, target_backpressure),
                delivery_sequence(record),
            )
        });
        return (ready.into_iter().next(), None);
    }

    let wait_for = target_heads
        .values()
        .map(|record| delivery_target_available_at(record, target_backpressure).saturating_sub(now))
        .min();
    (None, wait_for)
}

fn delivery_target_available_at(
    record: &PendingDeliveryRecord,
    target_backpressure: &TargetBackpressureSnapshot,
) -> u64 {
    let target = safe_reply_target(&record.reply);
    target_backpressure
        .targets
        .get(&target)
        .map(|state| state.blocked_until_ms)
        .unwrap_or_default()
        .max(record.next_attempt_at_ms)
}

fn delivery_target_heads(
    snapshot: &DeliveryQueueSnapshot,
) -> BTreeMap<String, PendingDeliveryRecord> {
    let mut target_heads: BTreeMap<String, PendingDeliveryRecord> = BTreeMap::new();
    for record in snapshot.pending.values() {
        let target = reply_target_key(&record.reply);
        let should_replace = target_heads
            .get(&target)
            .map(|existing| delivery_sequence(record) < delivery_sequence(existing))
            .unwrap_or(true);
        if should_replace {
            target_heads.insert(target, record.clone());
        }
    }
    target_heads
}

fn non_zero_u64(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

fn retry_delay_ms(policy: &DeliveryRetryPolicy, attempts: u32) -> u64 {
    let exponent = attempts.saturating_sub(1).min(6);
    let delay = policy
        .initial_retry_delay_ms
        .saturating_mul(1u64 << exponent);
    delay.min(policy.max_retry_delay_ms)
}

fn effective_retry_delay_ms(
    policy: &DeliveryRetryPolicy,
    attempts: u32,
    retry_after_ms: Option<u64>,
) -> u64 {
    let retry_after_ms = retry_after_ms
        .map(|delay| delay.min(policy.max_retry_after_delay_ms))
        .unwrap_or_default();
    retry_after_ms.max(retry_delay_ms(policy, attempts))
}

fn reply_target_key(reply: &ReplyHandle) -> String {
    format!("{}\u{0}{}", reply.plugin, reply.address)
}

fn delivery_sequence(record: &PendingDeliveryRecord) -> u64 {
    delivery_sequence_from_id(&record.id)
}

fn delivery_sequence_from_id(delivery_id: &str) -> u64 {
    delivery_id
        .strip_prefix("delivery-")
        .and_then(|suffix| suffix.parse::<u64>().ok())
        .unwrap_or_default()
}

pub(crate) struct QueuedOutputPlugin {
    name: String,
    queue: Arc<DeliveryQueue>,
}

impl QueuedOutputPlugin {
    pub(crate) fn new(name: impl Into<String>, queue: Arc<DeliveryQueue>) -> Self {
        Self {
            name: name.into(),
            queue,
        }
    }
}

#[async_trait]
impl OutputPlugin for QueuedOutputPlugin {
    fn manifest(&self) -> OutputManifest {
        OutputManifest {
            name: self.name.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Queued external output delivery".to_string(),
        }
    }

    async fn deliver(&self, response: ResponseEnvelope) -> Result<()> {
        self.queue.enqueue(response).await
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write as _;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use kheish_session::write_json_pretty_atomically;
    use serde_json::Value;
    use tempfile::tempdir;

    use crate::now_ms;

    use super::{
        CompletedDeliveryRecord, DEFAULT_MAX_DELIVERY_ATTEMPTS, DeadLetterDeliveryRecord,
        DeliveryBulkReplayAction, DeliveryDispatcher, DeliveryListFilter, DeliveryQueue,
        DeliveryQueueSnapshot, DeliveryQueueStatusView, DeliveryRetryPolicy, DeliveryStatus,
        DeliveryTargetBackpressurePolicy, DeliveryTransport, FileDeliveryStore,
        MAX_TERMINAL_DELIVERY_ERROR_DETAIL_CHARS, PendingDeliveryRecord, QueuedOutputPlugin,
        TargetBackpressureRecord, TargetBackpressureSnapshot, delivery_store_root_dir,
        effective_retry_delay_ms, metadata_with_replay_context, next_delivery_candidate,
        retry_after_delivery_error, safe_delivery_error, safe_reply_target,
        safe_terminal_delivery_error_detail, terminal_delivery_error, unresolved_dead_letter_count,
    };
    use kheish_output::{OutputHost, ResponseEnvelope};
    use kheish_types::{ConversationKey, ReplyHandle};

    struct FlakyTransport {
        failures_left: Mutex<usize>,
        delivered: Arc<Mutex<Vec<ResponseEnvelope>>>,
    }

    struct RetryAfterTransport {
        retry_after_ms: u64,
    }

    fn sample_record(id: &str) -> PendingDeliveryRecord {
        PendingDeliveryRecord {
            id: id.to_string(),
            conversation: ConversationKey {
                session_id: "session-1".to_string(),
                thread_id: None,
            },
            run_id: Some("run-1".to_string()),
            reply: ReplyHandle {
                plugin: "http".to_string(),
                address: r#"{"url":"https://example.invalid"}"#.to_string(),
            },
            content: "hello".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            metadata: Value::Null,
            attempts: 0,
            next_attempt_at_ms: now_ms(),
            last_error: None,
        }
    }

    fn queue_with_target_policy(
        temp: &tempfile::TempDir,
        dispatcher: DeliveryDispatcher,
        target_policy: DeliveryTargetBackpressurePolicy,
    ) -> Result<Arc<DeliveryQueue>> {
        Ok(Arc::new(DeliveryQueue::load_with_policies(
            temp.path().join("deliveries.json"),
            Arc::new(dispatcher),
            DeliveryRetryPolicy {
                initial_retry_delay_ms: 10,
                max_retry_delay_ms: 10,
                max_retry_after_delay_ms: 1_000,
                max_attempts: DEFAULT_MAX_DELIVERY_ATTEMPTS,
            },
            target_policy,
        )?))
    }

    #[test]
    fn delivery_error_and_reply_target_are_redacted_for_persistence_and_logs() {
        let error = anyhow!("failed to deliver to https://example.test/hook?token=secret-token");
        let safe_error = safe_delivery_error(&error);
        assert_eq!(safe_error, "failed:internal");
        assert!(!safe_error.contains("secret-token"));

        let reply = ReplyHandle {
            plugin: "http".to_string(),
            address: r#"{"url":"https://example.test/hook?token=secret-token"}"#.to_string(),
        };
        let target = safe_reply_target(&reply);
        assert!(target.starts_with("http:address_sha256:"));
        assert!(!target.contains("secret-token"));
    }

    #[test]
    fn terminal_delivery_dead_letter_detail_is_redacted_and_bounded() {
        let detail = format!(
            "external fetch callback rejected: not in the configured external fetch allowlist: https://example.test/hook?token=secret-token {}",
            "x".repeat(700)
        );
        let error = terminal_delivery_error(detail);
        let safe_error = safe_delivery_error(&error);
        let terminal_detail = safe_terminal_delivery_error_detail(&error, &safe_error);

        assert_eq!(safe_error, "failed:blocked_external_fetch");
        assert!(terminal_detail.contains("not in the configured external fetch allowlist"));
        assert!(!terminal_detail.contains("secret-token"));
        assert!(terminal_detail.chars().count() <= MAX_TERMINAL_DELIVERY_ERROR_DETAIL_CHARS + 3);
    }

    #[async_trait]
    impl DeliveryTransport for FlakyTransport {
        async fn deliver(&self, response: ResponseEnvelope) -> Result<()> {
            let mut failures_left = self.failures_left.lock();
            if *failures_left > 0 {
                *failures_left -= 1;
                return Err(anyhow!("temporary failure"));
            }
            drop(failures_left);
            self.delivered.lock().push(response);
            Ok(())
        }
    }

    #[async_trait]
    impl DeliveryTransport for RetryAfterTransport {
        async fn deliver(&self, _response: ResponseEnvelope) -> Result<()> {
            Err(retry_after_delivery_error(
                self.retry_after_ms,
                "rate limited by downstream target",
            ))
        }
    }

    #[tokio::test]
    async fn dispatcher_treats_unknown_transport_as_terminal() -> Result<()> {
        let dispatcher = DeliveryDispatcher::new();
        let error = dispatcher
            .deliver(ResponseEnvelope {
                conversation: ConversationKey {
                    session_id: "session-1".to_string(),
                    thread_id: None,
                },
                reply_targets: Vec::new(),
                reply: Some(ReplyHandle {
                    plugin: "missing".to_string(),
                    address: "target-1".to_string(),
                }),
                content: "hello".to_string(),
                parts: Vec::new(),
                artifacts: Vec::new(),
                metadata: Value::Null,
            })
            .await
            .expect_err("unknown transport should fail terminally");
        assert_eq!(safe_delivery_error(&error), "failed:terminal");
        Ok(())
    }

    #[tokio::test]
    async fn queued_output_plugin_retries_until_success() -> Result<()> {
        let temp = tempdir()?;
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher = DeliveryDispatcher::new();
        dispatcher.register(
            "http",
            FlakyTransport {
                failures_left: Mutex::new(1),
                delivered: delivered.clone(),
            },
        );
        let queue = Arc::new(DeliveryQueue::load(
            temp.path().join("deliveries.json"),
            Arc::new(dispatcher),
        )?);
        let worker = queue.clone().spawn_worker();
        let mut host = OutputHost::new();
        host.register(QueuedOutputPlugin::new("http", queue));
        host.deliver(ResponseEnvelope {
            conversation: ConversationKey {
                session_id: "session-1".to_string(),
                thread_id: None,
            },
            reply_targets: Vec::new(),
            reply: Some(ReplyHandle {
                plugin: "http".to_string(),
                address: r#"{"url":"https://example.invalid"}"#.to_string(),
            }),
            content: "hello".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            metadata: Value::Null,
        })
        .await?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if delivered.lock().len() == 1 {
                break;
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for queued delivery"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        worker.abort();
        Ok(())
    }

    #[tokio::test]
    async fn queued_delivery_survives_restart() -> Result<()> {
        let temp = tempdir()?;
        let delivered = Arc::new(Mutex::new(Vec::new()));

        let mut failing_dispatcher = DeliveryDispatcher::new();
        failing_dispatcher.register(
            "http",
            FlakyTransport {
                failures_left: Mutex::new(10),
                delivered: delivered.clone(),
            },
        );
        let queue_path = temp.path().join("deliveries.json");
        let queue = Arc::new(DeliveryQueue::load(
            queue_path.clone(),
            Arc::new(failing_dispatcher),
        )?);
        let worker = queue.clone().spawn_worker();
        queue
            .enqueue(ResponseEnvelope {
                conversation: ConversationKey {
                    session_id: "session-restart".to_string(),
                    thread_id: None,
                },
                reply_targets: Vec::new(),
                reply: Some(ReplyHandle {
                    plugin: "http".to_string(),
                    address: r#"{"url":"https://example.invalid"}"#.to_string(),
                }),
                content: "hello".to_string(),
                parts: Vec::new(),
                artifacts: Vec::new(),
                metadata: Value::Null,
            })
            .await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        worker.abort();
        assert!(
            delivery_store_root_dir(&queue_path).exists(),
            "queue state should be persisted"
        );

        let mut succeeding_dispatcher = DeliveryDispatcher::new();
        succeeding_dispatcher.register(
            "http",
            FlakyTransport {
                failures_left: Mutex::new(0),
                delivered: delivered.clone(),
            },
        );
        let restarted = Arc::new(DeliveryQueue::load(
            queue_path,
            Arc::new(succeeding_dispatcher),
        )?);
        let worker = restarted.clone().spawn_worker();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if delivered.lock().len() == 1 {
                break;
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for restarted queued delivery"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        worker.abort();
        Ok(())
    }

    #[test]
    fn next_candidate_keeps_fifo_within_one_reply_target() {
        let shared_reply = ReplyHandle {
            plugin: "http".to_string(),
            address: "same-target".to_string(),
        };
        let snapshot = DeliveryQueueSnapshot {
            next_id: 2,
            pending: BTreeMap::from([
                (
                    "delivery-1".to_string(),
                    PendingDeliveryRecord {
                        id: "delivery-1".to_string(),
                        conversation: ConversationKey {
                            session_id: "session-1".to_string(),
                            thread_id: None,
                        },
                        run_id: None,
                        reply: shared_reply.clone(),
                        content: "first".to_string(),
                        parts: Vec::new(),
                        artifacts: Vec::new(),
                        metadata: Value::Null,
                        attempts: 1,
                        next_attempt_at_ms: now_ms().saturating_add(60_000),
                        last_error: Some("retry later".to_string()),
                    },
                ),
                (
                    "delivery-2".to_string(),
                    PendingDeliveryRecord {
                        id: "delivery-2".to_string(),
                        conversation: ConversationKey {
                            session_id: "session-1".to_string(),
                            thread_id: None,
                        },
                        run_id: None,
                        reply: shared_reply,
                        content: "second".to_string(),
                        parts: Vec::new(),
                        artifacts: Vec::new(),
                        metadata: Value::Null,
                        attempts: 0,
                        next_attempt_at_ms: now_ms(),
                        last_error: None,
                    },
                ),
            ]),
        };

        let (record, wait_for) =
            next_delivery_candidate(&snapshot, &TargetBackpressureSnapshot::default());
        assert!(
            record.is_none(),
            "later deliveries must wait behind the target head"
        );
        assert!(
            wait_for.is_some(),
            "worker should sleep until the head delivery is due again"
        );
    }

    #[test]
    fn next_candidate_skips_throttled_target_but_dispatches_other_target() {
        let now = now_ms();
        let mut throttled = sample_record("delivery-1");
        throttled.reply.address = "same-target".to_string();
        throttled.next_attempt_at_ms = now;
        let mut other = sample_record("delivery-2");
        other.reply.address = "other-target".to_string();
        other.next_attempt_at_ms = now;
        let snapshot = DeliveryQueueSnapshot {
            next_id: 2,
            pending: BTreeMap::from([
                (throttled.id.clone(), throttled.clone()),
                (other.id.clone(), other.clone()),
            ]),
        };
        let target_key = safe_reply_target(&throttled.reply);
        let target_backpressure = TargetBackpressureSnapshot {
            targets: BTreeMap::from([(
                target_key.clone(),
                TargetBackpressureRecord {
                    target: target_key,
                    blocked_until_ms: now.saturating_add(60_000),
                    ..Default::default()
                },
            )]),
        };

        let (record, wait_for) = next_delivery_candidate(&snapshot, &target_backpressure);

        assert_eq!(
            record.map(|record| record.id),
            Some("delivery-2".to_string())
        );
        assert!(wait_for.is_none());
    }

    #[tokio::test]
    async fn queued_delivery_drops_items_after_retry_budget_is_exhausted() -> Result<()> {
        let temp = tempdir()?;
        let mut dispatcher = DeliveryDispatcher::new();
        dispatcher.register(
            "http",
            FlakyTransport {
                failures_left: Mutex::new(usize::MAX),
                delivered: Arc::new(Mutex::new(Vec::new())),
            },
        );
        let queue = Arc::new(DeliveryQueue::load(
            temp.path().join("deliveries.json"),
            Arc::new(dispatcher),
        )?);
        let record = PendingDeliveryRecord {
            id: "delivery-1".to_string(),
            conversation: ConversationKey {
                session_id: "session-1".to_string(),
                thread_id: None,
            },
            run_id: None,
            reply: ReplyHandle {
                plugin: "http".to_string(),
                address: r#"{"url":"https://example.invalid"}"#.to_string(),
            },
            content: "hello".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            metadata: Value::Null,
            attempts: DEFAULT_MAX_DELIVERY_ATTEMPTS.saturating_sub(1),
            next_attempt_at_ms: now_ms(),
            last_error: Some("still failing".to_string()),
        };
        {
            let mut snapshot = queue.snapshot.lock().await;
            snapshot.pending.insert(record.id.clone(), record.clone());
            snapshot.next_id = 1;
            queue.store.save_meta(snapshot.next_id)?;
            queue.store.save_pending(&record)?;
        }

        queue.process_one(record).await?;

        let snapshot = queue.snapshot.lock().await;
        assert!(
            snapshot.pending.is_empty(),
            "delivery should be dropped after exhausting the retry budget"
        );
        let dead_letter = fs::read_to_string(queue.store.dead_letter_path())?;
        assert!(dead_letter.contains("delivery-1"));
        Ok(())
    }

    #[tokio::test]
    async fn queued_delivery_records_completed_views_after_success() -> Result<()> {
        let temp = tempdir()?;
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher = DeliveryDispatcher::new();
        dispatcher.register(
            "http",
            FlakyTransport {
                failures_left: Mutex::new(0),
                delivered,
            },
        );
        let queue = Arc::new(DeliveryQueue::load(
            temp.path().join("deliveries.json"),
            Arc::new(dispatcher),
        )?);
        let record = sample_record("delivery-1");
        {
            let mut snapshot = queue.snapshot.lock().await;
            snapshot.next_id = 1;
            snapshot.pending.insert(record.id.clone(), record.clone());
            queue.store.save_meta(snapshot.next_id)?;
            queue.store.save_pending(&record)?;
        }

        queue.process_one(record).await?;

        let delivered = queue
            .list_views(DeliveryListFilter {
                status: Some(DeliveryStatus::Delivered),
                ..Default::default()
            })
            .await?;
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].delivery_id, "delivery-1");
        assert_eq!(delivered[0].attempts, 1);
        assert_eq!(delivered[0].run_id.as_deref(), Some("run-1"));
        Ok(())
    }

    #[tokio::test]
    async fn queued_delivery_replays_dead_letter_as_new_pending_item() -> Result<()> {
        let temp = tempdir()?;
        let mut dispatcher = DeliveryDispatcher::new();
        dispatcher.register(
            "http",
            FlakyTransport {
                failures_left: Mutex::new(usize::MAX),
                delivered: Arc::new(Mutex::new(Vec::new())),
            },
        );
        let queue = Arc::new(DeliveryQueue::load(
            temp.path().join("deliveries.json"),
            Arc::new(dispatcher),
        )?);
        let mut record = sample_record("delivery-1");
        record.attempts = DEFAULT_MAX_DELIVERY_ATTEMPTS.saturating_sub(1);
        {
            let mut snapshot = queue.snapshot.lock().await;
            snapshot.next_id = 1;
            snapshot.pending.insert(record.id.clone(), record.clone());
            queue.store.save_meta(snapshot.next_id)?;
            queue.store.save_pending(&record)?;
        }
        queue.process_one(record).await?;

        let replay = queue
            .replay_dead_letter("delivery-1", false)
            .await?
            .ok_or_else(|| anyhow!("dead-letter replay should exist"))?;

        assert_eq!(replay.replayed_from_delivery_id, "delivery-1");
        assert_ne!(replay.delivery.delivery_id, "delivery-1");
        assert_eq!(replay.delivery.status, DeliveryStatus::Pending);
        assert_eq!(
            replay.delivery.replayed_from_delivery_id.as_deref(),
            Some("delivery-1")
        );
        let pending = queue
            .list_views(DeliveryListFilter {
                status: Some(DeliveryStatus::Pending),
                ..Default::default()
            })
            .await?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].delivery_id, replay.delivery.delivery_id);
        Ok(())
    }

    #[tokio::test]
    async fn queued_delivery_replay_is_idempotent_without_force() -> Result<()> {
        let temp = tempdir()?;
        let mut dispatcher = DeliveryDispatcher::new();
        dispatcher.register(
            "http",
            FlakyTransport {
                failures_left: Mutex::new(usize::MAX),
                delivered: Arc::new(Mutex::new(Vec::new())),
            },
        );
        let queue = Arc::new(DeliveryQueue::load(
            temp.path().join("deliveries.json"),
            Arc::new(dispatcher),
        )?);
        let mut record = sample_record("delivery-1");
        record.attempts = DEFAULT_MAX_DELIVERY_ATTEMPTS.saturating_sub(1);
        {
            let mut snapshot = queue.snapshot.lock().await;
            snapshot.next_id = 1;
            snapshot.pending.insert(record.id.clone(), record.clone());
            queue.store.save_meta(snapshot.next_id)?;
            queue.store.save_pending(&record)?;
        }
        queue.process_one(record).await?;

        let first = queue
            .replay_dead_letter("delivery-1", false)
            .await?
            .ok_or_else(|| anyhow!("first dead-letter replay should exist"))?;
        let duplicate = queue
            .replay_dead_letter("delivery-1", false)
            .await?
            .ok_or_else(|| anyhow!("duplicate dead-letter replay should exist"))?;
        let forced = queue
            .replay_dead_letter("delivery-1", true)
            .await?
            .ok_or_else(|| anyhow!("forced dead-letter replay should exist"))?;

        assert_eq!(duplicate.delivery.delivery_id, first.delivery.delivery_id);
        assert_eq!(duplicate.delivery.status, DeliveryStatus::Pending);
        assert_ne!(forced.delivery.delivery_id, first.delivery.delivery_id);
        let pending = queue
            .list_views(DeliveryListFilter {
                status: Some(DeliveryStatus::Pending),
                ..Default::default()
            })
            .await?;
        assert_eq!(pending.len(), 2);
        assert_eq!(
            pending
                .iter()
                .filter(|view| view.replayed_from_delivery_id.as_deref() == Some("delivery-1"))
                .count(),
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn queued_delivery_idempotency_key_dedupes_same_reply_target_only() -> Result<()> {
        let temp = tempdir()?;
        let queue = Arc::new(DeliveryQueue::load(
            temp.path().join("deliveries.json"),
            Arc::new(DeliveryDispatcher::new()),
        )?);
        let mut envelope = ResponseEnvelope {
            conversation: ConversationKey {
                session_id: "session-1".to_string(),
                thread_id: None,
            },
            reply_targets: Vec::new(),
            reply: Some(ReplyHandle {
                plugin: "http".to_string(),
                address: r#"{"url":"https://example.invalid/a"}"#.to_string(),
            }),
            content: "hello".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            metadata: serde_json::json!({
                "run_id": "run-1",
                "delivery_idempotency_key": "operator-notification:session-1:call-1",
            }),
        };

        queue.enqueue(envelope.clone()).await?;
        queue.enqueue(envelope.clone()).await?;
        let pending = queue
            .list_views(DeliveryListFilter {
                status: Some(DeliveryStatus::Pending),
                ..Default::default()
            })
            .await?;
        assert_eq!(pending.len(), 1);

        envelope.reply = Some(ReplyHandle {
            plugin: "http".to_string(),
            address: r#"{"url":"https://example.invalid/b"}"#.to_string(),
        });
        queue.enqueue(envelope).await?;
        let pending = queue
            .list_views(DeliveryListFilter {
                status: Some(DeliveryStatus::Pending),
                ..Default::default()
            })
            .await?;
        assert_eq!(pending.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn queued_delivery_replay_is_idempotent_after_replay_delivers() -> Result<()> {
        let temp = tempdir()?;
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher = DeliveryDispatcher::new();
        dispatcher.register(
            "http",
            FlakyTransport {
                failures_left: Mutex::new(0),
                delivered,
            },
        );
        let queue = Arc::new(DeliveryQueue::load(
            temp.path().join("deliveries.json"),
            Arc::new(dispatcher),
        )?);
        let record = sample_record("delivery-1");
        queue.store.append_dead_letter(&record, "failed:terminal")?;
        {
            let mut snapshot = queue.snapshot.lock().await;
            snapshot.next_id = 1;
            queue.store.save_meta(snapshot.next_id)?;
        }

        let first = queue
            .replay_dead_letter("delivery-1", false)
            .await?
            .ok_or_else(|| anyhow!("first dead-letter replay should exist"))?;
        let replay_record = {
            let snapshot = queue.snapshot.lock().await;
            snapshot
                .pending
                .get(&first.delivery.delivery_id)
                .cloned()
                .ok_or_else(|| anyhow!("missing replay pending record"))?
        };
        queue.process_one(replay_record).await?;

        let duplicate = queue
            .replay_dead_letter("delivery-1", false)
            .await?
            .ok_or_else(|| anyhow!("duplicate dead-letter replay should exist"))?;
        assert_eq!(duplicate.delivery.delivery_id, first.delivery.delivery_id);
        assert_eq!(duplicate.delivery.status, DeliveryStatus::Delivered);

        let pending = queue
            .list_views(DeliveryListFilter {
                status: Some(DeliveryStatus::Pending),
                ..Default::default()
            })
            .await?;
        assert!(
            pending.is_empty(),
            "non-forced replay should not create a second pending item"
        );
        Ok(())
    }

    #[tokio::test]
    async fn delivery_dead_letter_resolution_is_persisted_and_clears_unresolved_status()
    -> Result<()> {
        let temp = tempdir()?;
        let queue_path = temp.path().join("deliveries.json");
        let queue = Arc::new(DeliveryQueue::load(
            queue_path.clone(),
            Arc::new(DeliveryDispatcher::new()),
        )?);
        let record = sample_record("delivery-1");
        queue.store.append_dead_letter(&record, "failed:terminal")?;

        let status = queue.status_snapshot(now_ms()).await?;
        assert_eq!(status.dead_lettered, 1);
        assert_eq!(status.unresolved_dead_lettered, 1);

        let resolved = queue
            .resolve_dead_letter(
                "delivery-1",
                "destination retired; api-key: sk-resolve-secret",
            )
            .await?
            .ok_or_else(|| anyhow!("resolved dead-letter view should exist"))?;
        assert_eq!(resolved.status, DeliveryStatus::DeadLettered);
        assert!(resolved.dead_letter_resolved_at_ms.is_some());
        let reason = resolved
            .dead_letter_resolution_reason
            .as_deref()
            .ok_or_else(|| anyhow!("missing resolution reason"))?;
        assert!(reason.contains("destination retired"));
        assert!(!reason.contains("sk-resolve-secret"));

        let duplicate = queue
            .resolve_dead_letter("delivery-1", "different reason")
            .await?
            .ok_or_else(|| anyhow!("duplicate resolution should return the existing view"))?;
        assert_eq!(
            duplicate.dead_letter_resolved_at_ms,
            resolved.dead_letter_resolved_at_ms
        );
        assert_eq!(
            duplicate.dead_letter_resolution_reason,
            resolved.dead_letter_resolution_reason
        );
        assert!(
            queue
                .resolve_dead_letter("delivery-missing", "not found")
                .await?
                .is_none()
        );

        let status = queue.status_snapshot(now_ms()).await?;
        assert_eq!(status.unresolved_dead_lettered, 0);
        let ledger = fs::read_to_string(queue.store.resolved_dead_letter_path())?;
        assert!(ledger.contains("delivery-1"));
        assert!(!ledger.contains("sk-resolve-secret"));

        let restarted = Arc::new(DeliveryQueue::load(
            queue_path.clone(),
            Arc::new(DeliveryDispatcher::new()),
        )?);
        let status = restarted.status_snapshot(now_ms()).await?;
        assert_eq!(status.dead_lettered, 1);
        assert_eq!(status.unresolved_dead_lettered, 0);

        fs::OpenOptions::new()
            .append(true)
            .open(restarted.store.resolved_dead_letter_path())?
            .write_all(b"{not valid resolved dead-letter json\n")?;
        let restarted_after_corrupt = Arc::new(DeliveryQueue::load(
            queue_path,
            Arc::new(DeliveryDispatcher::new()),
        )?);
        let status = restarted_after_corrupt.status_snapshot(now_ms()).await?;
        assert_eq!(status.unresolved_dead_lettered, 0);
        assert_eq!(status.status_error_count, 1);
        assert!(
            status
                .status_error
                .as_deref()
                .is_some_and(|error| error.contains("resolved dead-letter delivery ledger line"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn delivery_status_summary_resolves_replay_sources_idempotently() -> Result<()> {
        let temp = tempdir()?;
        let queue = Arc::new(DeliveryQueue::load(
            temp.path().join("deliveries.json"),
            Arc::new(DeliveryDispatcher::new()),
        )?);
        let first = sample_record("delivery-1");
        let second = sample_record("delivery-2");
        queue.store.append_dead_letter(&first, "failed:terminal")?;
        queue.store.append_dead_letter(&second, "failed:terminal")?;
        assert_eq!(
            queue
                .status_snapshot(now_ms())
                .await?
                .unresolved_dead_lettered,
            2
        );

        let mut replay = sample_record("delivery-10");
        replay.metadata = metadata_with_replay_context(&Value::Null, "delivery-1", 1);
        queue.store.append_completed(&replay)?;
        assert_eq!(
            queue
                .status_snapshot(now_ms())
                .await?
                .unresolved_dead_lettered,
            1
        );

        let mut forced_replay = sample_record("delivery-11");
        forced_replay.metadata = metadata_with_replay_context(&Value::Null, "delivery-1", 1);
        queue.store.append_completed(&forced_replay)?;
        queue
            .store
            .append_resolved_dead_letter("delivery-1", "already replayed")?;
        assert_eq!(
            queue
                .status_snapshot(now_ms())
                .await?
                .unresolved_dead_lettered,
            1
        );

        queue
            .store
            .append_resolved_dead_letter("delivery-2", "operator resolved")?;
        assert_eq!(
            queue
                .status_snapshot(now_ms())
                .await?
                .unresolved_dead_lettered,
            0
        );
        Ok(())
    }

    #[tokio::test]
    async fn delivery_status_marks_summary_stale_when_terminal_ledger_is_newer() -> Result<()> {
        let temp = tempdir()?;
        let queue = Arc::new(DeliveryQueue::load(
            temp.path().join("deliveries.json"),
            Arc::new(DeliveryDispatcher::new()),
        )?);
        queue
            .store
            .append_dead_letter(&sample_record("delivery-1"), "failed:terminal")?;
        let status = queue.status_snapshot(now_ms()).await?;
        assert_eq!(status.status_error_count, 0);

        std::thread::sleep(Duration::from_millis(25));
        let manual = DeadLetterDeliveryRecord {
            dropped_at_ms: now_ms(),
            terminal_error: "failed:manual".to_string(),
            record: sample_record("delivery-2"),
        };
        let line = serde_json::to_string(&manual)?;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(queue.store.dead_letter_path())?
            .write_all(format!("{line}\n").as_bytes())?;

        let status = queue.status_snapshot(now_ms()).await?;
        assert_eq!(status.dead_lettered, 1);
        assert_eq!(status.status_error_count, 1);
        assert!(
            status
                .status_error
                .as_deref()
                .is_some_and(|error| error.contains("older than terminal ledgers"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn delivery_status_update_rebuilds_when_base_summary_is_stale() -> Result<()> {
        let temp = tempdir()?;
        let queue = Arc::new(DeliveryQueue::load(
            temp.path().join("deliveries.json"),
            Arc::new(DeliveryDispatcher::new()),
        )?);
        queue
            .store
            .append_dead_letter(&sample_record("delivery-1"), "failed:terminal")?;

        let missed = DeadLetterDeliveryRecord {
            dropped_at_ms: now_ms(),
            terminal_error: "failed:missed-summary".to_string(),
            record: sample_record("delivery-2"),
        };
        let line = serde_json::to_string(&missed)?;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(queue.store.dead_letter_path())?
            .write_all(format!("{line}\n").as_bytes())?;
        let stale = queue.status_snapshot(now_ms()).await?;
        assert_eq!(stale.dead_lettered, 1);
        assert_eq!(stale.status_error_count, 1);

        queue
            .store
            .append_dead_letter(&sample_record("delivery-3"), "failed:terminal")?;
        let repaired = queue.status_snapshot(now_ms()).await?;
        assert_eq!(repaired.dead_lettered, 3);
        assert_eq!(repaired.unresolved_dead_lettered, 3);
        assert_eq!(repaired.status_error_count, 0);
        assert!(repaired.status_error.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn delivery_status_update_rebuilds_when_summary_is_missing() -> Result<()> {
        let temp = tempdir()?;
        let queue = Arc::new(DeliveryQueue::load(
            temp.path().join("deliveries.json"),
            Arc::new(DeliveryDispatcher::new()),
        )?);
        fs::remove_file(queue.store.status_summary_path())?;

        queue
            .store
            .append_dead_letter(&sample_record("delivery-1"), "failed:terminal")?;
        let status = queue.status_snapshot(now_ms()).await?;
        assert_eq!(status.dead_lettered, 1);
        assert_eq!(status.unresolved_dead_lettered, 1);
        assert_eq!(status.status_error_count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn delivery_bulk_replay_filters_dry_runs_and_is_idempotent() -> Result<()> {
        let temp = tempdir()?;
        let queue = Arc::new(DeliveryQueue::load(
            temp.path().join("deliveries.json"),
            Arc::new(DeliveryDispatcher::new()),
        )?);
        let first = sample_record("delivery-1");
        let mut resolved = sample_record("delivery-2");
        resolved.run_id = Some("run-resolved".to_string());
        let mut other_plugin = sample_record("delivery-3");
        other_plugin.reply.plugin = "telegram".to_string();
        queue.store.append_dead_letter(&first, "failed:terminal")?;
        queue
            .store
            .append_dead_letter(&resolved, "failed:terminal")?;
        queue
            .store
            .append_dead_letter(&other_plugin, "failed:terminal")?;
        queue
            .resolve_dead_letter("delivery-2", "destination retired")
            .await?;
        {
            let mut snapshot = queue.snapshot.lock().await;
            snapshot.next_id = 3;
            queue.store.save_meta(snapshot.next_id)?;
        }

        let dry_run = queue
            .bulk_replay_dead_letters(
                DeliveryListFilter {
                    plugin: Some("http".to_string()),
                    ..Default::default()
                },
                false,
                true,
                true,
                Some(10),
            )
            .await?;
        assert!(dry_run.dry_run);
        assert_eq!(dry_run.matched, 1);
        assert_eq!(dry_run.selected, 1);
        assert_eq!(dry_run.replayed, 0);
        assert_eq!(dry_run.items[0].replayed_from_delivery_id, "delivery-1");
        assert_eq!(
            dry_run.items[0].action,
            DeliveryBulkReplayAction::WouldReplay
        );
        assert!(dry_run.items[0].replay.is_none());
        assert!(queue.snapshot.lock().await.pending.is_empty());

        let replay = queue
            .bulk_replay_dead_letters(
                DeliveryListFilter {
                    plugin: Some("http".to_string()),
                    ..Default::default()
                },
                false,
                false,
                true,
                Some(10),
            )
            .await?;
        assert_eq!(replay.matched, 1);
        assert_eq!(replay.replayed, 1);
        assert_eq!(replay.items[0].action, DeliveryBulkReplayAction::Replayed);
        let replay_id = replay.items[0]
            .replay
            .as_ref()
            .map(|view| view.delivery_id.clone())
            .ok_or_else(|| anyhow!("missing replay view"))?;

        let duplicate = queue
            .bulk_replay_dead_letters(
                DeliveryListFilter {
                    plugin: Some("http".to_string()),
                    ..Default::default()
                },
                false,
                false,
                true,
                Some(10),
            )
            .await?;
        assert_eq!(duplicate.replayed, 0);
        assert_eq!(duplicate.existing_replays, 1);
        assert_eq!(
            duplicate.items[0]
                .replay
                .as_ref()
                .map(|view| view.delivery_id.as_str()),
            Some(replay_id.as_str())
        );

        let include_resolved = queue
            .bulk_replay_dead_letters(
                DeliveryListFilter {
                    plugin: Some("http".to_string()),
                    ..Default::default()
                },
                false,
                true,
                false,
                Some(10),
            )
            .await?;
        assert_eq!(include_resolved.matched, 2);
        assert!(
            include_resolved
                .items
                .iter()
                .any(|item| item.replayed_from_delivery_id == "delivery-2")
        );

        let limited = queue
            .bulk_replay_dead_letters(DeliveryListFilter::default(), true, true, false, Some(1))
            .await?;
        assert_eq!(limited.matched, 3);
        assert_eq!(limited.selected, 1);
        assert_eq!(limited.truncated, 2);
        Ok(())
    }

    #[tokio::test]
    async fn delivery_terminal_ledgers_skip_corrupt_lines() -> Result<()> {
        let temp = tempdir()?;
        let queue_path = temp.path().join("deliveries.json");
        let queue = Arc::new(DeliveryQueue::load(
            queue_path.clone(),
            Arc::new(DeliveryDispatcher::new()),
        )?);
        let completed = sample_record("delivery-4");
        let dead_letter = sample_record("delivery-7");
        queue.store.append_completed(&completed)?;
        fs::OpenOptions::new()
            .append(true)
            .open(queue.store.completed_path())?
            .write_all(b"{not valid completed json\n")?;
        queue
            .store
            .append_dead_letter(&dead_letter, "failed:terminal")?;
        fs::OpenOptions::new()
            .append(true)
            .open(queue.store.dead_letter_path())?
            .write_all(b"{not valid dead-letter json\n")?;

        let restarted = Arc::new(DeliveryQueue::load(
            queue_path,
            Arc::new(DeliveryDispatcher::new()),
        )?);
        assert_eq!(restarted.snapshot.lock().await.next_id, 7);

        let views = restarted.list_views(DeliveryListFilter::default()).await?;
        assert_eq!(views.len(), 2);
        assert!(views.iter().any(|view| {
            view.delivery_id == "delivery-4" && view.status == DeliveryStatus::Delivered
        }));
        assert!(views.iter().any(|view| {
            view.delivery_id == "delivery-7" && view.status == DeliveryStatus::DeadLettered
        }));

        let status = restarted.status_snapshot(now_ms()).await?;
        assert_eq!(status.delivered, 1);
        assert_eq!(status.dead_lettered, 1);
        assert_eq!(status.unresolved_dead_lettered, 1);
        assert_eq!(status.status_error_count, 2);
        assert!(
            status
                .status_error
                .as_deref()
                .is_some_and(|error| error.contains("completed delivery ledger line"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn delivery_queue_status_snapshot_counts_backlog_and_worker_liveness() -> Result<()> {
        let temp = tempdir()?;
        let queue = Arc::new(DeliveryQueue::load(
            temp.path().join("deliveries.json"),
            Arc::new(DeliveryDispatcher::new()),
        )?);
        let now = now_ms();
        let mut retrying_head = sample_record("delivery-1");
        retrying_head.attempts = 1;
        retrying_head.next_attempt_at_ms = now.saturating_add(60_000);
        let mut blocked_ready = sample_record("delivery-2");
        blocked_ready.next_attempt_at_ms = now;
        let mut other_ready = sample_record("delivery-3");
        other_ready.reply.address = r#"{"url":"https://other.invalid"}"#.to_string();
        other_ready.next_attempt_at_ms = now;
        let completed = sample_record("delivery-4");
        let dead_letter = sample_record("delivery-5");
        {
            let mut snapshot = queue.snapshot.lock().await;
            snapshot.next_id = 5;
            for record in [&retrying_head, &blocked_ready, &other_ready] {
                snapshot.pending.insert(record.id.clone(), record.clone());
                queue.store.save_pending(record)?;
            }
            queue.store.save_meta(snapshot.next_id)?;
            queue.store.append_completed(&completed)?;
            queue
                .store
                .append_dead_letter(&dead_letter, "failed:terminal")?;
        }

        let status = queue.status_snapshot(now).await?;
        assert_eq!(status.pending, 2);
        assert_eq!(status.retrying, 1);
        assert_eq!(status.delivered, 1);
        assert_eq!(status.dead_lettered, 1);
        assert_eq!(status.unresolved_dead_lettered, 1);
        assert_eq!(status.ready, 2);
        assert_eq!(status.dispatchable, 1);
        assert_eq!(status.target_count, 2);
        assert_eq!(status.blocked_target_count, 1);
        assert_eq!(status.next_attempt_at_ms, Some(now));
        assert!(status.worker_started_at_ms.is_none());
        assert!(status.worker_heartbeat_at_ms.is_none());

        let empty_queue = Arc::new(DeliveryQueue::load(
            temp.path().join("empty-deliveries.json"),
            Arc::new(DeliveryDispatcher::new()),
        )?);
        let worker = empty_queue.clone().spawn_worker();
        for _ in 0..20 {
            let status = empty_queue.status_snapshot(now_ms()).await?;
            if status.worker_started_at_ms.is_some() && status.worker_heartbeat_at_ms.is_some() {
                assert!(status.worker_lag_ms.is_some());
                worker.abort();
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        worker.abort();
        anyhow::bail!("delivery worker heartbeat was not recorded")
    }

    #[test]
    fn delivery_queue_status_prometheus_metrics_cover_queue_worker_and_dead_letters() {
        let status = DeliveryQueueStatusView {
            pending: 1,
            retrying: 2,
            delivered: 3,
            dead_lettered: 4,
            unresolved_dead_lettered: 2,
            ready: 5,
            dispatchable: 6,
            target_count: 7,
            blocked_target_count: 8,
            throttled_target_count: 9,
            open_circuit_target_count: 10,
            next_attempt_at_ms: Some(11),
            next_target_available_at_ms: Some(12),
            worker_started_at_ms: Some(13),
            worker_heartbeat_at_ms: Some(14),
            worker_lag_ms: Some(15),
            status_error_count: 0,
            status_error: None,
        };
        let metrics = status.render_prometheus_metrics();
        assert!(metrics.contains("kheish_delivery_pending 1"));
        assert!(metrics.contains("kheish_delivery_dead_lettered 4"));
        assert!(metrics.contains("kheish_delivery_unresolved_dead_lettered 2"));
        assert!(metrics.contains("kheish_delivery_dispatchable 6"));
        assert!(metrics.contains("kheish_delivery_targets_blocked 8"));
        assert!(metrics.contains("kheish_delivery_targets_throttled 9"));
        assert!(metrics.contains("kheish_delivery_targets_open_circuit 10"));
        assert!(metrics.contains("kheish_delivery_next_target_available_at_ms 12"));
        assert!(metrics.contains("kheish_delivery_worker_lag_ms 15"));
        assert!(metrics.contains("kheish_delivery_status_errors 0"));
    }

    #[test]
    fn retry_after_delay_is_capped_by_delivery_policy() {
        let policy = super::DeliveryRetryPolicy {
            initial_retry_delay_ms: 1_000,
            max_retry_delay_ms: 8_000,
            max_retry_after_delay_ms: 60_000,
            max_attempts: 8,
        };

        assert_eq!(effective_retry_delay_ms(&policy, 1, Some(5_000)), 5_000);
        assert_eq!(
            effective_retry_delay_ms(&policy, 1, Some(24 * 60 * 60 * 1_000)),
            60_000
        );
        assert_eq!(effective_retry_delay_ms(&policy, 4, Some(1_000)), 8_000);
    }

    #[test]
    fn delivery_queue_load_restores_uncommitted_settled_tombstone() -> Result<()> {
        let temp = tempdir()?;
        let queue_path = temp.path().join("deliveries.json");
        let store = FileDeliveryStore::new(queue_path.clone());
        let record = sample_record("delivery-1");
        store.save_meta(1)?;
        store.save_pending(&record)?;
        assert!(store.mark_pending_settled("delivery-1")?);

        let queue = DeliveryQueue::load(queue_path.clone(), Arc::new(DeliveryDispatcher::new()))?;
        let snapshot = queue.snapshot.blocking_lock();
        assert!(snapshot.pending.contains_key("delivery-1"));
        assert!(store.pending_path("delivery-1").exists());
        assert!(!store.settled_path("delivery-1").exists());
        Ok(())
    }

    #[test]
    fn delivery_queue_load_cleans_committed_settled_tombstone() -> Result<()> {
        let temp = tempdir()?;
        let queue_path = temp.path().join("deliveries.json");
        let store = FileDeliveryStore::new(queue_path.clone());
        let record = sample_record("delivery-1");
        store.save_meta(1)?;
        store.save_pending(&record)?;
        assert!(store.mark_pending_settled("delivery-1")?);
        store.append_dead_letter(&record, "failed:terminal")?;

        let queue = DeliveryQueue::load(queue_path, Arc::new(DeliveryDispatcher::new()))?;
        let snapshot = queue.snapshot.blocking_lock();
        assert!(snapshot.pending.is_empty());
        assert!(!store.settled_path("delivery-1").exists());
        Ok(())
    }

    #[tokio::test]
    async fn queued_delivery_respects_retry_after_delay() -> Result<()> {
        let temp = tempdir()?;
        let mut dispatcher = DeliveryDispatcher::new();
        dispatcher.register(
            "http",
            RetryAfterTransport {
                retry_after_ms: 5_000,
            },
        );
        let queue = Arc::new(DeliveryQueue::load(
            temp.path().join("deliveries.json"),
            Arc::new(dispatcher),
        )?);
        let record = PendingDeliveryRecord {
            id: "delivery-1".to_string(),
            conversation: ConversationKey {
                session_id: "session-1".to_string(),
                thread_id: None,
            },
            run_id: None,
            reply: ReplyHandle {
                plugin: "http".to_string(),
                address: r#"{"url":"https://example.invalid"}"#.to_string(),
            },
            content: "hello".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            metadata: Value::Null,
            attempts: 0,
            next_attempt_at_ms: now_ms(),
            last_error: None,
        };
        {
            let mut snapshot = queue.snapshot.lock().await;
            snapshot.pending.insert(record.id.clone(), record.clone());
            snapshot.next_id = 1;
            queue.store.save_meta(snapshot.next_id)?;
            queue.store.save_pending(&record)?;
        }

        let before = now_ms();
        queue.process_one(record).await?;

        let snapshot = queue.snapshot.lock().await;
        let updated = snapshot
            .pending
            .get("delivery-1")
            .ok_or_else(|| anyhow!("delivery should still be pending"))?;
        assert_eq!(updated.attempts, 1);
        assert_eq!(updated.last_error.as_deref(), Some("failed:rate_limited"));
        assert!(
            updated.next_attempt_at_ms >= before.saturating_add(4_500),
            "retry_after_ms should dominate generic retry delay: {updated:#?}"
        );
        drop(snapshot);
        let status = queue.status_snapshot(before).await?;
        assert_eq!(status.throttled_target_count, 1);
        assert_eq!(status.open_circuit_target_count, 0);
        assert!(
            status.next_target_available_at_ms >= Some(before.saturating_add(4_500)),
            "target backpressure should honor retry_after_ms: {status:#?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn target_circuit_opens_after_retryable_failures_and_resets_on_success() -> Result<()> {
        let temp = tempdir()?;
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher = DeliveryDispatcher::new();
        dispatcher.register(
            "http",
            FlakyTransport {
                failures_left: Mutex::new(2),
                delivered: delivered.clone(),
            },
        );
        let queue = queue_with_target_policy(
            &temp,
            dispatcher,
            DeliveryTargetBackpressurePolicy {
                min_interval_ms: 0,
                circuit_failure_threshold: 2,
                circuit_cooldown_ms: 60_000,
            },
        )?;
        let record = sample_record("delivery-1");
        {
            let mut snapshot = queue.snapshot.lock().await;
            snapshot.pending.insert(record.id.clone(), record.clone());
            snapshot.next_id = 1;
            queue.store.save_meta(snapshot.next_id)?;
            queue.store.save_pending(&record)?;
        }

        queue.process_one(record.clone()).await?;
        let second = queue
            .snapshot
            .lock()
            .await
            .pending
            .get("delivery-1")
            .cloned()
            .ok_or_else(|| anyhow!("delivery should remain pending after first failure"))?;
        queue.process_one(second).await?;

        let target_state = queue.target_backpressure.lock().await;
        let target = safe_reply_target(&record.reply);
        let opened = target_state
            .targets
            .get(&target)
            .ok_or_else(|| anyhow!("missing target backpressure state"))?
            .clone();
        assert_eq!(opened.consecutive_retryable_failures, 2);
        assert!(opened.circuit_opened_at_ms.is_some());
        assert!(opened.blocked_until_ms >= now_ms().saturating_add(55_000));
        drop(target_state);
        let persisted = fs::read_to_string(queue.store.target_backpressure_path())?;
        assert!(persisted.contains(&target));
        assert!(!persisted.contains("example.invalid"));

        let status = queue.status_snapshot(now_ms()).await?;
        assert_eq!(status.open_circuit_target_count, 1);
        assert_eq!(status.throttled_target_count, 1);

        let third = queue
            .snapshot
            .lock()
            .await
            .pending
            .get("delivery-1")
            .cloned()
            .ok_or_else(|| anyhow!("delivery should remain pending before success"))?;
        queue.process_one(third).await?;
        assert_eq!(delivered.lock().len(), 1);
        let target_state = queue.target_backpressure.lock().await;
        let reset = target_state
            .targets
            .get(&target)
            .ok_or_else(|| anyhow!("missing reset target backpressure state"))?;
        assert_eq!(reset.consecutive_retryable_failures, 0);
        assert!(reset.circuit_opened_at_ms.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn status_ignores_stale_backpressure_targets() -> Result<()> {
        let temp = tempdir()?;
        let queue = queue_with_target_policy(
            &temp,
            DeliveryDispatcher::new(),
            DeliveryTargetBackpressurePolicy {
                min_interval_ms: 0,
                circuit_failure_threshold: 1,
                circuit_cooldown_ms: 60_000,
            },
        )?;
        {
            let mut target_backpressure = queue.target_backpressure.lock().await;
            target_backpressure.targets.insert(
                "http:address_sha256:stale123".to_string(),
                TargetBackpressureRecord {
                    target: "http:address_sha256:stale123".to_string(),
                    last_attempt_at_ms: now_ms(),
                    consecutive_retryable_failures: 4,
                    blocked_until_ms: now_ms().saturating_add(60_000),
                    circuit_opened_at_ms: Some(now_ms()),
                },
            );
            queue.store.save_target_backpressure(&target_backpressure)?;
        }

        let status = queue.status_snapshot(now_ms()).await?;
        assert_eq!(status.target_count, 0);
        assert_eq!(status.throttled_target_count, 0);
        assert_eq!(status.open_circuit_target_count, 0);
        assert_eq!(status.dispatchable, 0);
        assert_eq!(status.next_target_available_at_ms, None);

        let reloaded = DeliveryQueue::load_with_policies(
            temp.path().join("deliveries.json"),
            Arc::new(DeliveryDispatcher::new()),
            DeliveryRetryPolicy::default(),
            DeliveryTargetBackpressurePolicy::default(),
        )?;
        let status = reloaded.status_snapshot(now_ms()).await?;
        assert_eq!(status.target_count, 0);
        assert_eq!(status.throttled_target_count, 0);
        assert_eq!(status.open_circuit_target_count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn delivery_status_and_metrics_do_not_leak_reply_target() -> Result<()> {
        let temp = tempdir()?;
        let mut dispatcher = DeliveryDispatcher::new();
        dispatcher.register(
            "http",
            RetryAfterTransport {
                retry_after_ms: 60_000,
            },
        );
        let queue = queue_with_target_policy(
            &temp,
            dispatcher,
            DeliveryTargetBackpressurePolicy {
                min_interval_ms: 0,
                circuit_failure_threshold: 1,
                circuit_cooldown_ms: 60_000,
            },
        )?;
        let raw_target =
            r#"{"url":"https://example.invalid/hook?token=super-secret-target"}"#.to_string();
        let mut record = sample_record("delivery-1");
        record.reply.address = raw_target.clone();
        {
            let mut snapshot = queue.snapshot.lock().await;
            snapshot.pending.insert(record.id.clone(), record.clone());
            snapshot.next_id = 1;
            queue.store.save_meta(snapshot.next_id)?;
            queue.store.save_pending(&record)?;
        }

        queue.process_one(record).await?;

        let status = queue.status_snapshot(now_ms()).await?;
        let status_json = serde_json::to_string(&status)?;
        let metrics = status.render_prometheus_metrics();
        let views_json =
            serde_json::to_string(&queue.list_views(DeliveryListFilter::default()).await?)?;
        let persisted = fs::read_to_string(queue.store.target_backpressure_path())?;
        for rendered in [&status_json, &metrics, &views_json, &persisted] {
            assert!(!rendered.contains("super-secret-target"));
            assert!(!rendered.contains("example.invalid/hook"));
            assert!(!rendered.contains(&raw_target));
        }
        assert!(persisted.contains("http:address_sha256:"));
        Ok(())
    }

    #[tokio::test]
    async fn delivery_backpressure_reset_filters_and_supports_dry_run() -> Result<()> {
        let temp = tempdir()?;
        let queue = queue_with_target_policy(
            &temp,
            DeliveryDispatcher::new(),
            DeliveryTargetBackpressurePolicy::default(),
        )?;
        {
            let mut target_backpressure = queue.target_backpressure.lock().await;
            for target in [
                "http:address_sha256:first",
                "http:address_sha256:second",
                "slack:address_sha256:first",
            ] {
                target_backpressure.targets.insert(
                    target.to_string(),
                    TargetBackpressureRecord {
                        target: target.to_string(),
                        last_attempt_at_ms: now_ms(),
                        consecutive_retryable_failures: 3,
                        blocked_until_ms: now_ms().saturating_add(60_000),
                        circuit_opened_at_ms: Some(now_ms()),
                    },
                );
            }
            queue.store.save_target_backpressure(&target_backpressure)?;
        }

        let dry_run = queue
            .reset_target_backpressure(None, Some("http"), true)
            .await?;
        assert!(dry_run.dry_run);
        assert_eq!(dry_run.matched, 2);
        assert_eq!(dry_run.removed, 0);
        assert_eq!(
            queue.target_backpressure.lock().await.targets.len(),
            3,
            "dry-run must not mutate target backpressure"
        );

        let reset = queue
            .reset_target_backpressure(Some("http:address_sha256:first"), Some("http"), false)
            .await?;
        assert!(!reset.dry_run);
        assert_eq!(reset.matched, 1);
        assert_eq!(reset.removed, 1);
        assert_eq!(reset.targets, vec!["http:address_sha256:first".to_string()]);
        let target_backpressure = queue.target_backpressure.lock().await;
        assert!(
            !target_backpressure
                .targets
                .contains_key("http:address_sha256:first")
        );
        assert!(
            target_backpressure
                .targets
                .contains_key("http:address_sha256:second")
        );
        assert!(
            target_backpressure
                .targets
                .contains_key("slack:address_sha256:first")
        );
        drop(target_backpressure);
        let persisted = fs::read_to_string(queue.store.target_backpressure_path())?;
        assert!(!persisted.contains("http:address_sha256:first"));
        assert!(persisted.contains("http:address_sha256:second"));
        assert!(persisted.contains("slack:address_sha256:first"));
        Ok(())
    }

    #[test]
    fn delivery_queue_load_migrates_legacy_snapshot_files() -> Result<()> {
        let temp = tempdir()?;
        let legacy_path = temp.path().join("deliveries.json");
        write_json_pretty_atomically(
            &legacy_path,
            &DeliveryQueueSnapshot {
                next_id: 7,
                pending: BTreeMap::from([(
                    "delivery-7".to_string(),
                    PendingDeliveryRecord {
                        id: "delivery-7".to_string(),
                        conversation: ConversationKey {
                            session_id: "session-legacy".to_string(),
                            thread_id: None,
                        },
                        run_id: None,
                        reply: ReplyHandle {
                            plugin: "http".to_string(),
                            address: r#"{"url":"https://example.invalid"}"#.to_string(),
                        },
                        content: "legacy".to_string(),
                        parts: Vec::new(),
                        artifacts: Vec::new(),
                        metadata: Value::Null,
                        attempts: 0,
                        next_attempt_at_ms: now_ms(),
                        last_error: None,
                    },
                )]),
            },
        )?;

        let queue = DeliveryQueue::load(legacy_path.clone(), Arc::new(DeliveryDispatcher::new()))?;
        let snapshot = queue.snapshot.blocking_lock();
        assert_eq!(snapshot.next_id, 7);
        assert!(snapshot.pending.contains_key("delivery-7"));
        assert!(
            !legacy_path.exists(),
            "legacy snapshot should be migrated away"
        );
        assert!(
            delivery_store_root_dir(&legacy_path)
                .join("pending")
                .join("delivery-7.json")
                .exists()
        );
        Ok(())
    }

    #[test]
    fn delivery_queue_load_reconciles_next_id_with_pending_files() -> Result<()> {
        let temp = tempdir()?;
        let queue_path = temp.path().join("deliveries.json");
        let store = FileDeliveryStore::new(queue_path.clone());
        store.save_meta(10)?;
        store.save_pending(&PendingDeliveryRecord {
            id: "delivery-11".to_string(),
            conversation: ConversationKey {
                session_id: "session-restart".to_string(),
                thread_id: None,
            },
            run_id: None,
            reply: ReplyHandle {
                plugin: "http".to_string(),
                address: r#"{"url":"https://example.invalid"}"#.to_string(),
            },
            content: "pending".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            metadata: Value::Null,
            attempts: 0,
            next_attempt_at_ms: now_ms(),
            last_error: None,
        })?;

        let queue = DeliveryQueue::load(queue_path, Arc::new(DeliveryDispatcher::new()))?;
        let snapshot = queue.snapshot.blocking_lock();
        assert_eq!(snapshot.next_id, 11);
        assert!(snapshot.pending.contains_key("delivery-11"));
        Ok(())
    }

    #[tokio::test]
    async fn delivery_queue_load_reconciles_next_id_with_terminal_ledgers() -> Result<()> {
        let temp = tempdir()?;
        let queue_path = temp.path().join("deliveries.json");
        let store = FileDeliveryStore::new(queue_path.clone());
        store.save_meta(1)?;
        let completed = PendingDeliveryRecord {
            id: "delivery-12".to_string(),
            conversation: ConversationKey {
                session_id: "session-completed".to_string(),
                thread_id: None,
            },
            run_id: None,
            reply: ReplyHandle {
                plugin: "http".to_string(),
                address: r#"{"url":"https://example.invalid"}"#.to_string(),
            },
            content: "completed".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            metadata: Value::Null,
            attempts: 1,
            next_attempt_at_ms: now_ms(),
            last_error: None,
        };
        let dead_letter = PendingDeliveryRecord {
            id: "delivery-15".to_string(),
            content: "dead letter".to_string(),
            ..completed.clone()
        };
        store.append_completed(&completed)?;
        store.append_dead_letter(&dead_letter, "failed:terminal")?;

        let queue = Arc::new(DeliveryQueue::load(
            queue_path,
            Arc::new(DeliveryDispatcher::new()),
        )?);
        queue
            .enqueue(ResponseEnvelope {
                conversation: ConversationKey {
                    session_id: "session-new".to_string(),
                    thread_id: None,
                },
                reply_targets: Vec::new(),
                reply: Some(ReplyHandle {
                    plugin: "http".to_string(),
                    address: r#"{"url":"https://example.invalid/new"}"#.to_string(),
                }),
                content: "new".to_string(),
                parts: Vec::new(),
                artifacts: Vec::new(),
                metadata: Value::Null,
            })
            .await?;
        let snapshot = queue.snapshot.lock().await;
        assert_eq!(snapshot.next_id, 16);
        assert!(snapshot.pending.contains_key("delivery-16"));
        Ok(())
    }

    #[test]
    fn unresolved_dead_letter_count_ignores_delivered_replays() {
        let mut original = sample_record("delivery-1");
        let unresolved = sample_record("delivery-2");
        let delivered_replay = PendingDeliveryRecord {
            id: "delivery-3".to_string(),
            metadata: super::metadata_with_replay_context(&Value::Null, "delivery-1", 123),
            ..sample_record("delivery-3")
        };
        original.last_error = Some("failed:terminal".to_string());
        let dead_letters = vec![
            DeadLetterDeliveryRecord {
                dropped_at_ms: 123,
                terminal_error: "failed:terminal".to_string(),
                record: original,
            },
            DeadLetterDeliveryRecord {
                dropped_at_ms: 124,
                terminal_error: "failed:terminal".to_string(),
                record: unresolved,
            },
        ];
        let completed = vec![CompletedDeliveryRecord {
            delivered_at_ms: 125,
            record: delivered_replay,
        }];

        assert_eq!(
            unresolved_dead_letter_count(&dead_letters, &completed, &[]),
            1
        );
    }
}
