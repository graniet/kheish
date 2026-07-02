//! Built-in MCP catalog CLI commands.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use axum::{
    Router,
    extract::{Query, State},
    response::IntoResponse,
    routing::get,
};
use kheish_mcp::{
    McpCatalogAuthKind, McpCatalogEntryView, McpCatalogStatus, builtin_catalog_entries,
    builtin_catalog_entry, builtin_catalog_profile, builtin_catalog_profiles,
    catalog_credential_secret_ref, normalize_catalog_profile_name,
};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};

#[derive(Debug, Serialize)]
struct McpCatalogCredentialSlotsView {
    entry_id: String,
    server_name: String,
    credentials: Vec<McpCatalogCredentialSlotView>,
}

#[derive(Debug, Serialize)]
struct McpCatalogCredentialSlotView {
    credential_env: String,
    secret_ref: String,
}

/// Handles `mcp ...`.
pub(crate) async fn run_mcp_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::McpCommand,
) -> Result<()> {
    match command {
        crate::McpCommand::Catalog { command } => match command {
            crate::McpCatalogCommand::List {
                profile,
                supported_only,
            } => {
                let mut entries = builtin_catalog_entries();
                if let Some(profile) = profile {
                    let profile = normalize_catalog_profile_name(&profile)?;
                    entries.retain(|entry| entry.profiles.iter().any(|value| value == &profile));
                }
                if supported_only {
                    entries.retain(|entry| entry.status == McpCatalogStatus::Supported);
                }
                printer.print(&entries)
            }
            crate::McpCatalogCommand::Get { id } => {
                let entry = builtin_catalog_entry(&id)
                    .ok_or_else(|| anyhow!("unknown MCP catalog entry `{id}`"))?;
                printer.print(&entry)
            }
        },
        crate::McpCommand::Profiles { command } => match command {
            crate::McpProfilesCommand::List => {
                let profiles = builtin_catalog_profiles();
                printer.print(&profiles)
            }
            crate::McpProfilesCommand::Get { id } => {
                let id = normalize_catalog_profile_name(&id)?;
                let profile = builtin_catalog_profile(&id)
                    .ok_or_else(|| anyhow!("unknown MCP profile `{id}`"))?;
                printer.print(&profile)
            }
        },
        crate::McpCommand::Auth { command } => match command {
            crate::McpAuthCommand::Slots { id } => {
                let entry = builtin_catalog_entry(&id)
                    .ok_or_else(|| anyhow!("unknown MCP catalog entry `{id}`"))?;
                printer.print(&credential_slots_view(&entry))
            }
            crate::McpAuthCommand::Set(args) => {
                let entry = builtin_catalog_entry(&args.id)
                    .ok_or_else(|| anyhow!("unknown MCP catalog entry `{}`", args.id))?;
                let credential_env =
                    resolve_credential_env(&entry, args.credential_env.as_deref())?;
                let secret_ref = catalog_credential_secret_ref(&entry.id, &credential_env);
                crate::cli::commands::secrets::run_secrets_command(
                    client,
                    printer,
                    crate::SecretsCommand::Set(crate::SecretSetArgs {
                        secret_ref,
                        provider: crate::SecretProviderKind::Generic,
                        value: args.value,
                        from_env: args.from_env,
                        from_file: args.from_file,
                        stdin: args.stdin,
                        organization: None,
                        project: None,
                        store: args.store,
                    }),
                )
                .await
            }
        },
        crate::McpCommand::Tools { command } => match command {
            crate::McpToolsCommand::Call(args) => {
                let has_inline = args.input_json.is_some();
                let input: serde_json::Value = if args.input_file.is_none() && !args.stdin {
                    let inline = args.input_json.unwrap_or_else(|| "{}".to_string());
                    serde_json::from_str(&inline)
                        .map_err(|error| anyhow!("invalid --input-json: {error}"))?
                } else {
                    if has_inline {
                        return Err(anyhow!(
                            "provide only one of --input-json, --input-file, or --stdin"
                        ));
                    }
                    crate::cli::read_json_input::<serde_json::Value>(
                        None,
                        args.input_file.as_deref(),
                        args.stdin,
                    )
                    .await?
                };
                if !input.is_object() {
                    return Err(anyhow!("MCP tool input must be a JSON object"));
                }
                let encoded = crate::cli::url_encode_path_segment(&args.tool_name);
                let response = client
                    .post_json::<_, kheish_daemon::McpToolCallResponse>(
                        &format!("/v1/runtime/mcp/tools/{encoded}/call"),
                        &kheish_daemon::McpToolCallRequest { input },
                    )
                    .await?;
                printer.print(&response)
            }
        },
        crate::McpCommand::Oauth { command } => match command {
            crate::McpOAuthCommand::Status(args) => {
                let slot_id = resolve_oauth_slot_id(&args.id, args.slot.as_deref());
                let status = client
                    .get_json::<kheish_auth::AuthSlotStatus>(&format!(
                        "/v1/runtime/auth/accounts/{slot_id}"
                    ))
                    .await?;
                printer.print(&status)
            }
            crate::McpOAuthCommand::Refresh(args) => {
                let slot_id = resolve_oauth_slot_id(&args.id, args.slot.as_deref());
                let status = client
                    .post_empty_json::<kheish_auth::AuthSlotStatus>(&format!(
                        "/v1/runtime/auth/accounts/{slot_id}/refresh"
                    ))
                    .await?;
                printer.print(&status)
            }
            crate::McpOAuthCommand::Logout(args) => {
                let slot_id = resolve_oauth_slot_id(&args.id, args.slot.as_deref());
                let status = client
                    .post_empty_json::<serde_json::Value>(&format!(
                        "/v1/runtime/auth/accounts/{slot_id}/revoke"
                    ))
                    .await?;
                printer.print(&status)
            }
            crate::McpOAuthCommand::Login(args) => {
                let status = run_mcp_oauth_login(client, &args).await?;
                printer.print(&status)
            }
        },
    }
}

fn credential_slots_view(entry: &McpCatalogEntryView) -> McpCatalogCredentialSlotsView {
    McpCatalogCredentialSlotsView {
        entry_id: entry.id.clone(),
        server_name: entry.server_name.clone(),
        credentials: entry
            .credential_env
            .iter()
            .map(|credential_env| McpCatalogCredentialSlotView {
                credential_env: credential_env.clone(),
                secret_ref: catalog_credential_secret_ref(&entry.id, credential_env),
            })
            .collect(),
    }
}

fn resolve_credential_env(entry: &McpCatalogEntryView, requested: Option<&str>) -> Result<String> {
    if entry.credential_env.is_empty() {
        return Err(anyhow!(
            "MCP catalog entry `{}` does not declare a token credential",
            entry.id
        ));
    }
    if let Some(requested) = requested {
        if entry
            .credential_env
            .iter()
            .any(|credential_env| credential_env == requested)
        {
            return Ok(requested.to_string());
        }
        return Err(anyhow!(
            "MCP catalog entry `{}` does not declare credential `{requested}`",
            entry.id
        ));
    }
    if entry.credential_env.len() == 1 {
        return Ok(entry.credential_env[0].clone());
    }
    Err(anyhow!(
        "MCP catalog entry `{}` declares multiple credentials; pass --credential-env",
        entry.id
    ))
}

#[derive(Debug, Serialize)]
struct McpOAuthLoginPromptView {
    authorization_url: String,
    redirect_uri: String,
    slot_id: String,
}

#[derive(Clone)]
struct OAuthCallbackState {
    expected_state: String,
    sender: Arc<Mutex<Option<oneshot::Sender<std::result::Result<String, String>>>>>,
}

async fn run_mcp_oauth_login(
    client: &crate::cli::DaemonHttpClient,
    args: &crate::McpOAuthLoginArgs,
) -> Result<kheish_auth::AuthSlotStatus> {
    let target = resolve_oauth_target(args)?;
    let slot_id = resolve_oauth_slot_id(&target.entry_id, args.slot.as_deref());
    let http_client = reqwest::Client::new();
    let bind = SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        args.callback_port.unwrap_or(0),
    );
    let listener = TcpListener::bind(bind).await?;
    let callback_addr = listener.local_addr()?;
    let redirect_uri = format!("http://127.0.0.1:{}/callback", callback_addr.port());
    let discovery =
        kheish_auth::discover_mcp_oauth(&http_client, &target.url, args.allow_http_for_loopback)
            .await?;
    let client_secret = resolve_client_secret(args)?;
    let (client_id, client_secret) = if let Some(client_id) = args.client_id.clone() {
        (client_id, client_secret)
    } else {
        kheish_auth::dynamic_register_mcp_oauth_client(&http_client, &discovery, &redirect_uri)
            .await?
    };
    let auth_request = kheish_auth::build_mcp_oauth_authorization_request(
        &discovery,
        &client_id,
        &redirect_uri,
        &args.scopes,
    )?;
    let (code_rx, shutdown_tx) = spawn_oauth_callback(listener, auth_request.state.clone()).await?;
    let prompt = McpOAuthLoginPromptView {
        authorization_url: auth_request.authorization_url.clone(),
        redirect_uri: redirect_uri.clone(),
        slot_id: slot_id.clone(),
    };
    eprintln!("{}", serde_json::to_string_pretty(&prompt)?);
    if !args.no_open {
        let _ = open_browser_url(&auth_request.authorization_url);
    }
    let code = tokio::time::timeout(Duration::from_secs(args.timeout_sec), code_rx)
        .await
        .map_err(|_| anyhow!("timed out waiting for OAuth callback"))?
        .map_err(|_| anyhow!("OAuth callback server stopped before receiving a code"))?
        .map_err(|error| anyhow!(error))?;
    let _ = shutdown_tx.send(());
    let token = kheish_auth::exchange_mcp_oauth_code(
        &http_client,
        &discovery,
        &client_id,
        client_secret.as_deref(),
        &redirect_uri,
        &code,
        &auth_request.code_verifier,
        &auth_request.scopes,
    )
    .await?;
    let input = kheish_auth::mcp_oauth_account_input(
        kheish_auth::AuthSlotId::new(slot_id),
        target.server_name,
        &discovery,
        client_id,
        client_secret,
        token,
    );
    client
        .post_json::<_, kheish_auth::AuthSlotStatus>("/v1/runtime/auth/accounts/mcp-oauth", &input)
        .await
}

fn resolve_client_secret(args: &crate::McpOAuthLoginArgs) -> Result<Option<String>> {
    let sources = [
        args.client_secret.is_some(),
        args.client_secret_env.is_some(),
        args.client_secret_file.is_some(),
        args.client_secret_stdin,
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if sources > 1 {
        return Err(anyhow!(
            "pass only one of --client-secret, --client-secret-env, --client-secret-file, or --client-secret-stdin"
        ));
    }
    if let Some(value) = args.client_secret.clone() {
        return Ok(Some(value));
    }
    if let Some(env_name) = args.client_secret_env.as_deref() {
        return std::env::var(env_name).map(Some).map_err(|error| {
            anyhow!("failed to read OAuth client secret from {env_name}: {error}")
        });
    }
    if let Some(path) = args.client_secret_file.as_deref() {
        let value = std::fs::read_to_string(path)?;
        return Ok(Some(value.trim_end_matches(&['\r', '\n'][..]).to_string()));
    }
    if args.client_secret_stdin {
        let mut value = String::new();
        std::io::stdin().read_to_string(&mut value)?;
        return Ok(Some(value.trim_end_matches(&['\r', '\n'][..]).to_string()));
    }
    Ok(None)
}

struct McpOAuthTarget {
    entry_id: String,
    server_name: String,
    url: String,
}

fn resolve_oauth_target(args: &crate::McpOAuthLoginArgs) -> Result<McpOAuthTarget> {
    if let Some(url) = args.url.clone() {
        return Ok(McpOAuthTarget {
            entry_id: args.id.clone(),
            server_name: args.id.clone(),
            url,
        });
    }
    let entry = builtin_catalog_entry(&args.id).ok_or_else(|| {
        anyhow!(
            "unknown MCP catalog entry `{}`; pass --url for custom OAuth MCP servers",
            args.id
        )
    })?;
    if !matches!(
        entry.auth,
        McpCatalogAuthKind::OAuth | McpCatalogAuthKind::OAuthClientApp
    ) {
        return Err(anyhow!(
            "MCP catalog entry `{}` uses {:?}, not OAuth",
            entry.id,
            entry.auth
        ));
    }
    Ok(McpOAuthTarget {
        entry_id: entry.id,
        server_name: entry.server_name,
        url: entry.endpoint,
    })
}

fn resolve_oauth_slot_id(id: &str, explicit: Option<&str>) -> String {
    explicit
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("mcp.oauth.{id}"))
}

async fn spawn_oauth_callback(
    listener: TcpListener,
    expected_state: String,
) -> Result<(
    oneshot::Receiver<std::result::Result<String, String>>,
    oneshot::Sender<()>,
)> {
    let (code_tx, code_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let state = OAuthCallbackState {
        expected_state,
        sender: Arc::new(Mutex::new(Some(code_tx))),
    };
    let router = Router::new()
        .route("/callback", get(oauth_callback))
        .with_state(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await;
    });
    Ok((code_rx, shutdown_tx))
}

async fn oauth_callback(
    State(state): State<OAuthCallbackState>,
    Query(params): Query<BTreeMap<String, String>>,
) -> impl IntoResponse {
    let result = match params.get("error") {
        Some(error) => Err(format!("OAuth authorization failed: {error}")),
        None => match (params.get("code"), params.get("state")) {
            (Some(code), Some(received_state)) if received_state == &state.expected_state => {
                Ok(code.clone())
            }
            (Some(_), Some(_)) => Err("OAuth callback state did not match".to_string()),
            _ => Err("OAuth callback did not include code and state".to_string()),
        },
    };
    if let Some(sender) = state.sender.lock().await.take() {
        let _ = sender.send(result.clone());
    }
    match result {
        Ok(_) => "Kheish OAuth login completed. You can close this tab.".to_string(),
        Err(error) => format!("Kheish OAuth login failed: {error}"),
    }
}

fn open_browser_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = std::process::Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start"]);
        command
    };
    command.arg(url).spawn()?;
    Ok(())
}
