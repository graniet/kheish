//! Session, agent, skill, and asset view methods implemented on [`DaemonState`].

use std::collections::{BTreeMap, BTreeSet};

use super::*;

const DEFAULT_SESSION_MEMORY_SEARCH_LIMIT: usize = 12;
const MAX_SESSION_MEMORY_SEARCH_LIMIT: usize = 50;

#[derive(Clone, Debug)]
struct ResolvedSessionMemoryProjection {
    effective_capability_scope: kheish_types::CapabilityScope,
    learning_scopes: Vec<kheish_types::LearningScope>,
    learned_context: Option<kheish_types::LearnedContextBundle>,
    recovered_memory: Option<kheish_types::RecoveredMemoryBundle>,
    visible_skills: Vec<SkillSummaryView>,
}

#[derive(Clone, Debug, Default)]
struct MemorySearchMatch {
    score: u64,
    excerpt: Option<String>,
    matched_fields: Vec<String>,
}

fn apply_agent_summary_run_overlay(
    summary: &mut AgentSummaryView,
    overlay: &crate::services::AgentRunSummaryOverlay,
) {
    summary.active_run_id = overlay.active_run_id.clone();
    summary.queued_run_count = overlay.queued_run_count;
    summary.pending_approval_count = overlay.pending_approval_count;
    summary.pending_question_count = overlay.pending_question_count;
    summary.pending_parent_clarification_count = overlay.pending_parent_clarification_count;
    summary.pending_parent_clarification_run_ids =
        overlay.pending_parent_clarification_run_ids.clone();
    summary.last_error = overlay.last_error.clone();
    summary.last_output_preview = overlay.last_output_preview.clone();
    summary.last_output_truncated = overlay.last_output_truncated;
    if let Some(activity_at_ms) = overlay.last_activity_at_ms {
        summary.last_activity_at_ms = Some(
            summary
                .last_activity_at_ms
                .map_or(activity_at_ms, |current| current.max(activity_at_ms)),
        );
    }

    if overlay.pending_parent_clarification_count > 0 {
        summary.status = AgentStatus::WaitingForUserInput;
        return;
    }

    if overlay.active_agent_id.as_deref() != Some(summary.agent_id.as_str()) {
        if summary.status == AgentStatus::Idle && overlay.queued_run_count > 0 {
            summary.status = AgentStatus::Running;
        }
        return;
    }

    if overlay.pending_question_count > 0
        || matches!(
            (&overlay.active_run_kind, &overlay.active_run_status),
            (
                Some(DaemonRunKind::ParentClarification),
                Some(DaemonRunStatus::WaitingForUserQuestion)
            )
        )
    {
        summary.status = AgentStatus::WaitingForUserInput;
        return;
    }

    if overlay.pending_approval_count > 0
        || matches!(
            overlay.active_run_status.as_ref(),
            Some(DaemonRunStatus::WaitingForApproval)
        )
    {
        summary.status = AgentStatus::WaitingForApproval;
        return;
    }

    if summary.status == AgentStatus::Idle
        && matches!(
            overlay.active_run_status.as_ref(),
            Some(DaemonRunStatus::Queued | DaemonRunStatus::Running)
        )
    {
        summary.status = AgentStatus::Running;
    }
}

fn effective_session_capability_scope(
    persona_binding: Option<&kheish_types::SessionPersonaBinding>,
    session_capability_scope: &kheish_types::CapabilityScope,
) -> kheish_types::CapabilityScope {
    persona_binding
        .map(|binding| binding.capability_scope.clone())
        .unwrap_or_default()
        .restrict_with(session_capability_scope)
}

fn effective_session_credential_scope(
    session_credential_scope: &kheish_types::CredentialScope,
) -> kheish_types::CredentialScope {
    session_credential_scope.normalized()
}

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    async fn resolved_session_memory_projection(
        &self,
        session_id: &str,
        query: Option<&str>,
    ) -> Result<ResolvedSessionMemoryProjection> {
        let capability_scope = self.load_session_capability_scope(session_id).await?;
        let persona_binding = self.load_session_persona_binding(session_id).await?;
        let effective_capability_scope =
            effective_session_capability_scope(persona_binding.as_ref(), &capability_scope);
        let learning_scopes = self.learning_scopes_for_session(session_id).await?;
        let learned_context = if query.is_some() {
            self.learning_service
                .learned_context_bundle_for_query(&learning_scopes, now_ms(), query)
                .await
        } else {
            self.learning_service
                .learned_context_bundle(&learning_scopes, now_ms())
                .await
        };
        let recovered_memory = self
            .recovered_memory_bundle(session_id, query, RecoveredMemoryBundleUsage::Preview)
            .await;
        let visible_skills = self
            .session_visible_skill_summaries(session_id, None)
            .await?
            .into_iter()
            .map(SkillSummaryView::from)
            .collect();
        Ok(ResolvedSessionMemoryProjection {
            effective_capability_scope,
            learning_scopes,
            learned_context,
            recovered_memory,
            visible_skills,
        })
    }

    pub(crate) async fn session_visible_skill_summaries(
        &self,
        session_id: &str,
        query: Option<&str>,
    ) -> Result<Vec<kheish_skills::SkillSummary>> {
        let capability_scope = self.load_session_capability_scope(session_id).await?;
        let persona_binding = self.load_session_persona_binding(session_id).await?;
        let effective_capability_scope =
            effective_session_capability_scope(persona_binding.as_ref(), &capability_scope);
        let visible_learning_scope_keys = self
            .learning_scopes_for_session(session_id)
            .await?
            .into_iter()
            .map(|scope| scope.scope_key())
            .collect::<BTreeSet<_>>();
        let promoted_scope_by_skill = self
            .learning_skill_service
            .list()
            .await
            .into_iter()
            .filter(|record| record.status == crate::LearningSkillStatus::Active)
            .map(|record| (record.skill_name, record.source_scope.scope_key()))
            .collect::<BTreeMap<_, _>>();
        Ok(self
            .skills
            .search(query)
            .into_iter()
            .filter(|skill| effective_capability_scope.allows_skill(&skill.name))
            .filter(|skill| {
                promoted_scope_by_skill
                    .get(&skill.name)
                    .map(|scope_key| visible_learning_scope_keys.contains(scope_key))
                    .unwrap_or(true)
            })
            .collect())
    }

    pub(crate) async fn session_memory_context_view(
        &self,
        session_id: &str,
        query: Option<&str>,
    ) -> Result<SessionMemoryContextView> {
        let projection = self
            .resolved_session_memory_projection(session_id, query)
            .await?;
        Ok(SessionMemoryContextView {
            session_id: session_id.to_string(),
            effective_capability_scope: projection.effective_capability_scope,
            learning_scopes: projection.learning_scopes,
            learned_context: projection.learned_context,
            recovered_memory: projection.recovered_memory,
            visible_skills: projection.visible_skills,
        })
    }

    pub(crate) async fn session_memory_search_view(
        &self,
        session_id: &str,
        query: Option<&str>,
        limit: Option<usize>,
    ) -> Result<SessionMemorySearchView> {
        let projection = self
            .resolved_session_memory_projection(session_id, None)
            .await?;
        let normalized_query = normalize_memory_search_query(query);
        let limit = limit
            .unwrap_or(DEFAULT_SESSION_MEMORY_SEARCH_LIMIT)
            .clamp(1, MAX_SESSION_MEMORY_SEARCH_LIMIT);
        let now = now_ms();
        let run_memory_policy = self.run_memory.policy();
        let mut results = Vec::new();

        for record in self
            .learning_service
            .visible_records_for_scopes(&projection.learning_scopes, now)
            .await
        {
            if !crate::services::learning_is_session_memory_search_visible(&record) {
                continue;
            }
            let matched =
                crate::services::rank_learning_record(&record, normalized_query.as_deref());
            if normalized_query.is_some() && matched.score == 0 {
                continue;
            }
            results.push(SessionMemorySearchResultView {
                kind: SessionMemorySearchResultKind::Learning,
                source_id: record.learning_id.clone(),
                title: format!("{} learning", learning_kind_label(&record.kind)),
                excerpt: truncate_memory_excerpt(&record.content),
                score: matched.score,
                timestamp_ms: record.published_at_ms,
                scope: Some(record.scope.clone()),
                prompt_eligible: crate::services::learning_is_prompt_visible(&record)
                    && record.kind.is_prompt_eligible(),
                learning_status: Some(record.status.clone()),
                publish_tier: Some(record.publish_tier.clone()),
                verification_status: Some(record.verification_status.clone()),
                matched_fields: matched.matched_fields,
            });
        }

        if run_memory_policy.enabled {
            let mut expired_by_session: BTreeMap<String, Vec<String>> = BTreeMap::new();
            let tracked_run_memory_entries = match run_memory_policy.search_visibility {
                crate::RunMemorySearchVisibility::SessionOnly => {
                    self.session_service
                        .tracked_run_memory_entries(session_id)
                        .await
                }
                crate::RunMemorySearchVisibility::LearningScopes => {
                    self.session_service
                        .tracked_run_memory_entries_for_scopes(&projection.learning_scopes)
                        .await
                }
            };
            for entry in tracked_run_memory_entries {
                let index_entry_expired =
                    run_memory_entry_expired(entry.recorded_at_ms, now, &run_memory_policy);
                let record = match self
                    .run_service
                    .run_memory_store()
                    .load_run_memory(&entry.run_id)
                {
                    Ok(Some(record)) => record,
                    Ok(None) => {
                        self.run_memory.record_skipped_unreadable();
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(
                            run_id = %entry.run_id,
                            error = %error,
                            "skipping unreadable run memory search record"
                        );
                        self.run_memory.record_skipped_unreadable();
                        continue;
                    }
                };
                if index_entry_expired
                    || run_memory_entry_expired(
                        record.memory.recorded_at_ms,
                        now,
                        &run_memory_policy,
                    )
                {
                    expired_by_session
                        .entry(record.session_id.clone())
                        .or_default()
                        .push(record.memory.run_id.clone());
                    if let Err(error) = self
                        .run_service
                        .run_memory_store()
                        .delete_run_memory(&record.memory.run_id)
                    {
                        tracing::warn!(
                            run_id = %record.memory.run_id,
                            error = %error,
                            "failed to delete expired run memory search record"
                        );
                    }
                    continue;
                }
                let mut matched = match_run_memory_record(&record, normalized_query.as_deref());
                if normalized_query.is_some() {
                    matched.score = matched.score.saturating_add(rank_run_memory_record(
                        &record,
                        normalized_query.as_deref(),
                    ));
                }
                if normalized_query.is_some() && matched.score == 0 {
                    continue;
                }
                results.push(SessionMemorySearchResultView {
                    kind: SessionMemorySearchResultKind::RecoveredRun,
                    source_id: record.memory.run_id.clone(),
                    title: format!("Recovered run {}", record.memory.run_id),
                    excerpt: matched
                        .excerpt
                        .unwrap_or_else(|| truncate_memory_excerpt(&record.memory.summary)),
                    score: matched.score,
                    timestamp_ms: record.memory.recorded_at_ms,
                    scope: Some(kheish_types::LearningScope {
                        kind: kheish_types::LearningScopeKind::Session,
                        id: record.session_id.clone(),
                    }),
                    prompt_eligible: projection.recovered_memory.as_ref().is_some_and(|bundle| {
                        bundle
                            .entries
                            .iter()
                            .any(|memory| memory.run_id == record.memory.run_id)
                    }),
                    learning_status: None,
                    publish_tier: None,
                    verification_status: None,
                    matched_fields: matched.matched_fields,
                });
            }
            let expired_count = expired_by_session.values().map(Vec::len).sum::<usize>();
            self.run_memory.record_pruned_ttl(expired_count);
            for (source_session_id, run_ids) in expired_by_session {
                if let Err(error) = self
                    .session_service
                    .forget_run_memories(&source_session_id, &run_ids)
                    .await
                {
                    tracing::warn!(
                        session_id = %source_session_id,
                        error = %error,
                        "failed to forget expired run memory search pointers"
                    );
                }
            }
        }

        if normalized_query.is_some() {
            for skill in &projection.visible_skills {
                let matched = match_skill_summary(skill, normalized_query.as_deref());
                if matched.score == 0 {
                    continue;
                }
                results.push(SessionMemorySearchResultView {
                    kind: SessionMemorySearchResultKind::Skill,
                    source_id: skill.name.clone(),
                    title: format!("Skill {}", skill.name),
                    excerpt: matched
                        .excerpt
                        .unwrap_or_else(|| truncate_memory_excerpt(&skill.description)),
                    score: matched.score,
                    timestamp_ms: 0,
                    scope: None,
                    prompt_eligible: false,
                    learning_status: None,
                    publish_tier: None,
                    verification_status: None,
                    matched_fields: matched.matched_fields,
                });
            }
        }

        results.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| right.timestamp_ms.cmp(&left.timestamp_ms))
                .then_with(|| left.source_id.cmp(&right.source_id))
        });
        let truncated = results.len() > limit;
        results.truncate(limit);
        Ok(SessionMemorySearchView {
            session_id: session_id.to_string(),
            effective_capability_scope: projection.effective_capability_scope,
            learning_scopes: projection.learning_scopes,
            query: normalized_query,
            truncated,
            results,
        })
    }

    pub(crate) async fn session_view(
        &self,
        session_id: &str,
        agent_id: &AgentId,
    ) -> Result<SessionView> {
        let snapshot = self.live_snapshot(agent_id).await?;
        let route_policy = self.load_session_route_policy(session_id).await?;
        let goal = self.load_session_goal(session_id).await?;
        let capability_scope = self.load_session_capability_scope(session_id).await?;
        let credential_scope = self.load_session_credential_scope(session_id).await?;
        let persona_binding = self.load_session_persona_binding(session_id).await?;
        let operator = self.load_session_operator_config(session_id).await?;
        let reply_targets = self.session_reply_targets(session_id).await;
        let effective_capability_scope =
            effective_session_capability_scope(persona_binding.as_ref(), &capability_scope);
        let effective_credential_scope = effective_session_credential_scope(&credential_scope);
        let persona = persona_binding.map(crate::SessionPersonaSummaryView::from);
        Ok(SessionView {
            session_id: session_id.to_string(),
            agent_id: agent_id.0.clone(),
            snapshot,
            route_policy,
            goal,
            capability_scope,
            effective_capability_scope,
            credential_scope,
            effective_credential_scope,
            persona,
            operator,
            reply_targets,
            outputs: self.delivery_service.session_outputs(session_id).await?,
        })
    }

    pub(crate) async fn session_permission_audits(
        &self,
        session_id: &str,
    ) -> Result<SessionPermissionAuditListView> {
        self.agent_id_for_session(session_id).await?;
        let stored = self.session_service.load_session(session_id).await?;
        Ok(SessionPermissionAuditListView {
            session_id: session_id.to_string(),
            audits: stored.audits,
        })
    }

    pub(crate) async fn list_sessions(
        &self,
        persona_id: Option<&str>,
    ) -> Result<Vec<SessionViewSummary>> {
        let pairs = if let Some(persona_id) = persona_id {
            self.session_service
                .session_pairs_for_persona(persona_id)
                .await
        } else {
            self.session_service.session_pairs().await
        };
        let mut sessions = Vec::with_capacity(pairs.len());
        for (session_id, agent_id) in pairs {
            let persona_binding = self.load_session_persona_binding(&session_id).await?;
            let persona = persona_binding
                .as_ref()
                .map(crate::SessionPersonaSummaryView::from);
            if persona_id.is_some_and(|persona_id| {
                persona.as_ref().map(|binding| binding.persona_id.as_str()) != Some(persona_id)
            }) {
                continue;
            }
            let snapshot = self.live_snapshot(&AgentId(agent_id.clone())).await?;
            if snapshot.agent.parent.is_some() && snapshot.agent.closed_at_ms.is_some() {
                continue;
            }
            let route_policy = self.load_session_route_policy(&session_id).await?;
            let goal = self.load_session_goal(&session_id).await?;
            let capability_scope = self.load_session_capability_scope(&session_id).await?;
            let credential_scope = self.load_session_credential_scope(&session_id).await?;
            let operator = self.load_session_operator_config(&session_id).await?;
            let reply_targets = self.session_reply_targets(&session_id).await;
            let effective_capability_scope =
                effective_session_capability_scope(persona_binding.as_ref(), &capability_scope);
            let effective_credential_scope = effective_session_credential_scope(&credential_scope);
            sessions.push(SessionViewSummary {
                session_id,
                agent_id,
                status: snapshot.agent.status,
                pending_approvals: snapshot.pending_approvals.len(),
                pending_questions: snapshot.pending_questions.len(),
                route_policy,
                goal,
                capability_scope,
                effective_capability_scope,
                credential_scope,
                effective_credential_scope,
                persona,
                operator,
                reply_targets,
            });
        }
        Ok(sessions)
    }

    pub(crate) async fn list_skills(&self, query: Option<&str>) -> Result<Vec<SkillSummaryView>> {
        Ok(self
            .skills
            .search(query)
            .into_iter()
            .map(SkillSummaryView::from)
            .collect())
    }

    pub(crate) async fn get_skill(&self, name: &str) -> Result<SkillView> {
        let skill = self
            .skills
            .get(name)
            .ok_or_else(|| anyhow!("unknown skill {name}"))?;
        Ok(SkillView::from(skill))
    }

    pub(crate) async fn list_session_skills(
        &self,
        session_id: &str,
        query: Option<&str>,
    ) -> Result<Vec<SkillSummaryView>> {
        Ok(self
            .session_visible_skill_summaries(session_id, query)
            .await?
            .into_iter()
            .map(SkillSummaryView::from)
            .collect())
    }

    pub(crate) async fn list_assets(&self, query: Option<&str>) -> Result<Vec<AssetSummaryView>> {
        Ok(self
            .assets
            .list(query)
            .into_iter()
            .map(AssetSummaryView::from)
            .collect())
    }

    pub(crate) async fn get_asset(&self, asset_id: &str) -> Result<AssetView> {
        let asset = self
            .assets
            .get(asset_id)
            .ok_or_else(|| anyhow!("unknown asset {asset_id}"))?;
        Ok(AssetView::from(asset))
    }

    pub(crate) async fn get_asset_raw(
        &self,
        asset_id: &str,
    ) -> Result<(StoredAssetRecord, Vec<u8>)> {
        self.assets.read_raw(asset_id)
    }

    pub(crate) async fn pending_delivery_record(
        &self,
        delivery_id: &str,
    ) -> Result<Option<crate::delivery::PendingDeliveryRecord>> {
        crate::delivery::load_pending_delivery_record(self.store.root(), delivery_id)
    }

    pub(crate) async fn load_asset_attachment(&self, asset_id: &str) -> Result<AttachmentRef> {
        let asset = self
            .assets
            .get(asset_id)
            .ok_or_else(|| anyhow!("unknown asset {asset_id}"))?;
        Ok(asset.attachment_ref())
    }

    pub(crate) async fn generate_image(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        tool_call_id: Option<&str>,
        request: GenerateImageToolRequest,
    ) -> Result<GenerateImageToolResponse> {
        let service = self
            .image_generation
            .as_ref()
            .ok_or_else(|| anyhow!("no image-generation backend is configured"))?;
        let preferred_route_id = if let Some(run_id) = run_id {
            self.run_service
                .run_record(run_id)
                .await
                .ok()
                .and_then(|record| record.view.request.provider)
        } else {
            None
        };
        // `preferred_route_id` (the run's text provider) is passed to `generate_with_context` as a
        // *soft* preference: tried first when a matching image backend exists, else default/any.
        // Copying it into `request.route.provider` would make it a *hard* override and wrongly fail
        // generation on runs whose text provider has no matching image backend. An explicit provider
        // supplied by the model in the tool call stays in `request.route.provider` as a hard override.
        let credential_scope = self.load_session_credential_scope(session_id).await?;
        service
            .generate_with_context(
                request,
                preferred_route_id.as_deref(),
                Some(&credential_scope),
                ImageToolExecutionContext {
                    session_id: Some(session_id.to_string()),
                    run_id: run_id.map(ToOwned::to_owned),
                    tool_call_id: tool_call_id.map(ToOwned::to_owned),
                },
            )
            .await
    }

    pub(crate) async fn generate_audio(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        tool_call_id: Option<&str>,
        request: GenerateAudioToolRequest,
    ) -> Result<GenerateAudioToolResponse> {
        let service = self
            .audio_generation
            .as_ref()
            .ok_or_else(|| anyhow!("no audio-generation backend is configured"))?;
        let preferred_route_id = if let Some(run_id) = run_id {
            self.run_service
                .run_record(run_id)
                .await
                .ok()
                .and_then(|record| record.view.request.provider)
        } else {
            None
        };
        // `preferred_route_id` (the run's text provider) is passed to `generate_with_context` as a
        // *soft* preference: tried first when a matching audio backend exists, else default/any.
        // Copying it into `request.route.provider` would make it a *hard* override and wrongly fail
        // generation on runs whose text provider has no matching audio backend. An explicit provider
        // supplied by the model in the tool call stays in `request.route.provider` as a hard override.
        let credential_scope = self.load_session_credential_scope(session_id).await?;
        service
            .generate_with_context(
                request,
                preferred_route_id.as_deref(),
                Some(&credential_scope),
                crate::audio_generation::AudioToolExecutionContext {
                    session_id: Some(session_id.to_string()),
                    run_id: run_id.map(ToOwned::to_owned),
                    tool_call_id: tool_call_id.map(ToOwned::to_owned),
                },
            )
            .await
    }

    pub(crate) async fn edit_image(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        tool_call_id: Option<&str>,
        mut request: EditImageToolRequest,
    ) -> Result<EditImageToolResponse> {
        let service = self
            .image_generation
            .as_ref()
            .ok_or_else(|| anyhow!("no image-edit backend is configured"))?;
        if request.image_asset_ids_was_omitted {
            request.image_asset_ids = self.infer_edit_image_asset_ids(session_id, run_id).await?;
        }
        let preferred_route_id = if let Some(run_id) = run_id {
            self.run_service
                .run_record(run_id)
                .await
                .ok()
                .and_then(|record| record.view.request.provider)
        } else {
            None
        };
        // `preferred_route_id` is the run's text provider. It is a *soft* preference: pass it to
        // `edit_with_context` so the service tries it first when a matching image-edit backend
        // exists, then falls back to the default/any configured route. Copying it into
        // `request.route.provider` would turn it into a *hard* override and wrongly fail edits on
        // runs whose text provider has no matching image-edit backend (e.g. an Anthropic run
        // editing through the default OpenAI image route). Only an explicit provider supplied by
        // the model in the tool call should be treated as a hard route override.
        let credential_scope = self.load_session_credential_scope(session_id).await?;
        service
            .edit_with_context(
                request,
                preferred_route_id.as_deref(),
                Some(&credential_scope),
                ImageToolExecutionContext {
                    session_id: Some(session_id.to_string()),
                    run_id: run_id.map(ToOwned::to_owned),
                    tool_call_id: tool_call_id.map(ToOwned::to_owned),
                },
            )
            .await
    }

    async fn infer_edit_image_asset_ids(
        &self,
        session_id: &str,
        run_id: Option<&str>,
    ) -> Result<Vec<String>> {
        let Some(run_id) = run_id else {
            bail!("edit_image requires image_asset_ids when no active run context is available");
        };
        let record = self.run_service.run_record(run_id).await?;
        anyhow::ensure!(
            record.view.session_id == session_id,
            "run {run_id} does not belong to session {session_id}"
        );
        let request = current_turn_submit_input_request(&record.payload).ok_or_else(|| {
            anyhow!(
                "edit_image omitted image_asset_ids but run {run_id} does not have one direct user-input turn to infer from"
            )
        })?;
        let image_asset_ids = self.current_turn_image_asset_ids(request)?;
        match image_asset_ids.len() {
            0 => bail!(
                "edit_image omitted image_asset_ids but the current user turn does not include any image attachments"
            ),
            1 => Ok(image_asset_ids),
            _ => bail!(
                "edit_image omitted image_asset_ids but the current user turn includes multiple image attachments; pass explicit image_asset_ids in attachment order"
            ),
        }
    }

    fn current_turn_image_asset_ids(&self, request: &SubmitInputRequest) -> Result<Vec<String>> {
        if !request.input_items.is_empty() {
            let mut image_asset_ids = Vec::new();
            for item in &request.input_items {
                match item {
                    SubmitInputItemRequest::AssetReference { asset_id } => {
                        if let Some(asset_id) = self.image_asset_id(asset_id)? {
                            image_asset_ids.push(asset_id);
                        }
                    }
                    SubmitInputItemRequest::BoardReference { .. } => {
                        bail!(
                            "edit_image fallback requires normalized daemon asset references in the current user turn"
                        );
                    }
                    SubmitInputItemRequest::InlineAsset(_) => {
                        bail!(
                            "edit_image fallback requires normalized daemon asset references in the current user turn"
                        );
                    }
                    SubmitInputItemRequest::Text { .. } => {}
                }
            }
            return Ok(image_asset_ids);
        }

        let mut image_asset_ids = Vec::new();
        for attachment in &request.attachments {
            match attachment {
                InputAttachmentRequest::AssetReference { asset_id } => {
                    if let Some(asset_id) = self.image_asset_id(asset_id)? {
                        image_asset_ids.push(asset_id);
                    }
                }
                InputAttachmentRequest::InlineAsset(_) => {
                    bail!(
                        "edit_image fallback requires normalized daemon asset references in the current user turn"
                    );
                }
            }
        }
        Ok(image_asset_ids)
    }

    fn image_asset_id(&self, asset_id: &str) -> Result<Option<String>> {
        let asset = self
            .assets
            .get(asset_id)
            .ok_or_else(|| anyhow!("current user turn references unknown asset {asset_id}"))?;
        Ok(asset.is_image().then_some(asset.id))
    }

    pub(crate) async fn import_asset(&self, upload: &InlineAssetUpload) -> Result<AssetView> {
        let asset = self.import_asset_upload(upload).await?;
        Ok(AssetView::from(asset))
    }

    pub(super) async fn live_snapshot(&self, agent_id: &AgentId) -> Result<ManagedAgentSnapshot> {
        let snapshot = if self.orchestrator.has_runtime(agent_id) {
            self.orchestrator.snapshot(agent_id).await?
        } else if let Some(snapshot) = self.supervisor.terminal_snapshot(agent_id) {
            snapshot
        } else if let Some(record) = self.supervisor.get(agent_id) {
            Self::passive_snapshot_for_record(record)
        } else {
            anyhow::bail!("unknown agent {}", agent_id.0);
        };
        self.overlay_daemon_wait_state(snapshot).await
    }

    async fn overlay_daemon_wait_state(
        &self,
        mut snapshot: ManagedAgentSnapshot,
    ) -> Result<ManagedAgentSnapshot> {
        if !snapshot.pending_questions.is_empty() {
            return Ok(snapshot);
        }
        if let Some(questions) = self
            .run_service
            .pending_parent_clarifications_by_requester()
            .await
            .remove(&snapshot.agent.id.0)
            && !questions.is_empty()
        {
            snapshot.agent.status = kheish_agent::AgentStatus::WaitingForUserInput;
            return Ok(snapshot);
        }
        let session_id = snapshot.agent.conversation.session_id.clone();
        let session_state = self.run_service.session_state(&session_id).await;
        let Some(session_state) = session_state else {
            return Ok(snapshot);
        };
        let active_run_id = session_state.active_run_id.as_deref();
        let Some(active_run_id) = active_run_id else {
            if snapshot.agent.status == AgentStatus::Idle
                && !session_state.queued_run_ids.is_empty()
            {
                snapshot.agent.status = AgentStatus::Running;
            }
            return Ok(snapshot);
        };
        let run = self.run_service.run_record(active_run_id).await.ok();
        let Some(run) = run else {
            if snapshot.agent.status == AgentStatus::Idle {
                snapshot.agent.status = AgentStatus::Running;
            }
            return Ok(snapshot);
        };
        if run.view.status == DaemonRunStatus::WaitingForApproval
            && run.view.agent_id == snapshot.agent.id.0
        {
            snapshot.agent.status = kheish_agent::AgentStatus::WaitingForApproval;
            return Ok(snapshot);
        }
        if snapshot.agent.status == AgentStatus::Idle
            && matches!(
                run.view.status,
                DaemonRunStatus::Queued | DaemonRunStatus::Running
            )
        {
            snapshot.agent.status = AgentStatus::Running;
        }
        if run.view.kind == DaemonRunKind::ParentClarification
            && run.view.status == DaemonRunStatus::WaitingForUserQuestion
            && run.view.agent_id == snapshot.agent.id.0
        {
            snapshot.agent.status = kheish_agent::AgentStatus::WaitingForUserInput;
            snapshot.pending_questions = run.view.pending_questions;
        }
        Ok(snapshot)
    }

    pub(crate) async fn list_pending_questions(
        self: &Arc<Self>,
        session_id: Option<&str>,
    ) -> Result<Vec<PendingQuestionView>> {
        self.expire_due_user_questions(now_ms()).await?;
        Ok(self.run_service.list_pending_questions(session_id))
    }

    pub(crate) async fn get_agent(&self, agent_id: &str) -> Result<ManagedAgentSnapshot> {
        self.live_snapshot(&AgentId(agent_id.to_string())).await
    }

    pub(crate) fn agent_supervisor_audit(
        &self,
        agent_id: Option<&str>,
    ) -> Vec<AgentSupervisorAuditEntry> {
        let agent_id = agent_id.map(|agent_id| AgentId(agent_id.to_string()));
        self.supervisor.audit_log(agent_id.as_ref())
    }

    pub(crate) async fn summarize_agent_records(
        &self,
        records: Vec<AgentRecord>,
    ) -> Vec<AgentSummaryView> {
        let mailbox_counts = self.supervisor.mailbox_counts();
        let runtime_ids = self.orchestrator.runtime_ids();
        let run_overlays = self.run_service.agent_summary_overlays().await;
        let mut summaries = Vec::with_capacity(records.len());
        for record in records {
            let mut summary = AgentSummaryView::from_record(
                &record,
                mailbox_counts.get(&record.id).copied().unwrap_or(0),
                runtime_ids.contains(&record.id),
            );
            if let Some(overlay) = run_overlays.get(&record.conversation.session_id) {
                apply_agent_summary_run_overlay(&mut summary, overlay);
            }
            summaries.push(summary);
        }
        summaries
    }

    pub(crate) async fn list_agent_summaries(&self) -> Vec<AgentSummaryView> {
        self.summarize_agent_records(self.supervisor.list()).await
    }

    pub(crate) async fn list_agent_summaries_for_root(
        &self,
        agent_id: &AgentId,
    ) -> Result<Vec<AgentSummaryView>> {
        Ok(self
            .summarize_agent_records(self.supervisor.root_tree_records(agent_id)?)
            .await)
    }

    pub(crate) async fn set_agent_nickname(
        &self,
        agent_id: &str,
        nickname: Option<String>,
    ) -> Result<ManagedAgentSnapshot> {
        let agent_id = AgentId(agent_id.to_string());
        let previous = self
            .supervisor
            .get(&agent_id)
            .ok_or_else(|| anyhow!("unknown agent {}", agent_id.0))?;
        let updated = self
            .supervisor
            .set_nickname(&agent_id, nickname.as_deref())?;
        let previous_display_name = previous
            .nickname
            .clone()
            .or(previous.name.clone())
            .unwrap_or_else(|| previous.id.0.clone());
        let updated_display_name = updated
            .nickname
            .clone()
            .or(updated.name.clone())
            .unwrap_or_else(|| updated.id.0.clone());
        if let Err(error) = self.persist_topology().await {
            let _ = self
                .supervisor
                .set_nickname(&agent_id, previous.nickname.as_deref());
            return Err(error);
        }
        if previous_display_name != updated_display_name
            && let Err(error) = self
                .sync_following_channel_member_display_names(
                    &updated.conversation.session_id,
                    &updated_display_name,
                )
                .await
        {
            let _ = self
                .supervisor
                .set_nickname(&agent_id, previous.nickname.as_deref());
            let _ = self.persist_topology().await;
            let _ = self
                .sync_following_channel_member_display_names(
                    &updated.conversation.session_id,
                    &previous_display_name,
                )
                .await;
            return Err(error);
        }
        let snapshot = self.live_snapshot(&updated.id).await?;
        self.publish_snapshot_for_session(&updated.conversation.session_id, &snapshot);
        Ok(snapshot)
    }

    pub(crate) async fn list_agents(&self) -> Result<Vec<ManagedAgentSnapshot>> {
        let mut agents = Vec::new();
        for record in self.supervisor.list() {
            agents.push(self.live_snapshot(&record.id).await?);
        }
        Ok(agents)
    }

    pub(crate) async fn session_events(&self, session_id: &str) -> Result<SessionEventLogView> {
        self.agent_id_for_session(session_id).await?;
        Ok(SessionEventLogView {
            session: self.session_service.load_session(session_id).await?,
            daemon_outputs: self.delivery_service.session_outputs(session_id).await?,
            run_events: self.run_service.session_run_events(session_id).await?,
        })
    }
}

fn current_turn_submit_input_request(payload: &RunRequestPayload) -> Option<&SubmitInputRequest> {
    match payload {
        RunRequestPayload::Input { request, .. } => Some(request),
        RunRequestPayload::ObservationMaterialization { request }
        | RunRequestPayload::ScheduledObservationMaterialization { request, .. } => {
            Some(&request.request)
        }
        RunRequestPayload::ApprovalResume {
            original_request, ..
        }
        | RunRequestPayload::UserQuestionResume {
            original_request, ..
        } => original_request.as_ref(),
        RunRequestPayload::ScheduledInput { .. } => None,
        RunRequestPayload::MailboxDelivery { .. }
        | RunRequestPayload::ChannelDelivery { .. }
        | RunRequestPayload::ParentClarification { .. } => None,
    }
}

fn normalize_memory_search_query(query: Option<&str>) -> Option<String> {
    query
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_ascii_lowercase)
}

fn match_run_memory_record(
    record: &crate::RunMemoryRecord,
    query: Option<&str>,
) -> MemorySearchMatch {
    let mut matched = match_fields(&[("summary", &record.memory.summary)], query);
    if let Some(request_preview) = record.memory.request_preview.as_deref() {
        matched = merge_match_fields(
            matched,
            match_fields(&[("request_preview", request_preview)], query),
        );
    }
    if let Some(outcome_preview) = record.memory.outcome_preview.as_deref() {
        matched = merge_match_fields(
            matched,
            match_fields(&[("outcome_preview", outcome_preview)], query),
        );
    }
    if !record.memory.failure_markers.is_empty() {
        let joined = record.memory.failure_markers.join(" ");
        matched = merge_match_fields(
            matched,
            match_fields(&[("failure_markers", &joined)], query),
        );
    }
    matched
}

fn match_skill_summary(skill: &SkillSummaryView, query: Option<&str>) -> MemorySearchMatch {
    let when_to_use = skill.when_to_use.as_deref().unwrap_or_default();
    match_fields(
        &[
            ("name", &skill.name),
            ("description", &skill.description),
            ("when_to_use", when_to_use),
        ],
        query,
    )
}

fn merge_match_fields(left: MemorySearchMatch, right: MemorySearchMatch) -> MemorySearchMatch {
    let mut matched_fields = left.matched_fields;
    for field in right.matched_fields {
        if !matched_fields.iter().any(|existing| existing == &field) {
            matched_fields.push(field);
        }
    }
    MemorySearchMatch {
        score: left.score.saturating_add(right.score),
        excerpt: left.excerpt.or(right.excerpt),
        matched_fields,
    }
}

fn match_fields(fields: &[(&str, &str)], query: Option<&str>) -> MemorySearchMatch {
    let Some(query) = query else {
        let excerpt = fields
            .iter()
            .map(|(_, value)| value.trim())
            .find(|value| !value.is_empty())
            .map(truncate_memory_excerpt);
        return MemorySearchMatch {
            score: 0,
            excerpt,
            matched_fields: Vec::new(),
        };
    };
    let terms = query
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>();
    let mut matched = MemorySearchMatch::default();
    for (field_name, value) in fields {
        let lowered = value.to_ascii_lowercase();
        let mut score = 0u64;
        if lowered.contains(query) {
            score = score.saturating_add(100);
        }
        for term in &terms {
            if lowered.contains(term) {
                score = score.saturating_add(10);
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
            matched.matched_fields.push((*field_name).to_string());
        }
        if matched.excerpt.is_none() {
            matched.excerpt = Some(truncate_memory_excerpt(value));
        }
    }
    matched
}

fn truncate_memory_excerpt(content: &str) -> String {
    const MAX_EXCERPT_CHARS: usize = 180;
    let trimmed = content.trim();
    let mut excerpt = trimmed.chars().take(MAX_EXCERPT_CHARS).collect::<String>();
    if trimmed.chars().count() > MAX_EXCERPT_CHARS {
        excerpt.push('…');
    }
    excerpt
}

fn learning_kind_label(kind: &kheish_types::LearningKind) -> &'static str {
    match kind {
        kheish_types::LearningKind::Fact => "fact",
        kheish_types::LearningKind::Preference => "preference",
        kheish_types::LearningKind::Decision => "decision",
        kheish_types::LearningKind::Procedure => "procedure",
        kheish_types::LearningKind::RunSummary => "run summary",
    }
}
