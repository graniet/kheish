use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AuthSlotId(pub String);

impl AuthSlotId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl Display for AuthSlotId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthProvider {
    Generic,
    #[serde(rename = "mcp_oauth", alias = "mcp_o_auth")]
    McpOAuth,
    OpenAi,
    Anthropic,
    Google,
    OpenRouter,
    XAi,
}

impl Display for AuthProvider {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Generic => "generic",
            Self::McpOAuth => "mcp_oauth",
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
            Self::OpenRouter => "openrouter",
            Self::XAi => "xai",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    OpaqueSecret,
    ApiKey,
    #[serde(rename = "oauth_account", alias = "o_auth_account")]
    OAuthAccount,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthSlotRecord {
    pub slot_id: AuthSlotId,
    pub provider: AuthProvider,
    pub mode: AuthMode,
    #[serde(default)]
    pub state: Value,
    pub updated_at_ms: u64,
}

impl std::fmt::Debug for AuthSlotRecord {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSlotRecord")
            .field("slot_id", &self.slot_id)
            .field("provider", &self.provider)
            .field("mode", &self.mode)
            .field("state", &"<redacted>")
            .field("updated_at_ms", &self.updated_at_ms)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedAuthMaterial {
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub base_url_override: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
}

impl std::fmt::Debug for ResolvedAuthMaterial {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedAuthMaterial")
            .field("headers", &format_args!("{} redacted", self.headers.len()))
            .field("base_url_override", &self.base_url_override)
            .field("grant_id", &self.grant_id)
            .field("lease_id", &self.lease_id)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSlotStatus {
    pub slot_id: AuthSlotId,
    pub provider: AuthProvider,
    pub mode: AuthMode,
    pub summary: String,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, Value>,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AuthStoreSnapshot {
    #[serde(default)]
    pub slots: BTreeMap<String, AuthSlotRecord>,
}

impl std::fmt::Debug for AuthStoreSnapshot {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthStoreSnapshot")
            .field("slots", &format_args!("{} redacted", self.slots.len()))
            .finish()
    }
}
