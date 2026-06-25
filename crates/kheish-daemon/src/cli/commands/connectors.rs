//! Runtime connector command handlers.

use anyhow::Result;

/// Handles `connectors ...`.
pub(crate) async fn run_connectors_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::ConnectorsCommand,
) -> Result<()> {
    match command {
        crate::ConnectorsCommand::List => {
            let connectors = client
                .get_json::<Vec<kheish_daemon::ConnectorView>>("/v1/runtime/connectors")
                .await?;
            printer.print(&connectors)
        }
        crate::ConnectorsCommand::Get { kind, name } => {
            let kind = crate::cli::url_encode_path_segment(&kind);
            let name = crate::cli::url_encode_path_segment(&name);
            let connector = client
                .get_json::<kheish_daemon::ConnectorView>(&format!(
                    "/v1/runtime/connectors/{kind}/{name}"
                ))
                .await?;
            printer.print(&connector)
        }
        crate::ConnectorsCommand::PutExternal(args) => {
            let request =
                crate::cli::read_required_typed_json_input::<
                    kheish_daemon::PutExternalConnectorRequest,
                >(args.json.as_deref(), args.file.as_deref(), "connector JSON")
                .await?;
            let name = crate::cli::url_encode_path_segment(&args.name);
            let connector = client
                .put_json::<_, kheish_daemon::ConnectorView>(
                    &format!("/v1/runtime/connectors/external/{name}"),
                    &request,
                )
                .await?;
            printer.print(&connector)
        }
        crate::ConnectorsCommand::PutTelegram(args) => {
            let request =
                crate::cli::read_required_typed_json_input::<
                    kheish_daemon::PutTelegramConnectorRequest,
                >(args.json.as_deref(), args.file.as_deref(), "connector JSON")
                .await?;
            let name = crate::cli::url_encode_path_segment(&args.name);
            let connector = client
                .put_json::<_, kheish_daemon::ConnectorView>(
                    &format!("/v1/runtime/connectors/telegram/{name}"),
                    &request,
                )
                .await?;
            printer.print(&connector)
        }
        crate::ConnectorsCommand::PutSlack(args) => {
            let request = crate::cli::read_required_typed_json_input::<
                kheish_daemon::PutSlackConnectorRequest,
            >(
                args.json.as_deref(), args.file.as_deref(), "connector JSON"
            )
            .await?;
            let name = crate::cli::url_encode_path_segment(&args.name);
            let connector = client
                .put_json::<_, kheish_daemon::ConnectorView>(
                    &format!("/v1/runtime/connectors/slack/{name}"),
                    &request,
                )
                .await?;
            printer.print(&connector)
        }
        crate::ConnectorsCommand::PutHttp(args) => {
            let request = crate::cli::read_required_typed_json_input::<
                kheish_daemon::PutHttpConnectorRequest,
            >(
                args.json.as_deref(), args.file.as_deref(), "connector JSON"
            )
            .await?;
            let name = crate::cli::url_encode_path_segment(&args.name);
            let connector = client
                .put_json::<_, kheish_daemon::ConnectorView>(
                    &format!("/v1/runtime/connectors/http/{name}"),
                    &request,
                )
                .await?;
            printer.print(&connector)
        }
        crate::ConnectorsCommand::Delete { kind, name } => {
            let kind = crate::cli::url_encode_path_segment(&kind);
            let name = crate::cli::url_encode_path_segment(&name);
            let response = client
                .delete_json::<crate::AcceptedResponse>(&format!(
                    "/v1/runtime/connectors/{kind}/{name}"
                ))
                .await?;
            printer.print(&response)
        }
    }
}
