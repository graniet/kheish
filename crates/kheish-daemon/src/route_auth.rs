//! Request-time credential providers for model routes.
//!
//! A route never holds a provider credential of its own. Every model request
//! resolves its auth material through the daemon's credential broker, under
//! the execution scope of the run that makes it, so credential-scope rules
//! (`route_allow`, `route_deny`), leases and revocation apply per request and
//! a rotated slot takes effect on the next request without a restart.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use kheish_auth::{
    AuthManager, AuthSlotId, CredentialLease, DEFAULT_ROUTE_LEASE_TTL_MS,
    ExecutionCredentialContext, RequestAuthProvider, ResolvedAuthMaterial,
};
use kheish_runtime::current_execution_scope;

#[derive(Clone)]
enum ScopedAuthMaterialSource {
    /// A provider-typed slot in the auth store, resolved on every request.
    Slot(AuthSlotId),
    /// Static material (an inline key), still authorized per request.
    Inline(ResolvedAuthMaterial),
}

/// Resolves a route's auth material through the credential broker under the
/// current run's execution scope.
#[derive(Clone)]
pub struct ScopedRequestAuthProvider {
    manager: Arc<AuthManager>,
    route_id: String,
    source: ScopedAuthMaterialSource,
}

impl ScopedRequestAuthProvider {
    /// Builds a provider that reads `slot_id` from the auth store on every
    /// request, so rotating the slot needs no restart.
    pub fn for_slot(
        manager: Arc<AuthManager>,
        slot_id: AuthSlotId,
        route_id: impl Into<String>,
    ) -> Self {
        Self {
            manager,
            route_id: route_id.into(),
            source: ScopedAuthMaterialSource::Slot(slot_id),
        }
    }

    /// Builds a provider around static material, authorized per request.
    pub fn for_inline(
        manager: Arc<AuthManager>,
        route_id: impl Into<String>,
        material: ResolvedAuthMaterial,
    ) -> Self {
        Self {
            manager,
            route_id: route_id.into(),
            source: ScopedAuthMaterialSource::Inline(material),
        }
    }

    fn context(&self) -> ExecutionCredentialContext {
        current_execution_scope()
            .map(|scope| ExecutionCredentialContext {
                session_id: Some(scope.session_id).filter(|value| !value.is_empty()),
                agent_id: scope.agent_id,
                run_id: scope.run_id,
                principal_id: scope.principal_id,
                parent_principal_id: scope.parent_principal_id,
                delegation_id: scope.delegation_id,
                credential_scope: scope.credential_scope.normalized(),
            })
            .unwrap_or_default()
    }

    fn authorize_inline_route(&self) -> Result<CredentialLease> {
        let synthetic_slot = AuthSlotId::new(format!("inline-route:{}", self.route_id));
        let grant = self.manager.broker().authorize_route(
            &synthetic_slot,
            &self.route_id,
            &self.context(),
        )?;
        self.manager
            .broker()
            .issue_route_lease(&grant, DEFAULT_ROUTE_LEASE_TTL_MS)
    }

    async fn material(&self, force_refresh: bool) -> Result<ResolvedAuthMaterial> {
        match &self.source {
            ScopedAuthMaterialSource::Slot(slot_id) => {
                self.manager
                    .resolve_brokered(slot_id, &self.route_id, &self.context(), force_refresh)
                    .await
            }
            ScopedAuthMaterialSource::Inline(material) => {
                let mut material = material.clone();
                let lease = self.authorize_inline_route()?;
                material.grant_id = Some(lease.grant_id);
                material.lease_id = Some(lease.id);
                Ok(material)
            }
        }
    }
}

#[async_trait]
impl RequestAuthProvider for ScopedRequestAuthProvider {
    async fn resolve(&self) -> Result<ResolvedAuthMaterial> {
        self.material(false).await
    }

    async fn refresh(&self) -> Result<ResolvedAuthMaterial> {
        self.material(true).await
    }

    async fn ensure_active(&self, material: &ResolvedAuthMaterial) -> Result<()> {
        self.manager.ensure_resolved_material_active(material)
    }
}
