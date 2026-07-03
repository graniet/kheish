//! Persistence and topology support types shared by daemon state workflows.

use super::*;
use parking_lot::Mutex;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use async_trait::async_trait;
use kheish_output::{OutputManifest, OutputPlugin, ResponseEnvelope};
use kheish_session::{
    append_json_line_sync, atomic_write, prepare_storage_path_for_write,
    resolve_storage_path_for_read, write_json_pretty_atomically,
};
use kheish_types::{InputContentPart, TaskRecord, TaskStatus};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::state_files::read_json_or_quarantine;

const MAX_OBSERVATION_INGRESS_RECEIPTS: usize = 10_000;
const MAX_OBSERVATION_INGRESS_PENDING_AGE_MS: u64 = 15 * 60 * 1000;
const MAX_OBSERVATION_INGRESS_RECEIPT_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const MAX_SESSION_RUN_IDEMPOTENCY_RECEIPTS: usize = 10_000;
const MAX_SESSION_RUN_IDEMPOTENCY_PENDING_AGE_MS: u64 = 15 * 60 * 1000;
const MAX_SESSION_RUN_IDEMPOTENCY_RECEIPT_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const SUPERVISOR_AUDIT_LIMIT: usize = 2_048;

fn supervisor_audit_lock_for(root: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let root_key = root.canonicalize().unwrap_or_else(|_| {
        if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(root))
                .unwrap_or_else(|_| root.to_path_buf())
        }
        .components()
        .collect::<PathBuf>()
    });
    let mut locks = locks.lock();
    locks
        .entry(root_key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn trim_ascii_bytes(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|position| position + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

pub(super) fn import_inline_asset(
    assets: &Arc<FileAssetStore>,
    upload: &InlineAssetUpload,
) -> Result<StoredAssetRecord> {
    let bytes = STANDARD
        .decode(upload.content_base64.trim())
        .context("failed to decode attachment content_base64")?;
    assets.import_bytes(&upload.file_name, upload.media_type.as_deref(), &bytes)
}

#[derive(Clone, Debug)]
pub(super) enum ResolvedInputPart {
    Text(String),
    Asset(StoredAssetRecord),
}

impl ResolvedInputPart {
    pub(super) fn content_part(&self) -> Option<InputContentPart> {
        match self {
            Self::Text(text) if !text.trim().is_empty() => {
                Some(InputContentPart::Text { text: text.clone() })
            }
            Self::Text(_) => None,
            Self::Asset(asset) => Some(InputContentPart::Attachment {
                attachment: asset.attachment_ref(),
            }),
        }
    }

    pub(super) fn attachment_ref(&self) -> Option<AttachmentRef> {
        match self {
            Self::Asset(asset) => Some(asset.attachment_ref()),
            Self::Text(_) => None,
        }
    }
}

pub(super) fn normalized_submit_input_items(
    parts: &[ResolvedInputPart],
) -> Vec<SubmitInputItemRequest> {
    parts
        .iter()
        .filter_map(|part| match part {
            ResolvedInputPart::Text(text) if !text.trim().is_empty() => {
                Some(SubmitInputItemRequest::Text { text: text.clone() })
            }
            ResolvedInputPart::Text(_) => None,
            ResolvedInputPart::Asset(asset) => Some(SubmitInputItemRequest::AssetReference {
                asset_id: asset.id.clone(),
            }),
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionIndex {
    pub(crate) sessions: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) task_summaries: BTreeMap<String, SessionTaskStatusSummaryState>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) session_personas: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) reply_targets: BTreeMap<String, Vec<ReplyHandle>>,
    #[serde(default)]
    pub(crate) bindings: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) connector_cursors: BTreeMap<String, ConnectorCursorState>,
    #[serde(default)]
    pub(crate) connector_ingress_receipts: BTreeMap<String, ConnectorIngressReceiptState>,
    #[serde(default)]
    pub(crate) observation_ingress_receipts: BTreeMap<String, ObservationIngressReceiptState>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) session_run_idempotency_receipts:
        BTreeMap<String, SessionRunIdempotencyReceiptState>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) run_operation_idempotency_receipts:
        BTreeMap<String, SessionRunIdempotencyReceiptState>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) sidechain_spawn_receipts: BTreeMap<String, SidechainSpawnReceiptState>,
    #[serde(default, skip_serializing_if = "RunMemoryIndex::is_empty")]
    pub(crate) run_memories: RunMemoryIndex,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionTaskStatusSummaryState {
    #[serde(default)]
    pub(crate) total: usize,
    #[serde(default)]
    pub(crate) pending: usize,
    #[serde(default)]
    pub(crate) in_progress: usize,
    #[serde(default)]
    pub(crate) blocked: usize,
    #[serde(default)]
    pub(crate) completed: usize,
    #[serde(default)]
    pub(crate) failed: usize,
    #[serde(default)]
    pub(crate) cancelled: usize,
}

impl SessionTaskStatusSummaryState {
    pub(crate) fn from_tasks(tasks: &[TaskRecord]) -> Self {
        let mut summary = Self::default();
        for task in tasks {
            summary.total += 1;
            match task.status {
                TaskStatus::Pending => summary.pending += 1,
                TaskStatus::InProgress => summary.in_progress += 1,
                TaskStatus::Blocked => summary.blocked += 1,
                TaskStatus::Completed => summary.completed += 1,
                TaskStatus::Failed => summary.failed += 1,
                TaskStatus::Cancelled => summary.cancelled += 1,
            }
        }
        summary
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// Tracks one persisted connector cursor owned by the daemon.
pub(crate) enum ConnectorCursorState {
    TelegramPolling { next_update_id: i64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// Records one connector ingress idempotency receipt persisted in the daemon topology index.
pub(crate) enum ConnectorIngressReceiptState {
    Pending {
        recorded_at_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fingerprint: Option<String>,
    },
    Submitted {
        run_id: String,
        recorded_at_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fingerprint: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Represents the current idempotency reservation state for one connector ingress submission.
pub(crate) enum ConnectorIngressReservation {
    Reserved,
    Pending,
    Existing { run_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Represents the current persisted state for one connector ingress key without reserving it.
pub(crate) enum ConnectorIngressLookup {
    Absent,
    Pending,
    Existing { run_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// Records one observation ingest idempotency receipt persisted in the daemon topology index.
pub(crate) enum ObservationIngressReceiptState {
    Pending {
        request_fingerprint: String,
        recorded_at_ms: u64,
    },
    Submitted {
        observation_id: String,
        request_fingerprint: String,
        recorded_at_ms: u64,
    },
}

impl ObservationIngressReceiptState {
    fn recorded_at_ms(&self) -> u64 {
        match self {
            Self::Pending { recorded_at_ms, .. } | Self::Submitted { recorded_at_ms, .. } => {
                *recorded_at_ms
            }
        }
    }

    pub(crate) fn request_fingerprint(&self) -> &str {
        match self {
            Self::Pending {
                request_fingerprint,
                ..
            }
            | Self::Submitted {
                request_fingerprint,
                ..
            } => request_fingerprint,
        }
    }

    fn is_stale(&self, now_ms: u64) -> bool {
        match self {
            Self::Pending { recorded_at_ms, .. } => {
                now_ms.saturating_sub(*recorded_at_ms) > MAX_OBSERVATION_INGRESS_PENDING_AGE_MS
            }
            Self::Submitted { recorded_at_ms, .. } => {
                now_ms.saturating_sub(*recorded_at_ms) > MAX_OBSERVATION_INGRESS_RECEIPT_AGE_MS
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Represents the current idempotency reservation state for one observation ingest submission.
pub(crate) enum ObservationIngressReservation {
    Reserved,
    Pending,
    Existing { observation_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// Records one direct session-run idempotency receipt in the daemon topology index.
pub(crate) enum SessionRunIdempotencyReceiptState {
    Pending {
        run_id: String,
        request_fingerprint: String,
        recorded_at_ms: u64,
    },
    Submitted {
        run_id: String,
        request_fingerprint: String,
        recorded_at_ms: u64,
    },
}

impl SessionRunIdempotencyReceiptState {
    fn recorded_at_ms(&self) -> u64 {
        match self {
            Self::Pending { recorded_at_ms, .. } | Self::Submitted { recorded_at_ms, .. } => {
                *recorded_at_ms
            }
        }
    }

    pub(crate) fn run_id(&self) -> &str {
        match self {
            Self::Pending { run_id, .. } | Self::Submitted { run_id, .. } => run_id,
        }
    }

    pub(crate) fn request_fingerprint(&self) -> &str {
        match self {
            Self::Pending {
                request_fingerprint,
                ..
            }
            | Self::Submitted {
                request_fingerprint,
                ..
            } => request_fingerprint,
        }
    }

    fn is_stale(&self, now_ms: u64) -> bool {
        match self {
            Self::Pending { recorded_at_ms, .. } => {
                now_ms.saturating_sub(*recorded_at_ms) > MAX_SESSION_RUN_IDEMPOTENCY_PENDING_AGE_MS
            }
            Self::Submitted { recorded_at_ms, .. } => {
                now_ms.saturating_sub(*recorded_at_ms) > MAX_SESSION_RUN_IDEMPOTENCY_RECEIPT_AGE_MS
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Represents the current reservation state for one direct session-run idempotency key.
pub(crate) enum SessionRunIdempotencyReservation {
    Reserved { run_id: String },
    Pending { run_id: String },
    Existing { run_id: String },
}

pub(crate) fn prune_session_run_idempotency_receipts(
    index: &mut SessionIndex,
    now_ms: u64,
) -> bool {
    let len_before = index.session_run_idempotency_receipts.len();
    index
        .session_run_idempotency_receipts
        .retain(|_, receipt| !receipt.is_stale(now_ms));
    if index.session_run_idempotency_receipts.len() <= MAX_SESSION_RUN_IDEMPOTENCY_RECEIPTS {
        return index.session_run_idempotency_receipts.len() != len_before;
    }

    let mut keys_by_age = index
        .session_run_idempotency_receipts
        .iter()
        .map(|(key, receipt)| (key.clone(), receipt.recorded_at_ms()))
        .collect::<Vec<_>>();
    keys_by_age.sort_by_key(|(_, recorded_at_ms)| *recorded_at_ms);
    let overflow = keys_by_age
        .len()
        .saturating_sub(MAX_SESSION_RUN_IDEMPOTENCY_RECEIPTS);
    for (key, _) in keys_by_age.into_iter().take(overflow) {
        index.session_run_idempotency_receipts.remove(&key);
    }
    index.session_run_idempotency_receipts.len() != len_before
}

pub(crate) fn prune_run_operation_idempotency_receipts(
    index: &mut SessionIndex,
    now_ms: u64,
) -> bool {
    let len_before = index.run_operation_idempotency_receipts.len();
    index
        .run_operation_idempotency_receipts
        .retain(|_, receipt| !receipt.is_stale(now_ms));
    if index.run_operation_idempotency_receipts.len() <= MAX_SESSION_RUN_IDEMPOTENCY_RECEIPTS {
        return index.run_operation_idempotency_receipts.len() != len_before;
    }

    let mut keys_by_age = index
        .run_operation_idempotency_receipts
        .iter()
        .map(|(key, receipt)| (key.clone(), receipt.recorded_at_ms()))
        .collect::<Vec<_>>();
    keys_by_age.sort_by_key(|(_, recorded_at_ms)| *recorded_at_ms);
    let overflow = keys_by_age
        .len()
        .saturating_sub(MAX_SESSION_RUN_IDEMPOTENCY_RECEIPTS);
    for (key, _) in keys_by_age.into_iter().take(overflow) {
        index.run_operation_idempotency_receipts.remove(&key);
    }
    index.run_operation_idempotency_receipts.len() != len_before
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// Records one sidechain spawn idempotency receipt owned by the daemon topology index.
pub(crate) enum SidechainSpawnReceiptState {
    Pending {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
        request_fingerprint: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subtask_request_json: Option<String>,
        recorded_at_ms: u64,
    },
    Committed {
        agent_id: String,
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
        request_fingerprint: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subtask_request_json: Option<String>,
        recorded_at_ms: u64,
    },
}

pub(crate) fn prune_observation_ingress_receipts(index: &mut SessionIndex, now_ms: u64) -> bool {
    let len_before = index.observation_ingress_receipts.len();
    index
        .observation_ingress_receipts
        .retain(|_, receipt| !receipt.is_stale(now_ms));
    if index.observation_ingress_receipts.len() <= MAX_OBSERVATION_INGRESS_RECEIPTS {
        return index.observation_ingress_receipts.len() != len_before;
    }

    let mut keys_by_age = index
        .observation_ingress_receipts
        .iter()
        .map(|(key, receipt)| (key.clone(), receipt.recorded_at_ms()))
        .collect::<Vec<_>>();
    keys_by_age.sort_by_key(|(_, recorded_at_ms)| *recorded_at_ms);
    let overflow = keys_by_age
        .len()
        .saturating_sub(MAX_OBSERVATION_INGRESS_RECEIPTS);
    for (key, _) in keys_by_age.into_iter().take(overflow) {
        index.observation_ingress_receipts.remove(&key);
    }
    index.observation_ingress_receipts.len() != len_before
}

fn merge_supervisor_audit_log(
    snapshot: &mut AgentSupervisorSnapshot,
    ledger: Vec<AgentSupervisorAuditEntry>,
) {
    let mut by_id = snapshot
        .audit_log
        .iter()
        .cloned()
        .map(|entry| (entry.audit_id, entry))
        .collect::<BTreeMap<_, _>>();
    for entry in ledger {
        if let Some(existing) = by_id.get(&entry.audit_id) {
            if existing != &entry {
                warn!(
                    audit_id = entry.audit_id,
                    agent_id = %entry.agent_id.0,
                    "skipping supervisor audit ledger entry that conflicts with checkpointed audit entry"
                );
            }
            continue;
        }
        snapshot.next_audit_id = snapshot.next_audit_id.max(entry.audit_id);
        by_id.insert(entry.audit_id, entry);
    }
    snapshot.audit_log = by_id.into_values().collect();
    let overflow = snapshot
        .audit_log
        .len()
        .saturating_sub(SUPERVISOR_AUDIT_LIMIT);
    if overflow > 0 {
        snapshot.audit_log.drain(..overflow);
    }
}

fn supervisor_audit_entry_rejection_reason(
    entry: &AgentSupervisorAuditEntry,
) -> Option<&'static str> {
    if entry.audit_id == 0 {
        return Some("audit id must be positive");
    }
    if entry.event.trim().is_empty() {
        return Some("event must not be empty");
    }
    None
}

fn reconcile_supervisor_audit_ledger_entries(
    snapshot: &AgentSupervisorSnapshot,
    ledger: Vec<AgentSupervisorAuditEntry>,
    path: &Path,
) -> SupervisorAuditLedgerLoad {
    let checkpointed = snapshot
        .audit_log
        .iter()
        .map(|entry| (entry.audit_id, entry))
        .collect::<BTreeMap<_, _>>();
    let mut accepted = BTreeMap::<u64, AgentSupervisorAuditEntry>::new();
    let mut corrupt_line_count = 0usize;

    for entry in ledger {
        if let Some(reason) = supervisor_audit_entry_rejection_reason(&entry) {
            corrupt_line_count += 1;
            warn!(
                path = %path.display(),
                audit_id = entry.audit_id,
                reason,
                "skipping invalid supervisor audit ledger entry"
            );
            continue;
        }

        if let Some(snapshot_entry) = checkpointed.get(&entry.audit_id) {
            corrupt_line_count += 1;
            if *snapshot_entry != &entry {
                warn!(
                    path = %path.display(),
                    audit_id = entry.audit_id,
                    "skipping supervisor audit ledger entry that conflicts with checkpointed snapshot"
                );
            }
            continue;
        }

        if entry.audit_id <= snapshot.next_audit_id {
            corrupt_line_count += 1;
            warn!(
                path = %path.display(),
                audit_id = entry.audit_id,
                checkpoint_audit_id = snapshot.next_audit_id,
                "skipping stale supervisor audit ledger entry"
            );
            continue;
        }

        match accepted.get(&entry.audit_id) {
            Some(existing) if existing == &entry => {
                corrupt_line_count += 1;
            }
            Some(_) => {
                corrupt_line_count += 1;
                warn!(
                    path = %path.display(),
                    audit_id = entry.audit_id,
                    "skipping conflicting duplicate supervisor audit ledger entry"
                );
            }
            None => {
                accepted.insert(entry.audit_id, entry);
            }
        }
    }

    SupervisorAuditLedgerLoad {
        entries: accepted.into_values().collect(),
        corrupt_line_count,
    }
}

impl kheish_agent::AgentSupervisorAuditSink for FileDaemonStore {
    fn append_supervisor_audit(&self, entry: &AgentSupervisorAuditEntry) -> Result<()> {
        FileDaemonStore::append_supervisor_audit(self, entry)
    }
}

#[derive(Default)]
struct SupervisorAuditLedgerLoad {
    entries: Vec<AgentSupervisorAuditEntry>,
    corrupt_line_count: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct FileDaemonStore {
    root: PathBuf,
    supervisor_audit_lock: Arc<Mutex<()>>,
}

impl FileDaemonStore {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let supervisor_audit_lock = supervisor_audit_lock_for(&root);
        Self {
            root,
            supervisor_audit_lock,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("daemon-index.json")
    }

    fn supervisor_path(&self) -> PathBuf {
        self.root.join("daemon-supervisor.json")
    }

    fn supervisor_audit_path(&self) -> PathBuf {
        self.root.join("daemon-supervisor-audit.jsonl")
    }

    fn outputs_path(&self, session_id: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.root.join("outputs"), session_id, "jsonl")
    }

    pub(crate) fn shell_task_output_path(&self, task_id: &str) -> PathBuf {
        self.root.join("shell-tasks").join(format!("{task_id}.log"))
    }

    pub(crate) fn load_index(&self) -> Result<SessionIndex> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(SessionIndex::default());
        }
        Ok(read_json_or_quarantine(&path, "daemon index")?.unwrap_or_default())
    }

    pub(crate) fn index_modified_at(&self) -> Result<Option<SystemTime>> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(None);
        }
        let metadata = fs::metadata(&path)?;
        Ok(Some(metadata.modified()?))
    }

    pub(crate) fn save_index(&self, index: &SessionIndex) -> Result<()> {
        write_json_pretty_atomically(&self.index_path(), index)
    }

    pub(crate) fn load_supervisor(&self) -> Result<Option<AgentSupervisorSnapshot>> {
        let _guard = self.supervisor_audit_lock.lock();
        let path = self.supervisor_path();
        if !path.exists() {
            return Ok(None);
        }
        let mut snapshot = serde_json::from_slice::<AgentSupervisorSnapshot>(&fs::read(path)?)?;
        let ledger_path = self.supervisor_audit_path();
        let ledger = self.load_supervisor_audit_ledger_locked()?;
        let syntactic_corrupt_line_count = ledger.corrupt_line_count;
        let ledger =
            reconcile_supervisor_audit_ledger_entries(&snapshot, ledger.entries, &ledger_path);
        if syntactic_corrupt_line_count + ledger.corrupt_line_count > 0 {
            self.rewrite_supervisor_audit_ledger_locked(&ledger.entries)?;
        }
        if !ledger.entries.is_empty() {
            merge_supervisor_audit_log(&mut snapshot, ledger.entries);
        }
        Ok(Some(snapshot))
    }

    pub(crate) fn save_supervisor(&self, snapshot: &AgentSupervisorSnapshot) -> Result<()> {
        let _guard = self.supervisor_audit_lock.lock();
        write_json_pretty_atomically(&self.supervisor_path(), snapshot)?;
        self.compact_supervisor_audit_ledger_locked(snapshot.next_audit_id)
    }

    pub(crate) fn append_supervisor_audit(&self, entry: &AgentSupervisorAuditEntry) -> Result<()> {
        if let Some(reason) = supervisor_audit_entry_rejection_reason(entry) {
            bail!(
                "invalid supervisor audit entry {}: {reason}",
                entry.audit_id
            );
        }
        let _guard = self.supervisor_audit_lock.lock();
        if self.supervisor_audit_ledger_tail_needs_repair_locked()? {
            let ledger = self.load_supervisor_audit_ledger_locked()?;
            self.rewrite_supervisor_audit_ledger_locked(&ledger.entries)?;
        }
        append_json_line_sync(&self.supervisor_audit_path(), entry)
    }

    fn load_supervisor_audit_ledger_locked(&self) -> Result<SupervisorAuditLedgerLoad> {
        let path = self.supervisor_audit_path();
        if !path.exists() {
            return Ok(SupervisorAuditLedgerLoad::default());
        }
        let raw = fs::read(&path)?;
        let mut entries: Vec<AgentSupervisorAuditEntry> = Vec::new();
        let mut corrupt_line_count = 0usize;
        for (line_index, line) in raw.split(|byte| *byte == b'\n').enumerate() {
            let trimmed = trim_ascii_bytes(line);
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_slice::<AgentSupervisorAuditEntry>(trimmed) {
                Ok(entry) => {
                    if let Some(reason) = supervisor_audit_entry_rejection_reason(&entry) {
                        corrupt_line_count += 1;
                        warn!(
                            path = %path.display(),
                            line = line_index + 1,
                            audit_id = entry.audit_id,
                            reason,
                            "skipping invalid supervisor audit ledger entry"
                        );
                    } else {
                        entries.push(entry);
                    }
                }
                Err(error) => {
                    corrupt_line_count += 1;
                    warn!(
                        path = %path.display(),
                        line = line_index + 1,
                        error = %error,
                        "skipping corrupt supervisor audit ledger line"
                    );
                }
            }
        }
        Ok(SupervisorAuditLedgerLoad {
            entries,
            corrupt_line_count,
        })
    }

    fn supervisor_audit_ledger_tail_needs_repair_locked(&self) -> Result<bool> {
        let path = self.supervisor_audit_path();
        if !path.exists() {
            return Ok(false);
        }
        let bytes = fs::read(&path)?;
        Ok(bytes.last().is_some_and(|last| *last != b'\n'))
    }

    fn rewrite_supervisor_audit_ledger_locked(
        &self,
        entries: &[AgentSupervisorAuditEntry],
    ) -> Result<()> {
        let path = self.supervisor_audit_path();
        let mut payload = Vec::new();
        for entry in entries {
            serde_json::to_writer(&mut payload, entry)?;
            payload.push(b'\n');
        }
        atomic_write(&path, &payload)
    }

    fn compact_supervisor_audit_ledger_locked(&self, checkpoint_audit_id: u64) -> Result<()> {
        let path = self.supervisor_audit_path();
        if !path.exists() {
            return Ok(());
        }
        let retained = self
            .load_supervisor_audit_ledger_locked()?
            .entries
            .into_iter()
            .filter(|entry| entry.audit_id > checkpoint_audit_id)
            .collect::<Vec<_>>();
        self.rewrite_supervisor_audit_ledger_locked(&retained)
    }

    pub(crate) fn append_output(&self, record: &DaemonOutputRecord) -> Result<()> {
        let path = prepare_storage_path_for_write(
            &self.root.join("outputs"),
            &record.session_id,
            "jsonl",
        )?;
        append_json_line_sync(&path, record)
    }

    pub(crate) fn load_outputs(&self, session_id: &str) -> Result<Vec<DaemonOutputRecord>> {
        let path = self.outputs_path(session_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let raw = fs::read_to_string(path)?;
        raw.lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub(crate) fn ensure_parent_dir(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
        fs::create_dir_all(parent)?;
        Ok(())
    }
}

pub(crate) struct DaemonOutputPlugin {
    sender: DaemonOutputSender,
}

impl DaemonOutputPlugin {
    pub(crate) fn new(sender: DaemonOutputSender) -> Self {
        Self { sender }
    }
}

pub(crate) type DaemonOutputSender = mpsc::UnboundedSender<DaemonOutputDelivery>;
pub(crate) type DaemonOutputReceiver = mpsc::UnboundedReceiver<DaemonOutputDelivery>;

pub(crate) struct DaemonOutputDelivery {
    pub(crate) response: ResponseEnvelope,
    pub(crate) ack: oneshot::Sender<Result<()>>,
}

#[async_trait]
impl OutputPlugin for DaemonOutputPlugin {
    fn manifest(&self) -> OutputManifest {
        OutputManifest {
            name: "daemon".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Daemon output collector".to_string(),
        }
    }

    async fn deliver(&self, response: ResponseEnvelope) -> Result<()> {
        let (ack, receipt) = oneshot::channel();
        self.sender
            .send(DaemonOutputDelivery { response, ack })
            .map_err(|_| anyhow!("daemon output receiver is closed"))?;
        receipt
            .await
            .map_err(|_| anyhow!("daemon output receiver stopped before acknowledgement"))?
    }
}
