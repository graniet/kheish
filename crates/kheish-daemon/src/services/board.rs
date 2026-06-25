use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};
use tokio::sync::Mutex;

use crate::boards::{BoardRevisionView, BoardView, FileBoardStore};

#[derive(Clone, Debug, Default)]
struct BoardState {
    boards: BTreeMap<String, BoardView>,
    revisions: BTreeMap<String, BoardRevisionView>,
    revisions_by_board: BTreeMap<String, Vec<String>>,
}

impl BoardState {
    fn new(
        boards: BTreeMap<String, BoardView>,
        revisions: BTreeMap<String, BoardRevisionView>,
    ) -> Self {
        let mut revisions_by_board = BTreeMap::<String, Vec<String>>::new();
        for revision in revisions.values() {
            revisions_by_board
                .entry(revision.board_id.clone())
                .or_default()
                .push(revision.revision_id.clone());
        }
        for revision_ids in revisions_by_board.values_mut() {
            revision_ids.sort_by(|left, right| {
                let left_view = revisions.get(left).expect("revision index should be valid");
                let right_view = revisions
                    .get(right)
                    .expect("revision index should be valid");
                left_view
                    .created_at_ms
                    .cmp(&right_view.created_at_ms)
                    .then_with(|| left_view.revision_id.cmp(&right_view.revision_id))
            });
        }
        Self {
            boards,
            revisions,
            revisions_by_board,
        }
    }
}

/// Owns durable boards and immutable board revisions.
pub(crate) struct BoardService {
    store: FileBoardStore,
    state: Mutex<BoardState>,
    next_board_id: AtomicU64,
    next_revision_id: AtomicU64,
}

impl BoardService {
    /// Creates a new board service backed by persisted daemon state.
    pub(crate) fn new(
        store: FileBoardStore,
        boards: BTreeMap<String, BoardView>,
        revisions: BTreeMap<String, BoardRevisionView>,
        next_board_id: AtomicU64,
        next_revision_id: AtomicU64,
    ) -> Self {
        Self {
            store,
            state: Mutex::new(BoardState::new(boards, revisions)),
            next_board_id,
            next_revision_id,
        }
    }

    /// Returns one fresh daemon-managed board identifier.
    pub(crate) fn next_board_id(&self) -> String {
        format!(
            "board-{}",
            self.next_board_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Returns one fresh daemon-managed board revision identifier.
    pub(crate) fn next_revision_id(&self) -> String {
        format!(
            "board-revision-{}",
            self.next_revision_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Lists boards optionally filtered by owner session and a case-insensitive query.
    pub(crate) async fn list_boards(
        &self,
        owner_session_id: Option<&str>,
        query: Option<&str>,
    ) -> Vec<BoardView> {
        let query = query
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        let mut boards = self
            .state
            .lock()
            .await
            .boards
            .values()
            .filter(|board| {
                owner_session_id.is_none_or(|owner_session_id| {
                    board.summary.owner_session_id.as_deref() == Some(owner_session_id)
                }) && query.as_ref().is_none_or(|query| {
                    board.summary.board_id.to_ascii_lowercase().contains(query)
                        || board
                            .summary
                            .display_name
                            .to_ascii_lowercase()
                            .contains(query)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        boards.sort_by(|left, right| left.summary.board_id.cmp(&right.summary.board_id));
        boards
    }

    /// Returns one board by identifier.
    pub(crate) async fn get_board(&self, board_id: &str) -> Result<BoardView> {
        self.state
            .lock()
            .await
            .boards
            .get(board_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown board {board_id}"))
    }

    /// Returns one revision by board and revision identifier.
    pub(crate) async fn get_revision(
        &self,
        board_id: &str,
        revision_id: &str,
    ) -> Result<BoardRevisionView> {
        let state = self.state.lock().await;
        let revision = state
            .revisions
            .get(revision_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown board revision {revision_id}"))?;
        anyhow::ensure!(
            revision.board_id == board_id,
            "board revision {revision_id} does not belong to board {board_id}"
        );
        Ok(revision)
    }

    /// Lists immutable revisions for one board from the current chain tip backward.
    pub(crate) async fn list_revisions(&self, board_id: &str) -> Result<Vec<BoardRevisionView>> {
        let state = self.state.lock().await;
        let Some(revision_ids) = state.revisions_by_board.get(board_id) else {
            if state.boards.contains_key(board_id) {
                return Ok(Vec::new());
            }
            bail!("unknown board {board_id}");
        };
        let mut revisions = revision_ids
            .iter()
            .filter_map(|revision_id| state.revisions.get(revision_id).cloned())
            .collect::<Vec<_>>();
        sort_revisions_by_chain_tip(&mut revisions);
        Ok(revisions)
    }

    /// Persists one new board record.
    pub(crate) async fn create_board(&self, board: BoardView) -> Result<BoardView> {
        let mut state = self.state.lock().await;
        if state.boards.contains_key(&board.summary.board_id) {
            bail!("board {} already exists", board.summary.board_id);
        }
        self.store.save_board(&board)?;
        state
            .boards
            .insert(board.summary.board_id.clone(), board.clone());
        state
            .revisions_by_board
            .entry(board.summary.board_id.clone())
            .or_default();
        Ok(board)
    }

    /// Updates one persisted board record in place.
    pub(crate) async fn update_board(
        &self,
        board_id: &str,
        update: impl FnOnce(&mut BoardView) -> Result<bool>,
    ) -> Result<BoardView> {
        let mut state = self.state.lock().await;
        let board = state
            .boards
            .get_mut(board_id)
            .ok_or_else(|| anyhow!("unknown board {board_id}"))?;
        let previous = board.clone();
        if !update(board)? {
            return Ok(previous);
        }
        if let Err(error) = self.store.save_board(board) {
            *board = previous;
            return Err(error);
        }
        Ok(board.clone())
    }

    /// Persists one new immutable revision for the provided board.
    pub(crate) async fn create_revision(
        &self,
        board_id: &str,
        revision: BoardRevisionView,
    ) -> Result<BoardRevisionView> {
        let mut state = self.state.lock().await;
        if state.revisions.contains_key(&revision.revision_id) {
            bail!("board revision {} already exists", revision.revision_id);
        }
        let expected_previous_revision_id = state
            .boards
            .get(board_id)
            .ok_or_else(|| anyhow!("unknown board {board_id}"))?
            .summary
            .latest_revision_id
            .clone();
        anyhow::ensure!(
            revision.board_id == board_id,
            "board revision {} targets {} but was submitted for {}",
            revision.revision_id,
            revision.board_id,
            board_id
        );
        if let Some(client_revision_id) = revision.client_revision_id.as_deref()
            && let Some(existing) =
                find_revision_by_client_revision_id(&state, board_id, client_revision_id)
        {
            ensure_revision_idempotency_replay_matches(existing, &revision)?;
            return Ok(existing.clone());
        }
        if let Some(previous_revision_id) = revision.previous_revision_id.as_deref() {
            let previous = state
                .revisions
                .get(previous_revision_id)
                .ok_or_else(|| anyhow!("unknown previous board revision {previous_revision_id}"))?;
            anyhow::ensure!(
                previous.board_id == board_id,
                "previous board revision {previous_revision_id} does not belong to board {board_id}"
            );
        }
        let board = state
            .boards
            .get_mut(board_id)
            .ok_or_else(|| anyhow!("unknown board {board_id}"))?;
        anyhow::ensure!(
            expected_previous_revision_id == revision.previous_revision_id,
            "board {board_id} expects previous revision {:?}, got {:?}",
            expected_previous_revision_id,
            revision.previous_revision_id
        );

        let previous_board = board.clone();
        self.store.save_revision(&revision)?;
        board.summary.latest_revision_id = Some(revision.revision_id.clone());
        board.summary.revision_count = board.summary.revision_count.saturating_add(1);
        board.summary.updated_at_ms = revision.created_at_ms.max(board.summary.updated_at_ms);
        if let Err(error) = self.store.save_board(board) {
            *board = previous_board;
            return match self.store.delete_revision(&revision.revision_id) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(anyhow!(
                    "failed to persist board {board_id} after writing revision {}; rollback also failed: {rollback_error}",
                    revision.revision_id
                )),
            };
        }

        state
            .revisions_by_board
            .entry(board_id.to_string())
            .or_default()
            .push(revision.revision_id.clone());
        state
            .revisions
            .insert(revision.revision_id.clone(), revision.clone());
        Ok(revision)
    }
}

fn sort_revisions_by_chain_tip(revisions: &mut Vec<BoardRevisionView>) {
    let Some(chain) = linear_revision_chain(revisions) else {
        revisions.sort_by(|left, right| {
            right
                .created_at_ms
                .cmp(&left.created_at_ms)
                .then_with(|| right.revision_id.cmp(&left.revision_id))
        });
        return;
    };
    *revisions = chain.into_iter().rev().cloned().collect();
}

fn linear_revision_chain<'a>(
    revisions: &'a [BoardRevisionView],
) -> Option<Vec<&'a BoardRevisionView>> {
    if revisions.is_empty() {
        return Some(Vec::new());
    }
    let revisions_by_id = revisions
        .iter()
        .map(|revision| (revision.revision_id.as_str(), revision))
        .collect::<BTreeMap<_, _>>();
    let mut roots = Vec::<&BoardRevisionView>::new();
    let mut children_by_parent = BTreeMap::<&str, Vec<&BoardRevisionView>>::new();
    for revision in revisions {
        if let Some(previous_revision_id) = revision.previous_revision_id.as_deref() {
            let parent = revisions_by_id.get(previous_revision_id)?;
            if parent.board_id != revision.board_id {
                return None;
            }
            children_by_parent
                .entry(previous_revision_id)
                .or_default()
                .push(revision);
        } else {
            roots.push(revision);
        }
    }
    if roots.len() != 1
        || children_by_parent
            .values()
            .any(|children| children.len() != 1)
    {
        return None;
    }

    let mut chain = Vec::with_capacity(revisions.len());
    let mut current = roots[0];
    loop {
        if chain
            .iter()
            .any(|seen: &&BoardRevisionView| seen.revision_id == current.revision_id)
        {
            return None;
        }
        chain.push(current);
        let Some(children) = children_by_parent.get(current.revision_id.as_str()) else {
            break;
        };
        current = children[0];
    }
    if chain.len() == revisions.len() {
        Some(chain)
    } else {
        None
    }
}

fn find_revision_by_client_revision_id<'a>(
    state: &'a BoardState,
    board_id: &str,
    client_revision_id: &str,
) -> Option<&'a BoardRevisionView> {
    state
        .revisions_by_board
        .get(board_id)?
        .iter()
        .filter_map(|revision_id| state.revisions.get(revision_id))
        .find(|revision| revision.client_revision_id.as_deref() == Some(client_revision_id))
}

fn ensure_revision_idempotency_replay_matches(
    existing: &BoardRevisionView,
    requested: &BoardRevisionView,
) -> Result<()> {
    let matches = existing.board_id == requested.board_id
        && existing.previous_revision_id == requested.previous_revision_id
        && existing.render_asset_id == requested.render_asset_id
        && existing.state_asset_id == requested.state_asset_id
        && existing.note == requested.note
        && existing.source_session_id == requested.source_session_id
        && existing.source_run_id == requested.source_run_id
        && existing.metadata == requested.metadata;
    if matches {
        return Ok(());
    }
    let client_revision_id = existing
        .client_revision_id
        .as_deref()
        .unwrap_or("<missing>");
    bail!(
        "board revision client_revision_id {client_revision_id} is already bound to revision {} with a different request payload",
        existing.revision_id
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use serde_json::{Value, json};
    use tempfile::tempdir;

    use super::*;
    use crate::boards::BoardSummaryView;

    fn board(board_id: &str) -> BoardView {
        BoardView {
            summary: BoardSummaryView {
                board_id: board_id.to_string(),
                display_name: "Test Board".to_string(),
                owner_session_id: Some("session-1".to_string()),
                latest_revision_id: None,
                revision_count: 0,
                created_at_ms: 1,
                updated_at_ms: 1,
            },
            metadata: Value::Null,
        }
    }

    fn revision(revision_id: &str, render_asset_id: &str) -> BoardRevisionView {
        BoardRevisionView {
            revision_id: revision_id.to_string(),
            board_id: "board-1".to_string(),
            previous_revision_id: None,
            client_revision_id: Some("client-rev-1".to_string()),
            render_asset_id: render_asset_id.to_string(),
            state_asset_id: Some("asset-state-1".to_string()),
            note: Some("same".to_string()),
            source_session_id: Some("session-1".to_string()),
            source_run_id: Some("run-1".to_string()),
            created_at_ms: 10,
            metadata: json!({"source": "test"}),
        }
    }

    fn chained_revision(
        revision_id: &str,
        previous_revision_id: Option<&str>,
        created_at_ms: u64,
    ) -> BoardRevisionView {
        BoardRevisionView {
            revision_id: revision_id.to_string(),
            board_id: "board-1".to_string(),
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

    fn service() -> BoardService {
        let temp = tempdir().expect("temp dir");
        let store = FileBoardStore::new(temp.keep());
        BoardService::new(
            store,
            BTreeMap::new(),
            BTreeMap::new(),
            AtomicU64::new(1),
            AtomicU64::new(1),
        )
    }

    #[tokio::test]
    async fn board_revision_client_revision_id_replays_same_payload() -> Result<()> {
        let service = service();
        service.create_board(board("board-1")).await?;

        let first = service
            .create_revision("board-1", revision("board-revision-1", "asset-render-1"))
            .await?;
        let replay = service
            .create_revision("board-1", revision("board-revision-2", "asset-render-1"))
            .await?;

        assert_eq!(replay.revision_id, first.revision_id);
        let board = service.get_board("board-1").await?;
        assert_eq!(
            board.summary.latest_revision_id.as_deref(),
            Some("board-revision-1")
        );
        assert_eq!(board.summary.revision_count, 1);
        let revisions = service.list_revisions("board-1").await?;
        assert_eq!(revisions.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn board_revision_client_revision_id_conflicts_on_different_payload() -> Result<()> {
        let service = service();
        service.create_board(board("board-1")).await?;
        service
            .create_revision("board-1", revision("board-revision-1", "asset-render-1"))
            .await?;

        let error = service
            .create_revision("board-1", revision("board-revision-2", "asset-render-2"))
            .await
            .expect_err("different payload for same client_revision_id should conflict");
        assert!(
            error
                .to_string()
                .contains("already bound to revision board-revision-1"),
            "unexpected error: {error:#}"
        );
        let board = service.get_board("board-1").await?;
        assert_eq!(board.summary.revision_count, 1);
        Ok(())
    }

    #[tokio::test]
    async fn list_revisions_orders_by_chain_tip_when_timestamps_are_skewed() -> Result<()> {
        let service = service();
        service.create_board(board("board-1")).await?;
        service
            .create_revision(
                "board-1",
                chained_revision("board-revision-root", None, 200),
            )
            .await?;
        service
            .create_revision(
                "board-1",
                chained_revision("board-revision-child", Some("board-revision-root"), 30),
            )
            .await?;

        let revisions = service.list_revisions("board-1").await?;

        assert_eq!(
            revisions
                .iter()
                .map(|revision| revision.revision_id.as_str())
                .collect::<Vec<_>>(),
            vec!["board-revision-child", "board-revision-root"]
        );
        Ok(())
    }
}
