//! Board command handlers.

use anyhow::Result;
use serde_json::Value;

/// Handles `boards ...`.
pub(crate) async fn run_boards_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::BoardsCommand,
) -> Result<()> {
    match command {
        crate::BoardsCommand::List {
            query,
            owner_session_id,
        } => {
            let boards = client
                .get_json_with_query::<_, Vec<kheish_daemon::BoardView>>(
                    "/v1/boards",
                    &kheish_daemon::BoardListQuery {
                        query,
                        owner_session_id,
                    },
                )
                .await?;
            printer.print(&boards)
        }
        crate::BoardsCommand::Get { board_id } => {
            let board_id = crate::cli::url_encode_path_segment(&board_id);
            let board = client
                .get_json::<kheish_daemon::BoardView>(&format!("/v1/boards/{board_id}"))
                .await?;
            printer.print(&board)
        }
        crate::BoardsCommand::Create(args) => {
            let metadata = crate::cli::read_optional_json_input(
                args.metadata_json.as_deref(),
                args.metadata_file.as_deref(),
            )
            .await?
            .unwrap_or(Value::Null);
            let board = client
                .post_json::<_, kheish_daemon::BoardView>(
                    "/v1/boards",
                    &kheish_daemon::CreateBoardRequest {
                        board_id: args.board_id,
                        display_name: args.display_name,
                        owner_session_id: args.owner_session_id,
                        metadata,
                    },
                )
                .await?;
            printer.print(&board)
        }
        crate::BoardsCommand::Update(args) => {
            let metadata = crate::cli::read_optional_json_input(
                args.metadata_json.as_deref(),
                args.metadata_file.as_deref(),
            )
            .await?;
            let board_id = crate::cli::url_encode_path_segment(&args.board_id);
            let board = client
                .put_json::<_, kheish_daemon::BoardView>(
                    &format!("/v1/boards/{board_id}"),
                    &kheish_daemon::UpdateBoardRequest {
                        display_name: args.display_name,
                        metadata,
                    },
                )
                .await?;
            printer.print(&board)
        }
        crate::BoardsCommand::Revisions { board_id } => {
            let board_id = crate::cli::url_encode_path_segment(&board_id);
            let revisions = client
                .get_json::<Vec<kheish_daemon::BoardRevisionView>>(&format!(
                    "/v1/boards/{board_id}/revisions"
                ))
                .await?;
            printer.print(&revisions)
        }
        crate::BoardsCommand::GetRevision {
            board_id,
            revision_id,
        } => {
            let board_id = crate::cli::url_encode_path_segment(&board_id);
            let revision_id = crate::cli::url_encode_path_segment(&revision_id);
            let revision = client
                .get_json::<kheish_daemon::BoardRevisionView>(&format!(
                    "/v1/boards/{board_id}/revisions/{revision_id}"
                ))
                .await?;
            printer.print(&revision)
        }
        crate::BoardsCommand::CreateRevision(args) => {
            let metadata = crate::cli::read_optional_json_input(
                args.metadata_json.as_deref(),
                args.metadata_file.as_deref(),
            )
            .await?
            .unwrap_or(Value::Null);
            let board_id = crate::cli::url_encode_path_segment(&args.board_id);
            let revision = client
                .post_json::<_, kheish_daemon::BoardRevisionView>(
                    &format!("/v1/boards/{board_id}/revisions"),
                    &kheish_daemon::CreateBoardRevisionRequest {
                        previous_revision_id: args.previous_revision_id,
                        client_revision_id: args.client_revision_id,
                        render_asset_id: args.render_asset_id,
                        state_asset_id: args.state_asset_id,
                        note: args.note,
                        source_session_id: args.source_session_id,
                        source_run_id: args.source_run_id,
                        metadata,
                    },
                )
                .await?;
            printer.print(&revision)
        }
    }
}
