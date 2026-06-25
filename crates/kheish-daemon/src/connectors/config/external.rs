use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use kheish_auth::AuthSlotId;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use kheish_types::{ReplyHandle, normalize_reply_targets};

use super::http::{ensure_public_http_reply_ip, parse_http_reply_host_ip};
use super::{ConnectorSessionPolicy, default_true, resolve_secret};

const RESERVED_CHILD_PROCESS_ENV_KEYS: &[&str] = &[
    "KHEISH_EXTERNAL_CONNECTOR_NAME",
    "KHEISH_EXTERNAL_CONNECTOR_BASE_URL",
    "KHEISH_EXTERNAL_CONNECTOR_DAEMON_BASE_URL",
    "KHEISH_EXTERNAL_CONNECTOR_SHARED_TOKEN",
    "KHEISH_EXTERNAL_CONNECTOR_CREDENTIAL_TOKEN",
    "KHEISH_EXTERNAL_CONNECTOR_CREDENTIAL_KEYS_JSON",
];

const DEFAULT_EXTERNAL_INGRESS_EVENTS_PER_SECOND: u32 = 100;

/// The runtime integration mode used by one external connector sidecar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalConnectorMode {
    /// A remote sidecar managed outside the daemon.
    #[default]
    RemoteHttp,
    /// A local process launched and supervised by the daemon.
    ChildProcess,
}

/// Child-process launch settings for one external connector sidecar.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalChildProcessConfig {
    /// Executable path or command name.
    pub command: String,
    /// Arguments passed to the executable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Explicit non-secret environment overrides passed to the child.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Secret-backed credential env names fetched from the daemon at sidecar startup.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub credential_slots: BTreeMap<String, String>,
    /// Optional working directory used when spawning the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

/// One external connector sidecar definition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExternalConnectorConfig {
    /// Stable connector identifier used in routes and reply addresses.
    pub name: String,
    /// User-visible platform or transport label used in thread keys and diagnostics.
    pub platform: String,
    /// Whether the sidecar is remotely managed or daemon-managed.
    #[serde(default)]
    pub mode: ExternalConnectorMode,
    /// The sidecar base URL used for manifest, health, and delivery requests.
    pub base_url: String,
    /// Allows remote_http sidecars to target private/loopback networks. Child-process sidecars are loopback-only.
    #[serde(default)]
    pub allow_private_network: bool,
    /// Optional shared bearer token used by both ingress and daemon-to-sidecar requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_token: Option<String>,
    /// Environment variable containing the shared bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_token_env: Option<String>,
    /// Secret-store slot containing the shared bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_token_secret_ref: Option<String>,
    /// Allows ingress and delivery without the shared bearer token.
    #[serde(default)]
    pub allow_unauthenticated_ingress: bool,
    /// Optional fixed session identifier. When omitted, sessions are derived from thread prefixes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_session_id: Option<String>,
    /// Whether replies should default back to the sidecar-provided route.
    #[serde(default = "default_true")]
    pub include_self_output: bool,
    /// Additional output targets appended after the self target when present.
    #[serde(default)]
    pub additional_reply_targets: Vec<ReplyHandle>,
    /// Additional opaque binding keys associated with each inbound conversation.
    #[serde(default)]
    pub additional_binding_keys: Vec<String>,
    /// Controls how this connector materializes daemon sessions for inbound traffic.
    #[serde(default, skip_serializing_if = "ConnectorSessionPolicy::is_empty")]
    pub session_policy: ConnectorSessionPolicy,
    /// Maximum accepted ingress events per second before daemon-side throttling.
    #[serde(default = "default_external_ingress_events_per_second")]
    pub ingress_events_per_second: u32,
    /// Optional child-process launch settings used only when `mode = child_process`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_process: Option<ExternalChildProcessConfig>,
}

/// One resolved external connector with secrets loaded.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedExternalConnector {
    pub name: String,
    pub platform: String,
    pub mode: ExternalConnectorMode,
    pub base_url: String,
    pub allow_private_network: bool,
    pub shared_token: Option<String>,
    pub allow_unauthenticated_ingress: bool,
    pub fixed_session_id: Option<String>,
    pub include_self_output: bool,
    pub additional_reply_targets: Vec<ReplyHandle>,
    pub additional_binding_keys: Vec<String>,
    pub session_policy: ConnectorSessionPolicy,
    pub ingress_events_per_second: u32,
    pub child_process: Option<ExternalChildProcessConfig>,
}

impl fmt::Debug for ResolvedExternalConnector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedExternalConnector")
            .field("name", &self.name)
            .field("platform", &self.platform)
            .field("mode", &self.mode)
            .field("base_url", &self.base_url)
            .field("allow_private_network", &self.allow_private_network)
            .field(
                "shared_token",
                &self.shared_token.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "allow_unauthenticated_ingress",
                &self.allow_unauthenticated_ingress,
            )
            .field("fixed_session_id", &self.fixed_session_id)
            .field("include_self_output", &self.include_self_output)
            .field("additional_reply_targets", &self.additional_reply_targets)
            .field("additional_binding_keys", &self.additional_binding_keys)
            .field("session_policy", &self.session_policy)
            .field("ingress_events_per_second", &self.ingress_events_per_second)
            .field("child_process", &self.child_process)
            .finish()
    }
}

/// One opaque reply route targeting one external sidecar conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalReplyRoute {
    pub connector: String,
    pub route: String,
}

/// One structured external conversation reference used to resolve daemon sessions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalThreadRef {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
}

impl ExternalThreadRef {
    pub fn is_empty(&self) -> bool {
        self.path.is_empty()
    }

    fn normalized_routing_key(routing_key: Option<&str>) -> Option<&str> {
        routing_key.map(str::trim).filter(|value| !value.is_empty())
    }

    /// Returns the durable binding keys used to map one external conversation back to a session.
    pub fn binding_keys(&self, connector: &ResolvedExternalConnector) -> Vec<String> {
        self.binding_keys_with_routing_key(connector, None)
    }

    /// Returns the durable binding keys used to map one external conversation back to a session.
    pub fn binding_keys_with_routing_key(
        &self,
        connector: &ResolvedExternalConnector,
        routing_key: Option<&str>,
    ) -> Vec<String> {
        let mut keys = Vec::new();
        if !self.path.is_empty() {
            let mut prefix = vec![
                format!("external:{}", connector.name),
                encode_binding_segment(&connector.platform),
            ];
            for segment in &self.path {
                prefix.push(encode_binding_segment(segment));
                keys.push(prefix.join(":"));
            }
        } else if let Some(routing_key) = Self::normalized_routing_key(routing_key) {
            keys.push(
                [
                    format!("external:{}", connector.name),
                    encode_binding_segment(&connector.platform),
                    "route".to_string(),
                    encode_binding_segment(routing_key),
                ]
                .join(":"),
            );
        }
        keys.extend(connector.additional_binding_keys.clone());
        keys.sort();
        keys.dedup();
        keys
    }

    /// Returns the natural fallback session identifier for one inbound sidecar event.
    pub fn natural_session_id(&self, connector: &ResolvedExternalConnector) -> Option<String> {
        self.natural_session_id_with_routing_key(connector, None)
    }

    /// Returns the natural fallback session identifier for one inbound sidecar event.
    pub fn natural_session_id_with_routing_key(
        &self,
        connector: &ResolvedExternalConnector,
        routing_key: Option<&str>,
    ) -> Option<String> {
        if let Some(session_id) = connector.fixed_session_id.clone() {
            return Some(session_id);
        }
        let raw = if self.path.is_empty() {
            let routing_key = Self::normalized_routing_key(routing_key)?;
            serde_json::to_vec(&("routing_key", connector.platform.as_str(), routing_key))
                .expect("external routing key should serialize")
        } else {
            serde_json::to_vec(&(connector.platform.as_str(), &self.path))
                .expect("external thread ref should serialize")
        };
        let digest = Sha256::digest(&raw);
        Some(format!(
            "external:{}:{}",
            connector.name,
            hex::encode(&digest[..8])
        ))
    }
}

pub(super) fn resolve_connector(
    auth_manager: &kheish_auth::AuthManager,
    config: ExternalConnectorConfig,
) -> Result<(String, ResolvedExternalConnector)> {
    let name = config.name.clone();
    let platform = normalize_non_empty_string(config.platform, "platform")?;
    let base_url = normalize_base_url(
        &config.base_url,
        config.mode,
        config.allow_private_network,
        &name,
    )?;
    let shared_token = resolve_secret(
        auth_manager,
        config.shared_token,
        config.shared_token_env,
        config.shared_token_secret_ref,
        &format!("external connector {name} shared_token"),
    )?;
    if shared_token
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        bail!("external connector {name} shared_token cannot be empty");
    }
    if shared_token.is_none() && !config.allow_unauthenticated_ingress {
        bail!(
            "external connector {name} requires shared_token, shared_token_env, shared_token_secret_ref, or allow_unauthenticated_ingress=true"
        );
    }
    match config.mode {
        ExternalConnectorMode::RemoteHttp => {
            if config.child_process.is_some() {
                bail!(
                    "external connector {name} may not configure child_process when mode is remote_http"
                );
            }
        }
        ExternalConnectorMode::ChildProcess => {
            let child_process = config.child_process.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "external connector {name} requires child_process when mode is child_process"
                )
            })?;
            if child_process.command.trim().is_empty() {
                bail!("external connector {name} child_process.command is required");
            }
            if let Some(reserved) = child_process
                .env
                .keys()
                .find(|key| RESERVED_CHILD_PROCESS_ENV_KEYS.contains(&key.as_str()))
            {
                bail!(
                    "external connector {name} child_process.env may not override reserved variable {reserved}"
                );
            }
            if let Some(reserved) = child_process
                .credential_slots
                .keys()
                .find(|key| RESERVED_CHILD_PROCESS_ENV_KEYS.contains(&key.as_str()))
            {
                bail!(
                    "external connector {name} child_process.credential_slots may not override reserved variable {reserved}"
                );
            }
            if child_process
                .credential_slots
                .iter()
                .any(|(key, slot)| key.trim().is_empty() || slot.trim().is_empty())
            {
                bail!(
                    "external connector {name} child_process.credential_slots entries must use non-empty env names and secret refs"
                );
            }
            for (env_key, secret_ref) in &child_process.credential_slots {
                let value = auth_manager
                    .secret_value(&AuthSlotId::new(secret_ref.clone()))
                    .with_context(|| {
                        format!(
                            "external connector {name} child_process.credential_slots {env_key} references invalid secret-store slot {secret_ref}"
                        )
                    })?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "external connector {name} child_process.credential_slots {env_key} references missing secret-store slot {secret_ref}"
                        )
                    })?;
                if value.trim().is_empty() {
                    bail!(
                        "external connector {name} child_process.credential_slots {env_key} references empty secret-store slot {secret_ref}"
                    );
                }
            }
        }
    }
    let resolved = ResolvedExternalConnector {
        name: config.name.clone(),
        platform,
        mode: config.mode,
        base_url,
        allow_private_network: config.allow_private_network
            || config.mode == ExternalConnectorMode::ChildProcess,
        shared_token,
        allow_unauthenticated_ingress: config.allow_unauthenticated_ingress,
        fixed_session_id: config.fixed_session_id,
        include_self_output: config.include_self_output,
        additional_reply_targets: config.additional_reply_targets,
        additional_binding_keys: config.additional_binding_keys,
        session_policy: config.session_policy.normalized(),
        ingress_events_per_second: config.ingress_events_per_second.max(1),
        child_process: config.child_process,
    };
    Ok((name, resolved))
}

impl ResolvedExternalConnector {
    /// Builds the default reply targets for one inbound sidecar event.
    pub fn reply_targets(&self, reply_route: Option<String>) -> Vec<ReplyHandle> {
        let mut targets = Vec::new();
        if self.include_self_output
            && let Some(route) = reply_route.filter(|value| !value.trim().is_empty())
        {
            targets.push(ReplyHandle {
                plugin: "external".to_string(),
                address: encode_external_reply_route(&ExternalReplyRoute {
                    connector: self.name.clone(),
                    route,
                }),
            });
        }
        targets.extend(self.additional_reply_targets.clone());
        normalize_reply_targets(None, targets)
    }
}

/// Encodes one external reply route into an opaque reply address.
pub fn encode_external_reply_route(route: &ExternalReplyRoute) -> String {
    serde_json::to_string(route).expect("external reply route should serialize")
}

/// Decodes one external reply route from an opaque reply address.
pub fn decode_external_reply_route(address: &str) -> Result<ExternalReplyRoute> {
    serde_json::from_str(address).context("invalid external reply route")
}

fn normalize_non_empty_string(value: String, field: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{field} is required");
    }
    Ok(trimmed.to_string())
}

fn normalize_base_url(
    value: &str,
    mode: ExternalConnectorMode,
    allow_private_network: bool,
    name: &str,
) -> Result<String> {
    let url = Url::parse(value)
        .with_context(|| format!("invalid external connector {name} base_url {value}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => {
            bail!("external connector {name} base_url must use http:// or https://, got {other}")
        }
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("external connector {name} base_url is missing a host"))?;
    if !url.username().is_empty() || url.password().is_some() {
        bail!("external connector {name} base_url must not include userinfo");
    }
    match mode {
        ExternalConnectorMode::ChildProcess => {
            if !is_loopback_host(host) {
                bail!(
                    "external connector {name} child_process base_url must target a loopback host"
                );
            }
            if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
                bail!(
                    "external connector {name} child_process base_url may not include a path, query, or fragment"
                );
            }
        }
        ExternalConnectorMode::RemoteHttp if !allow_private_network => {
            if is_loopback_host(host) {
                bail!(
                    "external connector {name} remote_http base_url targets a private network host; set allow_private_network=true only for trusted local sidecars"
                );
            }
            if let Some(ip) = parse_http_reply_host_ip(host) {
                ensure_public_http_reply_ip(ip).with_context(|| {
                    format!(
                        "external connector {name} remote_http base_url targets a private network address"
                    )
                })?;
            }
        }
        ExternalConnectorMode::RemoteHttp => {}
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn is_loopback_host(host: &str) -> bool {
    let normalized = host
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    normalized == "127.0.0.1"
        || normalized == "::1"
        || normalized == "localhost"
        || normalized.ends_with(".localhost")
}

fn encode_binding_segment(segment: &str) -> String {
    URL_SAFE_NO_PAD.encode(segment.as_bytes())
}

fn default_external_ingress_events_per_second() -> u32 {
    DEFAULT_EXTERNAL_INGRESS_EVENTS_PER_SECOND
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use anyhow::Result;
    use tempfile::tempdir;

    use kheish_auth::{AUTH_STORE_MASTER_KEY_ENV, AuthManager, AuthSlotId};

    use super::{
        ConnectorSessionPolicy, ExternalChildProcessConfig, ExternalConnectorConfig,
        ExternalConnectorMode, ExternalThreadRef, resolve_connector,
    };

    fn child_process_connector_with_credential_slots(
        credential_slots: BTreeMap<String, String>,
    ) -> ExternalConnectorConfig {
        ExternalConnectorConfig {
            name: "discord".to_string(),
            platform: "discord".to_string(),
            mode: ExternalConnectorMode::ChildProcess,
            base_url: "http://127.0.0.1:4444".to_string(),
            allow_private_network: false,
            shared_token: Some("secret".to_string()),
            shared_token_env: None,
            shared_token_secret_ref: None,
            allow_unauthenticated_ingress: false,
            fixed_session_id: None,
            include_self_output: true,
            additional_reply_targets: Vec::new(),
            additional_binding_keys: Vec::new(),
            session_policy: ConnectorSessionPolicy::default(),
            ingress_events_per_second: 100,
            child_process: Some(ExternalChildProcessConfig {
                command: "connector".to_string(),
                args: Vec::new(),
                env: BTreeMap::new(),
                credential_slots,
                working_dir: None,
            }),
        }
    }

    #[test]
    fn resolved_external_connector_debug_redacts_shared_token() {
        let connector = super::ResolvedExternalConnector {
            name: "discord-main".to_string(),
            platform: "discord".to_string(),
            mode: ExternalConnectorMode::RemoteHttp,
            base_url: "http://127.0.0.1:9999".to_string(),
            allow_private_network: true,
            shared_token: Some("super-secret-token".to_string()),
            allow_unauthenticated_ingress: false,
            fixed_session_id: None,
            include_self_output: true,
            additional_reply_targets: Vec::new(),
            additional_binding_keys: Vec::new(),
            session_policy: ConnectorSessionPolicy::default(),
            ingress_events_per_second: 100,
            child_process: None,
        };

        let rendered = format!("{connector:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("super-secret-token"));
    }

    #[test]
    fn child_process_connectors_require_loopback_base_urls() {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let error = resolve_connector(
            auth_manager.as_ref(),
            ExternalConnectorConfig {
                name: "discord".to_string(),
                platform: "discord".to_string(),
                mode: ExternalConnectorMode::ChildProcess,
                base_url: "https://example.com/sidecar".to_string(),
                allow_private_network: false,
                shared_token: Some("secret".to_string()),
                shared_token_env: None,
                shared_token_secret_ref: None,
                allow_unauthenticated_ingress: false,
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
                ingress_events_per_second: 100,
                child_process: Some(ExternalChildProcessConfig {
                    command: "connector".to_string(),
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    credential_slots: BTreeMap::new(),
                    working_dir: None,
                }),
            },
        )
        .expect_err("child-process connectors should reject non-loopback base URLs");
        assert!(
            error.to_string().contains("loopback"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn remote_http_connectors_reject_private_base_urls_without_explicit_opt_in() {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        for base_url in [
            "http://localhost:4444",
            "http://localhost.:4444",
            "http://app.localhost:4444",
            "http://127.0.0.1:4444",
            "http://10.1.2.3:4444",
            "http://169.254.169.254",
            "http://[::1]:4444",
            "http://[::ffff:127.0.0.1]:4444",
        ] {
            let error = resolve_connector(
                auth_manager.as_ref(),
                ExternalConnectorConfig {
                    name: "discord".to_string(),
                    platform: "discord".to_string(),
                    mode: ExternalConnectorMode::RemoteHttp,
                    base_url: base_url.to_string(),
                    allow_private_network: false,
                    shared_token: Some("secret".to_string()),
                    shared_token_env: None,
                    shared_token_secret_ref: None,
                    allow_unauthenticated_ingress: false,
                    fixed_session_id: None,
                    include_self_output: true,
                    additional_reply_targets: Vec::new(),
                    additional_binding_keys: Vec::new(),
                    session_policy: ConnectorSessionPolicy::default(),
                    ingress_events_per_second: 100,
                    child_process: None,
                },
            )
            .expect_err("private remote_http base URL should fail without opt-in");
            assert!(
                error.to_string().contains("private network"),
                "unexpected error for {base_url}: {error:#}"
            );
        }
    }

    #[test]
    fn remote_http_connectors_allow_private_base_urls_with_explicit_opt_in() -> Result<()> {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let (_, connector) = resolve_connector(
            auth_manager.as_ref(),
            ExternalConnectorConfig {
                name: "discord".to_string(),
                platform: "discord".to_string(),
                mode: ExternalConnectorMode::RemoteHttp,
                base_url: "http://127.0.0.1:4444".to_string(),
                allow_private_network: true,
                shared_token: Some("secret".to_string()),
                shared_token_env: None,
                shared_token_secret_ref: None,
                allow_unauthenticated_ingress: false,
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
                ingress_events_per_second: 100,
                child_process: None,
            },
        )?;
        assert!(connector.allow_private_network);
        Ok(())
    }

    #[test]
    fn external_connectors_reject_empty_tokens_and_base_url_userinfo() {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let empty_token = resolve_connector(
            auth_manager.as_ref(),
            ExternalConnectorConfig {
                name: "discord".to_string(),
                platform: "discord".to_string(),
                mode: ExternalConnectorMode::RemoteHttp,
                base_url: "https://example.com".to_string(),
                allow_private_network: false,
                shared_token: Some(" ".to_string()),
                shared_token_env: None,
                shared_token_secret_ref: None,
                allow_unauthenticated_ingress: false,
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
                ingress_events_per_second: 100,
                child_process: None,
            },
        )
        .expect_err("empty shared token should fail");
        assert!(
            empty_token
                .to_string()
                .contains("shared_token cannot be empty"),
            "unexpected error: {empty_token:#}"
        );

        let userinfo = resolve_connector(
            auth_manager.as_ref(),
            ExternalConnectorConfig {
                name: "discord".to_string(),
                platform: "discord".to_string(),
                mode: ExternalConnectorMode::RemoteHttp,
                base_url: "https://user:secret@example.com".to_string(),
                allow_private_network: false,
                shared_token: Some("secret".to_string()),
                shared_token_env: None,
                shared_token_secret_ref: None,
                allow_unauthenticated_ingress: false,
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
                ingress_events_per_second: 100,
                child_process: None,
            },
        )
        .expect_err("base URL userinfo should fail");
        assert!(
            userinfo.to_string().contains("must not include userinfo"),
            "unexpected error: {userinfo:#}"
        );
    }

    #[test]
    fn child_process_connectors_reject_reserved_env_keys() {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let error = resolve_connector(
            auth_manager.as_ref(),
            ExternalConnectorConfig {
                name: "discord".to_string(),
                platform: "discord".to_string(),
                mode: ExternalConnectorMode::ChildProcess,
                base_url: "http://127.0.0.1:4444".to_string(),
                allow_private_network: false,
                shared_token: Some("secret".to_string()),
                shared_token_env: None,
                shared_token_secret_ref: None,
                allow_unauthenticated_ingress: false,
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
                ingress_events_per_second: 100,
                child_process: Some(ExternalChildProcessConfig {
                    command: "connector".to_string(),
                    args: Vec::new(),
                    env: BTreeMap::from([(
                        "KHEISH_EXTERNAL_CONNECTOR_SHARED_TOKEN".to_string(),
                        "override".to_string(),
                    )]),
                    credential_slots: BTreeMap::new(),
                    working_dir: None,
                }),
            },
        )
        .expect_err("reserved child-process env keys should be rejected");
        assert!(
            error.to_string().contains("reserved variable"),
            "unexpected error: {error:#}"
        );
    }

    #[tokio::test]
    async fn child_process_credential_slots_reject_missing_secret_refs() {
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let error = resolve_connector(
            auth_manager.as_ref(),
            child_process_connector_with_credential_slots(BTreeMap::from([(
                "DISCORD_BOT_TOKEN".to_string(),
                "connectors.external.discord.bot_token".to_string(),
            )])),
        )
        .expect_err("missing child-process credential secret refs should be rejected");
        assert!(
            error.to_string().contains("missing secret-store slot"),
            "unexpected error: {error:#}"
        );
    }

    #[tokio::test]
    async fn child_process_credential_slots_accept_existing_generic_secret_refs() -> Result<()> {
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        auth_manager
            .store_generic_secret(
                AuthSlotId::new("connectors.external.discord.bot_token"),
                "bot-token",
            )
            .await?;

        resolve_connector(
            auth_manager.as_ref(),
            child_process_connector_with_credential_slots(BTreeMap::from([(
                "DISCORD_BOT_TOKEN".to_string(),
                "connectors.external.discord.bot_token".to_string(),
            )])),
        )?;
        Ok(())
    }

    #[test]
    fn child_process_connectors_reject_base_urls_with_path_prefixes() {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let error = resolve_connector(
            auth_manager.as_ref(),
            ExternalConnectorConfig {
                name: "discord".to_string(),
                platform: "discord".to_string(),
                mode: ExternalConnectorMode::ChildProcess,
                base_url: "http://127.0.0.1:4444/sidecar".to_string(),
                allow_private_network: false,
                shared_token: Some("secret".to_string()),
                shared_token_env: None,
                shared_token_secret_ref: None,
                allow_unauthenticated_ingress: false,
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
                ingress_events_per_second: 100,
                child_process: Some(ExternalChildProcessConfig {
                    command: "connector".to_string(),
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    credential_slots: BTreeMap::new(),
                    working_dir: None,
                }),
            },
        )
        .expect_err("child-process connectors should reject base URLs with path prefixes");
        assert!(
            error.to_string().contains("may not include a path"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn external_thread_ref_builds_prefix_binding_keys() {
        let connector = super::ResolvedExternalConnector {
            name: "discord-main".to_string(),
            platform: "discord".to_string(),
            mode: ExternalConnectorMode::RemoteHttp,
            base_url: "http://127.0.0.1:9999".to_string(),
            allow_private_network: true,
            shared_token: Some("secret".to_string()),
            allow_unauthenticated_ingress: false,
            fixed_session_id: None,
            include_self_output: true,
            additional_reply_targets: Vec::new(),
            additional_binding_keys: vec!["ops:global".to_string()],
            session_policy: ConnectorSessionPolicy::default(),
            ingress_events_per_second: 100,
            child_process: None,
        };
        let keys = ExternalThreadRef {
            path: vec!["guild-1".to_string(), "channel-2".to_string()],
        }
        .binding_keys(&connector);
        assert_eq!(keys.len(), 3);
        assert!(keys[0].starts_with("external:discord-main:"));
        assert!(keys[1].starts_with("external:discord-main:"));
        assert_eq!(keys[2], "ops:global");
    }

    #[test]
    fn external_thread_ref_uses_routing_key_when_thread_is_empty() {
        let connector = super::ResolvedExternalConnector {
            name: "grafana-main".to_string(),
            platform: "grafana".to_string(),
            mode: ExternalConnectorMode::RemoteHttp,
            base_url: "http://127.0.0.1:9999".to_string(),
            allow_private_network: true,
            shared_token: Some("secret".to_string()),
            allow_unauthenticated_ingress: false,
            fixed_session_id: None,
            include_self_output: true,
            additional_reply_targets: Vec::new(),
            additional_binding_keys: Vec::new(),
            session_policy: ConnectorSessionPolicy::default(),
            ingress_events_per_second: 100,
            child_process: None,
        };
        let thread = ExternalThreadRef::default();
        let keys = thread.binding_keys_with_routing_key(&connector, Some("alerts/team-a"));
        assert_eq!(keys.len(), 1);
        assert_eq!(
            keys[0],
            "external:grafana-main:Z3JhZmFuYQ:route:YWxlcnRzL3RlYW0tYQ"
        );
        assert!(
            thread
                .natural_session_id_with_routing_key(&connector, Some("alerts/team-a"))
                .is_some()
        );
    }
}
