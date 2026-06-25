use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::backends::AuthBackend;
use crate::{
    AuthMode, AuthProvider, AuthSlotId, AuthSlotRecord, AuthSlotStatus, ResolvedAuthMaterial,
    now_ms,
};

pub const DEFAULT_ANTHROPIC_OAUTH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const DEFAULT_CLAUDE_CODE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

const FORCE_REFRESH_COOLDOWN_MS: u64 = 5_000;
const REFRESH_BUFFER_MS: u64 = 5 * 60 * 1_000;
const REQUIRED_CLAUDE_AI_SCOPE: &str = "user:inference";
const ANTHROPIC_OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AnthropicStoredState {
    ApiKey {
        api_key: String,
    },
    ClaudeCodeAccount {
        credentials_path: PathBuf,
        token_url: String,
        client_id: String,
        access_token: String,
        refresh_token: String,
        expires_at_ms: u64,
        scopes: Vec<String>,
        subscription_type: Option<String>,
        rate_limit_tier: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_refresh_at_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_refresh_outcome: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ImportedClaudeCredentials {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<ImportedClaudeOauth>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ImportedClaudeOauth {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "refreshToken")]
    refresh_token: String,
    #[serde(rename = "expiresAt")]
    expires_at_ms: u64,
    scopes: Vec<String>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
    #[serde(rename = "rateLimitTier")]
    rate_limit_tier: Option<String>,
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    grant_type: &'a str,
    refresh_token: &'a str,
    client_id: &'a str,
    scope: String,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
    scope: Option<String>,
}

pub fn default_claude_code_credentials_path() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".claude"))
        })
        .map(|root| root.join(".credentials.json"))
}

pub struct AnthropicAuthBackend {
    client: Client,
}

impl AnthropicAuthBackend {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .build()
                .context("failed to build Anthropic auth HTTP client")?,
        })
    }

    pub fn static_api_key_record(
        slot_id: AuthSlotId,
        api_key: impl Into<String>,
    ) -> Result<AuthSlotRecord> {
        let state = AnthropicStoredState::ApiKey {
            api_key: api_key.into(),
        };
        Ok(AuthSlotRecord {
            slot_id,
            provider: AuthProvider::Anthropic,
            mode: AuthMode::ApiKey,
            state: serde_json::to_value(state)?,
            updated_at_ms: now_ms(),
        })
    }

    pub fn import_claude_code_record(
        slot_id: AuthSlotId,
        path: impl Into<PathBuf>,
    ) -> Result<AuthSlotRecord> {
        Self::import_claude_code_record_with_overrides(
            slot_id,
            path,
            DEFAULT_ANTHROPIC_OAUTH_TOKEN_URL,
            DEFAULT_CLAUDE_CODE_CLIENT_ID,
        )
    }

    pub fn import_claude_code_record_with_overrides(
        slot_id: AuthSlotId,
        path: impl Into<PathBuf>,
        token_url: impl Into<String>,
        client_id: impl Into<String>,
    ) -> Result<AuthSlotRecord> {
        let path = path.into();
        let imported = Self::read_claude_credentials(&path)?;
        let oauth = imported
            .claude_ai_oauth
            .ok_or_else(|| anyhow!("Claude Code credentials do not contain claudeAiOauth"))?;
        Self::ensure_required_scope(&oauth.scopes)?;
        let state = AnthropicStoredState::ClaudeCodeAccount {
            credentials_path: path,
            token_url: token_url.into(),
            client_id: client_id.into(),
            access_token: oauth.access_token,
            refresh_token: oauth.refresh_token,
            expires_at_ms: oauth.expires_at_ms,
            scopes: oauth.scopes,
            subscription_type: oauth.subscription_type,
            rate_limit_tier: oauth.rate_limit_tier,
            last_refresh_at_ms: None,
            last_refresh_outcome: None,
        };
        Ok(AuthSlotRecord {
            slot_id,
            provider: AuthProvider::Anthropic,
            mode: AuthMode::OAuthAccount,
            state: serde_json::to_value(state)?,
            updated_at_ms: now_ms(),
        })
    }

    fn read_claude_credentials(path: &Path) -> Result<ImportedClaudeCredentials> {
        let content = std::fs::read_to_string(path).with_context(|| {
            format!(
                "failed to read Claude Code credentials file {}",
                path.display()
            )
        })?;
        serde_json::from_str(&content).with_context(|| {
            format!(
                "failed to parse Claude Code credentials file {}",
                path.display()
            )
        })
    }

    fn ensure_required_scope(scopes: &[String]) -> Result<()> {
        if scopes.iter().any(|scope| scope == REQUIRED_CLAUDE_AI_SCOPE) {
            Ok(())
        } else {
            bail!(
                "Claude Code credentials are missing the required `{REQUIRED_CLAUDE_AI_SCOPE}` scope"
            )
        }
    }

    fn parse_state(record: &AuthSlotRecord) -> Result<AnthropicStoredState> {
        serde_json::from_value(record.state.clone()).context("invalid stored Anthropic auth state")
    }

    fn write_state(record: &mut AuthSlotRecord, state: AnthropicStoredState) -> Result<()> {
        record.state = serde_json::to_value(state)?;
        record.updated_at_ms = now_ms();
        Ok(())
    }

    fn should_force_refresh(last_refresh_at_ms: Option<u64>, now: u64) -> bool {
        last_refresh_at_ms
            .map(|last| now.saturating_sub(last) >= FORCE_REFRESH_COOLDOWN_MS)
            .unwrap_or(true)
    }

    fn token_needs_refresh(expires_at_ms: u64, now: u64) -> bool {
        expires_at_ms <= now.saturating_add(REFRESH_BUFFER_MS)
    }

    fn material_from_api_key(api_key: &str) -> ResolvedAuthMaterial {
        let mut headers = BTreeMap::new();
        headers.insert("x-api-key".to_string(), api_key.to_string());
        ResolvedAuthMaterial {
            headers,
            base_url_override: None,
            grant_id: None,
            lease_id: None,
        }
    }

    fn material_from_bearer(access_token: &str) -> ResolvedAuthMaterial {
        let mut headers = BTreeMap::new();
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer {access_token}"),
        );
        headers.insert(
            "anthropic-beta".to_string(),
            ANTHROPIC_OAUTH_BETA_HEADER.to_string(),
        );
        ResolvedAuthMaterial {
            headers,
            base_url_override: None,
            grant_id: None,
            lease_id: None,
        }
    }

    async fn refresh_claude_code_tokens(
        &self,
        token_url: &str,
        client_id: &str,
        refresh_token: &str,
        scopes: &[String],
    ) -> Result<RefreshResponse> {
        let response = self
            .client
            .post(token_url)
            .header("Content-Type", "application/json")
            .json(&RefreshRequest {
                grant_type: "refresh_token",
                refresh_token,
                client_id,
                scope: scopes.join(" "),
            })
            .send()
            .await
            .context("failed to refresh Claude Code Anthropic token")?;
        if !response.status().is_success() {
            let status = response.status();
            let _ = response.text().await;
            bail!(
                "Anthropic account token refresh failed with status {status}: response body redacted"
            );
        }
        response
            .json::<RefreshResponse>()
            .await
            .context("failed to decode Anthropic refresh response")
    }
}

#[async_trait]
impl AuthBackend for AnthropicAuthBackend {
    fn provider(&self) -> AuthProvider {
        AuthProvider::Anthropic
    }

    fn can_resolve_without_lock(&self, record: &AuthSlotRecord, force_refresh: bool) -> bool {
        if force_refresh {
            return false;
        }
        match Self::parse_state(record) {
            Ok(AnthropicStoredState::ApiKey { .. }) => true,
            Ok(AnthropicStoredState::ClaudeCodeAccount { expires_at_ms, .. }) => {
                !Self::token_needs_refresh(expires_at_ms, now_ms())
            }
            Err(_) => false,
        }
    }

    async fn resolve(
        &self,
        record: &mut AuthSlotRecord,
        force_refresh: bool,
    ) -> Result<ResolvedAuthMaterial> {
        let mut state = Self::parse_state(record)?;
        let original_state = state.clone();
        let material = match &mut state {
            AnthropicStoredState::ApiKey { api_key } => Self::material_from_api_key(api_key),
            AnthropicStoredState::ClaudeCodeAccount {
                token_url,
                client_id,
                access_token,
                refresh_token,
                expires_at_ms,
                scopes,
                last_refresh_at_ms,
                last_refresh_outcome,
                ..
            } => {
                let now = now_ms();
                if Self::token_needs_refresh(*expires_at_ms, now)
                    || (force_refresh && Self::should_force_refresh(*last_refresh_at_ms, now))
                {
                    let refreshed = self
                        .refresh_claude_code_tokens(
                            token_url,
                            client_id,
                            refresh_token.as_str(),
                            scopes,
                        )
                        .await?;
                    let next_scopes = refreshed
                        .scope
                        .as_deref()
                        .map(|scope| {
                            scope
                                .split_whitespace()
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_else(|| scopes.clone());
                    ensure_no_scope_escalation(scopes, &next_scopes)?;
                    Self::ensure_required_scope(&next_scopes)?;
                    *access_token = refreshed.access_token;
                    if let Some(next_refresh_token) = refreshed.refresh_token {
                        *refresh_token = next_refresh_token;
                    }
                    *expires_at_ms = now.saturating_add(refreshed.expires_in.saturating_mul(1_000));
                    *scopes = next_scopes;
                    *last_refresh_at_ms = Some(now);
                    *last_refresh_outcome = Some("success".to_string());
                }
                Self::material_from_bearer(access_token)
            }
        };
        if state != original_state {
            Self::write_state(record, state)?;
        }
        Ok(material)
    }

    fn status(&self, record: &AuthSlotRecord) -> Result<AuthSlotStatus> {
        let state = Self::parse_state(record)?;
        let mut details = BTreeMap::new();
        let summary = match state {
            AnthropicStoredState::ApiKey { .. } => "api_key".to_string(),
            AnthropicStoredState::ClaudeCodeAccount {
                credentials_path,
                subscription_type,
                rate_limit_tier,
                expires_at_ms,
                scopes,
                last_refresh_at_ms,
                last_refresh_outcome,
                ..
            } => {
                details.insert(
                    "source".to_string(),
                    json!(credentials_path.display().to_string()),
                );
                details.insert("expires_at_ms".to_string(), json!(expires_at_ms));
                details.insert("scopes".to_string(), json!(scopes));
                if let Some(subscription_type) = subscription_type.as_ref() {
                    details.insert("subscription_type".to_string(), json!(subscription_type));
                }
                if let Some(rate_limit_tier) = rate_limit_tier.as_ref() {
                    details.insert("rate_limit_tier".to_string(), json!(rate_limit_tier));
                }
                if let Some(last_refresh_at_ms) = last_refresh_at_ms {
                    details.insert("last_refresh_at_ms".to_string(), json!(last_refresh_at_ms));
                }
                if let Some(last_refresh_outcome) = last_refresh_outcome.as_ref() {
                    details.insert(
                        "last_refresh_outcome".to_string(),
                        json!(last_refresh_outcome),
                    );
                }
                match subscription_type {
                    Some(subscription_type) => format!(
                        "claude_code_account:{} ({})",
                        subscription_type,
                        credentials_path.display()
                    ),
                    None => format!("claude_code_account ({})", credentials_path.display()),
                }
            }
        };
        Ok(AuthSlotStatus {
            slot_id: record.slot_id.clone(),
            provider: record.provider,
            mode: record.mode,
            summary,
            updated_at_ms: record.updated_at_ms,
            details,
        })
    }
}

fn ensure_no_scope_escalation(existing: &[String], next: &[String]) -> Result<()> {
    for scope in next {
        anyhow::ensure!(
            existing.iter().any(|existing| existing == scope),
            "Anthropic refresh response attempted to add unapproved scope `{scope}`"
        );
    }
    Ok(())
}
