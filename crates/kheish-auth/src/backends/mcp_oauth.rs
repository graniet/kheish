use std::collections::BTreeMap;
use std::fmt::Formatter;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::backends::AuthBackend;
use crate::oauth::oauth_url_query_key_is_sensitive;
use crate::{
    AuthMode, AuthProvider, AuthSlotId, AuthSlotRecord, AuthSlotStatus, ResolvedAuthMaterial,
    now_ms,
};

const REFRESH_BUFFER_MS: u64 = 5 * 60 * 1_000;
const FORCE_REFRESH_COOLDOWN_MS: u64 = 5_000;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpOAuthStoredState {
    McpOAuthAccount {
        server_name: String,
        resource: String,
        issuer: String,
        authorization_endpoint: String,
        token_endpoint: String,
        client_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_secret: Option<String>,
        access_token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scopes: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_refresh_at_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_refresh_outcome: Option<String>,
    },
}

impl std::fmt::Debug for McpOAuthStoredState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::McpOAuthAccount {
                server_name,
                resource,
                issuer,
                authorization_endpoint,
                token_endpoint,
                client_id,
                expires_at_ms,
                scopes,
                last_refresh_at_ms,
                last_refresh_outcome,
                ..
            } => f
                .debug_struct("McpOAuthAccount")
                .field("server_name", server_name)
                .field("resource", resource)
                .field("issuer", issuer)
                .field("authorization_endpoint", authorization_endpoint)
                .field("token_endpoint", token_endpoint)
                .field("client_id", client_id)
                .field("client_secret", &"<redacted>")
                .field("access_token", &"<redacted>")
                .field("refresh_token", &"<redacted>")
                .field("expires_at_ms", expires_at_ms)
                .field("scopes", scopes)
                .field("last_refresh_at_ms", last_refresh_at_ms)
                .field("last_refresh_outcome", last_refresh_outcome)
                .finish(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpOAuthAccountRecordInput {
    pub slot_id: AuthSlotId,
    pub server_name: String,
    pub resource: String,
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at_ms: Option<u64>,
    pub scopes: Vec<String>,
}

impl std::fmt::Debug for McpOAuthAccountRecordInput {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpOAuthAccountRecordInput")
            .field("slot_id", &self.slot_id)
            .field("server_name", &self.server_name)
            .field("resource", &self.resource)
            .field("issuer", &self.issuer)
            .field("authorization_endpoint", &self.authorization_endpoint)
            .field("token_endpoint", &self.token_endpoint)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("scopes", &self.scopes)
            .finish()
    }
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
}

pub struct McpOAuthAuthBackend {
    client: Client,
}

impl McpOAuthAuthBackend {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .build()
                .context("failed to build MCP OAuth HTTP client")?,
        })
    }

    pub fn account_record(input: McpOAuthAccountRecordInput) -> Result<AuthSlotRecord> {
        let state = McpOAuthStoredState::McpOAuthAccount {
            server_name: non_empty(input.server_name, "server_name")?,
            resource: secure_url(non_empty(input.resource, "resource")?, "resource", true)?,
            issuer: secure_url(non_empty(input.issuer, "issuer")?, "issuer", true)?,
            authorization_endpoint: secure_url(
                non_empty(input.authorization_endpoint, "authorization_endpoint")?,
                "authorization_endpoint",
                false,
            )?,
            token_endpoint: secure_url(
                non_empty(input.token_endpoint, "token_endpoint")?,
                "token_endpoint",
                false,
            )?,
            client_id: non_empty(input.client_id, "client_id")?,
            client_secret: input.client_secret.filter(|value| !value.trim().is_empty()),
            access_token: non_empty(input.access_token, "access_token")?,
            refresh_token: input.refresh_token.filter(|value| !value.trim().is_empty()),
            expires_at_ms: input.expires_at_ms,
            scopes: normalize_scopes(input.scopes),
            last_refresh_at_ms: None,
            last_refresh_outcome: None,
        };
        Ok(AuthSlotRecord {
            slot_id: input.slot_id,
            provider: AuthProvider::McpOAuth,
            mode: AuthMode::OAuthAccount,
            state: serde_json::to_value(state)?,
            updated_at_ms: now_ms(),
        })
    }

    pub fn parse_state(record: &AuthSlotRecord) -> Result<McpOAuthStoredState> {
        let state: McpOAuthStoredState = serde_json::from_value(record.state.clone())
            .context("invalid stored MCP OAuth state")?;
        validate_stored_state_urls(&state)?;
        Ok(state)
    }

    fn write_state(record: &mut AuthSlotRecord, state: McpOAuthStoredState) -> Result<()> {
        record.state = serde_json::to_value(state)?;
        record.updated_at_ms = now_ms();
        Ok(())
    }

    fn token_needs_refresh(expires_at_ms: Option<u64>, now: u64) -> bool {
        expires_at_ms
            .map(|expires_at_ms| expires_at_ms <= now.saturating_add(REFRESH_BUFFER_MS))
            .unwrap_or(false)
    }

    fn should_force_refresh(last_refresh_at_ms: Option<u64>, now: u64) -> bool {
        last_refresh_at_ms
            .map(|last| now.saturating_sub(last) >= FORCE_REFRESH_COOLDOWN_MS)
            .unwrap_or(true)
    }

    fn material(access_token: &str) -> ResolvedAuthMaterial {
        let mut headers = BTreeMap::new();
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer {access_token}"),
        );
        ResolvedAuthMaterial {
            headers,
            base_url_override: None,
            grant_id: None,
            lease_id: None,
        }
    }

    async fn refresh_tokens(
        &self,
        token_endpoint: &str,
        client_id: &str,
        client_secret: Option<&str>,
        refresh_token: &str,
        resource: &str,
        scopes: &[String],
    ) -> Result<RefreshResponse> {
        let mut form = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
            ("client_id".to_string(), client_id.to_string()),
            ("resource".to_string(), resource.to_string()),
        ];
        if let Some(client_secret) = client_secret {
            form.push(("client_secret".to_string(), client_secret.to_string()));
        }
        if !scopes.is_empty() {
            form.push(("scope".to_string(), scopes.join(" ")));
        }
        let response = self
            .client
            .post(token_endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&form)
            .send()
            .await
            .context("failed to refresh MCP OAuth token")?;
        if !response.status().is_success() {
            let status = response.status();
            let _ = response.text().await;
            bail!("MCP OAuth token refresh failed with status {status}: response body redacted");
        }
        let parsed = response
            .json::<RefreshResponse>()
            .await
            .context("failed to decode MCP OAuth refresh response")?;
        if let Some(token_type) = parsed.token_type.as_deref() {
            anyhow::ensure!(
                token_type.eq_ignore_ascii_case("bearer"),
                "MCP OAuth refresh returned unsupported token_type `{token_type}`"
            );
        }
        Ok(parsed)
    }
}

#[async_trait]
impl AuthBackend for McpOAuthAuthBackend {
    fn provider(&self) -> AuthProvider {
        AuthProvider::McpOAuth
    }

    fn can_resolve_without_lock(&self, record: &AuthSlotRecord, force_refresh: bool) -> bool {
        if force_refresh {
            return false;
        }
        match Self::parse_state(record) {
            Ok(McpOAuthStoredState::McpOAuthAccount { expires_at_ms, .. }) => {
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
            McpOAuthStoredState::McpOAuthAccount {
                resource,
                token_endpoint,
                client_id,
                client_secret,
                access_token,
                refresh_token: refresh_token_slot,
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
                    let refresh_token = refresh_token_slot
                        .as_deref()
                        .ok_or_else(|| anyhow!("MCP OAuth account has no refresh_token"))?;
                    let refreshed = self
                        .refresh_tokens(
                            token_endpoint,
                            client_id,
                            client_secret.as_deref(),
                            refresh_token,
                            resource,
                            scopes,
                        )
                        .await?;
                    *access_token = refreshed.access_token;
                    if let Some(next_refresh_token) = refreshed.refresh_token {
                        *refresh_token_slot = Some(next_refresh_token);
                    }
                    if let Some(expires_in) = refreshed.expires_in {
                        *expires_at_ms = Some(now.saturating_add(expires_in.saturating_mul(1_000)));
                    }
                    if let Some(scope) = refreshed.scope {
                        let next_scopes = normalize_scope_string(&scope);
                        ensure_no_scope_escalation(scopes, &next_scopes)?;
                        *scopes = next_scopes;
                    }
                    *last_refresh_at_ms = Some(now);
                    *last_refresh_outcome = Some("success".to_string());
                }
                Self::material(access_token)
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
            McpOAuthStoredState::McpOAuthAccount {
                server_name,
                resource,
                issuer,
                authorization_endpoint,
                token_endpoint,
                client_id,
                expires_at_ms,
                scopes,
                last_refresh_at_ms,
                last_refresh_outcome,
                ..
            } => {
                details.insert("server_name".to_string(), json!(server_name));
                details.insert("resource".to_string(), json!(resource));
                details.insert("issuer".to_string(), json!(issuer));
                details.insert(
                    "authorization_endpoint".to_string(),
                    json!(authorization_endpoint),
                );
                details.insert("token_endpoint".to_string(), json!(token_endpoint));
                details.insert("client_id".to_string(), json!(client_id));
                if let Some(expires_at_ms) = expires_at_ms {
                    details.insert("expires_at_ms".to_string(), json!(expires_at_ms));
                }
                if !scopes.is_empty() {
                    details.insert("scopes".to_string(), json!(scopes));
                }
                if let Some(last_refresh_at_ms) = last_refresh_at_ms {
                    details.insert("last_refresh_at_ms".to_string(), json!(last_refresh_at_ms));
                }
                if let Some(last_refresh_outcome) = last_refresh_outcome {
                    details.insert(
                        "last_refresh_outcome".to_string(),
                        json!(last_refresh_outcome),
                    );
                }
                format!("mcp_oauth_account:{server_name}")
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

fn non_empty(value: String, field: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("MCP OAuth {field} cannot be empty");
    }
    Ok(trimmed.to_string())
}

fn secure_url(value: String, field: &str, trim_trailing_slash: bool) -> Result<String> {
    let parsed = Url::parse(&value).with_context(|| format!("invalid MCP OAuth {field} URL"))?;
    let host = parsed.host_str().unwrap_or_default();
    let is_loopback = matches!(host, "127.0.0.1" | "localhost" | "::1");
    anyhow::ensure!(
        parsed.scheme() == "https" || (parsed.scheme() == "http" && is_loopback),
        "MCP OAuth {field} URL must use https except loopback test URLs"
    );
    anyhow::ensure!(
        parsed.fragment().is_none(),
        "MCP OAuth {field} URL must not contain a fragment"
    );
    anyhow::ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "MCP OAuth {field} URL must not contain userinfo"
    );
    for (key, _) in parsed.query_pairs() {
        anyhow::ensure!(
            !oauth_url_query_key_is_sensitive(&key),
            "MCP OAuth {field} URL must not contain sensitive query parameter `{key}`"
        );
    }
    if trim_trailing_slash {
        Ok(parsed.to_string().trim_end_matches('/').to_string())
    } else {
        Ok(parsed.to_string())
    }
}

fn validate_stored_state_urls(state: &McpOAuthStoredState) -> Result<()> {
    let McpOAuthStoredState::McpOAuthAccount {
        resource,
        issuer,
        authorization_endpoint,
        token_endpoint,
        ..
    } = state;
    secure_url(resource.clone(), "resource", true)?;
    secure_url(issuer.clone(), "issuer", true)?;
    secure_url(
        authorization_endpoint.clone(),
        "authorization_endpoint",
        false,
    )?;
    secure_url(token_endpoint.clone(), "token_endpoint", false)?;
    Ok(())
}

fn normalize_scopes(scopes: Vec<String>) -> Vec<String> {
    let mut scopes = scopes
        .into_iter()
        .flat_map(|scope| normalize_scope_string(&scope))
        .collect::<Vec<_>>();
    scopes.sort();
    scopes.dedup();
    scopes
}

fn normalize_scope_string(scope: &str) -> Vec<String> {
    scope
        .split_whitespace()
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn ensure_no_scope_escalation(existing: &[String], next: &[String]) -> Result<()> {
    for scope in next {
        anyhow::ensure!(
            existing.iter().any(|existing| existing == scope),
            "MCP OAuth refresh response attempted to add unapproved scope `{scope}`"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input() -> McpOAuthAccountRecordInput {
        McpOAuthAccountRecordInput {
            slot_id: AuthSlotId::new("mcp.oauth.test"),
            server_name: "test".to_string(),
            resource: "https://mcp.example.com/mcp".to_string(),
            issuer: "https://issuer.example.com".to_string(),
            authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
            token_endpoint: "https://issuer.example.com/token".to_string(),
            client_id: "client".to_string(),
            client_secret: None,
            access_token: "access-token".to_string(),
            refresh_token: None,
            expires_at_ms: None,
            scopes: vec!["read".to_string()],
        }
    }

    #[test]
    fn account_record_rejects_oauth_urls_with_userinfo() {
        let mut input = base_input();
        input.token_endpoint = "https://user:pass@issuer.example.com/token".to_string();
        let error =
            McpOAuthAuthBackend::account_record(input).expect_err("userinfo should be rejected");
        assert!(error.to_string().contains("must not contain userinfo"));
    }

    #[test]
    fn account_record_rejects_oauth_urls_with_sensitive_query_keys() {
        let mut input = base_input();
        input.authorization_endpoint =
            "https://issuer.example.com/authorize?client_secret=leak".to_string();
        let error = McpOAuthAuthBackend::account_record(input)
            .expect_err("sensitive query key should be rejected");
        assert!(
            error
                .to_string()
                .contains("sensitive query parameter `client_secret`")
        );
    }

    #[test]
    fn parse_state_rejects_legacy_secret_bearing_urls() {
        let mut record =
            McpOAuthAuthBackend::account_record(base_input()).expect("valid account record");
        if let serde_json::Value::Object(state) = &mut record.state {
            state.insert(
                "token_endpoint".to_string(),
                json!("https://issuer.example.com/token?clientSecret=legacy"),
            );
        }
        let error = McpOAuthAuthBackend::parse_state(&record)
            .expect_err("legacy sensitive query keys should be rejected");
        assert!(
            error
                .to_string()
                .contains("sensitive query parameter `clientSecret`")
        );
    }
}
