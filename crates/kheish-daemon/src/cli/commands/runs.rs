//! Run inspection and control command handlers.

use anyhow::Result;

/// Handles `runs ...`.
pub(crate) async fn run_runs_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::RunsCommand,
) -> Result<()> {
    match command {
        crate::RunsCommand::List {
            session_id,
            pagination,
        } => {
            let mut path = "/v1/runs".to_string();
            let mut params = Vec::new();
            if let Some(session_id) = session_id {
                params.push(format!(
                    "session_id={}",
                    crate::cli::url_encode_component(&session_id)
                ));
            }
            pagination.append_query_params(&mut params);
            if !params.is_empty() {
                path.push('?');
                path.push_str(&params.join("&"));
            }
            if pagination.wants_page() {
                let runs = client
                    .get_list_page_compat::<kheish_daemon::RunView>(&path)
                    .await?;
                printer.print(&runs)
            } else {
                let runs = client
                    .get_json::<Vec<kheish_daemon::RunView>>(&path)
                    .await?;
                printer.print(&runs)
            }
        }
        crate::RunsCommand::Get { run_id } => {
            let run_id = crate::cli::url_encode_path_segment(&run_id);
            let run = client
                .get_json::<kheish_daemon::RunView>(&format!("/v1/runs/{run_id}"))
                .await?;
            printer.print(&run)
        }
        crate::RunsCommand::ExternalActions { run_id } => {
            let run_id = crate::cli::url_encode_path_segment(&run_id);
            let records = client
                .get_json::<Vec<kheish_daemon::ExternalActionAuditRecord>>(&format!(
                    "/v1/runs/{run_id}/external-actions"
                ))
                .await?;
            printer.print(&records)
        }
        crate::RunsCommand::Events { run_id } => {
            let run_id = crate::cli::url_encode_path_segment(&run_id);
            let events = client
                .get_json::<Vec<kheish_daemon::RunEventEntry>>(&format!("/v1/runs/{run_id}/events"))
                .await?;
            printer.print(&events)
        }
        crate::RunsCommand::Stream { run_id, cursor } => {
            let run_id = crate::cli::url_encode_path_segment(&run_id);
            let suffix = cursor
                .map(|cursor| format!("?cursor={cursor}"))
                .unwrap_or_default();
            client
                .stream_events(&format!("/v1/runs/{run_id}/stream{suffix}"), printer)
                .await
        }
        crate::RunsCommand::Wait(args) => {
            let run = crate::cli::wait_for_run(client, &args.run_id, args.poll_interval_ms).await?;
            printer.print(&run)
        }
        crate::RunsCommand::Debug { run_id } => {
            let run_id = crate::cli::url_encode_path_segment(&run_id);
            let view = client
                .get_json::<kheish_daemon::RunDebugView>(&format!("/v1/runs/{run_id}/debug"))
                .await?;
            printer.print(&view)
        }
        crate::RunsCommand::DebugArtifact {
            run_id,
            artifact_id,
        } => {
            let run_id = crate::cli::url_encode_path_segment(&run_id);
            let artifact_id = crate::cli::url_encode_path_segment(&artifact_id);
            let body = client
                .get_text(&format!("/v1/runs/{run_id}/debug/artifacts/{artifact_id}"))
                .await?;
            println!("{body}");
            Ok(())
        }
        crate::RunsCommand::Prune(args) => {
            let response = client
                .post_json::<_, kheish_daemon::RunRetentionPruneResponse>(
                    "/v1/runs/prune",
                    &kheish_daemon::RunRetentionPruneRequest {
                        older_than_ms: args.older_than_ms,
                        session_id: args.session_id,
                        limit: args.limit,
                        dry_run: args.dry_run,
                    },
                )
                .await?;
            printer.print(&response)
        }
        crate::RunsCommand::Cancel { run_id } => {
            let run_id = crate::cli::url_encode_path_segment(&run_id);
            let run = client
                .post_empty_json::<kheish_daemon::RunView>(&format!("/v1/runs/{run_id}/cancel"))
                .await?;
            printer.print(&run)
        }
    }
}
