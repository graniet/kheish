//! Board lifecycle methods implemented on [`DaemonState`].

use super::*;
use crate::boards::{board_state_payload_asset_ids, validate_board_state_payload};

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(crate) async fn list_boards(
        &self,
        owner_session_id: Option<&str>,
        query: Option<&str>,
    ) -> Result<Vec<crate::boards::BoardView>> {
        if let Some(owner_session_id) = owner_session_id {
            let _ = self.agent_id_for_session(owner_session_id).await?;
        }
        Ok(self
            .board_service
            .list_boards(owner_session_id, query)
            .await)
    }

    pub(crate) async fn get_board(&self, board_id: &str) -> Result<crate::boards::BoardView> {
        self.board_service.get_board(board_id).await
    }

    pub(crate) async fn create_board(
        &self,
        request: crate::CreateBoardRequest,
    ) -> Result<crate::boards::BoardView> {
        anyhow::ensure!(
            !request.display_name.trim().is_empty(),
            "display_name is required"
        );
        if let Some(owner_session_id) = request.owner_session_id.as_deref() {
            let _ = self.agent_id_for_session(owner_session_id).await?;
        }
        let now = now_ms();
        let auto_board_id = request
            .board_id
            .as_ref()
            .is_none_or(|value| value.trim().is_empty());
        let board_id = request
            .board_id
            .filter(|value: &String| !value.trim().is_empty())
            .map(|value| value.trim().to_string())
            .unwrap_or_else(|| self.board_service.next_board_id());
        let mut board = crate::boards::BoardView {
            summary: crate::boards::BoardSummaryView {
                board_id,
                display_name: request.display_name.trim().to_string(),
                owner_session_id: request.owner_session_id,
                latest_revision_id: None,
                revision_count: 0,
                created_at_ms: now,
                updated_at_ms: now,
            },
            metadata: request.metadata,
        };
        loop {
            match self.board_service.create_board(board.clone()).await {
                Ok(board) => return Ok(board),
                Err(error)
                    if auto_board_id
                        && error.to_string().contains(&format!(
                            "board {} already exists",
                            board.summary.board_id
                        )) =>
                {
                    board.summary.board_id = self.board_service.next_board_id();
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) async fn update_board(
        &self,
        board_id: &str,
        request: crate::UpdateBoardRequest,
    ) -> Result<crate::boards::BoardView> {
        let display_name = request.display_name.clone();
        let metadata = request.metadata.clone();
        if let Some(display_name) = display_name.as_deref() {
            anyhow::ensure!(!display_name.trim().is_empty(), "display_name is required");
        }
        self.board_service
            .update_board(board_id, |board| {
                let mut changed = false;
                if let Some(display_name) = display_name.as_deref()
                    && board.summary.display_name != display_name.trim()
                {
                    board.summary.display_name = display_name.trim().to_string();
                    changed = true;
                }
                if let Some(metadata) = metadata.clone()
                    && board.metadata != metadata
                {
                    board.metadata = metadata;
                    changed = true;
                }
                if changed {
                    board.summary.updated_at_ms = now_ms();
                }
                Ok(changed)
            })
            .await
    }

    pub(crate) async fn list_board_revisions(
        &self,
        board_id: &str,
    ) -> Result<Vec<crate::boards::BoardRevisionView>> {
        self.board_service.list_revisions(board_id).await
    }

    pub(crate) async fn get_board_revision(
        &self,
        board_id: &str,
        revision_id: &str,
    ) -> Result<crate::boards::BoardRevisionView> {
        self.board_service.get_revision(board_id, revision_id).await
    }

    pub(crate) async fn create_board_revision(
        &self,
        board_id: &str,
        request: crate::CreateBoardRevisionRequest,
    ) -> Result<crate::boards::BoardRevisionView> {
        let board = self.board_service.get_board(board_id).await?;
        let render_asset = self
            .assets
            .get(&request.render_asset_id)
            .ok_or_else(|| anyhow!("unknown asset {}", request.render_asset_id))?;
        anyhow::ensure!(
            render_asset.media_type.starts_with("image/"),
            "board revisions require an image render asset; {} has media type {}",
            request.render_asset_id,
            render_asset.media_type
        );
        let _ = self
            .assets
            .read_raw(&request.render_asset_id)
            .map_err(|_| {
                anyhow!(
                    "board revision render asset {} is missing raw payload",
                    request.render_asset_id
                )
            })?;
        if let Some(state_asset_id) = request.state_asset_id.as_deref() {
            let state_asset = self
                .assets
                .get(state_asset_id)
                .ok_or_else(|| anyhow!("unknown asset {state_asset_id}"))?;
            anyhow::ensure!(
                state_asset.media_type == "application/json",
                "board state asset {state_asset_id} must be application/json, got {}",
                state_asset.media_type
            );
            let (_, state_bytes) = self.assets.read_raw(state_asset_id).map_err(|_| {
                anyhow!("board state asset {state_asset_id} is missing raw payload")
            })?;
            validate_board_state_payload(
                board_id,
                request.previous_revision_id.as_deref(),
                &state_bytes,
            )?;
            for embedded_asset_id in board_state_payload_asset_ids(&state_bytes)? {
                let _ = self.assets.read_raw(&embedded_asset_id).map_err(|_| {
                    anyhow!(
                        "board state asset {state_asset_id} references unreadable asset {embedded_asset_id}"
                    )
                })?;
            }
        }
        if let Some(source_session_id) = request.source_session_id.as_deref() {
            let _ = self.agent_id_for_session(source_session_id).await?;
        }
        let mut resolved_source_session_id = request.source_session_id.clone();
        let client_revision_id = normalize_board_client_revision_id(request.client_revision_id)?;
        if let Some(source_run_id) = request.source_run_id.as_deref() {
            let record = self.run_service.run_record(source_run_id).await?;
            resolved_source_session_id.get_or_insert_with(|| record.view.session_id.clone());
            if let Some(source_session_id) = request.source_session_id.as_deref() {
                anyhow::ensure!(
                    record.view.session_id == source_session_id,
                    "run {source_run_id} does not belong to session {source_session_id}"
                );
            }
            let run_references_asset = |asset_id: &str| {
                record
                    .view
                    .input_attachments
                    .iter()
                    .any(|attachment| attachment.id == asset_id)
                    || record
                        .view
                        .outputs
                        .iter()
                        .flat_map(|output| output.artifacts.iter())
                        .any(|artifact| artifact.id == asset_id)
            };
            anyhow::ensure!(
                run_references_asset(&request.render_asset_id),
                "run {source_run_id} does not reference render asset {}",
                request.render_asset_id
            );
            if let Some(state_asset_id) = request.state_asset_id.as_deref() {
                anyhow::ensure!(
                    run_references_asset(state_asset_id),
                    "run {source_run_id} does not reference state asset {state_asset_id}"
                );
            }
        }
        if let Some(owner_session_id) = board.summary.owner_session_id.as_deref() {
            anyhow::ensure!(
                resolved_source_session_id.as_deref() == Some(owner_session_id),
                "board {board_id} is owned by session {owner_session_id}; revisions must come from that session"
            );
        }
        let revision = crate::boards::BoardRevisionView {
            revision_id: self.board_service.next_revision_id(),
            board_id: board_id.to_string(),
            previous_revision_id: request.previous_revision_id,
            client_revision_id,
            render_asset_id: request.render_asset_id,
            state_asset_id: request.state_asset_id,
            note: request
                .note
                .filter(|value: &String| !value.trim().is_empty()),
            source_session_id: resolved_source_session_id,
            source_run_id: request.source_run_id,
            created_at_ms: now_ms(),
            metadata: request.metadata,
        };
        self.board_service.create_revision(board_id, revision).await
    }
}

fn normalize_board_client_revision_id(value: Option<String>) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    anyhow::ensure!(!trimmed.is_empty(), "client_revision_id is required");
    anyhow::ensure!(
        trimmed.len() <= 256,
        "client_revision_id must not exceed 256 bytes"
    );
    anyhow::ensure!(
        !trimmed.chars().any(char::is_control),
        "client_revision_id must not contain control characters"
    );
    Ok(Some(trimmed.to_string()))
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serde_json::json;

    use super::*;

    #[test]
    fn board_state_payload_accepts_supported_v1_schema() -> Result<()> {
        validate_board_state_payload(
            "board-1",
            Some("board-revision-1"),
            serde_json::to_vec(&json!({
                "schema_version": "kheish.board_state.v1",
                "board_id": "board-1",
                "previous_revision_id": "board-revision-1",
                "canvas": { "width": 800, "height": 600 },
                "strokes": []
            }))?
            .as_slice(),
        )
    }

    #[test]
    fn board_state_payload_rejects_missing_schema_version() {
        let error = validate_board_state_payload(
            "board-1",
            None,
            br#"{"board_id":"board-1","strokes":[]}"#,
        )
        .expect_err("missing schema version should fail");
        assert!(
            error.to_string().contains("requires schema_version"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn board_state_payload_rejects_wrong_parent() {
        let error = validate_board_state_payload(
            "board-1",
            Some("board-revision-2"),
            br#"{"schema_version":1,"board_id":"board-1","previous_revision_id":"board-revision-1","strokes":[]}"#,
        )
        .expect_err("wrong parent should fail");
        assert!(
            error
                .to_string()
                .contains("previous_revision_id must match"),
            "unexpected error: {error}"
        );
    }
}
