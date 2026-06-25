use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use kheish_types::{ReplyHandle, normalize_reply_targets};

use super::{ConnectorSessionPolicy, default_true, resolve_secret};

const DEFAULT_SLACK_API_BASE_URL: &str = "https://slack.com/api";
const DEFAULT_SLACK_INGRESS_EVENTS_PER_SECOND: u32 = 60;

/// A Slack bot connector used for both webhook ingress and Web API egress.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlackConnectorConfig {
    /// Stable connector identifier used in routes and reply addresses.
    pub name: String,
    /// Slack bot token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,
    /// Environment variable containing the Slack bot token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token_env: Option<String>,
    /// Secret-store slot containing the Slack bot token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token_secret_ref: Option<String>,
    /// Optional Slack signing secret used to verify webhook requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_secret: Option<String>,
    /// Environment variable containing the Slack signing secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_secret_env: Option<String>,
    /// Secret-store slot containing the Slack signing secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_secret_secret_ref: Option<String>,
    /// Allows inbound webhook requests without Slack request authentication.
    #[serde(default)]
    pub allow_unauthenticated_ingress: bool,
    /// Override Slack API base URL for tests or self-hosted proxies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    /// Optional fixed session identifier. When omitted, sessions are derived from channel/thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_session_id: Option<String>,
    /// Whether replies should default to the same Slack thread or channel.
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
    /// Maximum accepted Slack Events API callbacks per second for one connector/team/channel scope.
    #[serde(default = "default_slack_ingress_events_per_second")]
    pub ingress_events_per_second: u32,
    /// Optional Slack app IDs allowed to submit Events API payloads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_api_app_ids: Vec<String>,
    /// Optional Enterprise Grid IDs allowed to submit Events API payloads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_enterprise_ids: Vec<String>,
    /// Optional workspace/team IDs allowed to submit Events API payloads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_team_ids: Vec<String>,
    /// Optional channel IDs allowed for ingress and egress.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_channel_ids: Vec<String>,
    /// Optional file download hosts. When empty, Slack file hosts and the configured API host are allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_file_hosts: Vec<String>,
    /// Optional per-team bot tokens used for Enterprise Grid routing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub team_bot_tokens: Vec<SlackTeamBotTokenConfig>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlackTeamBotTokenConfig {
    pub team_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token_secret_ref: Option<String>,
}

/// One resolved Slack connector with secrets loaded.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedSlackConnector {
    pub name: String,
    pub bot_token: Option<String>,
    pub signing_secret: Option<String>,
    pub allow_unauthenticated_ingress: bool,
    pub api_base_url: String,
    pub fixed_session_id: Option<String>,
    pub include_self_output: bool,
    pub additional_reply_targets: Vec<ReplyHandle>,
    pub additional_binding_keys: Vec<String>,
    pub session_policy: ConnectorSessionPolicy,
    pub ingress_events_per_second: u32,
    pub allowed_api_app_ids: Vec<String>,
    pub allowed_enterprise_ids: Vec<String>,
    pub allowed_team_ids: Vec<String>,
    pub allowed_channel_ids: Vec<String>,
    pub allowed_file_hosts: Vec<String>,
    pub team_bot_tokens: BTreeMap<String, String>,
}

impl fmt::Debug for ResolvedSlackConnector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedSlackConnector")
            .field("name", &self.name)
            .field("bot_token", &self.bot_token.as_ref().map(|_| "<redacted>"))
            .field(
                "signing_secret",
                &self.signing_secret.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "allow_unauthenticated_ingress",
                &self.allow_unauthenticated_ingress,
            )
            .field("api_base_url", &self.api_base_url)
            .field("fixed_session_id", &self.fixed_session_id)
            .field("include_self_output", &self.include_self_output)
            .field("additional_reply_targets", &self.additional_reply_targets)
            .field("additional_binding_keys", &self.additional_binding_keys)
            .field("session_policy", &self.session_policy)
            .field("ingress_events_per_second", &self.ingress_events_per_second)
            .field("allowed_api_app_ids", &self.allowed_api_app_ids)
            .field("allowed_enterprise_ids", &self.allowed_enterprise_ids)
            .field("allowed_team_ids", &self.allowed_team_ids)
            .field("allowed_channel_ids", &self.allowed_channel_ids)
            .field("allowed_file_hosts", &self.allowed_file_hosts)
            .field(
                "team_bot_tokens",
                &self
                    .team_bot_tokens
                    .keys()
                    .map(|team_id| (team_id, "<redacted>"))
                    .collect::<BTreeMap<_, _>>(),
            )
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackReplyRoute {
    pub connector: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    pub channel_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
}

pub(super) fn resolve_connector(
    auth_manager: &kheish_auth::AuthManager,
    config: SlackConnectorConfig,
) -> Result<(String, ResolvedSlackConnector)> {
    let name = config.name.clone();
    let signing_secret = resolve_secret(
        auth_manager,
        config.signing_secret,
        config.signing_secret_env,
        config.signing_secret_secret_ref,
        &format!("slack connector {name} signing_secret"),
    )?;
    if signing_secret.is_none() && !config.allow_unauthenticated_ingress {
        anyhow::bail!(
            "slack connector {name} requires signing_secret, signing_secret_env, signing_secret_secret_ref, or allow_unauthenticated_ingress=true"
        );
    }
    let bot_token = resolve_secret(
        auth_manager,
        config.bot_token,
        config.bot_token_env,
        config.bot_token_secret_ref,
        &format!("slack connector {name} bot_token"),
    )?;
    let mut team_bot_tokens = BTreeMap::new();
    for team_config in config.team_bot_tokens {
        let team_id = normalize_identifier(&team_config.team_id);
        anyhow::ensure!(
            !team_id.is_empty(),
            "slack connector {name} team_bot_tokens entry requires team_id"
        );
        let token = resolve_secret(
            auth_manager,
            team_config.bot_token,
            team_config.bot_token_env,
            team_config.bot_token_secret_ref,
            &format!("slack connector {name} team_bot_tokens[{team_id}].bot_token"),
        )?;
        let token = token.ok_or_else(|| {
            anyhow::anyhow!(
                "slack connector {name} team_bot_tokens[{team_id}] requires bot_token, bot_token_env, or bot_token_secret_ref"
            )
        })?;
        if team_bot_tokens.insert(team_id.clone(), token).is_some() {
            anyhow::bail!(
                "slack connector {name} defines duplicate team_bot_tokens entry `{team_id}`"
            );
        }
    }
    if config.include_self_output && bot_token.is_none() && team_bot_tokens.is_empty() {
        anyhow::bail!(
            "slack connector {name} requires bot_token, bot_token_env, bot_token_secret_ref, or team_bot_tokens when include_self_output is enabled"
        );
    }
    anyhow::ensure!(
        config.ingress_events_per_second > 0,
        "slack connector {name} ingress_events_per_second must be greater than zero"
    );
    let resolved = ResolvedSlackConnector {
        name: config.name.clone(),
        bot_token,
        signing_secret,
        allow_unauthenticated_ingress: config.allow_unauthenticated_ingress,
        api_base_url: config
            .api_base_url
            .unwrap_or_else(|| DEFAULT_SLACK_API_BASE_URL.to_string()),
        fixed_session_id: config.fixed_session_id,
        include_self_output: config.include_self_output,
        additional_reply_targets: config.additional_reply_targets,
        additional_binding_keys: config.additional_binding_keys,
        session_policy: config.session_policy.normalized(),
        ingress_events_per_second: config.ingress_events_per_second,
        allowed_api_app_ids: normalize_list(config.allowed_api_app_ids),
        allowed_enterprise_ids: normalize_list(config.allowed_enterprise_ids),
        allowed_team_ids: normalize_list(config.allowed_team_ids),
        allowed_channel_ids: normalize_list(config.allowed_channel_ids),
        allowed_file_hosts: normalize_list(config.allowed_file_hosts)
            .into_iter()
            .map(|host| host.to_ascii_lowercase())
            .collect(),
        team_bot_tokens,
    };
    Ok((name, resolved))
}

impl ResolvedSlackConnector {
    pub fn natural_session_id_scoped(
        &self,
        channel_id: &str,
        root_ts: &str,
        enterprise_id: Option<&str>,
        team_id: Option<&str>,
    ) -> String {
        self.fixed_session_id.clone().unwrap_or_else(|| {
            self.scoped_conversation_key(channel_id, root_ts, enterprise_id, team_id)
        })
    }

    pub fn binding_keys_scoped(
        &self,
        channel_id: &str,
        root_ts: &str,
        enterprise_id: Option<&str>,
        team_id: Option<&str>,
    ) -> Vec<String> {
        let mut keys =
            vec![self.scoped_conversation_key(channel_id, root_ts, enterprise_id, team_id)];
        keys.extend(self.additional_binding_keys.clone());
        keys.sort();
        keys.dedup();
        keys
    }

    pub fn reply_targets_scoped(
        &self,
        channel_id: &str,
        thread_ts: Option<String>,
        enterprise_id: Option<String>,
        team_id: Option<String>,
    ) -> Vec<ReplyHandle> {
        let mut targets = Vec::new();
        if self.include_self_output {
            targets.push(ReplyHandle {
                plugin: "slack".to_string(),
                address: encode_slack_reply_route(&SlackReplyRoute {
                    connector: self.name.clone(),
                    enterprise_id,
                    team_id,
                    channel_id: channel_id.to_string(),
                    thread_ts,
                }),
            });
        }
        targets.extend(self.additional_reply_targets.clone());
        normalize_reply_targets(None, targets)
    }

    pub fn bot_token_for_team(&self, team_id: Option<&str>) -> Option<&str> {
        team_id
            .and_then(|team_id| self.team_bot_tokens.get(team_id).map(String::as_str))
            .or(self.bot_token.as_deref())
    }

    pub fn is_channel_allowed(&self, channel_id: &str) -> bool {
        self.allowed_channel_ids.is_empty()
            || self
                .allowed_channel_ids
                .iter()
                .any(|allowed| allowed == channel_id)
    }

    pub fn is_team_allowed(&self, team_id: Option<&str>) -> bool {
        allowlist_matches_optional(&self.allowed_team_ids, team_id)
    }

    pub fn is_enterprise_allowed(&self, enterprise_id: Option<&str>) -> bool {
        allowlist_matches_optional(&self.allowed_enterprise_ids, enterprise_id)
    }

    pub fn is_api_app_allowed(&self, api_app_id: Option<&str>) -> bool {
        allowlist_matches_optional(&self.allowed_api_app_ids, api_app_id)
    }

    fn scoped_conversation_key(
        &self,
        channel_id: &str,
        root_ts: &str,
        enterprise_id: Option<&str>,
        team_id: Option<&str>,
    ) -> String {
        match (
            normalize_optional_identifier(enterprise_id),
            normalize_optional_identifier(team_id),
        ) {
            (Some(enterprise_id), Some(team_id)) => format!(
                "slack:{}:enterprise:{}:team:{}:{}:{}",
                self.name, enterprise_id, team_id, channel_id, root_ts
            ),
            (Some(enterprise_id), None) => format!(
                "slack:{}:enterprise:{}:{}:{}",
                self.name, enterprise_id, channel_id, root_ts
            ),
            (None, Some(team_id)) => format!(
                "slack:{}:team:{}:{}:{}",
                self.name, team_id, channel_id, root_ts
            ),
            (None, None) => format!("slack:{}:{}:{}", self.name, channel_id, root_ts),
        }
    }
}

/// Encodes one Slack reply route into an opaque reply address.
pub fn encode_slack_reply_route(route: &SlackReplyRoute) -> String {
    serde_json::to_string(route).expect("slack reply route should serialize")
}

/// Decodes one Slack reply route from an opaque reply address.
pub fn decode_slack_reply_route(address: &str) -> Result<SlackReplyRoute> {
    serde_json::from_str(address).context("invalid slack reply route")
}

fn normalize_identifier(value: &str) -> String {
    value.trim().to_string()
}

fn normalize_optional_identifier(value: Option<&str>) -> Option<String> {
    value
        .map(normalize_identifier)
        .filter(|value| !value.is_empty())
}

fn normalize_list(values: Vec<String>) -> Vec<String> {
    let mut values = values
        .into_iter()
        .map(|value| normalize_identifier(&value))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

fn allowlist_matches_optional(allowlist: &[String], value: Option<&str>) -> bool {
    if allowlist.is_empty() {
        return true;
    }
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    allowlist.iter().any(|allowed| allowed == value)
}

pub fn default_slack_ingress_events_per_second() -> u32 {
    DEFAULT_SLACK_INGRESS_EVENTS_PER_SECOND
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use kheish_auth::AuthManager;

    use super::{
        ConnectorSessionPolicy, SlackConnectorConfig, default_slack_ingress_events_per_second,
        resolve_connector,
    };

    #[test]
    fn slack_self_output_requires_a_bot_token() {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let error = resolve_connector(
            auth_manager.as_ref(),
            SlackConnectorConfig {
                name: "workspace".to_string(),
                bot_token: None,
                bot_token_env: None,
                bot_token_secret_ref: None,
                signing_secret: None,
                signing_secret_env: None,
                signing_secret_secret_ref: None,
                allow_unauthenticated_ingress: true,
                api_base_url: None,
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
                ingress_events_per_second: default_slack_ingress_events_per_second(),
                allowed_api_app_ids: Vec::new(),
                allowed_enterprise_ids: Vec::new(),
                allowed_team_ids: Vec::new(),
                allowed_channel_ids: Vec::new(),
                allowed_file_hosts: Vec::new(),
                team_bot_tokens: Vec::new(),
            },
        )
        .expect_err("slack self-output without a bot token should fail");
        assert!(
            error
                .to_string()
                .contains("when include_self_output is enabled"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn slack_without_self_output_can_omit_bot_token() {
        let temp = tempdir().expect("tempdir should exist");
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let (_, resolved) = resolve_connector(
            auth_manager.as_ref(),
            SlackConnectorConfig {
                name: "workspace".to_string(),
                bot_token: None,
                bot_token_env: None,
                bot_token_secret_ref: None,
                signing_secret: None,
                signing_secret_env: None,
                signing_secret_secret_ref: None,
                allow_unauthenticated_ingress: true,
                api_base_url: None,
                fixed_session_id: None,
                include_self_output: false,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
                ingress_events_per_second: default_slack_ingress_events_per_second(),
                allowed_api_app_ids: Vec::new(),
                allowed_enterprise_ids: Vec::new(),
                allowed_team_ids: Vec::new(),
                allowed_channel_ids: Vec::new(),
                allowed_file_hosts: Vec::new(),
                team_bot_tokens: Vec::new(),
            },
        )
        .expect("slack connector without self-output should resolve without a bot token");
        assert!(resolved.bot_token.is_none());
    }

    #[test]
    fn resolved_slack_debug_redacts_secrets() {
        let resolved = super::ResolvedSlackConnector {
            name: "workspace".to_string(),
            bot_token: Some("xoxb-secret-token".to_string()),
            signing_secret: Some("slack-signing-secret".to_string()),
            allow_unauthenticated_ingress: false,
            api_base_url: "https://slack.com/api".to_string(),
            fixed_session_id: None,
            include_self_output: true,
            additional_reply_targets: Vec::new(),
            additional_binding_keys: Vec::new(),
            session_policy: ConnectorSessionPolicy::default(),
            ingress_events_per_second: default_slack_ingress_events_per_second(),
            allowed_api_app_ids: Vec::new(),
            allowed_enterprise_ids: Vec::new(),
            allowed_team_ids: Vec::new(),
            allowed_channel_ids: Vec::new(),
            allowed_file_hosts: Vec::new(),
            team_bot_tokens: std::collections::BTreeMap::new(),
        };
        let debug = format!("{resolved:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("xoxb-secret-token"));
        assert!(!debug.contains("slack-signing-secret"));
    }
}
