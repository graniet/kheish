use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};
use tokio::sync::Mutex;

use crate::learning::{
    FileLearningStore, LearningAutomationReview, LearningCandidateOrigin, LearningCandidateState,
    LearningCandidateView, LearningView, semantic_learning_content_key,
    semantic_learning_subject_key, validate_learning_scope,
};
use kheish_types::{
    LearnedContextBundle, LearnedContextEntry, LearningKind, LearningPolicyDecision,
    LearningPublishTier, LearningScope, LearningSensitivity, LearningSourceRef, LearningStatus,
    LearningVerificationStatus,
};

#[derive(Default)]
struct LearningIndexes {
    candidates_by_scope: BTreeMap<String, BTreeSet<String>>,
    records_by_scope: BTreeMap<String, BTreeSet<String>>,
    run_summary_candidates_by_run: BTreeMap<String, String>,
}

/// Query filters applied while listing learning candidates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LearningCandidateListFilter {
    pub(crate) query: Option<String>,
    pub(crate) scope: Option<LearningScope>,
    pub(crate) kind: Option<LearningKind>,
    pub(crate) state: Option<LearningCandidateState>,
}

/// Query filters applied while listing published learnings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LearningListFilter {
    pub(crate) query: Option<String>,
    pub(crate) scope: Option<LearningScope>,
    pub(crate) kind: Option<LearningKind>,
    pub(crate) status: Option<LearningStatus>,
    pub(crate) policy_decision: Option<LearningPolicyDecision>,
    pub(crate) policy_actor: Option<String>,
    pub(crate) matched_rule_name: Option<String>,
}

/// Deterministic query match retained for session-memory retrieval and explainability.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LearningRetrievalMatch {
    pub(crate) score: u64,
    pub(crate) matched_fields: Vec<String>,
}

/// Owns durable learning candidates and published learning records.
pub(crate) struct LearningService {
    store: FileLearningStore,
    mutation_lock: Mutex<()>,
    candidates: Mutex<BTreeMap<String, LearningCandidateView>>,
    records: Mutex<BTreeMap<String, LearningView>>,
    indexes: Mutex<LearningIndexes>,
    next_candidate_id: AtomicU64,
    next_learning_id: AtomicU64,
}

impl LearningService {
    /// Creates a new learning service backed by one file store.
    pub(crate) fn new(
        store: FileLearningStore,
        candidates: BTreeMap<String, LearningCandidateView>,
        records: BTreeMap<String, LearningView>,
        next_candidate_id: AtomicU64,
        next_learning_id: AtomicU64,
    ) -> Self {
        let indexes = build_indexes(&candidates, &records);
        Self {
            store,
            mutation_lock: Mutex::new(()),
            candidates: Mutex::new(candidates),
            records: Mutex::new(records),
            indexes: Mutex::new(indexes),
            next_candidate_id,
            next_learning_id,
        }
    }

    /// Returns one fresh daemon-managed candidate identifier.
    pub(crate) fn next_candidate_id(&self) -> String {
        format!(
            "learning-candidate-{}",
            self.next_candidate_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Returns one fresh daemon-managed published learning identifier.
    pub(crate) fn next_learning_id(&self) -> String {
        format!(
            "learning-{}",
            self.next_learning_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Returns every candidate that matches the provided filter.
    pub(crate) async fn list_candidates(
        &self,
        filter: &LearningCandidateListFilter,
    ) -> Vec<LearningCandidateView> {
        let normalized = filter
            .query
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase);
        self.candidates
            .lock()
            .await
            .values()
            .filter(|candidate| {
                filter
                    .scope
                    .as_ref()
                    .is_none_or(|scope| &candidate.scope == scope)
                    && filter
                        .kind
                        .as_ref()
                        .is_none_or(|kind| &candidate.kind == kind)
                    && filter
                        .state
                        .as_ref()
                        .is_none_or(|state| &candidate.state == state)
                    && normalized
                        .as_ref()
                        .is_none_or(|query| candidate_matches_query(candidate, query))
            })
            .cloned()
            .collect()
    }

    /// Returns one candidate by identifier when it exists.
    pub(crate) async fn get_candidate(&self, candidate_id: &str) -> Option<LearningCandidateView> {
        self.candidates.lock().await.get(candidate_id).cloned()
    }

    /// Returns true when one daemon-owned non-summary candidate already exists for the run.
    pub(crate) async fn has_daemon_semantic_candidate_for_run(&self, run_id: &str) -> bool {
        self.candidates.lock().await.values().any(|candidate| {
            candidate.origin == LearningCandidateOrigin::Daemon
                && candidate.kind != LearningKind::RunSummary
                && candidate.source.run_id.as_deref() == Some(run_id)
        })
    }

    /// Returns candidate identifiers that are still eligible for daemon automation.
    pub(crate) async fn pending_candidate_ids(&self) -> Vec<String> {
        self.candidates
            .lock()
            .await
            .values()
            .filter(|candidate| candidate.state == LearningCandidateState::Pending)
            .map(|candidate| candidate.candidate_id.clone())
            .collect()
    }

    /// Persists one new learning candidate and registers it in the in-memory caches.
    pub(crate) async fn create_candidate(
        &self,
        candidate: LearningCandidateView,
    ) -> Result<LearningCandidateView> {
        candidate.validate()?;
        self.store.save_candidate(&candidate)?;
        self.candidates
            .lock()
            .await
            .insert(candidate.candidate_id.clone(), candidate.clone());
        let mut indexes = self.indexes.lock().await;
        indexes
            .candidates_by_scope
            .entry(candidate.scope.scope_key())
            .or_default()
            .insert(candidate.candidate_id.clone());
        if candidate.kind == LearningKind::RunSummary
            && let Some(run_id) = candidate.source.run_id.clone()
        {
            indexes
                .run_summary_candidates_by_run
                .insert(run_id, candidate.candidate_id.clone());
        }
        Ok(candidate)
    }

    /// Creates one daemon-owned candidate when an equivalent candidate does not already exist.
    pub(crate) async fn ensure_daemon_candidate(
        &self,
        candidate: LearningCandidateView,
    ) -> Result<Option<LearningCandidateView>> {
        if candidate.origin != LearningCandidateOrigin::Daemon {
            bail!("ensure_daemon_candidate requires daemon-owned provenance");
        }
        candidate.validate()?;
        let _mutation = self.mutation_lock.lock().await;
        if self
            .candidates
            .lock()
            .await
            .values()
            .any(|existing| daemon_candidate_matches(existing, &candidate))
        {
            return Ok(None);
        }
        self.store.save_candidate(&candidate)?;
        self.candidates
            .lock()
            .await
            .insert(candidate.candidate_id.clone(), candidate.clone());
        let mut indexes = self.indexes.lock().await;
        indexes
            .candidates_by_scope
            .entry(candidate.scope.scope_key())
            .or_default()
            .insert(candidate.candidate_id.clone());
        if candidate.kind == LearningKind::RunSummary
            && let Some(run_id) = candidate.source.run_id.clone()
        {
            indexes
                .run_summary_candidates_by_run
                .insert(run_id, candidate.candidate_id.clone());
        }
        Ok(Some(candidate))
    }

    /// Creates a session-scoped run-summary candidate when it does not already exist.
    pub(crate) async fn ensure_run_summary_candidate(
        &self,
        session_id: &str,
        agent_id: &str,
        run_id: &str,
        content: &str,
        created_at_ms: u64,
        expires_at_ms: Option<u64>,
    ) -> Result<Option<LearningCandidateView>> {
        if content.trim().is_empty() {
            return Ok(None);
        }
        let _mutation = self.mutation_lock.lock().await;
        if self
            .indexes
            .lock()
            .await
            .run_summary_candidates_by_run
            .contains_key(run_id)
        {
            return Ok(None);
        }
        let candidate = LearningCandidateView {
            candidate_id: self.next_candidate_id(),
            origin: LearningCandidateOrigin::Daemon,
            scope: LearningScope {
                kind: kheish_types::LearningScopeKind::Session,
                id: session_id.to_string(),
            },
            kind: LearningKind::RunSummary,
            sensitivity: kheish_types::LearningSensitivity::Sensitive,
            content: content.trim().to_string(),
            confidence: 100,
            source: LearningSourceRef {
                run_id: Some(run_id.to_string()),
                session_id: Some(session_id.to_string()),
                agent_id: Some(agent_id.to_string()),
                ..LearningSourceRef::default()
            },
            evidence_refs: Vec::new(),
            created_at_ms,
            expires_at_ms,
            state: LearningCandidateState::Pending,
            automation_review: None,
            published_learning_id: None,
        };
        candidate.validate()?;
        self.store.save_candidate(&candidate)?;
        self.candidates
            .lock()
            .await
            .insert(candidate.candidate_id.clone(), candidate.clone());
        let mut indexes = self.indexes.lock().await;
        indexes
            .candidates_by_scope
            .entry(candidate.scope.scope_key())
            .or_default()
            .insert(candidate.candidate_id.clone());
        indexes
            .run_summary_candidates_by_run
            .insert(run_id.to_string(), candidate.candidate_id.clone());
        Ok(Some(candidate))
    }

    /// Returns every published learning that matches the provided filter.
    pub(crate) async fn list(&self, filter: &LearningListFilter) -> Vec<LearningView> {
        let normalized = filter
            .query
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase);
        let normalized_policy_actor = filter
            .policy_actor
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase);
        let normalized_rule_name = filter
            .matched_rule_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase);
        let candidates = self.candidates.lock().await;
        self.records
            .lock()
            .await
            .values()
            .filter(|record| {
                filter
                    .scope
                    .as_ref()
                    .is_none_or(|scope| &record.scope == scope)
                    && filter.kind.as_ref().is_none_or(|kind| &record.kind == kind)
                    && filter
                        .status
                        .as_ref()
                        .is_none_or(|status| &record.status == status)
                    && filter
                        .policy_decision
                        .as_ref()
                        .is_none_or(|policy_decision| {
                            record.policy_decision.as_ref() == Some(policy_decision)
                        })
                    && normalized_policy_actor.as_ref().is_none_or(|policy_actor| {
                        record
                            .policy_actor
                            .as_deref()
                            .is_some_and(|value| value.to_ascii_lowercase() == *policy_actor)
                    })
                    && normalized_rule_name.as_ref().is_none_or(|rule_name| {
                        record
                            .source_candidate_id
                            .as_deref()
                            .and_then(|candidate_id| candidates.get(candidate_id))
                            .and_then(|candidate| candidate.automation_review.as_ref())
                            .and_then(|review| review.matched_rule_name.as_deref())
                            .is_some_and(|value| value.to_ascii_lowercase() == *rule_name)
                    })
                    && normalized
                        .as_ref()
                        .is_none_or(|query| record_matches_query(record, query))
            })
            .cloned()
            .collect()
    }

    /// Returns one published learning by identifier when it exists.
    pub(crate) async fn get(&self, learning_id: &str) -> Option<LearningView> {
        self.records.lock().await.get(learning_id).cloned()
    }

    /// Publishes one pending learning candidate into the durable learning store.
    pub(crate) async fn publish_candidate(
        &self,
        candidate_id: &str,
        mut learning: LearningView,
    ) -> Result<LearningView> {
        let _mutation = self.mutation_lock.lock().await;
        learning.validate()?;
        if learning.supersedes.is_some() && learning.publish_tier != LearningPublishTier::Active {
            bail!("superseding learnings must use the active publish tier");
        }
        let candidate = self
            .candidates
            .lock()
            .await
            .get(candidate_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown learning candidate {candidate_id}"))?;
        if candidate.state == LearningCandidateState::Rejected {
            bail!("learning candidate {candidate_id} was rejected");
        }
        if let Some(existing) = self.find_learning_by_candidate(candidate_id).await {
            self.persist_published_candidate(candidate, &existing.learning_id)
                .await?;
            if let Some(superseded_id) = existing.supersedes.as_deref() {
                self.ensure_superseded_link(superseded_id, &existing.learning_id)
                    .await?;
            }
            return Ok(existing);
        }
        if candidate.state == LearningCandidateState::Published {
            bail!("learning candidate {candidate_id} was already published");
        }
        if let Some(existing) = self
            .find_equivalent_prompt_visible_learning(&learning)
            .await
        {
            self.persist_published_candidate(candidate, &existing.learning_id)
                .await?;
            return Ok(existing);
        }
        if let Some(superseded_id) = learning.supersedes.clone() {
            if let Some(existing) = self.find_live_replacement_for(&superseded_id).await {
                self.ensure_superseded_link(&superseded_id, &existing.learning_id)
                    .await?;
                self.persist_published_candidate(candidate, &existing.learning_id)
                    .await?;
                return Ok(existing);
            }
            let superseded = self.ensure_supersedable(&superseded_id).await?;
            if learning.scope != superseded.scope {
                bail!("superseding learning scope must match the source learning scope");
            }
        }
        if let Some(conflict) = self
            .active_conflicting_learning_for_learning(&learning)
            .await
        {
            bail!(
                "learning conflicts with active learning {}; publish with supersedes to replace it",
                conflict.learning_id
            );
        }

        learning.source_candidate_id = Some(candidate_id.to_string());
        self.store.save_learning(&learning)?;
        self.insert_learning_in_memory(learning.clone()).await;
        if let Some(superseded_id) = learning.supersedes.clone() {
            self.ensure_superseded_link(&superseded_id, &learning.learning_id)
                .await?;
        }

        self.persist_published_candidate(candidate, &learning.learning_id)
            .await?;
        Ok(learning)
    }

    /// Returns an active learning that appears to conflict with one candidate's semantic subject.
    pub(crate) async fn active_conflicting_learning_for_candidate(
        &self,
        candidate: &LearningCandidateView,
    ) -> Option<LearningView> {
        if !matches!(
            candidate.kind,
            LearningKind::Fact | LearningKind::Preference | LearningKind::Decision
        ) {
            return None;
        }
        let subject = semantic_learning_subject_key(&candidate.content)?;
        let candidate_content = semantic_learning_content_key(&candidate.content);
        self.records
            .lock()
            .await
            .values()
            .find(|record| {
                record.scope == candidate.scope
                    && record.kind == candidate.kind
                    && learning_is_prompt_visible(record)
                    && record
                        .expires_at_ms
                        .is_none_or(|expires_at_ms| expires_at_ms > candidate.created_at_ms)
                    && semantic_learning_subject_key(&record.content).as_deref()
                        == Some(subject.as_str())
                    && semantic_learning_content_key(&record.content) != candidate_content
            })
            .cloned()
    }

    /// Returns an active learning that appears to conflict with one draft learning's semantic subject.
    async fn active_conflicting_learning_for_learning(
        &self,
        learning: &LearningView,
    ) -> Option<LearningView> {
        if !matches!(
            learning.kind,
            LearningKind::Fact | LearningKind::Preference | LearningKind::Decision
        ) {
            return None;
        }
        let subject = semantic_learning_subject_key(&learning.content)?;
        let learning_content = semantic_learning_content_key(&learning.content);
        self.records
            .lock()
            .await
            .values()
            .find(|record| {
                learning.supersedes.as_deref() != Some(record.learning_id.as_str())
                    && record.scope == learning.scope
                    && record.kind == learning.kind
                    && learning_is_prompt_visible(record)
                    && record
                        .expires_at_ms
                        .is_none_or(|expires_at_ms| expires_at_ms > learning.published_at_ms)
                    && semantic_learning_subject_key(&record.content).as_deref()
                        == Some(subject.as_str())
                    && semantic_learning_content_key(&record.content) != learning_content
            })
            .cloned()
    }

    /// Supersedes one existing learning by publishing a new active record.
    pub(crate) async fn supersede(
        &self,
        learning_id: &str,
        replacement: LearningView,
    ) -> Result<LearningView> {
        let _mutation = self.mutation_lock.lock().await;
        replacement.validate()?;
        if replacement.publish_tier != LearningPublishTier::Active {
            bail!("superseding learnings must use the active publish tier");
        }
        if let Some(existing) = self.find_live_replacement_for(learning_id).await {
            self.ensure_superseded_link(learning_id, &existing.learning_id)
                .await?;
            return Ok(existing);
        }
        let current = self
            .records
            .lock()
            .await
            .get(learning_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown learning {learning_id}"))?;
        validate_learning_scope(&replacement.scope)?;
        if replacement.scope != current.scope {
            bail!("superseding learning scope must match the source learning scope");
        }
        if let Some(conflict) = self
            .active_conflicting_learning_for_learning(&replacement)
            .await
        {
            bail!(
                "learning conflicts with active learning {}; publish with supersedes to replace it",
                conflict.learning_id
            );
        }
        match current.status {
            LearningStatus::Active | LearningStatus::Provisional => {}
            LearningStatus::Superseded => {
                if let Some(existing_id) = current.superseded_by.as_deref()
                    && let Some(existing) = self.get(existing_id).await
                {
                    return Ok(existing);
                }
                bail!("learning {learning_id} was already superseded");
            }
            LearningStatus::Revoked => {
                bail!("cannot supersede revoked learning {learning_id}");
            }
        }
        self.store.save_learning(&replacement)?;
        self.insert_learning_in_memory(replacement.clone()).await;
        self.ensure_superseded_link(learning_id, &replacement.learning_id)
            .await?;
        Ok(replacement)
    }

    /// Revokes one published learning in place.
    pub(crate) async fn revoke(
        &self,
        learning_id: &str,
        revoked_at_ms: u64,
        reason: Option<String>,
    ) -> Result<LearningView> {
        let _mutation = self.mutation_lock.lock().await;
        let mut records = self.records.lock().await;
        let record = records
            .get(learning_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown learning {learning_id}"))?;
        if record.status == LearningStatus::Revoked {
            return Ok(record);
        }
        let mut updated = record.clone();
        updated.status = LearningStatus::Revoked;
        updated.revoked_at_ms = Some(revoked_at_ms);
        updated.revoked_reason = reason.filter(|value| !value.trim().is_empty());
        self.store.save_learning(&updated)?;
        records.insert(updated.learning_id.clone(), updated.clone());
        Ok(updated)
    }

    /// Restores one previously persisted learning record verbatim.
    pub(crate) async fn restore(&self, learning: LearningView) -> Result<LearningView> {
        let _mutation = self.mutation_lock.lock().await;
        learning.validate()?;
        self.store.save_learning(&learning)?;
        self.records
            .lock()
            .await
            .insert(learning.learning_id.clone(), learning.clone());
        self.indexes
            .lock()
            .await
            .records_by_scope
            .entry(learning.scope.scope_key())
            .or_default()
            .insert(learning.learning_id.clone());
        Ok(learning)
    }

    /// Rejects one pending learning candidate in place.
    pub(crate) async fn reject_candidate(
        &self,
        candidate_id: &str,
    ) -> Result<LearningCandidateView> {
        let _mutation = self.mutation_lock.lock().await;
        let mut candidates = self.candidates.lock().await;
        let candidate = candidates
            .get(candidate_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown learning candidate {candidate_id}"))?;
        if candidate.state == LearningCandidateState::Published {
            bail!("learning candidate {candidate_id} was already published");
        }
        if candidate.state == LearningCandidateState::Rejected {
            return Ok(candidate);
        }
        let mut updated = candidate.clone();
        updated.state = LearningCandidateState::Rejected;
        self.store.save_candidate(&updated)?;
        candidates.insert(updated.candidate_id.clone(), updated.clone());
        Ok(updated)
    }

    /// Records one daemon-owned automation review and optionally updates the candidate state.
    pub(crate) async fn record_automation_review(
        &self,
        candidate_id: &str,
        next_state: Option<LearningCandidateState>,
        review: LearningAutomationReview,
    ) -> Result<LearningCandidateView> {
        let _mutation = self.mutation_lock.lock().await;
        let mut candidates = self.candidates.lock().await;
        let candidate = candidates
            .get(candidate_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown learning candidate {candidate_id}"))?;
        if candidate.state == LearningCandidateState::Rejected {
            return Ok(candidate);
        }
        let mut updated = candidate.clone();
        updated.automation_review = Some(review);
        if let Some(next_state) = next_state
            && updated.state != LearningCandidateState::Published
        {
            updated.state = next_state;
        }
        self.store.save_candidate(&updated)?;
        candidates.insert(updated.candidate_id.clone(), updated.clone());
        Ok(updated)
    }

    /// Returns the current durable learnings visible through the provided scopes.
    pub(crate) async fn visible_records_for_scopes(
        &self,
        scopes: &[LearningScope],
        now_ms: u64,
    ) -> Vec<LearningView> {
        let records = self.records.lock().await;
        let indexes = self.indexes.lock().await;
        let shadowed_learning_ids = records
            .values()
            .filter(|record| {
                matches!(
                    record.status,
                    LearningStatus::Active | LearningStatus::Provisional
                ) && record.verification_status != LearningVerificationStatus::Failed
                    && record
                        .expires_at_ms
                        .is_none_or(|expires_at_ms| expires_at_ms > now_ms)
            })
            .filter_map(|record| record.supersedes.clone())
            .collect::<BTreeSet<_>>();
        let mut matched = Vec::new();
        for scope in scopes {
            let Some(learning_ids) = indexes.records_by_scope.get(&scope.scope_key()) else {
                continue;
            };
            for learning_id in learning_ids {
                if shadowed_learning_ids.contains(learning_id) {
                    continue;
                }
                let Some(record) = records.get(learning_id) else {
                    continue;
                };
                if !matches!(
                    record.status,
                    LearningStatus::Active | LearningStatus::Provisional
                ) || record.verification_status == LearningVerificationStatus::Failed
                {
                    continue;
                }
                if record
                    .expires_at_ms
                    .is_some_and(|expires_at_ms| expires_at_ms <= now_ms)
                {
                    continue;
                }
                matched.push(record.clone());
            }
        }
        matched.sort_by(|left, right| {
            scope_priority(&left.scope)
                .cmp(&scope_priority(&right.scope))
                .then_with(|| right.published_at_ms.cmp(&left.published_at_ms))
                .then_with(|| left.learning_id.cmp(&right.learning_id))
        });
        matched
    }

    /// Builds one prompt bundle from active learnings that match the provided scopes.
    pub(crate) async fn learned_context_bundle(
        &self,
        scopes: &[LearningScope],
        now_ms: u64,
    ) -> Option<LearnedContextBundle> {
        self.learned_context_bundle_for_query(scopes, now_ms, None)
            .await
    }

    /// Builds one prompt bundle and prioritizes learnings that match the current input.
    pub(crate) async fn learned_context_bundle_for_query(
        &self,
        scopes: &[LearningScope],
        now_ms: u64,
        query: Option<&str>,
    ) -> Option<LearnedContextBundle> {
        let normalized_query = normalize_learning_query(query);
        let mut matched = self
            .visible_records_for_scopes(scopes, now_ms)
            .await
            .into_iter()
            .filter(|record| record.kind.is_prompt_eligible())
            .filter(learning_is_prompt_visible)
            .collect::<Vec<_>>();
        if let Some(query) = normalized_query.as_deref() {
            let mut scored = matched
                .into_iter()
                .map(|record| {
                    let score = rank_learning_record_for_prompt(&record, Some(query));
                    (record, score)
                })
                .collect::<Vec<_>>();
            if scored.iter().any(|(_, score)| *score > 0) {
                scored.retain(|(_, score)| *score > 0);
            } else if !learning_query_explicitly_requests_memory(query) {
                return None;
            }
            scored.sort_by(|(left, left_score), (right, right_score)| {
                right_score
                    .cmp(left_score)
                    .then_with(|| scope_priority(&left.scope).cmp(&scope_priority(&right.scope)))
                    .then_with(|| right.published_at_ms.cmp(&left.published_at_ms))
                    .then_with(|| left.learning_id.cmp(&right.learning_id))
            });
            matched = scored.into_iter().map(|(record, _)| record).collect();
        }
        if matched.is_empty() {
            return None;
        }
        Some(LearnedContextBundle {
            entries: matched
                .into_iter()
                .map(|record| LearnedContextEntry {
                    learning_id: record.learning_id,
                    kind: record.kind,
                    published_at_ms: record.published_at_ms,
                    content: record.content,
                })
                .collect(),
            truncated: false,
        })
    }

    async fn ensure_supersedable(&self, learning_id: &str) -> Result<LearningView> {
        let record = self
            .records
            .lock()
            .await
            .get(learning_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown learning {learning_id}"))?;
        match record.status {
            LearningStatus::Active | LearningStatus::Provisional => Ok(record),
            LearningStatus::Superseded => bail!("learning {learning_id} was already superseded"),
            LearningStatus::Revoked => bail!("cannot supersede revoked learning {learning_id}"),
        }
    }

    async fn ensure_superseded_link(
        &self,
        learning_id: &str,
        replacement_learning_id: &str,
    ) -> Result<()> {
        let mut records = self.records.lock().await;
        let record = records
            .get(learning_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown learning {learning_id}"))?;
        if record.status == LearningStatus::Superseded
            && record.superseded_by.as_deref() == Some(replacement_learning_id)
        {
            return Ok(());
        }
        let mut updated = record.clone();
        match record.status {
            LearningStatus::Active | LearningStatus::Provisional => {
                updated.status = LearningStatus::Superseded;
                updated.superseded_by = Some(replacement_learning_id.to_string());
            }
            LearningStatus::Superseded => {
                bail!("learning {learning_id} was already superseded");
            }
            LearningStatus::Revoked => {
                bail!("cannot supersede revoked learning {learning_id}");
            }
        }
        self.store.save_learning(&updated)?;
        records.insert(updated.learning_id.clone(), updated);
        Ok(())
    }

    async fn find_learning_by_candidate(&self, candidate_id: &str) -> Option<LearningView> {
        self.records
            .lock()
            .await
            .values()
            .find(|record| record.source_candidate_id.as_deref() == Some(candidate_id))
            .cloned()
    }

    async fn find_live_replacement_for(&self, learning_id: &str) -> Option<LearningView> {
        self.records
            .lock()
            .await
            .values()
            .find(|record| {
                matches!(
                    record.status,
                    LearningStatus::Active | LearningStatus::Provisional
                ) && record.supersedes.as_deref() == Some(learning_id)
            })
            .cloned()
    }

    async fn find_equivalent_prompt_visible_learning(
        &self,
        learning: &LearningView,
    ) -> Option<LearningView> {
        let learning_content = semantic_learning_content_key(&learning.content);
        self.records
            .lock()
            .await
            .values()
            .find(|record| {
                record.scope == learning.scope
                    && record.kind == learning.kind
                    && learning_is_prompt_visible(record)
                    && record
                        .expires_at_ms
                        .is_none_or(|expires_at_ms| expires_at_ms > learning.published_at_ms)
                    && semantic_learning_content_key(&record.content) == learning_content
            })
            .cloned()
    }

    async fn insert_learning_in_memory(&self, learning: LearningView) {
        self.records
            .lock()
            .await
            .insert(learning.learning_id.clone(), learning.clone());
        self.indexes
            .lock()
            .await
            .records_by_scope
            .entry(learning.scope.scope_key())
            .or_default()
            .insert(learning.learning_id.clone());
    }

    async fn persist_published_candidate(
        &self,
        candidate: LearningCandidateView,
        learning_id: &str,
    ) -> Result<()> {
        let mut updated = candidate;
        updated.state = LearningCandidateState::Published;
        updated.published_learning_id = Some(learning_id.to_string());
        self.store.save_candidate(&updated)?;
        self.candidates
            .lock()
            .await
            .insert(updated.candidate_id.clone(), updated);
        Ok(())
    }
}

pub(crate) fn learning_is_prompt_visible(record: &LearningView) -> bool {
    record.status == LearningStatus::Active
        && record.publish_tier == LearningPublishTier::Active
        && record.sensitivity != LearningSensitivity::Sensitive
        && record.verification_status != LearningVerificationStatus::Failed
        && match record.policy_decision {
            Some(kheish_types::LearningPolicyDecision::Automatic) => {
                record.verification_status == LearningVerificationStatus::Verified
            }
            Some(kheish_types::LearningPolicyDecision::Escalated) => false,
            Some(kheish_types::LearningPolicyDecision::Manual) | None => true,
        }
}

pub(crate) fn learning_is_session_memory_search_visible(record: &LearningView) -> bool {
    record.sensitivity != LearningSensitivity::Sensitive
}

pub(crate) fn rank_learning_record(
    record: &LearningView,
    query: Option<&str>,
) -> LearningRetrievalMatch {
    let Some(query) = normalize_learning_query(query) else {
        return LearningRetrievalMatch::default();
    };
    let terms = learning_query_terms(&query);
    if terms.is_empty() {
        return LearningRetrievalMatch::default();
    }
    let kind = learning_kind_label(&record.kind);
    let scope = learning_scope_kind_label(&record.scope.kind);
    let mut matched = LearningRetrievalMatch::default();
    for (field_name, value, weight) in [
        ("content", record.content.as_str(), 4u64),
        ("kind", kind, 1u64),
        ("scope", scope, 1u64),
    ] {
        let lowered = value.to_lowercase();
        let mut score = 0u64;
        if lowered.contains(&query) {
            score = score.saturating_add(100 * weight);
        }
        let value_terms = learning_query_terms(&lowered);
        for term in &terms {
            if value_terms.iter().any(|value_term| value_term == term) {
                score = score.saturating_add(10 * weight);
            }
        }
        if score == 0 {
            continue;
        }
        matched.score = matched.score.saturating_add(score);
        if !matched
            .matched_fields
            .iter()
            .any(|existing| existing == field_name)
        {
            matched.matched_fields.push(field_name.to_string());
        }
    }
    matched
}

fn rank_learning_record_for_prompt(record: &LearningView, query: Option<&str>) -> u64 {
    rank_learning_field(record.content.as_str(), query, 4).score
}

fn rank_learning_field(value: &str, query: Option<&str>, weight: u64) -> LearningRetrievalMatch {
    let Some(query) = normalize_learning_query(query) else {
        return LearningRetrievalMatch::default();
    };
    let terms = learning_query_terms(&query);
    if terms.is_empty() {
        return LearningRetrievalMatch::default();
    }
    let lowered = value.to_lowercase();
    let mut matched = LearningRetrievalMatch::default();
    if lowered.contains(&query) {
        matched.score = matched.score.saturating_add(100 * weight);
    }
    let value_terms = learning_query_terms(&lowered);
    for term in &terms {
        if value_terms.iter().any(|value_term| value_term == term) {
            matched.score = matched.score.saturating_add(10 * weight);
        }
    }
    if matched.score > 0 {
        matched.matched_fields.push("content".to_string());
    }
    matched
}

fn learning_query_explicitly_requests_memory(query: &str) -> bool {
    let terms = learning_query_terms(query);
    terms.iter().any(|term| {
        matches!(
            term.as_str(),
            "memory"
                | "memories"
                | "memoire"
                | "mémoire"
                | "remember"
                | "remembered"
                | "recall"
                | "learning"
                | "learnings"
                | "appris"
        )
    }) || query.contains("learned context")
        || query.contains("durable learning")
}

fn build_indexes(
    candidates: &BTreeMap<String, LearningCandidateView>,
    records: &BTreeMap<String, LearningView>,
) -> LearningIndexes {
    let mut indexes = LearningIndexes::default();
    for candidate in candidates.values() {
        indexes
            .candidates_by_scope
            .entry(candidate.scope.scope_key())
            .or_default()
            .insert(candidate.candidate_id.clone());
        if candidate.kind == LearningKind::RunSummary
            && let Some(run_id) = candidate.source.run_id.clone()
        {
            indexes
                .run_summary_candidates_by_run
                .insert(run_id, candidate.candidate_id.clone());
        }
    }
    for record in records.values() {
        indexes
            .records_by_scope
            .entry(record.scope.scope_key())
            .or_default()
            .insert(record.learning_id.clone());
    }
    indexes
}

fn scope_priority(scope: &LearningScope) -> u8 {
    match scope.kind {
        kheish_types::LearningScopeKind::Session => 0,
        kheish_types::LearningScopeKind::Persona => 1,
        kheish_types::LearningScopeKind::Project => 2,
        kheish_types::LearningScopeKind::Workspace => 3,
    }
}

fn candidate_matches_query(candidate: &LearningCandidateView, query: &str) -> bool {
    candidate.candidate_id.to_ascii_lowercase().contains(query)
        || candidate
            .scope
            .scope_key()
            .to_ascii_lowercase()
            .contains(query)
        || format!("{:?}", candidate.kind)
            .to_ascii_lowercase()
            .contains(query)
        || candidate.content.to_ascii_lowercase().contains(query)
        || candidate
            .source
            .run_id
            .as_deref()
            .is_some_and(|run_id| run_id.to_ascii_lowercase().contains(query))
}

fn daemon_candidate_matches(
    existing: &LearningCandidateView,
    expected: &LearningCandidateView,
) -> bool {
    existing.origin == LearningCandidateOrigin::Daemon
        && existing.scope == expected.scope
        && existing.kind == expected.kind
        && semantic_learning_content_key(&existing.content)
            == semantic_learning_content_key(&expected.content)
        && existing.source.run_id == expected.source.run_id
        && existing.source.session_id == expected.source.session_id
}

fn record_matches_query(record: &LearningView, query: &str) -> bool {
    record.learning_id.to_ascii_lowercase().contains(query)
        || record
            .scope
            .scope_key()
            .to_ascii_lowercase()
            .contains(query)
        || format!("{:?}", record.kind)
            .to_ascii_lowercase()
            .contains(query)
        || record.content.to_ascii_lowercase().contains(query)
        || record
            .source
            .run_id
            .as_deref()
            .is_some_and(|run_id| run_id.to_ascii_lowercase().contains(query))
}

fn normalize_learning_query(query: Option<&str>) -> Option<String> {
    query
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_lowercase)
}

fn learning_query_terms(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .filter(|term| !is_learning_query_stop_word(term))
        .map(str::to_lowercase)
        .collect()
}

fn is_learning_query_stop_word(term: &str) -> bool {
    matches!(
        term,
        "a" | "about"
            | "am"
            | "an"
            | "and"
            | "are"
            | "be"
            | "been"
            | "being"
            | "de"
            | "des"
            | "du"
            | "et"
            | "exactly"
            | "for"
            | "how"
            | "i"
            | "is"
            | "la"
            | "le"
            | "les"
            | "maintenant"
            | "me"
            | "my"
            | "now"
            | "of"
            | "or"
            | "ou"
            | "please"
            | "pour"
            | "quel"
            | "quelle"
            | "reply"
            | "sont"
            | "the"
            | "to"
            | "un"
            | "une"
            | "what"
            | "when"
            | "where"
            | "which"
            | "who"
            | "why"
            | "with"
            | "your"
            | "est"
            | "avec"
    )
}

fn learning_kind_label(kind: &LearningKind) -> &'static str {
    match kind {
        LearningKind::Fact => "fact",
        LearningKind::Preference => "preference",
        LearningKind::Decision => "decision",
        LearningKind::Procedure => "procedure",
        LearningKind::RunSummary => "run summary",
    }
}

fn learning_scope_kind_label(kind: &kheish_types::LearningScopeKind) -> &'static str {
    match kind {
        kheish_types::LearningScopeKind::Session => "session",
        kheish_types::LearningScopeKind::Persona => "persona",
        kheish_types::LearningScopeKind::Project => "project",
        kheish_types::LearningScopeKind::Workspace => "workspace",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicU64;

    use anyhow::Result;
    use tempfile::tempdir;

    use super::{
        LearningCandidateListFilter, LearningListFilter, LearningService,
        learning_is_prompt_visible, learning_is_session_memory_search_visible,
        rank_learning_record, rank_learning_record_for_prompt,
    };
    use crate::learning::{
        FileLearningStore, LearningAutomationMode, LearningAutomationReview,
        LearningCandidateOrigin, LearningCandidateState, LearningCandidateView,
        LearningPublicationAction, LearningView,
    };
    use kheish_types::{
        LearningKind, LearningPolicyDecision, LearningPublishTier, LearningScope,
        LearningScopeKind, LearningSensitivity, LearningSourceRef, LearningStatus,
        LearningVerificationStatus,
    };

    fn sample_candidate(candidate_id: &str) -> LearningCandidateView {
        LearningCandidateView {
            candidate_id: candidate_id.to_string(),
            origin: LearningCandidateOrigin::Api,
            scope: LearningScope {
                kind: LearningScopeKind::Session,
                id: "demo".to_string(),
            },
            kind: LearningKind::Fact,
            sensitivity: LearningSensitivity::Scoped,
            content: "The repo prefers small JSON fixtures.".to_string(),
            confidence: 70,
            source: LearningSourceRef::default(),
            evidence_refs: Vec::new(),
            created_at_ms: 1,
            expires_at_ms: None,
            state: LearningCandidateState::Pending,
            automation_review: None,
            published_learning_id: None,
        }
    }

    fn sample_learning(learning_id: &str) -> LearningView {
        LearningView {
            learning_id: learning_id.to_string(),
            scope: LearningScope {
                kind: LearningScopeKind::Session,
                id: "demo".to_string(),
            },
            kind: LearningKind::Fact,
            sensitivity: LearningSensitivity::Scoped,
            content: "The repo prefers small JSON fixtures.".to_string(),
            confidence: 90,
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
            verification_status: LearningVerificationStatus::Unverified,
            supersedes: None,
            superseded_by: None,
            revoked_at_ms: None,
            revoked_reason: None,
        }
    }

    fn service() -> LearningService {
        let temp = tempdir().expect("tempdir");
        let root = temp.keep();
        LearningService::new(
            FileLearningStore::new(root),
            BTreeMap::new(),
            BTreeMap::new(),
            AtomicU64::new(1),
            AtomicU64::new(1),
        )
    }

    fn service_with_state(
        candidates: BTreeMap<String, LearningCandidateView>,
        records: BTreeMap<String, LearningView>,
    ) -> LearningService {
        let temp = tempdir().expect("tempdir");
        let root = temp.keep();
        LearningService::new(
            FileLearningStore::new(root),
            candidates,
            records,
            AtomicU64::new(1),
            AtomicU64::new(1),
        )
    }

    #[tokio::test]
    async fn learning_service_publishes_candidates_and_builds_prompt_bundle() -> Result<()> {
        let service = service();
        let candidate = service
            .create_candidate(sample_candidate("learning-candidate-1"))
            .await?;
        let published = service
            .publish_candidate(
                &candidate.candidate_id,
                LearningView {
                    learning_id: "learning-1".to_string(),
                    source_candidate_id: None,
                    published_at_ms: 2,
                    ..sample_learning("learning-1")
                },
            )
            .await?;
        let bundle = service
            .learned_context_bundle(
                &[LearningScope {
                    kind: LearningScopeKind::Session,
                    id: "demo".to_string(),
                }],
                10,
            )
            .await
            .expect("bundle");
        assert_eq!(
            published.source_candidate_id.as_deref(),
            Some("learning-candidate-1")
        );
        assert_eq!(bundle.entries.len(), 1);
        assert_eq!(bundle.entries[0].learning_id, "learning-1");
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_reuses_equivalent_prompt_visible_learning_on_publish() -> Result<()> {
        let service = service();
        let first_candidate = service
            .create_candidate(sample_candidate("learning-candidate-1"))
            .await?;
        let first = service
            .publish_candidate(
                &first_candidate.candidate_id,
                LearningView {
                    learning_id: "learning-1".to_string(),
                    source_candidate_id: None,
                    content: "Project codename is Atlas.".to_string(),
                    published_at_ms: 2,
                    ..sample_learning("learning-1")
                },
            )
            .await?;

        let mut duplicate_candidate = sample_candidate("learning-candidate-2");
        duplicate_candidate.content = "project codename: atlas".to_string();
        let duplicate_candidate = service.create_candidate(duplicate_candidate).await?;
        let duplicate = service
            .publish_candidate(
                &duplicate_candidate.candidate_id,
                LearningView {
                    learning_id: "learning-2".to_string(),
                    source_candidate_id: None,
                    content: "project codename: atlas".to_string(),
                    published_at_ms: 3,
                    ..sample_learning("learning-2")
                },
            )
            .await?;

        assert_eq!(duplicate.learning_id, first.learning_id);
        let updated_candidate = service
            .get_candidate(&duplicate_candidate.candidate_id)
            .await
            .expect("candidate");
        assert_eq!(updated_candidate.state, LearningCandidateState::Published);
        assert_eq!(
            updated_candidate.published_learning_id.as_deref(),
            Some(first.learning_id.as_str())
        );
        assert_eq!(service.list(&LearningListFilter::default()).await.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_detects_active_same_subject_conflicts() -> Result<()> {
        let existing = LearningView {
            learning_id: "learning-current".to_string(),
            content: "Project codename is Atlas.".to_string(),
            ..sample_learning("learning-current")
        };
        let service = service_with_state(
            BTreeMap::new(),
            BTreeMap::from([(existing.learning_id.clone(), existing.clone())]),
        );
        let mut conflicting = sample_candidate("learning-candidate-conflict");
        conflicting.content = "Project codename is Borealis.".to_string();
        let conflict = service
            .active_conflicting_learning_for_candidate(&conflicting)
            .await
            .expect("conflict");
        assert_eq!(conflict.learning_id, existing.learning_id);

        conflicting.content = "The project codename: Atlas".to_string();
        assert!(
            service
                .active_conflicting_learning_for_candidate(&conflicting)
                .await
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_deduplicates_daemon_candidates_by_semantic_key() -> Result<()> {
        let service = service();
        let mut first = sample_candidate("learning-candidate-1");
        first.origin = LearningCandidateOrigin::Daemon;
        first.content = "Project codename is Atlas.".to_string();
        first.source.run_id = Some("run-1".to_string());
        first.source.session_id = Some("demo".to_string());
        assert!(service.ensure_daemon_candidate(first).await?.is_some());

        let mut equivalent = sample_candidate("learning-candidate-2");
        equivalent.origin = LearningCandidateOrigin::Daemon;
        equivalent.content = "The project codename: atlas".to_string();
        equivalent.source.run_id = Some("run-1".to_string());
        equivalent.source.session_id = Some("demo".to_string());
        assert!(service.ensure_daemon_candidate(equivalent).await?.is_none());
        assert_eq!(
            service
                .list_candidates(&LearningCandidateListFilter::default())
                .await
                .len(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_lists_and_revokes_records() -> Result<()> {
        let service = service();
        service
            .create_candidate(sample_candidate("learning-candidate-1"))
            .await?;
        service
            .publish_candidate(
                "learning-candidate-1",
                LearningView {
                    learning_id: "learning-1".to_string(),
                    source_candidate_id: None,
                    published_at_ms: 2,
                    ..sample_learning("learning-1")
                },
            )
            .await?;
        let listed = service
            .list(&LearningListFilter {
                query: Some("small json".to_string()),
                ..LearningListFilter::default()
            })
            .await;
        assert_eq!(listed.len(), 1);
        let revoked = service
            .revoke("learning-1", 3, Some("stale".to_string()))
            .await?;
        assert_eq!(revoked.status, LearningStatus::Revoked);
        assert!(
            service
                .learned_context_bundle(
                    &[LearningScope {
                        kind: LearningScopeKind::Session,
                        id: "demo".to_string(),
                    }],
                    10,
                )
                .await
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_deduplicates_run_summary_candidates() -> Result<()> {
        let service = service();
        assert!(
            service
                .ensure_run_summary_candidate("demo", "agent-1", "run-1", "summary", 1, None)
                .await?
                .is_some()
        );
        assert!(
            service
                .ensure_run_summary_candidate("demo", "agent-1", "run-1", "summary", 1, None)
                .await?
                .is_none()
        );
        let listed = service
            .list_candidates(&LearningCandidateListFilter::default())
            .await;
        assert_eq!(listed.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_repairs_published_candidate_from_existing_learning() -> Result<()> {
        let candidate = sample_candidate("learning-candidate-1");
        let existing_learning = LearningView {
            learning_id: "learning-1".to_string(),
            source_candidate_id: Some(candidate.candidate_id.clone()),
            published_at_ms: 2,
            ..sample_learning("learning-1")
        };
        let service = service_with_state(
            BTreeMap::from([(candidate.candidate_id.clone(), candidate.clone())]),
            BTreeMap::from([(
                existing_learning.learning_id.clone(),
                existing_learning.clone(),
            )]),
        );

        let published = service
            .publish_candidate(
                &candidate.candidate_id,
                LearningView {
                    learning_id: "learning-2".to_string(),
                    source_candidate_id: None,
                    published_at_ms: 3,
                    ..sample_learning("learning-2")
                },
            )
            .await?;
        let repaired_candidate = service
            .get_candidate(&candidate.candidate_id)
            .await
            .expect("candidate should exist");

        assert_eq!(published.learning_id, existing_learning.learning_id);
        assert_eq!(repaired_candidate.state, LearningCandidateState::Published);
        assert_eq!(
            repaired_candidate.published_learning_id.as_deref(),
            Some(existing_learning.learning_id.as_str())
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_reject_candidate_conflicts_after_publish() -> Result<()> {
        let service = service();
        let candidate = service
            .create_candidate(sample_candidate("learning-candidate-1"))
            .await?;
        service
            .publish_candidate(
                &candidate.candidate_id,
                LearningView {
                    learning_id: "learning-1".to_string(),
                    source_candidate_id: None,
                    published_at_ms: 2,
                    ..sample_learning("learning-1")
                },
            )
            .await?;

        assert!(
            service
                .reject_candidate(&candidate.candidate_id)
                .await
                .expect_err("published candidate should not become rejected")
                .to_string()
                .contains("already published")
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_records_shadow_review_without_state_transition() -> Result<()> {
        let service = service();
        let candidate = service
            .create_candidate(sample_candidate("learning-candidate-1"))
            .await?;

        let reviewed = service
            .record_automation_review(
                &candidate.candidate_id,
                None,
                LearningAutomationReview {
                    mode: LearningAutomationMode::Shadow,
                    action: LearningPublicationAction::PublishActive,
                    reviewed_at_ms: 42,
                    matched_rule_name: Some("session-fact".to_string()),
                    judge: None,
                    reason: "shadow evaluation matched one active publication rule".to_string(),
                },
            )
            .await?;

        assert_eq!(reviewed.state, LearningCandidateState::Pending);
        assert_eq!(
            reviewed
                .automation_review
                .as_ref()
                .map(|review| review.action.clone()),
            Some(LearningPublicationAction::PublishActive)
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_can_escalate_candidate_after_automation_review() -> Result<()> {
        let service = service();
        let candidate = service
            .create_candidate(sample_candidate("learning-candidate-1"))
            .await?;

        let reviewed = service
            .record_automation_review(
                &candidate.candidate_id,
                Some(LearningCandidateState::Escalated),
                LearningAutomationReview {
                    mode: LearningAutomationMode::Enabled,
                    action: LearningPublicationAction::ManualReview,
                    reviewed_at_ms: 42,
                    matched_rule_name: None,
                    judge: None,
                    reason: "candidate requires manual review".to_string(),
                },
            )
            .await?;

        assert_eq!(reviewed.state, LearningCandidateState::Escalated);
        assert_eq!(
            reviewed
                .automation_review
                .as_ref()
                .and_then(|review| review.matched_rule_name.as_deref()),
            None
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_filters_records_by_policy_metadata_and_rule_name() -> Result<()> {
        let candidate = LearningCandidateView {
            state: LearningCandidateState::Published,
            automation_review: Some(LearningAutomationReview {
                mode: LearningAutomationMode::Enabled,
                action: LearningPublicationAction::PublishProvisional,
                reviewed_at_ms: 42,
                matched_rule_name: Some("session-fact-autopublish".to_string()),
                judge: None,
                reason: "matched rule".to_string(),
            }),
            published_learning_id: Some("learning-1".to_string()),
            ..sample_candidate("learning-candidate-1")
        };
        let automatic_learning = LearningView {
            learning_id: "learning-1".to_string(),
            source_candidate_id: Some(candidate.candidate_id.clone()),
            policy_decision: Some(LearningPolicyDecision::Automatic),
            policy_actor: Some("daemon".to_string()),
            publish_tier: LearningPublishTier::Provisional,
            status: LearningStatus::Provisional,
            ..sample_learning("learning-1")
        };
        let manual_learning = LearningView {
            learning_id: "learning-2".to_string(),
            source_candidate_id: None,
            policy_decision: Some(LearningPolicyDecision::Manual),
            policy_actor: Some("operator".to_string()),
            ..sample_learning("learning-2")
        };
        let service = service_with_state(
            BTreeMap::from([(candidate.candidate_id.clone(), candidate)]),
            BTreeMap::from([
                (
                    automatic_learning.learning_id.clone(),
                    automatic_learning.clone(),
                ),
                (manual_learning.learning_id.clone(), manual_learning),
            ]),
        );

        let filtered = service
            .list(&LearningListFilter {
                policy_decision: Some(LearningPolicyDecision::Automatic),
                policy_actor: Some("daemon".to_string()),
                matched_rule_name: Some("session-fact-autopublish".to_string()),
                ..LearningListFilter::default()
            })
            .await;

        assert_eq!(filtered, vec![automatic_learning]);
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_supersede_reuses_existing_active_replacement() -> Result<()> {
        let source = sample_learning("learning-1");
        let replacement = LearningView {
            learning_id: "learning-2".to_string(),
            source_candidate_id: None,
            published_at_ms: 3,
            supersedes: Some("learning-1".to_string()),
            ..sample_learning("learning-2")
        };
        let service = service_with_state(
            BTreeMap::new(),
            BTreeMap::from([
                (source.learning_id.clone(), source.clone()),
                (replacement.learning_id.clone(), replacement.clone()),
            ]),
        );

        let reused = service
            .supersede(
                "learning-1",
                LearningView {
                    learning_id: "learning-3".to_string(),
                    source_candidate_id: None,
                    published_at_ms: 4,
                    supersedes: Some("learning-1".to_string()),
                    ..sample_learning("learning-3")
                },
            )
            .await?;
        let updated_source = service
            .get("learning-1")
            .await
            .expect("source should exist");

        assert_eq!(reused.learning_id, "learning-2");
        assert_eq!(updated_source.status, LearningStatus::Superseded);
        assert_eq!(updated_source.superseded_by.as_deref(), Some("learning-2"));
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_bundle_skips_expired_and_non_prompt_entries_and_orders_scopes()
    -> Result<()> {
        let session_learning = LearningView {
            learning_id: "learning-session".to_string(),
            source_candidate_id: None,
            published_at_ms: 5,
            ..sample_learning("learning-session")
        };
        let project_learning = LearningView {
            learning_id: "learning-project".to_string(),
            scope: LearningScope {
                kind: LearningScopeKind::Project,
                id: "proj".to_string(),
            },
            source_candidate_id: None,
            published_at_ms: 4,
            ..sample_learning("learning-project")
        };
        let workspace_learning = LearningView {
            learning_id: "learning-workspace".to_string(),
            scope: LearningScope {
                kind: LearningScopeKind::Workspace,
                id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
            },
            source_candidate_id: None,
            published_at_ms: 3,
            ..sample_learning("learning-workspace")
        };
        let expired_learning = LearningView {
            learning_id: "learning-expired".to_string(),
            expires_at_ms: Some(10),
            published_at_ms: 2,
            source_candidate_id: None,
            ..sample_learning("learning-expired")
        };
        let procedure_learning = LearningView {
            learning_id: "learning-procedure".to_string(),
            kind: LearningKind::Procedure,
            source_candidate_id: None,
            published_at_ms: 1,
            ..sample_learning("learning-procedure")
        };
        let service = service_with_state(
            BTreeMap::new(),
            BTreeMap::from([
                (session_learning.learning_id.clone(), session_learning),
                (project_learning.learning_id.clone(), project_learning),
                (workspace_learning.learning_id.clone(), workspace_learning),
                (expired_learning.learning_id.clone(), expired_learning),
                (procedure_learning.learning_id.clone(), procedure_learning),
            ]),
        );

        let bundle = service
            .learned_context_bundle(
                &[
                    LearningScope {
                        kind: LearningScopeKind::Session,
                        id: "demo".to_string(),
                    },
                    LearningScope {
                        kind: LearningScopeKind::Project,
                        id: "proj".to_string(),
                    },
                    LearningScope {
                        kind: LearningScopeKind::Workspace,
                        id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
                    },
                ],
                10,
            )
            .await
            .expect("bundle");

        assert_eq!(
            bundle
                .entries
                .iter()
                .map(|entry| entry.learning_id.as_str())
                .collect::<Vec<_>>(),
            vec!["learning-session", "learning-project", "learning-workspace"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_bundle_query_ranks_relevant_records_before_scope_recency_order()
    -> Result<()> {
        let session_irrelevant = LearningView {
            learning_id: "learning-session-irrelevant".to_string(),
            content: "Default snack preference is banana.".to_string(),
            source_candidate_id: None,
            published_at_ms: 30,
            ..sample_learning("learning-session-irrelevant")
        };
        let workspace_relevant = LearningView {
            learning_id: "learning-workspace-relevant".to_string(),
            scope: LearningScope {
                kind: LearningScopeKind::Workspace,
                id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
            },
            content: "Atlas project color is vermilion.".to_string(),
            source_candidate_id: None,
            published_at_ms: 10,
            ..sample_learning("learning-workspace-relevant")
        };
        let service = service_with_state(
            BTreeMap::new(),
            BTreeMap::from([
                (
                    session_irrelevant.learning_id.clone(),
                    session_irrelevant.clone(),
                ),
                (workspace_relevant.learning_id.clone(), workspace_relevant),
            ]),
        );

        let unscored = service
            .learned_context_bundle(
                &[
                    LearningScope {
                        kind: LearningScopeKind::Session,
                        id: "demo".to_string(),
                    },
                    LearningScope {
                        kind: LearningScopeKind::Workspace,
                        id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
                    },
                ],
                100,
            )
            .await
            .expect("bundle");
        assert_eq!(
            unscored
                .entries
                .iter()
                .map(|entry| entry.learning_id.as_str())
                .collect::<Vec<_>>(),
            vec!["learning-session-irrelevant", "learning-workspace-relevant"]
        );

        let scored = service
            .learned_context_bundle_for_query(
                &[
                    LearningScope {
                        kind: LearningScopeKind::Session,
                        id: "demo".to_string(),
                    },
                    LearningScope {
                        kind: LearningScopeKind::Workspace,
                        id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
                    },
                ],
                100,
                Some("What color is Atlas?"),
            )
            .await
            .expect("bundle");
        assert_eq!(
            scored
                .entries
                .iter()
                .map(|entry| entry.learning_id.as_str())
                .collect::<Vec<_>>(),
            vec!["learning-workspace-relevant"]
        );
        assert_eq!(
            rank_learning_record(&session_irrelevant, Some("What color is Atlas?")).score,
            0
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_bundle_query_matches_unicode_content() -> Result<()> {
        let session_irrelevant = LearningView {
            learning_id: "learning-session-irrelevant".to_string(),
            content: "Default snack preference is banana.".to_string(),
            source_candidate_id: None,
            published_at_ms: 30,
            ..sample_learning("learning-session-irrelevant")
        };
        let workspace_unicode = LearningView {
            learning_id: "learning-workspace-unicode".to_string(),
            scope: LearningScope {
                kind: LearningScopeKind::Workspace,
                id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
            },
            content: "東京 office color is vermilion.".to_string(),
            source_candidate_id: None,
            published_at_ms: 10,
            ..sample_learning("learning-workspace-unicode")
        };
        let service = service_with_state(
            BTreeMap::new(),
            BTreeMap::from([
                (
                    session_irrelevant.learning_id.clone(),
                    session_irrelevant.clone(),
                ),
                (
                    workspace_unicode.learning_id.clone(),
                    workspace_unicode.clone(),
                ),
            ]),
        );

        assert_eq!(
            rank_learning_record(&workspace_unicode, Some("東京")).matched_fields,
            vec!["content".to_string()]
        );
        assert_eq!(
            rank_learning_record(&session_irrelevant, Some("東京")).score,
            0
        );

        let scored = service
            .learned_context_bundle_for_query(
                &[
                    LearningScope {
                        kind: LearningScopeKind::Session,
                        id: "demo".to_string(),
                    },
                    LearningScope {
                        kind: LearningScopeKind::Workspace,
                        id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
                    },
                ],
                100,
                Some("東京"),
            )
            .await
            .expect("bundle");
        assert_eq!(
            scored
                .entries
                .iter()
                .map(|entry| entry.learning_id.as_str())
                .collect::<Vec<_>>(),
            vec!["learning-workspace-unicode"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_bundle_query_keeps_recency_fallback_for_explicit_memory_requests()
    -> Result<()> {
        let session_learning = LearningView {
            learning_id: "learning-session".to_string(),
            content: "Default snack preference is banana.".to_string(),
            source_candidate_id: None,
            published_at_ms: 30,
            ..sample_learning("learning-session")
        };
        let workspace_learning = LearningView {
            learning_id: "learning-workspace".to_string(),
            scope: LearningScope {
                kind: LearningScopeKind::Workspace,
                id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
            },
            content: "Default editor is vim.".to_string(),
            source_candidate_id: None,
            published_at_ms: 10,
            ..sample_learning("learning-workspace")
        };
        let service = service_with_state(
            BTreeMap::new(),
            BTreeMap::from([
                (session_learning.learning_id.clone(), session_learning),
                (workspace_learning.learning_id.clone(), workspace_learning),
            ]),
        );

        let scored = service
            .learned_context_bundle_for_query(
                &[
                    LearningScope {
                        kind: LearningScopeKind::Session,
                        id: "demo".to_string(),
                    },
                    LearningScope {
                        kind: LearningScopeKind::Workspace,
                        id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
                    },
                ],
                100,
                Some("Use durable learning if it exists."),
            )
            .await
            .expect("bundle");
        assert_eq!(
            scored
                .entries
                .iter()
                .map(|entry| entry.learning_id.as_str())
                .collect::<Vec<_>>(),
            vec!["learning-session", "learning-workspace"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_bundle_query_omits_no_match_non_memory_inputs() -> Result<()> {
        let session_learning = LearningView {
            learning_id: "learning-session".to_string(),
            content: "Default snack preference is banana.".to_string(),
            source_candidate_id: None,
            published_at_ms: 30,
            ..sample_learning("learning-session")
        };
        let workspace_learning = LearningView {
            learning_id: "learning-workspace".to_string(),
            scope: LearningScope {
                kind: LearningScopeKind::Workspace,
                id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
            },
            content: "Default editor is vim.".to_string(),
            source_candidate_id: None,
            published_at_ms: 10,
            ..sample_learning("learning-workspace")
        };
        let service = service_with_state(
            BTreeMap::new(),
            BTreeMap::from([
                (
                    session_learning.learning_id.clone(),
                    session_learning.clone(),
                ),
                (
                    workspace_learning.learning_id.clone(),
                    workspace_learning.clone(),
                ),
            ]),
        );

        assert!(
            service
                .learned_context_bundle_for_query(
                    &[
                        LearningScope {
                            kind: LearningScopeKind::Session,
                            id: "demo".to_string(),
                        },
                        LearningScope {
                            kind: LearningScopeKind::Workspace,
                            id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
                        },
                    ],
                    100,
                    Some("What is the deployment status?"),
                )
                .await
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_bundle_query_does_not_score_scope_or_kind_for_prompt() -> Result<()> {
        let workspace_fact = LearningView {
            learning_id: "learning-workspace".to_string(),
            scope: LearningScope {
                kind: LearningScopeKind::Workspace,
                id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
            },
            kind: LearningKind::Fact,
            content: "Default editor is vim.".to_string(),
            source_candidate_id: None,
            published_at_ms: 10,
            ..sample_learning("learning-workspace")
        };
        let service = service_with_state(
            BTreeMap::new(),
            BTreeMap::from([(workspace_fact.learning_id.clone(), workspace_fact.clone())]),
        );

        assert!(rank_learning_record(&workspace_fact, Some("workspace fact")).score > 0);
        assert_eq!(
            rank_learning_record_for_prompt(&workspace_fact, Some("workspace fact")),
            0
        );
        assert!(
            service
                .learned_context_bundle_for_query(
                    &[LearningScope {
                        kind: LearningScopeKind::Workspace,
                        id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
                    }],
                    100,
                    Some("workspace fact"),
                )
                .await
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_bundle_skips_provisional_learnings() -> Result<()> {
        let provisional = LearningView {
            learning_id: "learning-provisional".to_string(),
            status: LearningStatus::Provisional,
            publish_tier: LearningPublishTier::Provisional,
            ..sample_learning("learning-provisional")
        };
        let service = service_with_state(
            BTreeMap::new(),
            BTreeMap::from([(provisional.learning_id.clone(), provisional)]),
        );

        assert!(
            service
                .learned_context_bundle(
                    &[LearningScope {
                        kind: LearningScopeKind::Session,
                        id: "demo".to_string(),
                    }],
                    10,
                )
                .await
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_rejects_provisional_superseding_publication() -> Result<()> {
        let service = service();
        service
            .create_candidate(sample_candidate("learning-candidate-1"))
            .await?;

        let error = service
            .publish_candidate(
                "learning-candidate-1",
                LearningView {
                    learning_id: "learning-2".to_string(),
                    source_candidate_id: None,
                    published_at_ms: 2,
                    status: LearningStatus::Provisional,
                    publish_tier: LearningPublishTier::Provisional,
                    supersedes: Some("learning-1".to_string()),
                    ..sample_learning("learning-2")
                },
            )
            .await
            .expect_err("provisional superseding publication must be rejected");

        assert!(
            error
                .to_string()
                .contains("superseding learnings must use the active publish tier")
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_bundle_skips_failed_verification_records() -> Result<()> {
        let failed = LearningView {
            learning_id: "learning-failed".to_string(),
            verification_status: LearningVerificationStatus::Failed,
            ..sample_learning("learning-failed")
        };
        let service = service_with_state(
            BTreeMap::new(),
            BTreeMap::from([(failed.learning_id.clone(), failed)]),
        );

        assert!(
            service
                .learned_context_bundle(
                    &[LearningScope {
                        kind: LearningScopeKind::Session,
                        id: "demo".to_string(),
                    }],
                    10,
                )
                .await
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_bundle_skips_sensitive_records() -> Result<()> {
        let sensitive = LearningView {
            learning_id: "learning-sensitive".to_string(),
            sensitivity: LearningSensitivity::Sensitive,
            ..sample_learning("learning-sensitive")
        };
        let service = service_with_state(
            BTreeMap::new(),
            BTreeMap::from([(sensitive.learning_id.clone(), sensitive.clone())]),
        );

        assert!(
            service
                .learned_context_bundle(
                    &[LearningScope {
                        kind: LearningScopeKind::Session,
                        id: "demo".to_string(),
                    }],
                    10,
                )
                .await
                .is_none()
        );
        assert!(!learning_is_prompt_visible(&sensitive));
        assert!(!learning_is_session_memory_search_visible(&sensitive));
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_bundle_skips_unverified_automatic_active_records() -> Result<()> {
        let automatic = LearningView {
            learning_id: "learning-automatic".to_string(),
            policy_decision: Some(LearningPolicyDecision::Automatic),
            policy_actor: Some("daemon".to_string()),
            verification_status: LearningVerificationStatus::Unverified,
            ..sample_learning("learning-automatic")
        };
        let service = service_with_state(
            BTreeMap::new(),
            BTreeMap::from([(automatic.learning_id.clone(), automatic)]),
        );

        assert!(
            service
                .learned_context_bundle(
                    &[LearningScope {
                        kind: LearningScopeKind::Session,
                        id: "demo".to_string(),
                    }],
                    10,
                )
                .await
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_bundle_keeps_unverified_manual_active_records() -> Result<()> {
        let manual = LearningView {
            learning_id: "learning-manual".to_string(),
            policy_decision: Some(LearningPolicyDecision::Manual),
            policy_actor: Some("operator".to_string()),
            verification_status: LearningVerificationStatus::Unverified,
            ..sample_learning("learning-manual")
        };
        let service = service_with_state(
            BTreeMap::new(),
            BTreeMap::from([(manual.learning_id.clone(), manual)]),
        );

        let bundle = service
            .learned_context_bundle(
                &[LearningScope {
                    kind: LearningScopeKind::Session,
                    id: "demo".to_string(),
                }],
                10,
            )
            .await
            .expect("bundle");

        assert_eq!(
            bundle
                .entries
                .iter()
                .map(|entry| entry.learning_id.as_str())
                .collect::<Vec<_>>(),
            vec!["learning-manual"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn learning_service_bundle_hides_active_records_shadowed_by_replacements() -> Result<()> {
        let current = LearningView {
            learning_id: "learning-current".to_string(),
            content: "CURRENT".to_string(),
            published_at_ms: 10,
            ..sample_learning("learning-current")
        };
        let replacement = LearningView {
            learning_id: "learning-replacement".to_string(),
            content: "REPLACEMENT".to_string(),
            published_at_ms: 20,
            supersedes: Some("learning-current".to_string()),
            ..sample_learning("learning-replacement")
        };
        let service = service_with_state(
            BTreeMap::new(),
            BTreeMap::from([
                (current.learning_id.clone(), current),
                (replacement.learning_id.clone(), replacement),
            ]),
        );

        let bundle = service
            .learned_context_bundle(
                &[LearningScope {
                    kind: LearningScopeKind::Session,
                    id: "demo".to_string(),
                }],
                100,
            )
            .await
            .expect("bundle");

        assert_eq!(
            bundle
                .entries
                .iter()
                .map(|entry| entry.learning_id.as_str())
                .collect::<Vec<_>>(),
            vec!["learning-replacement"]
        );
        Ok(())
    }
}
