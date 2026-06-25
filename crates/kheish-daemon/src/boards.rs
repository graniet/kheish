//! Durable daemon-owned board records and immutable visual revisions.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use kheish_session::{
    decode_safe_storage_name, prepare_storage_path_for_write, resolve_storage_path_for_read,
    write_json_pretty_atomically,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state_files::read_json_or_quarantine;

const MAX_BOARD_STATE_BYTES: u64 = 1024 * 1024;

/// One compact board summary returned by list APIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardSummaryView {
    /// The stable daemon-owned board identifier.
    pub board_id: String,
    /// The user-visible board name.
    pub display_name: String,
    /// The owning session when this board is session-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    /// The latest immutable revision identifier when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_revision_id: Option<String>,
    /// The current immutable revision count.
    #[serde(default)]
    pub revision_count: u64,
    /// The creation timestamp in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// The last update timestamp in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
}

/// One full board record returned by detail APIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardView {
    /// The compact externally visible board summary.
    #[serde(flatten)]
    pub summary: BoardSummaryView,
    /// Optional caller-supplied metadata stored with the board.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// One immutable board revision returned by detail APIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardRevisionView {
    /// The stable daemon-owned board revision identifier.
    pub revision_id: String,
    /// The owning board identifier.
    pub board_id: String,
    /// The parent revision when this revision extends an existing board.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_revision_id: Option<String>,
    /// Optional caller-scoped idempotency key for this board revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_revision_id: Option<String>,
    /// The daemon-owned rendered asset identifier used for multimodal input.
    pub render_asset_id: String,
    /// The optional daemon-owned structured state asset identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_asset_id: Option<String>,
    /// One optional user-visible note associated with the revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The originating session when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_id: Option<String>,
    /// The originating run when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_run_id: Option<String>,
    /// The creation timestamp in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// Optional caller-supplied metadata stored with the revision.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// Filesystem-backed board storage rooted under one daemon state directory.
#[derive(Clone, Debug)]
pub(crate) struct FileBoardStore {
    root: PathBuf,
}

impl FileBoardStore {
    /// Creates a new board store rooted under one daemon state directory.
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn boards_root(&self) -> PathBuf {
        self.root.join("boards")
    }

    fn revisions_root(&self) -> PathBuf {
        self.root.join("board-revisions")
    }

    fn revision_path(&self, revision_id: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.revisions_root(), revision_id, "json")
    }

    /// Loads every persisted board record, quarantining corrupted files.
    pub(crate) fn load_boards(&self) -> Result<BTreeMap<String, BoardView>> {
        load_json_records::<BoardView>(&self.boards_root(), "board record")
            .map(|records| into_board_map(records, |record| record.summary.board_id.clone()))
    }

    /// Loads every persisted board revision, quarantining corrupted files.
    pub(crate) fn load_revisions(&self) -> Result<BTreeMap<String, BoardRevisionView>> {
        load_json_records::<BoardRevisionView>(&self.revisions_root(), "board revision")
            .map(|records| into_board_map(records, |record| record.revision_id.clone()))
    }

    /// Persists one board record atomically.
    pub(crate) fn save_board(&self, board: &BoardView) -> Result<()> {
        let path =
            prepare_storage_path_for_write(&self.boards_root(), &board.summary.board_id, "json")?;
        write_json_pretty_atomically(&path, board)
    }

    /// Persists one board revision atomically.
    pub(crate) fn save_revision(&self, revision: &BoardRevisionView) -> Result<()> {
        let path =
            prepare_storage_path_for_write(&self.revisions_root(), &revision.revision_id, "json")?;
        write_json_pretty_atomically(&path, revision)
    }

    /// Deletes one persisted board revision when it exists.
    pub(crate) fn delete_revision(&self, revision_id: &str) -> Result<()> {
        delete_if_exists(self.revision_path(revision_id), "board revision")
    }

    /// Removes persisted revisions that reference missing assets or broken parents.
    pub(crate) fn repair_invalid_revisions(
        &self,
        revisions: &mut BTreeMap<String, BoardRevisionView>,
        mut revision_is_valid: impl FnMut(&BoardRevisionView) -> bool,
    ) -> Result<bool> {
        let mut invalid_revisions = revisions
            .values()
            .filter(|revision| !revision_is_valid(revision))
            .map(|revision| (revision.revision_id.clone(), "reference"))
            .collect::<BTreeMap<_, _>>();

        loop {
            let mut changed = false;
            for revision in revisions.values() {
                if invalid_revisions.contains_key(&revision.revision_id) {
                    continue;
                }
                let Some(previous_revision_id) = revision.previous_revision_id.as_deref() else {
                    continue;
                };
                let parent = revisions.get(previous_revision_id);
                if parent.is_none_or(|parent| {
                    parent.board_id != revision.board_id
                        || invalid_revisions.contains_key(&parent.revision_id)
                }) {
                    invalid_revisions.insert(revision.revision_id.clone(), "reference");
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for revision_id in invalid_board_history_revision_ids(revisions, &invalid_revisions) {
            invalid_revisions.entry(revision_id).or_insert("topology");
        }

        let mut changed = false;
        for (revision_id, reason) in invalid_revisions {
            revisions.remove(&revision_id);
            quarantine_invalid_file(self.revision_path(&revision_id), "board revision", reason)?;
            changed = true;
        }
        Ok(changed)
    }

    /// Returns the next numeric board identifier seed.
    pub(crate) fn next_board_seed(&self) -> u64 {
        next_seed_from_root(&self.boards_root(), "board-")
    }

    /// Returns the next numeric board-revision identifier seed.
    pub(crate) fn next_revision_seed(&self) -> u64 {
        next_seed_from_root(&self.revisions_root(), "board-revision-")
    }

    /// Repairs loaded board summaries from immutable revision history.
    pub(crate) fn repair_boards_from_revisions(
        &self,
        boards: &mut BTreeMap<String, BoardView>,
        revisions: &BTreeMap<String, BoardRevisionView>,
    ) -> Result<bool> {
        let mut changed = false;
        let mut revisions_by_board = BTreeMap::<String, Vec<&BoardRevisionView>>::new();
        for revision in revisions.values() {
            revisions_by_board
                .entry(revision.board_id.clone())
                .or_default()
                .push(revision);
        }

        for board in boards.values_mut() {
            if revisions_by_board.contains_key(&board.summary.board_id) {
                continue;
            }
            if board.summary.latest_revision_id.is_none() && board.summary.revision_count == 0 {
                continue;
            }
            board.summary.latest_revision_id = None;
            board.summary.revision_count = 0;
            board.summary.updated_at_ms =
                board.summary.updated_at_ms.max(board.summary.created_at_ms);
            self.save_board(board)?;
            changed = true;
        }

        for (board_id, records) in revisions_by_board {
            let history = linear_board_history(&records).ok_or_else(|| {
                anyhow!(
                    "non-linear board revision history for {board_id}; run revision repair before summary repair"
                )
            })?;
            let next_latest = history.last().map(|record| record.revision_id.clone());
            let next_count = records.len() as u64;
            let next_created_at = history
                .first()
                .map(|record| record.created_at_ms)
                .unwrap_or_default();
            let next_updated_at = history
                .iter()
                .map(|record| record.created_at_ms)
                .max()
                .unwrap_or(next_created_at);
            let next_owner_session_id = history
                .first()
                .and_then(|record| record.source_session_id.clone());
            if let Some(board) = boards.get_mut(&board_id) {
                let next_updated_at = next_updated_at.max(board.summary.created_at_ms);
                if board.summary.latest_revision_id != next_latest
                    || board.summary.revision_count != next_count
                    || board.summary.updated_at_ms != next_updated_at
                {
                    board.summary.latest_revision_id = next_latest;
                    board.summary.revision_count = next_count;
                    board.summary.updated_at_ms = next_updated_at;
                    self.save_board(board)?;
                    changed = true;
                }
                continue;
            }

            let board = BoardView {
                summary: BoardSummaryView {
                    board_id: board_id.clone(),
                    display_name: board_id.clone(),
                    owner_session_id: next_owner_session_id,
                    latest_revision_id: next_latest,
                    revision_count: next_count,
                    created_at_ms: next_created_at,
                    updated_at_ms: next_updated_at,
                },
                metadata: Value::Null,
            };
            self.save_board(&board)?;
            boards.insert(board_id, board);
            changed = true;
        }

        Ok(changed)
    }
}

fn invalid_board_history_revision_ids(
    revisions: &BTreeMap<String, BoardRevisionView>,
    invalid_revisions: &BTreeMap<String, &'static str>,
) -> BTreeSet<String> {
    let mut revisions_by_board = BTreeMap::<String, Vec<&BoardRevisionView>>::new();
    for revision in revisions.values() {
        if invalid_revisions.contains_key(&revision.revision_id) {
            continue;
        }
        revisions_by_board
            .entry(revision.board_id.clone())
            .or_default()
            .push(revision);
    }

    revisions_by_board
        .into_values()
        .filter(|records| linear_board_history(records).is_none())
        .flat_map(|records| {
            records
                .into_iter()
                .map(|revision| revision.revision_id.clone())
        })
        .collect()
}

fn linear_board_history<'a>(
    records: &[&'a BoardRevisionView],
) -> Option<Vec<&'a BoardRevisionView>> {
    if records.is_empty() {
        return Some(Vec::new());
    }

    let records_by_id = records
        .iter()
        .map(|record| (record.revision_id.as_str(), *record))
        .collect::<BTreeMap<_, _>>();
    let mut roots = Vec::<&BoardRevisionView>::new();
    let mut children_by_parent = BTreeMap::<&str, Vec<&BoardRevisionView>>::new();
    for record in records {
        if let Some(parent_id) = record.previous_revision_id.as_deref() {
            let parent = records_by_id.get(parent_id)?;
            if parent.board_id != record.board_id {
                return None;
            }
            children_by_parent
                .entry(parent_id)
                .or_default()
                .push(*record);
        } else {
            roots.push(*record);
        }
    }

    if roots.len() != 1
        || children_by_parent
            .values()
            .any(|children| children.len() != 1)
    {
        return None;
    }

    let mut ordered = Vec::with_capacity(records.len());
    let mut visited = BTreeSet::<&str>::new();
    let mut current = roots[0];
    loop {
        if !visited.insert(current.revision_id.as_str()) {
            return None;
        }
        ordered.push(current);
        let Some(children) = children_by_parent.get(current.revision_id.as_str()) else {
            break;
        };
        current = children[0];
    }

    if ordered.len() == records.len() {
        Some(ordered)
    } else {
        None
    }
}

fn delete_if_exists(path: PathBuf, label: &str) -> Result<()> {
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to delete {label} {}", path.display()))
        }
    }
}

fn quarantine_invalid_file(path: PathBuf, label: &str, reason: &str) -> Result<()> {
    match fs::rename(&path, invalid_state_path(&path, reason)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to quarantine {label} {}", path.display()))
        }
    }
}

fn invalid_state_path(path: &std::path::Path, reason: &str) -> PathBuf {
    let timestamp_ms = crate::now_ms();
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("board-revision.json");
    path.with_file_name(format!("{file_name}.invalid-{reason}-{timestamp_ms}"))
}

fn load_json_records<T>(root: &PathBuf, label: &'static str) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for scan_root in storage_scan_roots(root) {
        if !scan_root.exists() {
            continue;
        }
        for entry in fs::read_dir(scan_root)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(record) = read_json_or_quarantine::<T>(&path, label)? {
                records.push(record);
            }
        }
    }
    Ok(records)
}

fn into_board_map<T>(mut records: Vec<T>, key: impl Fn(&T) -> String) -> BTreeMap<String, T> {
    records.sort_by(|left, right| key(left).cmp(&key(right)));
    let mut map = BTreeMap::new();
    for record in records {
        map.insert(key(&record), record);
    }
    map
}

pub(crate) fn validate_board_state_payload(
    board_id: &str,
    previous_revision_id: Option<&str>,
    bytes: &[u8],
) -> Result<()> {
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_BOARD_STATE_BYTES,
        "board state asset exceeds {MAX_BOARD_STATE_BYTES} bytes"
    );
    let value: Value = serde_json::from_slice(bytes)
        .with_context(|| "board state asset is not valid JSON".to_string())?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("board state asset must be a JSON object"))?;
    let schema_version = object
        .get("schema_version")
        .ok_or_else(|| anyhow!("board state asset requires schema_version"))?;
    anyhow::ensure!(
        board_state_schema_version_is_supported(schema_version),
        "board state asset has unsupported schema_version {schema_version}"
    );
    if let Some(state_board_id) = object.get("board_id") {
        anyhow::ensure!(
            state_board_id.as_str() == Some(board_id),
            "board state asset board_id must match {board_id}"
        );
    }
    if let Some(state_previous_revision_id) = object.get("previous_revision_id") {
        let expected = previous_revision_id;
        let actual = state_previous_revision_id.as_str();
        anyhow::ensure!(
            (state_previous_revision_id.is_null() && expected.is_none()) || actual == expected,
            "board state asset previous_revision_id must match submitted previous_revision_id"
        );
    }
    for field in ["elements", "strokes", "layers"] {
        if let Some(value) = object.get(field) {
            anyhow::ensure!(
                value.is_array(),
                "board state asset field {field} must be an array"
            );
        }
    }
    if let Some(value) = object.get("assets") {
        anyhow::ensure!(
            value.is_array() || value.is_object(),
            "board state asset field assets must be an array or object"
        );
    }
    if let Some(canvas) = object.get("canvas") {
        validate_board_state_canvas(canvas)?;
    }
    anyhow::ensure!(
        [
            "canvas", "elements", "strokes", "layers", "assets", "metadata", "payload", "note"
        ]
        .iter()
        .any(|field| object.contains_key(*field)),
        "board state asset must include board content"
    );
    Ok(())
}

pub(crate) fn board_state_payload_asset_ids(bytes: &[u8]) -> Result<BTreeSet<String>> {
    let value: Value = serde_json::from_slice(bytes)
        .with_context(|| "board state asset is not valid JSON".to_string())?;
    let mut asset_ids = BTreeSet::new();
    if let Some(assets) = value.as_object().and_then(|object| object.get("assets")) {
        collect_board_state_asset_ids(assets, &mut asset_ids);
    }
    collect_known_board_state_asset_refs(&value, &mut asset_ids);
    Ok(asset_ids)
}

fn board_state_schema_version_is_supported(value: &Value) -> bool {
    value.as_u64() == Some(1)
        || value
            .as_str()
            .is_some_and(|value| matches!(value, "1" | "kheish.board_state.v1"))
}

fn collect_board_state_asset_ids(value: &Value, asset_ids: &mut BTreeSet<String>) {
    match value {
        Value::String(value) => {
            if board_state_asset_id_is_daemon_asset(value) {
                asset_ids.insert(value.to_string());
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_board_state_asset_ids(value, asset_ids);
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                if board_state_asset_id_is_daemon_asset(key) {
                    asset_ids.insert(key.to_string());
                }
                collect_board_state_asset_ids(value, asset_ids);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn collect_known_board_state_asset_refs(value: &Value, asset_ids: &mut BTreeSet<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_known_board_state_asset_refs(value, asset_ids);
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                if board_state_asset_reference_key(key) {
                    collect_board_state_asset_ids(value, asset_ids);
                }
                collect_known_board_state_asset_refs(value, asset_ids);
            }
        }
        Value::String(_) | Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn board_state_asset_reference_key(key: &str) -> bool {
    matches!(
        key,
        "asset_id"
            | "asset_ids"
            | "image_asset_id"
            | "image_asset_ids"
            | "render_asset_id"
            | "source_asset_id"
            | "source_asset_ids"
            | "mask_asset_id"
    )
}

fn board_state_asset_id_is_daemon_asset(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix("asset-") else {
        return false;
    };
    !suffix.is_empty() && suffix.chars().all(|character| character.is_ascii_digit())
}

fn validate_board_state_canvas(value: &Value) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("board state asset field canvas must be an object"))?;
    for field in ["width", "height"] {
        let Some(dimension) = object.get(field) else {
            continue;
        };
        let Some(dimension) = dimension.as_u64() else {
            bail!("board state asset canvas.{field} must be a positive integer");
        };
        anyhow::ensure!(
            (1..=100_000).contains(&dimension),
            "board state asset canvas.{field} must be between 1 and 100000"
        );
    }
    Ok(())
}

fn next_seed_from_root(root: &PathBuf, prefix: &str) -> u64 {
    storage_scan_roots(root)
        .into_iter()
        .filter_map(|scan_root| fs::read_dir(scan_root).ok())
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| storage_identifier_from_path(&entry.path()))
        .filter_map(|identifier| {
            identifier
                .strip_prefix(prefix)
                .and_then(|suffix| suffix.parse::<u64>().ok())
        })
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

fn storage_scan_roots(root: &PathBuf) -> [PathBuf; 2] {
    [root.clone(), root.join("__safe")]
}

fn storage_identifier_from_path(path: &std::path::Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let stem = stem.split(".corrupt-").next().unwrap_or(stem);
    let identifier = stem.strip_suffix(".json").unwrap_or(stem);
    decode_safe_storage_name(identifier).or_else(|| Some(identifier.to_string()))
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn test_revision(
        revision_id: &str,
        board_id: &str,
        previous_revision_id: Option<&str>,
        created_at_ms: u64,
    ) -> BoardRevisionView {
        BoardRevisionView {
            revision_id: revision_id.to_string(),
            board_id: board_id.to_string(),
            previous_revision_id: previous_revision_id.map(str::to_string),
            client_revision_id: None,
            render_asset_id: format!("asset-render-{revision_id}"),
            state_asset_id: None,
            note: None,
            source_session_id: Some("session-1".to_string()),
            source_run_id: None,
            created_at_ms,
            metadata: Value::Null,
        }
    }

    #[test]
    fn repair_boards_from_revisions_updates_latest_pointer_and_count() -> Result<()> {
        let temp = tempdir()?;
        let store = FileBoardStore::new(temp.path());
        let mut boards = BTreeMap::from([(
            "board-1".to_string(),
            BoardView {
                summary: BoardSummaryView {
                    board_id: "board-1".to_string(),
                    display_name: "Sketch".to_string(),
                    owner_session_id: Some("session-1".to_string()),
                    latest_revision_id: None,
                    revision_count: 0,
                    created_at_ms: 10,
                    updated_at_ms: 10,
                },
                metadata: json!({}),
            },
        )]);
        let revisions = BTreeMap::from([
            (
                "board-revision-1".to_string(),
                BoardRevisionView {
                    revision_id: "board-revision-1".to_string(),
                    board_id: "board-1".to_string(),
                    previous_revision_id: None,
                    client_revision_id: None,
                    render_asset_id: "asset-1".to_string(),
                    state_asset_id: Some("asset-2".to_string()),
                    note: Some("first".to_string()),
                    source_session_id: Some("session-1".to_string()),
                    source_run_id: None,
                    created_at_ms: 20,
                    metadata: Value::Null,
                },
            ),
            (
                "board-revision-2".to_string(),
                BoardRevisionView {
                    revision_id: "board-revision-2".to_string(),
                    board_id: "board-1".to_string(),
                    previous_revision_id: Some("board-revision-1".to_string()),
                    client_revision_id: None,
                    render_asset_id: "asset-3".to_string(),
                    state_asset_id: Some("asset-4".to_string()),
                    note: Some("second".to_string()),
                    source_session_id: Some("session-1".to_string()),
                    source_run_id: Some("run-1".to_string()),
                    created_at_ms: 30,
                    metadata: Value::Null,
                },
            ),
        ]);

        assert!(store.repair_boards_from_revisions(&mut boards, &revisions)?);
        let repaired = boards.get("board-1").expect("board");
        assert_eq!(
            repaired.summary.latest_revision_id.as_deref(),
            Some("board-revision-2")
        );
        assert_eq!(repaired.summary.revision_count, 2);
        assert_eq!(repaired.summary.updated_at_ms, 30);
        Ok(())
    }

    #[test]
    fn repair_boards_from_revisions_uses_chain_tip_when_timestamps_are_skewed() -> Result<()> {
        let temp = tempdir()?;
        let store = FileBoardStore::new(temp.path());
        let mut boards = BTreeMap::from([(
            "board-skewed".to_string(),
            BoardView {
                summary: BoardSummaryView {
                    board_id: "board-skewed".to_string(),
                    display_name: "Skewed".to_string(),
                    owner_session_id: Some("session-1".to_string()),
                    latest_revision_id: Some("board-revision-root".to_string()),
                    revision_count: 1,
                    created_at_ms: 10,
                    updated_at_ms: 200,
                },
                metadata: Value::Null,
            },
        )]);
        let root = test_revision("board-revision-root", "board-skewed", None, 200);
        let child = test_revision(
            "board-revision-child",
            "board-skewed",
            Some("board-revision-root"),
            30,
        );
        let revisions = BTreeMap::from([
            (root.revision_id.clone(), root),
            (child.revision_id.clone(), child),
        ]);

        assert!(store.repair_boards_from_revisions(&mut boards, &revisions)?);
        let repaired = boards.get("board-skewed").expect("board");
        assert_eq!(
            repaired.summary.latest_revision_id.as_deref(),
            Some("board-revision-child")
        );
        assert_eq!(repaired.summary.revision_count, 2);
        assert_eq!(repaired.summary.updated_at_ms, 200);
        Ok(())
    }

    #[test]
    fn repair_boards_from_revisions_reconstructs_missing_board_records() -> Result<()> {
        let temp = tempdir()?;
        let store = FileBoardStore::new(temp.path());
        let mut boards = BTreeMap::new();
        let revisions = BTreeMap::from([
            (
                "board-revision-1".to_string(),
                BoardRevisionView {
                    revision_id: "board-revision-1".to_string(),
                    board_id: "board-7".to_string(),
                    previous_revision_id: None,
                    client_revision_id: None,
                    render_asset_id: "asset-1".to_string(),
                    state_asset_id: None,
                    note: Some("seed".to_string()),
                    source_session_id: Some("session-1".to_string()),
                    source_run_id: None,
                    created_at_ms: 40,
                    metadata: json!({"kind": "whiteboard"}),
                },
            ),
            (
                "board-revision-2".to_string(),
                BoardRevisionView {
                    revision_id: "board-revision-2".to_string(),
                    board_id: "board-7".to_string(),
                    previous_revision_id: Some("board-revision-1".to_string()),
                    client_revision_id: None,
                    render_asset_id: "asset-2".to_string(),
                    state_asset_id: Some("asset-3".to_string()),
                    note: Some("second".to_string()),
                    source_session_id: Some("session-1".to_string()),
                    source_run_id: Some("run-2".to_string()),
                    created_at_ms: 55,
                    metadata: Value::Null,
                },
            ),
        ]);

        assert!(store.repair_boards_from_revisions(&mut boards, &revisions)?);
        let reconstructed = boards.get("board-7").expect("reconstructed board");
        assert_eq!(reconstructed.summary.display_name, "board-7");
        assert_eq!(
            reconstructed.summary.owner_session_id.as_deref(),
            Some("session-1")
        );
        assert_eq!(reconstructed.summary.created_at_ms, 40);
        assert_eq!(reconstructed.summary.updated_at_ms, 55);
        assert_eq!(reconstructed.summary.revision_count, 2);
        assert_eq!(
            reconstructed.summary.latest_revision_id.as_deref(),
            Some("board-revision-2")
        );
        assert_eq!(reconstructed.metadata, Value::Null);
        Ok(())
    }

    #[test]
    fn load_boards_and_revisions_reads_safe_storage_namespace() -> Result<()> {
        let temp = tempdir()?;
        let store = FileBoardStore::new(temp.path());
        let board = BoardView {
            summary: BoardSummaryView {
                board_id: "board-safe".to_string(),
                display_name: "Safe Board".to_string(),
                owner_session_id: Some("session-safe".to_string()),
                latest_revision_id: Some("board-revision-safe".to_string()),
                revision_count: 1,
                created_at_ms: 10,
                updated_at_ms: 20,
            },
            metadata: json!({"kind": "whiteboard"}),
        };
        let revision = BoardRevisionView {
            revision_id: "board-revision-safe".to_string(),
            board_id: "board-safe".to_string(),
            previous_revision_id: None,
            client_revision_id: None,
            render_asset_id: "asset-render".to_string(),
            state_asset_id: Some("asset-state".to_string()),
            note: Some("seed".to_string()),
            source_session_id: Some("session-safe".to_string()),
            source_run_id: Some("run-safe".to_string()),
            created_at_ms: 20,
            metadata: Value::Null,
        };

        store.save_board(&board)?;
        store.save_revision(&revision)?;

        let boards = store.load_boards()?;
        let revisions = store.load_revisions()?;
        assert_eq!(boards.get("board-safe"), Some(&board));
        assert_eq!(revisions.get("board-revision-safe"), Some(&revision));
        Ok(())
    }

    #[test]
    fn next_seed_from_root_scans_safe_storage_namespace() -> Result<()> {
        let temp = tempdir()?;
        let store = FileBoardStore::new(temp.path());
        let board = BoardView {
            summary: BoardSummaryView {
                board_id: "board-41".to_string(),
                display_name: "Seed".to_string(),
                owner_session_id: None,
                latest_revision_id: None,
                revision_count: 0,
                created_at_ms: 1,
                updated_at_ms: 1,
            },
            metadata: Value::Null,
        };
        let revision = BoardRevisionView {
            revision_id: "board-revision-17".to_string(),
            board_id: "board-41".to_string(),
            previous_revision_id: None,
            client_revision_id: None,
            render_asset_id: "asset-render".to_string(),
            state_asset_id: None,
            note: None,
            source_session_id: None,
            source_run_id: None,
            created_at_ms: 2,
            metadata: Value::Null,
        };

        store.save_board(&board)?;
        store.save_revision(&revision)?;

        assert_eq!(store.next_board_seed(), 42);
        assert_eq!(store.next_revision_seed(), 18);
        Ok(())
    }

    #[test]
    fn repair_boards_from_revisions_clears_stale_revision_summary_when_history_is_missing()
    -> Result<()> {
        let temp = tempdir()?;
        let store = FileBoardStore::new(temp.path());
        let mut boards = BTreeMap::from([(
            "board-stale".to_string(),
            BoardView {
                summary: BoardSummaryView {
                    board_id: "board-stale".to_string(),
                    display_name: "Stale Board".to_string(),
                    owner_session_id: Some("session-1".to_string()),
                    latest_revision_id: Some("board-revision-missing".to_string()),
                    revision_count: 1,
                    created_at_ms: 10,
                    updated_at_ms: 20,
                },
                metadata: Value::Null,
            },
        )]);
        let revisions = BTreeMap::new();

        assert!(store.repair_boards_from_revisions(&mut boards, &revisions)?);
        let repaired = boards.get("board-stale").expect("stale board");
        assert!(repaired.summary.latest_revision_id.is_none());
        assert_eq!(repaired.summary.revision_count, 0);
        assert_eq!(repaired.summary.updated_at_ms, 20);
        Ok(())
    }

    #[test]
    fn repair_invalid_revisions_quarantines_missing_asset_revision_records() -> Result<()> {
        let temp = tempdir()?;
        let store = FileBoardStore::new(temp.path());
        let valid = BoardRevisionView {
            revision_id: "board-revision-valid".to_string(),
            board_id: "board-1".to_string(),
            previous_revision_id: None,
            client_revision_id: None,
            render_asset_id: "asset-render-valid".to_string(),
            state_asset_id: Some("asset-state-valid".to_string()),
            note: None,
            source_session_id: None,
            source_run_id: None,
            created_at_ms: 10,
            metadata: Value::Null,
        };
        let missing = BoardRevisionView {
            revision_id: "board-revision-missing".to_string(),
            board_id: "board-1".to_string(),
            previous_revision_id: Some(valid.revision_id.clone()),
            client_revision_id: None,
            render_asset_id: "asset-render-missing".to_string(),
            state_asset_id: None,
            note: None,
            source_session_id: None,
            source_run_id: None,
            created_at_ms: 20,
            metadata: Value::Null,
        };
        store.save_revision(&valid)?;
        store.save_revision(&missing)?;
        let missing_path = store.revision_path(&missing.revision_id);
        let mut revisions = BTreeMap::from([
            (valid.revision_id.clone(), valid.clone()),
            (missing.revision_id.clone(), missing.clone()),
        ]);

        assert!(store.repair_invalid_revisions(&mut revisions, |revision| {
            revision.render_asset_id == "asset-render-valid"
                && revision
                    .state_asset_id
                    .as_deref()
                    .is_none_or(|state_asset_id| state_asset_id == "asset-state-valid")
        })?);

        assert_eq!(revisions.len(), 1);
        assert_eq!(revisions.get(&valid.revision_id), Some(&valid));
        assert!(!missing_path.exists());
        let quarantined = fs::read_dir(missing_path.parent().expect("revision parent"))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .any(|file_name| file_name.contains(".invalid-reference-"));
        assert!(quarantined, "missing revision record should be quarantined");
        Ok(())
    }

    #[test]
    fn repair_invalid_revisions_quarantines_descendants_of_removed_revisions() -> Result<()> {
        let temp = tempdir()?;
        let store = FileBoardStore::new(temp.path());
        let root = BoardRevisionView {
            revision_id: "board-revision-root".to_string(),
            board_id: "board-1".to_string(),
            previous_revision_id: None,
            client_revision_id: None,
            render_asset_id: "asset-root".to_string(),
            state_asset_id: None,
            note: None,
            source_session_id: None,
            source_run_id: None,
            created_at_ms: 10,
            metadata: Value::Null,
        };
        let missing_parent = BoardRevisionView {
            revision_id: "board-revision-missing-parent".to_string(),
            board_id: "board-1".to_string(),
            previous_revision_id: Some(root.revision_id.clone()),
            client_revision_id: None,
            render_asset_id: "asset-missing".to_string(),
            state_asset_id: None,
            note: None,
            source_session_id: None,
            source_run_id: None,
            created_at_ms: 20,
            metadata: Value::Null,
        };
        let descendant = BoardRevisionView {
            revision_id: "board-revision-descendant".to_string(),
            board_id: "board-1".to_string(),
            previous_revision_id: Some(missing_parent.revision_id.clone()),
            client_revision_id: None,
            render_asset_id: "asset-descendant".to_string(),
            state_asset_id: None,
            note: None,
            source_session_id: None,
            source_run_id: None,
            created_at_ms: 30,
            metadata: Value::Null,
        };
        for revision in [&root, &missing_parent, &descendant] {
            store.save_revision(revision)?;
        }
        let mut revisions = BTreeMap::from([
            (root.revision_id.clone(), root.clone()),
            (missing_parent.revision_id.clone(), missing_parent.clone()),
            (descendant.revision_id.clone(), descendant.clone()),
        ]);

        assert!(store.repair_invalid_revisions(&mut revisions, |revision| {
            matches!(
                revision.render_asset_id.as_str(),
                "asset-root" | "asset-descendant"
            )
        })?);

        assert_eq!(revisions.len(), 1);
        assert_eq!(revisions.get(&root.revision_id), Some(&root));
        Ok(())
    }

    #[test]
    fn repair_invalid_revisions_quarantines_forked_histories() -> Result<()> {
        let temp = tempdir()?;
        let store = FileBoardStore::new(temp.path());
        let root = test_revision("board-revision-root", "board-1", None, 10);
        let left = test_revision(
            "board-revision-left",
            "board-1",
            Some("board-revision-root"),
            20,
        );
        let right = test_revision(
            "board-revision-right",
            "board-1",
            Some("board-revision-root"),
            30,
        );
        for revision in [&root, &left, &right] {
            store.save_revision(revision)?;
        }
        let revision_parent = store
            .revision_path(&root.revision_id)
            .parent()
            .expect("revision parent")
            .to_path_buf();
        let mut revisions = BTreeMap::from([
            (root.revision_id.clone(), root),
            (left.revision_id.clone(), left),
            (right.revision_id.clone(), right),
        ]);

        assert!(store.repair_invalid_revisions(&mut revisions, |_| true)?);

        assert!(revisions.is_empty());
        let topology_quarantines = fs::read_dir(revision_parent)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|file_name| file_name.contains(".invalid-topology-"))
            .count();
        assert_eq!(topology_quarantines, 3);
        Ok(())
    }

    #[test]
    fn repair_invalid_revisions_quarantines_multiple_roots_and_cycles() -> Result<()> {
        let temp = tempdir()?;
        let store = FileBoardStore::new(temp.path());
        let first_root = test_revision("board-revision-root-1", "board-roots", None, 10);
        let second_root = test_revision("board-revision-root-2", "board-roots", None, 20);
        let cycle_a = test_revision(
            "board-revision-cycle-a",
            "board-cycle",
            Some("board-revision-cycle-b"),
            30,
        );
        let cycle_b = test_revision(
            "board-revision-cycle-b",
            "board-cycle",
            Some("board-revision-cycle-a"),
            40,
        );
        for revision in [&first_root, &second_root, &cycle_a, &cycle_b] {
            store.save_revision(revision)?;
        }
        let revision_parent = store
            .revision_path(&first_root.revision_id)
            .parent()
            .expect("revision parent")
            .to_path_buf();
        let mut revisions = BTreeMap::from([
            (first_root.revision_id.clone(), first_root),
            (second_root.revision_id.clone(), second_root),
            (cycle_a.revision_id.clone(), cycle_a),
            (cycle_b.revision_id.clone(), cycle_b),
        ]);

        assert!(store.repair_invalid_revisions(&mut revisions, |_| true)?);

        assert!(revisions.is_empty());
        let topology_quarantines = fs::read_dir(revision_parent)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|file_name| file_name.contains(".invalid-topology-"))
            .count();
        assert_eq!(topology_quarantines, 4);
        Ok(())
    }

    #[test]
    fn board_state_payload_asset_ids_extracts_daemon_asset_references() -> Result<()> {
        let asset_ids = board_state_payload_asset_ids(
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "canvas": {"width": 800, "height": 600},
                "assets": [
                    "asset-7",
                    {"asset_id": "asset-8"},
                    {"asset-9": {"role": "layer"}},
                    {"nested": [{"id": "asset-10"}, "external-asset"]}
                ],
                "elements": [
                    {"type": "image", "asset_id": "asset-11"},
                    {"type": "mask", "image_asset_ids": ["asset-12", "external-asset"]},
                    {"type": "note", "text": "mentions asset-13 without referencing it"}
                ],
                "metadata": {"source_asset_id": "asset-14"}
            }))?
            .as_slice(),
        )?;

        assert_eq!(
            asset_ids,
            BTreeSet::from([
                "asset-7".to_string(),
                "asset-8".to_string(),
                "asset-9".to_string(),
                "asset-10".to_string(),
                "asset-11".to_string(),
                "asset-12".to_string(),
                "asset-14".to_string()
            ])
        );

        Ok(())
    }
}
