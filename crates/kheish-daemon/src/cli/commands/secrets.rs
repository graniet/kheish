//! Secret-store command handlers.

use anyhow::{Result, bail};

/// Handles `secrets ...`.
pub(crate) async fn run_secrets_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::SecretsCommand,
) -> Result<()> {
    if matches!(&command, crate::SecretsCommand::Generate) {
        println!("{}", kheish_auth::generate_auth_store_master_key_base64());
        return Ok(());
    }
    if crate::cli::secrets::secrets_command_store(&command).is_some_and(|store| store.offline) {
        return run_local_secrets_command(printer, command).await;
    }
    if crate::cli::daemon_control_plane_is_reachable(client).await {
        return run_remote_secrets_command(client, printer, command).await;
    }
    bail!(
        "daemon control-plane is unreachable; rerun the command with --offline and an explicit --state-root to operate on the local secret store"
    )
}

/// Handles `secrets ... --offline` against the local secret store.
pub(crate) async fn run_local_secrets_command(
    printer: &crate::cli::Printer,
    command: crate::SecretsCommand,
) -> Result<()> {
    match command {
        crate::SecretsCommand::Generate => unreachable!("generate is handled before store routing"),
        crate::SecretsCommand::List(args) => {
            let manager = kheish_auth::AuthManager::new(crate::cli::global_auth_store_path(
                &crate::cli::secrets::local_secret_store_path(&args)?,
            ))?;
            let statuses = manager.list_statuses().await?;
            printer.print(&statuses)
        }
        crate::SecretsCommand::Get(args) => {
            let manager = kheish_auth::AuthManager::new(crate::cli::global_auth_store_path(
                &crate::cli::secrets::local_secret_store_path(&args.store)?,
            ))?;
            let status = manager
                .status(&kheish_auth::AuthSlotId::new(args.secret_ref))
                .await?;
            printer.print(&status)
        }
        crate::SecretsCommand::Set(args) => {
            crate::cli::ensure_secret_manager_master_key_configured()?;
            let manager = kheish_auth::AuthManager::new(crate::cli::global_auth_store_path(
                &crate::cli::secrets::local_secret_store_path(&args.store)?,
            ))?;
            let record = crate::cli::secrets::build_secret_set_record(&args)?;
            let status = manager.put_record(record).await?;
            printer.print(&status)
        }
        crate::SecretsCommand::ImportCodex(args) => {
            crate::cli::ensure_secret_manager_master_key_configured()?;
            let manager = kheish_auth::AuthManager::new(crate::cli::global_auth_store_path(
                &crate::cli::secrets::local_secret_store_path(&args.store)?,
            ))?;
            let record = crate::cli::secrets::build_codex_import_record(&args)?;
            let status = manager.put_record(record).await?;
            printer.print(&status)
        }
        crate::SecretsCommand::ImportClaudeCode(args) => {
            crate::cli::ensure_secret_manager_master_key_configured()?;
            let manager = kheish_auth::AuthManager::new(crate::cli::global_auth_store_path(
                &crate::cli::secrets::local_secret_store_path(&args.store)?,
            ))?;
            let record = crate::cli::secrets::build_claude_code_import_record(&args)?;
            let status = manager.put_record(record).await?;
            printer.print(&status)
        }
        crate::SecretsCommand::Delete(args) => {
            crate::cli::ensure_secret_manager_master_key_configured()?;
            let manager = kheish_auth::AuthManager::new(crate::cli::global_auth_store_path(
                &crate::cli::secrets::local_secret_store_path(&args.store)?,
            ))?;
            let deleted = manager
                .delete(&kheish_auth::AuthSlotId::new(args.secret_ref))
                .await?;
            printer.print(&crate::AcceptedResponse { accepted: deleted })
        }
    }
}

/// Handles `secrets ...` through the daemon control plane.
async fn run_remote_secrets_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::SecretsCommand,
) -> Result<()> {
    match command {
        crate::SecretsCommand::Generate => {
            unreachable!("generate is handled before daemon routing")
        }
        crate::SecretsCommand::List(_) => {
            let statuses = client
                .get_json::<Vec<kheish_auth::AuthSlotStatus>>("/v1/runtime/secrets")
                .await?;
            printer.print(&statuses)
        }
        crate::SecretsCommand::Get(args) => {
            let status = client
                .get_json::<kheish_auth::AuthSlotStatus>(&format!(
                    "/v1/runtime/secrets/{}",
                    args.secret_ref
                ))
                .await?;
            printer.print(&status)
        }
        crate::SecretsCommand::Set(args) => {
            let record = crate::cli::secrets::build_secret_set_record(&args)?;
            let status = client
                .post_json::<_, kheish_auth::AuthSlotStatus>("/v1/runtime/secrets", &record)
                .await?;
            printer.print(&status)
        }
        crate::SecretsCommand::ImportCodex(args) => {
            let record = crate::cli::secrets::build_codex_import_record(&args)?;
            let status = client
                .post_json::<_, kheish_auth::AuthSlotStatus>("/v1/runtime/secrets", &record)
                .await?;
            printer.print(&status)
        }
        crate::SecretsCommand::ImportClaudeCode(args) => {
            let record = crate::cli::secrets::build_claude_code_import_record(&args)?;
            let status = client
                .post_json::<_, kheish_auth::AuthSlotStatus>("/v1/runtime/secrets", &record)
                .await?;
            printer.print(&status)
        }
        crate::SecretsCommand::Delete(args) => {
            let response = client
                .delete_json::<crate::AcceptedResponse>(&format!(
                    "/v1/runtime/secrets/{}",
                    args.secret_ref
                ))
                .await?;
            printer.print(&response)
        }
    }
}
