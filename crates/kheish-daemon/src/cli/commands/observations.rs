//! Observation command handlers.

use anyhow::{Result, anyhow};
use serde_json::Value;

/// Handles `observations ...`.
pub(crate) async fn run_observations_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::ObservationsCommand,
) -> Result<()> {
    match command {
        crate::ObservationsCommand::Sources { command } => match command {
            crate::ObservationSourcesCommand::List { query } => {
                let mut path = "/v1/observation-sources".to_string();
                if let Some(query) = query.filter(|value| !value.trim().is_empty()) {
                    path = format!(
                        "/v1/observation-sources?query={}",
                        crate::cli::url_encode_component(&query)
                    );
                }
                let sources = client
                    .get_json::<Vec<kheish_daemon::ObservationSourceView>>(&path)
                    .await?;
                printer.print(&sources)
            }
            crate::ObservationSourcesCommand::Get { source_id } => {
                let source_id = crate::cli::url_encode_path_segment(&source_id);
                let source = client
                    .get_json::<kheish_daemon::ObservationSourceView>(&format!(
                        "/v1/observation-sources/{source_id}"
                    ))
                    .await?;
                printer.print(&source)
            }
            crate::ObservationSourcesCommand::Create(args) => {
                let upload_token = crate::cli::read_secret_arg(
                    args.upload_token,
                    args.upload_token_file.as_deref(),
                    "--upload-token",
                    "--upload-token-file",
                )?
                .ok_or_else(|| anyhow!("provide --upload-token or --upload-token-file"))?;
                let source = client
                    .post_json::<_, kheish_daemon::ObservationSourceView>(
                        "/v1/observation-sources",
                        &kheish_daemon::CreateObservationSourceRequest {
                            source_id: args.source_id,
                            display_name: args.display_name,
                            kind: args.kind.into(),
                            upload_token,
                            sensitivity: args.sensitivity.into(),
                            retention_seconds: args.retention_seconds,
                            max_active_observations: args.max_active_observations,
                            max_active_bytes: args.max_active_bytes,
                            ingest_rate_limit_window_ms: args.ingest_rate_limit_window_ms,
                            ingest_rate_limit_burst: args.ingest_rate_limit_burst,
                            purge_raw_on_retention: args.purge_raw_on_retention,
                            allow_materialization: !args.disable_materialization,
                            allow_output_delivery: args.allow_output_delivery,
                        },
                    )
                    .await?;
                printer.print(&source)
            }
            crate::ObservationSourcesCommand::RotateToken(args) => {
                let source_id = crate::cli::url_encode_path_segment(&args.source_id);
                let upload_token = crate::cli::read_secret_arg(
                    args.upload_token,
                    args.upload_token_file.as_deref(),
                    "--upload-token",
                    "--upload-token-file",
                )?
                .ok_or_else(|| anyhow!("provide --upload-token or --upload-token-file"))?;
                let source = client
                    .post_json::<_, kheish_daemon::ObservationSourceView>(
                        &format!("/v1/observation-sources/{source_id}/rotate-token"),
                        &kheish_daemon::RotateObservationSourceTokenRequest {
                            upload_token,
                            grace_period_ms: args.grace_period_ms,
                        },
                    )
                    .await?;
                printer.print(&source)
            }
            crate::ObservationSourcesCommand::RevokeToken(args) => {
                let source_id = crate::cli::url_encode_path_segment(&args.source_id);
                let source = client
                    .post_json::<_, kheish_daemon::ObservationSourceView>(
                        &format!("/v1/observation-sources/{source_id}/revoke-token"),
                        &kheish_daemon::RevokeObservationSourceTokenRequest {
                            reason: args.reason,
                        },
                    )
                    .await?;
                printer.print(&source)
            }
        },
        crate::ObservationsCommand::List {
            source_id,
            stream_id,
            after_ms,
            before_ms,
            include_purged,
        } => {
            let mut query = Vec::new();
            let source_id = source_id.filter(|value| !value.trim().is_empty());
            let stream_id = match stream_id {
                Some(stream_id) => {
                    let stream_id = stream_id.trim().to_string();
                    anyhow::ensure!(!stream_id.is_empty(), "--stream-id cannot be empty");
                    anyhow::ensure!(source_id.is_some(), "--stream-id requires --source-id");
                    Some(stream_id)
                }
                None => None,
            };
            if let Some(source_id) = source_id {
                query.push(format!(
                    "source_id={}",
                    crate::cli::url_encode_component(&source_id)
                ));
            }
            if let Some(stream_id) = stream_id {
                query.push(format!(
                    "stream_id={}",
                    crate::cli::url_encode_component(&stream_id)
                ));
            }
            if let Some(after_ms) = after_ms {
                query.push(format!("after_ms={after_ms}"));
            }
            if let Some(before_ms) = before_ms {
                query.push(format!("before_ms={before_ms}"));
            }
            if include_purged {
                query.push("include_purged=true".to_string());
            }
            let path = if query.is_empty() {
                "/v1/observations".to_string()
            } else {
                format!("/v1/observations?{}", query.join("&"))
            };
            let observations = client
                .get_json::<Vec<kheish_daemon::ObservationView>>(&path)
                .await?;
            printer.print(&observations)
        }
        crate::ObservationsCommand::Get { observation_id } => {
            let observation_id = crate::cli::url_encode_path_segment(&observation_id);
            let observation = client
                .get_json::<kheish_daemon::ObservationView>(&format!(
                    "/v1/observations/{observation_id}"
                ))
                .await?;
            printer.print(&observation)
        }
        crate::ObservationsCommand::Audit(args) => {
            let mut query = Vec::new();
            if let Some(source_id) = args.source_id.filter(|value| !value.trim().is_empty()) {
                query.push(format!(
                    "source_id={}",
                    crate::cli::url_encode_component(&source_id)
                ));
            }
            if let Some(event) = args.event.filter(|value| !value.trim().is_empty()) {
                query.push(format!(
                    "event={}",
                    crate::cli::url_encode_component(&event)
                ));
            }
            query.push(format!("limit={}", args.limit));
            let records = client
                .get_json::<Vec<kheish_daemon::ObservationAuditRecord>>(&format!(
                    "/v1/observation-audit?{}",
                    query.join("&")
                ))
                .await?;
            printer.print(&records)
        }
        crate::ObservationsCommand::Ingest(args) => {
            let source_id = crate::cli::url_encode_path_segment(&args.source_id);
            let upload_token = crate::cli::read_secret_arg(
                args.upload_token,
                args.upload_token_file.as_deref(),
                "--upload-token",
                "--upload-token-file",
            )?
            .ok_or_else(|| anyhow!("provide --upload-token or --upload-token-file"))?;
            let upload =
                crate::cli::inline_asset_upload_from_path(&args.path, args.media_type.as_deref())
                    .await?;
            let canonical_text = crate::cli::read_optional_text_input(
                args.canonical_text,
                args.canonical_text_file.as_deref(),
                false,
            )
            .await?;
            let metadata = crate::cli::read_optional_json_input(
                args.metadata_json.as_deref(),
                args.metadata_file.as_deref(),
            )
            .await?
            .unwrap_or(Value::Null);
            let observation = client
                .post_json_with_bearer::<_, kheish_daemon::ObservationView>(
                    &format!("/v1/observation-sources/{source_id}/observations"),
                    &upload_token,
                    &kheish_daemon::CreateObservationRequest {
                        upload,
                        idempotency_key: args.idempotency_key,
                        captured_at_ms: args.captured_at_ms,
                        stream_id: args.stream_id,
                        seq_no: args.seq_no,
                        canonical_text,
                        metadata,
                    },
                )
                .await?;
            printer.print(&observation)
        }
        crate::ObservationsCommand::Materialize(args) => {
            let route_ids = crate::cli::fetch_known_route_ids(client).await?;
            let request =
                crate::cli::build_observation_materialization_request(&args, &route_ids).await?;
            let run = client
                .post_json::<_, kheish_daemon::RunView>(
                    "/v1/observation-materializations",
                    &request,
                )
                .await?;
            if args.wait {
                let run =
                    crate::cli::wait_for_run(client, &run.run_id, args.poll_interval_ms).await?;
                printer.print(&run)
            } else {
                printer.print(&run)
            }
        }
        crate::ObservationsCommand::Schedule(args) => {
            let route_ids = crate::cli::fetch_known_route_ids(client).await?;
            let request =
                crate::cli::build_observation_schedule_create_request(&args, &route_ids).await?;
            let schedule = client
                .post_json::<_, kheish_daemon::ScheduleView>("/v1/schedules", &request)
                .await?;
            printer.print(&schedule)
        }
    }
}
