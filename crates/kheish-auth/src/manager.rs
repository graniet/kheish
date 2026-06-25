use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, RwLock};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::backends::{
    AnthropicAuthBackend, AuthBackend, GenericAuthBackend, GoogleAuthBackend,
    McpOAuthAccountRecordInput, McpOAuthAuthBackend, McpOAuthStoredState, OpenAiAuthBackend,
    OpenRouterAuthBackend, XAiAuthBackend,
};
use crate::broker::{
    AuthSubjectStatus, CredentialBroker, CredentialLease, CredentialLeaseStatus,
    DEFAULT_CONNECTOR_LEASE_TTL_MS, DEFAULT_MCP_LEASE_TTL_MS, DEFAULT_ROUTE_LEASE_TTL_MS,
    ExecutionCredentialContext,
};
use crate::store::FileAuthStore;
use crate::{
    AuthProvider, AuthSlotId, AuthSlotRecord, AuthSlotStatus, AuthStoreSnapshot,
    ResolvedAuthMaterial,
    redaction::{
        register_auth_record_debug_redaction_tokens,
        replace_auth_store_debug_redaction_tokens_for_records,
    },
};

/// Resolves request credentials for one provider call.
///
/// Implementations used by daemon execution paths must enforce broker authorization before
/// returning material. `ManagedRequestAuthProvider` is intentionally unscoped and is only safe as
/// a root credential adapter wrapped by a broker-aware caller.
#[async_trait]
pub trait RequestAuthProvider: Send + Sync {
    async fn resolve(&self) -> Result<ResolvedAuthMaterial>;
    async fn refresh(&self) -> Result<ResolvedAuthMaterial>;
    async fn ensure_active(&self, material: &ResolvedAuthMaterial) -> Result<()>;
}

#[derive(Clone)]
pub struct ManagedRequestAuthProvider {
    manager: Arc<AuthManager>,
    slot_id: AuthSlotId,
}

impl ManagedRequestAuthProvider {
    pub fn new(manager: Arc<AuthManager>, slot_id: AuthSlotId) -> Self {
        Self { manager, slot_id }
    }
}

#[async_trait]
impl RequestAuthProvider for ManagedRequestAuthProvider {
    async fn resolve(&self) -> Result<ResolvedAuthMaterial> {
        self.manager.resolve(&self.slot_id, false).await
    }

    async fn refresh(&self) -> Result<ResolvedAuthMaterial> {
        self.manager.resolve(&self.slot_id, true).await
    }

    async fn ensure_active(&self, material: &ResolvedAuthMaterial) -> Result<()> {
        self.manager.ensure_resolved_material_active(material)
    }
}

pub struct AuthManager {
    store: FileAuthStore,
    records: RwLock<BTreeMap<String, AuthSlotRecord>>,
    slot_locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
    mutation_lock: Mutex<()>,
    generic: Arc<GenericAuthBackend>,
    mcp_oauth: Arc<McpOAuthAuthBackend>,
    openai: Arc<OpenAiAuthBackend>,
    anthropic: Arc<AnthropicAuthBackend>,
    google: Arc<GoogleAuthBackend>,
    openrouter: Arc<OpenRouterAuthBackend>,
    xai: Arc<XAiAuthBackend>,
    broker: Arc<CredentialBroker>,
}

impl AuthManager {
    pub fn new(store_path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let store = FileAuthStore::new(store_path);
        let snapshot = store.load()?;
        replace_auth_store_debug_redaction_tokens_for_records(snapshot.slots.values());
        let broker = Arc::new(CredentialBroker::new(broker_state_path(store.path()))?);
        Ok(Arc::new(Self {
            store,
            records: RwLock::new(snapshot.slots),
            slot_locks: StdMutex::new(HashMap::new()),
            mutation_lock: Mutex::new(()),
            generic: Arc::new(GenericAuthBackend::new()),
            mcp_oauth: Arc::new(McpOAuthAuthBackend::new()?),
            openai: Arc::new(OpenAiAuthBackend::new()?),
            anthropic: Arc::new(AnthropicAuthBackend::new()?),
            google: Arc::new(GoogleAuthBackend::new()),
            openrouter: Arc::new(OpenRouterAuthBackend::new()),
            xai: Arc::new(XAiAuthBackend::new()),
            broker,
        }))
    }

    /// Returns an unbrokered root credential adapter for callers that add their own broker gate.
    pub fn request_provider(
        self: &Arc<Self>,
        slot_id: impl Into<AuthSlotId>,
    ) -> Arc<dyn RequestAuthProvider> {
        Arc::new(ManagedRequestAuthProvider::new(
            self.clone(),
            slot_id.into(),
        ))
    }

    /// Returns the broker responsible for route authorization, connector leases, and revocation.
    pub fn broker(&self) -> Arc<CredentialBroker> {
        self.broker.clone()
    }

    /// Resolves auth material after enforcing the effective execution credential scope.
    pub async fn resolve_brokered(
        &self,
        slot_id: &AuthSlotId,
        route_id: &str,
        context: &ExecutionCredentialContext,
        force_refresh: bool,
    ) -> Result<ResolvedAuthMaterial> {
        self.ensure_slot_not_revoked(slot_id)?;
        let grant = self.broker.authorize_route(slot_id, route_id, context)?;
        let route_lease = self
            .broker
            .issue_route_lease(&grant, DEFAULT_ROUTE_LEASE_TTL_MS)?;
        let mut material = self.resolve(slot_id, force_refresh).await?;
        material.grant_id = Some(route_lease.grant_id);
        material.lease_id = Some(route_lease.id);
        Ok(material)
    }

    /// Resolves OAuth material for one MCP server after enforcing execution credential scope.
    pub async fn resolve_mcp_brokered(
        &self,
        slot_id: &AuthSlotId,
        server: &str,
        resource: &str,
        scopes: &[String],
        context: &ExecutionCredentialContext,
        force_refresh: bool,
    ) -> Result<ResolvedAuthMaterial> {
        anyhow::ensure!(
            context.session_id.is_some()
                || context.agent_id.is_some()
                || context.principal_id.is_some(),
            "MCP OAuth credentials require a scoped execution context"
        );
        self.ensure_slot_not_revoked(slot_id)?;
        self.validate_mcp_oauth_binding(slot_id, server, resource, scopes)?;
        let grant = self
            .broker
            .authorize_mcp_server(slot_id, server, scopes, context)?;
        let lease = self
            .broker
            .issue_mcp_lease(&grant, DEFAULT_MCP_LEASE_TTL_MS)?;
        let mut material = self.resolve(slot_id, force_refresh).await?;
        material.grant_id = Some(lease.grant_id);
        material.lease_id = Some(lease.id);
        Ok(material)
    }

    pub fn validate_mcp_oauth_binding(
        &self,
        slot_id: &AuthSlotId,
        server: &str,
        resource: &str,
        scopes: &[String],
    ) -> Result<()> {
        let record = self
            .records
            .read()
            .expect("auth manager records rwlock poisoned")
            .get(&slot_id.0)
            .cloned()
            .ok_or_else(|| anyhow!("auth slot `{slot_id}` not found"))?;
        anyhow::ensure!(
            record.provider == AuthProvider::McpOAuth,
            "auth slot `{slot_id}` is not an MCP OAuth account"
        );
        let state = McpOAuthAuthBackend::parse_state(&record)?;
        let McpOAuthStoredState::McpOAuthAccount {
            server_name,
            resource: stored_resource,
            scopes: stored_scopes,
            ..
        } = state;
        anyhow::ensure!(
            server_name == server,
            "MCP OAuth slot `{slot_id}` is bound to server `{server_name}`, not `{server}`"
        );
        anyhow::ensure!(
            stored_resource.trim_end_matches('/') == resource.trim_end_matches('/'),
            "MCP OAuth slot `{slot_id}` is bound to resource `{stored_resource}`, not `{resource}`"
        );
        for scope in scopes
            .iter()
            .map(|scope| scope.trim())
            .filter(|scope| !scope.is_empty())
        {
            anyhow::ensure!(
                stored_scopes.iter().any(|stored| stored == scope),
                "MCP OAuth slot `{slot_id}` does not include requested scope `{scope}`"
            );
        }
        Ok(())
    }

    /// Issues one short-lived connector lease scoped to one connector credential subset.
    pub fn issue_connector_lease(
        &self,
        connector: &str,
        env_keys: &[String],
        secret_refs: &[AuthSlotId],
        ttl_ms: Option<u64>,
        context: Option<&ExecutionCredentialContext>,
    ) -> Result<(String, CredentialLease)> {
        for secret_ref in secret_refs {
            self.ensure_slot_not_revoked(secret_ref)?;
        }
        self.broker.issue_connector_lease(
            connector,
            env_keys,
            secret_refs,
            ttl_ms.unwrap_or(DEFAULT_CONNECTOR_LEASE_TTL_MS),
            context,
        )
    }

    /// Validates one connector lease token and returns the corresponding active lease.
    pub fn validate_connector_lease(
        &self,
        token: &str,
        connector: &str,
        env_key: &str,
    ) -> Result<CredentialLease> {
        self.broker
            .validate_connector_lease(token, connector, env_key)
    }

    /// Validates one connector lease token against the concrete backing secret slot.
    pub fn validate_connector_lease_for_secret_ref(
        &self,
        token: &str,
        connector: &str,
        env_key: &str,
        secret_ref: &AuthSlotId,
    ) -> Result<CredentialLease> {
        self.broker
            .validate_connector_lease_for_secret_ref(token, connector, env_key, secret_ref)
    }

    /// Revokes one subject and every active lease owned by it immediately.
    pub fn revoke_subject_and_leases(&self, subject_id: &str) -> Result<()> {
        self.broker.revoke_subject(subject_id).map(|_| ())
    }

    /// Revokes one issued lease immediately.
    pub fn revoke_lease(&self, lease_id: &str, expires_at_ms: u64) -> Result<()> {
        self.broker.revoke_lease(lease_id, expires_at_ms)
    }

    /// Revokes active route, connector, and MCP leases bound to one daemon-managed auth slot.
    pub fn revoke_slot_leases(&self, slot_id: &AuthSlotId) -> Result<usize> {
        self.broker.revoke_slot_leases(slot_id)
    }

    /// Returns whether one daemon-managed auth slot is currently revoked.
    pub fn is_slot_revoked(&self, slot_id: &AuthSlotId) -> bool {
        self.broker.is_slot_revoked(slot_id)
    }

    /// Returns the current broker status for one execution subject.
    pub fn subject_status(&self, subject_id: &str) -> Option<AuthSubjectStatus> {
        self.broker.subject_status(subject_id)
    }

    /// Returns the current broker status for one issued lease when known.
    pub fn lease_status(&self, lease_id: &str) -> Option<CredentialLeaseStatus> {
        self.broker.lease_status(lease_id)
    }

    /// Verifies one brokered credential lease is still active immediately before use.
    pub fn ensure_resolved_material_active(&self, material: &ResolvedAuthMaterial) -> Result<()> {
        let Some(lease_id) = material.lease_id.as_deref() else {
            return Ok(());
        };
        let status = self
            .broker
            .lease_status(lease_id)
            .ok_or_else(|| anyhow!("credential lease `{lease_id}` was not found"))?;
        anyhow::ensure!(status.active, "credential lease `{lease_id}` is not active");
        Ok(())
    }

    pub async fn store_openai_api_key(
        &self,
        slot_id: impl Into<AuthSlotId>,
        api_key: impl Into<String>,
        organization: Option<String>,
        project: Option<String>,
    ) -> Result<AuthSlotStatus> {
        let record = OpenAiAuthBackend::static_api_key_record(
            slot_id.into(),
            api_key,
            organization,
            project,
        )?;
        self.put_record(record).await
    }

    pub async fn import_openai_codex(
        &self,
        slot_id: impl Into<AuthSlotId>,
        path: impl Into<PathBuf>,
        organization: Option<String>,
        project: Option<String>,
    ) -> Result<AuthSlotStatus> {
        let record =
            OpenAiAuthBackend::import_codex_record(slot_id.into(), path, organization, project)?;
        self.put_record(record).await
    }

    pub async fn store_anthropic_api_key(
        &self,
        slot_id: impl Into<AuthSlotId>,
        api_key: impl Into<String>,
    ) -> Result<AuthSlotStatus> {
        let record = AnthropicAuthBackend::static_api_key_record(slot_id.into(), api_key)?;
        self.put_record(record).await
    }

    pub async fn import_anthropic_claude_code(
        &self,
        slot_id: impl Into<AuthSlotId>,
        path: impl Into<PathBuf>,
    ) -> Result<AuthSlotStatus> {
        let record = AnthropicAuthBackend::import_claude_code_record(slot_id.into(), path)?;
        self.put_record(record).await
    }

    /// Stores one static Google API key under one daemon-managed slot.
    pub async fn store_google_api_key(
        &self,
        slot_id: impl Into<AuthSlotId>,
        api_key: impl Into<String>,
    ) -> Result<AuthSlotStatus> {
        let record = GoogleAuthBackend::static_api_key_record(slot_id.into(), api_key)?;
        self.put_record(record).await
    }

    /// Stores one static OpenRouter API key under one daemon-managed slot.
    pub async fn store_openrouter_api_key(
        &self,
        slot_id: impl Into<AuthSlotId>,
        api_key: impl Into<String>,
    ) -> Result<AuthSlotStatus> {
        let record = OpenRouterAuthBackend::static_api_key_record(slot_id.into(), api_key)?;
        self.put_record(record).await
    }

    /// Stores one static xAI API key under one daemon-managed slot.
    pub async fn store_xai_api_key(
        &self,
        slot_id: impl Into<AuthSlotId>,
        api_key: impl Into<String>,
    ) -> Result<AuthSlotStatus> {
        let record = XAiAuthBackend::static_api_key_record(slot_id.into(), api_key)?;
        self.put_record(record).await
    }

    /// Stores one opaque daemon-managed secret under one slot.
    pub async fn store_generic_secret(
        &self,
        slot_id: impl Into<AuthSlotId>,
        value: impl Into<String>,
    ) -> Result<AuthSlotStatus> {
        let record = GenericAuthBackend::static_secret_record(slot_id.into(), value)?;
        self.put_record(record).await
    }

    /// Stores one OAuth 2.1 account record for a protected HTTP MCP server.
    pub async fn store_mcp_oauth_account(
        &self,
        input: McpOAuthAccountRecordInput,
    ) -> Result<AuthSlotStatus> {
        let record = McpOAuthAuthBackend::account_record(input)?;
        self.put_record(record).await
    }

    pub async fn put_record(&self, record: AuthSlotRecord) -> Result<AuthSlotStatus> {
        let slot_lock = self.slot_lock(&record.slot_id);
        let _slot_guard = slot_lock.lock().await;
        let _mutation_guard = self.mutation_lock.lock().await;
        let mut next = self.snapshot_records();
        let replaced_existing_slot = next.contains_key(&record.slot_id.0);
        if let Some(previous) = next.get(&record.slot_id.0) {
            register_auth_record_debug_redaction_tokens(previous);
        }
        if replaced_existing_slot {
            self.revoke_slot_leases(&record.slot_id)?;
        }
        next.insert(record.slot_id.0.clone(), record.clone());
        if let Err(error) = self.persist_records(next.clone()).await {
            if replaced_existing_slot {
                let _ = self.broker.unrevoke_slot(&record.slot_id);
            }
            return Err(error);
        }
        *self
            .records
            .write()
            .expect("auth manager records rwlock poisoned") = next;
        self.refresh_debug_redaction_tokens();
        self.broker.unrevoke_slot(&record.slot_id)?;
        self.status(&record.slot_id)
            .await?
            .ok_or_else(|| anyhow!("failed to load auth slot status after save"))
    }

    pub async fn has_slot(&self, slot_id: &AuthSlotId) -> bool {
        self.records
            .read()
            .expect("auth manager records rwlock poisoned")
            .contains_key(&slot_id.0)
    }

    pub async fn status(&self, slot_id: &AuthSlotId) -> Result<Option<AuthSlotStatus>> {
        let record = self
            .records
            .read()
            .expect("auth manager records rwlock poisoned")
            .get(&slot_id.0)
            .cloned();
        record
            .map(|record| self.backend(record.provider)?.status(&record))
            .transpose()
    }

    /// Resolves one opaque secret value from the daemon-managed store.
    pub fn secret_value(&self, slot_id: &AuthSlotId) -> Result<Option<String>> {
        self.ensure_slot_not_revoked(slot_id)?;
        let record = self
            .records
            .read()
            .expect("auth manager records rwlock poisoned")
            .get(&slot_id.0)
            .cloned();
        match record {
            Some(record) if record.provider == AuthProvider::Generic => {
                Ok(Some(GenericAuthBackend::secret_value(&record)?))
            }
            Some(_) => Err(anyhow!("auth slot `{slot_id}` is not a generic secret")),
            None => Ok(None),
        }
    }

    /// Lists the current status of every stored auth slot in stable slot-id order.
    pub async fn list_statuses(&self) -> Result<Vec<AuthSlotStatus>> {
        let records = self
            .records
            .read()
            .expect("auth manager records rwlock poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut statuses = records
            .into_iter()
            .map(|record| self.backend(record.provider)?.status(&record))
            .collect::<Result<Vec<_>>>()?;
        statuses.sort_by(|left, right| left.slot_id.cmp(&right.slot_id));
        Ok(statuses)
    }

    pub async fn delete(&self, slot_id: &AuthSlotId) -> Result<bool> {
        let slot_lock = self.slot_lock(slot_id);
        let _slot_guard = slot_lock.lock().await;
        let _mutation_guard = self.mutation_lock.lock().await;
        let mut next = self.snapshot_records();
        if let Some(previous) = next.get(&slot_id.0) {
            register_auth_record_debug_redaction_tokens(previous);
        }
        let removed = next.remove(&slot_id.0).is_some();
        if removed {
            self.persist_records(next.clone()).await?;
            *self
                .records
                .write()
                .expect("auth manager records rwlock poisoned") = next;
            self.refresh_debug_redaction_tokens();
            self.revoke_slot_leases(slot_id)?;
        }
        Ok(removed)
    }

    pub async fn resolve(
        &self,
        slot_id: &AuthSlotId,
        force_refresh: bool,
    ) -> Result<ResolvedAuthMaterial> {
        self.ensure_slot_not_revoked(slot_id)?;
        let initial = self
            .records
            .read()
            .expect("auth manager records rwlock poisoned")
            .get(&slot_id.0)
            .cloned()
            .ok_or_else(|| anyhow!("auth slot `{slot_id}` not found"))?;
        let backend = self.backend(initial.provider)?;
        if backend.can_resolve_without_lock(&initial, force_refresh) {
            let mut optimistic = initial.clone();
            let material = backend.resolve(&mut optimistic, force_refresh).await?;
            if optimistic == initial {
                self.ensure_slot_not_revoked(slot_id)?;
                return Ok(material);
            }
        }

        let lock = self.slot_lock(slot_id);
        let _guard = lock.lock().await;
        let mut record = self
            .records
            .read()
            .expect("auth manager records rwlock poisoned")
            .get(&slot_id.0)
            .cloned()
            .ok_or_else(|| anyhow!("auth slot `{slot_id}` not found"))?;
        let original = record.clone();
        let backend = self.backend(record.provider)?;
        let material = backend.resolve(&mut record, force_refresh).await?;
        self.ensure_slot_not_revoked(slot_id)?;
        if record != original {
            register_auth_record_debug_redaction_tokens(&original);
            let _mutation_guard = self.mutation_lock.lock().await;
            self.ensure_slot_not_revoked(slot_id)?;
            let mut next = self.snapshot_records();
            next.insert(slot_id.0.clone(), record);
            self.persist_records(next.clone()).await?;
            *self
                .records
                .write()
                .expect("auth manager records rwlock poisoned") = next;
            self.refresh_debug_redaction_tokens();
        }
        Ok(material)
    }

    /// Forces one slot through its backend refresh path and returns a redacted status.
    pub async fn refresh_status(&self, slot_id: &AuthSlotId) -> Result<AuthSlotStatus> {
        let _ = self.resolve(slot_id, true).await?;
        self.status(slot_id)
            .await?
            .ok_or_else(|| anyhow!("auth slot `{slot_id}` not found after refresh"))
    }

    pub fn store_path(&self) -> &Path {
        self.store.path()
    }

    fn backend(&self, provider: AuthProvider) -> Result<Arc<dyn AuthBackend>> {
        match provider {
            AuthProvider::Generic => Ok(self.generic.clone()),
            AuthProvider::McpOAuth => Ok(self.mcp_oauth.clone()),
            AuthProvider::OpenAi => Ok(self.openai.clone()),
            AuthProvider::Anthropic => Ok(self.anthropic.clone()),
            AuthProvider::Google => Ok(self.google.clone()),
            AuthProvider::OpenRouter => Ok(self.openrouter.clone()),
            AuthProvider::XAi => Ok(self.xai.clone()),
        }
    }

    fn slot_lock(&self, slot_id: &AuthSlotId) -> Arc<Mutex<()>> {
        let mut locks = self
            .slot_locks
            .lock()
            .expect("auth manager slot lock mutex poisoned");
        locks
            .entry(slot_id.0.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn snapshot_records(&self) -> BTreeMap<String, AuthSlotRecord> {
        self.records
            .read()
            .expect("auth manager records rwlock poisoned")
            .clone()
    }

    fn refresh_debug_redaction_tokens(&self) {
        let records = self
            .records
            .read()
            .expect("auth manager records rwlock poisoned");
        replace_auth_store_debug_redaction_tokens_for_records(records.values());
    }

    async fn persist_records(&self, records: BTreeMap<String, AuthSlotRecord>) -> Result<()> {
        let snapshot = AuthStoreSnapshot { slots: records };
        self.store.save(&snapshot).with_context(|| {
            format!(
                "failed to persist auth store {}",
                self.store.path().display()
            )
        })
    }

    fn ensure_slot_not_revoked(&self, slot_id: &AuthSlotId) -> Result<()> {
        anyhow::ensure!(
            !self.broker.is_slot_revoked(slot_id),
            "auth slot `{slot_id}` has been revoked"
        );
        Ok(())
    }
}

fn broker_state_path(store_path: &Path) -> PathBuf {
    let parent = store_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let file_name = store_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| format!("{stem}.broker-state.json"))
        .unwrap_or_else(|| "auth-store.broker-state.json".to_string());
    parent.join(file_name)
}
