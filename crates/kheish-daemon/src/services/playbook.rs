//! Playbook catalog and Flow record service.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};
use tokio::sync::Mutex;

use crate::playbooks::{
    AppendFlowEvidenceRequest, CreatePlaybookRequest, FilePlaybookStore, FlowListQuery, FlowRecord,
    FlowStartReservation, PlaybookListQuery, PlaybookManifest, PlaybookRecord,
    PlaybookReleaseRecord, PlaybookReleaseStatus, PlaybookRuntimeDefaults, PlaybookVersionRef,
    PlaybookView, PublishPlaybookRequest, RevokePlaybookRequest, StartFlowRequest,
    ValidatePlaybookRequest, ensure_control_identifier, flow_input_digest,
    playbook_manifest_digest, record_to_view, require_matching_version, validate_playbook_manifest,
};
use crate::runs::now_ms;

#[derive(Clone, Debug, Default)]
struct PlaybookState {
    playbooks: BTreeMap<String, PlaybookRecord>,
    flows: BTreeMap<String, FlowRecord>,
    flows_by_idempotency_key: BTreeMap<String, String>,
    flow_start_in_progress: BTreeSet<String>,
}

impl PlaybookState {
    fn new(
        playbooks: BTreeMap<String, PlaybookRecord>,
        flows: BTreeMap<String, FlowRecord>,
    ) -> Self {
        let mut state = Self {
            playbooks,
            flows,
            flows_by_idempotency_key: BTreeMap::new(),
            flow_start_in_progress: BTreeSet::new(),
        };
        state.rebuild_indexes();
        state
    }

    fn rebuild_indexes(&mut self) {
        self.flows_by_idempotency_key.clear();
        for flow in self.flows.values() {
            if let Some(key) = flow.idempotency_key.as_deref() {
                self.flows_by_idempotency_key
                    .insert(key.to_string(), flow.flow_id.clone());
            }
        }
    }
}

/// Owns durable Playbook definitions and Flow correlation records.
pub(crate) struct PlaybookService {
    store: FilePlaybookStore,
    state: Mutex<PlaybookState>,
    next_flow_id: AtomicU64,
}

impl PlaybookService {
    /// Creates a new service backed by persisted daemon state.
    pub(crate) fn new(
        store: FilePlaybookStore,
        playbooks: BTreeMap<String, PlaybookRecord>,
        flows: BTreeMap<String, FlowRecord>,
        next_flow_id: AtomicU64,
    ) -> Self {
        Self {
            store,
            state: Mutex::new(PlaybookState::new(playbooks, flows)),
            next_flow_id,
        }
    }

    /// Returns one fresh daemon-managed Flow identifier.
    pub(crate) fn next_flow_id(&self) -> String {
        format!("flow-{}", self.next_flow_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Validates one Playbook manifest structurally.
    pub(crate) fn validate_playbook(
        &self,
        request: ValidatePlaybookRequest,
    ) -> crate::PlaybookValidationResult {
        validate_playbook_manifest(&request.manifest)
    }

    /// Lists Playbooks filtered by optional text/status query.
    pub(crate) async fn list_playbooks(&self, query: PlaybookListQuery) -> Vec<PlaybookView> {
        let normalized_query = query
            .query
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        let state = self.state.lock().await;
        let mut playbooks = state
            .playbooks
            .values()
            .filter(|record| {
                query.status.as_ref().is_none_or(|status| {
                    record
                        .releases
                        .values()
                        .any(|release| &release.status == status)
                }) && normalized_query.as_ref().is_none_or(|query| {
                    record.playbook_id.to_ascii_lowercase().contains(query)
                        || record.versions.values().any(|version| {
                            version.digest.to_ascii_lowercase().contains(query)
                                || version.manifest.title.to_ascii_lowercase().contains(query)
                                || version
                                    .manifest
                                    .objective
                                    .to_ascii_lowercase()
                                    .contains(query)
                        })
                })
            })
            .map(|record| record_to_view(record, None))
            .collect::<Vec<_>>();
        playbooks.sort_by(|left, right| left.playbook_id.cmp(&right.playbook_id));
        playbooks
    }

    /// Returns one Playbook by identifier.
    pub(crate) async fn get_playbook(
        &self,
        playbook_id: &str,
        version: Option<&str>,
    ) -> Result<PlaybookView> {
        let state = self.state.lock().await;
        let record = state
            .playbooks
            .get(playbook_id)
            .ok_or_else(|| anyhow!("unknown playbook {playbook_id}"))?;
        Ok(record_to_view(record, version))
    }

    pub(crate) async fn runtime_defaults(
        &self,
        playbook_ref: &PlaybookVersionRef,
    ) -> Result<PlaybookRuntimeDefaults> {
        Ok(self.manifest_for_ref(playbook_ref).await?.runtime_defaults)
    }

    pub(crate) async fn manifest_for_ref(
        &self,
        playbook_ref: &PlaybookVersionRef,
    ) -> Result<PlaybookManifest> {
        let state = self.state.lock().await;
        let playbook = state
            .playbooks
            .get(&playbook_ref.playbook_id)
            .ok_or_else(|| anyhow!("unknown playbook {}", playbook_ref.playbook_id))?;
        require_matching_version(
            playbook,
            &playbook_ref.playbook_id,
            &playbook_ref.version,
            &playbook_ref.digest,
        )?;
        Ok(playbook
            .versions
            .get(&playbook_ref.version)
            .ok_or_else(|| anyhow!("unknown playbook version {}", playbook_ref.version))?
            .manifest
            .clone())
    }

    /// Stores one immutable Playbook version.
    pub(crate) async fn create_playbook(
        &self,
        request: CreatePlaybookRequest,
    ) -> Result<PlaybookView> {
        let validation = validate_playbook_manifest(&request.manifest);
        if !validation.valid {
            bail!(
                "invalid playbook manifest: {}",
                validation.errors.join("; ")
            );
        }
        let digest = playbook_manifest_digest(&request.manifest)?;
        let now = now_ms();
        let playbook_id = request.manifest.playbook_id.clone();
        let version = request.manifest.version.clone();
        let version_record = crate::PlaybookVersionRecord {
            playbook_id: playbook_id.clone(),
            version: version.clone(),
            digest: digest.clone(),
            manifest: request.manifest,
            created_at_ms: now,
        };

        let mut state = self.state.lock().await;
        let mut updated_playbooks = state.playbooks.clone();
        let record = updated_playbooks
            .entry(playbook_id.clone())
            .or_insert_with(|| PlaybookRecord {
                playbook_id: playbook_id.clone(),
                versions: BTreeMap::new(),
                releases: BTreeMap::new(),
                created_at_ms: now,
                updated_at_ms: now,
            });
        if let Some(existing) = record.versions.get(&version) {
            if existing.digest != digest {
                bail!(
                    "playbook {playbook_id}@{version} already exists with digest {}",
                    existing.digest
                );
            }
            return Ok(record_to_view(record, Some(&version)));
        }
        record.versions.insert(version.clone(), version_record);
        record
            .releases
            .entry(version.clone())
            .or_insert(PlaybookReleaseRecord {
                status: PlaybookReleaseStatus::Draft,
                updated_at_ms: now,
                evidence_refs: Vec::new(),
                revoked_reason: None,
            });
        record.updated_at_ms = now;
        self.store.save_playbooks(&updated_playbooks)?;
        state.playbooks = updated_playbooks;
        let record = state
            .playbooks
            .get(&playbook_id)
            .ok_or_else(|| anyhow!("unknown playbook {playbook_id}"))?;
        Ok(record_to_view(record, Some(&version)))
    }

    /// Publishes one immutable Playbook version into a startable release state.
    pub(crate) async fn publish_playbook(
        &self,
        playbook_id: &str,
        request: PublishPlaybookRequest,
    ) -> Result<PlaybookView> {
        let status = request.status.unwrap_or(PlaybookReleaseStatus::Active);
        if matches!(
            status,
            PlaybookReleaseStatus::Draft | PlaybookReleaseStatus::Revoked
        ) {
            bail!("publish status must be verified, canary, or active");
        }
        if status.requires_evidence() && request.evidence_refs.is_empty() {
            bail!(
                "publishing playbook {playbook_id}@{} requires evidence_refs",
                request.version
            );
        }
        let now = now_ms();
        let mut state = self.state.lock().await;
        let mut updated_playbooks = state.playbooks.clone();
        let record = updated_playbooks
            .get_mut(playbook_id)
            .ok_or_else(|| anyhow!("unknown playbook {playbook_id}"))?;
        require_matching_version(record, playbook_id, &request.version, &request.digest)?;
        if status == PlaybookReleaseStatus::Active {
            for (version, release) in record.releases.iter_mut() {
                if version != &request.version && release.status == PlaybookReleaseStatus::Active {
                    release.status = PlaybookReleaseStatus::Verified;
                    release.updated_at_ms = now;
                }
            }
        }
        record.releases.insert(
            request.version.clone(),
            PlaybookReleaseRecord {
                status,
                updated_at_ms: now,
                evidence_refs: request.evidence_refs,
                revoked_reason: None,
            },
        );
        record.updated_at_ms = now;
        self.store.save_playbooks(&updated_playbooks)?;
        state.playbooks = updated_playbooks;
        let record = state
            .playbooks
            .get(playbook_id)
            .ok_or_else(|| anyhow!("unknown playbook {playbook_id}"))?;
        Ok(record_to_view(record, Some(&request.version)))
    }

    /// Revokes one immutable Playbook version.
    pub(crate) async fn revoke_playbook(
        &self,
        playbook_id: &str,
        request: RevokePlaybookRequest,
    ) -> Result<PlaybookView> {
        let now = now_ms();
        let mut state = self.state.lock().await;
        let mut updated_playbooks = state.playbooks.clone();
        let record = updated_playbooks
            .get_mut(playbook_id)
            .ok_or_else(|| anyhow!("unknown playbook {playbook_id}"))?;
        require_matching_version(record, playbook_id, &request.version, &request.digest)?;
        record.releases.insert(
            request.version.clone(),
            PlaybookReleaseRecord {
                status: PlaybookReleaseStatus::Revoked,
                updated_at_ms: now,
                evidence_refs: request.evidence_refs,
                revoked_reason: request.reason,
            },
        );
        record.updated_at_ms = now;
        self.store.save_playbooks(&updated_playbooks)?;
        state.playbooks = updated_playbooks;
        let record = state
            .playbooks
            .get(playbook_id)
            .ok_or_else(|| anyhow!("unknown playbook {playbook_id}"))?;
        Ok(record_to_view(record, Some(&request.version)))
    }

    /// Reserves a Flow record before scheduling the underlying Kheish run.
    pub(crate) async fn reserve_flow_start(
        &self,
        request: &StartFlowRequest,
    ) -> Result<FlowStartReservation> {
        if let Some(flow_id) = request.flow_id.as_deref() {
            ensure_control_identifier("flow_id", flow_id)?;
        }
        if let Some(idempotency_key) = request.idempotency_key.as_deref() {
            ensure_control_identifier("idempotency_key", idempotency_key)?;
        }
        let input_digest = flow_input_digest(&request.request)?;
        let flow_id = request
            .flow_id
            .clone()
            .unwrap_or_else(|| self.next_flow_id());
        let now = now_ms();
        let mut state = self.state.lock().await;
        if let Some(existing_id) = request
            .idempotency_key
            .as_deref()
            .and_then(|key| state.flows_by_idempotency_key.get(key))
            .cloned()
        {
            let record = state
                .flows
                .get(&existing_id)
                .cloned()
                .ok_or_else(|| anyhow!("unknown flow {existing_id}"))?;
            ensure_flow_start_matches(&record, request, &input_digest)?;
            let should_submit_run = record.run_id.is_none()
                && record.cancelled_at_ms.is_none()
                && !state.flow_start_in_progress.contains(&record.flow_id);
            if should_submit_run {
                state.flow_start_in_progress.insert(record.flow_id.clone());
            }
            return Ok(FlowStartReservation {
                should_submit_run,
                record,
            });
        }
        if let Some(record) = state.flows.get(&flow_id).cloned() {
            ensure_flow_start_matches(&record, request, &input_digest)?;
            let should_submit_run = record.run_id.is_none()
                && record.cancelled_at_ms.is_none()
                && !state.flow_start_in_progress.contains(&record.flow_id);
            if should_submit_run {
                state.flow_start_in_progress.insert(record.flow_id.clone());
            }
            return Ok(FlowStartReservation {
                should_submit_run,
                record,
            });
        }

        let playbook = state
            .playbooks
            .get(&request.playbook_ref.playbook_id)
            .ok_or_else(|| anyhow!("unknown playbook {}", request.playbook_ref.playbook_id))?;
        require_matching_version(
            playbook,
            &request.playbook_ref.playbook_id,
            &request.playbook_ref.version,
            &request.playbook_ref.digest,
        )?;
        let release = playbook
            .releases
            .get(&request.playbook_ref.version)
            .ok_or_else(|| {
                anyhow!(
                    "playbook {}@{} has no release record",
                    request.playbook_ref.playbook_id,
                    request.playbook_ref.version
                )
            })?;
        if !release.status.is_startable() {
            bail!(
                "playbook {}@{} is not startable in status {:?}",
                request.playbook_ref.playbook_id,
                request.playbook_ref.version,
                release.status
            );
        }

        let mut updated_flows = state.flows.clone();
        let record = FlowRecord {
            flow_id: flow_id.clone(),
            idempotency_key: request.idempotency_key.clone(),
            playbook_ref: request.playbook_ref.clone(),
            session_id: request.session_id.clone(),
            input_digest,
            correlation_nonce: new_flow_correlation_nonce(),
            run_id: None,
            created_at_ms: now,
            updated_at_ms: now,
            completed_at_ms: None,
            cancelled_at_ms: None,
            metadata: request.metadata.clone(),
            evidence_refs: request.evidence_refs.clone(),
        };
        updated_flows.insert(flow_id, record.clone());
        self.store.save_flows(&updated_flows)?;
        state.flows = updated_flows;
        state.rebuild_indexes();
        state.flow_start_in_progress.insert(record.flow_id.clone());
        Ok(FlowStartReservation {
            record,
            should_submit_run: true,
        })
    }

    /// Attaches a scheduled run to a Flow record.
    pub(crate) async fn attach_flow_run(&self, flow_id: &str, run_id: &str) -> Result<FlowRecord> {
        let mut state = self.state.lock().await;
        let mut updated_flows = state.flows.clone();
        let record = updated_flows
            .get_mut(flow_id)
            .ok_or_else(|| anyhow!("unknown flow {flow_id}"))?;
        if let Some(existing) = record.run_id.as_deref() {
            if existing != run_id {
                bail!("flow {flow_id} already references run {existing}");
            }
        } else {
            record.run_id = Some(run_id.to_string());
            record.updated_at_ms = now_ms();
        }
        let updated = record.clone();
        self.store.save_flows(&updated_flows)?;
        state.flows = updated_flows;
        state.rebuild_indexes();
        state.flow_start_in_progress.remove(flow_id);
        Ok(updated)
    }

    /// Appends Flow evidence refs idempotently by `kind + id`.
    pub(crate) async fn append_flow_evidence(
        &self,
        flow_id: &str,
        request: AppendFlowEvidenceRequest,
    ) -> Result<FlowRecord> {
        if request.evidence_refs.is_empty() {
            bail!("evidence_refs is required");
        }
        for evidence in &request.evidence_refs {
            if evidence.kind.trim().is_empty() {
                bail!("evidence kind is required");
            }
            if evidence.id.trim().is_empty() {
                bail!("evidence id is required");
            }
        }
        let mut state = self.state.lock().await;
        let mut updated_flows = state.flows.clone();
        let record = updated_flows
            .get_mut(flow_id)
            .ok_or_else(|| anyhow!("unknown flow {flow_id}"))?;
        let mut seen = record
            .evidence_refs
            .iter()
            .map(|evidence| (evidence.kind.clone(), evidence.id.clone()))
            .collect::<BTreeSet<_>>();
        let mut changed = false;
        for evidence in request.evidence_refs {
            if seen.insert((evidence.kind.clone(), evidence.id.clone())) {
                record.evidence_refs.push(evidence);
                changed = true;
            }
        }
        if changed {
            record.updated_at_ms = now_ms();
            self.store.save_flows(&updated_flows)?;
            state.flows = updated_flows;
            state.rebuild_indexes();
        }
        state
            .flows
            .get(flow_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown flow {flow_id}"))
    }

    /// Seals one Flow projection at its first terminal boundary.
    pub(crate) async fn mark_flow_completed_at(
        &self,
        flow_id: &str,
        completed_at_ms: u64,
    ) -> Result<FlowRecord> {
        let mut state = self.state.lock().await;
        let mut updated_flows = state.flows.clone();
        let record = updated_flows
            .get_mut(flow_id)
            .ok_or_else(|| anyhow!("unknown flow {flow_id}"))?;
        if record.completed_at_ms.is_none() {
            record.completed_at_ms = Some(completed_at_ms);
            record.updated_at_ms = now_ms();
            let updated = record.clone();
            self.store.save_flows(&updated_flows)?;
            state.flows = updated_flows;
            state.rebuild_indexes();
            return Ok(updated);
        }
        Ok(record.clone())
    }

    /// Lists Flow records using storage-level filters only.
    pub(crate) async fn list_flow_records(&self, query: &FlowListQuery) -> Vec<FlowRecord> {
        let state = self.state.lock().await;
        let mut flows = state
            .flows
            .values()
            .filter(|flow| {
                query
                    .playbook_id
                    .as_deref()
                    .is_none_or(|id| flow.playbook_ref.playbook_id == id)
                    && query
                        .session_id
                        .as_deref()
                        .is_none_or(|id| flow.session_id == id)
            })
            .cloned()
            .collect::<Vec<_>>();
        flows.sort_by(|left, right| left.flow_id.cmp(&right.flow_id));
        flows
    }

    /// Returns one Flow record.
    pub(crate) async fn get_flow_record(&self, flow_id: &str) -> Result<FlowRecord> {
        self.state
            .lock()
            .await
            .flows
            .get(flow_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown flow {flow_id}"))
    }

    /// Removes a Flow reservation that failed before a run was created.
    pub(crate) async fn rollback_unattached_flow_start(&self, flow_id: &str) -> Result<()> {
        let mut state = self.state.lock().await;
        let Some(record) = state.flows.get(flow_id) else {
            return Ok(());
        };
        if record.run_id.is_some() || record.cancelled_at_ms.is_some() {
            state.flow_start_in_progress.remove(flow_id);
            return Ok(());
        }
        let mut updated_flows = state.flows.clone();
        updated_flows.remove(flow_id);
        self.store.save_flows(&updated_flows)?;
        state.flows = updated_flows;
        state.rebuild_indexes();
        state.flow_start_in_progress.remove(flow_id);
        Ok(())
    }

    /// Marks a Flow cancelled when it has no run yet.
    pub(crate) async fn cancel_pending_flow(&self, flow_id: &str) -> Result<FlowRecord> {
        let mut state = self.state.lock().await;
        let mut updated_flows = state.flows.clone();
        let record = updated_flows
            .get_mut(flow_id)
            .ok_or_else(|| anyhow!("unknown flow {flow_id}"))?;
        if record.run_id.is_none() && record.cancelled_at_ms.is_none() {
            let now = now_ms();
            record.cancelled_at_ms = Some(now);
            record.updated_at_ms = now;
        }
        let updated = record.clone();
        self.store.save_flows(&updated_flows)?;
        state.flows = updated_flows;
        state.rebuild_indexes();
        state.flow_start_in_progress.remove(flow_id);
        Ok(updated)
    }
}

fn new_flow_correlation_nonce() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

fn ensure_flow_start_matches(
    record: &FlowRecord,
    request: &StartFlowRequest,
    input_digest: &str,
) -> Result<()> {
    if let Some(requested_flow_id) = request.flow_id.as_deref()
        && record.flow_id != requested_flow_id
    {
        bail!(
            "idempotency key is already bound to flow {}",
            record.flow_id
        );
    }
    if record.playbook_ref != request.playbook_ref {
        bail!(
            "flow {} already references a different playbook",
            record.flow_id
        );
    }
    if record.session_id != request.session_id {
        bail!(
            "flow {} already targets session {}",
            record.flow_id,
            record.session_id
        );
    }
    if record.input_digest != input_digest {
        bail!(
            "flow {} already has a different input digest",
            record.flow_id
        );
    }
    if record.idempotency_key != request.idempotency_key {
        bail!(
            "flow {} already has a different idempotency key",
            record.flow_id
        );
    }
    if record.metadata != request.metadata {
        bail!("flow {} already has different metadata", record.flow_id);
    }
    if record.evidence_refs != request.evidence_refs {
        bail!(
            "flow {} already has different evidence refs",
            record.flow_id
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicU64;

    use serde_json::{Value, json};
    use tempfile::tempdir;

    use super::*;
    use crate::{
        FlowEvidenceRef, PlaybookManifest, PlaybookPhase, PlaybookToolPolicy, PlaybookVersionRef,
        SubmitInputRequest,
    };

    #[tokio::test]
    async fn pending_flow_reservation_does_not_schedule_duplicate_start() -> Result<()> {
        let temp = tempdir()?;
        let service = PlaybookService::new(
            FilePlaybookStore::new(temp.path()),
            BTreeMap::new(),
            BTreeMap::new(),
            AtomicU64::new(1),
        );
        let manifest = test_manifest();
        let digest = playbook_manifest_digest(&manifest)?;
        service
            .create_playbook(CreatePlaybookRequest { manifest })
            .await?;
        service
            .publish_playbook(
                "ops-flow",
                PublishPlaybookRequest {
                    version: "1".to_string(),
                    digest: digest.clone(),
                    status: Some(PlaybookReleaseStatus::Active),
                    evidence_refs: test_evidence(),
                },
            )
            .await?;
        let request = test_start_request(digest);

        let first = service.reserve_flow_start(&request).await?;
        assert!(first.should_submit_run);
        assert!(!first.record.correlation_nonce.is_empty());

        let second = service.reserve_flow_start(&request).await?;
        assert!(
            !second.should_submit_run,
            "a concurrent retry must observe the pending Flow without scheduling a duplicate run"
        );
        assert_eq!(second.record.flow_id, first.record.flow_id);

        service.attach_flow_run("flow-1", "run-1").await?;
        let third = service.reserve_flow_start(&request).await?;
        assert!(!third.should_submit_run);
        assert_eq!(third.record.run_id.as_deref(), Some("run-1"));
        Ok(())
    }

    #[tokio::test]
    async fn pending_flow_reservation_can_retry_after_rollback() -> Result<()> {
        let temp = tempdir()?;
        let service = PlaybookService::new(
            FilePlaybookStore::new(temp.path()),
            BTreeMap::new(),
            BTreeMap::new(),
            AtomicU64::new(1),
        );
        let manifest = test_manifest();
        let digest = playbook_manifest_digest(&manifest)?;
        service
            .create_playbook(CreatePlaybookRequest { manifest })
            .await?;
        service
            .publish_playbook(
                "ops-flow",
                PublishPlaybookRequest {
                    version: "1".to_string(),
                    digest: digest.clone(),
                    status: Some(PlaybookReleaseStatus::Active),
                    evidence_refs: test_evidence(),
                },
            )
            .await?;
        let request = test_start_request(digest);

        let first = service.reserve_flow_start(&request).await?;
        assert!(first.should_submit_run);
        service
            .rollback_unattached_flow_start(&first.record.flow_id)
            .await?;

        let second = service.reserve_flow_start(&request).await?;
        assert!(second.should_submit_run);
        Ok(())
    }

    fn test_manifest() -> PlaybookManifest {
        PlaybookManifest {
            playbook_id: "ops-flow".to_string(),
            version: "1".to_string(),
            title: "Ops flow".to_string(),
            objective: "Validate Flow reservation behavior.".to_string(),
            description: None,
            inputs: Vec::new(),
            preconditions: Vec::new(),
            roles: Vec::new(),
            phases: vec![PlaybookPhase {
                phase_id: "run".to_string(),
                objective: "Run the Flow.".to_string(),
                acceptance_criteria: Vec::new(),
                required_evidence: Vec::new(),
            }],
            acceptance_criteria: vec!["The Flow reserves once.".to_string()],
            evidence_expectations: Vec::new(),
            required_evidence: Vec::new(),
            tools: PlaybookToolPolicy::default(),
            runtime_defaults: Default::default(),
            scopes: Default::default(),
            metadata: Value::Null,
        }
    }

    fn test_start_request(digest: String) -> StartFlowRequest {
        StartFlowRequest {
            flow_id: Some("flow-1".to_string()),
            idempotency_key: Some("idem-1".to_string()),
            playbook_ref: PlaybookVersionRef {
                playbook_id: "ops-flow".to_string(),
                version: "1".to_string(),
                digest,
            },
            session_id: "session-1".to_string(),
            request: SubmitInputRequest {
                provider: None,
                source_plugin: Some("daemon".to_string()),
                source_kind: Some("api".to_string()),
                actor_id: Some("operator".to_string()),
                content: "run".to_string(),
                input_items: Vec::new(),
                attachments: Vec::new(),
                generation: None,
                completion_requirements: None,
                metadata: None,
                binding_keys: Vec::new(),
                reply_targets: Vec::new(),
                reply_plugin: None,
                reply_address: None,
            },
            metadata: json!({"test": true}),
            evidence_refs: test_evidence(),
        }
    }

    fn test_evidence() -> Vec<FlowEvidenceRef> {
        vec![FlowEvidenceRef {
            kind: "test".to_string(),
            id: "reservation".to_string(),
            description: None,
        }]
    }
}
