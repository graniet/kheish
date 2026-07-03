use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use kheish_core::{AgentEngine, LoopPolicy};
use kheish_types::{ConversationKey, LogEntry, SessionCheckpoint};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    append_json_line_sync, append_json_lines_sync, decode_safe_storage_name, legacy_storage_path,
    prepare_storage_path_for_write, resolve_storage_path_for_read, safe_storage_path,
};

/// The current JSONL envelope version stored on disk.
pub const CURRENT_SESSION_ENVELOPE_VERSION: u32 = 2;

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

    /// Appends a single record to the session JSONL file.
    pub async fn append(&self, session_id: &str, record: PersistedSessionRecord) -> Result<()> {
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

    /// Appends a batch of records as-is with a single fsync. Callers own
    /// dedup; the incremental journal path uses this so each turn boundary
    /// costs one durable write.
    pub async fn append_records(
        &self,
        session_id: &str,
        records: &[PersistedSessionRecord],
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let path = prepare_storage_path_for_write(&self.root, session_id, "jsonl")?;
        let envelopes = records
            .iter()
            .cloned()
            .map(|record| SessionRecordEnvelope {
                version: CURRENT_SESSION_ENVELOPE_VERSION,
                session_id: session_id.to_string(),
                record,
            })
            .collect::<Vec<_>>();
        append_json_lines_sync(&path, &envelopes)
            .with_context(|| format!("failed to append to {}", path.display()))
    }

    /// Appends only the non-duplicate suffix of a record batch.
    pub async fn append_batch_dedup(
        &self,
        session_id: &str,
        records: &[PersistedSessionRecord],
    ) -> Result<Vec<PersistedSessionRecord>> {
        if records.is_empty() {
            return Ok(Vec::new());
        }

        // Dedup only ever matches a suffix of the existing records against a
        // prefix of the incoming batch, so the last `records.len()` persisted
        // records are enough — reading just the file tail keeps each persist
        // O(batch) instead of re-parsing the whole transcript.
        let existing_records = self.load_record_tail(session_id, records.len()).await?;
        let overlap = max_suffix_prefix_overlap(&existing_records, records);
        let appended = records[overlap..].to_vec();
        if appended.is_empty() {
            return Ok(appended);
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
        Ok(appended)
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
            let modified_at = fs::metadata(&path)
                .with_context(|| format!("failed to stat session file {}", path.display()))?
                .modified()
                .with_context(|| {
                    format!("failed to read mtime for session file {}", path.display())
                })?;
            if modified_at >= since {
                session_ids.push(session_id);
            }
        }
        Ok(session_ids)
    }

    /// Loads only the records after the provided cursor.
    pub async fn load_after(
        &self,
        session_id: &str,
        cursor: SessionRestoreCursor,
    ) -> Result<(StoredSession, SessionRestoreCursor)> {
        let path = resolve_storage_path_for_read(&self.root, session_id, "jsonl");
        if !path.exists() {
            return Ok((
                StoredSession {
                    session_id: session_id.to_string(),
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

        Ok((session, next_cursor))
    }

    /// Loads the latest persisted value for one metadata key without materializing the full session.
    pub async fn load_metadata_value(&self, session_id: &str, key: &str) -> Result<Option<Value>> {
        let path = resolve_storage_path_for_read(&self.root, session_id, "jsonl");
        if !path.exists() {
            return Ok(None);
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read session file {}", path.display()))?;
        let mut latest = None;
        for (_, envelope) in self.parse_session_lines(&path, &raw, 0)? {
            if let PersistedSessionRecord::Metadata {
                key: record_key,
                value,
            } = envelope.record
                && record_key == key
            {
                latest = Some(value);
            }
        }
        Ok(latest)
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

    /// Loads at least the last `count` persisted records by reading backwards
    /// from the end of the file, without parsing the whole transcript.
    async fn load_record_tail(
        &self,
        session_id: &str,
        count: usize,
    ) -> Result<Vec<PersistedSessionRecord>> {
        use std::io::{Read, Seek, SeekFrom};

        let path = resolve_storage_path_for_read(&self.root, session_id, "jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = fs::File::open(&path)
            .with_context(|| format!("failed to read session file {}", path.display()))?;
        let len = file
            .metadata()
            .with_context(|| format!("failed to stat session file {}", path.display()))?
            .len();
        if len == 0 {
            return Ok(Vec::new());
        }

        const CHUNK: u64 = 64 * 1024;
        // One newline per record line, plus one for the boundary line we drop
        // and one spare for a torn tail.
        let mut demanded = count.saturating_add(2);
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
            if parsed.len() >= count || reached_start {
                return Ok(parsed
                    .into_iter()
                    .map(|(_, envelope)| envelope.record)
                    .collect());
            }
            demanded = demanded.saturating_mul(2);
        }
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

        store
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: "summary".to_string(),
                    value: json!("first"),
                },
            )
            .await?;
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

        store
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: "summary".to_string(),
                    value: json!("first"),
                },
            )
            .await?;
        let path = store.session_path(session_id);
        let mut raw = std::fs::read(&path)?;
        raw.extend_from_slice(br#"{"version":2,"torn"#);
        std::fs::write(&path, &raw)?;

        store
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: "status".to_string(),
                    value: json!("appended-after-tear"),
                },
            )
            .await?;

        // The torn fragment is gone and every remaining line parses; nothing
        // was glued onto the partial line.
        let loaded = store.load(session_id).await?;
        assert_eq!(loaded.metadata.get("summary"), Some(&json!("first")));
        assert_eq!(
            loaded.metadata.get("status"),
            Some(&json!("appended-after-tear"))
        );
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
        let first = PersistedSessionRecord::Metadata {
            key: "summary".to_string(),
            value: json!("first"),
        };
        let second = PersistedSessionRecord::Metadata {
            key: "status".to_string(),
            value: json!("second"),
        };
        let third = PersistedSessionRecord::Metadata {
            key: "tail".to_string(),
            value: json!("third"),
        };

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
}
