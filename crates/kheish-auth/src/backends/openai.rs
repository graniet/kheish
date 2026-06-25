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

pub const DEFAULT_OPENAI_AUTH_ISSUER: &str = "https://auth.openai.com";
pub const DEFAULT_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const DEFAULT_OPENAI_CODEX_API_BASE_URL: &str =
    "https://chatgpt.com/backend-api/codex/responses";
const FORCE_REFRESH_COOLDOWN_MS: u64 = 5_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum OpenAiStoredState {
    ApiKey {
        api_key: String,
        organization: Option<String>,
        project: Option<String>,
    },
    CodexAccount {
        codex_auth_path: PathBuf,
        issuer: String,
        client_id: String,
        api_base_url: String,
        access_token: Option<String>,
        refresh_token: String,
        account_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_refresh_at_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_refresh_outcome: Option<String>,
        organization: Option<String>,
        project: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ImportedCodexAuth {
    auth_mode: Option<String>,
    #[serde(rename = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
    tokens: Option<ImportedCodexTokens>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ImportedCodexTokens {
    access_token: Option<String>,
    refresh_token: String,
    account_id: Option<String>,
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
}

pub struct OpenAiAuthBackend {
    client: Client,
}

impl OpenAiAuthBackend {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .build()
                .context("failed to build OpenAI auth HTTP client")?,
        })
    }

    pub fn static_api_key_record(
        slot_id: AuthSlotId,
        api_key: impl Into<String>,
        organization: Option<String>,
        project: Option<String>,
    ) -> Result<AuthSlotRecord> {
        let state = OpenAiStoredState::ApiKey {
            api_key: api_key.into(),
            organization,
            project,
        };
        Ok(AuthSlotRecord {
            slot_id,
            provider: AuthProvider::OpenAi,
            mode: AuthMode::ApiKey,
            state: serde_json::to_value(state)?,
            updated_at_ms: now_ms(),
        })
    }

    pub fn import_codex_record(
        slot_id: AuthSlotId,
        path: impl Into<PathBuf>,
        organization: Option<String>,
        project: Option<String>,
    ) -> Result<AuthSlotRecord> {
        Self::import_codex_record_with_overrides(
            slot_id,
            path,
            DEFAULT_OPENAI_AUTH_ISSUER,
            DEFAULT_CODEX_CLIENT_ID,
            DEFAULT_OPENAI_CODEX_API_BASE_URL,
            organization,
            project,
        )
    }

    pub fn import_codex_record_with_overrides(
        slot_id: AuthSlotId,
        path: impl Into<PathBuf>,
        issuer: impl Into<String>,
        client_id: impl Into<String>,
        api_base_url: impl Into<String>,
        organization: Option<String>,
        project: Option<String>,
    ) -> Result<AuthSlotRecord> {
        let path = path.into();
        let imported = Self::read_codex_auth(&path)?;
        let tokens = imported
            .tokens
            .ok_or_else(|| anyhow!("Codex auth file does not contain account tokens"))?;
        let auth_mode = imported.auth_mode.unwrap_or_else(|| "chatgpt".to_string());
        if auth_mode != "chatgpt" {
            bail!("Codex auth mode `{auth_mode}` is not supported for OpenAI account import");
        }
        let state = OpenAiStoredState::CodexAccount {
            codex_auth_path: path,
            issuer: issuer.into(),
            client_id: client_id.into(),
            api_base_url: api_base_url.into(),
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            account_id: tokens.account_id,
            last_refresh_at_ms: None,
            last_refresh_outcome: None,
            organization,
            project,
        };
        Ok(AuthSlotRecord {
            slot_id,
            provider: AuthProvider::OpenAi,
            mode: AuthMode::OAuthAccount,
            state: serde_json::to_value(state)?,
            updated_at_ms: now_ms(),
        })
    }

    fn read_codex_auth(path: &Path) -> Result<ImportedCodexAuth> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read Codex auth file {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("failed to parse Codex auth file {}", path.display()))
    }

    fn parse_state(record: &AuthSlotRecord) -> Result<OpenAiStoredState> {
        serde_json::from_value(record.state.clone()).context("invalid stored OpenAI auth state")
    }

    fn write_state(record: &mut AuthSlotRecord, state: OpenAiStoredState) -> Result<()> {
        record.state = serde_json::to_value(state)?;
        record.updated_at_ms = now_ms();
        Ok(())
    }

    fn should_force_refresh(last_refresh_at_ms: Option<u64>, now: u64) -> bool {
        last_refresh_at_ms
            .map(|last| now.saturating_sub(last) >= FORCE_REFRESH_COOLDOWN_MS)
            .unwrap_or(true)
    }

    fn material(
        api_key: &str,
        organization: Option<&str>,
        project: Option<&str>,
    ) -> ResolvedAuthMaterial {
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {api_key}"));
        if let Some(organization) = organization {
            headers.insert("OpenAI-Organization".to_string(), organization.to_string());
        }
        if let Some(project) = project {
            headers.insert("OpenAI-Project".to_string(), project.to_string());
        }
        ResolvedAuthMaterial {
            headers,
            base_url_override: None,
            grant_id: None,
            lease_id: None,
        }
    }

    fn codex_material(
        access_token: &str,
        account_id: Option<&str>,
        api_base_url: &str,
    ) -> ResolvedAuthMaterial {
        let mut headers = BTreeMap::new();
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer {access_token}"),
        );
        if let Some(account_id) = account_id {
            headers.insert("ChatGPT-Account-ID".to_string(), account_id.to_string());
        }
        ResolvedAuthMaterial {
            headers,
            base_url_override: Some(api_base_url.to_string()),
            grant_id: None,
            lease_id: None,
        }
    }

    async fn refresh_codex_tokens(
        &self,
        issuer: &str,
        client_id: &str,
        refresh_token: &str,
    ) -> Result<RefreshResponse> {
        let response = self
            .client
            .post(format!("{issuer}/oauth/token"))
            .header("Content-Type", "application/json")
            .json(&RefreshRequest {
                client_id,
                grant_type: "refresh_token",
                refresh_token,
            })
            .send()
            .await
            .context("failed to refresh OpenAI account token")?;
        if !response.status().is_success() {
            let status = response.status();
            let _ = response.text().await;
            bail!(
                "OpenAI account token refresh failed with status {status}: response body redacted"
            );
        }
        response
            .json::<RefreshResponse>()
            .await
            .context("failed to decode OpenAI refresh response")
    }
}

#[async_trait]
impl AuthBackend for OpenAiAuthBackend {
    fn provider(&self) -> AuthProvider {
        AuthProvider::OpenAi
    }

    fn can_resolve_without_lock(&self, record: &AuthSlotRecord, force_refresh: bool) -> bool {
        if force_refresh {
            return false;
        }
        match Self::parse_state(record) {
            Ok(OpenAiStoredState::ApiKey { .. }) => true,
            Ok(OpenAiStoredState::CodexAccount { access_token, .. }) => access_token.is_some(),
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
            OpenAiStoredState::ApiKey {
                api_key,
                organization,
                project,
            } => Self::material(api_key, organization.as_deref(), project.as_deref()),
            OpenAiStoredState::CodexAccount {
                issuer,
                client_id,
                api_base_url,
                access_token,
                refresh_token,
                last_refresh_at_ms,
                last_refresh_outcome,
                account_id,
                ..
            } => {
                let now = now_ms();
                if (force_refresh && Self::should_force_refresh(*last_refresh_at_ms, now))
                    || access_token.is_none()
                {
                    let refreshed = self
                        .refresh_codex_tokens(issuer, client_id, refresh_token.as_str())
                        .await?;
                    *access_token = Some(refreshed.access_token.ok_or_else(|| {
                        anyhow!(
                            "OpenAI refresh response did not include a replacement access_token"
                        )
                    })?);
                    if let Some(next_refresh) = refreshed.refresh_token {
                        *refresh_token = next_refresh;
                    }
                    *last_refresh_at_ms = Some(now);
                    *last_refresh_outcome = Some("success".to_string());
                }
                Self::codex_material(
                    access_token
                        .as_deref()
                        .ok_or_else(|| anyhow!("missing refreshed OpenAI access token"))?,
                    account_id.as_deref(),
                    api_base_url,
                )
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
            OpenAiStoredState::ApiKey { .. } => "api_key".to_string(),
            OpenAiStoredState::CodexAccount {
                codex_auth_path,
                account_id,
                last_refresh_at_ms,
                last_refresh_outcome,
                ..
            } => {
                details.insert(
                    "source".to_string(),
                    json!(codex_auth_path.display().to_string()),
                );
                if let Some(account_id) = account_id.as_ref() {
                    details.insert("account_id".to_string(), json!(account_id));
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
                match account_id {
                    Some(account_id) => format!(
                        "codex_account:{} ({})",
                        account_id,
                        codex_auth_path.display()
                    ),
                    None => format!("codex_account ({})", codex_auth_path.display()),
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
