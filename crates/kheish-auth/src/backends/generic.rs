use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::backends::AuthBackend;
use crate::{
    AuthMode, AuthProvider, AuthSlotId, AuthSlotRecord, AuthSlotStatus, ResolvedAuthMaterial,
    now_ms,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GenericStoredState {
    value: String,
}

/// Stores and resolves daemon-managed opaque secrets.
pub struct GenericAuthBackend;

impl GenericAuthBackend {
    /// Creates a new generic secret backend.
    pub fn new() -> Self {
        Self
    }

    /// Builds one static opaque secret record.
    pub fn static_secret_record(
        slot_id: AuthSlotId,
        value: impl Into<String>,
    ) -> Result<AuthSlotRecord> {
        let state = GenericStoredState {
            value: value.into(),
        };
        Ok(AuthSlotRecord {
            slot_id,
            provider: AuthProvider::Generic,
            mode: AuthMode::OpaqueSecret,
            state: serde_json::to_value(state)?,
            updated_at_ms: now_ms(),
        })
    }

    /// Extracts the stored secret value from one generic auth slot.
    pub fn secret_value(record: &AuthSlotRecord) -> Result<String> {
        let state = Self::parse_state(record)?;
        Ok(state.value)
    }

    fn parse_state(record: &AuthSlotRecord) -> Result<GenericStoredState> {
        serde_json::from_value(record.state.clone()).context("invalid stored generic secret state")
    }
}

#[async_trait]
impl AuthBackend for GenericAuthBackend {
    fn provider(&self) -> AuthProvider {
        AuthProvider::Generic
    }

    fn can_resolve_without_lock(&self, _record: &AuthSlotRecord, _force_refresh: bool) -> bool {
        true
    }

    async fn resolve(
        &self,
        record: &mut AuthSlotRecord,
        _force_refresh: bool,
    ) -> Result<ResolvedAuthMaterial> {
        let _ = Self::parse_state(record)?;
        Ok(ResolvedAuthMaterial {
            headers: std::collections::BTreeMap::new(),
            base_url_override: None,
            grant_id: None,
            lease_id: None,
        })
    }

    fn status(&self, record: &AuthSlotRecord) -> Result<AuthSlotStatus> {
        let _ = Self::parse_state(record)?;
        Ok(AuthSlotStatus {
            slot_id: record.slot_id.clone(),
            provider: record.provider,
            mode: record.mode,
            summary: "opaque_secret".to_string(),
            updated_at_ms: record.updated_at_ms,
            details: Default::default(),
        })
    }
}
