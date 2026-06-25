mod anthropic;
mod generic;
mod google;
mod mcp_oauth;
mod openai;
mod openrouter;
mod xai;

use anyhow::Result;
use async_trait::async_trait;

use crate::{AuthProvider, AuthSlotRecord, AuthSlotStatus, ResolvedAuthMaterial};

pub use anthropic::{
    AnthropicAuthBackend, DEFAULT_ANTHROPIC_OAUTH_TOKEN_URL, DEFAULT_CLAUDE_CODE_CLIENT_ID,
    default_claude_code_credentials_path,
};
pub use generic::GenericAuthBackend;
pub use google::GoogleAuthBackend;
pub use mcp_oauth::{McpOAuthAccountRecordInput, McpOAuthAuthBackend, McpOAuthStoredState};
pub use openai::{
    DEFAULT_CODEX_CLIENT_ID, DEFAULT_OPENAI_AUTH_ISSUER, DEFAULT_OPENAI_CODEX_API_BASE_URL,
    OpenAiAuthBackend,
};
pub use openrouter::OpenRouterAuthBackend;
pub use xai::XAiAuthBackend;

#[async_trait]
pub trait AuthBackend: Send + Sync {
    fn provider(&self) -> AuthProvider;

    fn can_resolve_without_lock(&self, _record: &AuthSlotRecord, _force_refresh: bool) -> bool {
        false
    }

    async fn resolve(
        &self,
        record: &mut AuthSlotRecord,
        force_refresh: bool,
    ) -> Result<ResolvedAuthMaterial>;

    fn status(&self, record: &AuthSlotRecord) -> Result<AuthSlotStatus>;
}
