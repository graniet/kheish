//! Learning candidate, publication, and retrieval workflows implemented on [`DaemonState`].

use super::*;

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    const LEARNING_AUTOMATION_RETRY_BACKOFF_MS: u64 = 500;

    pub(crate) fn next_learning_candidate_id(&self) -> String {
        self.learning_service.next_candidate_id()
    }

    pub(crate) fn next_learning_id(&self) -> String {
        self.learning_service.next_learning_id()
    }

    pub(crate) async fn repair_learning_skill_catalog(&self) -> Result<()> {
        self.learning_skill_service.repair_catalog().await
    }

    pub(crate) fn spawn_learning_publication_worker(
        self: &Arc<Self>,
    ) -> tokio::task::JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            state.learning_publication_worker_loop().await;
        })
    }

    pub(crate) async fn restore_learning_publication_worker_on_boot(&self) -> Result<()> {
        for candidate_id in self.learning_service.pending_candidate_ids().await {
            self.learning_policy_service
                .enqueue_candidate(candidate_id)
                .await;
        }
        Ok(())
    }

    pub(crate) async fn learning_scopes_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<kheish_types::LearningScope>> {
        let mut scopes = vec![kheish_types::LearningScope {
            kind: kheish_types::LearningScopeKind::Session,
            id: session_id.to_string(),
        }];
        if let Some(persona) = self.load_session_persona_binding(session_id).await? {
            scopes.push(kheish_types::LearningScope {
                kind: kheish_types::LearningScopeKind::Persona,
                id: persona.persona_id,
            });
        }
        for project_id in self
            .project_service
            .project_ids_for_session(session_id)
            .await
        {
            scopes.push(kheish_types::LearningScope {
                kind: kheish_types::LearningScopeKind::Project,
                id: project_id,
            });
        }
        scopes.push(kheish_types::LearningScope {
            kind: kheish_types::LearningScopeKind::Workspace,
            id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
        });
        Ok(scopes)
    }

    pub(crate) async fn learned_context_bundle(
        &self,
        session_id: &str,
        query: Option<&str>,
    ) -> Result<Option<kheish_types::LearnedContextBundle>> {
        let scopes = self.learning_scopes_for_session(session_id).await?;
        Ok(self
            .learning_service
            .learned_context_bundle_for_query(&scopes, now_ms(), query)
            .await)
    }

    pub(crate) async fn list_learning_candidates(
        &self,
        filter: &LearningCandidateListFilter,
    ) -> Result<Vec<LearningCandidateView>> {
        Ok(self.learning_service.list_candidates(filter).await)
    }

    pub(crate) async fn get_learning_candidate(
        &self,
        candidate_id: &str,
    ) -> Result<LearningCandidateView> {
        self.learning_service
            .get_candidate(candidate_id)
            .await
            .ok_or_else(|| anyhow!("unknown learning candidate {candidate_id}"))
    }

    pub(crate) async fn create_learning_candidate(
        &self,
        candidate: LearningCandidateView,
    ) -> Result<LearningCandidateView> {
        let candidate = self.learning_service.create_candidate(candidate).await?;
        self.learning_policy_service
            .enqueue_candidate(candidate.candidate_id.clone())
            .await;
        Ok(candidate)
    }

    pub(crate) async fn publish_learning_candidate(
        &self,
        candidate_id: &str,
        learning: LearningView,
    ) -> Result<LearningView> {
        let candidate = self.get_learning_candidate(candidate_id).await?;
        let learning = self.learning_policy_service.prepare_published_learning(
            &candidate,
            learning,
            LearningMutationMode::Manual,
        )?;
        self.learning_service
            .publish_candidate(candidate_id, learning)
            .await
    }

    pub(crate) async fn reject_learning_candidate(
        &self,
        candidate_id: &str,
    ) -> Result<LearningCandidateView> {
        self.learning_service.reject_candidate(candidate_id).await
    }

    pub(crate) async fn list_learnings(
        &self,
        filter: &LearningListFilter,
    ) -> Result<Vec<LearningView>> {
        Ok(self.learning_service.list(filter).await)
    }

    pub(crate) async fn get_learning(&self, learning_id: &str) -> Result<LearningView> {
        self.learning_service
            .get(learning_id)
            .await
            .ok_or_else(|| anyhow!("unknown learning {learning_id}"))
    }

    pub(crate) async fn revoke_learning(
        &self,
        learning_id: &str,
        reason: Option<String>,
    ) -> Result<LearningView> {
        validate_learning_revocation_reason(reason.as_deref())?;
        let current = self.get_learning(learning_id).await?;
        let prior_skill = if current.kind == kheish_types::LearningKind::Procedure {
            self.learning_skill_service
                .live_by_learning(learning_id)
                .await
        } else {
            None
        };
        if let Some(skill) = &prior_skill {
            self.learning_skill_service
                .revoke(
                    &skill.skill_name,
                    now_ms(),
                    Some(format!("source learning {learning_id} was revoked")),
                )
                .await?;
        }
        match self
            .learning_service
            .revoke(learning_id, now_ms(), reason.clone())
            .await
        {
            Ok(revoked) => Ok(revoked),
            Err(error) => {
                if let Some(skill) = prior_skill
                    && let Err(restore_error) = self
                        .learning_skill_service
                        .restore_active(skill.clone())
                        .await
                {
                    return Err(anyhow!(
                        "failed to revoke learning {learning_id}; promoted skill rollback also failed: {restore_error}; original error: {error}"
                    ));
                }
                Err(error)
            }
        }
    }

    pub(crate) async fn revoke_matching_learnings(
        &self,
        filter: &LearningListFilter,
        reason: Option<String>,
    ) -> Result<Vec<LearningView>> {
        validate_learning_revocation_reason(reason.as_deref())?;
        let matched = self.learning_service.list(filter).await;
        if matched.is_empty() {
            return Ok(Vec::new());
        }

        let originals = matched.clone();
        let mut prior_skills = std::collections::BTreeMap::new();
        for learning in &originals {
            if learning.kind == kheish_types::LearningKind::Procedure
                && let Some(skill) = self
                    .learning_skill_service
                    .live_by_learning(&learning.learning_id)
                    .await
            {
                prior_skills.insert(learning.learning_id.clone(), skill);
            }
        }

        let mut revoked = Vec::with_capacity(originals.len());
        for learning in &originals {
            match self
                .revoke_learning(&learning.learning_id, reason.clone())
                .await
            {
                Ok(value) => revoked.push(value),
                Err(error) => {
                    for restored in &originals {
                        if revoked.iter().any(|revoked_learning| {
                            revoked_learning.learning_id == restored.learning_id
                        }) {
                            let _ = self.learning_service.restore(restored.clone()).await;
                        }
                    }
                    for skill in prior_skills.values() {
                        let _ = self
                            .learning_skill_service
                            .restore_active(skill.clone())
                            .await;
                    }
                    return Err(anyhow!(
                        "failed to revoke matching learnings after {} successful updates: {error}",
                        revoked.len()
                    ));
                }
            }
        }
        Ok(revoked)
    }

    pub(crate) async fn supersede_learning(
        &self,
        learning_id: &str,
        replacement: LearningView,
    ) -> Result<LearningView> {
        let current = self.get_learning(learning_id).await?;
        let replacement = self.learning_policy_service.prepare_superseding_learning(
            &current,
            replacement,
            LearningMutationMode::Manual,
        )?;
        let prior_skill = if current.kind == kheish_types::LearningKind::Procedure {
            self.learning_skill_service
                .live_by_learning(learning_id)
                .await
        } else {
            None
        };
        if let Some(skill) = &prior_skill {
            self.learning_skill_service
                .revoke(
                    &skill.skill_name,
                    now_ms(),
                    Some(format!("source learning {learning_id} is being superseded")),
                )
                .await?;
        }
        match self
            .learning_service
            .supersede(learning_id, replacement)
            .await
        {
            Ok(superseded) => Ok(superseded),
            Err(error) => {
                if let Some(skill) = prior_skill
                    && let Err(restore_error) = self
                        .learning_skill_service
                        .restore_active(skill.clone())
                        .await
                {
                    return Err(anyhow!(
                        "failed to supersede learning {learning_id}; promoted skill rollback also failed: {restore_error}; original error: {error}"
                    ));
                }
                Err(error)
            }
        }
    }

    pub(crate) async fn list_learning_skills(&self) -> Result<Vec<crate::LearningSkillView>> {
        Ok(self.learning_skill_service.list().await)
    }

    pub(crate) async fn get_learning_skill(
        &self,
        skill_name: &str,
    ) -> Result<crate::LearningSkillView> {
        self.learning_skill_service
            .get(skill_name)
            .await
            .ok_or_else(|| anyhow!("unknown learning skill {skill_name}"))
    }

    pub(crate) async fn promote_learning_to_skill(
        &self,
        learning_id: &str,
        draft: crate::procedural_skills::LearningSkillDraft,
    ) -> Result<crate::LearningSkillView> {
        let learning = self.get_learning(learning_id).await?;
        let draft = self.learning_policy_service.prepare_promoted_skill(
            draft,
            &learning,
            LearningMutationMode::Manual,
        )?;
        self.learning_skill_service
            .promote(&learning, draft, now_ms())
            .await
    }

    pub(crate) async fn record_learning_skill_rollout_result(
        &self,
        skill_name: &str,
        request: crate::LearningSkillRolloutResultRequest,
    ) -> Result<crate::LearningSkillView> {
        let expected = request.expected_output_contains.trim();
        if expected.is_empty() {
            bail!("expected_output_contains is required");
        }
        let run = self.run_service.run_record(&request.run_id).await?;
        let latest_output = run.view.outputs.iter().rev().find_map(|output| {
            (!output.content.trim().is_empty()).then(|| output.content.as_str())
        });
        let success = run.view.status == DaemonRunStatus::Completed
            && latest_output.is_some_and(|output| output.contains(expected));
        self.learning_skill_service
            .record_rollout_result(
                skill_name,
                crate::procedural_skills::LearningSkillRolloutResult {
                    kind: request.kind,
                    run_id: run.view.run_id,
                    session_id: run.view.session_id,
                    success,
                    definition_fingerprint: request.definition_fingerprint,
                    recorded_at_ms: now_ms(),
                },
            )
            .await
    }

    pub(crate) async fn revoke_learning_skill(
        &self,
        skill_name: &str,
        reason: Option<String>,
    ) -> Result<crate::LearningSkillView> {
        self.learning_skill_service
            .revoke(skill_name, now_ms(), reason)
            .await
    }

    pub(crate) async fn rollback_learning_skill(
        &self,
        skill_name: &str,
        reason: Option<String>,
    ) -> Result<crate::LearningSkillView> {
        let skill = self.get_learning_skill(skill_name).await?;
        let source = self.get_learning(&skill.source_learning_id).await?;
        if source.kind != kheish_types::LearningKind::Procedure
            || source.status != kheish_types::LearningStatus::Active
            || source.publish_tier != kheish_types::LearningPublishTier::Active
        {
            bail!(
                "cannot rollback promoted skill {skill_name}; source learning {} is not an active procedure learning",
                skill.source_learning_id
            );
        }
        self.learning_skill_service
            .rollback(skill_name, now_ms(), reason)
            .await
    }

    async fn learning_publication_worker_loop(self: Arc<Self>) {
        loop {
            if let Some(candidate_id) = self.learning_policy_service.dequeue_candidate().await {
                if let Err(error) = self
                    .process_learning_candidate_automation(&candidate_id)
                    .await
                {
                    error!(
                        candidate_id = %candidate_id,
                        error = ?error,
                        "learning publication worker error"
                    );
                    self.learning_policy_service
                        .enqueue_candidate(candidate_id)
                        .await;
                    let notify = self.learning_policy_service.notify();
                    tokio::select! {
                        _ = notify.notified() => {}
                        _ = sleep_until(Instant::now() + Duration::from_millis(Self::LEARNING_AUTOMATION_RETRY_BACKOFF_MS)) => {}
                    }
                }
                continue;
            }
            let notify = self.learning_policy_service.notify();
            notify.notified().await;
        }
    }

    async fn process_learning_candidate_automation(&self, candidate_id: &str) -> Result<()> {
        let candidate = match self.learning_service.get_candidate(candidate_id).await {
            Some(candidate) => candidate,
            None => return Ok(()),
        };
        if matches!(
            candidate.state,
            crate::LearningCandidateState::Rejected | crate::LearningCandidateState::Escalated
        ) {
            return Ok(());
        }
        let reviewed_at_ms = now_ms();
        let Some(evaluation) = self
            .learning_policy_service
            .evaluate_candidate(&candidate, reviewed_at_ms)
            .await
        else {
            return Ok(());
        };
        if candidate.state == crate::LearningCandidateState::Published {
            if candidate.automation_review.is_none() {
                self.learning_service
                    .record_automation_review(candidate_id, None, evaluation.review(reviewed_at_ms))
                    .await?;
            }
            return Ok(());
        }
        let judged_evaluation = self
            .learning_judge_service
            .apply_if_enabled(
                &candidate,
                evaluation,
                &self.learning_policy_service.settings(),
                reviewed_at_ms,
            )
            .await?;
        let effective_evaluation = self
            .effective_automation_evaluation(&candidate, judged_evaluation)
            .await?;
        match effective_evaluation.mode {
            crate::LearningAutomationMode::Shadow => {
                self.learning_service
                    .record_automation_review(
                        candidate_id,
                        None,
                        effective_evaluation.review(reviewed_at_ms),
                    )
                    .await?;
            }
            crate::LearningAutomationMode::Enabled => match effective_evaluation.action {
                crate::LearningPublicationAction::ManualReview => {
                    self.learning_service
                        .record_automation_review(
                            candidate_id,
                            Some(crate::LearningCandidateState::Escalated),
                            effective_evaluation.review(reviewed_at_ms),
                        )
                        .await?;
                }
                crate::LearningPublicationAction::Reject => {
                    self.learning_service
                        .record_automation_review(
                            candidate_id,
                            Some(crate::LearningCandidateState::Rejected),
                            effective_evaluation.review(reviewed_at_ms),
                        )
                        .await?;
                }
                crate::LearningPublicationAction::PublishProvisional
                | crate::LearningPublicationAction::PublishActive => {
                    let Some(mut learning) =
                        self.learning_policy_service.build_automatic_learning(
                            &candidate,
                            self.next_learning_id(),
                            reviewed_at_ms,
                            &effective_evaluation,
                        )?
                    else {
                        return Ok(());
                    };
                    if effective_evaluation.action
                        == crate::LearningPublicationAction::PublishActive
                    {
                        learning.verification_status =
                            kheish_types::LearningVerificationStatus::Verified;
                    }
                    let learning = self.learning_policy_service.prepare_published_learning(
                        &candidate,
                        learning,
                        LearningMutationMode::Automatic,
                    )?;
                    self.learning_service
                        .publish_candidate(candidate_id, learning)
                        .await?;
                    self.learning_service
                        .record_automation_review(
                            candidate_id,
                            None,
                            effective_evaluation.review(reviewed_at_ms),
                        )
                        .await?;
                }
            },
            crate::LearningAutomationMode::ManualOnly => {}
        }
        Ok(())
    }

    async fn effective_automation_evaluation(
        &self,
        candidate: &crate::LearningCandidateView,
        mut evaluation: crate::services::LearningAutomationEvaluation,
    ) -> Result<crate::services::LearningAutomationEvaluation> {
        if evaluation.action != crate::LearningPublicationAction::PublishActive {
            return Ok(evaluation);
        }
        if let Some(conflict) = self
            .learning_service
            .active_conflicting_learning_for_candidate(candidate)
            .await
        {
            evaluation.action = crate::LearningPublicationAction::ManualReview;
            evaluation.reason = format!(
                "{}; escalated because candidate conflicts with active learning {}",
                evaluation.reason, conflict.learning_id
            );
            return Ok(evaluation);
        }
        if self
            .candidate_supports_automatic_active_publication(candidate)
            .await?
        {
            return Ok(evaluation);
        }
        evaluation.action = crate::LearningPublicationAction::PublishProvisional;
        evaluation.reason = format!(
            "{}; retained as provisional because no daemon-owned evidence matched the candidate content",
            evaluation.reason
        );
        Ok(evaluation)
    }

    async fn candidate_supports_automatic_active_publication(
        &self,
        candidate: &crate::LearningCandidateView,
    ) -> Result<bool> {
        let needle = candidate.content.trim();
        if needle.is_empty() {
            return Ok(false);
        }
        let Some(source_run_id) = candidate.source.run_id.as_deref() else {
            return Ok(false);
        };
        if candidate.origin == crate::LearningCandidateOrigin::Daemon
            && matches!(
                candidate.kind,
                kheish_types::LearningKind::Fact
                    | kheish_types::LearningKind::Preference
                    | kheish_types::LearningKind::Decision
            )
            && candidate
                .evidence_refs
                .iter()
                .any(|evidence| evidence.run_id.as_deref() == Some(source_run_id))
            && let Some(record) = self
                .run_service
                .run_memory_store()
                .load_run_memory(source_run_id)?
            && crate::memory::run_memory_supports_candidate_content(&record, needle)
        {
            return Ok(true);
        }
        for evidence in &candidate.evidence_refs {
            let Some(run_id) = evidence.run_id.as_deref() else {
                continue;
            };
            if run_id != source_run_id {
                continue;
            }
            let Some(artifact_id) = evidence.artifact_id.as_deref() else {
                continue;
            };
            let body = match self.run_debug_artifact(run_id, artifact_id) {
                Ok(body) => body,
                Err(_) => continue,
            };
            if body.contains(needle) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn validate_learning_revocation_reason(reason: Option<&str>) -> Result<()> {
    let Some(reason) = reason.map(str::trim).filter(|reason| !reason.is_empty()) else {
        return Ok(());
    };
    if crate::learning::learning_content_has_secret_material(reason) {
        bail!("learning revocation reason contains secret-like material");
    }
    Ok(())
}
