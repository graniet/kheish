use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use kheish_types::CredentialScope;
use parking_lot::RwLock;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::redaction::register_ephemeral_debug_redaction_token;
use crate::{AuthSlotId, now_ms};

/// Default lease lifetime for connector child-process credentials.
pub const DEFAULT_CONNECTOR_LEASE_TTL_MS: u64 = 5 * 60 * 1_000;
/// Default lease lifetime for provider route credential resolution.
pub const DEFAULT_ROUTE_LEASE_TTL_MS: u64 = 60_000;
/// Default lease lifetime for MCP OAuth credential resolution.
pub const DEFAULT_MCP_LEASE_TTL_MS: u64 = 60_000;

/// The principal family associated with one brokered execution subject.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthSubjectKind {
    /// One daemon-scoped internal subject.
    Daemon,
    /// One session-scoped subject.
    Session,
    /// One agent-scoped subject.
    Agent,
    /// One connector-sidecar subject.
    Connector,
}

/// The identity associated with one brokered authorization request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionCredentialContext {
    /// The session identifier when the request runs inside one session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The agent identifier when the request runs inside one agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// The daemon run identifier when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The stable principal identifier propagated through the runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    /// The parent principal when the request was delegated explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_principal_id: Option<String>,
    /// The stable delegation identifier when the request was spawned explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_id: Option<String>,
    /// The effective credential scope enforced for the request.
    #[serde(default, skip_serializing_if = "CredentialScope::is_empty")]
    pub credential_scope: CredentialScope,
}

/// One first-class subject recognized by the credential broker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSubject {
    /// The stable subject identifier.
    pub subject_id: String,
    /// The principal family.
    pub kind: AuthSubjectKind,
    /// The session identifier when one session owns the subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The agent identifier when one agent owns the subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// The daemon run identifier when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The stable principal identifier propagated through the runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    /// The parent principal when the request was delegated explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_principal_id: Option<String>,
    /// The stable delegation identifier when the subject was created from delegated work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_id: Option<String>,
}

/// One supported lease audience issued by the credential broker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialLeaseAudience {
    /// One model-provider route bound to one daemon-managed auth slot.
    Route {
        route_id: String,
        slot_id: AuthSlotId,
    },
    /// One external connector child process bound to a fixed connector credential subset.
    Connector {
        connector: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env_keys: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        secret_refs: Vec<AuthSlotId>,
    },
    /// One HTTP MCP server bound to one daemon-managed OAuth slot and scope fingerprint.
    McpServer {
        server: String,
        slot_id: AuthSlotId,
        scopes_hash: String,
    },
}

/// One derived authorization grant computed from an execution identity and one audience.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialGrant {
    /// The stable grant identifier.
    pub id: String,
    /// The broker-recognized subject.
    pub subject: AuthSubject,
    /// The effective credential scope applied to the grant.
    #[serde(default, skip_serializing_if = "CredentialScope::is_empty")]
    pub scope: CredentialScope,
    /// The audience authorized by the grant.
    pub audience: CredentialLeaseAudience,
    /// The issuance timestamp.
    pub issued_at_ms: u64,
}

/// One short-lived opaque lease issued by the credential broker.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialLease {
    /// The stable lease identifier.
    pub id: String,
    /// The grant from which the lease was derived.
    pub grant_id: String,
    /// The owning subject identifier.
    pub subject_id: String,
    /// The subject epoch captured when the lease was issued.
    pub subject_epoch: u64,
    /// The opaque token digest retained by the broker.
    #[serde(default)]
    pub token_digest: String,
    /// The audience authorized by the lease.
    pub audience: CredentialLeaseAudience,
    /// The issuance timestamp.
    pub issued_at_ms: u64,
    /// The expiration timestamp.
    pub expires_at_ms: u64,
}

impl fmt::Debug for CredentialLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialLease")
            .field("id", &self.id)
            .field("grant_id", &self.grant_id)
            .field("subject_id", &self.subject_id)
            .field("subject_epoch", &self.subject_epoch)
            .field("token_digest", &"<redacted>")
            .field("audience", &self.audience)
            .field("issued_at_ms", &self.issued_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// One operator-facing subject status returned by the broker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSubjectStatus {
    /// The stable subject identifier.
    pub subject_id: String,
    /// The current revocation epoch.
    pub current_epoch: u64,
    /// Whether the subject has been revoked for future auth resolution.
    pub revoked: bool,
    /// The currently active connector lease identifiers owned by the subject.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_connector_lease_ids: Vec<String>,
    /// The currently active provider route lease identifiers owned by the subject.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_route_lease_ids: Vec<String>,
    /// The currently active MCP credential lease identifiers owned by the subject.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_mcp_lease_ids: Vec<String>,
}

/// One operator-facing lease status returned by the broker.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct CredentialLeaseStatus {
    /// The stored lease payload.
    pub lease: CredentialLease,
    /// Whether the lease is currently revoked.
    pub revoked: bool,
    /// Whether the lease is currently active.
    pub active: bool,
}

impl fmt::Debug for CredentialLeaseStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialLeaseStatus")
            .field("lease", &self.lease)
            .field("revoked", &self.revoked)
            .field("active", &self.active)
            .finish()
    }
}

impl Serialize for CredentialLeaseStatus {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct CredentialLeasePublicView<'a> {
            id: &'a str,
            grant_id: &'a str,
            subject_id: &'a str,
            subject_epoch: u64,
            audience: &'a CredentialLeaseAudience,
            issued_at_ms: u64,
            expires_at_ms: u64,
        }

        #[derive(Serialize)]
        struct CredentialLeaseStatusPublicView<'a> {
            lease: CredentialLeasePublicView<'a>,
            revoked: bool,
            active: bool,
        }

        CredentialLeaseStatusPublicView {
            lease: CredentialLeasePublicView {
                id: &self.lease.id,
                grant_id: &self.lease.grant_id,
                subject_id: &self.lease.subject_id,
                subject_epoch: self.lease.subject_epoch,
                audience: &self.lease.audience,
                issued_at_ms: self.lease.issued_at_ms,
                expires_at_ms: self.lease.expires_at_ms,
            },
            revoked: self.revoked,
            active: self.active,
        }
        .serialize(serializer)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct RevocationState {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    issued_grants: BTreeMap<String, CredentialGrant>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    issued_route_leases: BTreeMap<String, CredentialLease>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    subject_epochs: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    revoked_subjects: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    revoked_leases: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    revoked_slots: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    issued_connector_leases: BTreeMap<String, CredentialLease>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    issued_mcp_leases: BTreeMap<String, CredentialLease>,
}

/// Durable revocation registry and active lease broker.
pub struct CredentialBroker {
    state_path: PathBuf,
    state: RwLock<RevocationState>,
    connector_leases: RwLock<BTreeMap<String, CredentialLease>>,
}

impl CredentialBroker {
    /// Opens or initializes one broker state file.
    pub fn new(state_path: impl Into<PathBuf>) -> Result<Self> {
        let state_path = state_path.into();
        let mut state = load_state(&state_path)?;
        prune_state(&mut state, now_ms());
        persist_state(&state_path, &state)?;
        let connector_leases = state
            .issued_connector_leases
            .values()
            .cloned()
            .map(|lease| (lease.token_digest.clone(), lease))
            .collect();
        Ok(Self {
            state_path,
            state: RwLock::new(state),
            connector_leases: RwLock::new(connector_leases),
        })
    }

    /// Returns the durable state path used by the broker.
    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    /// Resolves one route-scoped grant from one execution context.
    pub fn authorize_route(
        &self,
        slot_id: &AuthSlotId,
        route_id: &str,
        context: &ExecutionCredentialContext,
    ) -> Result<CredentialGrant> {
        let subject = AuthSubject::from_context(context, None);
        let scope = context.credential_scope.normalized();
        if !scope.is_empty() && !scope.allows_route(route_id) {
            bail!("credential scope blocks route {route_id}");
        }
        let mut state = self.state.write();
        prune_state(&mut state, now_ms());
        let subject_epoch = remember_subject_locked(&mut state, &subject.subject_id);
        ensure_subject_not_revoked_locked(&state, &subject.subject_id, subject_epoch)?;
        let grant = CredentialGrant {
            id: stable_id(&(
                "route",
                &subject,
                &slot_id.0,
                route_id,
                &scope,
                subject_epoch,
            )),
            subject,
            scope,
            audience: CredentialLeaseAudience::Route {
                route_id: route_id.to_string(),
                slot_id: slot_id.clone(),
            },
            issued_at_ms: now_ms(),
        };
        ensure_audience_slots_not_revoked_locked(&state, &grant.audience)?;
        state.issued_grants.insert(grant.id.clone(), grant.clone());
        self.persist_state(&state)?;
        Ok(grant)
    }

    /// Issues one short-lived route lease for an already-authorized provider grant.
    pub fn issue_route_lease(
        &self,
        grant: &CredentialGrant,
        ttl_ms: u64,
    ) -> Result<CredentialLease> {
        match &grant.audience {
            CredentialLeaseAudience::Route { .. } => {}
            CredentialLeaseAudience::Connector { .. }
            | CredentialLeaseAudience::McpServer { .. } => {
                bail!("route credential lease expected a route grant");
            }
        }
        let issued_at_ms = now_ms();
        let expires_at_ms = issued_at_ms.saturating_add(ttl_ms.max(1_000));
        let mut token_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut token_bytes);
        let token_digest = digest_bytes(&token_bytes);
        let mut state = self.state.write();
        prune_state(&mut state, now_ms());
        let subject_epoch = remember_subject_locked(&mut state, &grant.subject.subject_id);
        ensure_subject_not_revoked_locked(&state, &grant.subject.subject_id, subject_epoch)?;
        ensure_audience_slots_not_revoked_locked(&state, &grant.audience)?;
        let lease = CredentialLease {
            id: stable_id(&(
                "route-lease",
                &grant.id,
                &token_digest,
                issued_at_ms,
                expires_at_ms,
            )),
            grant_id: grant.id.clone(),
            subject_id: grant.subject.subject_id.clone(),
            subject_epoch,
            token_digest,
            audience: grant.audience.clone(),
            issued_at_ms,
            expires_at_ms,
        };
        state.issued_grants.insert(grant.id.clone(), grant.clone());
        state
            .issued_route_leases
            .insert(lease.id.clone(), lease.clone());
        self.persist_state(&state)?;
        Ok(lease)
    }

    /// Issues one short-lived connector lease for a child-process sidecar.
    pub fn issue_connector_lease(
        &self,
        connector: &str,
        env_keys: &[String],
        secret_refs: &[AuthSlotId],
        ttl_ms: u64,
        context: Option<&ExecutionCredentialContext>,
    ) -> Result<(String, CredentialLease)> {
        let subject = AuthSubject::from_context(
            context.unwrap_or(&ExecutionCredentialContext::default()),
            Some(connector),
        );
        let scope = context
            .map(|context| context.credential_scope.normalized())
            .unwrap_or_else(|| connector_scope(connector, env_keys));
        if !scope.is_empty() && !scope.allows_connector(connector) {
            bail!("credential scope blocks connector {connector}");
        }
        let normalized_env_keys = normalize_entries(env_keys);
        let normalized_secret_refs = normalize_slot_refs(secret_refs);
        for env_key in &normalized_env_keys {
            if !scope.is_empty() && !scope.allows_connector_credential(connector, env_key) {
                bail!("credential scope blocks connector credential {connector}:{env_key}");
            }
        }
        let issued_at_ms = now_ms();
        let expires_at_ms = issued_at_ms.saturating_add(ttl_ms.max(1_000));
        let mut token_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut token_bytes);
        let token = URL_SAFE_NO_PAD.encode(token_bytes);
        let token_digest = digest_str(&token);
        let mut state = self.state.write();
        prune_state(&mut state, now_ms());
        let subject_epoch = remember_subject_locked(&mut state, &subject.subject_id);
        ensure_subject_not_revoked_locked(&state, &subject.subject_id, subject_epoch)?;
        let grant = CredentialGrant {
            id: stable_id(&(
                "connector",
                &subject,
                connector,
                &normalized_env_keys,
                &normalized_secret_refs,
                &scope,
                subject_epoch,
            )),
            subject: subject.clone(),
            scope,
            audience: CredentialLeaseAudience::Connector {
                connector: connector.to_string(),
                env_keys: normalized_env_keys.clone(),
                secret_refs: normalized_secret_refs.clone(),
            },
            issued_at_ms,
        };
        ensure_audience_slots_not_revoked_locked(&state, &grant.audience)?;
        let lease = CredentialLease {
            id: stable_id(&(
                "lease",
                &grant.id,
                &token_digest,
                issued_at_ms,
                expires_at_ms,
            )),
            grant_id: grant.id.clone(),
            subject_id: subject.subject_id,
            subject_epoch,
            token_digest: token_digest.clone(),
            audience: CredentialLeaseAudience::Connector {
                connector: connector.to_string(),
                env_keys: normalized_env_keys,
                secret_refs: normalized_secret_refs,
            },
            issued_at_ms,
            expires_at_ms,
        };
        state.issued_grants.insert(grant.id.clone(), grant.clone());
        state
            .issued_connector_leases
            .insert(lease.id.clone(), lease.clone());
        self.persist_state(&state)?;
        drop(state);
        let mut leases = self.connector_leases.write();
        prune_connector_leases(&mut leases, now_ms());
        leases.insert(token_digest, lease.clone());
        register_ephemeral_debug_redaction_token(token.clone());
        Ok((token, lease))
    }

    /// Validates one connector lease token and env-key access.
    pub fn validate_connector_lease(
        &self,
        token: &str,
        connector: &str,
        env_key: &str,
    ) -> Result<CredentialLease> {
        self.validate_connector_lease_scoped(token, connector, env_key, None)
    }

    /// Validates one connector lease token, env-key access, and backing secret slot.
    pub fn validate_connector_lease_for_secret_ref(
        &self,
        token: &str,
        connector: &str,
        env_key: &str,
        secret_ref: &AuthSlotId,
    ) -> Result<CredentialLease> {
        self.validate_connector_lease_scoped(token, connector, env_key, Some(secret_ref))
    }

    fn validate_connector_lease_scoped(
        &self,
        token: &str,
        connector: &str,
        env_key: &str,
        secret_ref: Option<&AuthSlotId>,
    ) -> Result<CredentialLease> {
        let token_digest = digest_str(token);
        let now = now_ms();
        let lease = {
            let mut leases = self.connector_leases.write();
            prune_connector_leases(&mut leases, now);
            leases
                .get(&token_digest)
                .cloned()
                .ok_or_else(|| anyhow!("unknown connector credential lease"))?
        };
        self.ensure_lease_active(&lease, now, "connector credential lease")?;
        anyhow::ensure!(
            lease.expires_at_ms >= now,
            "connector credential lease has expired"
        );
        match &lease.audience {
            CredentialLeaseAudience::Connector {
                connector: allowed_connector,
                env_keys,
                secret_refs,
            } => {
                anyhow::ensure!(
                    allowed_connector == connector,
                    "connector credential lease targets {allowed_connector}"
                );
                anyhow::ensure!(
                    env_keys.iter().any(|candidate| candidate == env_key),
                    "connector credential lease does not expose {connector}:{env_key}"
                );
                if let Some(secret_ref) = secret_ref {
                    anyhow::ensure!(
                        secret_refs.iter().any(|candidate| candidate == secret_ref),
                        "connector credential lease does not expose {connector}:{env_key} from secret {secret_ref}"
                    );
                }
            }
            CredentialLeaseAudience::Route { .. } => {
                bail!("connector credential lease expected a connector audience");
            }
            CredentialLeaseAudience::McpServer { .. } => {
                bail!("connector credential lease expected a connector audience");
            }
        }
        Ok(lease)
    }

    /// Resolves one MCP-server-scoped grant from one execution context.
    pub fn authorize_mcp_server(
        &self,
        slot_id: &AuthSlotId,
        server: &str,
        scopes: &[String],
        context: &ExecutionCredentialContext,
    ) -> Result<CredentialGrant> {
        let subject = AuthSubject::from_context(context, None);
        let scope = context.credential_scope.normalized();
        if !scope.is_empty() && !scope.allows_mcp_server(server) {
            bail!("credential scope blocks MCP server {server}");
        }
        let normalized_scopes = normalize_entries(scopes);
        let scopes_hash = stable_id(&normalized_scopes);
        let mut state = self.state.write();
        prune_state(&mut state, now_ms());
        let subject_epoch = remember_subject_locked(&mut state, &subject.subject_id);
        ensure_subject_not_revoked_locked(&state, &subject.subject_id, subject_epoch)?;
        let grant = CredentialGrant {
            id: stable_id(&(
                "mcp-server",
                &subject,
                &slot_id.0,
                server,
                &scopes_hash,
                &scope,
                subject_epoch,
            )),
            subject,
            scope,
            audience: CredentialLeaseAudience::McpServer {
                server: server.to_string(),
                slot_id: slot_id.clone(),
                scopes_hash,
            },
            issued_at_ms: now_ms(),
        };
        ensure_audience_slots_not_revoked_locked(&state, &grant.audience)?;
        state.issued_grants.insert(grant.id.clone(), grant.clone());
        self.persist_state(&state)?;
        Ok(grant)
    }

    /// Issues one short-lived MCP lease for an already-authorized MCP grant.
    pub fn issue_mcp_lease(&self, grant: &CredentialGrant, ttl_ms: u64) -> Result<CredentialLease> {
        match &grant.audience {
            CredentialLeaseAudience::McpServer { .. } => {}
            CredentialLeaseAudience::Route { .. } | CredentialLeaseAudience::Connector { .. } => {
                bail!("MCP credential lease expected an MCP server grant");
            }
        }
        let issued_at_ms = now_ms();
        let expires_at_ms = issued_at_ms.saturating_add(ttl_ms.max(1_000));
        let mut token_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut token_bytes);
        let token_digest = digest_bytes(&token_bytes);
        let mut state = self.state.write();
        prune_state(&mut state, now_ms());
        let subject_epoch = remember_subject_locked(&mut state, &grant.subject.subject_id);
        ensure_subject_not_revoked_locked(&state, &grant.subject.subject_id, subject_epoch)?;
        ensure_audience_slots_not_revoked_locked(&state, &grant.audience)?;
        let lease = CredentialLease {
            id: stable_id(&(
                "mcp-lease",
                &grant.id,
                &token_digest,
                issued_at_ms,
                expires_at_ms,
            )),
            grant_id: grant.id.clone(),
            subject_id: grant.subject.subject_id.clone(),
            subject_epoch,
            token_digest,
            audience: grant.audience.clone(),
            issued_at_ms,
            expires_at_ms,
        };
        state.issued_grants.insert(grant.id.clone(), grant.clone());
        state
            .issued_mcp_leases
            .insert(lease.id.clone(), lease.clone());
        self.persist_state(&state)?;
        Ok(lease)
    }

    /// Revokes the subject for all future auth resolution and active leases.
    pub fn revoke_subject(&self, subject_id: &str) -> Result<AuthSubjectStatus> {
        let mut state = self.state.write();
        let now = now_ms();
        prune_state(&mut state, now);
        anyhow::ensure!(
            known_subject_locked(&state, subject_id),
            "credential subject `{subject_id}` was not found"
        );
        let next = state
            .subject_epochs
            .get(subject_id)
            .copied()
            .unwrap_or_default()
            .saturating_add(1);
        state.subject_epochs.insert(subject_id.to_string(), next);
        state.revoked_subjects.insert(subject_id.to_string(), now);
        let lease_revocations = state
            .issued_connector_leases
            .values()
            .chain(state.issued_route_leases.values())
            .chain(state.issued_mcp_leases.values())
            .filter(|lease| lease.subject_id == subject_id && lease.expires_at_ms >= now)
            .map(|lease| (lease.id.clone(), lease.expires_at_ms.max(now)))
            .collect::<Vec<_>>();
        for (lease_id, expires_at_ms) in lease_revocations {
            state.revoked_leases.insert(lease_id, expires_at_ms);
        }
        self.persist_state(&state)?;
        drop(state);
        self.subject_status(subject_id)
            .ok_or_else(|| anyhow!("credential subject `{subject_id}` was not found"))
    }

    /// Returns one operator-facing subject status.
    pub fn subject_status(&self, subject_id: &str) -> Option<AuthSubjectStatus> {
        let now = now_ms();
        let state = self.state.read();
        if !known_subject_locked(&state, subject_id) {
            return None;
        }
        let current_epoch = state
            .subject_epochs
            .get(subject_id)
            .copied()
            .unwrap_or_default();
        let revoked = state.revoked_subjects.contains_key(subject_id);
        let mut active_connector_lease_ids = state
            .issued_connector_leases
            .values()
            .filter(|lease| {
                lease.subject_id == subject_id
                    && lease.expires_at_ms >= now
                    && !revoked
                    && !lease_is_revoked_locked(&state, lease, now)
                    && current_epoch <= lease.subject_epoch
            })
            .map(|lease| lease.id.clone())
            .collect::<Vec<_>>();
        active_connector_lease_ids.sort();
        let active_lease = |lease: &CredentialLease| {
            lease.subject_id == subject_id
                && lease.expires_at_ms >= now
                && !revoked
                && !lease_is_revoked_locked(&state, lease, now)
                && current_epoch <= lease.subject_epoch
        };
        let mut active_route_lease_ids = state
            .issued_route_leases
            .values()
            .filter(|lease| active_lease(lease))
            .map(|lease| lease.id.clone())
            .collect::<Vec<_>>();
        active_route_lease_ids.sort();
        let mut active_mcp_lease_ids = state
            .issued_mcp_leases
            .values()
            .filter(|lease| active_lease(lease))
            .map(|lease| lease.id.clone())
            .collect::<Vec<_>>();
        active_mcp_lease_ids.sort();
        Some(AuthSubjectStatus {
            subject_id: subject_id.to_string(),
            current_epoch,
            revoked,
            active_connector_lease_ids,
            active_route_lease_ids,
            active_mcp_lease_ids,
        })
    }

    /// Revokes one concrete lease identifier until the provided expiry timestamp.
    pub fn revoke_lease(&self, lease_id: &str, expires_at_ms: u64) -> Result<()> {
        let mut state = self.state.write();
        let now = now_ms();
        prune_state(&mut state, now);
        state
            .revoked_leases
            .insert(lease_id.to_string(), expires_at_ms.max(now));
        self.persist_state(&state)
    }

    /// Revokes active route, connector, and MCP leases bound to one daemon-managed auth slot.
    pub fn revoke_slot_leases(&self, slot_id: &AuthSlotId) -> Result<usize> {
        let mut state = self.state.write();
        let now = now_ms();
        prune_state(&mut state, now);
        let lease_revocations = state
            .issued_route_leases
            .values()
            .chain(state.issued_connector_leases.values())
            .chain(state.issued_mcp_leases.values())
            .filter(|lease| {
                lease.expires_at_ms >= now
                    && lease_targets_slot(lease, slot_id)
                    && !state
                        .revoked_leases
                        .get(&lease.id)
                        .copied()
                        .is_some_and(|expires_at_ms| expires_at_ms >= now)
            })
            .map(|lease| (lease.id.clone(), lease.expires_at_ms.max(now)))
            .collect::<Vec<_>>();
        let revoked_count = lease_revocations.len();
        for (lease_id, expires_at_ms) in lease_revocations {
            state.revoked_leases.insert(lease_id, expires_at_ms);
        }
        state.revoked_slots.insert(slot_id.0.clone(), now);
        self.persist_state(&state)?;
        Ok(revoked_count)
    }

    /// Re-enables one daemon-managed auth slot after a fresh credential write.
    pub fn unrevoke_slot(&self, slot_id: &AuthSlotId) -> Result<()> {
        let mut state = self.state.write();
        let now = now_ms();
        prune_state(&mut state, now);
        if state.revoked_slots.remove(&slot_id.0).is_some() {
            self.persist_state(&state)?;
        }
        Ok(())
    }

    /// Returns whether one daemon-managed auth slot has been explicitly revoked.
    pub fn is_slot_revoked(&self, slot_id: &AuthSlotId) -> bool {
        let now = now_ms();
        let state = self.state.read();
        state
            .revoked_slots
            .get(&slot_id.0)
            .copied()
            .is_some_and(|revoked_at_ms| revoked_at_ms <= now)
    }

    /// Returns one operator-facing status for an issued lease when known.
    pub fn lease_status(&self, lease_id: &str) -> Option<CredentialLeaseStatus> {
        let now = now_ms();
        let state = self.state.read();
        let lease = state
            .issued_connector_leases
            .get(lease_id)
            .or_else(|| state.issued_route_leases.get(lease_id))
            .or_else(|| state.issued_mcp_leases.get(lease_id))?
            .clone();
        let revoked = lease_is_revoked_locked(&state, &lease, now);
        let active = !revoked
            && lease.expires_at_ms >= now
            && !state.revoked_subjects.contains_key(&lease.subject_id)
            && state
                .subject_epochs
                .get(&lease.subject_id)
                .copied()
                .unwrap_or_default()
                <= lease.subject_epoch;
        Some(CredentialLeaseStatus {
            lease,
            revoked,
            active,
        })
    }

    fn ensure_lease_active(
        &self,
        lease: &CredentialLease,
        now_ms: u64,
        context: &str,
    ) -> Result<()> {
        let state = self.state.read();
        anyhow::ensure!(
            !lease_is_revoked_locked(&state, lease, now_ms),
            "{context} is revoked"
        );
        let current_epoch = state
            .subject_epochs
            .get(&lease.subject_id)
            .copied()
            .unwrap_or_default();
        anyhow::ensure!(
            !state.revoked_subjects.contains_key(&lease.subject_id)
                && current_epoch <= lease.subject_epoch,
            "credential subject {} has been revoked",
            lease.subject_id
        );
        Ok(())
    }

    fn persist_state(&self, state: &RevocationState) -> Result<()> {
        persist_state(&self.state_path, state)
    }
}

impl AuthSubject {
    fn from_context(context: &ExecutionCredentialContext, connector: Option<&str>) -> Self {
        if let Some(agent_id) = context.agent_id.as_ref() {
            return Self {
                subject_id: context
                    .principal_id
                    .clone()
                    .unwrap_or_else(|| format!("agent:{agent_id}")),
                kind: AuthSubjectKind::Agent,
                session_id: context.session_id.clone(),
                agent_id: Some(agent_id.clone()),
                run_id: context.run_id.clone(),
                principal_id: context.principal_id.clone(),
                parent_principal_id: context.parent_principal_id.clone(),
                delegation_id: context.delegation_id.clone(),
            };
        }
        if let Some(session_id) = context.session_id.as_ref() {
            return Self {
                subject_id: context
                    .principal_id
                    .clone()
                    .unwrap_or_else(|| format!("session:{session_id}")),
                kind: AuthSubjectKind::Session,
                session_id: Some(session_id.clone()),
                agent_id: None,
                run_id: context.run_id.clone(),
                principal_id: context.principal_id.clone(),
                parent_principal_id: context.parent_principal_id.clone(),
                delegation_id: context.delegation_id.clone(),
            };
        }
        if let Some(connector) = connector {
            return Self {
                subject_id: format!("connector:{connector}"),
                kind: AuthSubjectKind::Connector,
                session_id: None,
                agent_id: None,
                run_id: None,
                principal_id: None,
                parent_principal_id: None,
                delegation_id: None,
            };
        }
        Self {
            subject_id: "daemon".to_string(),
            kind: AuthSubjectKind::Daemon,
            session_id: None,
            agent_id: None,
            run_id: None,
            principal_id: None,
            parent_principal_id: None,
            delegation_id: None,
        }
    }
}

fn connector_scope(connector: &str, env_keys: &[String]) -> CredentialScope {
    CredentialScope {
        connector_allow: vec![connector.to_string()],
        connector_credential_allow: env_keys
            .iter()
            .map(|env_key| format!("{connector}:{}", env_key.trim()))
            .collect(),
        ..CredentialScope::default()
    }
    .normalized()
}

fn normalize_entries(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .map(ToOwned::to_owned)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn normalize_slot_refs(entries: &[AuthSlotId]) -> Vec<AuthSlotId> {
    entries
        .iter()
        .map(|entry| entry.0.trim())
        .filter(|entry| !entry.is_empty())
        .map(AuthSlotId::new)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn stable_id<T>(value: &T) -> String
where
    T: Serialize,
{
    digest_bytes(&serde_json::to_vec(value).expect("stable id payload should serialize"))
}

fn digest_str(value: &str) -> String {
    digest_bytes(value.as_bytes())
}

fn digest_bytes(value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value);
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

fn load_state(path: &Path) -> Result<RevocationState> {
    if !path.exists() {
        return Ok(RevocationState::default());
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read credential broker state {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse credential broker state {}", path.display()))
}

fn persist_state(path: &Path, state: &RevocationState) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create credential broker state directory {}",
            parent.display()
        )
    })?;
    let payload = serde_json::to_vec_pretty(state)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".credential-broker-")
        .suffix(".json.tmp")
        .tempfile_in(parent)
        .with_context(|| {
            format!(
                "failed to create temporary credential broker state in {}",
                parent.display()
            )
        })?;
    #[cfg(unix)]
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| {
            format!(
                "failed to set credential broker state permissions for {}",
                path.display()
            )
        })?;
    tmp.write_all(&payload).with_context(|| {
        format!(
            "failed to write temporary credential broker state {}",
            tmp.path().display()
        )
    })?;
    tmp.flush().with_context(|| {
        format!(
            "failed to flush temporary credential broker state {}",
            tmp.path().display()
        )
    })?;
    tmp.as_file().sync_all().with_context(|| {
        format!(
            "failed to sync temporary credential broker state {}",
            tmp.path().display()
        )
    })?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).with_context(|| {
            format!(
                "failed to replace existing credential broker state {}",
                path.display()
            )
        })?;
    }
    tmp.persist(path).map(|_| ()).map_err(|error| {
        anyhow!(
            "failed to persist credential broker state from {} to {}: {}",
            error.file.path().display(),
            path.display(),
            error.error
        )
    })
}

fn prune_connector_leases(leases: &mut BTreeMap<String, CredentialLease>, now_ms: u64) {
    leases.retain(|_, lease| lease.expires_at_ms >= now_ms);
}

fn prune_revoked_leases(leases: &mut BTreeMap<String, u64>, now_ms: u64) {
    leases.retain(|_, expires_at_ms| *expires_at_ms >= now_ms);
}

fn prune_state(state: &mut RevocationState, now_ms: u64) {
    prune_revoked_leases(&mut state.revoked_leases, now_ms);
    state
        .issued_route_leases
        .retain(|_, lease| lease.expires_at_ms >= now_ms);
    state
        .issued_connector_leases
        .retain(|_, lease| lease.expires_at_ms >= now_ms);
    state
        .issued_mcp_leases
        .retain(|_, lease| lease.expires_at_ms >= now_ms);
    let active_grant_ids = state
        .issued_route_leases
        .values()
        .chain(state.issued_connector_leases.values())
        .chain(state.issued_mcp_leases.values())
        .map(|lease| lease.grant_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    state
        .issued_grants
        .retain(|grant_id, _| active_grant_ids.contains(grant_id));
}

fn known_subject_locked(state: &RevocationState, subject_id: &str) -> bool {
    state.subject_epochs.contains_key(subject_id)
        || state.revoked_subjects.contains_key(subject_id)
        || state
            .issued_grants
            .values()
            .any(|grant| grant.subject.subject_id == subject_id)
        || state
            .issued_route_leases
            .values()
            .any(|lease| lease.subject_id == subject_id)
        || state
            .issued_connector_leases
            .values()
            .any(|lease| lease.subject_id == subject_id)
        || state
            .issued_mcp_leases
            .values()
            .any(|lease| lease.subject_id == subject_id)
}

fn remember_subject_locked(state: &mut RevocationState, subject_id: &str) -> u64 {
    if !known_subject_locked(state, subject_id) {
        state.subject_epochs.insert(subject_id.to_string(), 0);
    }
    state
        .subject_epochs
        .get(subject_id)
        .copied()
        .unwrap_or_default()
}

fn ensure_subject_not_revoked_locked(
    state: &RevocationState,
    subject_id: &str,
    observed_epoch: u64,
) -> Result<()> {
    let current_epoch = state
        .subject_epochs
        .get(subject_id)
        .copied()
        .unwrap_or_default();
    anyhow::ensure!(
        !state.revoked_subjects.contains_key(subject_id) && current_epoch <= observed_epoch,
        "credential subject {subject_id} has been revoked"
    );
    Ok(())
}

fn ensure_audience_slots_not_revoked_locked(
    state: &RevocationState,
    audience: &CredentialLeaseAudience,
) -> Result<()> {
    for slot_id in audience_slot_refs(audience) {
        anyhow::ensure!(
            !state.revoked_slots.contains_key(slot_id),
            "auth slot `{slot_id}` has been revoked"
        );
    }
    Ok(())
}

fn lease_is_revoked_locked(state: &RevocationState, lease: &CredentialLease, now_ms: u64) -> bool {
    state
        .revoked_leases
        .get(&lease.id)
        .copied()
        .is_some_and(|expires_at_ms| expires_at_ms >= now_ms)
        || lease_targets_revoked_slot_locked(state, lease)
}

fn lease_targets_revoked_slot_locked(state: &RevocationState, lease: &CredentialLease) -> bool {
    audience_slot_refs(&lease.audience)
        .into_iter()
        .any(|slot_id| state.revoked_slots.contains_key(slot_id))
}

fn audience_slot_refs(audience: &CredentialLeaseAudience) -> Vec<&str> {
    match audience {
        CredentialLeaseAudience::Route { slot_id, .. }
        | CredentialLeaseAudience::McpServer { slot_id, .. } => vec![slot_id.0.as_str()],
        CredentialLeaseAudience::Connector { secret_refs, .. } => secret_refs
            .iter()
            .map(|slot_id| slot_id.0.as_str())
            .collect(),
    }
}

fn lease_targets_slot(lease: &CredentialLease, slot_id: &AuthSlotId) -> bool {
    match &lease.audience {
        CredentialLeaseAudience::Route {
            slot_id: lease_slot,
            ..
        }
        | CredentialLeaseAudience::McpServer {
            slot_id: lease_slot,
            ..
        } => lease_slot == slot_id,
        CredentialLeaseAudience::Connector { secret_refs, .. } => {
            secret_refs.iter().any(|secret_ref| secret_ref == slot_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{
        CredentialBroker, CredentialLeaseAudience, DEFAULT_CONNECTOR_LEASE_TTL_MS,
        ExecutionCredentialContext,
    };
    use crate::AuthSlotId;

    #[test]
    fn route_authorization_honors_credential_scope() {
        let temp = tempdir().expect("tempdir");
        let broker = CredentialBroker::new(temp.path().join("broker.json")).expect("broker");
        let context = ExecutionCredentialContext {
            session_id: Some("session-1".to_string()),
            credential_scope: kheish_types::CredentialScope {
                route_allow: vec!["openai".to_string()],
                ..Default::default()
            },
            ..ExecutionCredentialContext::default()
        };

        broker
            .authorize_route(&AuthSlotId::new("openai.prod"), "openai", &context)
            .expect("openai route should be allowed");
        let error = broker
            .authorize_route(&AuthSlotId::new("anthropic.prod"), "anthropic", &context)
            .expect_err("anthropic route should be blocked");
        assert!(error.to_string().contains("blocks route anthropic"));
    }

    #[test]
    fn connector_leases_validate_and_subject_revocation_is_immediate() {
        let temp = tempdir().expect("tempdir");
        let broker = CredentialBroker::new(temp.path().join("broker.json")).expect("broker");
        let context = ExecutionCredentialContext {
            agent_id: Some("agent-1".to_string()),
            principal_id: Some("agent:agent-1".to_string()),
            credential_scope: kheish_types::CredentialScope {
                connector_allow: vec!["slack".to_string()],
                connector_credential_allow: vec!["slack:BOT_TOKEN".to_string()],
                ..Default::default()
            },
            ..ExecutionCredentialContext::default()
        };

        let (token, lease) = broker
            .issue_connector_lease(
                "slack",
                &[String::from("BOT_TOKEN")],
                &[AuthSlotId::new("slack.bot_token")],
                DEFAULT_CONNECTOR_LEASE_TTL_MS,
                Some(&context),
            )
            .expect("lease should be issued");
        match &lease.audience {
            CredentialLeaseAudience::Connector {
                connector,
                env_keys,
                secret_refs,
            } => {
                assert_eq!(connector, "slack");
                assert_eq!(env_keys, &vec!["BOT_TOKEN".to_string()]);
                assert_eq!(secret_refs, &vec![AuthSlotId::new("slack.bot_token")]);
            }
            other => panic!("unexpected audience: {other:?}"),
        }
        broker
            .validate_connector_lease(&token, "slack", "BOT_TOKEN")
            .expect("lease should validate");

        broker
            .revoke_subject("agent:agent-1")
            .expect("subject revocation should persist");
        let error = broker
            .validate_connector_lease(&token, "slack", "BOT_TOKEN")
            .expect_err("revoked subject should invalidate the lease");
        assert!(
            error.to_string().contains("is revoked")
                || error.to_string().contains("has been revoked")
        );
        let status = broker
            .lease_status(&lease.id)
            .expect("lease status should still be queryable");
        assert!(status.revoked);
        assert!(!status.active);
        let error = broker
            .authorize_route(&AuthSlotId::new("openai.prod"), "openai", &context)
            .expect_err("revoked subject should not receive fresh route grants");
        assert!(error.to_string().contains("has been revoked"));
    }

    #[test]
    fn connector_lease_rejects_secret_ref_mismatch() {
        let temp = tempdir().expect("tempdir");
        let broker = CredentialBroker::new(temp.path().join("broker.json")).expect("broker");
        let (token, _) = broker
            .issue_connector_lease(
                "webhook",
                &[String::from("WEBHOOK_ROUTES_JSON")],
                &[AuthSlotId::new("sidecars.webhook.routes")],
                DEFAULT_CONNECTOR_LEASE_TTL_MS,
                None,
            )
            .expect("lease should be issued");

        broker
            .validate_connector_lease_for_secret_ref(
                &token,
                "webhook",
                "WEBHOOK_ROUTES_JSON",
                &AuthSlotId::new("sidecars.webhook.routes"),
            )
            .expect("matching secret ref should validate");
        let error = broker
            .validate_connector_lease_for_secret_ref(
                &token,
                "webhook",
                "WEBHOOK_ROUTES_JSON",
                &AuthSlotId::new("sidecars.webhook.remapped"),
            )
            .expect_err("remapped secret ref should be rejected");
        assert!(error.to_string().contains("does not expose"));
    }

    #[test]
    fn revoked_slot_blocks_direct_broker_issuance_and_validation() {
        let temp = tempdir().expect("tempdir");
        let broker = CredentialBroker::new(temp.path().join("broker.json")).expect("broker");
        let slot_id = AuthSlotId::new("sidecars.webhook.routes");
        let (token, lease) = broker
            .issue_connector_lease(
                "webhook",
                &[String::from("WEBHOOK_ROUTES_JSON")],
                std::slice::from_ref(&slot_id),
                DEFAULT_CONNECTOR_LEASE_TTL_MS,
                None,
            )
            .expect("lease should be issued");

        broker
            .validate_connector_lease_for_secret_ref(
                &token,
                "webhook",
                "WEBHOOK_ROUTES_JSON",
                &slot_id,
            )
            .expect("lease should validate before slot revoke");
        broker
            .revoke_slot_leases(&slot_id)
            .expect("slot revoke should persist");

        let status = broker.lease_status(&lease.id).expect("lease status");
        assert!(status.revoked);
        assert!(!status.active);
        let error = broker
            .validate_connector_lease_for_secret_ref(
                &token,
                "webhook",
                "WEBHOOK_ROUTES_JSON",
                &slot_id,
            )
            .expect_err("revoked slot should invalidate connector lease");
        assert!(error.to_string().contains("is revoked"));

        let error = broker
            .issue_connector_lease(
                "webhook",
                &[String::from("WEBHOOK_ROUTES_JSON")],
                std::slice::from_ref(&slot_id),
                DEFAULT_CONNECTOR_LEASE_TTL_MS,
                None,
            )
            .expect_err("revoked slot should block connector lease issuance");
        assert!(error.to_string().contains("has been revoked"));
        let context = ExecutionCredentialContext::default();
        let error = broker
            .authorize_route(&slot_id, "openai", &context)
            .expect_err("revoked slot should block route grants");
        assert!(error.to_string().contains("has been revoked"));
        let error = broker
            .authorize_mcp_server(&slot_id, "docs", &[], &context)
            .expect_err("revoked slot should block MCP grants");
        assert!(error.to_string().contains("has been revoked"));

        broker
            .unrevoke_slot(&slot_id)
            .expect("reprovision clears tombstone");
        broker
            .issue_connector_lease(
                "webhook",
                &[String::from("WEBHOOK_ROUTES_JSON")],
                std::slice::from_ref(&slot_id),
                DEFAULT_CONNECTOR_LEASE_TTL_MS,
                None,
            )
            .expect("freshly provisioned slot should issue leases");
    }

    #[test]
    fn connector_lease_expiry_survives_restart_and_status_redacts_digest() {
        let temp = tempdir().expect("tempdir");
        let state_path = temp.path().join("broker.json");
        let broker = CredentialBroker::new(&state_path).expect("broker");
        let (token, lease) = broker
            .issue_connector_lease(
                "webhook",
                &[String::from("TOKEN")],
                &[AuthSlotId::new("sidecars.webhook.token")],
                1,
                None,
            )
            .expect("lease should be issued");
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        let error = broker
            .validate_connector_lease(&token, "webhook", "TOKEN")
            .expect_err("expired lease should fail validation");
        assert!(
            error.to_string().contains("expired")
                || error
                    .to_string()
                    .contains("unknown connector credential lease")
        );
        let status = broker.lease_status(&lease.id).expect("lease status");
        assert!(!status.active);
        let serialized = serde_json::to_string(&status).expect("serialize status");
        assert!(!serialized.contains("token_digest"));
        assert!(!serialized.contains(&lease.token_digest));
        let debug_status = format!("{status:?}");
        assert!(debug_status.contains("token_digest: \"<redacted>\""));
        assert!(!debug_status.contains(&lease.token_digest));
        let debug_lease = format!("{lease:?}");
        assert!(debug_lease.contains("token_digest: \"<redacted>\""));
        assert!(!debug_lease.contains(&lease.token_digest));

        let restarted = CredentialBroker::new(&state_path).expect("broker restart");
        assert!(
            restarted.lease_status(&lease.id).is_none(),
            "expired lease should be pruned on restart"
        );
        let error = restarted
            .validate_connector_lease(&token, "webhook", "TOKEN")
            .expect_err("expired restarted lease should not be active");
        assert!(
            error
                .to_string()
                .contains("unknown connector credential lease")
        );
    }

    #[cfg(unix)]
    #[test]
    fn broker_state_is_written_private_on_unix() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempdir().expect("tempdir");
        let state_path = temp.path().join("broker.json");
        let broker = CredentialBroker::new(&state_path).expect("broker");
        broker
            .issue_connector_lease(
                "webhook",
                &[String::from("TOKEN")],
                &[AuthSlotId::new("sidecars.webhook.token")],
                DEFAULT_CONNECTOR_LEASE_TTL_MS,
                None,
            )
            .expect("lease should be issued");
        let mode = std::fs::metadata(&state_path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn mcp_leases_are_tracked_separately_from_route_leases() {
        let temp = tempdir().expect("tempdir");
        let broker = CredentialBroker::new(temp.path().join("broker.json")).expect("broker");
        let context = ExecutionCredentialContext {
            session_id: Some("session-1".to_string()),
            credential_scope: kheish_types::CredentialScope {
                route_allow: vec!["openai".to_string()],
                mcp_server_allow: vec!["notion".to_string()],
                ..Default::default()
            },
            ..ExecutionCredentialContext::default()
        };

        let route_grant = broker
            .authorize_route(&AuthSlotId::new("openai.prod"), "openai", &context)
            .expect("route grant");
        let route_lease = broker
            .issue_route_lease(&route_grant, 60_000)
            .expect("route lease");
        let mcp_grant = broker
            .authorize_mcp_server(
                &AuthSlotId::new("mcp.oauth.notion"),
                "notion",
                &["read".to_string()],
                &context,
            )
            .expect("mcp grant");
        let mcp_lease = broker
            .issue_mcp_lease(&mcp_grant, 60_000)
            .expect("mcp lease");

        let status = broker
            .subject_status("session:session-1")
            .expect("subject status");
        assert_eq!(status.active_route_lease_ids, vec![route_lease.id.clone()]);
        assert_eq!(status.active_mcp_lease_ids, vec![mcp_lease.id.clone()]);
        assert!(broker.lease_status(&route_lease.id).expect("route").active);
        assert!(broker.lease_status(&mcp_lease.id).expect("mcp").active);
    }
}
