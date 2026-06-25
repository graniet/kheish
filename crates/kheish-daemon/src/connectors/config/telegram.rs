use std::fmt;

use anyhow::{Context, Result, bail};
use reqwest::Url;
use serde::{Deserialize, Serialize};

use kheish_types::{ReplyHandle, normalize_reply_targets};

use super::{ConnectorSessionPolicy, default_true, resolve_secret};

const DEFAULT_TELEGRAM_API_BASE_URL: &str = "https://api.telegram.org";
const DEFAULT_POLLING_TIMEOUT_SECONDS: u64 = 30;
const DEFAULT_TELEGRAM_INGRESS_EVENTS_PER_SECOND: u32 = 100;

/// The ingress transport mode used by one Telegram connector.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelegramIngressMode {
    /// Accept inbound updates through the daemon webhook route.
    #[default]
    Webhook,
    /// Poll Telegram `getUpdates` from the daemon itself.
    Polling,
}

/// A Telegram bot connector used for both webhook ingress and Bot API egress.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TelegramConnectorConfig {
    /// Stable connector identifier used in routes and reply addresses.
    pub name: String,
    /// Telegram bot token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,
    /// Environment variable containing the Telegram bot token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token_env: Option<String>,
    /// Secret-store slot containing the Telegram bot token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token_secret_ref: Option<String>,
    /// Optional secret token header expected on webhook requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_token: Option<String>,
    /// Environment variable containing the secret token header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_token_env: Option<String>,
    /// Secret-store slot containing the webhook secret token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_token_secret_ref: Option<String>,
    /// Allows webhook ingress requests without the Telegram secret-token header.
    #[serde(default)]
    pub allow_unauthenticated_ingress: bool,
    /// Override Telegram API base URL for tests or proxies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    /// How inbound updates reach the daemon.
    #[serde(default)]
    pub ingress_mode: TelegramIngressMode,
    /// Long-poll timeout passed to `getUpdates` when polling ingress is enabled.
    #[serde(default = "default_polling_timeout_seconds")]
    pub polling_timeout_seconds: u64,
    /// Maximum new webhook/polling updates accepted per second for this connector.
    #[serde(default = "default_telegram_ingress_events_per_second")]
    pub ingress_events_per_second: u32,
    /// Optional chat allowlist. Empty means all chats are allowed for backward compatibility.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_chat_ids: Vec<i64>,
    /// Optional fixed session identifier. When omitted, sessions are derived from chat/topic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_session_id: Option<String>,
    /// Whether replies should default to the same Telegram chat or topic.
    #[serde(default = "default_true")]
    pub include_self_output: bool,
    /// Additional output targets appended to the default self target.
    #[serde(default)]
    pub additional_reply_targets: Vec<ReplyHandle>,
    /// Additional opaque binding keys associated with each inbound conversation.
    #[serde(default)]
    pub additional_binding_keys: Vec<String>,
    /// Controls how this connector materializes daemon sessions for inbound traffic.
    #[serde(default, skip_serializing_if = "ConnectorSessionPolicy::is_empty")]
    pub session_policy: ConnectorSessionPolicy,
}

/// One resolved Telegram connector with secrets loaded.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedTelegramConnector {
    pub name: String,
    pub bot_token: Option<String>,
    pub secret_token: Option<String>,
    pub allow_unauthenticated_ingress: bool,
    pub api_base_url: String,
    pub ingress_mode: TelegramIngressMode,
    pub polling_timeout_seconds: u64,
    pub ingress_events_per_second: u32,
    pub allowed_chat_ids: Vec<i64>,
    pub fixed_session_id: Option<String>,
    pub include_self_output: bool,
    pub additional_reply_targets: Vec<ReplyHandle>,
    pub additional_binding_keys: Vec<String>,
    pub session_policy: ConnectorSessionPolicy,
}

impl fmt::Debug for ResolvedTelegramConnector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedTelegramConnector")
            .field("name", &self.name)
            .field("bot_token", &self.bot_token.as_ref().map(|_| "<redacted>"))
            .field(
                "secret_token",
                &self.secret_token.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "allow_unauthenticated_ingress",
                &self.allow_unauthenticated_ingress,
            )
            .field("api_base_url", &self.api_base_url)
            .field("ingress_mode", &self.ingress_mode)
            .field("polling_timeout_seconds", &self.polling_timeout_seconds)
            .field("ingress_events_per_second", &self.ingress_events_per_second)
            .field("allowed_chat_ids", &self.allowed_chat_ids)
            .field("fixed_session_id", &self.fixed_session_id)
            .field("include_self_output", &self.include_self_output)
            .field("additional_reply_targets", &self.additional_reply_targets)
            .field("additional_binding_keys", &self.additional_binding_keys)
            .field("session_policy", &self.session_policy)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelegramReplyRoute {
    pub connector: String,
    pub chat_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_thread_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<i64>,
}

pub(super) fn resolve_connector(
    auth_manager: &kheish_auth::AuthManager,
    config: TelegramConnectorConfig,
) -> Result<(String, ResolvedTelegramConnector)> {
    let name = config.name.clone();
    let bot_token = resolve_secret(
        auth_manager,
        config.bot_token,
        config.bot_token_env,
        config.bot_token_secret_ref,
        &format!("telegram connector {name} bot_token"),
    )?;
    let secret_token = resolve_secret(
        auth_manager,
        config.secret_token,
        config.secret_token_env,
        config.secret_token_secret_ref,
        &format!("telegram connector {name} secret_token"),
    )?;
    if bot_token
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        anyhow::bail!("telegram connector {name} bot_token cannot be empty");
    }
    if secret_token
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        anyhow::bail!("telegram connector {name} secret_token cannot be empty");
    }
    if config.ingress_mode == TelegramIngressMode::Polling && bot_token.is_none() {
        anyhow::bail!(
            "telegram connector {name} requires bot_token, bot_token_env, or bot_token_secret_ref when ingress_mode is polling"
        );
    }
    if config.include_self_output && bot_token.is_none() {
        anyhow::bail!(
            "telegram connector {name} requires bot_token, bot_token_env, or bot_token_secret_ref when include_self_output is enabled"
        );
    }
    if config.ingress_mode == TelegramIngressMode::Webhook
        && secret_token.is_none()
        && !config.allow_unauthenticated_ingress
    {
        anyhow::bail!(
            "telegram connector {name} requires secret_token, secret_token_env, secret_token_secret_ref, or allow_unauthenticated_ingress=true when ingress_mode is webhook"
        );
    }
    let mut allowed_chat_ids = config.allowed_chat_ids;
    allowed_chat_ids.sort_unstable();
    allowed_chat_ids.dedup();
    let api_base_url = normalize_telegram_api_base_url(&name, config.api_base_url)?;
    let resolved = ResolvedTelegramConnector {
        name: config.name.clone(),
        bot_token,
        secret_token,
        allow_unauthenticated_ingress: config.allow_unauthenticated_ingress,
        api_base_url,
        ingress_mode: config.ingress_mode,
        polling_timeout_seconds: config.polling_timeout_seconds.max(1),
        ingress_events_per_second: config.ingress_events_per_second.max(1),
        allowed_chat_ids,
        fixed_session_id: config.fixed_session_id,
        include_self_output: config.include_self_output,
        additional_reply_targets: config.additional_reply_targets,
        additional_binding_keys: config.additional_binding_keys,
        session_policy: config.session_policy.normalized(),
    };
    Ok((name, resolved))
}

impl ResolvedTelegramConnector {
    /// Returns whether this connector is allowed to receive or send in one chat.
    pub fn allows_chat_id(&self, chat_id: i64) -> bool {
        self.allowed_chat_ids.is_empty() || self.allowed_chat_ids.binary_search(&chat_id).is_ok()
    }

    /// Returns the durable cursor key used by one polling connector.
    pub fn polling_cursor_key(&self) -> String {
        format!("telegram_polling:{}", self.name)
    }

    /// Returns the natural fallback session identifier for one inbound Telegram update.
    pub fn natural_session_id(&self, chat_id: i64, thread_id: Option<i64>) -> String {
        self.fixed_session_id
            .clone()
            .unwrap_or_else(|| match thread_id {
                Some(thread_id) => format!("telegram:{}:{}:{}", self.name, chat_id, thread_id),
                None => format!("telegram:{}:{}", self.name, chat_id),
            })
    }

    /// Returns opaque binding keys for one inbound Telegram update.
    pub fn binding_keys(&self, chat_id: i64, thread_id: Option<i64>) -> Vec<String> {
        let mut keys = vec![match thread_id {
            Some(thread_id) => format!("telegram:{}:{}:{}", self.name, chat_id, thread_id),
            None => format!("telegram:{}:{}", self.name, chat_id),
        }];
        keys.extend(self.additional_binding_keys.clone());
        keys.sort();
        keys.dedup();
        keys
    }

    /// Builds the default reply targets for one inbound Telegram update.
    pub fn reply_targets(
        &self,
        chat_id: i64,
        thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
    ) -> Vec<ReplyHandle> {
        let mut targets = Vec::new();
        if self.include_self_output {
            targets.push(ReplyHandle {
                plugin: "telegram".to_string(),
                address: encode_telegram_reply_route(&TelegramReplyRoute {
                    connector: self.name.clone(),
                    chat_id,
                    message_thread_id: thread_id,
                    reply_to_message_id,
                }),
            });
        }
        targets.extend(self.additional_reply_targets.clone());
        normalize_reply_targets(None, targets)
    }
}

fn default_polling_timeout_seconds() -> u64 {
    DEFAULT_POLLING_TIMEOUT_SECONDS
}

pub fn default_telegram_ingress_events_per_second() -> u32 {
    DEFAULT_TELEGRAM_INGRESS_EVENTS_PER_SECOND
}

fn normalize_telegram_api_base_url(name: &str, value: Option<String>) -> Result<String> {
    let Some(value) = value else {
        return Ok(DEFAULT_TELEGRAM_API_BASE_URL.to_string());
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("telegram connector {name} api_base_url is required when configured");
    }
    let url = Url::parse(trimmed).with_context(|| {
        format!("telegram connector {name} api_base_url is not a valid absolute URL")
    })?;
    match url.scheme() {
        "http" | "https" => {}
        other => {
            bail!(
                "telegram connector {name} api_base_url must use http:// or https://, got {other}"
            )
        }
    }
    if url.host_str().is_none() {
        bail!("telegram connector {name} api_base_url is missing a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("telegram connector {name} api_base_url must not include userinfo");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("telegram connector {name} api_base_url must not include query or fragment");
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

/// Encodes one Telegram reply route into an opaque reply address.
pub fn encode_telegram_reply_route(route: &TelegramReplyRoute) -> String {
    serde_json::to_string(route).expect("telegram reply route should serialize")
}

/// Decodes one Telegram reply route from an opaque reply address.
pub fn decode_telegram_reply_route(address: &str) -> Result<TelegramReplyRoute> {
    serde_json::from_str(address).context("invalid telegram reply route")
}

/// Builds one Telegram Bot API method URL from a configured base URL.
pub fn telegram_bot_api_method_url(api_base_url: &str, bot_token: &str, method: &str) -> String {
    let base = api_base_url.trim_end_matches('/');
    if base.ends_with("/bot") {
        format!("{base}{bot_token}/{method}")
    } else {
        format!("{base}/bot{bot_token}/{method}")
    }
}

/// Builds one Telegram raw file-download URL from a configured API base URL.
pub fn telegram_bot_file_url(api_base_url: &str, bot_token: &str, file_path: &str) -> String {
    let base = api_base_url.trim_end_matches('/');
    let root = base.strip_suffix("/bot").unwrap_or(base);
    format!(
        "{root}/file/bot{bot_token}/{}",
        file_path.trim_start_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use kheish_auth::AuthManager;

    use super::{
        ConnectorSessionPolicy, TelegramConnectorConfig, TelegramIngressMode, resolve_connector,
        telegram_bot_api_method_url, telegram_bot_file_url,
    };

    #[test]
    fn polling_connector_requires_a_bot_token() {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let error = resolve_connector(
            auth_manager.as_ref(),
            TelegramConnectorConfig {
                name: "polling".to_string(),
                bot_token: None,
                bot_token_env: None,
                bot_token_secret_ref: None,
                secret_token: None,
                secret_token_env: None,
                secret_token_secret_ref: None,
                allow_unauthenticated_ingress: false,
                api_base_url: None,
                ingress_mode: TelegramIngressMode::Polling,
                polling_timeout_seconds: 30,
                ingress_events_per_second: 100,
                allowed_chat_ids: Vec::new(),
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            },
        )
        .expect_err("polling connector without bot token should fail");
        assert!(
            error
                .to_string()
                .contains("requires bot_token, bot_token_env, or bot_token_secret_ref"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn webhook_connector_can_omit_the_bot_token() {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let (_, resolved) = resolve_connector(
            auth_manager.as_ref(),
            TelegramConnectorConfig {
                name: "webhook".to_string(),
                bot_token: None,
                bot_token_env: None,
                bot_token_secret_ref: None,
                secret_token: None,
                secret_token_env: None,
                secret_token_secret_ref: None,
                allow_unauthenticated_ingress: true,
                api_base_url: None,
                ingress_mode: TelegramIngressMode::Webhook,
                polling_timeout_seconds: 30,
                ingress_events_per_second: 100,
                allowed_chat_ids: Vec::new(),
                fixed_session_id: None,
                include_self_output: false,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            },
        )
        .expect("webhook connector without bot token should still resolve");
        assert!(resolved.bot_token.is_none());
    }

    #[test]
    fn self_output_requires_a_bot_token_even_for_webhooks() {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let error = resolve_connector(
            auth_manager.as_ref(),
            TelegramConnectorConfig {
                name: "webhook".to_string(),
                bot_token: None,
                bot_token_env: None,
                bot_token_secret_ref: None,
                secret_token: None,
                secret_token_env: None,
                secret_token_secret_ref: None,
                allow_unauthenticated_ingress: true,
                api_base_url: None,
                ingress_mode: TelegramIngressMode::Webhook,
                polling_timeout_seconds: 30,
                ingress_events_per_second: 100,
                allowed_chat_ids: Vec::new(),
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            },
        )
        .expect_err("self-output without a bot token should fail");
        assert!(
            error
                .to_string()
                .contains("when include_self_output is enabled"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn telegram_connector_rejects_empty_secrets() {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let error = resolve_connector(
            auth_manager.as_ref(),
            TelegramConnectorConfig {
                name: "webhook".to_string(),
                bot_token: Some(" ".to_string()),
                bot_token_env: None,
                bot_token_secret_ref: None,
                secret_token: None,
                secret_token_env: None,
                secret_token_secret_ref: None,
                allow_unauthenticated_ingress: true,
                api_base_url: None,
                ingress_mode: TelegramIngressMode::Webhook,
                polling_timeout_seconds: 30,
                ingress_events_per_second: 100,
                allowed_chat_ids: Vec::new(),
                fixed_session_id: None,
                include_self_output: false,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            },
        )
        .expect_err("empty bot token should fail");
        assert!(
            error.to_string().contains("bot_token cannot be empty"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn telegram_connector_normalizes_allowed_chat_ids() {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let (_, resolved) = resolve_connector(
            auth_manager.as_ref(),
            TelegramConnectorConfig {
                name: "webhook".to_string(),
                bot_token: None,
                bot_token_env: None,
                bot_token_secret_ref: None,
                secret_token: None,
                secret_token_env: None,
                secret_token_secret_ref: None,
                allow_unauthenticated_ingress: true,
                api_base_url: None,
                ingress_mode: TelegramIngressMode::Webhook,
                polling_timeout_seconds: 30,
                ingress_events_per_second: 100,
                allowed_chat_ids: vec![9, 3, 9],
                fixed_session_id: None,
                include_self_output: false,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            },
        )
        .expect("connector should resolve");
        assert_eq!(resolved.allowed_chat_ids, vec![3, 9]);
        assert!(resolved.allows_chat_id(3));
        assert!(!resolved.allows_chat_id(4));
    }

    #[test]
    fn telegram_connector_normalizes_safe_api_base_url() {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let (_, resolved) = resolve_connector(
            auth_manager.as_ref(),
            TelegramConnectorConfig {
                name: "webhook".to_string(),
                bot_token: None,
                bot_token_env: None,
                bot_token_secret_ref: None,
                secret_token: None,
                secret_token_env: None,
                secret_token_secret_ref: None,
                allow_unauthenticated_ingress: true,
                api_base_url: Some(" http://127.0.0.1:12345/bot/ ".to_string()),
                ingress_mode: TelegramIngressMode::Webhook,
                polling_timeout_seconds: 30,
                ingress_events_per_second: 100,
                allowed_chat_ids: Vec::new(),
                fixed_session_id: None,
                include_self_output: false,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            },
        )
        .expect("safe Telegram API base URL should resolve");
        assert_eq!(resolved.api_base_url, "http://127.0.0.1:12345/bot");
    }

    #[test]
    fn telegram_connector_rejects_secret_bearing_api_base_url() {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        for (api_base_url, expected) in [
            (
                "https://user:base-url-secret@example.com",
                "must not include userinfo",
            ),
            (
                "https://example.com?token=base-url-secret",
                "must not include query or fragment",
            ),
            (
                "https://example.com#base-url-secret",
                "must not include query or fragment",
            ),
        ] {
            let error = resolve_connector(
                auth_manager.as_ref(),
                TelegramConnectorConfig {
                    name: "webhook".to_string(),
                    bot_token: None,
                    bot_token_env: None,
                    bot_token_secret_ref: None,
                    secret_token: None,
                    secret_token_env: None,
                    secret_token_secret_ref: None,
                    allow_unauthenticated_ingress: true,
                    api_base_url: Some(api_base_url.to_string()),
                    ingress_mode: TelegramIngressMode::Webhook,
                    polling_timeout_seconds: 30,
                    ingress_events_per_second: 100,
                    allowed_chat_ids: Vec::new(),
                    fixed_session_id: None,
                    include_self_output: false,
                    additional_reply_targets: Vec::new(),
                    additional_binding_keys: Vec::new(),
                    session_policy: ConnectorSessionPolicy::default(),
                },
            )
            .expect_err("secret-bearing Telegram API base URL should fail");
            let rendered = format!("{error:#}");
            assert!(
                rendered.contains(expected),
                "unexpected error for {api_base_url}: {rendered}"
            );
            assert!(
                !rendered.contains("base-url-secret"),
                "api_base_url diagnostics should not leak secret material: {rendered}"
            );
        }
    }

    #[test]
    fn bot_api_method_url_handles_root_and_bot_style_bases() {
        assert_eq!(
            telegram_bot_api_method_url("https://api.telegram.org", "token", "getUpdates"),
            "https://api.telegram.org/bottoken/getUpdates"
        );
        assert_eq!(
            telegram_bot_api_method_url("https://api.telegram.org/bot", "token", "getUpdates"),
            "https://api.telegram.org/bottoken/getUpdates"
        );
        assert_eq!(
            telegram_bot_file_url("https://api.telegram.org", "token", "files/demo.png"),
            "https://api.telegram.org/file/bottoken/files/demo.png"
        );
        assert_eq!(
            telegram_bot_file_url("https://api.telegram.org/bot", "token", "/files/demo.png"),
            "https://api.telegram.org/file/bottoken/files/demo.png"
        );
    }

    #[test]
    fn resolved_telegram_debug_redacts_secrets() {
        let resolved = super::ResolvedTelegramConnector {
            name: "bot".to_string(),
            bot_token: Some("telegram-secret-token".to_string()),
            secret_token: Some("telegram-webhook-secret".to_string()),
            allow_unauthenticated_ingress: false,
            api_base_url: "https://api.telegram.org".to_string(),
            ingress_mode: TelegramIngressMode::Webhook,
            polling_timeout_seconds: 30,
            ingress_events_per_second: 100,
            allowed_chat_ids: Vec::new(),
            fixed_session_id: None,
            include_self_output: true,
            additional_reply_targets: Vec::new(),
            additional_binding_keys: Vec::new(),
            session_policy: ConnectorSessionPolicy::default(),
        };
        let debug = format!("{resolved:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("telegram-secret-token"));
        assert!(!debug.contains("telegram-webhook-secret"));
    }
}
