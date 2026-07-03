//! Offline compaction of one session journal.
//!
//! Metadata records are last-wins per key, so every superseded snapshot in the
//! journal is dead weight; a session driven by frequent control-state updates
//! can grow to gigabytes of it. Vacuuming rewrites the file keeping every
//! non-metadata record in order and only the final value of each metadata key.
//!
//! The caller owns exclusivity (the CLI acquires the daemon state-root lock):
//! this module never runs concurrently with an appending daemon.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::fs::{sync_parent_dir, temp_path_for};
use crate::resolve_storage_path_for_read;

/// The outcome of one vacuum run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VacuumReport {
    /// The rewritten journal path.
    pub path: PathBuf,
    /// Where the pre-vacuum journal is preserved.
    pub backup_path: PathBuf,
    /// Journal size before the rewrite.
    pub bytes_before: u64,
    /// Journal size after the rewrite.
    pub bytes_after: u64,
    /// Non-metadata records kept, by record type.
    pub kept_by_type: BTreeMap<String, usize>,
    /// Distinct metadata keys kept (one record each).
    pub metadata_keys_kept: usize,
    /// Superseded metadata records dropped.
    pub metadata_records_dropped: usize,
    /// Whether a torn trailing line was dropped.
    pub torn_tail_dropped: bool,
}

/// Streaming summary used to prove the rewrite lost nothing.
#[derive(Debug, Default, PartialEq, Eq)]
struct StreamSummary {
    /// Order-sensitive digest over the raw bytes of non-metadata lines.
    ordered_digest: [u8; 32],
    non_metadata_count: usize,
    /// Last value per metadata key (parsed, so formatting differences never
    /// count as divergence).
    metadata_last: BTreeMap<String, Value>,
}

struct ScannedLine {
    raw: String,
    /// `Some(key)` for metadata records, `None` otherwise.
    metadata_key: Option<String>,
}

/// Iterates complete records of a session journal, tolerating exactly one
/// torn line at the tail (same rule as the store's readers). Invokes the
/// callback for every kept line; returns whether a torn tail was dropped.
fn scan_journal(path: &Path, mut on_line: impl FnMut(ScannedLine) -> Result<()>) -> Result<bool> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut pending: Option<(usize, String)> = None;
    let mut line_number = 0usize;
    loop {
        let mut raw = String::new();
        let read = reader
            .read_line(&mut raw)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        line_number += 1;
        if raw.trim().is_empty() {
            continue;
        }
        // A previously buffered parse failure followed by more data is real
        // corruption, not a torn tail.
        if let Some((failed_line, _)) = pending.take() {
            bail!("corrupt session record at {}:{failed_line}", path.display());
        }
        let trimmed = raw.trim_end_matches(['\n', '\r']);
        match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => {
                let metadata_key = (value.pointer("/record/type").and_then(Value::as_str)
                    == Some("metadata"))
                .then(|| {
                    value
                        .pointer("/record/key")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .flatten();
                on_line(ScannedLine {
                    raw: trimmed.to_string(),
                    metadata_key,
                })?;
            }
            Err(_) => {
                // Only tolerated if this turns out to be the final data line.
                pending = Some((line_number, raw));
            }
        }
    }
    Ok(pending.is_some())
}

fn stream_summary(path: &Path) -> Result<StreamSummary> {
    let mut hasher = Sha256::new();
    let mut summary = StreamSummary::default();
    scan_journal(path, |line| {
        match line.metadata_key {
            Some(key) => {
                let value = serde_json::from_str::<Value>(&line.raw)
                    .expect("scanned line already parsed")
                    .pointer("/record/value")
                    .cloned()
                    .unwrap_or(Value::Null);
                summary.metadata_last.insert(key, value);
            }
            None => {
                hasher.update(line.raw.as_bytes());
                hasher.update(b"\n");
                summary.non_metadata_count += 1;
            }
        }
        Ok(())
    })?;
    summary.ordered_digest = hasher.finalize().into();
    Ok(summary)
}

/// Rewrites the journal of `session_id` under `root`, dropping superseded
/// metadata snapshots. The original file is preserved next to the journal as
/// `<name>.vacuum-bak`; the rewrite is verified against the original before
/// the atomic swap and the whole operation aborts on any divergence.
pub fn vacuum_session(root: &Path, session_id: &str) -> Result<VacuumReport> {
    let path = resolve_storage_path_for_read(root, session_id, "jsonl");
    anyhow::ensure!(
        path.exists(),
        "session {session_id} has no journal under {}",
        root.display()
    );
    vacuum_session_file(&path)
}

/// Same as [`vacuum_session`], for an already resolved journal path.
pub fn vacuum_session_file(path: &Path) -> Result<VacuumReport> {
    let backup_path = path.with_extension("jsonl.vacuum-bak");
    anyhow::ensure!(
        !backup_path.exists(),
        "backup {} already exists; remove it before vacuuming again",
        backup_path.display()
    );
    let bytes_before = fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len();

    // Pass 1: index which line holds the final record of each metadata key.
    let mut last_line_per_key: BTreeMap<String, usize> = BTreeMap::new();
    let mut metadata_records = 0usize;
    let mut data_line = 0usize;
    scan_journal(path, |line| {
        if let Some(key) = line.metadata_key {
            metadata_records += 1;
            last_line_per_key.insert(key, data_line);
        }
        data_line += 1;
        Ok(())
    })?;
    let kept_lines = last_line_per_key.values().copied().collect::<Vec<_>>();

    // Pass 2: stream the rewrite. Non-metadata records keep their order;
    // surviving metadata records are re-emitted at the end, preserving their
    // original relative order (so last-wins stays identical even if a legacy
    // migration later aliases two keys onto one).
    let temp_path = temp_path_for(path)?;
    let result = (|| -> Result<VacuumReport> {
        let temp_file = File::create(&temp_path)
            .with_context(|| format!("failed to create {}", temp_path.display()))?;
        let mut writer = BufWriter::new(temp_file);
        let mut kept_metadata: Vec<(usize, String)> = Vec::new();
        let mut kept_by_type: BTreeMap<String, usize> = BTreeMap::new();
        let mut data_line = 0usize;
        let torn_tail_dropped = scan_journal(path, |line| {
            match line.metadata_key {
                Some(_) => {
                    if kept_lines.contains(&data_line) {
                        kept_metadata.push((data_line, line.raw));
                    }
                }
                None => {
                    let record_type = serde_json::from_str::<Value>(&line.raw)
                        .expect("scanned line already parsed")
                        .pointer("/record/type")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    *kept_by_type.entry(record_type).or_default() += 1;
                    writer.write_all(line.raw.as_bytes())?;
                    writer.write_all(b"\n")?;
                }
            }
            data_line += 1;
            Ok(())
        })?;
        kept_metadata.sort_by_key(|(position, _)| *position);
        let metadata_keys_kept = kept_metadata.len();
        for (_, raw) in kept_metadata {
            writer.write_all(raw.as_bytes())?;
            writer.write_all(b"\n")?;
        }
        let temp_file = writer
            .into_inner()
            .context("failed to flush vacuum output")?;
        temp_file
            .sync_all()
            .with_context(|| format!("failed to sync {}", temp_path.display()))?;
        drop(temp_file);

        // Verify: the rewrite must preserve every non-metadata record in
        // order and the final value of every metadata key.
        let before = stream_summary(path)?;
        let after = stream_summary(&temp_path)?;
        anyhow::ensure!(
            before == after,
            "vacuum verification failed for {}; original left untouched",
            path.display()
        );

        fs::hard_link(path, &backup_path)
            .with_context(|| format!("failed to preserve backup {}", backup_path.display()))?;
        fs::rename(&temp_path, path).with_context(|| {
            format!("failed to replace {} with vacuumed journal", path.display())
        })?;
        sync_parent_dir(path)?;

        let bytes_after = fs::metadata(path)?.len();
        Ok(VacuumReport {
            path: path.to_path_buf(),
            backup_path: backup_path.clone(),
            bytes_before,
            bytes_after,
            kept_by_type,
            metadata_keys_kept,
            metadata_records_dropped: metadata_records.saturating_sub(metadata_keys_kept),
            torn_tail_dropped,
        })
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serde_json::json;

    use super::vacuum_session;
    use crate::store::{FileSessionStore, PersistedSessionRecord};
    use kheish_types::{InputEnvelope, LogEntry, SessionEvent};

    fn event(session_id: &str, offset: u64) -> PersistedSessionRecord {
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
                        format!("payload-{offset}"),
                    ),
                },
            },
        }
    }

    fn metadata(key: &str, value: serde_json::Value) -> PersistedSessionRecord {
        PersistedSessionRecord::Metadata {
            key: key.to_string(),
            value,
        }
    }

    #[tokio::test]
    async fn vacuum_drops_superseded_metadata_and_preserves_load() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-vacuum";

        // Inline metadata simulates the pre-sidecar journals vacuum exists to
        // compact; current writes never add inline metadata.
        store.append(session_id, event(session_id, 0)).await?;
        for revision in 0..5 {
            store.append_inline_for_tests(
                session_id,
                metadata("session_control_state", json!({ "revision": revision })),
            )?;
        }
        store.append(session_id, event(session_id, 1)).await?;
        store.append_inline_for_tests(session_id, metadata("summary", json!("latest")))?;
        store.append_inline_for_tests(session_id, metadata("tombstone", json!(null)))?;

        let before = store.load(session_id).await?;
        let report = vacuum_session(root.path(), session_id)?;
        let after = store.load(session_id).await?;

        assert_eq!(before, after, "vacuum must not change the loaded session");
        assert_eq!(report.metadata_keys_kept, 3);
        assert_eq!(report.metadata_records_dropped, 4);
        assert_eq!(report.kept_by_type.get("event"), Some(&2));
        assert!(report.bytes_after < report.bytes_before);
        assert!(report.backup_path.exists());
        // Null tombstone survives as an explicit last value (the service
        // layer interprets null; the store must not drop the record).
        assert_eq!(
            after.metadata.get("tombstone"),
            Some(&serde_json::Value::Null)
        );
        let raw = std::fs::read_to_string(&report.path)?;
        assert!(raw.contains("\"tombstone\""));
        Ok(())
    }

    #[tokio::test]
    async fn vacuum_tolerates_a_torn_tail_and_rejects_mid_file_corruption() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-vacuum-torn";
        store.append(session_id, event(session_id, 0)).await?;
        let path = store.session_path(session_id);

        // Torn tail: tolerated and dropped.
        let mut bytes = std::fs::read(&path)?;
        bytes.extend_from_slice(br#"{"version":2,"torn"#);
        std::fs::write(&path, &bytes)?;
        let report = vacuum_session(root.path(), session_id)?;
        assert!(report.torn_tail_dropped);
        assert_eq!(store.load(session_id).await?.journal.len(), 1);

        // Mid-file corruption: refused, file untouched.
        std::fs::remove_file(&report.backup_path)?;
        let intact = std::fs::read_to_string(&path)?;
        std::fs::write(&path, format!("{{corrupt}}\n{intact}"))?;
        let error = vacuum_session(root.path(), session_id)
            .expect_err("mid-file corruption must abort the vacuum");
        assert!(error.to_string().contains("corrupt session record"));
        assert!(std::fs::read_to_string(&path)?.starts_with("{corrupt}"));
        Ok(())
    }

    #[tokio::test]
    async fn vacuum_refuses_to_overwrite_an_existing_backup() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileSessionStore::new(root.path());
        let session_id = "session-vacuum-backup";
        store.append(session_id, event(session_id, 0)).await?;

        let first = vacuum_session(root.path(), session_id)?;
        assert!(first.backup_path.exists());
        let error = vacuum_session(root.path(), session_id)
            .expect_err("a stale backup must not be clobbered");
        assert!(error.to_string().contains("already exists"));
        Ok(())
    }
}
