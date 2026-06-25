use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use kheish_skills::{SharedSkillRegistry, SkillDefinition};
use tokio::sync::Mutex;

use crate::learning::{LearningView, learning_content_has_secret_material};
use crate::procedural_skills::{
    FileLearningSkillStore, LearningSkillDraft, LearningSkillLifecycleEvent,
    LearningSkillRolloutKind, LearningSkillRolloutResult, LearningSkillStatus, LearningSkillView,
    learning_skill_definition_fingerprint, normalize_learning_skill_text,
};
use kheish_types::{
    LearningEvidenceRef, LearningKind, LearningPublishTier, LearningScopeKind, LearningStatus,
    LearningVerificationStatus, SkillExecutionContext,
};

/// Owns daemon-managed procedural skills promoted from reviewed learnings.
pub(crate) struct LearningSkillService {
    store: FileLearningSkillStore,
    skills: Arc<SharedSkillRegistry>,
    mutation_lock: Mutex<()>,
    records: Mutex<BTreeMap<String, LearningSkillView>>,
}

impl LearningSkillService {
    /// Creates one promoted-skill service backed by the provided store.
    pub(crate) fn new(
        store: FileLearningSkillStore,
        records: BTreeMap<String, LearningSkillView>,
        skills: Arc<SharedSkillRegistry>,
    ) -> Self {
        Self {
            store,
            skills,
            mutation_lock: Mutex::new(()),
            records: Mutex::new(records),
        }
    }

    /// Repairs missing daemon-owned skill files for active promoted skills and refreshes metadata.
    pub(crate) async fn repair_catalog(&self) -> Result<()> {
        let records = self
            .records
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let active_learning_ids = records
            .iter()
            .filter(|record| record.status == LearningSkillStatus::Active)
            .map(|record| record.source_learning_id.clone())
            .collect::<BTreeSet<_>>();
        let mut catalog_changed = false;
        let mut repaired = Vec::new();
        for learning_id in self.store.list_catalog_learning_ids()? {
            if active_learning_ids.contains(&learning_id) {
                continue;
            }
            self.store.remove_skill_files_by_learning_id(&learning_id)?;
            catalog_changed = true;
        }
        for record in records {
            if record.status != LearningSkillStatus::Active {
                continue;
            }
            let loaded_matches_record = self
                .skills
                .get(&record.skill_name)
                .map(|loaded| self.loaded_skill_matches_record_definition(&record, &loaded))
                .unwrap_or(false);
            if !loaded_matches_record {
                self.store.write_skill_files(&record)?;
                catalog_changed = true;
            }
        }
        if catalog_changed {
            self.skills.reload();
        }
        for record in self.records.lock().await.values().cloned() {
            if record.status != LearningSkillStatus::Active {
                continue;
            }
            let Some(loaded) = self.skills.get(&record.skill_name) else {
                bail!(
                    "active promoted skill {} is missing after repair",
                    record.skill_name
                );
            };
            if !self.loaded_skill_matches_record_definition(&record, &loaded) {
                bail!(
                    "active promoted skill {} loaded definition does not match the daemon record",
                    record.skill_name
                );
            }
            if record.skill_path != loaded.skill_path.display().to_string()
                || record.skill_root != loaded.skill_root.display().to_string()
                || record.digest != loaded.digest
            {
                let mut updated = record.clone();
                updated.skill_path = loaded.skill_path.display().to_string();
                updated.skill_root = loaded.skill_root.display().to_string();
                updated.digest = loaded.digest.clone();
                repaired.push(updated);
            }
        }
        if repaired.is_empty() {
            return Ok(());
        }
        let mut records = self.records.lock().await;
        for record in repaired {
            self.store.save(&record)?;
            records.insert(record.skill_name.clone(), record);
        }
        Ok(())
    }

    /// Returns every promoted procedural skill record.
    pub(crate) async fn list(&self) -> Vec<LearningSkillView> {
        self.records.lock().await.values().cloned().collect()
    }

    /// Returns one promoted procedural skill by name.
    pub(crate) async fn get(&self, skill_name: &str) -> Option<LearningSkillView> {
        self.records.lock().await.get(skill_name).cloned()
    }

    /// Promotes one reviewed procedure learning into a daemon-owned skill.
    pub(crate) async fn promote(
        &self,
        learning: &LearningView,
        mut draft: LearningSkillDraft,
        promoted_at_ms: u64,
    ) -> Result<LearningSkillView> {
        let _mutation = self.mutation_lock.lock().await;
        ensure_promotable_learning(learning)?;
        if draft.runtime.agent_profile.is_none() {
            draft.runtime.agent_profile = Some("verification".to_string());
        }
        ensure_promotable_runtime(&draft.runtime)?;
        draft.description = draft
            .description
            .as_deref()
            .and_then(normalize_learning_skill_text);
        draft.when_to_use = draft
            .when_to_use
            .as_deref()
            .and_then(normalize_learning_skill_text);
        draft.version = draft
            .version
            .as_deref()
            .and_then(normalize_learning_skill_text);

        let records = self.records.lock().await;
        if let Some(existing) = records
            .values()
            .find(|record| {
                record.status != LearningSkillStatus::Revoked
                    && record.source_learning_id == learning.learning_id
            })
            .cloned()
        {
            if existing.skill_name == draft.skill_name {
                drop(records);
                let updated = merged_promoted_record(existing.clone(), draft, promoted_at_ms)?;
                return self
                    .store_promoted_record(updated, Some(existing), promoted_at_ms)
                    .await;
            }
            bail!(
                "learning {} is already promoted as skill {}",
                learning.learning_id,
                existing.skill_name
            );
        }
        if let Some(existing) = records.get(&draft.skill_name).cloned()
            && existing.status != LearningSkillStatus::Revoked
        {
            bail!("promoted skill {} already exists", draft.skill_name);
        }
        drop(records);
        if draft.status != LearningSkillStatus::Draft {
            bail!("new promoted skills must start in draft status");
        }
        if self.skills.get(&draft.skill_name).is_some() {
            bail!(
                "skill {} already exists in the daemon catalog",
                draft.skill_name
            );
        }

        let expected_root = self.store.expected_skill_root(&learning.learning_id);
        let mut provisional = LearningSkillView {
            skill_name: draft.skill_name.clone(),
            source_learning_id: learning.learning_id.clone(),
            source_scope: learning.scope.clone(),
            status: draft.status.clone(),
            description: draft
                .description
                .clone()
                .or_else(default_promoted_description)
                .expect("default description is always present"),
            when_to_use: draft.when_to_use.clone(),
            version: draft.version.clone(),
            instructions: draft.instructions.trim().to_string(),
            skill_path: expected_root.join("SKILL.md").display().to_string(),
            skill_root: expected_root.display().to_string(),
            digest: String::new(),
            definition_fingerprint: String::new(),
            runtime: draft.runtime.clone(),
            evidence_refs: draft.evidence_refs.clone(),
            lifecycle_events: Vec::new(),
            verification_status: verification_status_for_skill_status(
                &draft.status,
                &draft.verification_status,
            ),
            successful_run_count: draft.successful_run_count,
            distinct_session_count: draft.distinct_session_count,
            verifier_run_ids: draft.verifier_run_ids.clone(),
            real_daemon_verified: draft.real_daemon_verified,
            last_verified_workspace_digest: draft.last_verified_workspace_digest.clone(),
            canary_success_count: draft.canary_success_count,
            canary_failure_count: draft.canary_failure_count,
            promoted_at_ms,
            revoked_at_ms: None,
            revoked_reason: None,
        };
        provisional.definition_fingerprint = learning_skill_definition_fingerprint(&provisional);
        provisional.lifecycle_events.push(status_lifecycle_event(
            "promote",
            promoted_at_ms,
            None,
            provisional.status.clone(),
            &provisional,
            None,
        ));
        debug_assert_eq!(
            expected_root,
            self.store.expected_skill_root(&learning.learning_id)
        );
        self.store_promoted_record(provisional, None, promoted_at_ms)
            .await
    }

    /// Revokes one promoted procedural skill and removes it from the live catalog.
    pub(crate) async fn revoke(
        &self,
        skill_name: &str,
        revoked_at_ms: u64,
        reason: Option<String>,
    ) -> Result<LearningSkillView> {
        let _mutation = self.mutation_lock.lock().await;
        let reason = normalize_operator_reason("revocation", reason)?;
        let record = self
            .records
            .lock()
            .await
            .get(skill_name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown learning skill {skill_name}"))?;
        if record.status == LearningSkillStatus::Revoked {
            return Ok(record);
        }

        let mut revoked = record.clone();
        let previous_status = record.status.clone();
        revoked.status = LearningSkillStatus::Revoked;
        revoked.revoked_at_ms = Some(revoked_at_ms);
        revoked.revoked_reason = reason.clone();
        revoked.lifecycle_events.push(status_lifecycle_event(
            "revoke",
            revoked_at_ms,
            Some(previous_status),
            LearningSkillStatus::Revoked,
            &revoked,
            reason,
        ));
        self.store.save_history(&record, revoked_at_ms)?;
        self.store.save(&revoked)?;
        self.records
            .lock()
            .await
            .insert(revoked.skill_name.clone(), revoked.clone());
        if !status_mounts_catalog(record.status.clone()) {
            return Ok(revoked);
        }
        if let Err(error) = self.store.remove_skill_files(&record) {
            let rollback_result: Result<()> = async {
                self.store.save(&record)?;
                self.records
                    .lock()
                    .await
                    .insert(record.skill_name.clone(), record.clone());
                self.skills.reload();
                Ok(())
            }
            .await;
            return match rollback_result {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(anyhow!(
                    "failed to remove learning skill {skill_name} from the catalog; rollback also failed: {rollback_error}"
                )),
            };
        }
        self.skills.reload();
        Ok(revoked)
    }

    /// Restores the latest historical active snapshot for one promoted procedural skill.
    pub(crate) async fn rollback(
        &self,
        skill_name: &str,
        rolled_back_at_ms: u64,
        reason: Option<String>,
    ) -> Result<LearningSkillView> {
        let _mutation = self.mutation_lock.lock().await;
        let reason = normalize_operator_reason("rollback", reason)?;
        let current = self
            .records
            .lock()
            .await
            .get(skill_name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown learning skill {skill_name}"))?;
        let mut restored = self
            .store
            .load_latest_active_history(skill_name)?
            .ok_or_else(|| {
                anyhow!("no active rollback snapshot exists for learning skill {skill_name}")
            })?;
        restored.revoked_at_ms = None;
        restored.revoked_reason = None;
        restored.definition_fingerprint = learning_skill_definition_fingerprint(&restored);
        self.store.save_history(&current, rolled_back_at_ms)?;
        let rollback_reason = reason.clone();
        if let Some(reason) = reason {
            restored.evidence_refs.push(LearningEvidenceRef {
                run_id: None,
                artifact_id: None,
                note: Some(format!(
                    "rollback restored active snapshot: {}",
                    reason.trim()
                )),
            });
        }
        restored.lifecycle_events.push(status_lifecycle_event(
            "rollback",
            rolled_back_at_ms,
            Some(current.status.clone()),
            restored.status.clone(),
            &restored,
            rollback_reason,
        ));
        self.store.save(&restored)?;
        self.store.write_skill_files(&restored)?;
        self.skills.reload();
        let Some(loaded) = self.skills.get(&restored.skill_name) else {
            bail!(
                "rolled back promoted skill {} did not load into the catalog",
                restored.skill_name
            );
        };
        if !self.loaded_skill_matches_record_definition(&restored, &loaded) {
            bail!(
                "rolled back promoted skill {} loaded definition does not match the daemon record",
                restored.skill_name
            );
        }
        let updated = LearningSkillView {
            skill_path: loaded.skill_path.display().to_string(),
            skill_root: loaded.skill_root.display().to_string(),
            digest: loaded.digest.clone(),
            definition_fingerprint: learning_skill_definition_fingerprint(&restored),
            ..restored
        };
        self.store.save(&updated)?;
        self.records
            .lock()
            .await
            .insert(updated.skill_name.clone(), updated.clone());
        Ok(updated)
    }

    /// Records daemon-validated rollout evidence against the current promoted-skill definition.
    pub(crate) async fn record_rollout_result(
        &self,
        skill_name: &str,
        result: LearningSkillRolloutResult,
    ) -> Result<LearningSkillView> {
        let _mutation = self.mutation_lock.lock().await;
        let mut record = self
            .records
            .lock()
            .await
            .get(skill_name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown learning skill {skill_name}"))?;
        if record.status == LearningSkillStatus::Revoked {
            bail!("revoked promoted skills cannot accept rollout evidence");
        }
        if matches!(result.kind, LearningSkillRolloutKind::Canary)
            && record.status != LearningSkillStatus::Canary
        {
            bail!("canary rollout evidence requires promoted skill status canary");
        }
        if record.definition_fingerprint.is_empty() {
            record.definition_fingerprint = learning_skill_definition_fingerprint(&record);
        }
        if let Some(expected_fingerprint) = result
            .definition_fingerprint
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            && expected_fingerprint != record.definition_fingerprint
        {
            bail!(
                "rollout evidence definition_fingerprint does not match current promoted skill definition"
            );
        }
        if record
            .verifier_run_ids
            .iter()
            .any(|run_id| run_id == &result.run_id)
        {
            return Ok(record);
        }

        match result.kind {
            LearningSkillRolloutKind::Verification => {
                if !result.success {
                    bail!(
                        "verification rollout evidence requires a completed run with matching output"
                    );
                }
                record.verification_status = LearningVerificationStatus::Verified;
                record.real_daemon_verified = true;
                record.successful_run_count = record.successful_run_count.saturating_add(1);
                record.distinct_session_count = record.distinct_session_count.max(1);
                record.verifier_run_ids.push(result.run_id.clone());
                let evidence =
                    rollout_evidence_ref(&result, "verification rollout succeeded", &record);
                record.evidence_refs.push(evidence);
            }
            LearningSkillRolloutKind::Canary => {
                ensure_has_verification_evidence(&record)?;
                record.verifier_run_ids.push(result.run_id.clone());
                if result.success {
                    record.canary_success_count = record.canary_success_count.saturating_add(1);
                    record.successful_run_count = record.successful_run_count.saturating_add(1);
                    record.distinct_session_count = record.distinct_session_count.max(1);
                    let evidence =
                        rollout_evidence_ref(&result, "canary rollout succeeded", &record);
                    record.evidence_refs.push(evidence);
                } else {
                    record.canary_failure_count = record.canary_failure_count.saturating_add(1);
                    let evidence = rollout_evidence_ref(&result, "canary rollout failed", &record);
                    record.evidence_refs.push(evidence);
                }
            }
        }
        record
            .lifecycle_events
            .push(rollout_lifecycle_event(&record, &result));
        self.store.save(&record)?;
        self.records
            .lock()
            .await
            .insert(record.skill_name.clone(), record.clone());
        Ok(record)
    }

    /// Returns the current non-revoked promoted procedural skill derived from one learning.
    pub(crate) async fn live_by_learning(&self, learning_id: &str) -> Option<LearningSkillView> {
        self.records
            .lock()
            .await
            .values()
            .find(|record| {
                record.status != LearningSkillStatus::Revoked
                    && record.source_learning_id == learning_id
            })
            .cloned()
    }

    /// Restores one active promoted procedural skill after a partial failure in a higher-level workflow.
    pub(crate) async fn restore_active(
        &self,
        record: LearningSkillView,
    ) -> Result<LearningSkillView> {
        let _mutation = self.mutation_lock.lock().await;
        self.store.save(&record)?;
        if !status_mounts_catalog(record.status.clone()) {
            self.records
                .lock()
                .await
                .insert(record.skill_name.clone(), record.clone());
            return Ok(record);
        }
        self.store.write_skill_files(&record)?;
        self.skills.reload();
        let Some(loaded) = self.skills.get(&record.skill_name) else {
            bail!(
                "restored promoted skill {} did not load into the catalog",
                record.skill_name
            );
        };
        if !self.loaded_skill_matches_record_definition(&record, &loaded) {
            bail!(
                "restored promoted skill {} loaded definition does not match the daemon record",
                record.skill_name
            );
        }
        let restored = LearningSkillView {
            skill_path: loaded.skill_path.display().to_string(),
            skill_root: loaded.skill_root.display().to_string(),
            digest: loaded.digest.clone(),
            ..record
        };
        self.store.save(&restored)?;
        self.records
            .lock()
            .await
            .insert(restored.skill_name.clone(), restored.clone());
        Ok(restored)
    }

    async fn store_promoted_record(
        &self,
        record: LearningSkillView,
        previous: Option<LearningSkillView>,
        mutation_at_ms: u64,
    ) -> Result<LearningSkillView> {
        let skill_name = record.skill_name.clone();
        let source_learning_id = record.source_learning_id.clone();
        if let Some(previous) = previous.as_ref() {
            self.store.save_history(previous, mutation_at_ms)?;
        }
        self.store.save(&record)?;
        let update_result: Result<LearningSkillView> = async {
            if status_mounts_catalog(record.status.clone()) {
                self.store.write_skill_files(&record)?;
                self.skills.reload();
                let Some(loaded) = self.skills.get(&record.skill_name) else {
                    bail!(
                        "promoted skill {} did not load into the catalog",
                        record.skill_name
                    );
                };
                if !self.loaded_skill_matches_record_definition(&record, &loaded) {
                    bail!(
                        "promoted skill {} loaded definition does not match the daemon record",
                        record.skill_name
                    );
                }
                let updated = LearningSkillView {
                    skill_path: loaded.skill_path.display().to_string(),
                    skill_root: loaded.skill_root.display().to_string(),
                    digest: loaded.digest.clone(),
                    ..record.clone()
                };
                self.store.save(&updated)?;
                self.records
                    .lock()
                    .await
                    .insert(updated.skill_name.clone(), updated.clone());
                return Ok(updated);
            }

            self.store
                .remove_skill_files_by_learning_id(&record.source_learning_id)?;
            self.skills.reload();
            self.records
                .lock()
                .await
                .insert(record.skill_name.clone(), record.clone());
            Ok(record)
        }
        .await;

        match update_result {
            Ok(updated) => Ok(updated),
            Err(error) => {
                let rollback_error = self
                    .restore_record_state(previous, &skill_name, &source_learning_id)
                    .await
                    .err();
                match rollback_error {
                    Some(rollback_error) => Err(anyhow!(
                        "failed to update promoted skill {}; rollback also failed: {rollback_error}; original error: {error}",
                        skill_name
                    )),
                    None => Err(error),
                }
            }
        }
    }

    async fn restore_record_state(
        &self,
        previous: Option<LearningSkillView>,
        skill_name: &str,
        source_learning_id: &str,
    ) -> Result<()> {
        match previous {
            Some(previous) => {
                self.store.save(&previous)?;
                if status_mounts_catalog(previous.status.clone()) {
                    self.store.write_skill_files(&previous)?;
                } else {
                    self.store
                        .remove_skill_files_by_learning_id(source_learning_id)?;
                }
                self.skills.reload();
                self.records
                    .lock()
                    .await
                    .insert(previous.skill_name.clone(), previous);
            }
            None => {
                self.store.delete(skill_name)?;
                self.store
                    .remove_skill_files_by_learning_id(source_learning_id)?;
                self.skills.reload();
                self.records.lock().await.remove(skill_name);
            }
        }
        Ok(())
    }

    fn loaded_skill_matches_record_definition(
        &self,
        record: &LearningSkillView,
        loaded: &SkillDefinition,
    ) -> bool {
        let expected_root = self.store.expected_skill_root(&record.source_learning_id);
        let expected_path = self.store.expected_skill_path(&record.source_learning_id);
        loaded.name == record.skill_name
            && loaded.description == record.description
            && loaded.when_to_use == record.when_to_use
            && loaded.version == record.version
            && loaded.instructions.trim() == record.instructions.trim()
            && loaded.runtime == record.runtime
            && paths_match(&loaded.skill_root, &expected_root)
            && paths_match(&loaded.skill_path, &expected_path)
    }
}

fn status_lifecycle_event(
    event: &'static str,
    recorded_at_ms: u64,
    from_status: Option<LearningSkillStatus>,
    to_status: LearningSkillStatus,
    record: &LearningSkillView,
    reason: Option<String>,
) -> LearningSkillLifecycleEvent {
    LearningSkillLifecycleEvent {
        event: event.to_string(),
        recorded_at_ms,
        from_status,
        to_status,
        rollout_kind: None,
        run_id: None,
        session_id: None,
        success: None,
        definition_fingerprint: Some(record.definition_fingerprint.clone()),
        reason,
    }
}

fn rollout_lifecycle_event(
    record: &LearningSkillView,
    result: &LearningSkillRolloutResult,
) -> LearningSkillLifecycleEvent {
    LearningSkillLifecycleEvent {
        event: "rollout_result".to_string(),
        recorded_at_ms: result.recorded_at_ms,
        from_status: None,
        to_status: record.status.clone(),
        rollout_kind: Some(result.kind.clone()),
        run_id: Some(result.run_id.clone()),
        session_id: Some(result.session_id.clone()),
        success: Some(result.success),
        definition_fingerprint: Some(record.definition_fingerprint.clone()),
        reason: None,
    }
}

fn default_promoted_description() -> Option<String> {
    Some("Execute this promoted procedure in a dedicated child agent.".to_string())
}

fn normalize_operator_reason(context: &str, reason: Option<String>) -> Result<Option<String>> {
    let Some(reason) = reason.map(|value| value.trim().to_string()) else {
        return Ok(None);
    };
    if reason.is_empty() {
        return Ok(None);
    }
    if learning_content_has_secret_material(&reason) {
        bail!("promoted skill {context} reason appears to contain secret material");
    }
    Ok(Some(reason))
}

fn status_mounts_catalog(status: LearningSkillStatus) -> bool {
    matches!(status, LearningSkillStatus::Active)
}

fn merged_promoted_record(
    existing: LearningSkillView,
    draft: LearningSkillDraft,
    promoted_at_ms: u64,
) -> Result<LearningSkillView> {
    let existing_fingerprint = effective_definition_fingerprint(&existing);
    let existing_for_gate = existing.clone();
    let effective_verification_status =
        verification_status_for_skill_status(&draft.status, &existing.verification_status);
    let mut updated = LearningSkillView {
        skill_name: existing.skill_name.clone(),
        source_learning_id: existing.source_learning_id.clone(),
        source_scope: existing.source_scope.clone(),
        status: draft.status,
        description: draft.description.unwrap_or(existing.description),
        when_to_use: draft.when_to_use.or(existing.when_to_use),
        version: draft.version.or(existing.version),
        instructions: draft.instructions.trim().to_string(),
        skill_path: existing.skill_path,
        skill_root: existing.skill_root,
        digest: existing.digest,
        definition_fingerprint: String::new(),
        runtime: draft.runtime,
        evidence_refs: if draft.evidence_refs.is_empty() {
            existing.evidence_refs
        } else {
            draft.evidence_refs
        },
        lifecycle_events: existing.lifecycle_events,
        verification_status: effective_verification_status,
        successful_run_count: if draft.successful_run_count == 0 {
            existing.successful_run_count
        } else {
            draft.successful_run_count
        },
        distinct_session_count: if draft.distinct_session_count == 0 {
            existing.distinct_session_count
        } else {
            draft.distinct_session_count
        },
        verifier_run_ids: if draft.verifier_run_ids.is_empty() {
            existing.verifier_run_ids
        } else {
            draft.verifier_run_ids
        },
        real_daemon_verified: existing.real_daemon_verified || draft.real_daemon_verified,
        last_verified_workspace_digest: draft
            .last_verified_workspace_digest
            .or(existing.last_verified_workspace_digest),
        canary_success_count: if draft.canary_success_count == 0 {
            existing.canary_success_count
        } else {
            draft.canary_success_count
        },
        canary_failure_count: if draft.canary_failure_count == 0 {
            existing.canary_failure_count
        } else {
            draft.canary_failure_count
        },
        promoted_at_ms: existing.promoted_at_ms.min(promoted_at_ms),
        revoked_at_ms: None,
        revoked_reason: None,
    };
    updated.definition_fingerprint = learning_skill_definition_fingerprint(&updated);
    let definition_changed = existing_fingerprint != updated.definition_fingerprint;
    let status_changed = existing_for_gate.status != updated.status;
    let restarting_rollout = matches!(
        (&existing_for_gate.status, &updated.status),
        (
            LearningSkillStatus::Verified | LearningSkillStatus::Canary,
            LearningSkillStatus::Draft
        )
    );
    if definition_changed || restarting_rollout {
        reset_rollout_evidence(&mut updated);
    }
    ensure_skill_status_transition(&existing.status, &updated.status)?;
    ensure_rollout_gate(&existing_for_gate, &updated, definition_changed)?;
    if status_changed || definition_changed || restarting_rollout {
        let reason = if definition_changed {
            Some("definition changed".to_string())
        } else if restarting_rollout {
            Some("rollout restarted".to_string())
        } else {
            None
        };
        updated.lifecycle_events.push(status_lifecycle_event(
            "promote",
            promoted_at_ms,
            Some(existing_for_gate.status.clone()),
            updated.status.clone(),
            &updated,
            reason,
        ));
    }
    Ok(updated)
}

fn ensure_skill_status_transition(
    current: &LearningSkillStatus,
    next: &LearningSkillStatus,
) -> Result<()> {
    let allowed = matches!(
        (current, next),
        (LearningSkillStatus::Draft, LearningSkillStatus::Draft)
            | (LearningSkillStatus::Draft, LearningSkillStatus::Verified)
            | (LearningSkillStatus::Verified, LearningSkillStatus::Draft)
            | (LearningSkillStatus::Verified, LearningSkillStatus::Verified)
            | (LearningSkillStatus::Verified, LearningSkillStatus::Canary)
            | (LearningSkillStatus::Verified, LearningSkillStatus::Active)
            | (LearningSkillStatus::Canary, LearningSkillStatus::Draft)
            | (LearningSkillStatus::Canary, LearningSkillStatus::Canary)
            | (LearningSkillStatus::Canary, LearningSkillStatus::Active)
            | (LearningSkillStatus::Active, LearningSkillStatus::Active)
    );
    if allowed {
        return Ok(());
    }
    bail!(
        "cannot transition promoted skill from {:?} to {:?}",
        current,
        next
    )
}

fn ensure_rollout_gate(
    current: &LearningSkillView,
    next: &LearningSkillView,
    definition_changed: bool,
) -> Result<()> {
    if current.status == LearningSkillStatus::Active && definition_changed {
        bail!("active promoted skill definition changes must start a new draft rollout");
    }
    if current.status == LearningSkillStatus::Active
        && next.status == LearningSkillStatus::Active
        && !definition_changed
    {
        return Ok(());
    }
    match next.status {
        LearningSkillStatus::Draft | LearningSkillStatus::Revoked => Ok(()),
        LearningSkillStatus::Verified | LearningSkillStatus::Canary => {
            ensure_has_verification_evidence(next)
        }
        LearningSkillStatus::Active => {
            ensure_has_verification_evidence(next)?;
            if next.canary_failure_count > 0 {
                bail!("active promoted skills require zero canary failures");
            }
            if next.canary_success_count == 0 {
                bail!("active promoted skills require at least one successful canary rollout");
            }
            Ok(())
        }
    }
}

fn ensure_has_verification_evidence(record: &LearningSkillView) -> Result<()> {
    if record.verification_status != LearningVerificationStatus::Verified
        || !record.real_daemon_verified
        || record.successful_run_count == 0
        || record.verifier_run_ids.is_empty()
    {
        bail!("promoted skills require daemon-validated verification evidence");
    }
    Ok(())
}

fn reset_rollout_evidence(record: &mut LearningSkillView) {
    record.verification_status = LearningVerificationStatus::Unverified;
    record.successful_run_count = 0;
    record.distinct_session_count = 0;
    record.verifier_run_ids.clear();
    record.real_daemon_verified = false;
    record.last_verified_workspace_digest = None;
    record.canary_success_count = 0;
    record.canary_failure_count = 0;
}

fn effective_definition_fingerprint(record: &LearningSkillView) -> String {
    if record.definition_fingerprint.is_empty() {
        learning_skill_definition_fingerprint(record)
    } else {
        record.definition_fingerprint.clone()
    }
}

fn rollout_evidence_ref(
    result: &LearningSkillRolloutResult,
    note: &'static str,
    record: &LearningSkillView,
) -> LearningEvidenceRef {
    LearningEvidenceRef {
        run_id: Some(result.run_id.clone()),
        artifact_id: None,
        note: Some(format!(
            "{note}; session={}; definition_fingerprint={}; recorded_at_ms={}",
            result.session_id, record.definition_fingerprint, result.recorded_at_ms
        )),
    }
}

fn ensure_promotable_runtime(runtime: &kheish_skills::SkillRuntimeConfig) -> Result<()> {
    if runtime.context != SkillExecutionContext::Fork {
        bail!("promoted procedure skills must use fork context");
    }
    if runtime.agent_profile.as_deref() != Some("verification") {
        bail!("promoted procedure skills must use the verification agent profile");
    }
    Ok(())
}

fn verification_status_for_skill_status(
    status: &LearningSkillStatus,
    existing: &kheish_types::LearningVerificationStatus,
) -> kheish_types::LearningVerificationStatus {
    match status {
        LearningSkillStatus::Draft => existing.clone(),
        LearningSkillStatus::Verified
        | LearningSkillStatus::Canary
        | LearningSkillStatus::Active => kheish_types::LearningVerificationStatus::Verified,
        LearningSkillStatus::Revoked => existing.clone(),
    }
}

fn paths_match(left: &Path, right: &Path) -> bool {
    canonical_or_fallback(left) == canonical_or_fallback(right)
}

fn canonical_or_fallback(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn ensure_promotable_learning(learning: &LearningView) -> Result<()> {
    if learning.kind != LearningKind::Procedure {
        bail!(
            "learning {} is not a procedure learning",
            learning.learning_id
        );
    }
    if learning.status != LearningStatus::Active {
        bail!(
            "learning {} must be active before promotion",
            learning.learning_id
        );
    }
    if learning.publish_tier != LearningPublishTier::Active {
        bail!(
            "learning {} must use the active publish tier before promotion",
            learning.learning_id
        );
    }
    if learning.scope.kind != LearningScopeKind::Workspace {
        bail!(
            "learning {} must use workspace scope before promotion",
            learning.learning_id
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use anyhow::Result;
    use kheish_skills::{SharedSkillRegistry, SkillRoot, SkillRuntimeConfig, SkillScope};
    use kheish_types::{
        LearningScope, LearningScopeKind, LearningVerificationStatus, SkillExecutionContext,
    };
    use tempfile::tempdir;

    use super::{LearningSkillService, paths_match};
    use crate::learning::LearningView;
    use crate::procedural_skills::{
        FileLearningSkillStore, LearningSkillDraft, LearningSkillRolloutKind,
        LearningSkillRolloutResult, LearningSkillStatus, LearningSkillView,
    };
    use kheish_types::{
        LearningPolicyDecision, LearningPublishTier, LearningSensitivity, LearningSourceRef,
        LearningStatus,
    };

    fn sample_learning(learning_id: &str) -> LearningView {
        LearningView {
            learning_id: learning_id.to_string(),
            scope: LearningScope {
                kind: LearningScopeKind::Workspace,
                id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
            },
            kind: kheish_types::LearningKind::Procedure,
            sensitivity: LearningSensitivity::Scoped,
            content: "Inspect live_support.rs before adding a live procedural scenario."
                .to_string(),
            confidence: 95,
            source: LearningSourceRef::default(),
            evidence_refs: Vec::new(),
            source_candidate_id: None,
            created_at_ms: 1,
            published_at_ms: 2,
            expires_at_ms: None,
            status: LearningStatus::Active,
            publish_tier: LearningPublishTier::Active,
            policy_decision: Some(LearningPolicyDecision::Manual),
            policy_actor: Some("operator".to_string()),
            verification_status: LearningVerificationStatus::Verified,
            supersedes: None,
            superseded_by: None,
            revoked_at_ms: None,
            revoked_reason: None,
        }
    }

    fn service(temp: &std::path::Path) -> LearningSkillService {
        LearningSkillService::new(
            FileLearningSkillStore::new(temp),
            BTreeMap::new(),
            Arc::new(SharedSkillRegistry::load_from_roots(vec![SkillRoot {
                path: temp.join("skills"),
                scope: SkillScope::Explicit,
            }])),
        )
    }

    fn rollout_result(
        kind: LearningSkillRolloutKind,
        run_id: &str,
        success: bool,
    ) -> LearningSkillRolloutResult {
        LearningSkillRolloutResult {
            kind,
            run_id: run_id.to_string(),
            session_id: "session-1".to_string(),
            success,
            definition_fingerprint: None,
            recorded_at_ms: 42,
        }
    }

    #[tokio::test]
    async fn service_promotes_procedure_learning_into_reloadable_skill() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        let draft = service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:procedural-check".to_string(),
                    description: Some("Run the procedural checker in a child agent.".to_string()),
                    when_to_use: Some(
                        "Use when the user explicitly asks for learning:procedural-check."
                            .to_string(),
                    ),
                    version: Some("1".to_string()),
                    instructions: "Reply with exactly `PROCEDURAL_SKILL_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        agent_profile: Some("verification".to_string()),
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Draft,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                10,
            )
            .await?;
        service
            .record_rollout_result(
                "learning:procedural-check",
                rollout_result(LearningSkillRolloutKind::Verification, "run-verify-1", true),
            )
            .await?;
        let _verified = service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:procedural-check".to_string(),
                    description: Some("Run the procedural checker in a child agent.".to_string()),
                    when_to_use: Some(
                        "Use when the user explicitly asks for learning:procedural-check."
                            .to_string(),
                    ),
                    version: Some("1".to_string()),
                    instructions: "Reply with exactly `PROCEDURAL_SKILL_OK`.".to_string(),
                    runtime: draft.runtime.clone(),
                    status: LearningSkillStatus::Verified,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                20,
            )
            .await?;
        service
            .record_rollout_result(
                "learning:procedural-check",
                rollout_result(LearningSkillRolloutKind::Verification, "run-verify-3", true),
            )
            .await?;
        service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:procedural-check".to_string(),
                    description: Some("Run the procedural checker in a child agent.".to_string()),
                    when_to_use: Some(
                        "Use when the user explicitly asks for learning:procedural-check."
                            .to_string(),
                    ),
                    version: Some("1".to_string()),
                    instructions: "Reply with exactly `PROCEDURAL_SKILL_OK`.".to_string(),
                    runtime: draft.runtime.clone(),
                    status: LearningSkillStatus::Canary,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                25,
            )
            .await?;
        service
            .record_rollout_result(
                "learning:procedural-check",
                rollout_result(LearningSkillRolloutKind::Canary, "run-canary-1", true),
            )
            .await?;
        let promoted = service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:procedural-check".to_string(),
                    description: Some("Run the procedural checker in a child agent.".to_string()),
                    when_to_use: Some(
                        "Use when the user explicitly asks for learning:procedural-check."
                            .to_string(),
                    ),
                    version: Some("1".to_string()),
                    instructions: "Reply with exactly `PROCEDURAL_SKILL_OK`.".to_string(),
                    runtime: draft.runtime,
                    status: LearningSkillStatus::Active,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                30,
            )
            .await?;

        assert_eq!(promoted.skill_name, "learning:procedural-check");
        assert!(service.get("learning:procedural-check").await.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn service_can_progress_promoted_skill_from_draft_to_active() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        let draft = service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:progressive".to_string(),
                    description: Some("Progressive promoted skill.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `PROGRESSIVE_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Draft,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                10,
            )
            .await?;
        assert_eq!(draft.status, LearningSkillStatus::Draft);
        assert!(service.skills.get("learning:progressive").is_none());

        service
            .record_rollout_result(
                "learning:progressive",
                rollout_result(LearningSkillRolloutKind::Verification, "run-verify-2", true),
            )
            .await?;
        let verified = service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:progressive".to_string(),
                    description: Some("Progressive promoted skill.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `PROGRESSIVE_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Verified,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                20,
            )
            .await?;
        assert_eq!(verified.status, LearningSkillStatus::Verified);

        let canary = service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:progressive".to_string(),
                    description: Some("Progressive promoted skill.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `PROGRESSIVE_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Canary,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                25,
            )
            .await?;
        assert_eq!(canary.status, LearningSkillStatus::Canary);
        service
            .record_rollout_result(
                "learning:progressive",
                rollout_result(LearningSkillRolloutKind::Canary, "run-canary-2", true),
            )
            .await?;
        let active = service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:progressive".to_string(),
                    description: Some("Progressive promoted skill.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `PROGRESSIVE_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Active,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                30,
            )
            .await?;
        assert_eq!(active.status, LearningSkillStatus::Active);
        assert!(active.lifecycle_events.iter().any(|event| {
            event.event == "promote"
                && event.from_status == Some(LearningSkillStatus::Canary)
                && event.to_status == LearningSkillStatus::Active
                && event.definition_fingerprint.as_deref()
                    == Some(active.definition_fingerprint.as_str())
        }));
        assert!(active.lifecycle_events.iter().any(|event| {
            event.event == "rollout_result"
                && event.rollout_kind == Some(LearningSkillRolloutKind::Verification)
                && event.run_id.as_deref() == Some("run-verify-2")
                && event.success == Some(true)
        }));
        assert!(active.lifecycle_events.iter().any(|event| {
            event.event == "rollout_result"
                && event.rollout_kind == Some(LearningSkillRolloutKind::Canary)
                && event.run_id.as_deref() == Some("run-canary-2")
                && event.success == Some(true)
        }));
        assert!(service.skills.get("learning:progressive").is_some());
        Ok(())
    }

    #[tokio::test]
    async fn service_requires_verified_evidence_and_successful_canary_before_active() -> Result<()>
    {
        let temp = tempdir()?;
        let service = service(temp.path());
        service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:gated".to_string(),
                    description: Some("Gated promoted skill.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `GATED_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Draft,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                10,
            )
            .await?;

        let no_verification = service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:gated".to_string(),
                    description: Some("Gated promoted skill.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `GATED_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Verified,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                20,
            )
            .await
            .expect_err("verified rollout should require daemon evidence");
        assert!(
            no_verification
                .to_string()
                .contains("verification evidence")
        );

        service
            .record_rollout_result(
                "learning:gated",
                rollout_result(
                    LearningSkillRolloutKind::Verification,
                    "run-verify-gated",
                    true,
                ),
            )
            .await?;
        service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:gated".to_string(),
                    description: Some("Gated promoted skill.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `GATED_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Verified,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                30,
            )
            .await?;
        let no_canary = service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:gated".to_string(),
                    description: Some("Gated promoted skill.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `GATED_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Active,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                40,
            )
            .await
            .expect_err("active rollout should require canary success");
        assert!(no_canary.to_string().contains("successful canary"));

        service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:gated".to_string(),
                    description: Some("Gated promoted skill.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `GATED_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Canary,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                50,
            )
            .await?;
        let failed_canary = service
            .record_rollout_result(
                "learning:gated",
                rollout_result(LearningSkillRolloutKind::Canary, "run-canary-failed", false),
            )
            .await?;
        assert_eq!(failed_canary.canary_failure_count, 1);
        let blocked = service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:gated".to_string(),
                    description: Some("Gated promoted skill.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `GATED_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Active,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                60,
            )
            .await
            .expect_err("failed canary should block active rollout");
        assert!(blocked.to_string().contains("zero canary failures"));
        Ok(())
    }

    #[tokio::test]
    async fn service_rejects_rollout_evidence_for_stale_definition_fingerprint() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        let draft = service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:fingerprint-gated".to_string(),
                    description: Some("Fingerprint gated promoted skill.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `FINGERPRINT_GATED_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Draft,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                10,
            )
            .await?;

        let mut stale = rollout_result(
            LearningSkillRolloutKind::Verification,
            "run-stale-fingerprint",
            true,
        );
        stale.definition_fingerprint = Some("stale-definition-fingerprint".to_string());
        let error = service
            .record_rollout_result("learning:fingerprint-gated", stale)
            .await
            .expect_err("stale rollout evidence fingerprint should be rejected");
        assert!(
            error.to_string().contains("definition_fingerprint"),
            "unexpected error: {error:?}"
        );

        let mut current = rollout_result(
            LearningSkillRolloutKind::Verification,
            "run-current-fingerprint",
            true,
        );
        current.definition_fingerprint = Some(draft.definition_fingerprint.clone());
        let verified = service
            .record_rollout_result("learning:fingerprint-gated", current)
            .await?;
        assert!(
            verified
                .verifier_run_ids
                .iter()
                .any(|run_id| run_id == "run-current-fingerprint")
        );
        assert!(verified.evidence_refs.iter().any(|evidence| {
            evidence
                .note
                .as_deref()
                .is_some_and(|note| note.contains(&draft.definition_fingerprint))
        }));
        Ok(())
    }

    #[tokio::test]
    async fn service_rejects_non_procedure_learning_promotion() {
        let temp = tempdir().expect("tempdir");
        let service = service(temp.path());
        let mut learning = sample_learning("learning-1");
        learning.kind = kheish_types::LearningKind::Fact;

        let error = service
            .promote(
                &learning,
                LearningSkillDraft {
                    skill_name: "learning:fact".to_string(),
                    description: Some("fact".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `FACT`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Draft,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                10,
            )
            .await
            .expect_err("only procedure learnings should be promotable");
        assert!(error.to_string().contains("not a procedure learning"));
    }

    #[tokio::test]
    async fn service_rejects_non_workspace_learning_promotion() {
        let temp = tempdir().expect("tempdir");
        let service = service(temp.path());
        let mut learning = sample_learning("learning-1");
        learning.scope.kind = LearningScopeKind::Session;
        learning.scope.id = "session-1".to_string();

        let error = service
            .promote(
                &learning,
                LearningSkillDraft {
                    skill_name: "learning:fact".to_string(),
                    description: Some("fact".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `FACT`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Draft,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                10,
            )
            .await
            .expect_err("session-scoped procedures should stay out of the global skill catalog");
        assert!(error.to_string().contains("must use workspace scope"));
    }

    #[tokio::test]
    async fn service_rejects_inline_promoted_skills() {
        let temp = tempdir().expect("tempdir");
        let service = service(temp.path());

        let error = service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:inline".to_string(),
                    description: Some("inline".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `INLINE`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Inline,
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Draft,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                10,
            )
            .await
            .expect_err("promoted procedure skills should always fork");
        assert!(error.to_string().contains("must use fork context"));
    }

    #[tokio::test]
    async fn service_rejects_non_verification_promoted_profiles() {
        let temp = tempdir().expect("tempdir");
        let service = service(temp.path());

        let error = service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:wide-profile".to_string(),
                    description: Some("wide".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `NOPE`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        agent_profile: Some("default".to_string()),
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Draft,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                10,
            )
            .await
            .expect_err("promoted procedure skills must stay on verification profile");
        assert!(error.to_string().contains("verification agent profile"));
    }

    #[tokio::test]
    async fn service_uses_non_sensitive_default_description() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        let mut learning = sample_learning("learning-1");
        learning.content =
            "Step 1:\nInspect the secret procedure.\nStep 2:\nDo not leak it.".to_string();

        let promoted = service
            .promote(
                &learning,
                LearningSkillDraft {
                    skill_name: "learning:default-description".to_string(),
                    description: None,
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Draft,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                10,
            )
            .await?;

        assert_eq!(
            promoted.description,
            "Execute this promoted procedure in a dedicated child agent."
        );
        assert!(!promoted.description.contains("secret procedure"));
        Ok(())
    }

    #[tokio::test]
    async fn service_rejects_secret_like_operator_reasons() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:secret-reason".to_string(),
                    description: Some("Run the procedure in a child agent.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        agent_profile: Some("verification".to_string()),
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Draft,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                10,
            )
            .await?;

        let revoke_error = service
            .revoke(
                "learning:secret-reason",
                20,
                Some(format!("api-key: {}{}", "sk-", "proj-procedural-secret")),
            )
            .await
            .expect_err("revocation reason should reject secret material");
        assert!(
            revoke_error
                .to_string()
                .contains("revocation reason appears to contain secret material"),
            "unexpected error: {revoke_error:?}"
        );
        assert_eq!(
            service
                .get("learning:secret-reason")
                .await
                .expect("skill should still exist")
                .status,
            LearningSkillStatus::Draft
        );

        let rollback_error = service
            .rollback(
                "learning:secret-reason",
                21,
                Some("clientSecret: hunter2hunter2".to_string()),
            )
            .await
            .expect_err("rollback reason should reject secret material before persistence");
        assert!(
            rollback_error
                .to_string()
                .contains("rollback reason appears to contain secret material"),
            "unexpected error: {rollback_error:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn service_revokes_promoted_skill_and_unloads_it() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:procedural-check".to_string(),
                    description: Some("Run the procedural checker in a child agent.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `PROCEDURAL_SKILL_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        agent_profile: Some("verification".to_string()),
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Draft,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                10,
            )
            .await?;
        service
            .record_rollout_result(
                "learning:procedural-check",
                rollout_result(LearningSkillRolloutKind::Verification, "run-verify-3", true),
            )
            .await?;
        service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:procedural-check".to_string(),
                    description: Some("Run the procedural checker in a child agent.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `PROCEDURAL_SKILL_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        agent_profile: Some("verification".to_string()),
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Verified,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                15,
            )
            .await?;
        service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:procedural-check".to_string(),
                    description: Some("Run the procedural checker in a child agent.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `PROCEDURAL_SKILL_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        agent_profile: Some("verification".to_string()),
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Canary,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                18,
            )
            .await?;
        service
            .record_rollout_result(
                "learning:procedural-check",
                rollout_result(LearningSkillRolloutKind::Canary, "run-canary-3", true),
            )
            .await?;
        service
            .promote(
                &sample_learning("learning-1"),
                LearningSkillDraft {
                    skill_name: "learning:procedural-check".to_string(),
                    description: Some("Run the procedural checker in a child agent.".to_string()),
                    when_to_use: None,
                    version: None,
                    instructions: "Reply with exactly `PROCEDURAL_SKILL_OK`.".to_string(),
                    runtime: SkillRuntimeConfig {
                        context: SkillExecutionContext::Fork,
                        agent_profile: Some("verification".to_string()),
                        ..SkillRuntimeConfig::default()
                    },
                    status: LearningSkillStatus::Active,
                    evidence_refs: Vec::new(),
                    verification_status: LearningVerificationStatus::Unverified,
                    successful_run_count: 0,
                    distinct_session_count: 0,
                    verifier_run_ids: Vec::new(),
                    real_daemon_verified: false,
                    last_verified_workspace_digest: None,
                    canary_success_count: 0,
                    canary_failure_count: 0,
                },
                20,
            )
            .await?;

        let revoked = service
            .revoke("learning:procedural-check", 20, Some("stale".to_string()))
            .await?;
        assert_eq!(
            revoked.status,
            crate::procedural_skills::LearningSkillStatus::Revoked
        );
        assert!(service.get("learning:procedural-check").await.is_some());
        assert!(service.skills.get("learning:procedural-check").is_none());

        let rolled_back = service
            .rollback(
                "learning:procedural-check",
                30,
                Some("operator rollback".to_string()),
            )
            .await?;
        assert_eq!(rolled_back.status, LearningSkillStatus::Active);
        assert!(service.skills.get("learning:procedural-check").is_some());
        Ok(())
    }

    #[tokio::test]
    async fn repair_catalog_rewrites_missing_owned_skill_without_rebinding_to_foreign_root()
    -> Result<()> {
        let temp = tempdir()?;
        let daemon_store = FileLearningSkillStore::new(temp.path());
        let foreign_root = temp.path().join("foreign-skills");
        std::fs::create_dir_all(foreign_root.join("learning:procedural-check"))?;
        std::fs::write(
            foreign_root
                .join("learning:procedural-check")
                .join("SKILL.md"),
            "---\nname: learning:procedural-check\ndescription: foreign\n---\nReply with exactly `FOREIGN`.\n",
        )?;
        let record = LearningSkillView {
            skill_name: "learning:procedural-check".to_string(),
            source_learning_id: "learning-1".to_string(),
            source_scope: LearningScope {
                kind: LearningScopeKind::Workspace,
                id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
            },
            status: LearningSkillStatus::Active,
            description: "Run the procedural checker in a child agent.".to_string(),
            when_to_use: None,
            version: None,
            instructions: "Reply with exactly `OWNED`.".to_string(),
            skill_path: daemon_store
                .expected_skill_path("learning-1")
                .display()
                .to_string(),
            skill_root: daemon_store
                .expected_skill_root("learning-1")
                .display()
                .to_string(),
            digest: String::new(),
            definition_fingerprint: String::new(),
            runtime: SkillRuntimeConfig {
                context: SkillExecutionContext::Fork,
                ..SkillRuntimeConfig::default()
            },
            evidence_refs: Vec::new(),
            lifecycle_events: Vec::new(),
            verification_status: LearningVerificationStatus::Verified,
            successful_run_count: 2,
            distinct_session_count: 1,
            verifier_run_ids: vec![
                "run-verify-owned".to_string(),
                "run-canary-owned".to_string(),
            ],
            real_daemon_verified: true,
            last_verified_workspace_digest: None,
            canary_success_count: 1,
            canary_failure_count: 0,
            promoted_at_ms: 10,
            revoked_at_ms: None,
            revoked_reason: None,
        };
        daemon_store.save(&record)?;
        let service = LearningSkillService::new(
            daemon_store.clone(),
            daemon_store.load()?,
            Arc::new(SharedSkillRegistry::load_from_roots(vec![
                SkillRoot {
                    path: temp.path().join("skills"),
                    scope: SkillScope::Explicit,
                },
                SkillRoot {
                    path: foreign_root,
                    scope: SkillScope::Explicit,
                },
            ])),
        );

        service.repair_catalog().await?;
        let loaded = service
            .skills
            .get("learning:procedural-check")
            .expect("promoted skill should load from the daemon-owned root");
        assert!(paths_match(
            &loaded.skill_root,
            &temp
                .path()
                .join("skills")
                .join("procedural")
                .join("learning-1")
        ));
        assert!(loaded.instructions.contains("OWNED"));

        std::fs::write(
            daemon_store.expected_skill_path("learning-1"),
            "---\nname: learning:procedural-check\ndescription: tampered\n---\nReply with exactly `TAMPERED`.\n",
        )?;
        service.skills.reload();
        let tampered = service
            .skills
            .get("learning:procedural-check")
            .expect("tampered promoted skill should load before repair");
        assert!(tampered.instructions.contains("TAMPERED"));

        service.repair_catalog().await?;
        let repaired = service
            .skills
            .get("learning:procedural-check")
            .expect("promoted skill should be restored from daemon record");
        assert_eq!(repaired.description, record.description);
        assert!(repaired.instructions.contains("OWNED"));
        assert!(!repaired.instructions.contains("TAMPERED"));
        Ok(())
    }
}
