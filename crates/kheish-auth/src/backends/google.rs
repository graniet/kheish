use std::collections::BTreeMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::backends::AuthBackend;
use crate::{
    AuthMode, AuthProvider, AuthSlotId, AuthSlotRecord, AuthSlotStatus, ResolvedAuthMaterial,
    now_ms,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum GoogleStoredState {
    ApiKey { api_key: String },
}

/// Resolves Google API-key auth material stored in one daemon-managed auth slot.
pub struct GoogleAuthBackend;

impl GoogleAuthBackend {
    /// Creates one Google auth backend.
    pub fn new() -> Self {
        Self
    }

    /// Builds one static Google API-key slot record.
    pub fn static_api_key_record(
        slot_id: AuthSlotId,
        api_key: impl Into<String>,
    ) -> Result<AuthSlotRecord> {
        let state = GoogleStoredState::ApiKey {
            api_key: api_key.into(),
        };
        Ok(AuthSlotRecord {
            slot_id,
            provider: AuthProvider::Google,
            mode: AuthMode::ApiKey,
            state: serde_json::to_value(state)?,
            updated_at_ms: now_ms(),
        })
    }

    fn parse_state(record: &AuthSlotRecord) -> Result<GoogleStoredState> {
        serde_json::from_value(record.state.clone()).context("invalid stored Google auth state")
    }
}

#[async_trait]
impl AuthBackend for GoogleAuthBackend {
    fn provider(&self) -> AuthProvider {
        AuthProvider::Google
    }

    async fn resolve(
        &self,
        record: &mut AuthSlotRecord,
        _force_refresh: bool,
    ) -> Result<ResolvedAuthMaterial> {
        let GoogleStoredState::ApiKey { api_key } = Self::parse_state(record)?;
        let mut headers = BTreeMap::new();
        headers.insert("x-goog-api-key".to_string(), api_key);
        Ok(ResolvedAuthMaterial {
            headers,
            base_url_override: None,
            grant_id: None,
            lease_id: None,
        })
    }

    fn status(&self, record: &AuthSlotRecord) -> Result<AuthSlotStatus> {
        let _state = Self::parse_state(record)?;
        Ok(AuthSlotStatus {
            slot_id: record.slot_id.clone(),
            provider: record.provider,
            mode: record.mode,
            summary: "api_key".to_string(),
            updated_at_ms: record.updated_at_ms,
            details: Default::default(),
        })
    }
}
