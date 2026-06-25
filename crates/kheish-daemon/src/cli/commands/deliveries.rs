//! Delivery queue inspection and replay command handlers.

use anyhow::Result;

/// Handles `deliveries ...`.
pub(crate) async fn run_deliveries_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::DeliveriesCommand,
) -> Result<()> {
    match command {
        crate::DeliveriesCommand::List(args) => {
            let path = delivery_list_path("/v1/deliveries", &args);
            if args.pagination.wants_page() {
                let deliveries = client
                    .get_list_page_compat::<kheish_daemon::DeliveryView>(&path)
                    .await?;
                printer.print(&deliveries)
            } else {
                let deliveries = client
                    .get_json::<Vec<kheish_daemon::DeliveryView>>(&path)
                    .await?;
                printer.print(&deliveries)
            }
        }
        crate::DeliveriesCommand::DeadLetter(args) => {
            let path = delivery_list_path("/v1/deliveries/dead-letter", &args);
            if args.pagination.wants_page() {
                let deliveries = client
                    .get_list_page_compat::<kheish_daemon::DeliveryView>(&path)
                    .await?;
                printer.print(&deliveries)
            } else {
                let deliveries = client
                    .get_json::<Vec<kheish_daemon::DeliveryView>>(&path)
                    .await?;
                printer.print(&deliveries)
            }
        }
        crate::DeliveriesCommand::Get { delivery_id } => {
            let delivery_id = crate::cli::url_encode_path_segment(&delivery_id);
            let delivery = client
                .get_json::<kheish_daemon::DeliveryView>(&format!("/v1/deliveries/{delivery_id}"))
                .await?;
            printer.print(&delivery)
        }
        crate::DeliveriesCommand::Replay { delivery_id, force } => {
            let delivery_id = crate::cli::url_encode_path_segment(&delivery_id);
            let query = if force { "?force=true" } else { "" };
            let response = client
                .post_empty_json::<kheish_daemon::DeliveryReplayResponse>(&format!(
                    "/v1/deliveries/{delivery_id}/replay{query}"
                ))
                .await?;
            printer.print(&response)
        }
        crate::DeliveriesCommand::Resolve {
            delivery_id,
            reason,
        } => {
            let delivery_id = crate::cli::url_encode_path_segment(&delivery_id);
            let response = client
                .post_json::<_, kheish_daemon::DeliveryView>(
                    &format!("/v1/deliveries/{delivery_id}/resolve"),
                    &kheish_daemon::DeliveryResolveRequest {
                        reason: Some(reason),
                    },
                )
                .await?;
            printer.print(&response)
        }
        crate::DeliveriesCommand::ReplayBulk(args) => {
            let response = client
                .post_json::<_, kheish_daemon::DeliveryBulkReplayResponse>(
                    "/v1/deliveries/replay-bulk",
                    &kheish_daemon::DeliveryBulkReplayRequest {
                        session_id: args.session_id,
                        run_id: args.run_id,
                        plugin: args.plugin,
                        limit: args.limit,
                        dry_run: args.dry_run,
                        force: args.force,
                        unresolved_only: !args.include_resolved,
                    },
                )
                .await?;
            printer.print(&response)
        }
        crate::DeliveriesCommand::ResetBackpressure(args) => {
            let response = client
                .post_json::<_, kheish_daemon::DeliveryBackpressureResetResponse>(
                    "/v1/deliveries/backpressure/reset",
                    &kheish_daemon::DeliveryBackpressureResetRequest {
                        target: args.target,
                        plugin: args.plugin,
                        dry_run: args.dry_run,
                    },
                )
                .await?;
            printer.print(&response)
        }
    }
}

fn delivery_list_path(base: &str, args: &crate::DeliveryListArgs) -> String {
    let mut params = Vec::new();
    if let Some(session_id) = args.session_id.as_deref() {
        params.push(format!(
            "session_id={}",
            crate::cli::url_encode_component(session_id)
        ));
    }
    if let Some(run_id) = args.run_id.as_deref() {
        params.push(format!(
            "run_id={}",
            crate::cli::url_encode_component(run_id)
        ));
    }
    if let Some(plugin) = args.plugin.as_deref() {
        params.push(format!(
            "plugin={}",
            crate::cli::url_encode_component(plugin)
        ));
    }
    if let Some(status) = args.status {
        params.push(format!("status={}", status.as_query_value()));
    }
    args.pagination.append_query_params(&mut params);
    if params.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", params.join("&"))
    }
}
