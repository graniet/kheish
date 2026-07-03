use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow};
use kheish_runtime::redact_text;
use sha2::Digest;
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, Notify};
use tracing::warn;

use crate::assets::FileAssetStore;
use crate::capture_provision::{
    CaptureAgentAlertView, CaptureAgentHeartbeatRequest, CaptureAgentHeartbeatResponse,
    CaptureAgentRecord, CaptureAgentStatus, CaptureHeartbeatState, FileCaptureAgentStore,
};
use crate::observation_transcripts::ObservationTranscriptSelection;
use crate::observations::{
    CreateObservationSourceRequest, FileObservationStore, ObservationAuditRecord,
    ObservationMaterializationRequest, ObservationRetentionState, ObservationSelection,
    ObservationSourceRecord, ObservationSourceStatus, ObservationSourceUploadTokenRecord,
    ObservationSourceView, ObservationView, RevokeObservationSourceTokenRequest,
    RotateObservationSourceTokenRequest,
};
use crate::runs::now_ms;

/// Owns durable observation sources and observation metadata for the daemon.
pub(crate) struct ObservationService {
    store: FileObservationStore,
    capture_agent_store: FileCaptureAgentStore,
    assets: Arc<FileAssetStore>,
    sources: Mutex<BTreeMap<String, ObservationSourceRecord>>,
    capture_agents: Mutex<BTreeMap<String, CaptureAgentRecord>>,
    observations: Mutex<BTreeMap<String, ObservationView>>,
    ingress_rate_limits: Mutex<BTreeMap<String, ObservationIngressRateLimitState>>,
    audit_lock: Mutex<()>,
    notify: Notify,
    next_source_id: AtomicU64,
    next_observation_id: AtomicU64,
}

#[derive(Clone, Debug)]
struct ObservationIngressRateLimitState {
    /// Available tokens in the bucket (fractional; refills continuously with elapsed time).
    tokens: f64,
    /// Wall-clock timestamp (ms) of the last refill, used to compute elapsed time.
    last_refill_ms: u64,
}

impl ObservationIngressRateLimitState {
    /// Refills the bucket for the time elapsed since the last call, then tries to spend one token.
    ///
    /// This is a token bucket: capacity is `burst`, refilled at `burst` tokens per `window_ms`. It
    /// bounds the sustained rate to `burst`/`window_ms` and caps any instantaneous burst at `burst`
    /// — unlike the previous fixed-window counter, which admitted up to ~2x `burst` across a window
    /// boundary. `window_ms`/`burst` are clamped to >= 1 defensively; source validation
    /// (`CreateObservationSourceRequest::validate`) already guarantees both are non-zero.
    fn admit(
        &mut self,
        now_ms: u64,
        window_ms: u64,
        burst: u64,
    ) -> ObservationIngressRateLimitDecision {
        let window_ms = window_ms.max(1);
        let capacity = burst.max(1) as f64;
        let elapsed_ms = now_ms.saturating_sub(self.last_refill_ms);
        let refill = elapsed_ms as f64 * capacity / window_ms as f64;
        self.tokens = (self.tokens + refill).min(capacity);
        self.last_refill_ms = now_ms;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            ObservationIngressRateLimitDecision::Accepted
        } else {
            let deficit = 1.0 - self.tokens;
            let retry_after_ms = (deficit * window_ms as f64 / capacity).ceil() as u64;
            ObservationIngressRateLimitDecision::Limited {
                retry_after_ms: retry_after_ms.max(1),
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ObservationIngressRateLimitDecision {
    Accepted,
    Limited { retry_after_ms: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObservationUploadAuthorization {
    Authorized,
    InvalidToken,
    ExpiredToken,
    RevokedToken,
    SourceInactive,
}

impl ObservationService {
    /// Creates a new observation service backed by persisted daemon state.
    pub(crate) fn new(
        store: FileObservationStore,
        capture_agent_store: FileCaptureAgentStore,
        assets: Arc<FileAssetStore>,
        sources: BTreeMap<String, ObservationSourceRecord>,
        capture_agents: BTreeMap<String, CaptureAgentRecord>,
        observations: BTreeMap<String, ObservationView>,
        next_source_id: AtomicU64,
        next_observation_id: AtomicU64,
    ) -> Self {
        Self {
            store,
            capture_agent_store,
            assets,
            sources: Mutex::new(sources),
            capture_agents: Mutex::new(capture_agents),
            observations: Mutex::new(observations),
            ingress_rate_limits: Mutex::new(BTreeMap::new()),
            audit_lock: Mutex::new(()),
            notify: Notify::new(),
            next_source_id,
            next_observation_id,
        }
    }

    /// Returns one fresh daemon-managed observation source identifier.
    pub(crate) fn next_source_id(&self) -> String {
        format!(
            "source-{}",
            self.next_source_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Returns one fresh daemon-managed observation identifier.
    pub(crate) fn next_observation_id(&self) -> String {
        format!(
            "observation-{}",
            self.next_observation_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Lists source views optionally filtered by one case-insensitive query.
    pub(crate) async fn list_sources(&self, query: Option<&str>) -> Vec<ObservationSourceView> {
        let query = query
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        let mut views = self
            .sources
            .lock()
            .await
            .values()
            .map(|record| record.view.clone())
            .filter(|view| {
                query.as_ref().is_none_or(|query| {
                    view.source_id.to_ascii_lowercase().contains(query)
                        || view.display_name.to_ascii_lowercase().contains(query)
                })
            })
            .collect::<Vec<_>>();
        views.sort_by(|left, right| left.source_id.cmp(&right.source_id));
        views
    }

    /// Returns one source record when it exists.
    pub(crate) async fn source_record(&self, source_id: &str) -> Option<ObservationSourceRecord> {
        self.sources.lock().await.get(source_id).cloned()
    }

    /// Returns one source view by identifier.
    pub(crate) async fn get_source(&self, source_id: &str) -> Result<ObservationSourceView> {
        self.source_record(source_id)
            .await
            .map(|record| record.view)
            .ok_or_else(|| anyhow!("unknown observation source {source_id}"))
    }

    /// Lists durable capture agents, refreshing heartbeat-missing alerts first.
    pub(crate) async fn list_capture_agents(&self) -> Result<Vec<crate::CaptureAgentView>> {
        self.refresh_capture_agent_heartbeat_alerts(now_ms())
            .await?;
        let mut views = self
            .capture_agents
            .lock()
            .await
            .values()
            .map(|record| record.view.clone())
            .collect::<Vec<_>>();
        views.sort_by(|left, right| left.machine_id.cmp(&right.machine_id));
        Ok(views)
    }

    /// Returns one durable capture-agent view.
    pub(crate) async fn get_capture_agent(
        &self,
        machine_id: &str,
    ) -> Result<crate::CaptureAgentView> {
        self.refresh_capture_agent_heartbeat_alerts(now_ms())
            .await?;
        self.capture_agents
            .lock()
            .await
            .get(machine_id)
            .map(|record| record.view.clone())
            .ok_or_else(|| anyhow!("unknown capture agent {machine_id}"))
    }

    /// Returns current capture-agent alerts.
    pub(crate) async fn list_capture_alerts(&self) -> Result<Vec<CaptureAgentAlertView>> {
        let now = now_ms();
        self.refresh_capture_agent_heartbeat_alerts(now).await?;
        let mut alerts = self
            .capture_agents
            .lock()
            .await
            .values()
            .filter(|record| record.view.heartbeat_state == CaptureHeartbeatState::Missing)
            .map(|record| CaptureAgentAlertView {
                machine_id: record.view.machine_id.clone(),
                kind: "missing_heartbeat".to_string(),
                severity: "warning".to_string(),
                message: format!(
                    "capture agent {} missed heartbeat deadline {}",
                    record.view.machine_id, record.view.heartbeat_deadline_ms
                ),
                detected_at_ms: record.view.heartbeat_missing_since_ms.unwrap_or(now),
                heartbeat_deadline_ms: record.view.heartbeat_deadline_ms,
            })
            .collect::<Vec<_>>();
        alerts.sort_by(|left, right| left.machine_id.cmp(&right.machine_id));
        Ok(alerts)
    }

    async fn refresh_capture_agent_heartbeat_alerts(&self, now_ms: u64) -> Result<()> {
        let mut agents = self.capture_agents.lock().await;
        let mut missing_agent_ids = Vec::new();
        for record in agents.values_mut() {
            if record.view.status == CaptureAgentStatus::Active
                && record.view.heartbeat_state == CaptureHeartbeatState::Missing
                && record.view.heartbeat_missing_since_ms.is_none()
            {
                let previous = record.clone();
                record.view.heartbeat_missing_since_ms = Some(now_ms);
                record.view.updated_at_ms = now_ms;
                if let Err(error) = self.capture_agent_store.save_agent(record) {
                    *record = previous;
                    return Err(error);
                }
            }
            if record.view.status != CaptureAgentStatus::Active
                || record.view.heartbeat_state == CaptureHeartbeatState::Missing
                || now_ms <= record.view.heartbeat_deadline_ms
            {
                continue;
            }
            let previous = record.clone();
            record.view.heartbeat_state = CaptureHeartbeatState::Missing;
            record.view.heartbeat_missing_since_ms = Some(now_ms);
            record.view.updated_at_ms = now_ms;
            if let Err(error) = self.capture_agent_store.save_agent(record) {
                *record = previous;
                return Err(error);
            }
            missing_agent_ids.push(record.view.machine_id.clone());
        }
        drop(agents);
        for machine_id in missing_agent_ids {
            self.record_audit(ObservationAuditRecord {
                recorded_at_ms: now_ms,
                event: "capture_agent_heartbeat_missing".to_string(),
                source_id: machine_id,
                observation_id: None,
                reason: Some("missing_heartbeat".to_string()),
                upload_token_version: None,
                idempotency_key_sha256: None,
                request_fingerprint: None,
                media_type: None,
                byte_length: None,
                retry_after_ms: None,
                purged_asset_ids: Vec::new(),
            })
            .await;
        }
        Ok(())
    }

    /// Persists and registers one new source record, or rotates one existing source in place.
    pub(crate) async fn create_source(
        &self,
        mut record: ObservationSourceRecord,
    ) -> Result<ObservationSourceView> {
        let mut sources = self.sources.lock().await;
        let mut event = "source_created";
        if let Some(existing) = sources.get(&record.view.source_id).cloned() {
            anyhow::ensure!(
                existing.view.kind == record.view.kind,
                "observation source {} cannot change kind from {:?} to {:?}",
                record.view.source_id,
                existing.view.kind,
                record.view.kind
            );
            let token_rotated = existing.upload_token_sha256 != record.upload_token_sha256;
            let now = now_ms();
            record.view.created_at_ms = existing.view.created_at_ms;
            record.view.updated_at_ms = now;
            record.view.status = existing.view.status;
            record.view.last_authenticated_at_ms = existing.view.last_authenticated_at_ms;
            record.view.last_observed_at_ms = existing.view.last_observed_at_ms;
            record.view.active_observation_count = existing.view.active_observation_count;
            record.view.active_byte_length = existing.view.active_byte_length;
            record.view.upload_token_version = if token_rotated {
                existing.view.upload_token_version.saturating_add(1).max(1)
            } else {
                existing.view.upload_token_version.max(1)
            };
            record.view.last_token_rotated_at_ms = if token_rotated {
                Some(now)
            } else {
                existing.view.last_token_rotated_at_ms
            };
            record.view.upload_token_revoked_at_ms = if token_rotated {
                None
            } else {
                existing.view.upload_token_revoked_at_ms
            };
            record.previous_upload_tokens = if token_rotated {
                Vec::new()
            } else {
                existing.previous_upload_tokens
            };
            event = if token_rotated {
                "source_rotated"
            } else {
                "source_updated"
            };
        }
        self.store.save_source(&record)?;
        let view = record.view.clone();
        sources.insert(view.source_id.clone(), record);
        drop(sources);
        self.record_audit(ObservationAuditRecord {
            recorded_at_ms: now_ms(),
            event: event.to_string(),
            source_id: view.source_id.clone(),
            observation_id: None,
            reason: None,
            upload_token_version: Some(view.upload_token_version),
            idempotency_key_sha256: None,
            request_fingerprint: None,
            media_type: None,
            byte_length: None,
            retry_after_ms: None,
            purged_asset_ids: Vec::new(),
        })
        .await;
        self.notify.notify_waiters();
        Ok(view)
    }

    /// Persists and registers a batch of source records, rolling back best-effort on failure.
    pub(crate) async fn create_sources_batch(
        &self,
        records: Vec<ObservationSourceRecord>,
    ) -> Result<Vec<ObservationSourceView>> {
        let mut sources = self.sources.lock().await;
        let previous = sources.clone();
        let mut prepared = Vec::with_capacity(records.len());
        let mut incoming_source_ids = std::collections::BTreeSet::new();
        for mut record in records {
            anyhow::ensure!(
                incoming_source_ids.insert(record.view.source_id.clone()),
                "duplicate observation source {} in batch",
                record.view.source_id
            );
            if let Some(existing) = sources.get(&record.view.source_id).cloned() {
                anyhow::ensure!(
                    existing.view.kind == record.view.kind,
                    "observation source {} cannot change kind from {:?} to {:?}",
                    record.view.source_id,
                    existing.view.kind,
                    record.view.kind
                );
                let token_rotated = existing.upload_token_sha256 != record.upload_token_sha256;
                let now = now_ms();
                record.view.created_at_ms = existing.view.created_at_ms;
                record.view.updated_at_ms = now;
                record.view.status = existing.view.status;
                record.view.last_authenticated_at_ms = existing.view.last_authenticated_at_ms;
                record.view.last_observed_at_ms = existing.view.last_observed_at_ms;
                record.view.active_observation_count = existing.view.active_observation_count;
                record.view.active_byte_length = existing.view.active_byte_length;
                record.view.upload_token_version = if token_rotated {
                    existing.view.upload_token_version.saturating_add(1).max(1)
                } else {
                    existing.view.upload_token_version.max(1)
                };
                record.view.last_token_rotated_at_ms = if token_rotated {
                    Some(now)
                } else {
                    existing.view.last_token_rotated_at_ms
                };
                record.view.upload_token_revoked_at_ms = if token_rotated {
                    None
                } else {
                    existing.view.upload_token_revoked_at_ms
                };
                record.previous_upload_tokens = if token_rotated {
                    Vec::new()
                } else {
                    existing.previous_upload_tokens
                };
            }
            prepared.push(record);
        }

        let mut saved_source_ids: Vec<String> = Vec::new();
        for record in &prepared {
            if let Err(error) = self.store.save_source(record) {
                for source_id in saved_source_ids.iter().rev() {
                    if let Some(previous_record) = previous.get(source_id) {
                        let _ = self.store.save_source(previous_record);
                    } else {
                        let _ = self.store.delete_source(source_id);
                    }
                }
                *sources = previous;
                return Err(error);
            }
            saved_source_ids.push(record.view.source_id.clone());
        }

        let views = prepared
            .into_iter()
            .map(|record| {
                let view = record.view.clone();
                sources.insert(view.source_id.clone(), record);
                view
            })
            .collect::<Vec<_>>();
        drop(sources);
        for view in &views {
            self.record_audit(ObservationAuditRecord {
                recorded_at_ms: now_ms(),
                event: "source_batch_upserted".to_string(),
                source_id: view.source_id.clone(),
                observation_id: None,
                reason: None,
                upload_token_version: Some(view.upload_token_version),
                idempotency_key_sha256: None,
                request_fingerprint: None,
                media_type: None,
                byte_length: None,
                retry_after_ms: None,
                purged_asset_ids: Vec::new(),
            })
            .await;
        }
        self.notify.notify_waiters();
        Ok(views)
    }

    /// Rotates one source upload token while optionally accepting the previous token briefly.
    pub(crate) async fn rotate_source_upload_token(
        &self,
        source_id: &str,
        request: RotateObservationSourceTokenRequest,
        rotated_at_ms: u64,
    ) -> Result<ObservationSourceView> {
        request.validate()?;
        self.ensure_source_not_capture_owned(source_id).await?;
        let new_digest = digest_source_upload_token(&request.upload_token);
        let mut sources = self.sources.lock().await;
        let record = sources
            .get_mut(source_id)
            .ok_or_else(|| anyhow!("unknown observation source {source_id}"))?;
        let previous = record.clone();
        let old_digest = record.upload_token_sha256.clone();
        let old_version = record.view.upload_token_version.max(1);
        anyhow::ensure!(
            old_digest != new_digest || record.view.upload_token_revoked_at_ms.is_none(),
            "upload_token must differ from the revoked current token"
        );
        if old_digest != new_digest
            && request.grace_period_ms > 0
            && record.view.upload_token_revoked_at_ms.is_none()
        {
            record
                .previous_upload_tokens
                .push(ObservationSourceUploadTokenRecord {
                    sha256: old_digest.clone(),
                    version: old_version,
                    expires_at_ms: rotated_at_ms.saturating_add(request.grace_period_ms),
                    revoked_at_ms: None,
                });
        }
        record
            .previous_upload_tokens
            .retain(|token| token.revoked_at_ms.is_none() && token.expires_at_ms > rotated_at_ms);
        if old_digest != new_digest {
            record.view.upload_token_version = old_version.saturating_add(1);
        }
        record.upload_token_sha256 = new_digest;
        record.view.last_token_rotated_at_ms = Some(rotated_at_ms);
        record.view.upload_token_revoked_at_ms = None;
        record.view.updated_at_ms = rotated_at_ms;
        if let Err(error) = self.store.save_source(record) {
            *record = previous;
            return Err(error);
        }
        let view = record.view.clone();
        drop(sources);
        self.record_audit(ObservationAuditRecord {
            recorded_at_ms: rotated_at_ms,
            event: "source_token_rotated".to_string(),
            source_id: view.source_id.clone(),
            observation_id: None,
            reason: (request.grace_period_ms > 0)
                .then(|| format!("grace_period_ms={}", request.grace_period_ms)),
            upload_token_version: Some(view.upload_token_version),
            idempotency_key_sha256: None,
            request_fingerprint: None,
            media_type: None,
            byte_length: None,
            retry_after_ms: None,
            purged_asset_ids: Vec::new(),
        })
        .await;
        self.notify.notify_waiters();
        Ok(view)
    }

    /// Revokes the current source upload token and every grace-period token.
    pub(crate) async fn revoke_source_upload_token(
        &self,
        source_id: &str,
        request: RevokeObservationSourceTokenRequest,
        revoked_at_ms: u64,
    ) -> Result<ObservationSourceView> {
        request.validate()?;
        self.ensure_source_not_capture_owned(source_id).await?;
        let reason = sanitized_operator_audit_reason(request.reason.as_deref());
        let mut sources = self.sources.lock().await;
        let record = sources
            .get_mut(source_id)
            .ok_or_else(|| anyhow!("unknown observation source {source_id}"))?;
        if record.view.upload_token_revoked_at_ms == Some(revoked_at_ms) {
            return Ok(record.view.clone());
        }
        let previous = record.clone();
        record.view.upload_token_revoked_at_ms = Some(revoked_at_ms);
        record.view.updated_at_ms = revoked_at_ms;
        for token in &mut record.previous_upload_tokens {
            if token.revoked_at_ms.is_none() {
                token.revoked_at_ms = Some(revoked_at_ms);
            }
        }
        if let Err(error) = self.store.save_source(record) {
            *record = previous;
            return Err(error);
        }
        let view = record.view.clone();
        drop(sources);
        self.record_audit(ObservationAuditRecord {
            recorded_at_ms: revoked_at_ms,
            event: "source_token_revoked".to_string(),
            source_id: view.source_id.clone(),
            observation_id: None,
            reason,
            upload_token_version: Some(view.upload_token_version),
            idempotency_key_sha256: None,
            request_fingerprint: None,
            media_type: None,
            byte_length: None,
            retry_after_ms: None,
            purged_asset_ids: Vec::new(),
        })
        .await;
        self.notify.notify_waiters();
        Ok(view)
    }

    async fn ensure_source_not_capture_owned(&self, source_id: &str) -> Result<()> {
        if let Some(source) = self.source_record(source_id).await
            && let Some(machine_id) = source.capture_owner_machine_id.as_deref()
        {
            anyhow::bail!(
                "capture-owned observation source {source_id} is managed by capture agent {machine_id}; use capture-agent provisioning or capture-agent revoke"
            );
        }
        let agents = self.capture_agents.lock().await;
        if let Some(agent) = agents.values().find(|agent| {
            agent
                .leases
                .iter()
                .any(|lease| lease.view.source_id == source_id)
                || agent.view.source_ids.iter().any(|owned| owned == source_id)
        }) {
            anyhow::bail!(
                "capture-owned observation source {source_id} is managed by capture agent {}; use capture-agent provisioning or capture-agent revoke",
                agent.view.machine_id
            );
        }
        Ok(())
    }

    /// Persists provisioned observation sources and their owning capture-agent records.
    pub(crate) async fn provision_capture_agent_records(
        &self,
        source_records: Vec<ObservationSourceRecord>,
        agent_records: Vec<CaptureAgentRecord>,
        now_ms: u64,
    ) -> Result<()> {
        let source_views = self.create_sources_batch(source_records).await?;
        let source_versions = source_views
            .iter()
            .map(|view| (view.source_id.clone(), view.upload_token_version))
            .collect::<BTreeMap<_, _>>();
        self.activate_sources(source_versions.keys()).await?;
        self.upsert_capture_agents(agent_records, &source_versions, now_ms)
            .await?;
        Ok(())
    }

    /// Fails closed when a provisioning batch is retried after token material was already emitted.
    pub(crate) async fn ensure_capture_provision_batch_is_new(
        &self,
        batch_id: &str,
        provision_fingerprint_sha256: &str,
    ) -> Result<()> {
        let agents = self.capture_agents.lock().await;
        let mut matching_batch = agents
            .values()
            .filter(|record| record.view.batch_id == batch_id);
        let Some(first) = matching_batch.next() else {
            return Ok(());
        };
        let same_fingerprint = first.view.provision_fingerprint_sha256.as_deref()
            == Some(provision_fingerprint_sha256)
            && matching_batch.all(|record| {
                record.view.provision_fingerprint_sha256.as_deref()
                    == Some(provision_fingerprint_sha256)
            });
        if same_fingerprint {
            anyhow::bail!(
                "capture provisioning batch_id {batch_id} was already applied; raw upload tokens are not replayable, use a new batch_id to rotate"
            );
        }
        anyhow::bail!(
            "capture provisioning batch_id {batch_id} is already bound to a different request"
        );
    }

    async fn activate_sources<'a>(
        &self,
        source_ids: impl IntoIterator<Item = &'a String>,
    ) -> Result<()> {
        let mut sources = self.sources.lock().await;
        for source_id in source_ids {
            let Some(record) = sources.get_mut(source_id) else {
                continue;
            };
            if record.view.status == ObservationSourceStatus::Active {
                continue;
            }
            let previous = record.clone();
            record.view.status = ObservationSourceStatus::Active;
            record.view.updated_at_ms = now_ms();
            if let Err(error) = self.store.save_source(record) {
                *record = previous;
                return Err(error);
            }
        }
        Ok(())
    }

    async fn upsert_capture_agents(
        &self,
        agent_records: Vec<CaptureAgentRecord>,
        source_versions: &BTreeMap<String, u64>,
        now_ms: u64,
    ) -> Result<()> {
        let mut agents = self.capture_agents.lock().await;
        let previous = agents.clone();
        let mut prepared = Vec::with_capacity(agent_records.len());
        for mut incoming in agent_records {
            for lease in &mut incoming.leases {
                if let Some(version) = source_versions.get(&lease.view.source_id) {
                    lease.view.upload_token_version = *version;
                }
            }
            incoming.view.leases = incoming
                .leases
                .iter()
                .map(|lease| lease.view.clone())
                .collect();
            if let Some(existing) = agents.get(&incoming.view.machine_id).cloned() {
                incoming = merge_capture_agent_record(existing, incoming, now_ms);
            }
            prepared.push(incoming);
        }

        let mut saved_agent_ids: Vec<String> = Vec::new();
        for record in &prepared {
            if let Err(error) = self.capture_agent_store.save_agent(record) {
                for machine_id in saved_agent_ids.iter().rev() {
                    if let Some(previous_record) = previous.get(machine_id) {
                        let _ = self.capture_agent_store.save_agent(previous_record);
                    } else {
                        let _ = self.capture_agent_store.delete_agent(machine_id);
                    }
                }
                *agents = previous;
                return Err(error);
            }
            saved_agent_ids.push(record.view.machine_id.clone());
        }

        for record in prepared {
            let view = record.view.clone();
            agents.insert(view.machine_id.clone(), record);
            self.record_audit(ObservationAuditRecord {
                recorded_at_ms: now_ms,
                event: "capture_agent_provisioned".to_string(),
                source_id: view.machine_id,
                observation_id: None,
                reason: None,
                upload_token_version: None,
                idempotency_key_sha256: None,
                request_fingerprint: None,
                media_type: None,
                byte_length: None,
                retry_after_ms: None,
                purged_asset_ids: Vec::new(),
            })
            .await;
        }
        self.notify.notify_waiters();
        Ok(())
    }

    /// Updates the last-authenticated watermark for one source.
    pub(crate) async fn mark_source_authenticated(
        &self,
        source_id: &str,
        authenticated_at_ms: u64,
    ) -> Result<()> {
        let mut sources = self.sources.lock().await;
        let record = sources
            .get_mut(source_id)
            .ok_or_else(|| anyhow!("unknown observation source {source_id}"))?;
        if record.view.last_authenticated_at_ms == Some(authenticated_at_ms) {
            return Ok(());
        }
        let previous = record.clone();
        record.view.last_authenticated_at_ms = Some(authenticated_at_ms);
        record.view.updated_at_ms = authenticated_at_ms;
        if let Err(error) = self.store.save_source(record) {
            *record = previous;
            return Err(error);
        }
        drop(sources);
        self.notify.notify_waiters();
        Ok(())
    }

    /// Accepts one capture-agent heartbeat authenticated by the agent heartbeat token.
    pub(crate) async fn record_capture_agent_heartbeat(
        &self,
        machine_id: &str,
        digest: &[u8; 32],
        request: CaptureAgentHeartbeatRequest,
        heartbeat_at_ms: u64,
    ) -> Result<CaptureAgentHeartbeatResponse> {
        let mut agents = self.capture_agents.lock().await;
        let record = agents
            .get_mut(machine_id)
            .ok_or_else(|| anyhow!("unknown capture agent {machine_id}"))?;
        match authorize_capture_agent_heartbeat_token(record, digest, heartbeat_at_ms) {
            ObservationUploadAuthorization::Authorized => {}
            ObservationUploadAuthorization::ExpiredToken => {
                anyhow::bail!("capture agent token expired")
            }
            ObservationUploadAuthorization::RevokedToken => {
                anyhow::bail!("capture agent token revoked")
            }
            ObservationUploadAuthorization::SourceInactive => {
                anyhow::bail!("capture agent is revoked")
            }
            ObservationUploadAuthorization::InvalidToken => {
                anyhow::bail!("invalid capture agent token")
            }
        }
        let observed_source_ids =
            normalize_heartbeat_observed_source_ids(record, &request.observed_source_ids)?;
        let observed_source_id_set = observed_source_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<String>>();
        let unobserved_source_ids = if observed_source_ids.is_empty() {
            Vec::new()
        } else {
            record
                .view
                .source_ids
                .iter()
                .filter(|source_id| !observed_source_id_set.contains(*source_id))
                .cloned()
                .collect::<Vec<_>>()
        };
        let agent_version = sanitized_heartbeat_agent_version(request.agent_version.as_deref());
        let previous_record = record.clone();
        let was_missing = record.view.heartbeat_state == CaptureHeartbeatState::Missing;
        record.view.last_heartbeat_at_ms = Some(heartbeat_at_ms);
        record.view.last_heartbeat_agent_version = agent_version.clone();
        record.view.last_heartbeat_observed_source_ids = observed_source_ids.clone();
        record.view.last_heartbeat_unobserved_source_ids = unobserved_source_ids.clone();
        record.view.heartbeat_deadline_ms = heartbeat_at_ms
            .saturating_add(record.view.heartbeat_interval_ms)
            .saturating_add(record.view.heartbeat_grace_ms);
        record.view.heartbeat_state = CaptureHeartbeatState::Healthy;
        record.view.heartbeat_missing_since_ms = None;
        record.view.updated_at_ms = heartbeat_at_ms;
        if let Err(error) = self.capture_agent_store.save_agent(record) {
            *record = previous_record;
            return Err(error);
        }
        let response = CaptureAgentHeartbeatResponse {
            machine_id: record.view.machine_id.clone(),
            status: record.view.status,
            heartbeat_state: record.view.heartbeat_state,
            last_heartbeat_at_ms: heartbeat_at_ms,
            heartbeat_deadline_ms: record.view.heartbeat_deadline_ms,
            agent_version,
            observed_source_ids,
            unobserved_source_ids,
        };
        drop(agents);
        self.record_audit(ObservationAuditRecord {
            recorded_at_ms: heartbeat_at_ms,
            event: if was_missing {
                "capture_agent_heartbeat_recovered"
            } else {
                "capture_agent_heartbeat"
            }
            .to_string(),
            source_id: machine_id.to_string(),
            observation_id: None,
            reason: None,
            upload_token_version: None,
            idempotency_key_sha256: None,
            request_fingerprint: None,
            media_type: None,
            byte_length: None,
            retry_after_ms: None,
            purged_asset_ids: Vec::new(),
        })
        .await;
        self.notify.notify_waiters();
        Ok(response)
    }

    /// Revokes one capture agent and disables every source it owns.
    pub(crate) async fn revoke_capture_agent(
        &self,
        machine_id: &str,
        revoked_at_ms: u64,
        reason: Option<String>,
    ) -> Result<crate::CaptureAgentView> {
        let mut agents = self.capture_agents.lock().await;
        let record = agents
            .get_mut(machine_id)
            .ok_or_else(|| anyhow!("unknown capture agent {machine_id}"))?;
        let source_ids = record
            .leases
            .iter()
            .map(|lease| lease.view.source_id.clone())
            .chain(record.view.source_ids.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let sanitized_reason = sanitized_operator_audit_reason(reason.as_deref());
        let previous_agent = record.clone();
        let first_revocation = record.view.status != CaptureAgentStatus::Revoked;
        let mut agent_changed = false;
        if record.view.status != CaptureAgentStatus::Revoked {
            record.view.status = CaptureAgentStatus::Revoked;
            agent_changed = true;
        }
        if record.view.heartbeat_state != CaptureHeartbeatState::Revoked {
            record.view.heartbeat_state = CaptureHeartbeatState::Revoked;
            agent_changed = true;
        }
        if record.view.revoked_at_ms.is_none() {
            record.view.revoked_at_ms = Some(revoked_at_ms);
            agent_changed = true;
        }
        if first_revocation {
            record.view.revoked_reason = sanitized_reason.clone();
            agent_changed = true;
        } else if record.view.revoked_reason.is_none() && sanitized_reason.is_some() {
            record.view.revoked_reason = sanitized_reason.clone();
            agent_changed = true;
        }
        let effective_revoked_at_ms = record.view.revoked_at_ms.unwrap_or(revoked_at_ms);
        for lease in &mut record.leases {
            if lease.view.revoked_at_ms.is_none() {
                lease.view.revoked_at_ms = Some(effective_revoked_at_ms);
                agent_changed = true;
            }
        }
        record.view.leases = record
            .leases
            .iter()
            .map(|lease| lease.view.clone())
            .collect();
        if agent_changed {
            record.view.updated_at_ms = revoked_at_ms;
            if let Err(error) = self.capture_agent_store.save_agent(record) {
                *record = previous_agent;
                return Err(error);
            }
        }
        let view = record.view.clone();
        drop(agents);

        let mut sources = self.sources.lock().await;
        let mut changed_sources = Vec::new();
        for source_id in &source_ids {
            let Some(source) = sources.get_mut(source_id) else {
                continue;
            };
            if source.view.status == ObservationSourceStatus::Disabled {
                continue;
            }
            let previous = source.clone();
            source.view.status = ObservationSourceStatus::Disabled;
            source.view.updated_at_ms = revoked_at_ms;
            if let Err(error) = self.store.save_source(source) {
                *source = previous;
                return Err(error);
            }
            changed_sources.push(source_id.clone());
        }
        drop(sources);

        if first_revocation || !changed_sources.is_empty() || agent_changed {
            self.record_audit(ObservationAuditRecord {
                recorded_at_ms: revoked_at_ms,
                event: if first_revocation {
                    "capture_agent_revoked"
                } else {
                    "capture_agent_revoke_repaired"
                }
                .to_string(),
                source_id: machine_id.to_string(),
                observation_id: None,
                reason: sanitized_reason,
                upload_token_version: None,
                idempotency_key_sha256: None,
                request_fingerprint: None,
                media_type: None,
                byte_length: None,
                retry_after_ms: None,
                purged_asset_ids: changed_sources,
            })
            .await;
            self.notify.notify_waiters();
        }
        Ok(view)
    }

    /// Authorizes one source-scoped upload token.
    pub(crate) async fn authorize_upload_token(
        &self,
        source_id: &str,
        digest: &[u8; 32],
        now_ms: u64,
    ) -> ObservationUploadAuthorization {
        let Some(record) = self.source_record(source_id).await else {
            return ObservationUploadAuthorization::InvalidToken;
        };
        if !record.view.status.accepts_ingest() {
            return ObservationUploadAuthorization::SourceInactive;
        }

        let agents = self.capture_agents.lock().await;
        if let Some(agent) = agents.values().find(|agent| {
            agent
                .leases
                .iter()
                .any(|lease| lease.view.source_id == source_id)
        }) {
            return authorize_capture_agent_lease(agent, Some(source_id), digest, now_ms);
        }
        drop(agents);

        if record.capture_owner_machine_id.is_some() {
            return ObservationUploadAuthorization::SourceInactive;
        }

        let Ok(expected) = hex::decode(record.upload_token_sha256) else {
            return ObservationUploadAuthorization::InvalidToken;
        };
        if expected.len() != 32 {
            return ObservationUploadAuthorization::InvalidToken;
        }
        if expected.as_slice().ct_eq(digest.as_slice()).into() {
            return if record.view.upload_token_revoked_at_ms.is_some() {
                ObservationUploadAuthorization::RevokedToken
            } else {
                ObservationUploadAuthorization::Authorized
            };
        }
        for token in &record.previous_upload_tokens {
            let Ok(expected) = hex::decode(&token.sha256) else {
                continue;
            };
            if expected.len() != 32 || !bool::from(expected.as_slice().ct_eq(digest.as_slice())) {
                continue;
            }
            if token.revoked_at_ms.is_some() {
                return ObservationUploadAuthorization::RevokedToken;
            }
            if now_ms > token.expires_at_ms {
                return ObservationUploadAuthorization::ExpiredToken;
            }
            return ObservationUploadAuthorization::Authorized;
        }
        ObservationUploadAuthorization::InvalidToken
    }

    /// Applies the source-scoped upload rate limit after bearer authentication succeeds.
    pub(crate) async fn reserve_ingest_slot(
        &self,
        source_id: &str,
        now_ms: u64,
    ) -> Result<ObservationIngressRateLimitDecision> {
        let source = self.get_source(source_id).await?;
        let window_ms = source.ingest_rate_limit_window_ms;
        let burst = source.ingest_rate_limit_burst;
        let mut limits = self.ingress_rate_limits.lock().await;
        let state = limits.entry(source_id.to_string()).or_insert_with(|| {
            ObservationIngressRateLimitState {
                // A previously unseen source starts with a full bucket, preserving the prior
                // behavior where a fresh source could immediately spend its whole burst.
                tokens: burst.max(1) as f64,
                last_refill_ms: now_ms,
            }
        });
        Ok(state.admit(now_ms, window_ms, burst))
    }

    /// Appends one sanitized audit record. Audit persistence failures are logged but non-fatal.
    pub(crate) async fn record_audit(&self, record: ObservationAuditRecord) {
        let _guard = self.audit_lock.lock().await;
        if let Err(error) = self.store.append_audit(&record) {
            warn!(
                source_id = %record.source_id,
                event = %record.event,
                error = %error,
                "failed to append observation audit record"
            );
        }
    }

    /// Lists sanitized observation audit records newest first, with bounded filtering.
    pub(crate) async fn list_audit_records(
        &self,
        source_id: Option<&str>,
        event: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ObservationAuditRecord>> {
        let limit = limit.clamp(1, 1_000);
        let source_id = source_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string);
        let event = event
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string);
        let mut records = self.store.load_audit_records()?;
        records.retain(|record| {
            source_id
                .as_ref()
                .is_none_or(|source_id| &record.source_id == source_id)
                && event.as_ref().is_none_or(|event| &record.event == event)
        });
        records.sort_by(|left, right| {
            right
                .recorded_at_ms
                .cmp(&left.recorded_at_ms)
                .then_with(|| right.event.cmp(&left.event))
        });
        records.truncate(limit);
        Ok(records)
    }

    /// Persists and registers one new observation, then enforces source retention limits.
    pub(crate) async fn create_observation(
        &self,
        record: ObservationView,
        protected_asset_ids: BTreeSet<String>,
    ) -> Result<ObservationView> {
        let source_id = record.source_id.clone();
        {
            let mut observations = self.observations.lock().await;
            anyhow::ensure!(
                !observations.contains_key(&record.observation_id),
                "observation {} already exists",
                record.observation_id
            );
            self.store.save_observation(&record)?;
            observations.insert(record.observation_id.clone(), record.clone());
        }
        self.refresh_source_counters(&source_id).await?;
        self.enforce_source_retention(&source_id, &protected_asset_ids)
            .await?;
        self.notify.notify_waiters();
        Ok(self.get_observation(&record.observation_id).await?)
    }

    /// Enforces source retention policies for every loaded observation source.
    pub(crate) async fn enforce_retention(
        &self,
        protected_asset_ids: &BTreeSet<String>,
    ) -> Result<()> {
        let source_ids = self
            .sources
            .lock()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for source_id in source_ids {
            self.enforce_source_retention(&source_id, protected_asset_ids)
                .await?;
        }
        Ok(())
    }

    /// Returns one observation by identifier.
    pub(crate) async fn get_observation(&self, observation_id: &str) -> Result<ObservationView> {
        self.observations
            .lock()
            .await
            .get(observation_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown observation {observation_id}"))
    }

    /// Attaches one canonical text asset to an existing observation.
    pub(crate) async fn set_canonical_text_asset(
        &self,
        observation_id: &str,
        canonical_text_asset_id: &str,
    ) -> Result<ObservationView> {
        let mut observations = self.observations.lock().await;
        let record = observations
            .get_mut(observation_id)
            .ok_or_else(|| anyhow!("unknown observation {observation_id}"))?;
        if record.canonical_text_asset_id.as_deref() == Some(canonical_text_asset_id) {
            return Ok(record.clone());
        }
        let previous = record.clone();
        record.canonical_text_asset_id = Some(canonical_text_asset_id.to_string());
        if let Err(error) = self.store.save_observation(record) {
            *record = previous;
            return Err(error);
        }
        let updated = record.clone();
        drop(observations);
        self.notify.notify_waiters();
        Ok(updated)
    }

    /// Returns one existing observation for the provided source/idempotency pair.
    pub(crate) async fn find_by_ingest_key(
        &self,
        source_id: &str,
        idempotency_key: &str,
        request_fingerprint: &str,
    ) -> Result<Option<ObservationView>> {
        let observation = self
            .observations
            .lock()
            .await
            .values()
            .find(|view| {
                view.source_id == source_id && view.idempotency_key == idempotency_key.trim()
            })
            .cloned();
        if let Some(observation) = observation {
            anyhow::ensure!(
                observation.request_fingerprint == request_fingerprint,
                "observation ingest key observation:{source_id}:{idempotency_key} was reused with a different payload"
            );
            return Ok(Some(observation));
        }
        Ok(None)
    }

    /// Lists observations using one optional source and capture-time filter.
    pub(crate) async fn list_observations(
        &self,
        source_id: Option<&str>,
        stream_id: Option<&str>,
        after_ms: Option<u64>,
        before_ms: Option<u64>,
        include_purged: bool,
    ) -> Vec<ObservationView> {
        let mut views = self
            .observations
            .lock()
            .await
            .values()
            .filter(|view| {
                source_id.is_none_or(|source_id| view.source_id == source_id)
                    && stream_id
                        .is_none_or(|stream_id| view.stream_id.as_deref() == Some(stream_id))
                    && after_ms.is_none_or(|after_ms| view.captured_at_ms >= after_ms)
                    && before_ms.is_none_or(|before_ms| view.captured_at_ms <= before_ms)
                    && (include_purged || view.retention_state == ObservationRetentionState::Active)
            })
            .cloned()
            .collect::<Vec<_>>();
        views.sort_by(|left, right| {
            left.captured_at_ms
                .cmp(&right.captured_at_ms)
                .then_with(|| left.observation_id.cmp(&right.observation_id))
        });
        views
    }

    /// Resolves one observation materialization selection at execution time.
    pub(crate) async fn resolve_selection(
        &self,
        request: &ObservationMaterializationRequest,
        execution_time_ms: u64,
    ) -> Result<Vec<ObservationView>> {
        let observations = self.observations.lock().await;
        let resolved = match &request.selection {
            ObservationSelection::ObservationIds { observation_ids } => {
                let mut selected = Vec::new();
                for observation_id in observation_ids {
                    let observation = observations
                        .get(observation_id)
                        .cloned()
                        .ok_or_else(|| anyhow!("unknown observation {observation_id}"))?;
                    anyhow::ensure!(
                        observation.retention_state == ObservationRetentionState::Active,
                        "observation {observation_id} is no longer materializable"
                    );
                    selected.push(observation);
                }
                selected
            }
            ObservationSelection::ObservationGroup {
                capture_group_id,
                max_observations,
                lookback_seconds,
            } => {
                let earliest_captured_at_ms = lookback_seconds
                    .map(|seconds| execution_time_ms.saturating_sub(seconds.saturating_mul(1000)));
                let mut selected = observations
                    .values()
                    .filter(|view| {
                        view.retention_state == ObservationRetentionState::Active
                            && observation_capture_group_id(view) == Some(capture_group_id.as_str())
                            && earliest_captured_at_ms
                                .is_none_or(|start_at_ms| view.captured_at_ms >= start_at_ms)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                selected.sort_by(|left, right| {
                    right
                        .captured_at_ms
                        .cmp(&left.captured_at_ms)
                        .then_with(|| right.observation_id.cmp(&left.observation_id))
                });
                selected.truncate(*max_observations as usize);
                selected.sort_by(|left, right| {
                    left.captured_at_ms
                        .cmp(&right.captured_at_ms)
                        .then_with(|| left.observation_id.cmp(&right.observation_id))
                });
                selected
            }
            ObservationSelection::LatestFromSource {
                source_id,
                max_observations,
                lookback_seconds,
            } => {
                let earliest_captured_at_ms = lookback_seconds
                    .map(|seconds| execution_time_ms.saturating_sub(seconds.saturating_mul(1000)));
                let mut selected = observations
                    .values()
                    .filter(|view| {
                        view.source_id == *source_id
                            && view.retention_state == ObservationRetentionState::Active
                            && earliest_captured_at_ms
                                .is_none_or(|start_at_ms| view.captured_at_ms >= start_at_ms)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                selected.sort_by(|left, right| {
                    right
                        .captured_at_ms
                        .cmp(&left.captured_at_ms)
                        .then_with(|| right.observation_id.cmp(&left.observation_id))
                });
                selected.truncate(*max_observations as usize);
                selected.sort_by(|left, right| {
                    left.captured_at_ms
                        .cmp(&right.captured_at_ms)
                        .then_with(|| left.observation_id.cmp(&right.observation_id))
                });
                selected
            }
            ObservationSelection::LatestFromStream {
                source_id,
                stream_id,
                max_observations,
                lookback_seconds,
            } => {
                let earliest_captured_at_ms = lookback_seconds
                    .map(|seconds| execution_time_ms.saturating_sub(seconds.saturating_mul(1000)));
                let mut selected = observations
                    .values()
                    .filter(|view| {
                        view.source_id == *source_id
                            && view.stream_id.as_deref() == Some(stream_id.as_str())
                            && view.retention_state == ObservationRetentionState::Active
                            && earliest_captured_at_ms
                                .is_none_or(|start_at_ms| view.captured_at_ms >= start_at_ms)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                selected.sort_by(|left, right| {
                    right
                        .captured_at_ms
                        .cmp(&left.captured_at_ms)
                        .then_with(|| right.observation_id.cmp(&left.observation_id))
                });
                selected.truncate(*max_observations as usize);
                selected.sort_by(|left, right| {
                    left.captured_at_ms
                        .cmp(&right.captured_at_ms)
                        .then_with(|| left.observation_id.cmp(&right.observation_id))
                });
                selected
            }
        };
        drop(observations);
        if request.fail_when_empty && resolved.is_empty() {
            return Err(anyhow!(
                "observation selection did not resolve any active records"
            ));
        }
        Ok(resolved)
    }

    /// Resolves one transcript selection without applying a latest-N materialization cap.
    pub(crate) async fn resolve_transcript_selection(
        &self,
        selection: &ObservationTranscriptSelection,
    ) -> Vec<ObservationView> {
        let mut selected = self
            .observations
            .lock()
            .await
            .values()
            .filter(|view| selection.matches(view))
            .cloned()
            .collect::<Vec<_>>();
        selected.sort_by(|left, right| {
            left.captured_at_ms
                .cmp(&right.captured_at_ms)
                .then_with(|| left.observation_id.cmp(&right.observation_id))
        });
        selected
    }

    /// Returns every source selected by one observation materialization request.
    pub(crate) async fn selection_sources(
        &self,
        request: &ObservationMaterializationRequest,
    ) -> Result<Vec<ObservationSourceRecord>> {
        let source_ids = match &request.selection {
            ObservationSelection::LatestFromSource { source_id, .. }
            | ObservationSelection::LatestFromStream { source_id, .. } => {
                vec![source_id.clone()]
            }
            ObservationSelection::ObservationIds { observation_ids } => {
                let observations = self.observations.lock().await;
                let mut source_ids = BTreeMap::<String, ()>::new();
                for observation_id in observation_ids {
                    let view = observations
                        .get(observation_id)
                        .ok_or_else(|| anyhow!("unknown observation {observation_id}"))?;
                    source_ids.insert(view.source_id.clone(), ());
                }
                source_ids.into_keys().collect()
            }
            ObservationSelection::ObservationGroup {
                capture_group_id,
                lookback_seconds,
                ..
            } => {
                let earliest_captured_at_ms = lookback_seconds
                    .map(|seconds| now_ms().saturating_sub(seconds.saturating_mul(1000)));
                let observations = self.observations.lock().await;
                let mut source_ids = BTreeMap::<String, ()>::new();
                for view in observations.values().filter(|view| {
                    view.retention_state == ObservationRetentionState::Active
                        && observation_capture_group_id(view) == Some(capture_group_id.as_str())
                        && earliest_captured_at_ms
                            .is_none_or(|start_at_ms| view.captured_at_ms >= start_at_ms)
                }) {
                    source_ids.insert(view.source_id.clone(), ());
                }
                source_ids.into_keys().collect()
            }
        };
        let sources = self.sources.lock().await;
        let mut records = Vec::new();
        for source_id in source_ids {
            let source = sources
                .get(&source_id)
                .cloned()
                .ok_or_else(|| anyhow!("unknown observation source {source_id}"))?;
            records.push(source);
        }
        Ok(records)
    }

    async fn enforce_source_retention(
        &self,
        source_id: &str,
        protected_asset_ids: &BTreeSet<String>,
    ) -> Result<()> {
        let Some(source) = self.source_record(source_id).await else {
            return Ok(());
        };
        let now = now_ms();
        let oldest_allowed_ms =
            now.saturating_sub(source.view.retention_seconds.saturating_mul(1000));

        let mut observations = self.observations.lock().await;
        let mut active = observations
            .values()
            .filter(|view| {
                view.source_id == source_id
                    && view.retention_state == ObservationRetentionState::Active
            })
            .map(|view| (view.captured_at_ms, view.observation_id.clone()))
            .collect::<Vec<_>>();
        active.sort_by(|left, right| left.cmp(right));

        let mut active_count = active.len() as u64;
        let mut active_bytes = observations
            .values()
            .filter(|view| {
                view.source_id == source_id
                    && view.retention_state == ObservationRetentionState::Active
            })
            .map(|view| view.byte_length)
            .sum::<u64>();
        let mut changed = false;
        let mut purged_records = Vec::new();

        for (_, observation_id) in active {
            let Some(view) = observations.get_mut(&observation_id) else {
                continue;
            };
            let should_purge = view.received_at_ms < oldest_allowed_ms
                || active_count > source.view.max_active_observations
                || active_bytes > source.view.max_active_bytes;
            if !should_purge {
                continue;
            }
            if view.retention_state == ObservationRetentionState::Purged {
                continue;
            }
            let previous = view.clone();
            view.retention_state = ObservationRetentionState::Purged;
            if let Err(error) = self.store.save_observation(view) {
                *view = previous;
                return Err(error);
            }
            active_count = active_count.saturating_sub(1);
            active_bytes = active_bytes.saturating_sub(view.byte_length);
            purged_records.push(view.clone());
            changed = true;
        }
        let mut asset_ids_to_purge = BTreeSet::new();
        if source.view.purge_raw_on_retention {
            for purged in &purged_records {
                for asset_id in observation_asset_ids(purged) {
                    let still_active = observations.values().any(|view| {
                        view.retention_state == ObservationRetentionState::Active
                            && observation_references_asset(view, &asset_id)
                    });
                    if !still_active && !protected_asset_ids.contains(&asset_id) {
                        asset_ids_to_purge.insert(asset_id);
                    }
                }
            }
        }
        drop(observations);
        let mut purged_asset_ids = Vec::new();
        for asset_id in asset_ids_to_purge {
            if self.assets.delete_asset(&asset_id)? {
                purged_asset_ids.push(asset_id);
            }
        }
        if !purged_asset_ids.is_empty() {
            self.record_audit(ObservationAuditRecord {
                recorded_at_ms: now_ms(),
                event: "retention_purged_assets".to_string(),
                source_id: source_id.to_string(),
                observation_id: None,
                reason: Some("retention".to_string()),
                upload_token_version: Some(source.view.upload_token_version),
                idempotency_key_sha256: None,
                request_fingerprint: None,
                media_type: None,
                byte_length: None,
                retry_after_ms: None,
                purged_asset_ids,
            })
            .await;
        }
        if changed {
            self.refresh_source_counters(source_id).await?;
        }
        Ok(())
    }

    async fn refresh_source_counters(&self, source_id: &str) -> Result<()> {
        let observations = self.observations.lock().await;
        let active_count = observations
            .values()
            .filter(|view| {
                view.source_id == source_id
                    && view.retention_state == ObservationRetentionState::Active
            })
            .count() as u64;
        let active_byte_length = observations
            .values()
            .filter(|view| {
                view.source_id == source_id
                    && view.retention_state == ObservationRetentionState::Active
            })
            .map(|view| view.byte_length)
            .sum::<u64>();
        let last_observed_at_ms = observations
            .values()
            .filter(|view| view.source_id == source_id)
            .map(|view| view.received_at_ms)
            .max();
        drop(observations);

        let mut sources = self.sources.lock().await;
        let Some(record) = sources.get_mut(source_id) else {
            return Ok(());
        };
        let next_view = {
            let mut view = record.view.clone();
            view.active_observation_count = active_count;
            view.active_byte_length = active_byte_length;
            view.last_observed_at_ms = last_observed_at_ms;
            view.updated_at_ms = now_ms();
            view
        };
        if next_view == record.view {
            return Ok(());
        }
        let previous = record.clone();
        record.view = next_view;
        if let Err(error) = self.store.save_source(record) {
            *record = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Builds one new source record from the public create request.
    pub(crate) fn build_source_record(
        &self,
        request: CreateObservationSourceRequest,
    ) -> Result<ObservationSourceRecord> {
        request.validate()?;
        let now = now_ms();
        let source_id = request
            .source_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| self.next_source_id());
        Ok(ObservationSourceRecord {
            view: ObservationSourceView {
                source_id,
                display_name: request.display_name.trim().to_string(),
                kind: request.kind,
                status: ObservationSourceStatus::Active,
                sensitivity: request.sensitivity,
                retention_seconds: request.retention_seconds,
                max_active_observations: request.max_active_observations,
                max_active_bytes: request.max_active_bytes,
                ingest_rate_limit_window_ms: request.ingest_rate_limit_window_ms,
                ingest_rate_limit_burst: request.ingest_rate_limit_burst,
                purge_raw_on_retention: request.purge_raw_on_retention,
                allow_materialization: request.allow_materialization,
                allow_output_delivery: request.allow_output_delivery,
                created_at_ms: now,
                updated_at_ms: now,
                last_authenticated_at_ms: None,
                upload_token_version: 1,
                last_token_rotated_at_ms: Some(now),
                upload_token_revoked_at_ms: None,
                last_observed_at_ms: None,
                active_observation_count: 0,
                active_byte_length: 0,
            },
            upload_token_sha256: digest_source_upload_token(&request.upload_token),
            previous_upload_tokens: Vec::new(),
            capture_owner_machine_id: None,
            capture_lease_expires_at_ms: None,
        })
    }
}

fn digest_source_upload_token(token: &str) -> String {
    hex::encode(sha2::Sha256::digest(token.trim().as_bytes()))
}

fn observation_capture_group_id(view: &ObservationView) -> Option<&str> {
    view.metadata
        .get("capture_group_id")
        .and_then(|value| value.as_str())
}

fn merge_capture_agent_record(
    existing: CaptureAgentRecord,
    mut incoming: CaptureAgentRecord,
    now_ms: u64,
) -> CaptureAgentRecord {
    let new_lease_by_source = incoming
        .leases
        .iter()
        .map(|lease| (lease.view.source_id.clone(), lease.view.lease_id.clone()))
        .collect::<BTreeMap<_, _>>();

    let mut leases = existing.leases;
    for lease in &mut leases {
        if lease.view.revoked_at_ms.is_none() {
            lease.view.revoked_at_ms = Some(now_ms);
            lease.view.superseded_by = new_lease_by_source.get(&lease.view.source_id).cloned();
        }
    }
    leases.extend(incoming.leases);
    incoming.leases = leases;
    incoming.view.created_at_ms = existing.view.created_at_ms;
    incoming.view.heartbeat_token_version = existing
        .view
        .heartbeat_token_version
        .saturating_add(1)
        .max(1);
    incoming.view.last_heartbeat_at_ms = if existing.view.status == CaptureAgentStatus::Active {
        existing.view.last_heartbeat_at_ms
    } else {
        None
    };
    incoming.view.last_heartbeat_agent_version =
        if existing.view.status == CaptureAgentStatus::Active {
            existing.view.last_heartbeat_agent_version.clone()
        } else {
            None
        };
    incoming.view.last_heartbeat_observed_source_ids =
        if existing.view.status == CaptureAgentStatus::Active {
            existing.view.last_heartbeat_observed_source_ids.clone()
        } else {
            Vec::new()
        };
    incoming.view.last_heartbeat_unobserved_source_ids =
        if existing.view.status == CaptureAgentStatus::Active {
            existing.view.last_heartbeat_unobserved_source_ids.clone()
        } else {
            Vec::new()
        };
    incoming.view.heartbeat_state = match (existing.view.status, existing.view.heartbeat_state) {
        (CaptureAgentStatus::Active, CaptureHeartbeatState::Healthy) => {
            CaptureHeartbeatState::Healthy
        }
        (CaptureAgentStatus::Active, CaptureHeartbeatState::Missing) => {
            CaptureHeartbeatState::Missing
        }
        (CaptureAgentStatus::Active, _) => CaptureHeartbeatState::Pending,
        (CaptureAgentStatus::Revoked, _) => CaptureHeartbeatState::Pending,
    };
    incoming.view.heartbeat_deadline_ms = existing
        .view
        .last_heartbeat_at_ms
        .filter(|_| existing.view.status == CaptureAgentStatus::Active)
        .map(|last_heartbeat| {
            last_heartbeat
                .saturating_add(incoming.view.heartbeat_interval_ms)
                .saturating_add(incoming.view.heartbeat_grace_ms)
        })
        .unwrap_or(incoming.view.heartbeat_deadline_ms);
    incoming.view.heartbeat_missing_since_ms = if existing.view.status == CaptureAgentStatus::Active
        && existing.view.heartbeat_state == CaptureHeartbeatState::Missing
    {
        existing.view.heartbeat_missing_since_ms
    } else {
        None
    };
    incoming.view.status = CaptureAgentStatus::Active;
    incoming.view.revoked_at_ms = None;
    incoming.view.revoked_reason = None;
    incoming.view.updated_at_ms = now_ms;
    incoming.view.leases = incoming
        .leases
        .iter()
        .map(|lease| lease.view.clone())
        .collect();
    incoming
}

fn normalize_heartbeat_observed_source_ids(
    agent: &CaptureAgentRecord,
    source_ids: &[String],
) -> Result<Vec<String>> {
    let owned_source_ids = agent
        .view
        .source_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<String>>();
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::with_capacity(source_ids.len());
    for source_id in source_ids {
        let source_id = source_id.trim();
        anyhow::ensure!(
            !source_id.is_empty(),
            "heartbeat observed_source_ids must not contain empty source ids"
        );
        anyhow::ensure!(
            owned_source_ids.contains(source_id),
            "heartbeat observed_source_ids contains a source not owned by capture agent"
        );
        anyhow::ensure!(
            seen.insert(source_id.to_string()),
            "heartbeat observed_source_ids contains duplicate source ids"
        );
        normalized.push(source_id.to_string());
    }
    Ok(normalized)
}

fn sanitized_heartbeat_agent_version(value: Option<&str>) -> Option<String> {
    let normalized = value?
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty() {
        return None;
    }
    let redacted = redact_text(&normalized);
    if redacted != normalized {
        return Some("agent_version_redacted".to_string());
    }
    let mut output = String::new();
    for ch in normalized.chars().take(128) {
        if ch.is_control() {
            continue;
        }
        output.push(ch);
    }
    if output.is_empty() {
        None
    } else {
        Some(output)
    }
}

fn authorize_capture_agent_heartbeat_token(
    agent: &CaptureAgentRecord,
    digest: &[u8; 32],
    now_ms: u64,
) -> ObservationUploadAuthorization {
    if agent.view.status != CaptureAgentStatus::Active {
        return ObservationUploadAuthorization::SourceInactive;
    }
    let Ok(expected) = hex::decode(&agent.heartbeat_token_sha256) else {
        return ObservationUploadAuthorization::InvalidToken;
    };
    if expected.len() != 32 || !bool::from(expected.as_slice().ct_eq(digest.as_slice())) {
        return ObservationUploadAuthorization::InvalidToken;
    }
    if agent
        .view
        .token_expires_at_ms
        .is_some_and(|expires_at_ms| now_ms >= expires_at_ms)
    {
        return ObservationUploadAuthorization::ExpiredToken;
    }
    ObservationUploadAuthorization::Authorized
}

fn authorize_capture_agent_lease(
    agent: &CaptureAgentRecord,
    source_id: Option<&str>,
    digest: &[u8; 32],
    now_ms: u64,
) -> ObservationUploadAuthorization {
    if agent.view.status != CaptureAgentStatus::Active {
        return ObservationUploadAuthorization::RevokedToken;
    }
    let mut matched_revoked = false;
    let mut matched_expired = false;
    for lease in &agent.leases {
        if source_id.is_some_and(|source_id| lease.view.source_id != source_id) {
            continue;
        }
        let Ok(expected) = hex::decode(&lease.upload_token_sha256) else {
            continue;
        };
        if expected.len() != 32 || !bool::from(expected.as_slice().ct_eq(digest.as_slice())) {
            continue;
        }
        if lease.view.revoked_at_ms.is_some() || lease.view.superseded_by.is_some() {
            matched_revoked = true;
            continue;
        }
        if now_ms >= lease.view.expires_at_ms {
            matched_expired = true;
            continue;
        }
        return ObservationUploadAuthorization::Authorized;
    }
    if matched_revoked {
        ObservationUploadAuthorization::RevokedToken
    } else if matched_expired {
        ObservationUploadAuthorization::ExpiredToken
    } else {
        ObservationUploadAuthorization::InvalidToken
    }
}

fn observation_asset_ids(view: &ObservationView) -> Vec<String> {
    let mut asset_ids = vec![view.asset_id.clone()];
    if let Some(canonical_text_asset_id) = view.canonical_text_asset_id.as_ref()
        && canonical_text_asset_id != &view.asset_id
    {
        asset_ids.push(canonical_text_asset_id.clone());
    }
    asset_ids
}

fn observation_references_asset(view: &ObservationView, asset_id: &str) -> bool {
    view.asset_id == asset_id || view.canonical_text_asset_id.as_deref() == Some(asset_id)
}

fn sanitized_operator_audit_reason(reason: Option<&str>) -> Option<String> {
    let normalized = reason?
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty() {
        return None;
    }
    if normalized.chars().count() > 120 || audit_reason_looks_secret_like(&normalized) {
        return Some("operator_reason_redacted".to_string());
    }
    let redacted = redact_text(&normalized);
    if redacted != normalized {
        Some("operator_reason_redacted".to_string())
    } else {
        Some(normalized)
    }
}

fn audit_reason_looks_secret_like(reason: &str) -> bool {
    let lowered = reason.to_ascii_lowercase();
    [
        "authorization",
        "api key",
        "api-key",
        "apikey",
        "api_key",
        "bearer",
        "client secret",
        "client-secret",
        "clientsecret",
        "client_secret",
        "credential",
        "password",
        "pat:",
        "personal access",
        "private key",
        "private-key",
        "privatekey",
        "private_key",
        "refresh",
        "secret",
        "secret-token",
        "secret_token",
        "sk-",
        "token",
        "x api key",
        "x-api-key",
        "xapikey",
        "x_api_key",
    ]
    .iter()
    .any(|marker| lowered.contains(marker))
}

#[cfg(test)]
mod rate_limit_tests {
    use super::{ObservationIngressRateLimitDecision, ObservationIngressRateLimitState};

    fn fresh(burst: u64, now_ms: u64) -> ObservationIngressRateLimitState {
        ObservationIngressRateLimitState {
            tokens: burst.max(1) as f64,
            last_refill_ms: now_ms,
        }
    }

    fn drain(
        state: &mut ObservationIngressRateLimitState,
        now_ms: u64,
        window_ms: u64,
        burst: u64,
    ) {
        for _ in 0..burst {
            assert_eq!(
                state.admit(now_ms, window_ms, burst),
                ObservationIngressRateLimitDecision::Accepted,
            );
        }
    }

    #[test]
    fn admits_full_burst_then_limits() {
        let (window_ms, burst) = (60_000, 3);
        let mut state = fresh(burst, 0);
        drain(&mut state, 0, window_ms, burst);
        assert!(matches!(
            state.admit(0, window_ms, burst),
            ObservationIngressRateLimitDecision::Limited { .. }
        ));
    }

    #[test]
    fn refills_one_token_after_one_proportional_period() {
        // burst=6 over 60_000ms => one token every 10_000ms.
        let (window_ms, burst) = (60_000, 6);
        let mut state = fresh(burst, 0);
        drain(&mut state, 0, window_ms, burst);
        // Exactly one refill period later, exactly one more token is available, then limited.
        assert_eq!(
            state.admit(10_000, window_ms, burst),
            ObservationIngressRateLimitDecision::Accepted
        );
        assert!(matches!(
            state.admit(10_000, window_ms, burst),
            ObservationIngressRateLimitDecision::Limited { .. }
        ));
    }

    #[test]
    fn no_instantaneous_double_burst_across_window_boundary() {
        // Regression for the previous fixed-window counter, which reset at the window edge and could
        // admit ~2x `burst` within a tiny interval straddling the boundary. With a token bucket the
        // capacity caps any single-instant burst at `burst`: draining at the end of one window and
        // retrying right after the boundary must not yield a second full burst.
        let (window_ms, burst) = (60_000, 4);
        let mut state = fresh(burst, 0);
        let edge = window_ms - 1;
        let mut admitted_at_edge = 0u64;
        while let ObservationIngressRateLimitDecision::Accepted =
            state.admit(edge, window_ms, burst)
        {
            admitted_at_edge += 1;
            assert!(
                admitted_at_edge <= burst,
                "instantaneous burst exceeded capacity at the window edge"
            );
        }
        // 1ms past the boundary only a negligible fraction of a token has refilled.
        let mut admitted_after_boundary = 0u64;
        while let ObservationIngressRateLimitDecision::Accepted =
            state.admit(window_ms, window_ms, burst)
        {
            admitted_after_boundary += 1;
            assert!(
                admitted_after_boundary < burst,
                "a second full burst was admitted right after the window boundary (fixed-window 2x bug)"
            );
        }
    }

    #[test]
    fn full_window_idle_restores_exactly_one_burst() {
        let (window_ms, burst) = (60_000, 4);
        let mut state = fresh(burst, 0);
        drain(&mut state, 0, window_ms, burst);
        // After a full idle window the bucket is full again — a fresh burst, but not more.
        drain(&mut state, window_ms, window_ms, burst);
        assert!(matches!(
            state.admit(window_ms, window_ms, burst),
            ObservationIngressRateLimitDecision::Limited { .. }
        ));
    }

    #[test]
    fn limited_retry_after_is_positive_and_bounded_by_window() {
        let (window_ms, burst) = (60_000, 1);
        let mut state = fresh(burst, 0);
        assert_eq!(
            state.admit(0, window_ms, burst),
            ObservationIngressRateLimitDecision::Accepted
        );
        match state.admit(0, window_ms, burst) {
            ObservationIngressRateLimitDecision::Limited { retry_after_ms } => {
                assert!(retry_after_ms > 0);
                assert!(
                    retry_after_ms <= window_ms,
                    "retry_after {retry_after_ms} should not exceed a full window {window_ms}"
                );
            }
            other => panic!("expected limited, got {other:?}"),
        }
    }

    #[test]
    fn clock_skew_backwards_does_not_overflow_or_refill() {
        let (window_ms, burst) = (60_000, 2);
        let mut state = fresh(burst, 10_000);
        drain(&mut state, 10_000, window_ms, burst);
        // A timestamp earlier than the last refill must not refill (saturating elapsed) or panic.
        assert!(matches!(
            state.admit(0, window_ms, burst),
            ObservationIngressRateLimitDecision::Limited { .. }
        ));
    }

    #[test]
    fn zero_config_is_clamped_not_dividing_by_zero() {
        // Defensive: validation guarantees non-zero, but a malformed/legacy record must clamp to
        // 1/1 rather than produce NaN/inf or panic.
        let mut state = ObservationIngressRateLimitState {
            tokens: 1.0,
            last_refill_ms: 0,
        };
        assert_eq!(
            state.admit(0, 0, 0),
            ObservationIngressRateLimitDecision::Accepted
        );
        match state.admit(0, 0, 0) {
            ObservationIngressRateLimitDecision::Limited { retry_after_ms } => {
                assert!(retry_after_ms > 0);
            }
            other => panic!("expected limited, got {other:?}"),
        }
    }
}
