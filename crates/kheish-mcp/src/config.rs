use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use kheish_runtime::redact_text;
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::catalog::{McpResolvedSecrets, expand_catalog_profiles_with_secrets};
use crate::client::load_codex_bearer_token;

const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_TOOL_TIMEOUT_MS: u64 = 120_000;

/// One supported MCP server transport.
#[derive(Clone, PartialEq, Eq)]
pub enum McpServerTransport {
    /// Child-process stdio transport.
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    /// Streamable HTTP transport.
    StreamableHttp {
        url: String,
        headers: BTreeMap<String, String>,
        auth: McpHttpAuth,
    },
}

impl fmt::Debug for McpServerTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio {
                command,
                args,
                env,
                cwd,
            } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", &redacted_args(args))
                .field("env", &redacted_keys(env))
                .field("cwd", cwd)
                .finish(),
            Self::StreamableHttp { url, headers, auth } => f
                .debug_struct("StreamableHttp")
                .field("url", &redacted_url(url))
                .field("headers", &redacted_keys(headers))
                .field("auth", auth)
                .finish(),
        }
    }
}

/// HTTP auth mode for one streamable HTTP MCP server.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum McpHttpAuth {
    #[default]
    None,
    BearerToken {
        token: String,
    },
    OAuth {
        slot_id: String,
        resource: String,
        scopes: Vec<String>,
    },
}

impl std::fmt::Debug for McpHttpAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::BearerToken { .. } => f
                .debug_struct("BearerToken")
                .field("token", &"<redacted>")
                .finish(),
            Self::OAuth {
                slot_id,
                resource: _,
                scopes,
            } => f
                .debug_struct("OAuth")
                .field("slot_id", slot_id)
                .field("resource", &"<redacted>")
                .field("scopes", scopes)
                .finish(),
        }
    }
}

/// One daemon-usable MCP server definition.
#[derive(Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    /// Stable server name.
    pub name: String,
    /// Startup handshake timeout in milliseconds.
    pub startup_timeout_ms: u64,
    /// Default tool call timeout in milliseconds.
    pub tool_timeout_ms: u64,
    /// Whether startup failure should fail daemon startup.
    pub required: bool,
    /// Optional allow-list of MCP tools to expose.
    pub enabled_tools: Vec<String>,
    /// Optional deny-list of MCP tools to hide.
    pub disabled_tools: Vec<String>,
    /// Whether stdio child processes inherit the daemon environment.
    pub inherit_env: bool,
    /// Daemon auth-store slots used by this MCP server, without secret values.
    pub credential_secret_refs: Vec<String>,
    /// Configured transport.
    pub transport: McpServerTransport,
}

impl fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpServerConfig")
            .field("name", &self.name)
            .field("startup_timeout_ms", &self.startup_timeout_ms)
            .field("tool_timeout_ms", &self.tool_timeout_ms)
            .field("required", &self.required)
            .field("enabled_tools", &self.enabled_tools)
            .field("disabled_tools", &self.disabled_tools)
            .field("inherit_env", &self.inherit_env)
            .field("credential_secret_refs", &self.credential_secret_refs)
            .field("transport", &self.transport)
            .finish()
    }
}

fn redacted_keys(values: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
    values
        .keys()
        .map(|key| (key.as_str(), "<redacted>"))
        .collect()
}

fn redacted_args(args: &[String]) -> Vec<String> {
    let mut redact_next = false;
    args.iter()
        .map(|arg| {
            if redact_next && !arg.starts_with('-') {
                redact_next = false;
                return "<redacted>".to_string();
            }
            redact_next = false;
            if let Some((flag, _value)) = arg.split_once('=') {
                if arg_key_looks_sensitive(flag) {
                    return format!("{flag}=<redacted>");
                }
            }
            if arg_key_looks_sensitive(arg) {
                redact_next = true;
            }
            redact_text(arg)
        })
        .collect()
}

fn arg_key_looks_sensitive(value: &str) -> bool {
    let lower = value
        .trim_start_matches('-')
        .trim_start_matches('/')
        .to_ascii_lowercase();
    if [
        "api-key",
        "api_key",
        "apikey",
        "access-key",
        "access_key",
        "accesskey",
        "client-secret",
        "client_secret",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return true;
    }
    lower
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .any(|token| {
            matches!(
                token,
                "auth" | "authorization" | "bearer" | "password" | "secret" | "token"
            )
        })
}

fn redacted_url(value: &str) -> String {
    let Ok(mut url) = Url::parse(value) else {
        return "<redacted-url>".to_string();
    };
    if !url.username().is_empty() {
        let _ = url.set_username("<redacted>");
    }
    if url.password().is_some() {
        let _ = url.set_password(Some("<redacted>"));
    }
    if url.query().is_some() {
        url.set_query(Some("<redacted>"));
    }
    if url.fragment().is_some() {
        url.set_fragment(Some("<redacted>"));
    }
    redact_text(&url.to_string())
}

/// Compatibility options for importing Codex-style MCP configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexCompatOptions {
    /// Optional explicit config path.
    pub config_path: Option<PathBuf>,
    /// Optional explicit credentials path.
    pub credentials_path: Option<PathBuf>,
    /// Daemon-provided secret values, keyed by auth-store slot reference.
    pub resolved_secrets: McpResolvedSecrets,
}

/// Source-neutral MCP load options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpLoadOptions {
    /// Optional Codex-compatible config import.
    pub codex: CodexCompatOptions,
    /// Built-in catalog profiles selected by the operator.
    pub catalog_profiles: Vec<String>,
    /// Daemon-provided catalog credential values, keyed by auth-store slot reference.
    pub resolved_secrets: McpResolvedSecrets,
}

/// Provenance of one loaded MCP server definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpServerSource {
    /// Imported from Codex-compatible MCP configuration.
    CodexConfig,
    /// Expanded from one built-in Kheish catalog profile.
    BuiltInCatalog {
        profiles: Vec<String>,
        entry_id: String,
    },
    /// Registered through the daemon runtime API while the daemon runs.
    RuntimeApi,
}

/// One loaded MCP server plus source metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedMcpServerConfig {
    /// Server configuration consumed by the manager.
    pub config: McpServerConfig,
    /// Where the server came from.
    pub source: McpServerSource,
}

/// Loaded MCP server definitions and provenance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpLoadResult {
    /// Codex-compatible config path used when present.
    pub config_path: Option<PathBuf>,
    /// Normalized built-in catalog profile names.
    pub selected_profiles: Vec<String>,
    /// Loaded server definitions.
    pub servers: Vec<LoadedMcpServerConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct CodexConfigFile {
    #[serde(default)]
    mcp_servers: BTreeMap<String, CodexServerConfig>,
}

/// One Codex-compatible `[mcp_servers.<name>]` entry.
///
/// Public so the daemon runtime API can accept exactly this shape as JSON
/// and persist accepted entries in its state-root overlay.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CodexServerConfig {
    enabled: Option<bool>,
    required: Option<bool>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    env_secret_refs: BTreeMap<String, String>,
    inherit_env: Option<bool>,
    cwd: Option<String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    env_http_headers: BTreeMap<String, String>,
    #[serde(default)]
    http_header_secret_refs: BTreeMap<String, String>,
    bearer_token_env_var: Option<String>,
    bearer_token_secret_ref: Option<String>,
    oauth_slot_ref: Option<String>,
    oauth_resource: Option<String>,
    #[serde(default)]
    oauth_scopes: Vec<String>,
    #[serde(default)]
    enabled_tools: Vec<String>,
    #[serde(default)]
    disabled_tools: Vec<String>,
    startup_timeout_sec: Option<u64>,
    tool_timeout_sec: Option<u64>,
}

/// Returns the default Codex-compatible MCP config path when available.
pub fn default_codex_mcp_config_path() -> Option<PathBuf> {
    let home = env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".codex/config.toml");
    path.exists().then_some(path)
}

/// Returns the default Codex-compatible MCP credentials path when available.
pub fn default_codex_credentials_path() -> Option<PathBuf> {
    let home = env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".codex/.credentials.json");
    path.exists().then_some(path)
}

/// Loads MCP server definitions from a Codex-compatible config file.
pub fn load_codex_mcp_servers(options: &CodexCompatOptions) -> Result<Vec<McpServerConfig>> {
    let Some(config_path) = options
        .config_path
        .clone()
        .or_else(default_codex_mcp_config_path)
    else {
        return Ok(Vec::new());
    };
    let content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read MCP config {}", config_path.display()))?;
    let parsed: CodexConfigFile = toml::from_str(&content)
        .with_context(|| format!("failed to parse MCP config {}", config_path.display()))?;
    let credentials_path = options
        .credentials_path
        .clone()
        .or_else(default_codex_credentials_path);
    let mut servers = Vec::new();
    for (name, server) in parsed.mcp_servers {
        if let Some(config) =
            codex_server_to_config(name, server, options, credentials_path.as_deref())?
        {
            servers.push(config);
        }
    }
    servers.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(servers)
}

/// Converts one Codex-compatible entry into a runtime server config.
///
/// Returns `Ok(None)` when the entry is disabled, references a revoked
/// secret, or declares no transport — mirroring how config-file loading
/// skips such entries. Secret refs resolve through `options.resolved_secrets`
/// exactly like at boot.
pub fn codex_server_to_config(
    name: String,
    server: CodexServerConfig,
    options: &CodexCompatOptions,
    credentials_path: Option<&std::path::Path>,
) -> Result<Option<McpServerConfig>> {
    if matches!(server.enabled, Some(false)) {
        return Ok(None);
    }
    let startup_timeout_ms = server
        .startup_timeout_sec
        .unwrap_or(DEFAULT_STARTUP_TIMEOUT_MS / 1_000)
        * 1_000;
    let tool_timeout_ms = server
        .tool_timeout_sec
        .unwrap_or(DEFAULT_TOOL_TIMEOUT_MS / 1_000)
        * 1_000;
    let is_stdio = server.command.is_some();
    let is_http = server.url.is_some();
    validate_transport_specific_fields(&name, &server, is_stdio, is_http)?;
    let inherit_env = if is_stdio {
        resolve_stdio_inherit_env(
            &name,
            server.inherit_env,
            !server.env_secret_refs.is_empty(),
        )?
    } else {
        true
    };
    let mut credential_secret_refs = if is_stdio {
        stdio_secret_refs(&server)
    } else if is_http {
        http_secret_refs(&server)
    } else {
        Vec::new()
    };
    if credential_secret_refs.iter().any(|secret_ref| {
        options
            .resolved_secrets
            .revoked_secret_refs
            .contains(secret_ref)
    }) {
        return Ok(None);
    }
    let transport = if let Some(command) = server.command {
        let env = resolve_stdio_env(&server.env, &server.env_secret_refs, options)?;
        McpServerTransport::Stdio {
            command,
            args: server.args,
            env,
            cwd: server.cwd.map(PathBuf::from),
        }
    } else if let Some(url) = server.url {
        let has_authorization_header = configured_http_headers_include(
            &server.headers,
            &server.env_http_headers,
            &server.http_header_secret_refs,
            "authorization",
        );
        let has_bearer_source =
            server.bearer_token_env_var.is_some() || server.bearer_token_secret_ref.is_some();
        let has_oauth_source = server.oauth_slot_ref.is_some();
        if has_authorization_header && (has_bearer_source || has_oauth_source) {
            return Err(anyhow!(
                "MCP server `{name}` cannot configure Authorization header and managed HTTP auth"
            ));
        }
        if has_bearer_source && has_oauth_source {
            return Err(anyhow!(
                "MCP server `{name}` cannot configure both bearer token auth and OAuth auth"
            ));
        }
        let headers = resolve_http_headers(
            &name,
            &server.headers,
            &server.env_http_headers,
            &server.http_header_secret_refs,
            options,
        )?;
        let bearer_token = resolve_optional_secret_ref(
            &options.resolved_secrets,
            server.bearer_token_secret_ref.as_deref(),
            &format!("MCP server `{name}` bearer token"),
        )?
        .or_else(|| {
            server
                .bearer_token_env_var
                .as_deref()
                .and_then(env::var_os)
                .map(|value| value.to_string_lossy().into_owned())
        })
        .or_else(|| {
            credentials_path.and_then(|path| load_codex_bearer_token(path, &name, &url).ok())
        });
        if has_authorization_header && bearer_token.is_some() {
            return Err(anyhow!(
                "MCP server `{name}` cannot configure Authorization header and bearer token auth"
            ));
        }
        let auth = if let Some(oauth_slot_ref) = server.oauth_slot_ref {
            if !oauth_slot_ref.starts_with("mcp.oauth.") {
                return Err(anyhow!(
                    "MCP server `{name}` OAuth slot refs must use the `mcp.oauth.` namespace"
                ));
            }
            let resource = server.oauth_resource.unwrap_or_else(|| url.clone());
            credential_secret_refs.push(oauth_slot_ref.clone());
            McpHttpAuth::OAuth {
                slot_id: oauth_slot_ref,
                resource,
                scopes: normalize_entries(&server.oauth_scopes),
            }
        } else if let Some(token) = bearer_token {
            McpHttpAuth::BearerToken { token }
        } else {
            McpHttpAuth::None
        };
        McpServerTransport::StreamableHttp { url, headers, auth }
    } else {
        return Ok(None);
    };
    credential_secret_refs.sort();
    credential_secret_refs.dedup();
    Ok(Some(McpServerConfig {
        name,
        startup_timeout_ms,
        tool_timeout_ms,
        required: server.required.unwrap_or(false),
        enabled_tools: server.enabled_tools,
        disabled_tools: server.disabled_tools,
        inherit_env,
        credential_secret_refs,
        transport,
    }))
}

fn validate_transport_specific_fields(
    server_name: &str,
    server: &CodexServerConfig,
    is_stdio: bool,
    is_http: bool,
) -> Result<()> {
    if is_stdio && is_http {
        return Err(anyhow!(
            "MCP server `{server_name}` cannot configure both command and url"
        ));
    }
    if is_stdio
        && (!server.headers.is_empty()
            || !server.env_http_headers.is_empty()
            || !server.http_header_secret_refs.is_empty()
            || server.bearer_token_env_var.is_some()
            || server.bearer_token_secret_ref.is_some()
            || server.oauth_slot_ref.is_some()
            || server.oauth_resource.is_some()
            || !server.oauth_scopes.is_empty())
    {
        return Err(anyhow!(
            "MCP stdio server `{server_name}` cannot configure HTTP auth or header fields"
        ));
    }
    if is_http
        && (!server.args.is_empty()
            || !server.env.is_empty()
            || !server.env_secret_refs.is_empty()
            || server.cwd.is_some()
            || server.inherit_env.is_some())
    {
        return Err(anyhow!(
            "MCP HTTP server `{server_name}` cannot configure stdio command fields"
        ));
    }
    Ok(())
}

fn stdio_secret_refs(server: &CodexServerConfig) -> Vec<String> {
    server.env_secret_refs.values().cloned().collect()
}

fn http_secret_refs(server: &CodexServerConfig) -> Vec<String> {
    server
        .http_header_secret_refs
        .values()
        .chain(server.bearer_token_secret_ref.iter())
        .chain(server.oauth_slot_ref.iter())
        .cloned()
        .collect()
}

fn resolve_stdio_inherit_env(
    server_name: &str,
    configured: Option<bool>,
    has_secret_refs: bool,
) -> Result<bool> {
    if has_secret_refs {
        if configured == Some(true) {
            return Err(anyhow!(
                "MCP stdio server `{server_name}` cannot use inherit_env=true with env_secret_refs"
            ));
        }
        return Ok(false);
    }
    Ok(configured.unwrap_or(true))
}

fn resolve_stdio_env(
    env: &BTreeMap<String, String>,
    env_secret_refs: &BTreeMap<String, String>,
    options: &CodexCompatOptions,
) -> Result<BTreeMap<String, String>> {
    let mut resolved = env.clone();
    for (env_name, secret_ref) in env_secret_refs {
        if resolved.contains_key(env_name) {
            return Err(anyhow!(
                "MCP stdio env `{env_name}` cannot be configured from both literal env and secret_ref"
            ));
        }
        resolved.insert(
            env_name.clone(),
            resolve_required_secret_ref(
                &options.resolved_secrets,
                secret_ref,
                &format!("MCP stdio env `{env_name}`"),
            )?,
        );
    }
    Ok(resolved)
}

fn resolve_http_headers(
    server_name: &str,
    headers: &BTreeMap<String, String>,
    env_http_headers: &BTreeMap<String, String>,
    header_secret_refs: &BTreeMap<String, String>,
    options: &CodexCompatOptions,
) -> Result<BTreeMap<String, String>> {
    validate_unique_http_header_sources(
        server_name,
        headers,
        env_http_headers,
        header_secret_refs,
    )?;
    let mut resolved = headers.clone();
    for (header_name, env_key) in env_http_headers {
        if let Some(value) = env::var_os(env_key) {
            resolved.insert(header_name.clone(), value.to_string_lossy().into_owned());
        }
    }
    for (header_name, secret_ref) in header_secret_refs {
        resolved.insert(
            header_name.clone(),
            resolve_required_secret_ref(
                &options.resolved_secrets,
                secret_ref,
                &format!("MCP HTTP header `{header_name}`"),
            )?,
        );
    }
    Ok(resolved)
}

fn validate_unique_http_header_sources(
    server_name: &str,
    headers: &BTreeMap<String, String>,
    env_http_headers: &BTreeMap<String, String>,
    header_secret_refs: &BTreeMap<String, String>,
) -> Result<()> {
    let mut seen = BTreeMap::<String, &str>::new();
    for name in headers.keys() {
        record_http_header_source(server_name, &mut seen, name, "headers")?;
    }
    for name in env_http_headers.keys() {
        record_http_header_source(server_name, &mut seen, name, "env_http_headers")?;
    }
    for name in header_secret_refs.keys() {
        record_http_header_source(server_name, &mut seen, name, "http_header_secret_refs")?;
    }
    Ok(())
}

fn record_http_header_source(
    server_name: &str,
    seen: &mut BTreeMap<String, &str>,
    header_name: &str,
    source: &'static str,
) -> Result<()> {
    let canonical = header_name.to_ascii_lowercase();
    if let Some(existing) = seen.insert(canonical, source) {
        return Err(anyhow!(
            "MCP server `{server_name}` configures HTTP header `{header_name}` from both {existing} and {source}"
        ));
    }
    Ok(())
}

fn configured_http_headers_include(
    headers: &BTreeMap<String, String>,
    env_http_headers: &BTreeMap<String, String>,
    header_secret_refs: &BTreeMap<String, String>,
    target: &str,
) -> bool {
    headers
        .keys()
        .chain(env_http_headers.keys())
        .chain(header_secret_refs.keys())
        .any(|name| name.eq_ignore_ascii_case(target))
}

fn resolve_optional_secret_ref(
    secrets: &McpResolvedSecrets,
    secret_ref: Option<&str>,
    label: &str,
) -> Result<Option<String>> {
    let Some(secret_ref) = secret_ref else {
        return Ok(None);
    };
    Ok(Some(resolve_required_secret_ref(
        secrets, secret_ref, label,
    )?))
}

fn resolve_required_secret_ref(
    secrets: &McpResolvedSecrets,
    secret_ref: &str,
    label: &str,
) -> Result<String> {
    if secret_ref.trim().is_empty() {
        return Err(anyhow!("{label} secret_ref cannot be empty"));
    }
    secrets
        .secret_values
        .get(secret_ref)
        .cloned()
        .ok_or_else(|| anyhow!("missing MCP secret-store slot `{secret_ref}` for {label}"))
}

/// Loads MCP server definitions from explicit config and built-in catalog profiles.
pub fn load_mcp_servers(options: &McpLoadOptions) -> Result<McpLoadResult> {
    let config_path = options.codex.config_path.clone();
    let mut servers = Vec::new();
    if config_path.is_some() {
        for config in load_codex_mcp_servers(&CodexCompatOptions {
            config_path: config_path.clone(),
            credentials_path: options.codex.credentials_path.clone(),
            resolved_secrets: options.resolved_secrets.clone(),
        })? {
            servers.push(LoadedMcpServerConfig {
                config,
                source: McpServerSource::CodexConfig,
            });
        }
    }

    let catalog_profiles = normalize_catalog_profiles(&options.catalog_profiles);
    for (config, profiles, entry_id) in
        expand_catalog_profiles_with_secrets(&catalog_profiles, &options.resolved_secrets)?
    {
        servers.push(LoadedMcpServerConfig {
            config,
            source: McpServerSource::BuiltInCatalog { profiles, entry_id },
        });
    }
    validate_loaded_servers(&servers)?;
    if !catalog_profiles.is_empty()
        && !servers
            .iter()
            .any(|server| matches!(server.source, McpServerSource::BuiltInCatalog { .. }))
    {
        return Err(anyhow!(
            "selected MCP catalog profiles have no supported entries: {}",
            catalog_profiles.join(",")
        ));
    }
    Ok(McpLoadResult {
        config_path,
        selected_profiles: catalog_profiles,
        servers,
    })
}

fn normalize_catalog_profiles(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn normalize_entries(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn validate_loaded_servers(servers: &[LoadedMcpServerConfig]) -> Result<()> {
    let mut names = BTreeMap::<&str, &McpServerSource>::new();
    for server in servers {
        if let Some(existing) = names.insert(&server.config.name, &server.source) {
            return Err(anyhow!(
                "duplicate MCP server name `{}` from {:?} and {:?}",
                server.config.name,
                existing,
                server.source
            ));
        }
    }
    Ok(())
}

pub(crate) fn resolve_cwd(workspace_root: &Path, cwd: Option<&Path>) -> PathBuf {
    match cwd {
        Some(cwd) if cwd.is_relative() => workspace_root.join(cwd),
        Some(cwd) => cwd.to_path_buf(),
        None => workspace_root.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        CodexCompatOptions, DEFAULT_STARTUP_TIMEOUT_MS, DEFAULT_TOOL_TIMEOUT_MS, McpHttpAuth,
        McpLoadOptions, McpResolvedSecrets, McpServerConfig, McpServerSource, McpServerTransport,
        load_codex_mcp_servers, load_mcp_servers, resolve_cwd,
    };
    use std::collections::{BTreeMap, BTreeSet};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("kheish-mcp-{name}-{nanos}"))
    }

    #[test]
    fn load_codex_mcp_servers_imports_filters_headers_and_credentials() {
        let config_path = temp_path("config.toml");
        let credentials_path = temp_path("credentials.json");
        fs::write(
            &config_path,
            r#"
[mcp_servers.openaiDeveloperDocs]
url = "https://developers.openai.com/mcp"
required = true
enabled_tools = ["search_openai_docs"]
disabled_tools = ["fetch_openai_doc"]

[mcp_servers.linear]
url = "https://mcp.linear.app/mcp"
headers = { "x-linear-test" = "header-value" }
"#,
        )
        .expect("config should be written");
        fs::write(
            &credentials_path,
            r#"{
  "linear-entry": {
    "server_name": "linear",
    "server_url": "https://mcp.linear.app/mcp",
    "access_token": "linear-token"
  }
}"#,
        )
        .expect("credentials should be written");

        let result = load_codex_mcp_servers(&CodexCompatOptions {
            config_path: Some(config_path.clone()),
            credentials_path: Some(credentials_path.clone()),
            resolved_secrets: Default::default(),
        })
        .expect("config should parse");

        fs::remove_file(config_path).ok();
        fs::remove_file(credentials_path).ok();

        assert_eq!(result.len(), 2);
        let docs = result
            .iter()
            .find(|server| server.name == "openaiDeveloperDocs")
            .expect("docs server should exist");
        assert!(docs.required);
        assert_eq!(docs.enabled_tools, vec!["search_openai_docs"]);
        assert_eq!(docs.disabled_tools, vec!["fetch_openai_doc"]);

        let linear = result
            .iter()
            .find(|server| server.name == "linear")
            .expect("linear server should exist");
        match &linear.transport {
            McpServerTransport::StreamableHttp { headers, auth, .. } => {
                assert_eq!(
                    headers.get("x-linear-test").map(String::as_str),
                    Some("header-value")
                );
                assert_eq!(
                    auth,
                    &super::McpHttpAuth::BearerToken {
                        token: "linear-token".to_string()
                    }
                );
            }
            other => panic!("expected streamable HTTP transport, got {other:?}"),
        }
    }

    #[test]
    fn load_codex_mcp_servers_resolves_secret_refs() {
        let config_path = temp_path("secret-refs.toml");
        fs::write(
            &config_path,
            r#"
[mcp_servers.secretHttp]
url = "https://example.com/mcp"
bearer_token_secret_ref = "mcp.custom.secretHttp.BEARER_TOKEN"
http_header_secret_refs = { "x-api-key" = "mcp.custom.secretHttp.X_API_KEY" }

[mcp_servers.secretStdio]
command = "secret-mcp"
env_secret_refs = { "SECRET_TOKEN" = "mcp.custom.secretStdio.SECRET_TOKEN" }
"#,
        )
        .expect("config should be written");

        let result = load_codex_mcp_servers(&CodexCompatOptions {
            config_path: Some(config_path.clone()),
            credentials_path: None,
            resolved_secrets: McpResolvedSecrets {
                secret_values: BTreeMap::from([
                    (
                        "mcp.custom.secretHttp.BEARER_TOKEN".to_string(),
                        "bearer-secret".to_string(),
                    ),
                    (
                        "mcp.custom.secretHttp.X_API_KEY".to_string(),
                        "header-secret".to_string(),
                    ),
                    (
                        "mcp.custom.secretStdio.SECRET_TOKEN".to_string(),
                        "stdio-secret".to_string(),
                    ),
                ]),
                revoked_secret_refs: Default::default(),
                allow_env_fallback: false,
            },
        })
        .expect("config should parse");
        fs::remove_file(config_path).ok();

        let http = result
            .iter()
            .find(|server| server.name == "secretHttp")
            .expect("secret HTTP server should exist");
        assert_eq!(
            http.credential_secret_refs,
            vec![
                "mcp.custom.secretHttp.BEARER_TOKEN".to_string(),
                "mcp.custom.secretHttp.X_API_KEY".to_string(),
            ]
        );
        match &http.transport {
            McpServerTransport::StreamableHttp { auth, headers, .. } => {
                assert_eq!(
                    auth,
                    &super::McpHttpAuth::BearerToken {
                        token: "bearer-secret".to_string()
                    }
                );
                assert_eq!(
                    headers.get("x-api-key").map(String::as_str),
                    Some("header-secret")
                );
            }
            other => panic!("expected HTTP transport, got {other:?}"),
        }

        let stdio = result
            .iter()
            .find(|server| server.name == "secretStdio")
            .expect("secret stdio server should exist");
        assert!(
            !stdio.inherit_env,
            "secret-backed stdio MCP servers must not inherit daemon env"
        );
        assert_eq!(
            stdio.credential_secret_refs,
            vec!["mcp.custom.secretStdio.SECRET_TOKEN".to_string()]
        );
        match &stdio.transport {
            McpServerTransport::Stdio { env, .. } => {
                assert_eq!(
                    env.get("SECRET_TOKEN").map(String::as_str),
                    Some("stdio-secret")
                );
            }
            other => panic!("expected stdio transport, got {other:?}"),
        }
    }

    #[test]
    fn mcp_config_debug_redacts_resolved_env_and_header_values() {
        kheish_auth::register_ephemeral_debug_redaction_token("path-secret-canary");
        let config = McpServerConfig {
            name: "secret".to_string(),
            startup_timeout_ms: DEFAULT_STARTUP_TIMEOUT_MS,
            tool_timeout_ms: DEFAULT_TOOL_TIMEOUT_MS,
            required: false,
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            inherit_env: false,
            credential_secret_refs: vec![
                "mcp.custom.secret.HEADER".to_string(),
                "mcp.custom.secret.ENV".to_string(),
            ],
            transport: McpServerTransport::StreamableHttp {
                url: "https://user-canary:pass-canary@example.test/path-secret-canary/mcp?api_key=query-canary#frag-canary".to_string(),
                headers: BTreeMap::from([
                    (
                        "Authorization".to_string(),
                        "Bearer header-canary".to_string(),
                    ),
                    ("X-Custom".to_string(), "custom-header-canary".to_string()),
                ]),
                auth: McpHttpAuth::OAuth {
                    slot_id: "mcp.oauth.secret".to_string(),
                    resource: "https://resource.example/mcp?token=resource-canary".to_string(),
                    scopes: vec!["tools".to_string()],
                },
            },
        };
        kheish_auth::register_ephemeral_debug_redaction_token("arg-token-canary");
        let stdio = McpServerConfig {
            transport: McpServerTransport::Stdio {
                command: "secret-mcp".to_string(),
                args: vec![
                    "--token=arg-token-canary".to_string(),
                    "--api-key".to_string(),
                    "arg-value-canary".to_string(),
                ],
                env: BTreeMap::from([("MCP_TOKEN".to_string(), "env-canary".to_string())]),
                cwd: None,
            },
            ..config.clone()
        };

        let debug = format!("{config:?}\n{stdio:?}");

        for canary in [
            "header-canary",
            "custom-header-canary",
            "env-canary",
            "user-canary",
            "pass-canary",
            "path-secret-canary",
            "query-canary",
            "frag-canary",
            "resource-canary",
            "arg-token-canary",
            "arg-value-canary",
        ] {
            assert!(!debug.contains(canary), "debug leaked {canary}: {debug}");
        }
        assert!(debug.contains("--token=<redacted>"));
        assert!(debug.contains("--api-key"));
        assert!(debug.contains("Authorization"));
        assert!(debug.contains("MCP_TOKEN"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn load_codex_mcp_servers_skips_revoked_secret_refs() {
        let config_path = temp_path("revoked-secret-refs.toml");
        fs::write(
            &config_path,
            r#"
[mcp_servers.revoked]
command = "secret-mcp"
env_secret_refs = { "SECRET_TOKEN" = "mcp.custom.revoked.SECRET_TOKEN" }

[mcp_servers.active]
command = "active-mcp"
"#,
        )
        .expect("config should be written");

        let result = load_codex_mcp_servers(&CodexCompatOptions {
            config_path: Some(config_path.clone()),
            credentials_path: None,
            resolved_secrets: McpResolvedSecrets {
                secret_values: BTreeMap::new(),
                revoked_secret_refs: BTreeSet::from(
                    ["mcp.custom.revoked.SECRET_TOKEN".to_string()],
                ),
                allow_env_fallback: false,
            },
        })
        .expect("revoked secret refs should skip the affected server");
        fs::remove_file(config_path).ok();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "active");
    }

    #[test]
    fn load_codex_mcp_servers_rejects_secret_refs_with_inherit_env() {
        let config_path = temp_path("secret-refs-inherit-env.toml");
        fs::write(
            &config_path,
            r#"
[mcp_servers.secretStdio]
command = "secret-mcp"
inherit_env = true
env_secret_refs = { "SECRET_TOKEN" = "mcp.custom.secretStdio.SECRET_TOKEN" }
"#,
        )
        .expect("config should be written");
        let error = load_codex_mcp_servers(&CodexCompatOptions {
            config_path: Some(config_path.clone()),
            credentials_path: None,
            resolved_secrets: McpResolvedSecrets {
                secret_values: BTreeMap::from([(
                    "mcp.custom.secretStdio.SECRET_TOKEN".to_string(),
                    "stdio-secret".to_string(),
                )]),
                revoked_secret_refs: Default::default(),
                allow_env_fallback: false,
            },
        })
        .expect_err("inherit_env with secret refs should fail");
        fs::remove_file(config_path).ok();
        assert!(
            error
                .to_string()
                .contains("cannot use inherit_env=true with env_secret_refs")
        );
    }

    #[test]
    fn load_codex_mcp_servers_rejects_http_auth_conflicts_case_insensitively() {
        let config_path = temp_path("http-auth-conflict.toml");
        fs::write(
            &config_path,
            r#"
[mcp_servers.secretHttp]
url = "https://example.com/mcp"
env_http_headers = { "Authorization" = "MCP_AUTH_HEADER" }
bearer_token_secret_ref = "mcp.custom.secretHttp.BEARER_TOKEN"
"#,
        )
        .expect("config should be written");
        let error = load_codex_mcp_servers(&CodexCompatOptions {
            config_path: Some(config_path.clone()),
            credentials_path: None,
            resolved_secrets: McpResolvedSecrets {
                secret_values: BTreeMap::from([(
                    "mcp.custom.secretHttp.BEARER_TOKEN".to_string(),
                    "bearer-secret".to_string(),
                )]),
                revoked_secret_refs: Default::default(),
                allow_env_fallback: false,
            },
        })
        .expect_err("Authorization header and bearer token should conflict");
        fs::remove_file(config_path).ok();
        assert!(
            error
                .to_string()
                .contains("cannot configure Authorization header and managed HTTP auth")
        );
    }

    #[test]
    fn load_codex_mcp_servers_rejects_duplicate_http_headers_case_insensitively() {
        let config_path = temp_path("http-header-duplicate.toml");
        fs::write(
            &config_path,
            r#"
[mcp_servers.secretHttp]
url = "https://example.com/mcp"
headers = { "X-Api-Key" = "literal" }
http_header_secret_refs = { "x-api-key" = "mcp.custom.secretHttp.X_API_KEY" }
"#,
        )
        .expect("config should be written");
        let error = load_codex_mcp_servers(&CodexCompatOptions {
            config_path: Some(config_path.clone()),
            credentials_path: None,
            resolved_secrets: McpResolvedSecrets {
                secret_values: BTreeMap::from([(
                    "mcp.custom.secretHttp.X_API_KEY".to_string(),
                    "header-secret".to_string(),
                )]),
                revoked_secret_refs: Default::default(),
                allow_env_fallback: false,
            },
        })
        .expect_err("duplicate header sources should fail");
        fs::remove_file(config_path).ok();
        assert!(error.to_string().contains("from both headers"));
    }

    #[test]
    fn load_codex_mcp_servers_rejects_http_secret_refs_on_stdio() {
        let config_path = temp_path("stdio-http-secret-ref.toml");
        fs::write(
            &config_path,
            r#"
[mcp_servers.secretStdio]
command = "secret-mcp"
bearer_token_secret_ref = "mcp.custom.secretStdio.BEARER_TOKEN"
"#,
        )
        .expect("config should be written");
        let error = load_codex_mcp_servers(&CodexCompatOptions {
            config_path: Some(config_path.clone()),
            credentials_path: None,
            resolved_secrets: Default::default(),
        })
        .expect_err("stdio server should reject HTTP secret refs");
        fs::remove_file(config_path).ok();
        assert!(
            error
                .to_string()
                .contains("cannot configure HTTP auth or header fields")
        );
    }

    #[test]
    fn load_codex_mcp_servers_rejects_missing_secret_ref() {
        let config_path = temp_path("missing-secret-ref.toml");
        fs::write(
            &config_path,
            r#"
[mcp_servers.secretHttp]
url = "https://example.com/mcp"
bearer_token_secret_ref = "mcp.custom.secretHttp.BEARER_TOKEN"
"#,
        )
        .expect("config should be written");
        let error = load_codex_mcp_servers(&CodexCompatOptions {
            config_path: Some(config_path.clone()),
            credentials_path: None,
            resolved_secrets: Default::default(),
        })
        .expect_err("missing secret ref should fail");
        fs::remove_file(config_path).ok();
        assert!(
            error
                .to_string()
                .contains("missing MCP secret-store slot `mcp.custom.secretHttp.BEARER_TOKEN`")
        );
    }

    #[test]
    fn load_codex_mcp_servers_imports_oauth_auth_ref_without_secret_material() {
        let config_path = temp_path("oauth-ref.toml");
        fs::write(
            &config_path,
            r#"
[mcp_servers.oauthHttp]
url = "https://example.com/mcp"
oauth_slot_ref = "mcp.oauth.oauthHttp"
oauth_resource = "https://example.com/mcp"
oauth_scopes = ["read", "write"]
"#,
        )
        .expect("config should be written");

        let result = load_codex_mcp_servers(&CodexCompatOptions {
            config_path: Some(config_path.clone()),
            credentials_path: None,
            resolved_secrets: Default::default(),
        })
        .expect("config should parse");
        fs::remove_file(config_path).ok();

        let server = result
            .iter()
            .find(|server| server.name == "oauthHttp")
            .expect("oauth HTTP server should exist");
        assert_eq!(
            server.credential_secret_refs,
            vec!["mcp.oauth.oauthHttp".to_string()]
        );
        match &server.transport {
            McpServerTransport::StreamableHttp { auth, .. } => assert_eq!(
                auth,
                &super::McpHttpAuth::OAuth {
                    slot_id: "mcp.oauth.oauthHttp".to_string(),
                    resource: "https://example.com/mcp".to_string(),
                    scopes: vec!["read".to_string(), "write".to_string()],
                }
            ),
            other => panic!("expected HTTP transport, got {other:?}"),
        }
    }

    #[test]
    fn load_codex_mcp_servers_rejects_oauth_refs_outside_namespace() {
        let config_path = temp_path("oauth-bad-ref.toml");
        fs::write(
            &config_path,
            r#"
[mcp_servers.oauthHttp]
url = "https://example.com/mcp"
oauth_slot_ref = "mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN"
"#,
        )
        .expect("config should be written");

        let error = load_codex_mcp_servers(&CodexCompatOptions {
            config_path: Some(config_path.clone()),
            credentials_path: None,
            resolved_secrets: Default::default(),
        })
        .expect_err("non-oauth slot refs should fail");
        fs::remove_file(config_path).ok();

        assert!(
            error
                .to_string()
                .contains("OAuth slot refs must use the `mcp.oauth.` namespace")
        );
    }

    #[test]
    fn resolve_cwd_uses_workspace_root_for_missing_and_relative_cwd() {
        let workspace = Path::new("/tmp/workspace");
        assert_eq!(resolve_cwd(workspace, None), workspace);
        assert_eq!(
            resolve_cwd(workspace, Some(Path::new("relative"))),
            workspace.join("relative")
        );
        assert_eq!(
            resolve_cwd(workspace, Some(Path::new("/tmp/absolute"))),
            PathBuf::from("/tmp/absolute")
        );
    }

    #[test]
    fn load_mcp_servers_expands_catalog_without_codex_config() {
        let result = load_mcp_servers(&McpLoadOptions {
            codex: CodexCompatOptions::default(),
            catalog_profiles: vec!["docs".to_string()],
            resolved_secrets: Default::default(),
        })
        .expect("catalog-only load should work");
        assert!(result.config_path.is_none());
        assert_eq!(result.selected_profiles, vec!["docs"]);
        assert!(
            result
                .servers
                .iter()
                .any(|server| matches!(server.source, McpServerSource::BuiltInCatalog { .. }))
        );
    }

    #[test]
    fn load_mcp_servers_rejects_catalog_profiles_with_no_supported_entries() {
        let error = load_mcp_servers(&McpLoadOptions {
            codex: CodexCompatOptions::default(),
            catalog_profiles: vec!["knowledge".to_string()],
            resolved_secrets: Default::default(),
        })
        .expect_err("profile without supported entries should fail clearly");
        assert!(
            error
                .to_string()
                .contains("selected MCP catalog profiles have no supported entries")
        );
    }
}
