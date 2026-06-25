//! Asset command handlers.

use anyhow::Result;

/// Handles `assets ...`.
pub(crate) async fn run_assets_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::AssetsCommand,
) -> Result<()> {
    match command {
        crate::AssetsCommand::List { query, pagination } => {
            let mut path = "/v1/assets".to_string();
            let mut params = Vec::new();
            if let Some(query) = query.filter(|value| !value.trim().is_empty()) {
                params.push(format!(
                    "query={}",
                    crate::cli::url_encode_component(&query)
                ));
            }
            pagination.append_query_params(&mut params);
            if !params.is_empty() {
                path.push('?');
                path.push_str(&params.join("&"));
            }
            if pagination.wants_page() {
                let assets = client
                    .get_list_page_compat::<kheish_daemon::AssetSummaryView>(&path)
                    .await?;
                printer.print(&assets)
            } else {
                let assets = client
                    .get_json::<Vec<kheish_daemon::AssetSummaryView>>(&path)
                    .await?;
                printer.print(&assets)
            }
        }
        crate::AssetsCommand::Get { asset_id } => {
            let asset_id = crate::cli::url_encode_path_segment(&asset_id);
            let asset = client
                .get_json::<kheish_daemon::AssetView>(&format!("/v1/assets/{asset_id}"))
                .await?;
            printer.print(&asset)
        }
        crate::AssetsCommand::References { asset_id } => {
            let asset_id = crate::cli::url_encode_path_segment(&asset_id);
            let references = client
                .get_json::<kheish_daemon::AssetReferencesView>(&format!(
                    "/v1/assets/{asset_id}/references"
                ))
                .await?;
            printer.print(&references)
        }
        crate::AssetsCommand::Delete { asset_id, dry_run } => {
            let asset_id = crate::cli::url_encode_path_segment(&asset_id);
            let suffix = if dry_run { "?dry_run=true" } else { "" };
            let plan = client
                .delete_json::<kheish_daemon::AssetDeletionPlanView>(&format!(
                    "/v1/assets/{asset_id}{suffix}"
                ))
                .await?;
            printer.print(&plan)
        }
        crate::AssetsCommand::Gc { execute } => {
            let plan = client
                .post_json::<_, kheish_daemon::AssetGcPlanView>(
                    "/v1/assets/gc",
                    &kheish_daemon::AssetGcRequest {
                        dry_run: Some(!execute),
                    },
                )
                .await?;
            printer.print(&plan)
        }
        crate::AssetsCommand::Import(args) => {
            let upload =
                crate::cli::inline_asset_upload_from_path(&args.path, args.media_type.as_deref())
                    .await?;
            let asset = client
                .post_json::<_, kheish_daemon::AssetView>(
                    "/v1/assets",
                    &kheish_daemon::CreateAssetRequest { upload },
                )
                .await?;
            printer.print(&asset)
        }
    }
}
