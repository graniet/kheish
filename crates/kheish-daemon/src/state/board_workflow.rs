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

    /// Builds a compact, model-facing summary of one board for an agent.
    ///
    /// This enforces the same owner-or-unowned access rule as drawing but
    /// requires no run: an agent calls it to see the board's current contents
    /// before drawing, so it can continue an existing sketch instead of
    /// restarting from scratch. The summary carries the board metadata, the
    /// tip revision's author, a capped element list, an ASCII occupancy map,
    /// and a free-space hint. A board with no revisions yields an empty scene
    /// on the default 1600x1000 canvas with a friendly note.
    pub(crate) async fn agent_view_board(
        &self,
        session_id: &str,
        board_id: &str,
    ) -> Result<Value> {
        use crate::board_render;

        let _ = self.agent_id_for_session(session_id).await?;
        let board = self.board_service.get_board(board_id).await?;
        if let Some(owner) = board.summary.owner_session_id.as_deref() {
            anyhow::ensure!(
                owner == session_id,
                "board {board_id} is owned by session {owner}; only that session may view it"
            );
        }

        let (elements, canvas, last_author) = match board.summary.latest_revision_id.as_deref() {
            Some(revision_id) => {
                let revision = self
                    .board_service
                    .get_revision(board_id, revision_id)
                    .await?;
                let state = revision
                    .state_asset_id
                    .as_deref()
                    .and_then(|asset_id| self.assets.read_raw(asset_id).ok())
                    .and_then(|(_, bytes)| serde_json::from_slice::<Value>(&bytes).ok());
                let elements = state
                    .as_ref()
                    .map(board_render::elements_from_state)
                    .unwrap_or_default();
                let canvas = state
                    .as_ref()
                    .and_then(board_render::canvas_from_state)
                    .map(|(width, height)| board_render::clamp_canvas(width, height))
                    .unwrap_or((1600, 1000));
                (elements, canvas, board_view_last_author(&revision.metadata))
            }
            None => (Vec::new(), (1600u32, 1000u32), None),
        };

        let mut summary = board_render::scene_summary(canvas, &elements);
        if let Value::Object(map) = &mut summary {
            map.insert(
                "board".to_string(),
                serde_json::json!({
                    "board_id": board.summary.board_id,
                    "display_name": board.summary.display_name,
                    "revision_count": board.summary.revision_count,
                    "tip_revision_id": board.summary.latest_revision_id,
                    "canvas": {"width": canvas.0, "height": canvas.1},
                }),
            );
            map.insert(
                "last_author".to_string(),
                last_author.unwrap_or(Value::Null),
            );
        }
        Ok(summary)
    }

    /// Draws one batch of vector elements on a board on behalf of an agent:
    /// the batch is stamped with the agent's name and stable color, appended
    /// to the latest state, rasterized over the previous render, and stored
    /// as a new CAS-guarded revision. Concurrent writers retry against the
    /// fresh tip a bounded number of times.
    pub(crate) async fn agent_draw_on_board(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        board_id: &str,
        mut elements: Vec<crate::board_render::BoardElement>,
        note: Option<String>,
    ) -> Result<crate::boards::BoardRevisionView> {
        use crate::board_render;

        board_render::validate_elements(&elements)?;
        board_render::ensure_reasonable_text(&elements)?;

        let agent_id = self.agent_id_for_session(session_id).await?;
        let snapshot = self.live_snapshot(&agent_id).await?;
        let author_name = snapshot
            .agent
            .nickname
            .clone()
            .or(snapshot.agent.name.clone())
            .unwrap_or_else(|| session_id.to_string());
        let author_color = board_render::agent_color_for_session(session_id).to_string();
        for element in &mut elements {
            element.color.get_or_insert_with(|| author_color.clone());
            element.author = Some(crate::board_render::BoardElementAuthor {
                kind: "agent".to_string(),
                name: author_name.clone(),
                color: Some(author_color.clone()),
                session_id: Some(session_id.to_string()),
            });
        }

        const MAX_CAS_ATTEMPTS: usize = 3;
        let mut last_error = None;
        for _attempt in 0..MAX_CAS_ATTEMPTS {
            let board = self.board_service.get_board(board_id).await?;
            if let Some(owner) = board.summary.owner_session_id.as_deref() {
                anyhow::ensure!(
                    owner == session_id,
                    "board {board_id} is owned by session {owner}; only that session may draw on it"
                );
            }
            let latest_revision_id = board.summary.latest_revision_id.clone();
            let (previous_elements, canvas, previous_render) = match latest_revision_id.as_deref()
            {
                Some(revision_id) => {
                    let revision = self
                        .board_service
                        .get_revision(board_id, revision_id)
                        .await?;
                    let previous_state = revision
                        .state_asset_id
                        .as_deref()
                        .and_then(|asset_id| self.assets.read_raw(asset_id).ok())
                        .and_then(|(_, bytes)| serde_json::from_slice::<Value>(&bytes).ok());
                    let elements = previous_state
                        .as_ref()
                        .map(board_render::elements_from_state)
                        .unwrap_or_default();
                    let canvas = previous_state
                        .as_ref()
                        .and_then(board_render::canvas_from_state);
                    let render = self
                        .assets
                        .read_raw(&revision.render_asset_id)
                        .map(|(_, bytes)| bytes)
                        .ok();
                    (elements, canvas, render)
                }
                None => (Vec::new(), None, None),
            };
            let canvas = canvas
                .map(|(width, height)| board_render::clamp_canvas(width, height))
                .unwrap_or((1600, 1000));

            let png = board_render::render_board_png(
                canvas,
                previous_render.as_deref(),
                &elements,
                Some((author_name.as_str(), author_color.as_str())),
            )?;
            let stamp = now_ms();
            let provenance = crate::assets::AssetProvenanceRecord {
                kind: "board_draw".to_string(),
                tool_name: "board_draw".to_string(),
                session_id: Some(session_id.to_string()),
                run_id: run_id.map(ToOwned::to_owned),
                tool_call_id: None,
                route_id: None,
                provider: "daemon".to_string(),
                model: "board_render".to_string(),
                prompt_sha256: String::new(),
                source_assets: Vec::new(),
                output_index: 1,
                output_count: 1,
            };
            let render_asset = self.assets.import_bytes_with_provenance(
                &format!("board-{board_id}-{stamp}.png"),
                Some("image/png"),
                &png,
                Some(provenance.clone()),
            )?;
            let mut all_elements = previous_elements;
            all_elements.extend(elements.iter().cloned());
            let state_envelope = serde_json::json!({
                "schema_version": "kheish.board_state.v1",
                "board_id": board_id,
                "previous_revision_id": latest_revision_id,
                "canvas": {"width": canvas.0, "height": canvas.1},
                "elements": all_elements,
            });
            let state_asset = self.assets.import_bytes_with_provenance(
                &format!("board-{board_id}-{stamp}.json"),
                Some("application/json"),
                &serde_json::to_vec_pretty(&state_envelope)?,
                Some(provenance),
            )?;

            match self
                .create_board_revision(
                    board_id,
                    crate::CreateBoardRevisionRequest {
                        previous_revision_id: latest_revision_id,
                        client_revision_id: None,
                        render_asset_id: render_asset.id.clone(),
                        state_asset_id: Some(state_asset.id.clone()),
                        note: note.clone(),
                        source_session_id: Some(session_id.to_string()),
                        source_run_id: None,
                        metadata: serde_json::json!({
                            "tool": "board_draw",
                            "run_id": run_id,
                            "author_name": author_name,
                            "author_color": author_color,
                            "element_count": elements.len(),
                        }),
                    },
                )
                .await
            {
                Ok(revision) => return Ok(revision),
                Err(error) if error.to_string().contains("expects previous revision") => {
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            anyhow!("board {board_id} kept changing while the draw was being prepared")
        }))
    }
}

/// Extracts the tip revision's author name and color from its metadata, when
/// the drawing tool recorded them.
fn board_view_last_author(metadata: &Value) -> Option<Value> {
    let name = metadata.get("author_name").and_then(Value::as_str);
    let color = metadata.get("author_color").and_then(Value::as_str);
    match (name, color) {
        (None, None) => None,
        _ => Some(serde_json::json!({ "name": name, "color": color })),
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
