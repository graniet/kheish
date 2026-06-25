//! Connector configuration and route codecs for daemon-managed ingress and egress.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::sync::Arc;
use std::sync::RwLock;

use anyhow::{Context, Result};
use kheish_auth::{AuthManager, AuthSlotId};
use kheish_types::{CapabilityScope, CredentialScope, ReplyHandle};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

mod external;
mod http;
mod slack;
mod telegram;

pub use external::{
    ExternalChildProcessConfig, ExternalConnectorConfig, ExternalConnectorMode, ExternalReplyRoute,
    ExternalThreadRef, ResolvedExternalConnector, decode_external_reply_route,
    encode_external_reply_route,
};
pub use http::{
    HttpInputConnectorConfig, HttpReplyRoute, ResolvedHttpInputConnector, decode_http_reply_route,
    default_http_ingress_events_per_second as http_default_ingress_events_per_second,
    default_http_signature_max_age_secs as http_default_signature_max_age_secs,
    encode_http_reply_route,
};
pub(crate) use http::{ensure_public_http_reply_ip, parse_http_reply_host_ip};
pub use slack::{
    ResolvedSlackConnector, SlackConnectorConfig, SlackReplyRoute, SlackTeamBotTokenConfig,
    decode_slack_reply_route, default_slack_ingress_events_per_second, encode_slack_reply_route,
};
pub use telegram::{
    ResolvedTelegramConnector, TelegramConnectorConfig, TelegramIngressMode, TelegramReplyRoute,
    decode_telegram_reply_route, default_telegram_ingress_events_per_second,
    encode_telegram_reply_route, telegram_bot_api_method_url, telegram_bot_file_url,
};

/// Stable connector kinds exposed by the daemon control plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorKind {
    External,
    Http,
    Slack,
    Telegram,
}

impl ConnectorKind {
    /// Returns the public string identifier used by HTTP routes and CLI commands.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::External => "external",
            Self::Http => "http",
            Self::Slack => "slack",
            Self::Telegram => "telegram",
        }
    }

    /// Parses one public connector kind identifier.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "external" => Ok(Self::External),
            "http" => Ok(Self::Http),
            "slack" => Ok(Self::Slack),
            "telegram" => Ok(Self::Telegram),
            _ => Err(anyhow::anyhow!("unknown connector kind {value}")),
        }
    }
}

/// Returns true when one opaque reply target references the named connector.
pub fn reply_target_references_connector(
    target: &ReplyHandle,
    kind: ConnectorKind,
    name: &str,
) -> bool {
    match (kind, target.plugin.as_str()) {
        (ConnectorKind::External, "external") => decode_external_reply_route(&target.address)
            .map(|route| route.connector == name)
            .unwrap_or(false),
        (ConnectorKind::Telegram, "telegram") => decode_telegram_reply_route(&target.address)
            .map(|route| route.connector == name)
            .unwrap_or(false),
        (ConnectorKind::Slack, "slack") => decode_slack_reply_route(&target.address)
            .map(|route| route.connector == name)
            .unwrap_or(false),
        _ => false,
    }
}

impl fmt::Display for ConnectorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Controls how one connector materializes daemon sessions for inbound traffic.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorSessionPolicy {
    /// Allows the connector to create the resolved session when it does not already exist.
    #[serde(default)]
    pub create_if_missing: bool,
    /// Optionally binds newly created sessions to one persona.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona_id: Option<String>,
    /// Optionally applies one capability baseline to newly created sessions.
    #[serde(default, skip_serializing_if = "CapabilityScope::is_empty")]
    pub capability_scope: CapabilityScope,
    /// Optionally applies one credential baseline to newly created sessions.
    #[serde(default, skip_serializing_if = "CredentialScope::is_empty")]
    pub credential_scope: CredentialScope,
}

impl ConnectorSessionPolicy {
    /// Returns true when the policy does not request any special session bootstrap behavior.
    pub fn is_empty(&self) -> bool {
        !self.create_if_missing
            && self.persona_id.is_none()
            && self.capability_scope.is_empty()
            && self.credential_scope.is_empty()
    }

    /// Returns a normalized copy suitable for persistence and comparisons.
    pub fn normalized(&self) -> Self {
        Self {
            create_if_missing: self.create_if_missing,
            persona_id: self.persona_id.clone(),
            capability_scope: self.capability_scope.normalized(),
            credential_scope: self.credential_scope.normalized(),
        }
    }
}

/// Full connector settings loaded from one TOML file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConnectorSettings {
    /// External sidecar connectors keyed by `name`.
    #[serde(default)]
    pub external_connectors: Vec<ExternalConnectorConfig>,
    /// Slack bot connectors keyed by `name`.
    #[serde(default)]
    pub slack_connectors: Vec<SlackConnectorConfig>,
    /// Telegram bot connectors keyed by `name`.
    #[serde(default)]
    pub telegram_connectors: Vec<TelegramConnectorConfig>,
    /// Generic HTTP webhook ingress connectors keyed by `name`.
    #[serde(default)]
    pub http_connectors: Vec<HttpInputConnectorConfig>,
}

/// The runtime connector registry held by the daemon.
#[derive(Clone, Debug)]
pub struct ConnectorRegistry {
    inner: Arc<RwLock<ResolvedConnectorState>>,
    revision_tx: watch::Sender<u64>,
}

#[derive(Clone, Debug, Default)]
struct ResolvedConnectorState {
    external: BTreeMap<String, ResolvedExternalConnector>,
    slack: BTreeMap<String, ResolvedSlackConnector>,
    telegram: BTreeMap<String, ResolvedTelegramConnector>,
    http: BTreeMap<String, ResolvedHttpInputConnector>,
}

pub(super) fn default_true() -> bool {
    true
}

pub(super) fn resolve_secret(
    auth_manager: &AuthManager,
    inline: Option<String>,
    env_name: Option<String>,
    secret_ref: Option<String>,
    label: &str,
) -> Result<Option<String>> {
    let configured_sources = usize::from(inline.is_some())
        + usize::from(env_name.is_some())
        + usize::from(secret_ref.is_some());
    anyhow::ensure!(
        configured_sources <= 1,
        "{label} may configure only one secret source among inline, env, and secret_ref"
    );
    match (inline, env_name, secret_ref) {
        (_, _, Some(secret_ref)) => auth_manager
            .secret_value(&AuthSlotId::new(secret_ref.clone()))
            .map(|value| {
                value.ok_or_else(|| {
                    anyhow::anyhow!("missing secret-store slot {secret_ref} for {label}")
                })
            })?
            .map(Some),
        (_, Some(name), None) => env::var(&name)
            .with_context(|| format!("missing env var {name} for {label}"))
            .map(Some),
        (Some(value), None, None) => Ok(Some(value)),
        (None, None, None) => Ok(None),
    }
}

impl ConnectorRegistry {
    pub fn resolve(settings: ConnectorSettings, auth_manager: &AuthManager) -> Result<Self> {
        Ok(Self::from_state(Self::resolve_state(
            settings,
            auth_manager,
        )?))
    }

    /// Rebuilds the resolved registry contents from one complete connector settings snapshot.
    pub fn rebuild(&self, settings: ConnectorSettings, auth_manager: &AuthManager) -> Result<()> {
        self.replace_resolved(Self::resolve_state(settings, auth_manager)?);
        Ok(())
    }

    /// Returns the named Slack connector.
    pub fn external(&self, name: &str) -> Option<ResolvedExternalConnector> {
        self.inner
            .read()
            .expect("connector registry rwlock poisoned")
            .external
            .get(name)
            .cloned()
    }

    /// Returns all resolved external connectors.
    pub fn external_connectors(&self) -> Vec<ResolvedExternalConnector> {
        self.inner
            .read()
            .expect("connector registry rwlock poisoned")
            .external
            .values()
            .cloned()
            .collect()
    }

    /// Returns the named Slack connector.
    pub fn slack(&self, name: &str) -> Option<ResolvedSlackConnector> {
        self.inner
            .read()
            .expect("connector registry rwlock poisoned")
            .slack
            .get(name)
            .cloned()
    }

    /// Returns the named Telegram connector.
    pub fn telegram(&self, name: &str) -> Option<ResolvedTelegramConnector> {
        self.inner
            .read()
            .expect("connector registry rwlock poisoned")
            .telegram
            .get(name)
            .cloned()
    }

    /// Returns all resolved Telegram connectors.
    pub fn telegram_connectors(&self) -> Vec<ResolvedTelegramConnector> {
        self.inner
            .read()
            .expect("connector registry rwlock poisoned")
            .telegram
            .values()
            .cloned()
            .collect()
    }

    /// Returns the named HTTP webhook connector.
    pub fn http(&self, name: &str) -> Option<ResolvedHttpInputConnector> {
        self.inner
            .read()
            .expect("connector registry rwlock poisoned")
            .http
            .get(name)
            .cloned()
    }

    /// Replaces the entire resolved runtime connector state and notifies subscribers.
    fn replace_resolved(&self, state: ResolvedConnectorState) {
        *self
            .inner
            .write()
            .expect("connector registry rwlock poisoned") = state;
        let next_revision = self.revision_tx.borrow().saturating_add(1);
        let _ = self.revision_tx.send(next_revision);
    }

    /// Subscribes to registry revision changes for ingress reconciliation.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision_tx.subscribe()
    }

    fn from_state(state: ResolvedConnectorState) -> Self {
        let (revision_tx, _) = watch::channel(0_u64);
        Self {
            inner: Arc::new(RwLock::new(state)),
            revision_tx,
        }
    }

    fn resolve_state(
        settings: ConnectorSettings,
        auth_manager: &AuthManager,
    ) -> Result<ResolvedConnectorState> {
        let external = settings
            .external_connectors
            .into_iter()
            .map(|config| external::resolve_connector(auth_manager, config))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let slack = settings
            .slack_connectors
            .into_iter()
            .map(|config| slack::resolve_connector(auth_manager, config))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let telegram = settings
            .telegram_connectors
            .into_iter()
            .map(|config| telegram::resolve_connector(auth_manager, config))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let http = settings
            .http_connectors
            .into_iter()
            .map(|config| http::resolve_connector(auth_manager, config))
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok(ResolvedConnectorState {
            external,
            slack,
            telegram,
            http,
        })
    }
}

impl Default for ConnectorRegistry {
    fn default() -> Self {
        Self::from_state(ResolvedConnectorState::default())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use kheish_auth::AuthManager;

    use super::resolve_secret;

    #[test]
    fn resolve_secret_rejects_multiple_sources() {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let error = resolve_secret(
            auth_manager.as_ref(),
            Some("inline-secret".to_string()),
            Some("SECRET_ENV".to_string()),
            None,
            "telegram connector bot_token",
        )
        .expect_err("multiple secret sources should be rejected");
        assert!(
            error
                .to_string()
                .contains("may configure only one secret source"),
            "unexpected error: {error:#}"
        );
    }
}
