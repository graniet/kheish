use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use kheish_core::{AgentEngine, LoopPolicy};
use kheish_types::{ArchivedTaskRecord, ConversationKey, LogEntry, SessionCheckpoint};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    append_json_line_sync, append_json_lines_sync, atomic_write, decode_safe_storage_name,
    legacy_storage_path, prepare_storage_path_for_write, resolve_storage_path_for_read,
    safe_storage_name, safe_storage_path,
};

/// The current JSONL envelope version stored on disk.
pub const CURRENT_SESSION_ENVELOPE_VERSION: u32 = 2;

/// A single metadata value larger than this is logged as abnormal growth.
const OVERSIZED_METADATA_VALUE_BYTES: usize = 1024 * 1024;

/// The on-disk footprint of one persisted session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStorageSize {
    /// The session identifier.
    pub session_id: String,
    /// Bytes of the append-only journal.
    pub journal_bytes: u64,
    /// Bytes of the per-key metadata sidecars.
    pub metadata_bytes: u64,
    /// Bytes of the terminal-task archive.
    pub task_archive_bytes: u64,
}

impl SessionStorageSize {
    /// Returns the combined footprint across all storage files.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.journal_bytes + self.metadata_bytes + self.task_archive_bytes
    }
}

/// An audit trail entry for permission decisions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionAuditRecord {
    /// The scope from which the decision was derived.
    pub scope: String,
    /// The affected tool name.
    pub tool_name: String,
    /// The evaluated tool call identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The final decision.
    pub decision: String,
    /// The decision before applying runtime permission mode, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_decision: Option<String>,
    /// The effective runtime permission mode, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_mode: Option<String>,
    /// The named permission mode transformation, when one changed the static rule semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_effect: Option<String>,
    /// The selected static permission rule pattern, when one matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_rule_pattern: Option<String>,
    /// The selected rule provenance, such as static or hook_update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_rule_origin: Option<String>,
    /// The optional user or system justification.
    pub justification: Option<String>,
    /// The optional free-form reason.
    pub reason: Option<String>,
    /// The optional approval request identifier when the decision requires action.
    pub approval_request_id: Option<String>,
}

/// A persisted output dispatch record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredOutputRecord {
    /// The output plugin that received the dispatch.
    pub plugin: String,
    /// The destination address or routing key.
    pub address: String,
    /// The digest of the payload sent to the output.
    pub payload_digest: String,
}

/// The payload stored in each JSONL session line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PersistedSessionRecord {
    /// A journal event emitted by the agent loop.
    Event { entry: LogEntry },
    /// A compaction checkpoint emitted by the agent loop.
    Checkpoint { checkpoint: SessionCheckpoint },
    /// A permission audit emitted by the permission engine.
    PermissionAudit { audit: PermissionAuditRecord },
    /// A generic metadata record.
    Metadata { key: String, value: Value },
    /// A persisted output dispatch.
    Output { output: StoredOutputRecord },
}

/// A versioned session record wrapper stored as one JSONL line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionRecordEnvelope {
    /// The persisted schema version.
    pub version: u32,
    /// The conversation identifier.
    pub session_id: String,
    /// The wrapped session record payload.
    pub record: PersistedSessionRecord,
}

/// A cursor for incremental replay from a session file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionRestoreCursor {
    /// The last loaded journal offset.
    pub last_offset: Option<u64>,
    /// The number of lines consumed from the session file.
    pub line_count: usize,
}

/// The materialized contents of a persisted session file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StoredSession {
    /// The conversation identifier.
    pub session_id: String,
    /// The ordered journal entries.
    pub journal: Vec<LogEntry>,
    /// The ordered checkpoints.
    pub checkpoints: Vec<SessionCheckpoint>,
    /// The ordered permission audit records.
    pub audits: Vec<PermissionAuditRecord>,
    /// The ordered output dispatch records.
    pub outputs: Vec<StoredOutputRecord>,
    /// The latest metadata values.
    pub metadata: std::collections::BTreeMap<String, Value>,
}

impl StoredSession {
    /// Restores an [`AgentEngine`] from the persisted session state.
    pub fn restore_engine(&self, policy: LoopPolicy, thread_id: Option<String>) -> AgentEngine {
        AgentEngine::restore(
            ConversationKey {
                session_id: self.session_id.clone(),
                thread_id,
            },
            policy,
            self.journal.clone(),
            self.checkpoints.clone(),
        )
    }

    /// Flattens the materialized session back into persisted records.
    pub fn into_records(self) -> Vec<PersistedSessionRecord> {
        let mut records = Vec::new();
        records.extend(
            self.journal
                .into_iter()
                .map(|entry| PersistedSessionRecord::Event { entry }),
        );
        records.extend(
            self.checkpoints
                .into_iter()
                .map(|checkpoint| PersistedSessionRecord::Checkpoint { checkpoint }),
        );
        records.extend(
            self.audits
                .into_iter()
                .map(|audit| PersistedSessionRecord::PermissionAudit { audit }),
        );
        records.extend(
            self.metadata
                .into_iter()
                .map(|(key, value)| PersistedSessionRecord::Metadata { key, value }),
        );
        records.extend(
            self.outputs
                .into_iter()
                .map(|output| PersistedSessionRecord::Output { output }),
        );
        records
    }
}

/// A migration that upgrades a legacy envelope JSON value to a newer version.
pub trait SessionMigration: Send + Sync {
    /// Returns the source version handled by this migration.
    fn from_version(&self) -> u32;

    /// Returns the target version produced by this migration.
    fn to_version(&self) -> u32;

    /// Migrates a raw JSON value to the next schema version.
    fn migrate(&self, raw: Value) -> Result<Value>;
}

/// A filesystem-backed append-only session store.
#[derive(Default)]
pub struct FileSessionStore {
    root: PathBuf,
    migrations: Vec<Arc<dyn SessionMigration>>,
}

impl FileSessionStore {
    /// Creates a new store rooted at the given directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            migrations: Vec::new(),
        }
    }

    /// Registers a session envelope migration.
    pub fn with_migration<M>(mut self, migration: M) -> Self
    where
        M: SessionMigration + 'static,
    {
        self.migrations.push(Arc::new(migration));
        self
    }

    fn session_file_entries(&self) -> Result<Vec<(String, PathBuf)>> {
        let mut entries = std::collections::BTreeMap::new();

        let safe_root = self.root.join("__safe");
        if safe_root.exists() {
            for entry in fs::read_dir(&safe_root)
                .with_context(|| format!("failed to read {}", safe_root.display()))?
            {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                    continue;
                };
                if let Some(session_id) = decode_safe_storage_name(stem) {
                    entries.insert(session_id, path);
                }
            }
        }

        if self.root.exists() {
            for entry in fs::read_dir(&self.root)
                .with_context(|| format!("failed to read {}", self.root.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                if path.file_name().and_then(|value| value.to_str()) == Some("__safe") {
                    continue;
                }
                if !entry.file_type()?.is_file() {
                    continue;
                }
                if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                    continue;
                };
                entries.insert(stem.to_string(), path);
            }
        }

        Ok(entries.into_iter().collect())
    }

    /// Returns the filesystem path for a session JSONL file.
    pub fn session_path(&self, session_id: &str) -> PathBuf {
        safe_storage_path(&self.root, session_id, "jsonl")
    }

    /// Returns the directory holding the per-key metadata sidecars.
    fn metadata_sidecar_dir(&self, session_id: &str) -> PathBuf {
        safe_storage_path(&self.root, session_id, "meta")
    }

    /// Writes the latest value of one metadata key to its sidecar file.
    ///
    /// Metadata is last-wins per key; storing each key in its own
    /// atomically-replaced file makes writes O(value) instead of growing the
    /// journal, and reads O(1). The journal is created first when missing so
    /// a metadata-first session stays visible to `list_session_ids`. Returns
    /// whether the stored value changed.
    fn write_metadata_sidecar(&self, session_id: &str, key: &str, value: &Value) -> Result<bool> {
        let journal = prepare_storage_path_for_write(&self.root, session_id, "jsonl")?;
        self.ensure_parent_dir(&journal)?;
        if !journal.exists() {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&journal)
                .with_context(|| format!("failed to create {}", journal.display()))?;
            crate::fs::sync_parent_dir(&journal)?;
        }
        let dir = self.metadata_sidecar_dir(session_id);
        let path = dir.join(format!("{}.json", safe_storage_name(key)));
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() > OVERSIZED_METADATA_VALUE_BYTES {
            tracing::warn!(
                session_id,
                key,
                bytes = bytes.len(),
                "metadata value exceeds {} bytes; the state stored under this key is growing abnormally",
                OVERSIZED_METADATA_VALUE_BYTES
            );
        }
        if let Ok(existing) = fs::read(&path)
            && existing == bytes
        {
            return Ok(false);
        }
        atomic_write(&path, &bytes)
            .with_context(|| format!("failed to write metadata sidecar {}", path.display()))?;
        Ok(true)
    }

    /// Reads the sidecar value of one metadata key, when present.
    fn read_metadata_sidecar(&self, session_id: &str, key: &str) -> Result<Option<Value>> {
        let path = self
            .metadata_sidecar_dir(session_id)
            .join(format!("{}.json", safe_storage_name(key)));
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("corrupt metadata sidecar {}", path.display()))
                .map(Some),
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
            {
                Ok(None)
            }
            Err(error) => Err(error)
                .with_context(|| format!("failed to read metadata sidecar {}", path.display())),
        }
    }

    /// Reads every metadata sidecar of one session.
    fn read_metadata_sidecars(&self, session_id: &str) -> Result<BTreeMap<String, Value>> {
        let dir = self.metadata_sidecar_dir(session_id);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
            {
                return Ok(BTreeMap::new());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read sidecar dir {}", dir.display()));
            }
        };
        let mut metadata = BTreeMap::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(key) = path
                .file_stem()
                .and_then(|value| value.to_str())
                .and_then(decode_safe_storage_name)
            else {
                continue;
            };
            let bytes = fs::read(&path)
                .with_context(|| format!("failed to read metadata sidecar {}", path.display()))?;
            let value = serde_json::from_slice(&bytes)
                .with_context(|| format!("corrupt metadata sidecar {}", path.display()))?;
            metadata.insert(key, value);
        }
        Ok(metadata)
    }

    /// Appends a single record; metadata records go to their key sidecar.
    pub async fn append(&self, session_id: &str, record: PersistedSessionRecord) -> Result<()> {
        if let PersistedSessionRecord::Metadata { key, value } = &record {
            self.write_metadata_sidecar(session_id, key, value)?;
            return Ok(());
        }
        let path = prepare_storage_path_for_write(&self.root, session_id, "jsonl")?;
        self.ensure_parent_dir(&path)?;
        let envelope = SessionRecordEnvelope {
            version: CURRENT_SESSION_ENVELOPE_VERSION,
            session_id: session_id.to_string(),
            record,
        };
        append_json_line_sync(&path, &envelope)
            .with_context(|| format!("failed to append to {}", path.display()))
    }

    /// Appends a batch of records with a single journal fsync; metadata
    /// records go to their key sidecars. Callers own dedup; the incremental
    /// journal path uses this so each turn boundary costs one durable write.
    pub async fn append_records(
        &self,
        session_id: &str,
        records: &[PersistedSessionRecord],
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut envelopes = Vec::new();
        for record in records {
            if let PersistedSessionRecord::Metadata { key, value } = record {
                self.write_metadata_sidecar(session_id, key, value)?;
                continue;
            }
            envelopes.push(SessionRecordEnvelope {
                version: CURRENT_SESSION_ENVELOPE_VERSION,
                session_id: session_id.to_string(),
                record: record.clone(),
            });
        }
        if envelopes.is_empty() {
            return Ok(());
        }
        let path = prepare_storage_path_for_write(&self.root, session_id, "jsonl")?;
        append_json_lines_sync(&path, &envelopes)
            .with_context(|| format!("failed to append to {}", path.display()))
    }

    /// Appends only the non-duplicate suffix of a record batch; metadata
    /// records go to their key sidecars (unchanged values are skipped).
    pub async fn append_batch_dedup(
        &self,
        session_id: &str,
        records: &[PersistedSessionRecord],
    ) -> Result<Vec<PersistedSessionRecord>> {
        if records.is_empty() {
            return Ok(Vec::new());
        }

        let mut written = Vec::new();
        let mut journal_records = Vec::new();
        for record in records {
            if let PersistedSessionRecord::Metadata { key, value } = record {
                if self.write_metadata_sidecar(session_id, key, value)? {
                    written.push(record.clone());
                }
                continue;
            }
            journal_records.push(record.clone());
        }
        if journal_records.is_empty() {
            return Ok(written);
        }

        // Dedup only ever matches a suffix of the existing records against a
        // prefix of the incoming batch, so the last `records.len()` persisted
        // records are enough — reading just the file tail keeps each persist
        // O(batch) instead of re-parsing the whole transcript. Legacy inline
        // metadata records in the tail are filtered out so they cannot break
        // the contiguous suffix match and duplicate events or checkpoints;
        // the window is sized to the unfiltered batch so that filtering never
        // leaves it shorter than the incoming records.
        let existing_records = self
            .load_record_tail(session_id, records.len())
            .await?
            .into_iter()
            .filter(|record| !matches!(record, PersistedSessionRecord::Metadata { .. }))
            .collect::<Vec<_>>();
        let overlap = max_suffix_prefix_overlap(&existing_records, &journal_records);
        let appended = journal_records[overlap..].to_vec();
        if appended.is_empty() {
            return Ok(written);
        }
        let path = prepare_storage_path_for_write(&self.root, session_id, "jsonl")?;
        let envelopes = appended
            .iter()
            .cloned()
            .map(|record| SessionRecordEnvelope {
                version: CURRENT_SESSION_ENVELOPE_VERSION,
                session_id: session_id.to_string(),
                record,
            })
            .collect::<Vec<_>>();
        append_json_lines_sync(&path, &envelopes)
            .with_context(|| format!("failed to append to {}", path.display()))?;
        written.extend(appended);
        Ok(written)
    }

    /// Loads the full persisted session state.
    pub async fn load(&self, session_id: &str) -> Result<StoredSession> {
        self.load_after(session_id, SessionRestoreCursor::default())
            .await
            .map(|(session, _)| session)
    }

    /// Deletes the persisted session transcript file when it exists.
    pub fn delete(&self, session_id: &str) -> Result<()> {
        for path in [
            safe_storage_path(&self.root, session_id, "jsonl"),
            legacy_storage_path(&self.root, session_id, "jsonl")
                .unwrap_or_else(|| safe_storage_path(&self.root, session_id, "jsonl")),
            self.task_archive_path(session_id),
        ] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error)
                    if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to delete session file {}", path.display())
                    });
                }
            }
        }
        let sidecar_dir = self.metadata_sidecar_dir(session_id);
        match fs::remove_dir_all(&sidecar_dir) {
            Ok(()) => {}
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to delete sidecar dir {}", sidecar_dir.display())
                });
            }
        }
        Ok(())
    }

    /// Lists every session identifier that currently has a persisted transcript on disk.
    pub fn list_session_ids(&self) -> Result<Vec<String>> {
        Ok(self
            .session_file_entries()?
            .into_iter()
            .map(|(session_id, _)| session_id)
            .collect())
    }

    /// Lists the session identifiers whose transcript files were modified at or after the given
    /// time.
    pub fn list_session_ids_modified_since(&self, since: SystemTime) -> Result<Vec<String>> {
        let mut session_ids = Vec::new();
        for (session_id, path) in self.session_file_entries()? {
            let mut modified_at = fs::metadata(&path)
                .with_context(|| format!("failed to stat session file {}", path.display()))?
                .modified()
                .with_context(|| {
                    format!("failed to read mtime for session file {}", path.display())
                })?;
            // Metadata-only changes land in the sidecar dir, whose mtime is
            // bumped by each atomic-write rename; without it the boot-time
            // index repair would miss sessions whose only change was state.
            if let Ok(sidecar) = fs::metadata(self.metadata_sidecar_dir(&session_id))
                && let Ok(sidecar_modified_at) = sidecar.modified()
            {
                modified_at = modified_at.max(sidecar_modified_at);
            }
            if modified_at >= since {
                session_ids.push(session_id);
            }
        }
        Ok(session_ids)
    }

    /// Returns the path of the append-only per-session task archive.
    fn task_archive_path(&self, session_id: &str) -> PathBuf {
        safe_storage_path(&self.root, session_id, "tasks-archive.jsonl")
    }

    /// Appends archived task records to the per-session task archive with a
    /// single durable write.
    pub async fn append_task_archive(
        &self,
        session_id: &str,
        entries: &[ArchivedTaskRecord],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let path = self.task_archive_path(session_id);
        self.ensure_parent_dir(&path)?;
        append_json_lines_sync(&path, entries)
            .with_context(|| format!("failed to append to {}", path.display()))
    }

    /// Measures the on-disk footprint of every persisted session. Sizes are
    /// best-effort: a file racing a delete counts as zero.
    pub fn session_storage_sizes(&self) -> Result<Vec<SessionStorageSize>> {
        let file_len = |path: &Path| fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
        let mut sizes = Vec::new();
        for (session_id, journal_path) in self.session_file_entries()? {
            let mut size = SessionStorageSize {
                journal_bytes: file_len(&journal_path),
                task_archive_bytes: file_len(&self.task_archive_path(&session_id)),
                ..SessionStorageSize::default()
            };
            if let Ok(entries) = fs::read_dir(self.metadata_sidecar_dir(&session_id)) {
                for entry in entries.flatten() {
                    size.metadata_bytes += entry.metadata().map(|meta| meta.len()).unwrap_or(0);
                }
            }
            size.session_id = session_id;
            sizes.push(size);
        }
        Ok(sizes)
    }

    /// Loads every archived task record of one session, in archival order.
    ///
    /// Tolerates one torn trailing line (the only corruption an interrupted
    /// append can produce), like the session journal.
    pub async fn load_task_archive(&self, session_id: &str) -> Result<Vec<ArchivedTaskRecord>> {
        let path = self.task_archive_path(session_id);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
            {
                return Ok(Vec::new());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read task archive {}", path.display()));
            }
        };
        let lines = raw
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .collect::<Vec<_>>();
        let last_index = lines.last().map(|(index, _)| *index);
        let mut entries = Vec::with_capacity(lines.len());
        for (index, line) in lines {
            match serde_json::from_str::<ArchivedTaskRecord>(line) {
                Ok(entry) => entries.push(entry),
                Err(error) if Some(index) == last_index => {
                    tracing::warn!(
                        path = %path.display(),
                        line = index + 1,
                        error = %error,
                        "skipping torn trailing line in task archive"
                    );
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "corrupt task archive record at {}:{}",
                            path.display(),
                            index + 1
                        )
                    });
                }
            }
        }
        Ok(entries)
    }

    /// Loads only the records after the provided cursor.
    pub async fn load_after(
        &self,
        session_id: &str,
        cursor: SessionRestoreCursor,
    ) -> Result<(StoredSession, SessionRestoreCursor)> {
        let path = resolve_storage_path_for_read(&self.root, session_id, "jsonl");
        if !path.exists() {
            // Sidecar metadata can exist without journal records (a session
            // configured before its first run); it must still be visible.
            return Ok((
                StoredSession {
                    session_id: session_id.to_string(),
                    metadata: self.read_metadata_sidecars(session_id)?,
                    ..StoredSession::default()
                },
                cursor,
            ));
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read session file {}", path.display()))?;
        let mut session = StoredSession {
            session_id: session_id.to_string(),
            ..StoredSession::default()
        };
        let mut next_cursor = SessionRestoreCursor::default();

        let mut last_seen_offset = cursor.last_offset;
        for (line_index, envelope) in self.parse_session_lines(&path, &raw, cursor.line_count)? {
            next_cursor.line_count = line_index + 1;
            match envelope.record {
                PersistedSessionRecord::Event { entry } => {
                    // Offsets are unique and monotonic per session; an entry at
                    // or below the last accepted offset is a duplicate write
                    // (incremental flush later re-covered by a batched persist)
                    // and must not be replayed twice.
                    if last_seen_offset
                        .map(|last_offset| entry.offset <= last_offset)
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    last_seen_offset = Some(entry.offset);
                    next_cursor.last_offset = Some(entry.offset);
                    session.journal.push(entry);
                }
                PersistedSessionRecord::Checkpoint { checkpoint } => {
                    session.checkpoints.push(checkpoint);
                }
                PersistedSessionRecord::PermissionAudit { audit } => {
                    session.audits.push(audit);
                }
                PersistedSessionRecord::Metadata { key, value } => {
                    session.metadata.insert(key, value);
                }
                PersistedSessionRecord::Output { output } => {
                    session.outputs.push(output);
                }
            }
        }

        // Sidecars hold the latest value per key and win over any inline
        // journal record they superseded.
        for (key, value) in self.read_metadata_sidecars(session_id)? {
            session.metadata.insert(key, value);
        }

        Ok((session, next_cursor))
    }

    /// Loads the latest persisted value for one metadata key without materializing the full session.
    pub async fn load_metadata_value(&self, session_id: &str, key: &str) -> Result<Option<Value>> {
        // The sidecar, when present, is the latest value: every metadata write
        // goes there and wins over older inline journal records.
        if let Some(value) = self.read_metadata_sidecar(session_id, key)? {
            return Ok(Some(value));
        }
        // Legacy fallback: metadata is last-wins per key, so scanning
        // backwards the first match is the latest value and a multi-gigabyte
        // journal costs only a tail read instead of a full parse.
        self.scan_tail_windows(session_id, 32, |records, reached_start| {
            for envelope in records.into_iter().rev() {
                if let PersistedSessionRecord::Metadata {
                    key: record_key,
                    value,
                } = envelope.record
                    && record_key == key
                {
                    return Ok(Some(Some(value)));
                }
            }
            Ok(reached_start.then_some(None))
        })
        .await
        .map(Option::flatten)
    }

    /// Parses every session line, tolerating exactly one torn line at the tail.
    ///
    /// A crash during an append can leave a partial final line; that is the
    /// only corruption shape the appender can produce, so a JSON syntax error
    /// on the last non-empty line is skipped with a warning while the same
    /// error on any earlier line still fails the load. Envelope and migration
    /// errors stay fatal everywhere: a torn line is never valid JSON, so a
    /// well-formed line that fails those checks is real corruption.
    /// Lines before `skip_lines` are counted but not parsed or returned.
    fn parse_session_lines(
        &self,
        path: &Path,
        raw: &str,
        skip_lines: usize,
    ) -> Result<Vec<(usize, SessionRecordEnvelope)>> {
        let mut last_data_line = None;
        for (index, line) in raw.lines().enumerate() {
            if !line.trim().is_empty() {
                last_data_line = Some(index);
            }
        }
        let mut parsed = Vec::new();
        for (line_index, line) in raw.lines().enumerate() {
            if line.trim().is_empty() || line_index < skip_lines {
                continue;
            }
            let value = match serde_json::from_str::<Value>(line) {
                Ok(value) => value,
                Err(error) if Some(line_index) == last_data_line => {
                    tracing::warn!(
                        path = %path.display(),
                        line = line_index + 1,
                        %error,
                        "ignoring torn trailing session record (likely an interrupted append)"
                    );
                    break;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "corrupt session record at {}:{}",
                            path.display(),
                            line_index + 1
                        )
                    });
                }
            };
            parsed.push((line_index, self.upgrade_envelope(value)?));
        }
        Ok(parsed)
    }

    fn upgrade_envelope(&self, mut raw: Value) -> Result<SessionRecordEnvelope> {
        let mut version =
            raw.get("version")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("session envelope missing version"))? as u32;

        while version < CURRENT_SESSION_ENVELOPE_VERSION {
            let migration = self
                .migrations
                .iter()
                .find(|migration| migration.from_version() == version)
                .ok_or_else(|| anyhow!("missing migration for version {version}"))?;
            raw = migration.migrate(raw)?;
            version = migration.to_version();
        }

        serde_json::from_value(raw).map_err(Into::into)
    }

    /// Walks growing tail windows of the journal, each starting on a line
    /// boundary and always extending to the end of the file, until `visit`
    /// yields a result or a window covers the whole file. `visit` receives the
    /// window's parsed records (oldest first) and whether the window reached
    /// the file start; returning `Ok(None)` grows the window.
    async fn scan_tail_windows<T>(
        &self,
        session_id: &str,
        initial_demanded_lines: usize,
        mut visit: impl FnMut(Vec<SessionRecordEnvelope>, bool) -> Result<Option<T>>,
    ) -> Result<Option<T>> {
        use std::io::{Read, Seek, SeekFrom};

        let path = resolve_storage_path_for_read(&self.root, session_id, "jsonl");
        if !path.exists() {
            return Ok(None);
        }
        let file = fs::File::open(&path)
            .with_context(|| format!("failed to read session file {}", path.display()))?;
        let len = file
            .metadata()
            .with_context(|| format!("failed to stat session file {}", path.display()))?
            .len();
        if len == 0 {
            return visit(Vec::new(), true);
        }

        const CHUNK: u64 = 64 * 1024;
        // One newline per record line, plus one for the boundary line we drop
        // and one spare for a torn tail.
        let mut demanded = initial_demanded_lines.saturating_add(2);
        loop {
            let mut window: Vec<u8> = Vec::new();
            let mut newlines = 0usize;
            let mut cursor = len;
            while cursor > 0 && newlines < demanded {
                let start = cursor.saturating_sub(CHUNK);
                let span = (cursor - start) as usize;
                let mut chunk = vec![0u8; span];
                (&file)
                    .seek(SeekFrom::Start(start))
                    .and_then(|_| (&file).read_exact(&mut chunk))
                    .with_context(|| {
                        format!("failed to read tail of session file {}", path.display())
                    })?;
                newlines += chunk.iter().filter(|byte| **byte == b'\n').count();
                chunk.extend_from_slice(&window);
                window = chunk;
                cursor = start;
            }
            let reached_start = cursor == 0;
            let text_start = if reached_start {
                0
            } else {
                // Drop the leading partial line so parsing starts on a record
                // boundary.
                window
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map(|index| index + 1)
                    .unwrap_or(0)
            };
            // Lossy is safe here: a torn tail can split a multi-byte character,
            // and the replacement bytes just make that line unparseable JSON,
            // which the torn-tail tolerance already handles.
            let raw = String::from_utf8_lossy(&window[text_start..]).into_owned();
            let parsed = self.parse_session_lines(&path, &raw, 0).with_context(|| {
                format!(
                    "in the trailing window of session file {} (line numbers are window-relative)",
                    path.display()
                )
            })?;
            let records = parsed
                .into_iter()
                .map(|(_, envelope)| envelope)
                .collect::<Vec<_>>();
            if let Some(result) = visit(records, reached_start)? {
                return Ok(Some(result));
            }
            anyhow::ensure!(
                !reached_start,
                "tail window visitor must resolve once the window covers the whole file"
            );
            demanded = demanded.saturating_mul(2);
        }
    }

    /// Loads at least the last `count` persisted records by reading backwards
    /// from the end of the file, without parsing the whole transcript.
    async fn load_record_tail(
        &self,
        session_id: &str,
        count: usize,
    ) -> Result<Vec<PersistedSessionRecord>> {
        Ok(self
            .scan_tail_windows(session_id, count, move |records, reached_start| {
                if records.len() >= count || reached_start {
                    return Ok(Some(
                        records
                            .into_iter()
                            .map(|envelope| envelope.record)
                            .collect::<Vec<_>>(),
                    ));
                }
                Ok(None)
            })
            .await?
            .unwrap_or_default())
    }

    /// Appends a record inline to the journal, bypassing the metadata sidecar
    /// split, to simulate journals written before sidecars existed.
    #[cfg(test)]
    pub(crate) fn append_inline_for_tests(
        &self,
        session_id: &str,
        record: PersistedSessionRecord,
    ) -> Result<()> {
        let path = prepare_storage_path_for_write(&self.root, session_id, "jsonl")?;
        self.ensure_parent_dir(&path)?;
        append_json_line_sync(
            &path,
            &SessionRecordEnvelope {
                version: CURRENT_SESSION_ENVELOPE_VERSION,
                session_id: session_id.to_string(),
                record,
            },
        )
    }

    #[cfg(test)]
    async fn load_record_sequence(&self, session_id: &str) -> Result<Vec<PersistedSessionRecord>> {
        let path = resolve_storage_path_for_read(&self.root, session_id, "jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read session file {}", path.display()))?;
        Ok(self
            .parse_session_lines(&path, &raw, 0)?
            .into_iter()
            .map(|(_, envelope)| envelope.record)
            .collect())
    }

    fn ensure_parent_dir(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("session path has no parent: {}", path.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
        Ok(())
    }
}

/// In-flight journal entries stream straight into the session file at engine
/// turn boundaries; the end-of-run batched persist then dedups against what
/// is already on disk.
#[async_trait::async_trait]
impl kheish_core::JournalSink for FileSessionStore {
    async fn persist_entries(
        &self,
        conversation: &ConversationKey,
        entries: &[LogEntry],
    ) -> Result<()> {
        let records = entries
            .iter()
            .cloned()
            .map(|entry| PersistedSessionRecord::Event { entry })
            .collect::<Vec<_>>();
        self.append_records(&conversation.session_id, &records)
            .await
    }
}

fn max_suffix_prefix_overlap(
    existing: &[PersistedSessionRecord],
    incoming: &[PersistedSessionRecord],
) -> usize {
    let max_overlap = existing.len().min(incoming.len());
    for overlap in (0..=max_overlap).rev() {
        if existing[existing.len().saturating_sub(overlap)..] == incoming[..overlap] {
            return overlap;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serde_json::json;

    use super::{
        CURRENT_SESSION_ENVELOPE_VERSION, FileSessionStore, PermissionAuditRecord,
        PersistedSessionRecord, SessionMigration, SessionRecordEnvelope, SessionRestoreCursor,
    };
    use crate::{legacy_storage_path, prepare_storage_path_for_write, safe_storage_name};
    use kheish_types::{InputEnvelope, LogEntry, SessionEvent};

    struct RenameMetaMigration;

    impl SessionMigration for RenameMetaMigration {
        fn from_version(&self) -> u32 {
            1
        }

        fn to_version(&self) -> u32 {
            CURRENT_SESSION_ENVELOPE_VERSION
        }

        fn migrate(&self, mut raw: serde_json::Value) -> Result<serde_json::Value> {
            raw["version"] = json!(CURRENT_SESSION_ENVELOPE_VERSION);
            if let Some(key) = raw
                .pointer("/record/key")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
            {
                if key == "legacy_summary" {
                    raw["record"]["key"] = json!("summary");
                }
            }
            Ok(raw)
        }
    }

    #[test]
    fn permission_audit_record_decodes_legacy_records_without_explain_metadata() -> Result<()> {
        let audit: PermissionAuditRecord = serde_json::from_value(json!({
            "scope": "session",
            "tool_name": "bash",
            "tool_call_id": "call-legacy",
            "decision": "ask",
            "justification": null,
            "reason": "shell command requires approval",
            "approval_request_id": "approval-1"
        }))?;

        assert_eq!(audit.base_decision, None);
        assert_eq!(audit.effective_mode, None);
        assert_eq!(audit.mode_effect, None);
        assert_eq!(audit.matched_rule_pattern, None);
        assert_eq!(audit.matched_rule_origin, None);
        assert_eq!(audit.decision, "ask");
        Ok(())
    }

    #[tokio::test]
    async fn session_store_appends_and_loads_records() -> Result<()> {
        let root = std::env::temp_dir().join(format!("kheish-session-{}", std::process::id()));
        let store = FileSessionStore::new(&root);
        let session_id = "session-a";

        store
            .append(
                session_id,
                PersistedSessionRecord::Event {
                    entry: LogEntry {
                        offset: 0,
                        timestamp_ms: 0,
                        event: SessionEvent::InputReceived {
                            input: InputEnvelope::text(
                                "memory", "test", session_id, "user-1", "hello",
                            ),
                        },
                    },
                },
            )
            .await?;
        store
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: "summary".to_string(),
                    value: json!("hello"),
                },
            )
            .await?;

        let loaded = store.load(session_id).await?;
        assert_eq!(loaded.journal.len(), 1);
        assert_eq!(loaded.metadata.get("summary"), Some(&json!("hello")));
        Ok(())
    }

    #[tokio::test]
    async fn session_store_supports_incremental_restore() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("kheish-session-cursor-{}", std::process::id()));
        let store = FileSessionStore::new(&root);
        let session_id = "session-b";

        for offset in 0..3 {
            store
                .append(
                    session_id,
                    PersistedSessionRecord::Event {
                        entry: LogEntry {
                            offset,
                            timestamp_ms: 0,
                            event: SessionEvent::InputReceived {
                                input: InputEnvelope::text(
                                    "memory",
                                    "test",
                                    session_id,
                                    "user-1",
                                    format!("hello-{offset}"),
                                ),
                            },
                        },
                    },
                )
                .await?;
        }

        let (first, cursor) = store
            .load_after(session_id, SessionRestoreCursor::default())
            .await?;
        assert_eq!(first.journal.len(), 3);

        let (second, _) = store.load_after(session_id, cursor).await?;
        assert!(second.journal.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn session_store_applies_migrations() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("kheish-session-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        let store = FileSessionStore::new(&root).with_migration(RenameMetaMigration);
        let path = legacy_storage_path(&root, "session-c", "jsonl").expect("legacy path");
        let legacy = SessionRecordEnvelope {
            version: 1,
            session_id: "session-c".to_string(),
            record: PersistedSessionRecord::Metadata {
                key: "legacy_summary".to_string(),
                value: json!("migrated"),
            },
        };
        std::fs::write(&path, format!("{}\n", serde_json::to_string(&legacy)?))?;

        let loaded = store.load("session-c").await?;
        assert_eq!(loaded.metadata.get("summary"), Some(&json!("migrated")));
        Ok(())
    }

    #[tokio::test]
    async fn session_store_uses_safe_filenames_for_hostile_session_ids() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "../../../etc/passwd";

        store
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: "summary".to_string(),
                    value: json!("safe"),
                },
            )
            .await?;

        let safe_path = store.session_path(session_id);
        assert!(safe_path.starts_with(root.path()));
        assert!(safe_path.exists());
        let expected_name = format!("{}.jsonl", safe_storage_name(session_id));
        assert_eq!(
            safe_path.file_name().and_then(|name| name.to_str()),
            Some(expected_name.as_str())
        );
        assert!(!root.path().join("../../../etc/passwd.jsonl").exists());
        Ok(())
    }

    #[tokio::test]
    async fn session_store_migrates_legacy_path_on_append() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-legacy";
        let legacy_path =
            legacy_storage_path(root.path(), session_id, "jsonl").expect("legacy path");
        let legacy = SessionRecordEnvelope {
            version: CURRENT_SESSION_ENVELOPE_VERSION,
            session_id: session_id.to_string(),
            record: PersistedSessionRecord::Metadata {
                key: "summary".to_string(),
                value: json!("from-legacy"),
            },
        };
        std::fs::write(
            &legacy_path,
            format!("{}\n", serde_json::to_string(&legacy)?),
        )?;

        store
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: "status".to_string(),
                    value: json!("fresh"),
                },
            )
            .await?;

        let safe_path = store.session_path(session_id);
        assert!(safe_path.exists());
        assert!(!legacy_path.exists());
        let loaded = store.load(session_id).await?;
        assert_eq!(loaded.metadata.get("summary"), Some(&json!("from-legacy")));
        assert_eq!(loaded.metadata.get("status"), Some(&json!("fresh")));
        Ok(())
    }

    #[test]
    fn session_store_delete_removes_safe_files() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let path = prepare_storage_path_for_write(root.path(), "session-delete", "jsonl")?;
        std::fs::create_dir_all(path.parent().expect("safe session parent should exist"))?;
        std::fs::write(&path, b"{\"version\":2}\n")?;

        store.delete("session-delete")?;

        assert!(!path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn session_store_tolerates_a_torn_trailing_line() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-torn-tail";

        store
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: "summary".to_string(),
                    value: json!("intact"),
                },
            )
            .await?;
        let path = store.session_path(session_id);
        let mut raw = std::fs::read(&path)?;
        raw.extend_from_slice(br#"{"version":2,"session_id":"session-torn-ta"#);
        std::fs::write(&path, &raw)?;

        let loaded = store.load(session_id).await?;
        assert_eq!(loaded.metadata.get("summary"), Some(&json!("intact")));
        assert_eq!(
            store
                .load_metadata_value(session_id, "summary")
                .await?
                .as_ref(),
            Some(&json!("intact"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_store_still_fails_on_a_torn_middle_line() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-torn-middle";

        store.append_inline_for_tests(
            session_id,
            PersistedSessionRecord::Metadata {
                key: "summary".to_string(),
                value: json!("first"),
            },
        )?;
        let path = store.session_path(session_id);
        let intact = std::fs::read_to_string(&path)?;
        let torn_then_valid = format!("{}{}\n{}", intact, r#"{"version":2,"ses"#, intact.trim());
        std::fs::write(&path, torn_then_valid)?;

        let error = store
            .load(session_id)
            .await
            .expect_err("a torn line before valid records is real corruption");
        assert!(error.to_string().contains("corrupt session record"));
        Ok(())
    }

    #[tokio::test]
    async fn session_store_append_heals_a_torn_trailing_line() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-heal";

        store.append_inline_for_tests(
            session_id,
            PersistedSessionRecord::Metadata {
                key: "summary".to_string(),
                value: json!("first"),
            },
        )?;
        let path = store.session_path(session_id);
        let mut raw = std::fs::read(&path)?;
        raw.extend_from_slice(br#"{"version":2,"torn"#);
        std::fs::write(&path, &raw)?;

        store
            .append(
                session_id,
                PersistedSessionRecord::Event {
                    entry: LogEntry {
                        offset: 0,
                        timestamp_ms: 0,
                        event: SessionEvent::InputReceived {
                            input: InputEnvelope::text(
                                "memory",
                                "test",
                                session_id,
                                "user-1",
                                "appended-after-tear",
                            ),
                        },
                    },
                },
            )
            .await?;

        // The torn fragment is gone and every remaining line parses; nothing
        // was glued onto the partial line.
        let loaded = store.load(session_id).await?;
        assert_eq!(loaded.metadata.get("summary"), Some(&json!("first")));
        assert_eq!(loaded.journal.len(), 1);
        let raw = std::fs::read_to_string(&path)?;
        assert!(raw.ends_with('\n'));
        assert_eq!(raw.lines().count(), 2);
        for line in raw.lines() {
            serde_json::from_str::<serde_json::Value>(line)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn session_store_torn_tail_keeps_restore_cursor_stable() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-torn-cursor";

        for offset in 0..2 {
            store
                .append(
                    session_id,
                    PersistedSessionRecord::Event {
                        entry: LogEntry {
                            offset,
                            timestamp_ms: 0,
                            event: SessionEvent::InputReceived {
                                input: InputEnvelope::text(
                                    "memory",
                                    "test",
                                    session_id,
                                    "user-1",
                                    format!("hello-{offset}"),
                                ),
                            },
                        },
                    },
                )
                .await?;
        }
        let path = store.session_path(session_id);
        let mut raw = std::fs::read(&path)?;
        raw.extend_from_slice(br#"{"version":2,"torn"#);
        std::fs::write(&path, &raw)?;

        let (first, cursor) = store
            .load_after(session_id, SessionRestoreCursor::default())
            .await?;
        assert_eq!(first.journal.len(), 2);
        // The cursor stops at the last intact line, so once the tear is
        // healed by a later append the new record is picked up incrementally.
        assert_eq!(cursor.line_count, 2);

        store
            .append(
                session_id,
                PersistedSessionRecord::Event {
                    entry: LogEntry {
                        offset: 2,
                        timestamp_ms: 0,
                        event: SessionEvent::InputReceived {
                            input: InputEnvelope::text(
                                "memory", "test", session_id, "user-1", "hello-2",
                            ),
                        },
                    },
                },
            )
            .await?;
        let (second, _) = store.load_after(session_id, cursor).await?;
        assert_eq!(second.journal.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn session_store_dedups_only_the_overlapping_prefix_of_a_batch() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("kheish-session-dedup-{}", std::process::id()));
        let store = FileSessionStore::new(&root);
        let session_id = "session-d";
        let event = |offset: u64| PersistedSessionRecord::Event {
            entry: LogEntry {
                offset,
                timestamp_ms: 0,
                event: SessionEvent::InputReceived {
                    input: InputEnvelope::text(
                        "memory",
                        "test",
                        session_id,
                        "user-1",
                        format!("payload-{offset}"),
                    ),
                },
            },
        };
        let first = event(0);
        let second = event(1);
        let third = event(2);

        store
            .append_batch_dedup(session_id, &[first.clone(), second.clone()])
            .await?;
        let appended = store
            .append_batch_dedup(session_id, &[second.clone(), third.clone()])
            .await?;

        assert_eq!(appended, vec![third.clone()]);
        assert_eq!(
            store.load_record_sequence(session_id).await?,
            vec![first, second, third]
        );
        Ok(())
    }

    #[tokio::test]
    async fn load_metadata_value_scans_backwards_and_returns_the_latest_value() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-metadata-backward";

        // Inline journal metadata simulates a pre-sidecar session; the
        // early-only key forces the backward scan to grow its window past the
        // padding until it reaches the file start.
        store.append_inline_for_tests(
            session_id,
            PersistedSessionRecord::Metadata {
                key: "early_only".to_string(),
                value: json!("first-and-only"),
            },
        )?;
        for revision in 0..200 {
            store.append_inline_for_tests(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: "hot_key".to_string(),
                    value: json!({ "revision": revision, "padding": "x".repeat(512) }),
                },
            )?;
        }
        store.append_inline_for_tests(
            session_id,
            PersistedSessionRecord::Metadata {
                key: "tombstoned".to_string(),
                value: json!(null),
            },
        )?;

        let hot = store
            .load_metadata_value(session_id, "hot_key")
            .await?
            .expect("hot key should resolve");
        assert_eq!(hot["revision"], json!(199), "last-wins must be preserved");
        assert_eq!(
            store.load_metadata_value(session_id, "early_only").await?,
            Some(json!("first-and-only"))
        );
        assert_eq!(
            store.load_metadata_value(session_id, "tombstoned").await?,
            Some(json!(null)),
            "an explicit null tombstone must be returned, not skipped"
        );
        assert_eq!(
            store.load_metadata_value(session_id, "missing").await?,
            None
        );
        assert_eq!(
            store
                .load_metadata_value("no-such-session", "hot_key")
                .await?,
            None
        );

        // A sidecar write supersedes every inline value for that key, on both
        // the mono-key path and the full-load overlay.
        store
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: "hot_key".to_string(),
                    value: json!({ "revision": 200 }),
                },
            )
            .await?;
        assert_eq!(
            store.load_metadata_value(session_id, "hot_key").await?,
            Some(json!({ "revision": 200 }))
        );
        assert_eq!(
            store.load(session_id).await?.metadata.get("hot_key"),
            Some(&json!({ "revision": 200 }))
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_store_dedups_against_a_large_transcript_via_the_tail_window() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-large-tail";

        // Enough records to exceed one 64 KiB backward-read chunk, so the
        // dedup path exercises the windowed tail read instead of seeing the
        // whole file in a single chunk.
        let record = |offset: u64| PersistedSessionRecord::Event {
            entry: LogEntry {
                offset,
                timestamp_ms: 0,
                event: SessionEvent::InputReceived {
                    input: InputEnvelope::text(
                        "memory",
                        "test",
                        session_id,
                        "user-1",
                        format!("payload-{offset}-{}", "x".repeat(120)),
                    ),
                },
            },
        };
        let all = (0..500).map(record).collect::<Vec<_>>();
        store.append_batch_dedup(session_id, &all).await?;
        assert!(std::fs::metadata(store.session_path(session_id))?.len() > 64 * 1024);

        // Re-persisting a batch that overlaps the tail appends only the new
        // suffix.
        let batch = vec![record(498), record(499), record(500)];
        let appended = store.append_batch_dedup(session_id, &batch).await?;
        assert_eq!(appended, vec![record(500)]);

        let loaded = store.load(session_id).await?;
        assert_eq!(loaded.journal.len(), 501);
        Ok(())
    }

    #[tokio::test]
    async fn journal_sink_appends_are_deduped_by_the_batched_persist() -> Result<()> {
        use kheish_core::JournalSink as _;

        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-sink";
        let conversation = kheish_types::ConversationKey {
            session_id: session_id.to_string(),
            thread_id: None,
        };
        let entry = |offset: u64| LogEntry {
            offset,
            timestamp_ms: 0,
            event: SessionEvent::InputReceived {
                input: InputEnvelope::text(
                    "memory",
                    "test",
                    session_id,
                    "user-1",
                    format!("payload-{offset}"),
                ),
            },
        };

        // Incremental flushes during the run…
        store
            .persist_entries(&conversation, &[entry(0), entry(1)])
            .await?;
        store.persist_entries(&conversation, &[entry(2)]).await?;
        // …then the end-of-run batch re-sends the same entries.
        let records = (0..3)
            .map(|offset| PersistedSessionRecord::Event {
                entry: entry(offset),
            })
            .collect::<Vec<_>>();
        let appended = store.append_batch_dedup(session_id, &records).await?;
        assert!(
            appended.is_empty(),
            "the batched persist must recognize incrementally flushed entries"
        );

        let loaded = store.load(session_id).await?;
        assert_eq!(loaded.journal.len(), 3);
        assert_eq!(
            loaded
                .journal
                .iter()
                .map(|entry| entry.offset)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        Ok(())
    }

    #[tokio::test]
    async fn load_after_skips_duplicate_event_offsets() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-duplicate-offsets";
        let entry = |offset: u64| PersistedSessionRecord::Event {
            entry: LogEntry {
                offset,
                timestamp_ms: 0,
                event: SessionEvent::InputReceived {
                    input: InputEnvelope::text(
                        "memory",
                        "test",
                        session_id,
                        "user-1",
                        format!("payload-{offset}"),
                    ),
                },
            },
        };

        // A duplicated write (e.g. an interleaved metadata record defeated the
        // suffix dedup) must not replay the same offset twice.
        store
            .append_records(session_id, &[entry(0), entry(1)])
            .await?;
        store
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: "summary".to_string(),
                    value: json!("interleaved"),
                },
            )
            .await?;
        store
            .append_records(session_id, &[entry(1), entry(2)])
            .await?;

        let loaded = store.load(session_id).await?;
        assert_eq!(
            loaded
                .journal
                .iter()
                .map(|entry| entry.offset)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_store_lists_safe_and_legacy_session_ids() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());

        store
            .append(
                "safe/session",
                PersistedSessionRecord::Metadata {
                    key: "summary".to_string(),
                    value: json!("safe"),
                },
            )
            .await?;

        let legacy_path =
            legacy_storage_path(root.path(), "legacy-session", "jsonl").expect("legacy path");
        std::fs::write(
            &legacy_path,
            format!(
                "{}\n",
                serde_json::to_string(&SessionRecordEnvelope {
                    version: CURRENT_SESSION_ENVELOPE_VERSION,
                    session_id: "legacy-session".to_string(),
                    record: PersistedSessionRecord::Metadata {
                        key: "summary".to_string(),
                        value: json!("legacy"),
                    },
                })?
            ),
        )?;

        let session_ids = store.list_session_ids()?;
        assert_eq!(
            session_ids,
            vec!["legacy-session".to_string(), "safe/session".to_string()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn metadata_appends_write_sidecars_instead_of_growing_the_journal() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-sidecar";

        for revision in 0..3 {
            store
                .append(
                    session_id,
                    PersistedSessionRecord::Metadata {
                        key: "hot_key".to_string(),
                        value: json!({ "revision": revision }),
                    },
                )
                .await?;
        }

        // The journal exists (the session is listable) but holds no metadata
        // lines; rewriting one key does not grow it.
        assert!(store.load_record_sequence(session_id).await?.is_empty());
        assert_eq!(std::fs::metadata(store.session_path(session_id))?.len(), 0);
        assert_eq!(store.list_session_ids()?, vec![session_id.to_string()]);
        assert_eq!(
            store.load_metadata_value(session_id, "hot_key").await?,
            Some(json!({ "revision": 2 }))
        );
        assert_eq!(
            store.load(session_id).await?.metadata.get("hot_key"),
            Some(&json!({ "revision": 2 }))
        );

        // Sidecar metadata stays visible even without a journal file: a
        // metadata-first session must never become invisible.
        std::fs::remove_file(store.session_path(session_id))?;
        assert_eq!(
            store.load(session_id).await?.metadata.get("hot_key"),
            Some(&json!({ "revision": 2 }))
        );
        assert_eq!(
            store.load_metadata_value(session_id, "hot_key").await?,
            Some(json!({ "revision": 2 }))
        );
        Ok(())
    }

    #[tokio::test]
    async fn modified_since_sees_sidecar_only_changes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-mtime";

        store
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: "summary".to_string(),
                    value: json!("initial"),
                },
            )
            .await?;

        // Age both the journal and the sidecar dir, then cut after them: the
        // session must drop out of the modified-since view.
        let now = std::time::SystemTime::now();
        let past = now - std::time::Duration::from_secs(600);
        let since = now - std::time::Duration::from_secs(300);
        for path in [
            store.session_path(session_id),
            store.metadata_sidecar_dir(session_id),
        ] {
            std::fs::File::open(&path)?.set_modified(past)?;
        }
        assert!(store.list_session_ids_modified_since(since)?.is_empty());

        // A metadata-only write touches just the sidecar dir; the index
        // repair must still pick the session up.
        store
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: "summary".to_string(),
                    value: json!("updated"),
                },
            )
            .await?;
        assert_eq!(
            store.list_session_ids_modified_since(since)?,
            vec![session_id.to_string()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn mixed_batch_dedup_splits_metadata_without_duplicating_journal_records() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-mixed";
        let event = |offset: u64| PersistedSessionRecord::Event {
            entry: LogEntry {
                offset,
                timestamp_ms: 0,
                event: SessionEvent::InputReceived {
                    input: InputEnvelope::text(
                        "memory",
                        "test",
                        session_id,
                        "user-1",
                        format!("payload-{offset}"),
                    ),
                },
            },
        };
        let audit = PersistedSessionRecord::PermissionAudit {
            audit: PermissionAuditRecord {
                scope: "session".to_string(),
                tool_name: "bash".to_string(),
                tool_call_id: None,
                decision: "allow".to_string(),
                base_decision: None,
                effective_mode: None,
                mode_effect: None,
                matched_rule_pattern: None,
                matched_rule_origin: None,
                justification: None,
                reason: None,
                approval_request_id: None,
            },
        };
        let metadata = PersistedSessionRecord::Metadata {
            key: "hook_state".to_string(),
            value: json!("v1"),
        };

        let first = store
            .append_batch_dedup(
                session_id,
                &[event(0), metadata.clone(), audit.clone(), event(1)],
            )
            .await?;
        assert_eq!(first.len(), 4);

        // The end-of-run batch re-sends the same records plus a new suffix.
        // Interleaved metadata must not defeat the suffix match: audits (like
        // checkpoints) are not deduplicated at read time, so a miss here would
        // persist them twice.
        let second = store
            .append_batch_dedup(
                session_id,
                &[
                    event(0),
                    metadata.clone(),
                    audit.clone(),
                    event(1),
                    event(2),
                ],
            )
            .await?;
        assert_eq!(second, vec![event(2)]);

        let sequence = store.load_record_sequence(session_id).await?;
        assert_eq!(sequence, vec![event(0), audit, event(1), event(2)]);
        assert_eq!(
            store.load_metadata_value(session_id, "hook_state").await?,
            Some(json!("v1"))
        );

        // An identical metadata value is reported as written only once.
        let third = store.append_batch_dedup(session_id, &[metadata]).await?;
        assert!(third.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn task_archive_appends_load_in_order_and_tolerate_a_torn_tail() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-task-archive";
        let entry = |id: &str, archived_at_ms: u64| kheish_types::ArchivedTaskRecord {
            task: kheish_types::TaskRecord {
                id: id.to_string(),
                title: format!("Task {id}"),
                description: String::new(),
                status: kheish_types::TaskStatus::Completed,
                owner_agent_id: None,
                blocked_by: Vec::new(),
                blocks: Vec::new(),
                output: Some("done".to_string()),
                metadata: json!(null),
                created_at_ms: 1,
                updated_at_ms: archived_at_ms,
            },
            archived_at_ms,
            reason: kheish_types::TaskArchiveReason::Terminal,
        };

        assert!(store.load_task_archive(session_id).await?.is_empty());
        store
            .append_task_archive(session_id, &[entry("task-1", 10), entry("task-2", 11)])
            .await?;
        store
            .append_task_archive(session_id, &[entry("task-3", 12)])
            .await?;

        let loaded = store.load_task_archive(session_id).await?;
        assert_eq!(
            loaded
                .iter()
                .map(|e| e.task.id.as_str())
                .collect::<Vec<_>>(),
            vec!["task-1", "task-2", "task-3"]
        );

        // A torn trailing line is skipped; torn data before valid lines fails.
        let path = crate::safe_storage_path(root.path(), session_id, "tasks-archive.jsonl");
        let mut bytes = std::fs::read(&path)?;
        bytes.extend_from_slice(br#"{"task":{"id":"torn"#);
        std::fs::write(&path, &bytes)?;
        assert_eq!(store.load_task_archive(session_id).await?.len(), 3);

        store.delete(session_id)?;
        assert!(!path.exists());
        assert!(store.load_task_archive(session_id).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn session_storage_sizes_cover_journal_sidecars_and_archive() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-sizes";

        store
            .append(
                session_id,
                PersistedSessionRecord::Event {
                    entry: LogEntry {
                        offset: 0,
                        timestamp_ms: 0,
                        event: SessionEvent::InputReceived {
                            input: InputEnvelope::text(
                                "memory", "test", session_id, "user-1", "hello",
                            ),
                        },
                    },
                },
            )
            .await?;
        store
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: "summary".to_string(),
                    value: json!("x".repeat(64)),
                },
            )
            .await?;
        store
            .append_task_archive(
                session_id,
                &[kheish_types::ArchivedTaskRecord {
                    task: kheish_types::TaskRecord {
                        id: "task-1".to_string(),
                        title: "Task".to_string(),
                        description: String::new(),
                        status: kheish_types::TaskStatus::Completed,
                        owner_agent_id: None,
                        blocked_by: Vec::new(),
                        blocks: Vec::new(),
                        output: Some("done".to_string()),
                        metadata: json!(null),
                        created_at_ms: 1,
                        updated_at_ms: 2,
                    },
                    archived_at_ms: 3,
                    reason: kheish_types::TaskArchiveReason::Terminal,
                }],
            )
            .await?;

        let sizes = store.session_storage_sizes()?;
        assert_eq!(sizes.len(), 1);
        let size = &sizes[0];
        assert_eq!(size.session_id, session_id);
        assert!(size.journal_bytes > 0);
        assert!(size.metadata_bytes > 64);
        assert!(size.task_archive_bytes > 0);
        assert_eq!(
            size.total_bytes(),
            size.journal_bytes + size.metadata_bytes + size.task_archive_bytes
        );
        Ok(())
    }

    #[tokio::test]
    async fn delete_purges_metadata_sidecars() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-delete";

        store
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: "summary".to_string(),
                    value: json!("kept"),
                },
            )
            .await?;
        assert!(store.metadata_sidecar_dir(session_id).exists());

        store.delete(session_id)?;
        assert!(!store.metadata_sidecar_dir(session_id).exists());
        assert!(store.load(session_id).await?.metadata.is_empty());
        Ok(())
    }
}
