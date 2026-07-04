//! Session ingress, request normalization, and envelope-building methods on [`DaemonState`].

use kheish_types::{CapabilityScope, CredentialScope};

use crate::api::{validate_input_attachment_requests, validate_submit_input_items};
use crate::problems::DaemonProblem;

use super::*;

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    fn strip_daemon_owned_metadata(metadata: Value) -> Value {
        let mut object = match metadata {
            Value::Object(map) => map,
            other => return other,
        };
        for key in [
            kheish_types::LEARNED_CONTEXT_METADATA_KEY,
            kheish_types::RECOVERED_MEMORY_METADATA_KEY,
            kheish_types::SESSION_SKILLS_STATE_METADATA_KEY,
            kheish_types::SESSION_VISIBLE_SKILLS_METADATA_KEY,
            KHEISH_FLOW_METADATA_KEY,
        ] {
            object.remove(key);
        }
        Value::Object(object)
    }

    fn combined_create_session_failure(
        error: anyhow::Error,
        cleanup_error: anyhow::Error,
    ) -> anyhow::Error {
        anyhow!(
            "{}; session create rollback failed: {}",
            error,
            cleanup_error
        )
    }

    pub(super) async fn effective_session_route_policy(
        &self,
        session_id: &str,
    ) -> Result<SessionRoutePolicy> {
        self.load_session_route_policy(session_id).await
    }

    fn merge_session_route_policy(
        policy: &SessionRoutePolicy,
        provider: Option<String>,
        generation: Option<ModelGenerationConfig>,
    ) -> (Option<String>, Option<ModelGenerationConfig>) {
        let explicit_model = generation
            .as_ref()
            .and_then(|generation| generation.model.as_ref())
            .is_some();
        let provider =
            provider.or_else(|| (!explicit_model).then(|| policy.provider.clone()).flatten());
        let generation = merge_generation_override(policy.generation.clone(), generation);
        (provider, generation)
    }

    pub(super) fn normalize_spawn_sidechain_route_policy(
        request: &mut SpawnSidechainRequest,
    ) -> Result<()> {
        let Some(route_policy) = request.route_policy.take() else {
            return Ok(());
        };
        if route_policy.is_empty() {
            request.provider = None;
            request.generation = None;
            request.fork_context.provider = None;
            request.fork_context.generation = None;
            return Ok(());
        }

        let legacy_provider = request
            .provider
            .clone()
            .or_else(|| request.fork_context.provider.clone());
        let legacy_generation = merge_generation_override(
            request.fork_context.generation.clone(),
            request.generation.clone(),
        );
        let same_provider = legacy_provider.is_none() || legacy_provider == route_policy.provider;
        let same_generation =
            legacy_generation.is_none() || legacy_generation == route_policy.generation;
        if !same_provider || !same_generation {
            anyhow::bail!(
                "route_policy cannot be combined with conflicting legacy provider or generation fields"
            );
        }

        request.provider = route_policy.provider.clone();
        request.generation = route_policy.generation.clone();
        request.fork_context.provider = route_policy.provider;
        request.fork_context.generation = route_policy.generation;
        Ok(())
    }

    pub(super) async fn resolve_generation_route_for_session(
        &self,
        session_id: &str,
        provider: Option<String>,
        generation: Option<ModelGenerationConfig>,
    ) -> Result<(Option<String>, Option<ModelGenerationConfig>)> {
        let policy = self.effective_session_route_policy(session_id).await?;
        let (provider, generation) =
            Self::merge_session_route_policy(&policy, provider, generation);
        let (resolved_provider, resolved_generation) =
            self.resolve_generation_route(provider.as_deref(), generation)?;
        if let Some(route_id) = resolved_provider.as_deref() {
            let scope = self
                .load_session_credential_scope(session_id)
                .await?
                .normalized();
            anyhow::ensure!(
                !scope.constrains_routes() || scope.allows_route(route_id),
                "session {session_id} credential_scope does not allow route `{route_id}`"
            );
        }
        Ok((resolved_provider, resolved_generation))
    }

    pub(crate) async fn create_session(
        self: &Arc<Self>,
        request: CreateSessionRequest,
    ) -> Result<SessionView> {
        self.create_session_with_existing_policy(request, false)
            .await
    }

    pub(crate) async fn create_session_if_absent(
        self: &Arc<Self>,
        request: CreateSessionRequest,
    ) -> Result<SessionView> {
        self.create_session_with_existing_policy(request, true)
            .await
    }

    async fn create_session_with_existing_policy(
        self: &Arc<Self>,
        request: CreateSessionRequest,
        reject_existing: bool,
    ) -> Result<SessionView> {
        let _create_guard = self.session_service.session_control_lock().lock().await;
        let session_id = request
            .session_id
            .unwrap_or_else(|| self.session_service.next_session_id());
        validate_new_session_id(&session_id)?;
        if let Some(agent_id) = self.session_service.session_agent_id(&session_id).await {
            if reject_existing {
                bail!("session {session_id} already exists");
            }
            if let Some(requested_persona_id) = request.persona_id.as_deref() {
                let bound = self.load_session_persona_binding(&session_id).await?;
                anyhow::ensure!(
                    bound.as_ref().map(|binding| binding.persona_id.as_str())
                        == Some(requested_persona_id),
                    "session {session_id} is already bound to a different persona"
                );
            }
            if let Some(requested_scope) = request.capability_scope.as_ref() {
                let existing_scope = self.load_session_capability_scope(&session_id).await?;
                anyhow::ensure!(
                    existing_scope == requested_scope.normalized(),
                    "session {session_id} is already bound to a different capability scope"
                );
            }
            if let Some(requested_scope) = request.credential_scope.as_ref() {
                let existing_scope = self.load_session_credential_scope(&session_id).await?;
                anyhow::ensure!(
                    existing_scope == requested_scope.normalized(),
                    "session {session_id} is already bound to a different credential scope"
                );
            }
            debug!(
                session_id = %session_id,
                agent_id = %agent_id,
                "reusing existing session"
            );
            return self.session_view(&session_id, &AgentId(agent_id)).await;
        }
        let requested_capability_scope = request
            .capability_scope
            .clone()
            .unwrap_or_default()
            .normalized();
        let requested_credential_scope = request
            .credential_scope
            .clone()
            .unwrap_or_default()
            .normalized();
        let requested_persona = match request.persona_id.as_deref() {
            Some(persona_id) => Some(self.get_persona_record(persona_id).await?),
            None => None,
        };

        let snapshot = self
            .orchestrator
            .spawn_root(ConversationKey {
                session_id: session_id.clone(),
                thread_id: request.thread_id,
            })
            .await?;
        if let Some(persona) = requested_persona.as_ref()
            && let Err(error) = self
                .persist_session_persona_binding(&session_id, persona)
                .await
        {
            if let Err(cleanup_error) = self
                .rollback_created_session(&session_id, &snapshot.agent.id)
                .await
            {
                return Err(Self::combined_create_session_failure(error, cleanup_error));
            }
            return Err(error);
        }
        if !requested_capability_scope.is_empty()
            && let Err(error) = self
                .save_session_capability_scope(&session_id, requested_capability_scope)
                .await
        {
            if let Err(cleanup_error) = self
                .rollback_created_session(&session_id, &snapshot.agent.id)
                .await
            {
                return Err(Self::combined_create_session_failure(error, cleanup_error));
            }
            return Err(error);
        }
        if !requested_credential_scope.is_empty()
            && let Err(error) = self
                .save_session_credential_scope(&session_id, requested_credential_scope)
                .await
        {
            if let Err(cleanup_error) = self
                .rollback_created_session(&session_id, &snapshot.agent.id)
                .await
            {
                return Err(Self::combined_create_session_failure(error, cleanup_error));
            }
            return Err(error);
        }
        if let Err(error) = self
            .session_service
            .remember_session(&session_id, &snapshot.agent.id.0)
            .await
        {
            if let Err(cleanup_error) = self
                .rollback_created_session(&session_id, &snapshot.agent.id)
                .await
            {
                return Err(Self::combined_create_session_failure(error, cleanup_error));
            }
            return Err(error);
        }
        if let Err(error) = self.persist_topology().await {
            if let Err(cleanup_error) = self
                .rollback_created_session(&session_id, &snapshot.agent.id)
                .await
            {
                return Err(Self::combined_create_session_failure(error, cleanup_error));
            }
            return Err(error);
        }
        let view = self.session_view(&session_id, &snapshot.agent.id).await?;
        info!(
            session_id = %view.session_id,
            agent_id = %view.agent_id,
            thread_id = view.snapshot.agent.conversation.thread_id.as_deref(),
            "created daemon session"
        );
        self.publish_snapshot(&view);
        Ok(view)
    }

    async fn rollback_created_session(
        self: &Arc<Self>,
        session_id: &str,
        agent_id: &AgentId,
    ) -> Result<()> {
        let mut cleanup_error = self.session_service.forget_session(session_id).await.err();
        let remember_error = |slot: &mut Option<anyhow::Error>, error: anyhow::Error| {
            if slot.is_none() {
                *slot = Some(error);
            }
        };

        let _ = self.orchestrator.close_runtime(agent_id);
        let _ = self.supervisor.remove_agent(agent_id);

        if let Err(error) = self.session_service.delete_session_file(session_id) {
            remember_error(&mut cleanup_error, error);
        }

        if let Some(error) = cleanup_error {
            return Err(error);
        }
        Ok(())
    }

    pub(crate) async fn set_session_route_policy(
        &self,
        session_id: &str,
        policy: Option<SessionRoutePolicy>,
    ) -> Result<SessionView> {
        let normalized = if let Some(policy) = policy {
            if policy.is_empty() {
                SessionRoutePolicy::default()
            } else {
                let (provider, generation) = self
                    .resolve_generation_route(policy.provider.as_deref(), policy.generation)
                    .map(|(provider, generation)| (provider, generation))?;
                SessionRoutePolicy {
                    provider,
                    generation,
                }
            }
        } else {
            SessionRoutePolicy::default()
        };
        self.save_session_route_policy(session_id, normalized)
            .await?;
        let agent_id = self.agent_id_for_session(session_id).await?;
        let view = self.session_view(session_id, &agent_id).await?;
        self.publish_snapshot(&view);
        Ok(view)
    }

    pub(crate) async fn set_session_operator_config(
        &self,
        session_id: &str,
        config: Option<SessionOperatorConfig>,
    ) -> Result<SessionView> {
        self.run_service
            .with_session_idle_guard(session_id, || async {
                self.agent_id_for_session(session_id).await?;
                let normalized = crate::operator_contact::normalize_session_operator_config(
                    config.unwrap_or_default(),
                )?;
                if normalized.enabled
                    && normalized.allow_notify
                    && self
                        .operator_notification_reply_targets(session_id)
                        .await?
                        .is_empty()
                {
                    bail!(
                        "session operator config with notify_operator enabled requires at least one session reply target"
                    );
                }
                self.save_session_operator_config(session_id, normalized)
                    .await?;
                Ok(())
            })
            .await
            .map_err(|error| {
                if error.to_string().contains("has active or queued runs") {
                    anyhow::anyhow!(
                        "session {session_id} has non-terminal work or live descendants; operator config changes are only allowed while the session is idle"
                    )
                } else {
                    error
                }
            })?;
        let agent_id = self.agent_id_for_session(session_id).await?;
        let view = self.session_view(session_id, &agent_id).await?;
        self.publish_snapshot(&view);
        Ok(view)
    }

    /// Replaces the session's native tool surface overrides.
    ///
    /// Names are trimmed and deduplicated; a name in both lists is a caller
    /// bug and is rejected. Applied on the next run turn.
    pub(crate) async fn set_session_tool_overrides(
        &self,
        session_id: &str,
        overrides: Option<kheish_types::SessionToolOverrides>,
    ) -> Result<SessionView> {
        let mut normalized = overrides.unwrap_or_default();
        normalized.enable = normalize_tool_override_names(&normalized.enable);
        normalized.disable = normalize_tool_override_names(&normalized.disable);
        if let Some(conflict) = normalized
            .enable
            .iter()
            .find(|name| normalized.disable.contains(name))
        {
            bail!("tool `{conflict}` cannot be both enabled and disabled");
        }
        self.run_service
            .with_session_idle_guard(session_id, || async {
                self.agent_id_for_session(session_id).await?;
                self.save_session_tool_overrides(session_id, &normalized)
                    .await?;
                Ok(())
            })
            .await
            .map_err(|error| {
                if error.to_string().contains("has active or queued runs") {
                    anyhow::anyhow!(
                        "session {session_id} has non-terminal work or live descendants; tool-override changes are only allowed while the session is idle"
                    )
                } else {
                    error
                }
            })?;
        let agent_id = self.agent_id_for_session(session_id).await?;
        let view = self.session_view(session_id, &agent_id).await?;
        self.publish_snapshot(&view);
        Ok(view)
    }

    pub(crate) async fn set_session_capability_scope(
        &self,
        session_id: &str,
        scope: Option<CapabilityScope>,
    ) -> Result<SessionView> {
        if !self
            .session_is_idle_for_topology_mutation(session_id)
            .await?
        {
            bail!(
                "session {session_id} has non-terminal work or live descendants; capability-scope changes are only allowed while the session is idle"
            );
        }
        self.agent_id_for_session(session_id).await?;
        let normalized = scope.unwrap_or_default().normalized();
        self.save_session_capability_scope(session_id, normalized)
            .await?;
        let agent_id = self.agent_id_for_session(session_id).await?;
        let view = self.session_view(session_id, &agent_id).await?;
        self.publish_snapshot(&view);
        Ok(view)
    }

    pub(crate) async fn set_session_credential_scope(
        &self,
        session_id: &str,
        scope: Option<CredentialScope>,
    ) -> Result<SessionView> {
        if !self
            .session_is_idle_for_topology_mutation(session_id)
            .await?
        {
            bail!(
                "session {session_id} has non-terminal work or live descendants; credential-scope changes are only allowed while the session is idle"
            );
        }
        self.agent_id_for_session(session_id).await?;
        let normalized = scope.unwrap_or_default().normalized();
        self.save_session_credential_scope(session_id, normalized)
            .await?;
        self.clear_invalid_session_reply_targets_for_sessions(vec![session_id.to_string()])
            .await?;
        let agent_id = self.agent_id_for_session(session_id).await?;
        let view = self.session_view(session_id, &agent_id).await?;
        self.publish_snapshot(&view);
        Ok(view)
    }

    pub(crate) async fn set_session_reply_targets(
        &self,
        session_id: &str,
        reply_targets: Vec<ReplyHandle>,
    ) -> Result<SessionView> {
        self.run_service
            .with_session_idle_guard(session_id, || async {
                self.agent_id_for_session(session_id).await?;
                let normalized = normalize_reply_targets(None, reply_targets);
                let operator = self.load_session_operator_config(session_id).await?;
                if normalized.is_empty() && operator.enabled && operator.allow_notify {
                    bail!(
                        "cannot clear session reply targets while notify_operator is enabled for this session"
                    );
                }
                self.validate_session_reply_targets(session_id, &normalized)
                    .await?;
                self.session_service
                    .set_session_reply_targets(session_id, normalized)
                    .await?;
                Ok(())
            })
            .await
            .map_err(|error| {
                if error.to_string().contains("has active or queued runs") {
                    anyhow::anyhow!(
                        "session {session_id} has non-terminal work or live descendants; reply-target changes are only allowed while the session is idle"
                    )
                } else {
                    error
                }
            })?;
        let agent_id = self.agent_id_for_session(session_id).await?;
        let view = self.session_view(session_id, &agent_id).await?;
        self.publish_snapshot(&view);
        Ok(view)
    }

    pub(crate) async fn repair_session_reply_target_index(&self) -> Result<()> {
        self.session_service
            .repair_session_reply_target_index()
            .await?;
        let session_ids = self.session_service.cached_reply_target_session_ids().await;
        self.clear_invalid_session_reply_targets_for_sessions(session_ids)
            .await
    }

    pub(crate) async fn prune_session_reply_target_index(&self) -> Result<()> {
        self.session_service
            .prune_session_reply_target_index()
            .await?;
        let session_ids = self.session_service.cached_reply_target_session_ids().await;
        self.clear_invalid_session_reply_targets_for_sessions(session_ids)
            .await
    }

    pub(crate) async fn repair_session_reply_target_index_for_sessions<I>(
        &self,
        session_ids: I,
    ) -> Result<()>
    where
        I: IntoIterator<Item = String>,
    {
        let session_ids = session_ids.into_iter().collect::<Vec<_>>();
        self.session_service
            .repair_session_reply_target_index_for_sessions(session_ids.clone())
            .await?;
        self.clear_invalid_session_reply_targets_for_sessions(session_ids)
            .await
    }

    pub(crate) async fn clear_invalid_session_reply_targets_for_sessions<I>(
        &self,
        session_ids: I,
    ) -> Result<()>
    where
        I: IntoIterator<Item = String>,
    {
        for session_id in session_ids {
            let reply_targets = self
                .session_service
                .session_reply_targets(&session_id)
                .await;
            if reply_targets.is_empty() {
                continue;
            }
            if let Err(error) = self
                .validate_session_reply_targets(&session_id, &reply_targets)
                .await
            {
                warn!(
                    session_id = %session_id,
                    error = %error,
                    "clearing invalid session reply-target defaults"
                );
                self.session_service
                    .set_session_reply_targets(&session_id, Vec::new())
                    .await?;
                let operator = self.load_session_operator_config(&session_id).await?;
                if operator.enabled && operator.allow_notify {
                    let mut next = operator;
                    next.allow_notify = false;
                    if !next.allow_questions {
                        next.enabled = false;
                    }
                    self.save_session_operator_config(&session_id, next).await?;
                }
                if let Ok(agent_id) = self.agent_id_for_session(&session_id).await {
                    let view = self.session_view(&session_id, &agent_id).await?;
                    self.publish_snapshot(&view);
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn clear_invalid_session_reply_targets_referencing_connector(
        &self,
        kind: ConnectorKind,
        name: &str,
    ) -> Result<()> {
        let session_ids = self
            .session_service
            .cached_reply_target_session_ids_referencing_connector(kind, name)
            .await;
        self.clear_invalid_session_reply_targets_for_sessions(session_ids)
            .await
    }

    pub(crate) async fn reject_non_idle_reply_target_dependents(
        &self,
        kind: ConnectorKind,
        name: &str,
    ) -> Result<()> {
        let session_ids = self
            .session_service
            .cached_reply_target_session_ids_referencing_connector(kind, name)
            .await;
        let mut non_idle = Vec::new();
        for session_id in session_ids {
            if !self
                .session_is_idle_for_topology_mutation(&session_id)
                .await?
            {
                non_idle.push(session_id);
            }
        }
        if !non_idle.is_empty() {
            non_idle.sort();
            bail!(
                "connector {}/{} is referenced by reply targets on non-idle sessions {}; connector changes are only allowed after those sessions are idle",
                kind.as_str(),
                name,
                non_idle.join(", ")
            );
        }
        Ok(())
    }

    pub(super) async fn build_input_envelope(
        &self,
        session_id: &str,
        request: &SubmitInputRequest,
    ) -> Result<InputEnvelope> {
        self.build_input_envelope_with_board_access(session_id, session_id, request)
            .await
    }

    pub(super) async fn build_input_envelope_with_board_access(
        &self,
        session_id: &str,
        board_access_session_id: &str,
        request: &SubmitInputRequest,
    ) -> Result<InputEnvelope> {
        let completion_requirements = completion_requirements_for_request(request);
        let session_credential_scope = self.load_session_credential_scope(session_id).await?;
        let (preferred_provider, _) = self
            .resolve_generation_route_for_session(
                session_id,
                request.provider.clone(),
                request.generation.clone(),
            )
            .await?;
        let parts = self
            .ensure_audio_input_parts_canonical_text(
                self.resolve_request_input_parts_with_board_access(
                    board_access_session_id,
                    request,
                )
                .await?,
                preferred_provider.as_deref(),
                Some(&session_credential_scope),
            )
            .await?;
        let rendered_content = self.render_input_parts(&parts)?;
        let content_parts = parts
            .iter()
            .filter_map(ResolvedInputPart::content_part)
            .collect::<Vec<_>>();
        let attachments = parts
            .iter()
            .filter_map(ResolvedInputPart::attachment_ref)
            .collect::<Vec<_>>();
        let metadata = kheish_types::metadata_with_completion_requirements(
            Self::strip_daemon_owned_metadata(request.metadata.clone().unwrap_or(Value::Null)),
            &completion_requirements,
        )?;
        let learned_context = self
            .learned_context_bundle(session_id, Some(&rendered_content))
            .await?;
        let metadata =
            kheish_types::metadata_with_learned_context(metadata, learned_context.as_ref())?;
        let recovered_memory = self
            .recovered_memory_bundle(
                session_id,
                Some(&rendered_content),
                RecoveredMemoryBundleUsage::Prompt,
            )
            .await;
        let metadata = metadata_with_recovered_memory(metadata, recovered_memory.as_ref())?;
        let visible_skills = self
            .session_visible_skill_summaries(session_id, None)
            .await?
            .into_iter()
            .map(|skill| skill.name)
            .collect::<Vec<_>>();
        let metadata =
            kheish_types::metadata_with_session_visible_skills(metadata, &visible_skills)?;
        let reply_targets = self.envelope_reply_targets(session_id, request);
        Ok(InputEnvelope {
            source: SourceRef {
                plugin: request
                    .source_plugin
                    .clone()
                    .unwrap_or_else(|| "daemon".to_string()),
                kind: request
                    .source_kind
                    .clone()
                    .unwrap_or_else(|| "api".to_string()),
            },
            conversation: ConversationKey {
                session_id: session_id.to_string(),
                thread_id: None,
            },
            actor: ActorRef {
                id: request
                    .actor_id
                    .clone()
                    .unwrap_or_else(|| "api-user".to_string()),
                display_name: None,
            },
            payload: if !request.input_items.is_empty() || !request.attachments.is_empty() {
                InputPayload::Rich {
                    rendered_content,
                    items: content_parts,
                }
            } else {
                InputPayload::Text {
                    content: rendered_content,
                }
            },
            attachments,
            metadata,
            reply_targets: reply_targets.clone(),
            reply: reply_targets.first().cloned(),
        })
    }

    pub(super) fn apply_fallback_reply_targets(
        &self,
        request: &SubmitInputRequest,
        fallback_reply_targets: &[ReplyHandle],
    ) -> SubmitInputRequest {
        let mut request = request.clone();
        if request.reply_targets.is_empty()
            && request.reply_plugin.is_none()
            && request.reply_address.is_none()
            && !fallback_reply_targets.is_empty()
        {
            request.reply_targets = fallback_reply_targets.to_vec();
        }
        request
    }

    pub(super) async fn resolve_request_assets(
        &self,
        attachments: &[InputAttachmentRequest],
    ) -> Result<Vec<StoredAssetRecord>> {
        let assets = self.assets.clone();
        let requests = attachments.to_vec();
        tokio::task::spawn_blocking(move || {
            requests
                .iter()
                .map(|attachment| match attachment {
                    InputAttachmentRequest::AssetReference { asset_id } => {
                        let (asset, _) = assets.read_raw(asset_id)?;
                        let _ = assets.render_asset_transcript_part(&asset)?;
                        Ok(asset)
                    }
                    InputAttachmentRequest::InlineAsset(upload) => {
                        let asset = import_inline_asset(&assets, upload)?;
                        let _ = assets.render_asset_transcript_part(&asset)?;
                        Ok(asset)
                    }
                })
                .collect()
        })
        .await
        .context("attachment resolution task failed")?
    }

    pub(super) async fn resolve_request_input_items(
        &self,
        session_id: &str,
        items: &[SubmitInputItemRequest],
    ) -> Result<Vec<ResolvedInputPart>> {
        self.resolve_request_input_items_with_board_access(session_id, items)
            .await
    }

    async fn resolve_request_input_items_with_board_access(
        &self,
        board_access_session_id: &str,
        items: &[SubmitInputItemRequest],
    ) -> Result<Vec<ResolvedInputPart>> {
        let mut resolved = Vec::with_capacity(items.len());
        for item in items {
            match item {
                SubmitInputItemRequest::Text { text } => {
                    resolved.push(ResolvedInputPart::Text(text.clone()));
                }
                SubmitInputItemRequest::AssetReference { asset_id } => {
                    let asset = self
                        .assets
                        .get(asset_id)
                        .ok_or_else(|| anyhow!("unknown asset {asset_id}"))?;
                    resolved.push(ResolvedInputPart::Asset(asset));
                }
                SubmitInputItemRequest::BoardReference {
                    board_id,
                    revision_id,
                } => {
                    let asset = self
                        .resolve_board_reference_asset(
                            board_access_session_id,
                            board_id,
                            revision_id.as_deref(),
                        )
                        .await?;
                    resolved.push(ResolvedInputPart::Asset(asset));
                }
                SubmitInputItemRequest::InlineAsset(upload) => {
                    resolved.push(ResolvedInputPart::Asset(
                        self.import_asset_upload(upload).await?,
                    ));
                }
            }
        }
        Ok(resolved)
    }

    async fn resolve_board_reference_asset(
        &self,
        session_id: &str,
        board_id: &str,
        revision_id: Option<&str>,
    ) -> Result<StoredAssetRecord> {
        let board = self.board_service.get_board(board_id).await?;
        self.ensure_session_can_access_board(session_id, &board)
            .await?;
        let revision = if let Some(revision_id) = revision_id {
            self.board_service
                .get_revision(board_id, revision_id)
                .await?
        } else {
            let latest_revision_id = board
                .summary
                .latest_revision_id
                .ok_or_else(|| anyhow!("board {board_id} has no revisions"))?;
            self.board_service
                .get_revision(board_id, &latest_revision_id)
                .await?
        };
        match self.assets.read_raw(&revision.render_asset_id) {
            Ok((asset, _)) => Ok(asset),
            Err(error) if error.to_string().contains("unknown asset") => Err(anyhow!(
                "board revision {} references unknown asset {}",
                revision.revision_id,
                revision.render_asset_id
            )),
            Err(error) => Err(error),
        }
    }

    async fn ensure_session_can_access_board(
        &self,
        session_id: &str,
        board: &crate::boards::BoardView,
    ) -> Result<()> {
        let Some(owner_session_id) = board.summary.owner_session_id.as_deref() else {
            return Ok(());
        };
        if owner_session_id == session_id {
            return Ok(());
        }
        let agent_id = self.agent_id_for_session(session_id).await.map_err(|_| {
            anyhow!(
                "board {} is owned by session {owner_session_id}",
                board.summary.board_id
            )
        })?;
        let mut current = self.supervisor.get(&agent_id);
        while let Some(record) = current {
            if record.conversation.session_id == owner_session_id {
                return Ok(());
            }
            current = record
                .parent
                .as_ref()
                .and_then(|parent| self.supervisor.get(parent));
        }
        anyhow::bail!(
            "board {} is owned by session {owner_session_id}",
            board.summary.board_id
        );
    }

    async fn validate_request_input_items_for_session(
        &self,
        session_id: &str,
        items: &[SubmitInputItemRequest],
    ) -> Result<()> {
        for item in items {
            match item {
                SubmitInputItemRequest::Text { .. } | SubmitInputItemRequest::InlineAsset(_) => {}
                SubmitInputItemRequest::AssetReference { asset_id } => {
                    self.assets
                        .get(asset_id)
                        .ok_or_else(|| anyhow!("unknown asset {asset_id}"))?;
                }
                SubmitInputItemRequest::BoardReference {
                    board_id,
                    revision_id,
                } => {
                    self.resolve_board_reference_asset(
                        session_id,
                        board_id,
                        revision_id.as_deref(),
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    pub(super) async fn resolve_request_input_parts(
        &self,
        session_id: &str,
        request: &SubmitInputRequest,
    ) -> Result<Vec<ResolvedInputPart>> {
        self.resolve_request_input_parts_with_board_access(session_id, request)
            .await
    }

    async fn resolve_request_input_parts_with_board_access(
        &self,
        board_access_session_id: &str,
        request: &SubmitInputRequest,
    ) -> Result<Vec<ResolvedInputPart>> {
        if !request.input_items.is_empty() {
            return self
                .resolve_request_input_items_with_board_access(
                    board_access_session_id,
                    &request.input_items,
                )
                .await;
        }
        let mut parts = Vec::new();
        if !request.content.trim().is_empty() {
            parts.push(ResolvedInputPart::Text(request.content.clone()));
        }
        parts.extend(
            self.resolve_request_assets(&request.attachments)
                .await?
                .into_iter()
                .map(ResolvedInputPart::Asset),
        );
        Ok(parts)
    }

    pub(super) async fn normalize_submit_input_request(
        &self,
        session_id: &str,
        request: &mut SubmitInputRequest,
    ) -> Result<()> {
        self.normalize_submit_input_request_with_options(session_id, request, false)
            .await
    }

    pub(super) async fn normalize_submit_input_request_for_schedule(
        &self,
        session_id: &str,
        request: &mut SubmitInputRequest,
    ) -> Result<()> {
        self.normalize_submit_input_request_with_options(session_id, request, true)
            .await
    }

    async fn normalize_submit_input_request_with_options(
        &self,
        session_id: &str,
        request: &mut SubmitInputRequest,
        preserve_board_references: bool,
    ) -> Result<()> {
        self.validate_submit_input_request(session_id, request)
            .await?;
        if !request.input_items.is_empty() {
            if preserve_board_references {
                self.validate_request_input_items_for_session(session_id, &request.input_items)
                    .await?;
                let mut normalized = Vec::with_capacity(request.input_items.len());
                for item in request.input_items.clone() {
                    match item {
                        SubmitInputItemRequest::Text { .. }
                        | SubmitInputItemRequest::AssetReference { .. }
                        | SubmitInputItemRequest::BoardReference { .. } => normalized.push(item),
                        SubmitInputItemRequest::InlineAsset(upload) => {
                            let asset = self.import_asset_upload(&upload).await?;
                            normalized.push(SubmitInputItemRequest::AssetReference {
                                asset_id: asset.id,
                            });
                        }
                    }
                }
                request.input_items = normalized;
            } else {
                let parts = self
                    .resolve_request_input_items(session_id, &request.input_items)
                    .await?;
                request.input_items = normalized_submit_input_items(&parts);
            }
            request.attachments.clear();
            request.content.clear();
            return Ok(());
        }
        let assets = self.resolve_request_assets(&request.attachments).await?;
        request.attachments = assets
            .into_iter()
            .map(|asset| InputAttachmentRequest::AssetReference { asset_id: asset.id })
            .collect();
        Ok(())
    }

    /// Resolves the normalized attachment references represented by one input request.
    pub(super) async fn input_attachment_refs_for_request(
        &self,
        session_id: &str,
        request: &SubmitInputRequest,
    ) -> Result<Vec<AttachmentRef>> {
        let parts = self
            .resolve_request_input_parts(session_id, request)
            .await?;
        Ok(parts
            .iter()
            .filter_map(ResolvedInputPart::attachment_ref)
            .collect())
    }

    pub(super) async fn input_attachment_refs_for_mailbox_messages(
        &self,
        session_id: &str,
        messages: &[MailboxMessage],
    ) -> Result<Vec<AttachmentRef>> {
        let mut attachments = Vec::new();
        for message in messages {
            let Some(items_value) = message.payload.get("input_items").cloned() else {
                continue;
            };
            let input_items = serde_json::from_value::<Vec<SubmitInputItemRequest>>(items_value)
                .context("mailbox payload input_items must be an array")?;
            validate_submit_input_items(&input_items)?;
            attachments.extend(
                self.resolve_request_input_items(session_id, &input_items)
                    .await?
                    .iter()
                    .filter_map(ResolvedInputPart::attachment_ref),
            );
        }
        Ok(attachments)
    }

    pub(super) async fn normalize_mailbox_payload(
        &self,
        session_id: &str,
        payload: &mut Value,
    ) -> Result<()> {
        let Some(map) = payload.as_object_mut() else {
            return Ok(());
        };
        if map.contains_key("input_items") && map.contains_key("asset_ids") {
            anyhow::bail!("asset_ids cannot be combined with input_items");
        }
        if let Some(asset_ids_value) = map.get("asset_ids").cloned() {
            let asset_ids = serde_json::from_value::<Vec<String>>(asset_ids_value)
                .context("mailbox payload asset_ids must be an array")?;
            for asset_id in &asset_ids {
                if asset_id.trim().is_empty() {
                    anyhow::bail!("asset_ids entries must not be empty");
                }
            }
            if !asset_ids.is_empty() {
                let mut input_items = Vec::with_capacity(asset_ids.len().saturating_add(1));
                if let Some(message) = map
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|message| !message.is_empty())
                {
                    input_items.push(SubmitInputItemRequest::Text {
                        text: message.to_string(),
                    });
                    map.remove("message");
                }
                input_items.extend(
                    asset_ids
                        .into_iter()
                        .map(|asset_id| SubmitInputItemRequest::AssetReference { asset_id }),
                );
                map.insert(
                    "input_items".to_string(),
                    serde_json::to_value(input_items)?,
                );
            }
            map.remove("asset_ids");
        }
        let Some(items_value) = map.get("input_items").cloned() else {
            return Ok(());
        };
        let input_items = serde_json::from_value::<Vec<SubmitInputItemRequest>>(items_value)
            .context("mailbox payload input_items must be an array")?;
        validate_submit_input_items(&input_items)?;
        let parts = self
            .resolve_request_input_items(session_id, &input_items)
            .await?;
        let normalized = normalized_submit_input_items(&parts);
        if normalized.is_empty() {
            map.remove("input_items");
        } else {
            map.insert("input_items".to_string(), serde_json::to_value(normalized)?);
        }
        Ok(())
    }

    pub(super) async fn import_asset_upload(
        &self,
        upload: &InlineAssetUpload,
    ) -> Result<StoredAssetRecord> {
        let assets = self.assets.clone();
        let upload = upload.clone();
        tokio::task::spawn_blocking(move || import_inline_asset(&assets, &upload))
            .await
            .context("asset import task failed")?
    }

    pub(super) fn render_input_parts(&self, parts: &[ResolvedInputPart]) -> Result<String> {
        let mut fragments = Vec::new();
        for part in parts {
            match part {
                ResolvedInputPart::Text(text) => {
                    if !text.trim().is_empty() {
                        fragments.push(text.trim().to_string());
                    }
                }
                ResolvedInputPart::Asset(asset) => {
                    let rendered = self.assets.render_asset_transcript_part(asset)?;
                    if !rendered.is_empty() {
                        fragments.push(rendered);
                    }
                }
            }
        }
        Ok(fragments.join("\n\n").trim().to_string())
    }

    pub(super) fn envelope_reply_targets(
        &self,
        session_id: &str,
        request: &SubmitInputRequest,
    ) -> Vec<ReplyHandle> {
        let daemon_target = ReplyHandle {
            plugin: "daemon".to_string(),
            address: session_id.to_string(),
        };
        let explicit_reply = self.explicit_reply_handle(session_id, request);
        let mut targets = if request.reply_targets.is_empty() {
            explicit_reply.clone().into_iter().collect::<Vec<_>>()
        } else {
            request.reply_targets.clone()
        };
        targets.push(daemon_target);
        normalize_reply_targets(explicit_reply, targets)
    }

    pub(super) fn explicit_reply_handle(
        &self,
        session_id: &str,
        request: &SubmitInputRequest,
    ) -> Option<ReplyHandle> {
        match (&request.reply_plugin, &request.reply_address) {
            (Some(plugin), Some(address)) => Some(ReplyHandle {
                plugin: plugin.clone(),
                address: address.clone(),
            }),
            (Some(plugin), None) if plugin == "daemon" => Some(ReplyHandle {
                plugin: plugin.clone(),
                address: session_id.to_string(),
            }),
            (Some(_), None) => None,
            (None, Some(address)) => Some(ReplyHandle {
                plugin: "daemon".to_string(),
                address: address.clone(),
            }),
            (None, None) => None,
        }
    }

    pub(super) fn validate_reply_targets(&self, request: &SubmitInputRequest) -> Result<()> {
        let available_plugins = self.delivery_service.available_reply_plugins();
        if request.reply_plugin.is_some()
            && request.reply_address.is_none()
            && request.reply_plugin.as_deref() != Some("daemon")
        {
            bail!("reply_address is required when reply_plugin is not daemon");
        }
        if let Some(plugin) = request.reply_plugin.as_deref()
            && !available_plugins.contains(plugin)
        {
            bail!("unknown reply plugin {plugin}");
        }
        for target in &request.reply_targets {
            if !available_plugins.contains(&target.plugin) {
                bail!("unknown reply plugin {}", target.plugin);
            }
        }
        if !request.input_items.is_empty()
            && (!request.content.trim().is_empty() || !request.attachments.is_empty())
        {
            bail!("input_items cannot be combined with legacy content or attachments fields");
        }
        validate_input_attachment_requests(&request.attachments)?;
        validate_submit_input_items(&request.input_items)?;
        Ok(())
    }

    pub(super) fn ensure_submit_input_request_has_payload(
        &self,
        request: &SubmitInputRequest,
    ) -> Result<()> {
        let has_legacy_text = !request.content.trim().is_empty();
        let has_legacy_attachment = !request.attachments.is_empty();
        let has_ordered_item = request.input_items.iter().any(|item| match item {
            SubmitInputItemRequest::Text { text } => !text.trim().is_empty(),
            SubmitInputItemRequest::AssetReference { .. }
            | SubmitInputItemRequest::BoardReference { .. }
            | SubmitInputItemRequest::InlineAsset(_) => true,
        });
        if has_legacy_text || has_legacy_attachment || has_ordered_item {
            return Ok(());
        }
        bail!("content or attachments or input_items is required");
    }

    pub(super) async fn validate_submit_input_request(
        &self,
        session_id: &str,
        request: &SubmitInputRequest,
    ) -> Result<()> {
        self.validate_reply_targets(request)?;
        let explicit = self.explicit_input_reply_targets(session_id, request);
        self.validate_session_reply_targets(session_id, &explicit)
            .await
    }

    pub(crate) fn validate_persisted_reply_targets(
        &self,
        reply_targets: &[ReplyHandle],
    ) -> Result<()> {
        let available_plugins = self.delivery_service.available_reply_plugins();
        for target in reply_targets {
            if !available_plugins.contains(&target.plugin) {
                bail!("unknown reply plugin {}", target.plugin);
            }
            match target.plugin.as_str() {
                "external" => {
                    let route = crate::connectors::decode_external_reply_route(&target.address)?;
                    if self.connectors().external(&route.connector).is_none() {
                        bail!(
                            "reply target references unknown connector external/{}",
                            route.connector
                        );
                    }
                }
                "telegram" => {
                    let route = crate::connectors::decode_telegram_reply_route(&target.address)?;
                    let connector =
                        self.connectors()
                            .telegram(&route.connector)
                            .ok_or_else(|| {
                                anyhow!(
                                    "reply target references unknown connector telegram/{}",
                                    route.connector
                                )
                            })?;
                    if connector.bot_token.is_none() {
                        bail!(
                            "telegram connector {} has no bot token configured for replies",
                            route.connector
                        );
                    }
                    if !connector.allows_chat_id(route.chat_id) {
                        bail!(
                            "telegram reply target chat {} is outside connector {} allowlist",
                            route.chat_id,
                            route.connector
                        );
                    }
                }
                "slack" => {
                    let route = crate::connectors::decode_slack_reply_route(&target.address)?;
                    let connector = self.connectors().slack(&route.connector).ok_or_else(|| {
                        anyhow!(
                            "reply target references unknown connector slack/{}",
                            route.connector
                        )
                    })?;
                    if connector
                        .bot_token_for_team(route.team_id.as_deref())
                        .is_none()
                    {
                        bail!(
                            "slack connector {} has no bot token configured for replies",
                            route.connector
                        );
                    }
                    if !connector.is_enterprise_allowed(route.enterprise_id.as_deref()) {
                        bail!(
                            "slack reply target enterprise {:?} is outside connector {} allowlist",
                            route.enterprise_id,
                            route.connector
                        );
                    }
                    if !connector.is_team_allowed(route.team_id.as_deref()) {
                        bail!(
                            "slack reply target team {:?} is outside connector {} allowlist",
                            route.team_id,
                            route.connector
                        );
                    }
                    if !connector.is_channel_allowed(&route.channel_id) {
                        bail!(
                            "slack reply target channel {} is outside connector {} allowlist",
                            route.channel_id,
                            route.connector
                        );
                    }
                }
                "http" => {
                    crate::connectors::decode_http_reply_route(&target.address)?;
                }
                "daemon" => {
                    if target.address.trim().is_empty() {
                        bail!("daemon reply targets require a non-empty session address");
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(super) async fn validate_session_reply_targets(
        &self,
        session_id: &str,
        reply_targets: &[ReplyHandle],
    ) -> Result<()> {
        self.validate_persisted_reply_targets(reply_targets)?;
        let scope = self
            .load_session_credential_scope(session_id)
            .await?
            .normalized();
        if scope.is_empty() {
            return Ok(());
        }
        for target in reply_targets {
            match target.plugin.as_str() {
                "external" => {
                    let route = crate::connectors::decode_external_reply_route(&target.address)?;
                    let connector = self
                        .connectors()
                        .external(&route.connector)
                        .ok_or_else(|| anyhow!("unknown external connector {}", route.connector))?;
                    if !scope.allows_connector(&route.connector) {
                        bail!("credential scope blocks connector {}", route.connector);
                    }
                    if connector.shared_token.is_some()
                        && !scope.allows_connector_credential(&route.connector, "shared_token")
                    {
                        bail!(
                            "credential scope blocks connector credential {}:shared_token",
                            route.connector
                        );
                    }
                    if let Some(child) = connector.child_process.as_ref() {
                        for env_key in child.credential_slots.keys() {
                            if !scope.allows_connector_credential(&route.connector, env_key) {
                                bail!(
                                    "credential scope blocks connector credential {}:{}",
                                    route.connector,
                                    env_key
                                );
                            }
                        }
                    }
                }
                "telegram" => {
                    let route = crate::connectors::decode_telegram_reply_route(&target.address)?;
                    let connector = self
                        .connectors()
                        .telegram(&route.connector)
                        .ok_or_else(|| anyhow!("unknown telegram connector {}", route.connector))?;
                    if !scope.allows_connector(&route.connector) {
                        bail!("credential scope blocks connector {}", route.connector);
                    }
                    if connector.bot_token.is_some()
                        && !scope.allows_connector_credential(&route.connector, "bot_token")
                    {
                        bail!(
                            "credential scope blocks connector credential {}:bot_token",
                            route.connector
                        );
                    }
                }
                "slack" => {
                    let route = crate::connectors::decode_slack_reply_route(&target.address)?;
                    let connector = self
                        .connectors()
                        .slack(&route.connector)
                        .ok_or_else(|| anyhow!("unknown slack connector {}", route.connector))?;
                    if !scope.allows_connector(&route.connector) {
                        bail!("credential scope blocks connector {}", route.connector);
                    }
                    if connector
                        .bot_token_for_team(route.team_id.as_deref())
                        .is_some()
                        && !scope.allows_connector_credential(&route.connector, "bot_token")
                    {
                        bail!(
                            "credential scope blocks connector credential {}:bot_token",
                            route.connector
                        );
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(super) fn explicit_input_reply_targets(
        &self,
        session_id: &str,
        request: &SubmitInputRequest,
    ) -> Vec<ReplyHandle> {
        normalize_reply_targets(
            self.explicit_reply_handle(session_id, request),
            request.reply_targets.clone(),
        )
    }

    pub(crate) async fn session_reply_targets(&self, session_id: &str) -> Vec<ReplyHandle> {
        self.session_service.session_reply_targets(session_id).await
    }

    pub(super) async fn operator_notification_reply_targets(
        &self,
        session_id: &str,
    ) -> Result<Vec<ReplyHandle>> {
        let targets = normalize_reply_targets(None, self.session_reply_targets(session_id).await);
        self.validate_session_reply_targets(session_id, &targets)
            .await?;
        Ok(targets)
    }

    pub(super) async fn resolve_run_reply_targets(
        &self,
        session_id: &str,
        request: &SubmitInputRequest,
    ) -> Result<Vec<ReplyHandle>> {
        let explicit = self.explicit_input_reply_targets(session_id, request);
        if !explicit.is_empty() {
            self.validate_session_reply_targets(session_id, &explicit)
                .await?;
            return Ok(explicit);
        }
        let session_targets = self.session_reply_targets(session_id).await;
        self.validate_session_reply_targets(session_id, &session_targets)
            .await?;
        Ok(session_targets)
    }

    pub(super) async fn run_reply_targets(&self, run_id: &str) -> Vec<ReplyHandle> {
        self.run_service.run_reply_targets(run_id).await
    }

    pub(crate) async fn bound_session_id(&self, binding_keys: &[String]) -> Result<Option<String>> {
        self.session_service.bound_session_id(binding_keys).await
    }

    pub(crate) async fn connector_next_update_id(&self, key: &str) -> Option<i64> {
        self.connector_ingress_service
            .connector_next_update_id_from_legacy_key(key)
            .await
    }

    pub(crate) async fn remember_connector_next_update_id(
        &self,
        key: &str,
        next_update_id: i64,
    ) -> Result<()> {
        self.connector_ingress_service
            .remember_connector_next_update_id_from_legacy_key(key, next_update_id)
            .await
    }

    pub(crate) async fn begin_connector_ingress(
        &self,
        key: &str,
    ) -> Result<ConnectorIngressReservation> {
        self.connector_ingress_service
            .begin_connector_ingress_from_legacy_key(key)
            .await
    }

    pub(crate) async fn begin_connector_ingress_with_fingerprint(
        &self,
        key: &str,
        fingerprint: &str,
    ) -> Result<ConnectorIngressReservation> {
        self.connector_ingress_service
            .begin_connector_ingress_with_fingerprint_from_legacy_key(key, fingerprint)
            .await
    }

    pub(crate) async fn lookup_connector_ingress(
        &self,
        key: &str,
    ) -> Result<ConnectorIngressLookup> {
        self.connector_ingress_service
            .lookup_connector_ingress_from_legacy_key(key)
            .await
    }

    pub(crate) async fn remember_connector_ingress_run(
        &self,
        key: &str,
        run_id: &str,
    ) -> Result<()> {
        self.connector_ingress_service
            .remember_connector_ingress_run_from_legacy_key(key, run_id)
            .await
    }

    pub(crate) async fn forget_connector_ingress(&self, key: &str) -> Result<()> {
        self.connector_ingress_service
            .forget_connector_ingress_from_legacy_key(key)
            .await
    }

    pub(crate) async fn begin_observation_ingress(
        &self,
        key: &str,
        request_fingerprint: &str,
    ) -> Result<ObservationIngressReservation> {
        self.session_service
            .begin_observation_ingress(key, request_fingerprint)
            .await
    }

    pub(crate) async fn remember_observation_ingress(
        &self,
        key: &str,
        observation_id: &str,
        request_fingerprint: &str,
    ) -> Result<()> {
        self.session_service
            .remember_observation_ingress(key, observation_id, request_fingerprint)
            .await
    }

    pub(crate) async fn forget_observation_ingress(&self, key: &str) -> Result<()> {
        self.session_service.forget_observation_ingress(key).await
    }

    pub(super) async fn remember_session_reply_targets(
        &self,
        session_id: &str,
        reply_targets: Vec<ReplyHandle>,
    ) -> Result<()> {
        self.session_service
            .remember_session_reply_targets(session_id, reply_targets)
            .await
    }

    pub(super) async fn remember_session_bindings(
        &self,
        session_id: &str,
        binding_keys: Vec<String>,
    ) -> Result<()> {
        self.session_service
            .remember_session_bindings(session_id, binding_keys)
            .await
    }

    async fn build_multimodal_mailbox_parts(
        &self,
        session_id: &str,
        messages: &[MailboxMessage],
    ) -> Result<Option<Vec<ResolvedInputPart>>> {
        let has_multimodal_payload = messages
            .iter()
            .any(|message| message.payload.get("input_items").is_some());
        if !has_multimodal_payload {
            return Ok(None);
        }

        let mut parts = Vec::new();
        parts.push(ResolvedInputPart::Text(
            "You received mailbox messages from other agents. Treat them as new work items and act on them using the available tools when appropriate."
                .to_string(),
        ));

        for (index, message) in messages.iter().enumerate() {
            if let Some(items_value) = message.payload.get("input_items") {
                let input_items =
                    serde_json::from_value::<Vec<SubmitInputItemRequest>>(items_value.clone())
                        .context("mailbox payload input_items must be an array")?;
                validate_submit_input_items(&input_items)?;
                parts.push(ResolvedInputPart::Text(render_mailbox_message_context(
                    index, message,
                )));
                parts.extend(
                    self.resolve_request_input_items(session_id, &input_items)
                        .await?,
                );
            } else {
                parts.push(ResolvedInputPart::Text(render_mailbox_message(
                    index, message,
                )));
            }
        }

        Ok(Some(parts))
    }

    pub(super) async fn build_mailbox_input_envelope(
        &self,
        session_id: &str,
        recipient: &AgentId,
        messages: &[MailboxMessage],
    ) -> Result<InputEnvelope> {
        let content = render_mailbox_messages(messages);
        let metadata = serde_json::json!({
            "mailbox_messages": messages,
        });
        let reply = ReplyHandle {
            plugin: "daemon".to_string(),
            address: session_id.to_string(),
        };
        let multimodal_parts = self
            .build_multimodal_mailbox_parts(session_id, messages)
            .await?;
        let (payload, attachments) = if let Some(parts) = multimodal_parts {
            let rendered_content = self.render_input_parts(&parts)?;
            (
                InputPayload::Rich {
                    rendered_content,
                    items: parts
                        .iter()
                        .filter_map(ResolvedInputPart::content_part)
                        .collect(),
                },
                parts
                    .iter()
                    .filter_map(ResolvedInputPart::attachment_ref)
                    .collect(),
            )
        } else {
            (InputPayload::Text { content }, Vec::new())
        };
        Ok(InputEnvelope {
            source: SourceRef {
                plugin: "daemon".to_string(),
                kind: "mailbox".to_string(),
            },
            conversation: ConversationKey {
                session_id: session_id.to_string(),
                thread_id: None,
            },
            actor: ActorRef {
                id: recipient.0.clone(),
                display_name: None,
            },
            payload,
            attachments,
            metadata,
            reply_targets: vec![reply.clone()],
            reply: Some(reply),
        })
    }
}

fn validate_new_session_id(session_id: &str) -> Result<()> {
    if session_id.trim().is_empty() {
        return Err(DaemonProblem::bad_request(
            "sessions",
            "invalid_session_id",
            "session_id is required",
        )
        .into());
    }
    if matches!(session_id, "." | "..") {
        return Err(DaemonProblem::bad_request(
            "sessions",
            "invalid_session_id",
            "session_id cannot be '.' or '..' because URI clients normalize dot path segments",
        )
        .into());
    }
    Ok(())
}

fn render_mailbox_message_context(index: usize, message: &MailboxMessage) -> String {
    let mut payload = message.payload.clone();
    if let Value::Object(map) = &mut payload {
        map.remove("input_items");
        map.remove("type");
        if matches!(map.get("message"), Some(Value::String(text)) if text.trim().is_empty()) {
            map.remove("message");
        }
    }
    let mut lines = vec![
        format!("[Mailbox Message {}]", index + 1),
        format!("Id: {}", message.id),
        format!("Schema-Version: {}", message.schema_version),
        format!("State: {:?}", message.state),
        format!("From: {}", message.from.0),
        format!("Subject: {}", message.subject),
        format!("Type: {}", mailbox_message_type(message)),
    ];
    match payload {
        Value::Null => {}
        Value::Object(ref map) if map.is_empty() => {}
        other => {
            let rendered =
                serde_json::to_string_pretty(&other).unwrap_or_else(|_| other.to_string());
            lines.push("Payload metadata:".to_string());
            lines.push(rendered);
        }
    }
    lines.join("\n")
}

fn normalize_tool_override_names(values: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() || normalized.iter().any(|entry| entry == trimmed) {
            continue;
        }
        normalized.push(trimmed.to_string());
    }
    normalized
}
