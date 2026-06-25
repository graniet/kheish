//! Shared secret and auth-store helpers for CLI commands.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

/// Resolves the control-plane bearer token configured on the CLI.
pub(crate) fn resolve_cli_control_plane_token(cli: &crate::Cli) -> Result<Option<String>> {
    read_secret_arg(
        cli.token.clone(),
        cli.token_file.as_deref(),
        "--token",
        "--token-file",
    )
}

/// Reads one token-like secret from either an inline argument or a file.
pub(crate) fn read_secret_arg(
    inline: Option<String>,
    file: Option<&Path>,
    inline_flag: &str,
    file_flag: &str,
) -> Result<Option<String>> {
    match (inline, file) {
        (Some(_), Some(_)) => bail!("use either {inline_flag} or {file_flag}, not both"),
        (Some(secret), None) => {
            let trimmed = secret.trim().to_string();
            if trimmed.is_empty() {
                bail!("{inline_flag} cannot be empty");
            }
            Ok(Some(trimmed))
        }
        (None, Some(file)) => {
            let raw = std::fs::read_to_string(file)
                .with_context(|| format!("failed to read {}", file.display()))?;
            let trimmed = raw.trim().to_string();
            if trimmed.is_empty() {
                bail!("{file_flag} resolved to an empty token");
            }
            Ok(Some(trimmed))
        }
        (None, None) => Ok(None),
    }
}

/// Reads one secret value from exactly one configured source.
pub(crate) fn read_secret_value(
    inline: Option<String>,
    env_name: Option<&str>,
    file: Option<&Path>,
    stdin: bool,
    inline_flag: &str,
) -> Result<String> {
    let configured_count = usize::from(inline.is_some())
        + usize::from(env_name.is_some())
        + usize::from(file.is_some())
        + usize::from(stdin);
    if configured_count != 1 {
        bail!("provide exactly one of {inline_flag}, --from-env, --from-file, or --stdin");
    }
    if let Some(inline) = inline {
        if inline.trim().is_empty() {
            bail!("{inline_flag} cannot be empty");
        }
        return Ok(inline);
    }
    if let Some(env_name) = env_name {
        let raw = std::env::var(env_name)
            .with_context(|| format!("failed to read environment variable {env_name}"))?;
        if raw.trim().is_empty() {
            bail!("environment variable {env_name} resolved to an empty secret");
        }
        return Ok(raw);
    }
    if let Some(file) = file {
        let raw = std::fs::read_to_string(file)
            .with_context(|| format!("failed to read {}", file.display()))?;
        let value = strip_single_trailing_line_ending(raw);
        if value.trim().is_empty() {
            bail!("--from-file resolved to an empty secret");
        }
        return Ok(value);
    }
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .context("failed to read secret from stdin")?;
    let value = strip_single_trailing_line_ending(raw);
    if value.trim().is_empty() {
        bail!("--stdin resolved to an empty secret");
    }
    Ok(value)
}

fn strip_single_trailing_line_ending(mut value: String) -> String {
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    value
}

/// Returns the path of the daemon-global auth slot store inside one state root.
pub(crate) fn global_auth_store_path(state_root: &Path) -> PathBuf {
    state_root.join("auth/global-slots.json")
}

/// Ensures the auth store master key is available before mutating the secret store.
pub(crate) fn ensure_secret_manager_master_key_configured() -> Result<()> {
    kheish_auth::load_auth_store_master_key_from_env()?.ok_or_else(|| {
        anyhow!(
            "{} must be set before using the daemon secret manager",
            kheish_auth::AUTH_STORE_MASTER_KEY_ENV
        )
    })?;
    Ok(())
}

/// Builds one provider-specific auth slot record from CLI inputs.
pub(crate) fn build_auth_record(
    slot_id: kheish_auth::AuthSlotId,
    provider: crate::SecretProviderKind,
    value: String,
    organization: Option<String>,
    project: Option<String>,
) -> Result<kheish_auth::AuthSlotRecord> {
    match provider {
        crate::SecretProviderKind::Generic => {
            kheish_auth::GenericAuthBackend::static_secret_record(slot_id, &value)
        }
        crate::SecretProviderKind::Anthropic => {
            kheish_auth::AnthropicAuthBackend::static_api_key_record(slot_id, &value)
        }
        crate::SecretProviderKind::Google => {
            kheish_auth::GoogleAuthBackend::static_api_key_record(slot_id, &value)
        }
        crate::SecretProviderKind::Openai => kheish_auth::OpenAiAuthBackend::static_api_key_record(
            slot_id,
            &value,
            organization,
            project,
        ),
        crate::SecretProviderKind::Openrouter => {
            kheish_auth::OpenRouterAuthBackend::static_api_key_record(slot_id, &value)
        }
        crate::SecretProviderKind::Xai => {
            kheish_auth::XAiAuthBackend::static_api_key_record(slot_id, &value)
        }
    }
}

/// Rejects provider-specific records for daemon-managed opaque secret slots.
pub(crate) fn ensure_connector_secret_slot_record_allowed(
    record: &kheish_auth::AuthSlotRecord,
) -> Result<()> {
    if (record.slot_id.0.starts_with("connectors.") || record.slot_id.0.starts_with("mcp."))
        && !matches!(
            record.provider,
            kheish_auth::AuthProvider::Generic | kheish_auth::AuthProvider::McpOAuth
        )
    {
        bail!("connector and MCP secret slots must use generic opaque or MCP OAuth records");
    }
    if record.provider == kheish_auth::AuthProvider::McpOAuth
        && !record.slot_id.0.starts_with("mcp.oauth.")
    {
        bail!("MCP OAuth account slots must use the `mcp.oauth.` namespace");
    }
    Ok(())
}

/// Builds and validates one secret-store record from `secrets set` CLI inputs.
pub(crate) fn build_secret_set_record(
    args: &crate::SecretSetArgs,
) -> Result<kheish_auth::AuthSlotRecord> {
    let value = read_secret_value(
        args.value.clone(),
        args.from_env.as_deref(),
        args.from_file.as_deref(),
        args.stdin,
        "--value",
    )?;
    let record = build_auth_record(
        kheish_auth::AuthSlotId::new(args.secret_ref.clone()),
        args.provider,
        value,
        args.organization.clone(),
        args.project.clone(),
    )?;
    ensure_connector_secret_slot_record_allowed(&record)?;
    Ok(record)
}

/// Builds and validates one imported Codex auth record from CLI inputs.
pub(crate) fn build_codex_import_record(
    args: &crate::SecretImportCodexArgs,
) -> Result<kheish_auth::AuthSlotRecord> {
    let file = args
        .file
        .clone()
        .or_else(default_codex_auth_path)
        .ok_or_else(|| {
            anyhow!("no Codex auth file was provided and no default auth.json was found")
        })?;
    let issuer = std::env::var("KHEISH_OPENAI_AUTH_ISSUER")
        .unwrap_or_else(|_| kheish_auth::DEFAULT_OPENAI_AUTH_ISSUER.to_string());
    let client_id = std::env::var("KHEISH_OPENAI_AUTH_CLIENT_ID")
        .unwrap_or_else(|_| kheish_auth::DEFAULT_CODEX_CLIENT_ID.to_string());
    let api_base_url = std::env::var("KHEISH_OPENAI_CODEX_API_BASE_URL")
        .unwrap_or_else(|_| kheish_auth::DEFAULT_OPENAI_CODEX_API_BASE_URL.to_string());
    let record = kheish_auth::OpenAiAuthBackend::import_codex_record_with_overrides(
        kheish_auth::AuthSlotId::new(args.secret_ref.clone()),
        file,
        issuer,
        client_id,
        api_base_url,
        args.organization.clone(),
        args.project.clone(),
    )?;
    ensure_connector_secret_slot_record_allowed(&record)?;
    Ok(record)
}

/// Builds and validates one imported Claude Code auth record from CLI inputs.
pub(crate) fn build_claude_code_import_record(
    args: &crate::SecretImportClaudeCodeArgs,
) -> Result<kheish_auth::AuthSlotRecord> {
    let file = args
        .file
        .clone()
        .or_else(kheish_auth::default_claude_code_credentials_path)
        .ok_or_else(|| {
            anyhow!(
                "no Claude Code credentials file was provided and no default credentials path was found"
            )
        })?;
    let token_url = std::env::var("KHEISH_ANTHROPIC_OAUTH_TOKEN_URL")
        .unwrap_or_else(|_| kheish_auth::DEFAULT_ANTHROPIC_OAUTH_TOKEN_URL.to_string());
    let client_id = std::env::var("KHEISH_ANTHROPIC_OAUTH_CLIENT_ID")
        .unwrap_or_else(|_| kheish_auth::DEFAULT_CLAUDE_CODE_CLIENT_ID.to_string());
    let record = kheish_auth::AnthropicAuthBackend::import_claude_code_record_with_overrides(
        kheish_auth::AuthSlotId::new(args.secret_ref.clone()),
        file,
        token_url,
        client_id,
    )?;
    ensure_connector_secret_slot_record_allowed(&record)?;
    Ok(record)
}

/// Checks whether the daemon control plane is reachable.
pub(crate) async fn daemon_control_plane_is_reachable(
    client: &crate::cli::DaemonHttpClient,
) -> bool {
    client
        .request_status(reqwest::Method::GET, "/v1/capabilities")
        .await
        .is_ok()
}

/// Resolves the local auth-store path used by offline secret operations.
pub(crate) fn local_secret_store_path(args: &crate::SecretStoreArgs) -> Result<PathBuf> {
    args.state_root.clone().ok_or_else(|| {
        anyhow!(
            "offline secret store operations require an explicit --state-root or KHEISH_STATE_ROOT"
        )
    })
}

/// Returns the storage-routing arguments embedded in one secrets subcommand.
pub(crate) fn secrets_command_store(
    command: &crate::SecretsCommand,
) -> Option<&crate::SecretStoreArgs> {
    match command {
        crate::SecretsCommand::Generate => None,
        crate::SecretsCommand::List(args) => Some(args),
        crate::SecretsCommand::Get(args) => Some(&args.store),
        crate::SecretsCommand::Set(args) => Some(&args.store),
        crate::SecretsCommand::ImportCodex(args) => Some(&args.store),
        crate::SecretsCommand::ImportClaudeCode(args) => Some(&args.store),
        crate::SecretsCommand::Delete(args) => Some(&args.store),
    }
}

/// Returns the default Codex auth path inside the local Codex home.
pub(crate) fn default_codex_auth_path() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".codex"))
        })
        .map(|root| root.join("auth.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_secret_value_requires_exactly_one_source() {
        let error = read_secret_value(
            Some("value".to_string()),
            Some("IGNORED"),
            None,
            false,
            "--value",
        )
        .expect_err("expected error");
        assert!(
            error
                .to_string()
                .contains("provide exactly one of --value, --from-env, --from-file, or --stdin")
        );
    }

    #[test]
    fn read_secret_value_preserves_opaque_edge_spaces() {
        let inline = read_secret_value(
            Some("  opaque secret  ".to_string()),
            None,
            None,
            false,
            "--value",
        )
        .expect("inline secret");
        assert_eq!(inline, "  opaque secret  ");

        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("secret.txt");
        std::fs::write(&path, "  file secret  \n").expect("write secret");
        let file =
            read_secret_value(None, None, Some(&path), false, "--value").expect("file secret");
        assert_eq!(file, "  file secret  ");
    }

    #[test]
    fn build_secret_set_record_rejects_provider_specific_connector_slots() {
        let error = build_secret_set_record(&crate::SecretSetArgs {
            secret_ref: "connectors.demo".to_string(),
            provider: crate::SecretProviderKind::Openai,
            value: Some("test-secret".to_string()),
            from_env: None,
            from_file: None,
            stdin: false,
            organization: None,
            project: None,
            store: crate::SecretStoreArgs {
                state_root: None,
                offline: false,
            },
        })
        .expect_err("connector-backed secrets should reject provider-specific records");
        assert!(error.to_string().contains(
            "connector and MCP secret slots must use generic opaque or MCP OAuth records"
        ));
    }

    #[test]
    fn build_secret_set_record_allows_generic_mcp_slots() {
        let record = build_secret_set_record(&crate::SecretSetArgs {
            secret_ref: "mcp.linear.LINEAR_API_KEY".to_string(),
            provider: crate::SecretProviderKind::Generic,
            value: Some("linear-secret".to_string()),
            from_env: None,
            from_file: None,
            stdin: false,
            organization: None,
            project: None,
            store: crate::SecretStoreArgs {
                state_root: None,
                offline: false,
            },
        })
        .expect("generic MCP slots should be valid");
        assert_eq!(record.provider, kheish_auth::AuthProvider::Generic);
        assert_eq!(record.mode, kheish_auth::AuthMode::OpaqueSecret);
    }
}
