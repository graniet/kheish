//! Channel lifecycle methods implemented on [`DaemonState`].

use super::*;

/// Per-channel bookkeeping the autonomous heartbeat worker keeps in memory so it can pace
/// itself, notice when the room falls silent, rotate topics and speakers fairly, and decide
/// when to open a fresh topic instead of poking a thread that has already run its course.
#[derive(Debug, Default, Clone)]
struct ChannelHeartbeatState {
    /// True while a granted autonomous turn is still awaiting its outcome, so the next idle
    /// poll knows to score it (spoke vs. abstained) before granting again.
    pending_grant: bool,
    /// The channel's global latest message id at the previous scored poll. If it has not
    /// advanced by the next poll, nothing moved anywhere — the granted agent stayed quiet and
    /// no one else spoke — which is what counts as a silent turn.
    last_seen_message_id: Option<String>,
    /// How many granted turns in a row left the whole room silent. At
    /// `CHANNEL_HEARTBEAT_SILENCE_LIMIT` the current topic is treated as finished; past
    /// `CHANNEL_HEARTBEAT_DORMANCY_LIMIT` the channel goes dormant until someone speaks again.
    consecutive_silent: u32,
    /// When we last granted an autonomous turn, so silence can never trigger back-to-back
    /// turns faster than the heartbeat interval.
    last_turn_at_ms: u64,
    /// Monotonic per-channel turn phase used to space brand-new topics deterministically
    /// (no RNG) so the main feed keeps gaining subjects without every turn opening one.
    turn_counter: u64,
    /// Per-session timestamp of the last turn each member was granted, so rotation stays fair
    /// even when a member abstains — an abstention leaves no message to key on, but the grant
    /// still advances this, rotating the next turn to someone else.
    last_granted: std::collections::HashMap<String, u64>,
}

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    const CHANNEL_LEASE_ERROR_RETRY_BACKOFF_MS: u64 = 500;
    const CHANNEL_STIMULUS_ERROR_RETRY_BACKOFF_MS: u64 = 500;

    pub(crate) async fn list_channels(
        &self,
        query: Option<&str>,
    ) -> Result<Vec<crate::ChannelView>> {
        Ok(self.channel_service.list_channels(query).await)
    }

    pub(crate) async fn get_channel(&self, channel_id: &str) -> Result<crate::ChannelView> {
        self.channel_service.get_channel(channel_id).await
    }

    pub(crate) async fn create_channel(
        &self,
        request: crate::CreateChannelRequest,
    ) -> Result<crate::ChannelView> {
        anyhow::ensure!(!request.title.trim().is_empty(), "title is required");
        self.ensure_assets_exist(&request.pinned_asset_ids)?;
        let now = now_ms();
        let default_participation_mode = request.default_participation_mode.unwrap_or_default();
        let autonomy_policy = request.autonomy_policy.unwrap_or_default();
        validate_channel_autonomy_policy(&autonomy_policy)?;
        let mut members = Vec::with_capacity(request.members.len());
        for member in request.members {
            members.push(
                self.channel_member_from_request(&member, default_participation_mode.clone(), now)
                    .await?,
            );
        }
        self.channel_service
            .create_channel(CreateChannelRecord {
                channel_id: request
                    .channel_id
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| self.channel_service.next_channel_id()),
                title: request.title.trim().to_string(),
                description: trim_optional_string(request.description),
                purpose: trim_optional_string(request.purpose),
                pinned_asset_ids: request.pinned_asset_ids,
                created_by: request
                    .created_by
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "system".to_string()),
                created_at_ms: now,
                members,
                autonomy_policy,
                default_participation_mode,
                metadata: request.metadata,
            })
            .await
    }

    pub(crate) async fn update_channel(
        &self,
        channel_id: &str,
        request: crate::UpdateChannelRequest,
    ) -> Result<crate::ChannelView> {
        if let Some(title) = request.title.as_deref() {
            anyhow::ensure!(!title.trim().is_empty(), "title is required");
        }
        if let Some(pinned_asset_ids) = request.pinned_asset_ids.as_ref() {
            self.ensure_assets_exist(pinned_asset_ids)?;
        }
        let title = request.title.map(|value| value.trim().to_string());
        let description = request
            .description
            .map(|value| trim_optional_string(Some(value)));
        let purpose = request
            .purpose
            .map(|value| trim_optional_string(Some(value)));
        let pinned_asset_ids = request.pinned_asset_ids;
        let autonomy_policy = request.autonomy_policy;
        if let Some(autonomy_policy) = autonomy_policy.as_ref() {
            validate_channel_autonomy_policy(autonomy_policy)?;
        }
        let default_participation_mode = request.default_participation_mode;
        let paused = request.paused;
        let metadata = request.metadata;
        self.channel_service
            .update_channel(channel_id, move |channel| {
                let mut changed = false;
                if let Some(title) = title.as_deref()
                    && channel.summary.title != title
                {
                    channel.summary.title = title.to_string();
                    changed = true;
                }
                if let Some(description) = description.as_ref()
                    && channel.summary.description != *description
                {
                    channel.summary.description = description.clone();
                    changed = true;
                }
                if let Some(purpose) = purpose.as_ref()
                    && channel.summary.purpose != *purpose
                {
                    channel.summary.purpose = purpose.clone();
                    changed = true;
                }
                if let Some(pinned_asset_ids) = pinned_asset_ids.as_ref()
                    && channel.pinned_asset_ids != *pinned_asset_ids
                {
                    channel.pinned_asset_ids = pinned_asset_ids.clone();
                    changed = true;
                }
                if let Some(autonomy_policy) = autonomy_policy.as_ref()
                    && channel.autonomy_policy != *autonomy_policy
                {
                    channel.autonomy_policy = autonomy_policy.clone();
                    changed = true;
                }
                if let Some(default_participation_mode) = default_participation_mode.as_ref()
                    && channel.default_participation_mode != *default_participation_mode
                {
                    channel.default_participation_mode = default_participation_mode.clone();
                    changed = true;
                }
                if let Some(paused) = paused
                    && channel.summary.paused != paused
                {
                    channel.summary.paused = paused;
                    changed = true;
                }
                if let Some(metadata) = metadata.as_ref()
                    && channel.metadata != *metadata
                {
                    channel.metadata = metadata.clone();
                    changed = true;
                }
                if changed {
                    channel.summary.updated_at_ms = now_ms();
                }
                Ok(changed)
            })
            .await
    }

    pub(crate) async fn delete_channel(&self, channel_id: &str) -> Result<()> {
        self.reject_channel_project_dependencies(channel_id).await?;
        self.channel_service.delete_channel(channel_id).await
    }

    pub(crate) async fn list_channel_messages(
        &self,
        channel_id: &str,
        thread_root_message_id: Option<&str>,
        query: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<crate::ChannelMessageView>> {
        let mut messages = self
            .channel_service
            .list_messages(channel_id, thread_root_message_id)
            .await?;
        if let Some(query) = query.map(str::trim).filter(|query| !query.is_empty()) {
            let query = query.to_lowercase();
            messages.retain(|message| {
                message.sender.id.to_lowercase().contains(&query)
                    || message
                        .sender
                        .display_name
                        .as_deref()
                        .map(str::to_lowercase)
                        .is_some_and(|display_name| display_name.contains(&query))
                    || message.output.content.to_lowercase().contains(&query)
                    || message.output.parts.iter().any(|part| match part {
                        kheish_types::ContentPart::Text { text } => {
                            text.to_lowercase().contains(&query)
                        }
                        kheish_types::ContentPart::Attachment { attachment } => attachment
                            .file_name
                            .as_deref()
                            .map(str::to_lowercase)
                            .is_some_and(|file_name| file_name.contains(&query)),
                    })
            });
        }
        if let Some(limit) = limit
            && messages.len() > limit
        {
            messages = messages[messages.len().saturating_sub(limit)..].to_vec();
        }
        Ok(messages)
    }

    pub(crate) async fn list_channel_leases(
        &self,
        channel_id: &str,
    ) -> Result<Vec<crate::ChannelTurnLeaseView>> {
        self.channel_service.list_leases(channel_id).await
    }

    pub(crate) async fn list_channel_stimuli(
        &self,
        channel_id: &str,
        thread_root_message_id: Option<&str>,
        state: Option<crate::ChannelStimulusState>,
        limit: Option<usize>,
    ) -> Result<Vec<crate::ChannelStimulusView>> {
        let mut stimuli = self
            .channel_service
            .list_stimuli(channel_id, thread_root_message_id, state)
            .await?;
        if let Some(limit) = limit
            && stimuli.len() > limit
        {
            stimuli = stimuli[stimuli.len().saturating_sub(limit)..].to_vec();
        }
        Ok(stimuli)
    }

    pub(crate) async fn create_channel_stimulus(
        &self,
        channel_id: &str,
        request: crate::CreateChannelStimulusRequest,
    ) -> Result<crate::ChannelStimulusView> {
        anyhow::ensure!(!request.content.trim().is_empty(), "content is required");
        let has_thread_root = request
            .thread_root_message_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty());
        if matches!(request.scope, crate::ChannelStimulusScope::Thread) {
            anyhow::ensure!(
                has_thread_root,
                "thread_root_message_id is required for thread-scoped stimuli"
            );
        } else {
            anyhow::ensure!(
                !has_thread_root,
                "thread_root_message_id is only valid for thread-scoped stimuli"
            );
        }
        if let Some(session_id) = request.sender_session_id.as_deref() {
            anyhow::ensure!(
                self.channel_service
                    .channel_has_session(channel_id, session_id)
                    .await,
                "session {session_id} is not a member of channel {channel_id}"
            );
        }
        let now = now_ms();
        let available_at_ms = request.available_at_ms.unwrap_or(now);
        let stimulus = crate::ChannelStimulusView {
            stimulus_id: self.channel_service.next_stimulus_id(),
            channel_id: channel_id.to_string(),
            scope: request.scope,
            thread_root_message_id: trim_optional_string(request.thread_root_message_id),
            state: crate::ChannelStimulusState::Pending,
            kind: request.kind,
            visibility_hint: request.visibility_hint.unwrap_or_default(),
            content: request.content.trim().to_string(),
            addressed_member_ids: request.addressed_member_ids,
            sender_session_id: trim_optional_string(request.sender_session_id),
            sender_actor_id: trim_optional_string(request.sender_actor_id),
            sender_display_name: trim_optional_string(request.sender_display_name),
            source_kind: trim_optional_string(request.source_kind),
            source_ref: trim_optional_string(request.source_ref),
            dedupe_key: trim_optional_string(request.dedupe_key),
            progress_key: trim_optional_string(request.progress_key),
            created_at_ms: now,
            available_at_ms,
            expires_at_ms: request.expires_at_ms,
            claimed_at_ms: None,
            dispatched_at_ms: None,
            last_error: None,
            metadata: request.metadata,
        };
        self.channel_service.create_stimulus(stimulus).await
    }

    pub(crate) async fn list_channel_thread_work(
        &self,
        channel_id: &str,
        thread_root_message_id: Option<&str>,
    ) -> Result<Vec<crate::ChannelThreadWorkStateView>> {
        if let Some(thread_root_message_id) = thread_root_message_id {
            return self
                .channel_service
                .get_thread_state(channel_id, thread_root_message_id)
                .await
                .map(|state| state.into_iter().collect());
        }
        self.channel_service.list_thread_states(channel_id).await
    }

    pub(super) async fn channel_binding_from_run_id(
        &self,
        run_id: &str,
    ) -> Result<Option<(String, String)>> {
        let Ok(record) = self.run_record(run_id).await else {
            return Ok(None);
        };
        let Some(metadata) = record.view.input_metadata.as_ref() else {
            return Ok(None);
        };
        let Some(channel_id) = metadata.get("channel_id").and_then(Value::as_str) else {
            return Ok(None);
        };
        let Some(thread_root_message_id) = metadata
            .get("thread_root_message_id")
            .and_then(Value::as_str)
        else {
            return Ok(None);
        };
        if self.channel_service.get_channel(channel_id).await.is_err() {
            return Ok(None);
        }
        let Ok(message) = self
            .channel_service
            .get_message(channel_id, thread_root_message_id)
            .await
        else {
            return Ok(None);
        };
        if message.thread_root_message_id.is_some() {
            return Ok(None);
        }
        Ok(Some((
            channel_id.to_string(),
            thread_root_message_id.to_string(),
        )))
    }

    pub(super) async fn bind_schedule_to_channel_thread_from_run(
        &self,
        schedule_id: &str,
        run_id: &str,
    ) -> Result<()> {
        let Some((channel_id, thread_root_message_id)) =
            self.channel_binding_from_run_id(run_id).await?
        else {
            return Ok(());
        };
        self.channel_service
            .bind_thread_work(
                &channel_id,
                &thread_root_message_id,
                crate::channels::ChannelWorkBindingView {
                    binding_kind: crate::channels::ChannelWorkBindingKind::Schedule,
                    binding_ref: schedule_id.to_string(),
                    bound_at_ms: now_ms(),
                    latest_stimulus_id: None,
                },
            )
            .await?;
        Ok(())
    }

    async fn channel_binding_matches_thread(
        &self,
        channel_id: &str,
        thread_root_message_id: &str,
        binding: &crate::channels::ChannelWorkBindingView,
    ) -> Result<bool> {
        match binding.binding_kind {
            crate::channels::ChannelWorkBindingKind::Schedule => {
                let Ok(schedule) = self
                    .schedule_service
                    .get_schedule(&binding.binding_ref)
                    .await
                else {
                    return Ok(false);
                };
                let Some(created_by_run_id) = schedule.created_by_run_id.as_deref() else {
                    return Ok(false);
                };
                Ok(self
                    .channel_binding_from_run_id(created_by_run_id)
                    .await?
                    .is_some_and(|(candidate_channel_id, candidate_thread_root_message_id)| {
                        candidate_channel_id == channel_id
                            && candidate_thread_root_message_id == thread_root_message_id
                    }))
            }
            crate::channels::ChannelWorkBindingKind::SidechainAgent => {
                let Some(record) = self.supervisor.get(&AgentId(binding.binding_ref.clone()))
                else {
                    return Ok(false);
                };
                let Some(spawned_by_run_id) = record.spawned_by_run_id.as_deref() else {
                    return Ok(false);
                };
                Ok(self
                    .channel_binding_from_run_id(spawned_by_run_id)
                    .await?
                    .is_some_and(|(candidate_channel_id, candidate_thread_root_message_id)| {
                        candidate_channel_id == channel_id
                            && candidate_thread_root_message_id == thread_root_message_id
                    }))
            }
            _ => Ok(true),
        }
    }

    pub(super) async fn bind_sidechain_to_channel_thread_from_run(
        &self,
        sidechain_agent_id: &str,
        run_id: Option<&str>,
    ) -> Result<()> {
        let Some(run_id) = run_id else {
            return Ok(());
        };
        let Some((channel_id, thread_root_message_id)) =
            self.channel_binding_from_run_id(run_id).await?
        else {
            return Ok(());
        };
        self.channel_service
            .bind_thread_work(
                &channel_id,
                &thread_root_message_id,
                crate::channels::ChannelWorkBindingView {
                    binding_kind: crate::channels::ChannelWorkBindingKind::SidechainAgent,
                    binding_ref: sidechain_agent_id.to_string(),
                    bound_at_ms: now_ms(),
                    latest_stimulus_id: None,
                },
            )
            .await?;
        Ok(())
    }

    pub(super) async fn enqueue_channel_stimulus(
        &self,
        channel_id: &str,
        thread_root_message_id: Option<&str>,
        kind: crate::channels::ChannelStimulusKind,
        content: String,
        source_kind: Option<String>,
        source_ref: Option<String>,
        dedupe_key: Option<String>,
        progress_key: Option<String>,
        addressed_member_ids: Vec<String>,
        visibility_hint: crate::channels::ChannelStimulusVisibilityHint,
        available_at_ms: Option<u64>,
        metadata: Value,
    ) -> Result<crate::channels::ChannelStimulusView> {
        let now = now_ms();
        self.channel_service
            .create_stimulus(crate::channels::ChannelStimulusView {
                stimulus_id: self.channel_service.next_stimulus_id(),
                channel_id: channel_id.to_string(),
                scope: if thread_root_message_id.is_some() {
                    crate::channels::ChannelStimulusScope::Thread
                } else {
                    crate::channels::ChannelStimulusScope::Channel
                },
                thread_root_message_id: thread_root_message_id.map(ToOwned::to_owned),
                state: crate::channels::ChannelStimulusState::Pending,
                kind,
                visibility_hint,
                content,
                addressed_member_ids,
                sender_session_id: None,
                sender_actor_id: None,
                sender_display_name: None,
                source_kind,
                source_ref,
                dedupe_key,
                progress_key,
                created_at_ms: now,
                available_at_ms: available_at_ms.unwrap_or(now),
                expires_at_ms: None,
                claimed_at_ms: None,
                dispatched_at_ms: None,
                last_error: None,
                metadata,
            })
            .await
    }

    pub(crate) async fn upsert_channel_member(
        &self,
        channel_id: &str,
        request: crate::ChannelMemberRequest,
    ) -> Result<crate::ChannelView> {
        let channel = self.channel_service.get_channel(channel_id).await?;
        let joined_at_ms = channel
            .members
            .iter()
            .find(|member| member.member_id == request.member_id)
            .map(|member| member.joined_at_ms)
            .unwrap_or_else(now_ms);
        let member = self
            .channel_member_from_request(
                &request,
                channel.default_participation_mode.clone(),
                joined_at_ms,
            )
            .await?;
        self.channel_service.upsert_member(channel_id, member).await
    }

    pub(crate) async fn remove_channel_member(
        &self,
        channel_id: &str,
        member_id: &str,
    ) -> Result<crate::ChannelView> {
        self.channel_service
            .remove_member(channel_id, member_id)
            .await
    }

    pub(crate) async fn post_channel_message(
        self: &Arc<Self>,
        channel_id: &str,
        request: crate::PostChannelMessageRequest,
    ) -> Result<crate::ChannelMessageView> {
        anyhow::ensure!(
            !request.sender_actor_id.trim().is_empty(),
            "sender_actor_id is required"
        );
        let sender_session_id = request
            .sender_session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        self.ensure_sender_is_channel_member(
            channel_id,
            &request.sender_actor_id,
            sender_session_id.as_deref(),
        )
        .await?;
        let channel = self.channel_service.get_channel(channel_id).await?;
        let input_items =
            normalized_channel_input_items(request.content.as_deref(), &request.input_items)?;
        let resolved_parts = self
            .resolve_channel_message_input_parts(sender_session_id.as_deref(), &input_items)
            .await?;
        let reply_to_message_id = trim_optional_string(request.reply_to_message_id);
        let requested_thread_root_message_id = self
            .resolve_channel_thread_root(
                channel_id,
                reply_to_message_id.as_deref(),
                request.thread_root_message_id.as_deref(),
            )
            .await?;
        let output = RichOutput {
            content: self.render_input_parts(&resolved_parts)?,
            parts: resolved_parts
                .iter()
                .filter_map(ResolvedInputPart::content_part)
                .collect(),
            artifacts: Vec::new(),
        }
        .normalized();
        anyhow::ensure!(
            !output.content.is_empty() || !output.parts.is_empty(),
            "channel messages require content or input_items"
        );
        let addressed_member_ids = self.resolve_channel_message_addressed_members(
            &channel,
            sender_session_id.as_deref(),
            &request.addressed_member_ids,
            &output.content,
        )?;
        let message = self
            .channel_service
            .post_message(CreateChannelMessageRecord {
                channel_id: channel_id.to_string(),
                message_id: self.channel_service.next_message_id(),
                requested_thread_root_message_id,
                reply_to_message_id,
                sender: ActorRef {
                    id: request.sender_actor_id.trim().to_string(),
                    display_name: match trim_optional_string(request.sender_display_name) {
                        Some(display_name) => Some(display_name),
                        None => self
                            .channel_member_display_name_for_session(
                                &channel,
                                sender_session_id.as_deref(),
                            )
                            .cloned(),
                    },
                },
                sender_session_id,
                addressed_member_ids,
                output,
                metadata: request.metadata,
                created_at_ms: now_ms(),
            })
            .await?;
        let _ = self.drive_channel_after_public_message(&message).await?;
        Ok(message)
    }

    pub(crate) async fn set_channel_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        request: crate::SetChannelReactionRequest,
    ) -> Result<crate::ChannelMessageView> {
        anyhow::ensure!(!request.actor_id.trim().is_empty(), "actor_id is required");
        anyhow::ensure!(!request.emoji.trim().is_empty(), "emoji is required");
        self.ensure_reaction_actor_allowed(channel_id, &request.actor_id)
            .await?;
        self.channel_service
            .set_reaction(ChannelReactionMutation {
                channel_id: channel_id.to_string(),
                message_id: message_id.to_string(),
                actor_id: request.actor_id.trim().to_string(),
                emoji: request.emoji.trim().to_string(),
                timestamp_ms: now_ms(),
            })
            .await
    }

    pub(crate) async fn unset_channel_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        request: crate::SetChannelReactionRequest,
    ) -> Result<crate::ChannelMessageView> {
        anyhow::ensure!(!request.actor_id.trim().is_empty(), "actor_id is required");
        anyhow::ensure!(!request.emoji.trim().is_empty(), "emoji is required");
        self.ensure_reaction_actor_allowed(channel_id, &request.actor_id)
            .await?;
        self.channel_service
            .unset_reaction(ChannelReactionMutation {
                channel_id: channel_id.to_string(),
                message_id: message_id.to_string(),
                actor_id: request.actor_id.trim().to_string(),
                emoji: request.emoji.trim().to_string(),
                timestamp_ms: now_ms(),
            })
            .await
    }

    async fn ensure_sender_is_channel_member(
        &self,
        channel_id: &str,
        actor_id: &str,
        sender_session_id: Option<&str>,
    ) -> Result<()> {
        if let Some(session_id) = sender_session_id {
            anyhow::ensure!(
                self.channel_service
                    .channel_has_session(channel_id, session_id)
                    .await,
                "session {session_id} is not a member of channel {channel_id}"
            );
        } else {
            anyhow::ensure!(
                self.channel_service
                    .channel_has_actor(channel_id, actor_id)
                    .await,
                "actor {actor_id} is not a member of channel {channel_id}"
            );
        }
        Ok(())
    }

    async fn ensure_reaction_actor_allowed(&self, channel_id: &str, actor_id: &str) -> Result<()> {
        anyhow::ensure!(
            self.channel_service
                .channel_has_actor(channel_id, actor_id)
                .await
                || self
                    .channel_service
                    .channel_has_session(channel_id, actor_id)
                    .await,
            "actor {actor_id} is not a member of channel {channel_id}"
        );
        Ok(())
    }

    async fn resolve_channel_thread_root(
        &self,
        channel_id: &str,
        reply_to_message_id: Option<&str>,
        requested_thread_root_message_id: Option<&str>,
    ) -> Result<Option<String>> {
        let requested_thread_root_message_id = requested_thread_root_message_id
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let Some(reply_to_message_id) = reply_to_message_id else {
            if let Some(thread_root_message_id) = requested_thread_root_message_id {
                let root = self
                    .channel_service
                    .get_message(channel_id, thread_root_message_id)
                    .await?;
                anyhow::ensure!(
                    root.thread_root_message_id.is_none(),
                    "thread_root_message_id must point at a thread root message"
                );
                return Ok(Some(root.message_id));
            }
            return Ok(None);
        };
        let parent = self
            .channel_service
            .get_message(channel_id, reply_to_message_id)
            .await?;
        let derived_thread_root = parent
            .thread_root_message_id
            .clone()
            .unwrap_or_else(|| parent.message_id.clone());
        if let Some(thread_root_message_id) = requested_thread_root_message_id {
            anyhow::ensure!(
                derived_thread_root == thread_root_message_id,
                "reply_to_message_id {reply_to_message_id} does not belong to thread {thread_root_message_id}"
            );
        }
        Ok(Some(derived_thread_root))
    }

    async fn resolve_channel_message_input_parts(
        &self,
        sender_session_id: Option<&str>,
        input_items: &[SubmitInputItemRequest],
    ) -> Result<Vec<ResolvedInputPart>> {
        if let Some(session_id) = sender_session_id {
            return self
                .resolve_request_input_items(session_id, input_items)
                .await;
        }
        let mut resolved = Vec::with_capacity(input_items.len());
        for item in input_items {
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
                SubmitInputItemRequest::InlineAsset(upload) => {
                    resolved.push(ResolvedInputPart::Asset(
                        self.import_asset_upload(upload).await?,
                    ));
                }
                SubmitInputItemRequest::BoardReference { board_id, .. } => {
                    anyhow::bail!(
                        "board references in channel messages require one sender_session_id; board {board_id} cannot be resolved for a non-session sender"
                    );
                }
            }
        }
        Ok(resolved)
    }

    async fn channel_member_from_request(
        &self,
        request: &crate::ChannelMemberRequest,
        default_participation_mode: crate::ChannelParticipationMode,
        joined_at_ms: u64,
    ) -> Result<crate::ChannelMemberView> {
        anyhow::ensure!(
            !request.member_id.trim().is_empty(),
            "member_id is required"
        );
        anyhow::ensure!(
            !request.display_name.trim().is_empty(),
            "display_name is required"
        );
        let participation_mode = request
            .participation_mode
            .clone()
            .unwrap_or(default_participation_mode);
        let role = trim_optional_string(request.role.clone());
        let expertise_tags = request
            .expertise_tags
            .iter()
            .map(|tag| tag.trim())
            .filter(|tag| !tag.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        match request.member_kind {
            crate::ChannelMemberKind::Session => {
                anyhow::ensure!(
                    request.actor_id.is_none(),
                    "session channel members cannot define actor_id"
                );
                let session_id = request
                    .session_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or(request.member_id.trim());
                let effective_display_name = self
                    .effective_session_channel_display_name(session_id)
                    .await?;
                // Joining a channel is the session's social opt-in: make sure it carries a
                // (possibly empty) social ledger so its runtime surfaces `remember_about`
                // and the member can start forming impressions of the room. Best-effort —
                // membership must never fail on ledger plumbing.
                if let Err(error) = self.seed_session_social_ledger_if_missing(session_id).await {
                    warn!(
                        session_id = %session_id,
                        error = ?error,
                        "could not seed social ledger for channel member"
                    );
                }
                let display_name_mode = request.display_name_mode.unwrap_or_else(|| {
                    if request.display_name.trim() == effective_display_name {
                        crate::ChannelMemberDisplayNameMode::FollowAgent
                    } else {
                        crate::ChannelMemberDisplayNameMode::Manual
                    }
                });
                Ok(crate::ChannelMemberView {
                    member_id: request.member_id.trim().to_string(),
                    member_kind: crate::ChannelMemberKind::Session,
                    display_name: match display_name_mode {
                        crate::ChannelMemberDisplayNameMode::Manual => {
                            request.display_name.trim().to_string()
                        }
                        crate::ChannelMemberDisplayNameMode::FollowAgent => effective_display_name,
                    },
                    display_name_mode: Some(display_name_mode),
                    session_id: Some(session_id.to_string()),
                    actor_id: None,
                    role,
                    expertise_tags,
                    participation_mode,
                    muted: request.muted.unwrap_or(false),
                    joined_at_ms,
                })
            }
            crate::ChannelMemberKind::HumanActor => {
                anyhow::ensure!(
                    request.session_id.is_none(),
                    "human_actor channel members cannot define session_id"
                );
                anyhow::ensure!(
                    request.display_name_mode.is_none()
                        || request.display_name_mode
                            == Some(crate::ChannelMemberDisplayNameMode::Manual),
                    "human_actor channel members cannot follow agent display names"
                );
                let actor_id = request
                    .actor_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or(request.member_id.trim());
                Ok(crate::ChannelMemberView {
                    member_id: request.member_id.trim().to_string(),
                    member_kind: crate::ChannelMemberKind::HumanActor,
                    display_name_mode: Some(crate::ChannelMemberDisplayNameMode::Manual),
                    display_name: request.display_name.trim().to_string(),
                    session_id: None,
                    actor_id: Some(actor_id.to_string()),
                    role,
                    expertise_tags,
                    participation_mode,
                    muted: request.muted.unwrap_or(false),
                    joined_at_ms,
                })
            }
        }
    }

    pub(super) async fn effective_session_channel_display_name(
        &self,
        session_id: &str,
    ) -> Result<String> {
        let agent_id = self.agent_id_for_session(session_id).await?;
        let record = self
            .supervisor
            .get(&agent_id)
            .ok_or_else(|| anyhow!("unknown agent {}", agent_id.0))?;
        Ok(record
            .nickname
            .clone()
            .or(record.name.clone())
            .unwrap_or(agent_id.0))
    }

    pub(super) async fn sync_following_channel_member_display_names(
        &self,
        session_id: &str,
        display_name: &str,
    ) -> Result<usize> {
        let channels = self.channel_service.list_channels(None).await;
        let mut changed_channels = 0usize;
        for channel in channels {
            let channel_id = channel.summary.channel_id.clone();
            let channel_changed = channel.members.iter().any(|member| {
                member.member_kind == crate::ChannelMemberKind::Session
                    && member.session_id.as_deref() == Some(session_id)
                    && member.display_name_mode
                        == Some(crate::ChannelMemberDisplayNameMode::FollowAgent)
                    && member.display_name != display_name
            });
            if !channel_changed {
                continue;
            }
            self.channel_service
                .update_channel(&channel_id, |channel| {
                    let mut changed = false;
                    for member in &mut channel.members {
                        if member.member_kind != crate::ChannelMemberKind::Session
                            || member.session_id.as_deref() != Some(session_id)
                            || member.display_name_mode
                                != Some(crate::ChannelMemberDisplayNameMode::FollowAgent)
                        {
                            continue;
                        }
                        if member.display_name != display_name {
                            member.display_name = display_name.to_string();
                            changed = true;
                        }
                    }
                    if changed {
                        channel.summary.updated_at_ms = now_ms();
                    }
                    Ok(changed)
                })
                .await?;
            changed_channels += 1;
        }
        Ok(changed_channels)
    }

    pub(crate) async fn reconcile_channel_member_display_names(&self) -> Result<()> {
        let channels = self.channel_service.list_channels(None).await;
        for channel in channels {
            let channel_id = channel.summary.channel_id.clone();
            let mut expected_names = std::collections::BTreeMap::<String, String>::new();
            for member in &channel.members {
                if member.member_kind != crate::ChannelMemberKind::Session {
                    continue;
                }
                let Some(session_id) = member.session_id.as_deref() else {
                    continue;
                };
                if expected_names.contains_key(session_id) {
                    continue;
                }
                if let Ok(display_name) = self
                    .effective_session_channel_display_name(session_id)
                    .await
                {
                    expected_names.insert(session_id.to_string(), display_name);
                }
            }
            let channel_changed = channel.members.iter().any(|member| {
                if member.member_kind != crate::ChannelMemberKind::Session {
                    return member.display_name_mode
                        != Some(crate::ChannelMemberDisplayNameMode::Manual);
                }
                let Some(session_id) = member.session_id.as_deref() else {
                    return false;
                };
                let Some(current_display_name) = expected_names.get(session_id) else {
                    return false;
                };
                let next_mode = match member.display_name_mode {
                    Some(crate::ChannelMemberDisplayNameMode::FollowAgent) => {
                        Some(crate::ChannelMemberDisplayNameMode::FollowAgent)
                    }
                    Some(crate::ChannelMemberDisplayNameMode::Manual) | None => {
                        Some(crate::ChannelMemberDisplayNameMode::Manual)
                    }
                };
                member.display_name_mode != next_mode
                    || (next_mode == Some(crate::ChannelMemberDisplayNameMode::FollowAgent)
                        && member.display_name != *current_display_name)
            });
            if !channel_changed {
                continue;
            }
            self.channel_service
                .update_channel(&channel_id, |channel| {
                    let mut changed = false;
                    for member in &mut channel.members {
                        if member.member_kind != crate::ChannelMemberKind::Session {
                            if member.display_name_mode
                                != Some(crate::ChannelMemberDisplayNameMode::Manual)
                            {
                                member.display_name_mode =
                                    Some(crate::ChannelMemberDisplayNameMode::Manual);
                                changed = true;
                            }
                            continue;
                        }
                        let Some(session_id) = member.session_id.as_deref() else {
                            continue;
                        };
                        let Some(current_display_name) = expected_names.get(session_id) else {
                            continue;
                        };
                        let next_mode = match member.display_name_mode {
                            Some(crate::ChannelMemberDisplayNameMode::FollowAgent) => {
                                Some(crate::ChannelMemberDisplayNameMode::FollowAgent)
                            }
                            Some(crate::ChannelMemberDisplayNameMode::Manual) | None => {
                                Some(crate::ChannelMemberDisplayNameMode::Manual)
                            }
                        };
                        if member.display_name_mode != next_mode {
                            member.display_name_mode = next_mode;
                            changed = true;
                        }
                        if next_mode == Some(crate::ChannelMemberDisplayNameMode::FollowAgent)
                            && member.display_name != *current_display_name
                        {
                            member.display_name = current_display_name.clone();
                            changed = true;
                        }
                    }
                    if changed {
                        channel.summary.updated_at_ms = now_ms();
                    }
                    Ok(changed)
                })
                .await?;
        }
        Ok(())
    }

    fn channel_member_display_name_for_session<'a>(
        &self,
        channel: &'a crate::ChannelView,
        session_id: Option<&str>,
    ) -> Option<&'a String> {
        let session_id = session_id?;
        channel
            .members
            .iter()
            .find(|member| member.session_id.as_deref() == Some(session_id))
            .map(|member| &member.display_name)
    }

    fn stimulus_sender_for_channel(
        &self,
        channel: &crate::ChannelView,
        stimulus: &crate::ChannelStimulusView,
    ) -> (ActorRef, Option<String>) {
        if let Some(session_id) = stimulus.sender_session_id.as_deref()
            && let Some(member) = channel
                .members
                .iter()
                .find(|member| member.session_id.as_deref() == Some(session_id))
        {
            return (
                ActorRef {
                    id: session_id.to_string(),
                    display_name: Some(member.display_name.clone()),
                },
                Some(session_id.to_string()),
            );
        }
        if let Some(actor_id) = stimulus.sender_actor_id.as_deref() {
            return (
                ActorRef {
                    id: actor_id.to_string(),
                    display_name: trim_optional_string(stimulus.sender_display_name.clone()),
                },
                None,
            );
        }
        (
            ActorRef {
                id: "system".to_string(),
                display_name: Some("Kheish".to_string()),
            },
            None,
        )
    }

    fn ensure_assets_exist(&self, asset_ids: &[String]) -> Result<()> {
        for asset_id in asset_ids {
            anyhow::ensure!(!asset_id.trim().is_empty(), "asset_id is required");
            self.assets
                .get(asset_id)
                .ok_or_else(|| anyhow!("unknown asset {asset_id}"))?;
        }
        Ok(())
    }

    fn resolve_channel_message_addressed_members(
        &self,
        channel: &crate::ChannelView,
        sender_session_id: Option<&str>,
        explicit_addressed_member_ids: &[String],
        content: &str,
    ) -> Result<Vec<String>> {
        let mut addressed_member_ids = Vec::new();
        for member_id in explicit_addressed_member_ids
            .iter()
            .map(|member_id| member_id.trim())
            .filter(|member_id| !member_id.is_empty())
        {
            anyhow::ensure!(
                channel
                    .members
                    .iter()
                    .any(|member| member.member_id == member_id),
                "unknown addressed_member_id {member_id}"
            );
            let member_id = member_id.to_string();
            if !addressed_member_ids.contains(&member_id) {
                addressed_member_ids.push(member_id);
            }
        }
        if !addressed_member_ids.is_empty() || sender_session_id.is_some() {
            return Ok(addressed_member_ids);
        }

        let normalized_content = content.trim();
        if normalized_content.is_empty() {
            return Ok(addressed_member_ids);
        }

        let member_aliases = channel
            .members
            .iter()
            .map(|member| {
                let mut aliases = vec![normalize_address_alias(&member.member_id)];
                aliases.push(normalize_address_alias(&member.display_name));
                (member.member_id.clone(), aliases)
            })
            .collect::<Vec<_>>();

        let mut prefix_matches = channel
            .members
            .iter()
            .filter_map(|member| {
                let aliases = member_aliases
                    .iter()
                    .find(|(member_id, _)| member_id == &member.member_id)
                    .map(|(_, aliases)| aliases)?;
                let longest = aliases
                    .iter()
                    .filter(|alias| !alias.is_empty())
                    .filter(|alias| {
                        starts_with_member_alias(
                            &normalized_content.to_ascii_lowercase(),
                            alias.as_str(),
                        )
                    })
                    .max_by_key(|alias| alias.len())?;
                Some((longest.len(), member.member_id.clone()))
            })
            .collect::<Vec<_>>();
        prefix_matches.sort_by(|left, right| right.0.cmp(&left.0));
        if let Some((_, member_id)) = prefix_matches.first()
            && prefix_matches
                .iter()
                .filter(|(alias_len, _)| *alias_len == prefix_matches[0].0)
                .count()
                == 1
        {
            addressed_member_ids.push(member_id.clone());
        }

        for token in normalized_content.split_whitespace() {
            if !token.starts_with('@') {
                continue;
            }
            let mention = normalize_address_alias(token);
            if mention.is_empty() {
                continue;
            }
            let mut matches = channel
                .members
                .iter()
                .filter(|member| {
                    member_aliases
                        .iter()
                        .find(|(member_id, _)| member_id == &member.member_id)
                        .is_some_and(|(_, aliases)| aliases.iter().any(|alias| alias == &mention))
                })
                .map(|member| member.member_id.clone())
                .collect::<Vec<_>>();
            matches.dedup();
            if matches.len() == 1 && !addressed_member_ids.contains(&matches[0]) {
                addressed_member_ids.push(matches.remove(0));
            }
        }

        Ok(addressed_member_ids)
    }

    pub(super) async fn drive_channel_after_public_message(
        self: &Arc<Self>,
        message: &crate::ChannelMessageView,
    ) -> Result<Option<RunView>> {
        if message.sender_session_id.is_some() {
            return Ok(None);
        }
        let transition_lock = self.channel_turn_transition_lock(&message.channel_id).await;
        let _transition_guard = transition_lock.lock().await;
        let channel = self
            .channel_service
            .get_channel(&message.channel_id)
            .await?;
        if channel.summary.paused {
            return Ok(None);
        }
        let thread_root_message_id = message
            .thread_root_message_id
            .clone()
            .unwrap_or_else(|| message.message_id.clone());
        let mut leases = self.channel_leases_map(&channel.summary.channel_id).await?;
        let existing_lease = leases
            .values()
            .find(|lease| lease.thread_root_message_id == thread_root_message_id)
            .cloned();
        let latest_priority_session_ids = self
            .effective_priority_session_ids_for_human_message(
                &channel,
                message,
                existing_lease.as_ref(),
            )
            .await?;
        let active_holders =
            active_channel_holder_session_ids_for_thread(&leases, &thread_root_message_id);
        let active_count = active_holders.len() as u32;
        let mut candidates = self
            .rank_channel_session_candidates(
                &channel,
                &thread_root_message_id,
                None,
                &latest_priority_session_ids,
                Some(&message.message_id),
            )
            .await?
            .into_iter()
            .filter(|session_id| !active_holders.contains(session_id))
            .collect::<Vec<_>>();
        let slots_available = channel
            .autonomy_policy
            .max_parallel_public_speakers
            .saturating_sub(active_count) as usize;
        let budget_available = channel.autonomy_policy.max_agent_replies_per_human_message as usize;
        let start_count = candidates.len().min(slots_available).min(budget_available);
        let selected = candidates.drain(..start_count).collect::<Vec<_>>();
        let queued_after_selected = candidates;
        for lease in leases
            .values_mut()
            .filter(|lease| lease.thread_root_message_id == thread_root_message_id)
        {
            lease.last_human_message_id = Some(message.message_id.clone());
            lease.last_human_priority_session_ids = latest_priority_session_ids.clone();
            lease.expires_at_ms = now_ms() + channel.autonomy_policy.lease_timeout_ms;
            lease.queued_candidate_session_ids = queued_after_selected.clone();
        }
        let mut first_run = None;
        for holder_session_id in selected {
            let mut lease = crate::ChannelTurnLeaseView {
                turn_id: self.channel_service.next_turn_id(),
                channel_id: channel.summary.channel_id.clone(),
                thread_root_message_id: thread_root_message_id.clone(),
                origin_message_id: message.message_id.clone(),
                holder_session_id,
                active_run_id: None,
                superseded_run_id: None,
                superseded_turn_id: None,
                remaining_reply_budget: channel.autonomy_policy.max_agent_replies_per_human_message,
                expires_at_ms: now_ms() + channel.autonomy_policy.lease_timeout_ms,
                queued_candidate_session_ids: queued_after_selected.clone(),
                current_human_origin_message_id: Some(message.message_id.clone()),
                last_human_message_id: Some(message.message_id.clone()),
                last_human_priority_session_ids: latest_priority_session_ids.clone(),
                agent_reply_count_since_last_human: 0,
            };
            let run = self
                .schedule_channel_delivery_for_lease(&channel, &mut lease, false, false)
                .await?;
            if let Some(run) = run {
                if first_run.is_none() {
                    first_run = Some(run);
                }
                leases.insert(lease.turn_id.clone(), lease);
            }
        }
        self.replace_channel_leases(&channel.summary.channel_id, leases)
            .await?;
        Ok(first_run)
    }

    pub(super) async fn settle_channel_delivery_run(self: &Arc<Self>, run_id: &str) -> Result<()> {
        let record = self.run_record(run_id).await?;
        let Some(request) = record.payload.channel_delivery_request().cloned() else {
            return Ok(());
        };
        let transition_lock = self.channel_turn_transition_lock(&request.channel_id).await;
        let _transition_guard = transition_lock.lock().await;
        let thread_messages = self
            .channel_service
            .list_messages(&request.channel_id, Some(&request.thread_root_message_id))
            .await?;
        let current_human_origin_message_id = resolve_human_origin_message_id(
            &thread_messages,
            &request.origin_message_id,
            normalized_human_origin_message_id(&request),
        )
        .unwrap_or_else(|| request.origin_message_id.clone());
        let channel = self
            .channel_service
            .get_channel(&request.channel_id)
            .await?;
        let leases = self.channel_leases_map(&channel.summary.channel_id).await?;
        let Some(lease) = leases.get(&request.turn_id) else {
            return Ok(());
        };
        if lease.thread_root_message_id != request.thread_root_message_id {
            return Ok(());
        }

        // Idempotency: a re-settle (e.g. crash between posting and persisting the lease, then
        // boot recovery) must not post twice. A new-topic turn posts its own ROOT (thread_root
        // = None), so it would escape a thread-scoped search — look channel-wide in that case.
        let existing_message_scope = if request.autonomous_new_topic {
            None
        } else {
            Some(request.thread_root_message_id.as_str())
        };
        let existing_message = self
            .channel_service
            .list_messages(&request.channel_id, existing_message_scope)
            .await?
            .into_iter()
            .find(|message| {
                message
                    .metadata
                    .get("source_run_id")
                    .and_then(Value::as_str)
                    == Some(run_id)
            });

        let mut leases = self.channel_leases_map(&channel.summary.channel_id).await?;
        let Some(mut lease) = leases.remove(&request.turn_id) else {
            return Ok(());
        };
        if lease.thread_root_message_id != request.thread_root_message_id
            || lease.active_run_id.as_deref() != Some(run_id)
        {
            return Ok(());
        }

        let posted_message = if let Some(existing_message) = existing_message {
            Some(existing_message)
        } else if !self
            .channel_service
            .channel_has_session(&request.channel_id, &record.view.session_id)
            .await
        {
            None
        } else if let Some(output) = record.view.outputs.iter().rev().find(|output| {
            output.source_kind == Some(crate::DaemonOutputSourceKind::EmitOutput)
                && (!output.content.trim().is_empty()
                    || !output.parts.is_empty()
                    || !output.artifacts.is_empty())
        }) {
            let sender_display_name = channel
                .members
                .iter()
                .find(|member| {
                    member.session_id.as_deref() == Some(record.view.session_id.as_str())
                })
                .map(|member| member.display_name.clone());
            Some(
                self.channel_service
                    .post_message(CreateChannelMessageRecord {
                        channel_id: request.channel_id.clone(),
                        message_id: self.channel_service.next_message_id(),
                        sender: ActorRef {
                            id: record.view.session_id.clone(),
                            display_name: sender_display_name,
                        },
                        sender_session_id: Some(record.view.session_id.clone()),
                        addressed_member_ids: Vec::new(),
                        // Autonomous messages join the topic, they are NOT chained to the latest
                        // line: a hardcoded reply_to is what made every turn a reply to its
                        // predecessor (and the next turn's seed), regenerating a linear chain.
                        // Human turns keep their explicit reply target.
                        reply_to_message_id: if request.autonomous || request.autonomous_new_topic {
                            None
                        } else {
                            Some(lease.origin_message_id.clone())
                        },
                        requested_thread_root_message_id: if request.autonomous_new_topic {
                            None
                        } else {
                            Some(request.thread_root_message_id.clone())
                        },
                        output: RichOutput {
                            content: output.content.clone(),
                            parts: output.parts.clone(),
                            artifacts: output.artifacts.clone(),
                        }
                        .normalized(),
                        created_at_ms: now_ms(),
                        metadata: json!({
                            "source_kind": "channel_delivery",
                            "source_run_id": run_id,
                            "turn_id": request.turn_id,
                        }),
                    })
                    .await?,
            )
        } else {
            None
        };

        lease.expires_at_ms = now_ms() + channel.autonomy_policy.lease_timeout_ms;
        let lease_human_origin_message_id = lease
            .current_human_origin_message_id
            .clone()
            .unwrap_or_else(|| current_human_origin_message_id.clone());
        let pending_human_origin = lease
            .last_human_message_id
            .clone()
            .filter(|message_id| message_id != &lease_human_origin_message_id);
        let has_pending_human_origin = pending_human_origin.is_some();
        if let Some(next_human_origin) = pending_human_origin {
            lease.current_human_origin_message_id = Some(next_human_origin.clone());
            lease.origin_message_id = next_human_origin;
            lease.remaining_reply_budget =
                channel.autonomy_policy.max_agent_replies_per_human_message;
            lease.agent_reply_count_since_last_human = 0;
            let exclude_current_holder = lease
                .last_human_priority_session_ids
                .first()
                .is_none_or(|session_id| session_id != &record.view.session_id);
            lease.queued_candidate_session_ids = self
                .rank_channel_session_candidates(
                    &channel,
                    &request.thread_root_message_id,
                    exclude_current_holder.then_some(record.view.session_id.as_str()),
                    &lease.last_human_priority_session_ids,
                    lease.current_human_origin_message_id.as_deref(),
                )
                .await?;
        } else if let Some(posted_message) = posted_message.as_ref() {
            lease.origin_message_id = posted_message.message_id.clone();
        }

        let thread_messages_after_settle = self
            .channel_service
            .list_messages(&request.channel_id, Some(&request.thread_root_message_id))
            .await?;
        let current_human_origin_message_id = lease
            .current_human_origin_message_id
            .clone()
            .unwrap_or(current_human_origin_message_id);
        lease.agent_reply_count_since_last_human = agent_reply_count_since_human_origin(
            &thread_messages_after_settle,
            &current_human_origin_message_id,
        );
        lease.remaining_reply_budget = remaining_channel_reply_budget_for_human_origin(
            &channel,
            &thread_messages_after_settle,
            &leases,
            &request.thread_root_message_id,
            &current_human_origin_message_id,
        );

        if request.autonomous {
            // Autonomous turns are paced solely by the heartbeat worker; never cascade into
            // back-to-back agent replies here — the heartbeat picks the next speaker on its
            // own schedule. The lease was already removed above, so just persist and return.
            self.replace_channel_leases(&channel.summary.channel_id, leases)
                .await?;
            return Ok(());
        }

        if posted_message.is_none()
            && channel_thread_has_superseding_delivery_lease(
                &leases,
                &request.thread_root_message_id,
                run_id,
                &request.turn_id,
            )
        {
            self.replace_channel_leases(&channel.summary.channel_id, leases)
                .await?;
            return Ok(());
        }

        if lease.remaining_reply_budget == 0
            || active_channel_lease_count_for_thread(&leases, &request.thread_root_message_id)
                >= channel.autonomy_policy.max_parallel_public_speakers
        {
            self.replace_channel_leases(&channel.summary.channel_id, leases)
                .await?;
            return Ok(());
        }

        let exclude_next_holder = if has_pending_human_origin {
            lease
                .last_human_priority_session_ids
                .first()
                .is_none_or(|session_id| session_id != &record.view.session_id)
                .then_some(record.view.session_id.as_str())
        } else {
            Some(record.view.session_id.as_str())
        };
        let Some(holder_session_id) = self
            .next_channel_candidate_from_queue(&channel, &mut lease, exclude_next_holder)
            .await?
        else {
            self.replace_channel_leases(&channel.summary.channel_id, leases)
                .await?;
            return Ok(());
        };
        lease.turn_id = self.channel_service.next_turn_id();
        lease.holder_session_id = holder_session_id;
        lease.active_run_id = None;
        lease.superseded_run_id = None;
        lease.superseded_turn_id = None;
        let next_run = self
            .schedule_channel_delivery_for_lease(&channel, &mut lease, false, false)
            .await?;
        if next_run.is_some() {
            leases.insert(lease.turn_id.clone(), lease);
        }
        self.replace_channel_leases(&channel.summary.channel_id, leases)
            .await?;
        Ok(())
    }

    pub(crate) async fn restore_channel_leases(self: &Arc<Self>) -> Result<()> {
        self.recover_orphaned_channel_delivery_leases().await?;
        let channels = self.list_channels(None).await?;
        let mut runs_to_settle = Vec::new();
        for channel in channels {
            let transition_lock = self
                .channel_turn_transition_lock(&channel.summary.channel_id)
                .await;
            let _transition_guard = transition_lock.lock().await;
            let mut leases = self.channel_leases_map(&channel.summary.channel_id).await?;
            let mut changed = false;
            let mut missing_run_turn_ids = Vec::new();
            for lease in leases.values_mut() {
                let Some(run_id) = lease.active_run_id.clone() else {
                    continue;
                };
                match self.run_record(&run_id).await {
                    Ok(record) if record.view.status.is_terminal() => runs_to_settle.push(run_id),
                    Ok(_) => {}
                    Err(_) => {
                        missing_run_turn_ids.push(lease.turn_id.clone());
                        changed = true;
                    }
                }
            }
            for turn_id in missing_run_turn_ids {
                leases.remove(&turn_id);
            }
            if changed {
                self.replace_channel_leases(&channel.summary.channel_id, leases)
                    .await?;
            }
        }
        for run_id in runs_to_settle {
            self.settle_channel_delivery_run(&run_id).await?;
        }
        self.recover_orphaned_human_channel_messages().await?;
        Ok(())
    }

    pub(crate) fn spawn_channel_stimulus_worker(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            state.channel_stimulus_worker_loop().await;
        })
    }

    /// Spawns the background worker that grants autonomous speaking turns so members keep
    /// talking to each other with no human trigger, for channels enabled via the
    /// `KHEISH_CHANNEL_AUTONOMOUS` environment allowlist.
    pub(crate) fn spawn_channel_heartbeat_worker(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            state.channel_heartbeat_worker_loop().await;
        })
    }

    /// Whether daemon-driven autonomous chatter is enabled for one channel.
    ///
    /// `KHEISH_CHANNEL_AUTONOMOUS` accepts `1`/`true`/`*` to enable every channel, or a
    /// comma-separated allowlist of channel ids. Unset or empty keeps the feature off.
    fn channel_autonomous_enabled_for(channel_id: &str) -> bool {
        match std::env::var("KHEISH_CHANNEL_AUTONOMOUS") {
            Ok(value) => {
                let value = value.trim();
                if value.is_empty() {
                    return false;
                }
                if value == "1" || value.eq_ignore_ascii_case("true") || value == "*" {
                    return true;
                }
                value
                    .split(',')
                    .map(str::trim)
                    .any(|candidate| candidate == channel_id)
            }
            Err(_) => false,
        }
    }

    /// How many consecutive silent autonomous turns mark a topic as finished, after which
    /// the next granted turn opens a brand-new one in the main feed.
    const CHANNEL_HEARTBEAT_SILENCE_LIMIT: u32 = 2;

    /// How many consecutive silent turns put the channel to sleep entirely, so a genuinely
    /// dead room stops costing turns until a human (or any new message) wakes it.
    const CHANNEL_HEARTBEAT_DORMANCY_LIMIT: u32 = 4;

    /// One in every N autonomous turns opens a brand-new top-level topic, so the main feed
    /// keeps gaining subjects during a lively discussion — not only once a topic dies.
    const CHANNEL_HEARTBEAT_NEW_TOPIC_EVERY: u64 = 6;

    /// Cap on the number of live topics summarized on the channel "board" shown to agents.
    const CHANNEL_HEARTBEAT_BOARD_TOPICS: usize = 5;

    /// How long a topic can stay quiet and still be considered "live" enough to seed a turn
    /// into (so agents revive recent topics but leave genuinely dead ones alone). Defaults to
    /// fifteen heartbeat intervals; overridable via `KHEISH_CHANNEL_REVIVE_MS`.
    fn channel_heartbeat_revive_ms() -> u64 {
        let interval = Self::channel_heartbeat_interval_ms();
        std::env::var("KHEISH_CHANNEL_REVIVE_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value >= interval)
            .unwrap_or_else(|| interval.saturating_mul(15))
    }

    /// The idle interval before the daemon grants one autonomous speaking turn.
    fn channel_heartbeat_interval_ms() -> u64 {
        std::env::var("KHEISH_CHANNEL_HEARTBEAT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value >= 1_000)
            .unwrap_or(30_000)
    }

    /// The quiet period a finished topic must rest before an agent opens a fresh one.
    /// Defaults to four heartbeat intervals so new topics stay emergent rather than forced,
    /// and is floored at the heartbeat interval when configured via `KHEISH_CHANNEL_NEW_TOPIC_MS`.
    fn channel_heartbeat_new_topic_interval_ms() -> u64 {
        let interval = Self::channel_heartbeat_interval_ms();
        std::env::var("KHEISH_CHANNEL_NEW_TOPIC_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value >= interval)
            .unwrap_or_else(|| interval.saturating_mul(4))
    }

    async fn channel_heartbeat_worker_loop(self: Arc<Self>) {
        const POLL_INTERVAL_MS: u64 = 2_500;
        let mut states: std::collections::HashMap<String, ChannelHeartbeatState> =
            std::collections::HashMap::new();
        loop {
            if let Err(error) = self.channel_heartbeat_step(&mut states).await {
                error!(error = ?error, "channel heartbeat worker error");
            }
            sleep_until(Instant::now() + Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
    }

    async fn channel_heartbeat_step(
        self: &Arc<Self>,
        states: &mut std::collections::HashMap<String, ChannelHeartbeatState>,
    ) -> Result<()> {
        let interval_ms = Self::channel_heartbeat_interval_ms();
        let new_topic_interval_ms = Self::channel_heartbeat_new_topic_interval_ms();
        let revive_ms = Self::channel_heartbeat_revive_ms();
        let now = now_ms();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for channel in self.channel_service.list_channels(None).await {
            let channel_id = channel.summary.channel_id.clone();
            if !Self::channel_autonomous_enabled_for(&channel_id) {
                continue;
            }
            // Keep bookkeeping for enabled channels even while paused, so a pause/unpause never
            // resets the silence streak or lets the channel fire the instant it resumes.
            seen.insert(channel_id.clone());
            if channel.summary.paused {
                continue;
            }
            // Read leases and skip while a turn is in flight BEFORE reading messages: settle
            // posts the agent's message and only then removes the lease, so observing "not
            // busy" first guarantees the messages read below already includes whatever the
            // just-finished turn posted. Reading messages first could score a spoken turn as
            // silent (stale latest) if a turn settles between the two reads.
            let leases = match self.channel_leases_map(&channel_id).await {
                Ok(leases) => leases,
                Err(error) => {
                    warn!(channel_id = %channel_id, error = ?error, "heartbeat could not read leases");
                    continue;
                }
            };
            if leases.values().any(|lease| lease.active_run_id.is_some()) {
                continue;
            }
            // One channel's transient read error must not abort the poll for the others.
            let messages = match self.channel_service.list_messages(&channel_id, None).await {
                Ok(messages) => messages,
                Err(error) => {
                    warn!(channel_id = %channel_id, error = ?error, "heartbeat could not read messages");
                    continue;
                }
            };
            let Some(latest) = messages.last() else {
                continue;
            };
            let latest_id = latest.message_id.clone();
            let latest_created_ms = latest.created_at_ms;

            let state = states
                .entry(channel_id.clone())
                .or_insert_with(|| ChannelHeartbeatState {
                    // Defer a channel's first autonomous turn by one interval so a freshly
                    // booted daemon does not fire every enabled channel at once.
                    last_turn_at_ms: now,
                    last_seen_message_id: Some(latest_id.clone()),
                    ..Default::default()
                });

            // Score the previous grant, and reset the silence streak on ANY activity anywhere in
            // the room — the granted agent speaking, another agent, or a human in any thread.
            let advanced = state.last_seen_message_id.as_deref() != Some(latest_id.as_str());
            if state.pending_grant {
                state.pending_grant = false;
                if advanced {
                    state.consecutive_silent = 0;
                } else {
                    state.consecutive_silent = state.consecutive_silent.saturating_add(1);
                }
            } else if advanced {
                state.consecutive_silent = 0;
            }
            state.last_seen_message_id = Some(latest_id.clone());

            // A genuinely dead room sleeps until the activity check above wakes it — no endless
            // paid monologue into the void.
            if state.consecutive_silent >= Self::CHANNEL_HEARTBEAT_DORMANCY_LIMIT {
                continue;
            }
            // Pace on the last granted turn, not the last message, so an abstaining agent can
            // never trigger back-to-back turns (which would burn tokens on silence).
            if now.saturating_sub(state.last_turn_at_ms) < interval_ms {
                continue;
            }
            // Also require the channel itself to have gone quiet for one interval, so the daemon
            // never talks over an active human or agent exchange.
            if now.saturating_sub(latest_created_ms) < interval_ms {
                continue;
            }

            // Map the channel into topics (top-level roots) and their latest activity, so a turn
            // can be seeded into the stalest still-live topic instead of always the newest one.
            let mut root_last_activity: std::collections::BTreeMap<String, u64> =
                std::collections::BTreeMap::new();
            for message in &messages {
                let root = message
                    .thread_root_message_id
                    .clone()
                    .unwrap_or_else(|| message.message_id.clone());
                let entry = root_last_activity.entry(root).or_insert(0);
                *entry = (*entry).max(message.created_at_ms);
            }
            let mut live_roots: Vec<(String, u64)> = root_last_activity
                .into_iter()
                .filter(|(_, last)| now.saturating_sub(*last) <= revive_ms)
                .collect();
            // Stalest-first with a deterministic id tie-break, so attention rotates across every
            // live topic rather than fixating on the most recent.
            live_roots.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

            state.turn_counter = state.turn_counter.saturating_add(1);
            let phase = state.turn_counter;

            // Decide the turn's shape: keep a topic going, or open a brand-new one in the feed.
            let new_topic = if state.consecutive_silent >= Self::CHANNEL_HEARTBEAT_SILENCE_LIMIT {
                // The room wound down — rest a beat, then let someone start something fresh.
                if now.saturating_sub(latest_created_ms) < new_topic_interval_ms {
                    continue;
                }
                true
            } else {
                // No live topic to join, or the periodic slot: open a new main-feed subject.
                live_roots.is_empty() || phase % Self::CHANNEL_HEARTBEAT_NEW_TOPIC_EVERY == 0
            };

            // Choose where to seed a continuation: the stalest live topic. Fresh topics ignore
            // this and post their own root, so seed them on the channel's latest for context.
            let (seed_thread_root, seed_origin_id, exclude_session_id) = if new_topic {
                let root = latest
                    .thread_root_message_id
                    .clone()
                    .unwrap_or_else(|| latest_id.clone());
                (root, latest_id.clone(), latest.sender_session_id.clone())
            } else {
                let target_root = live_roots
                    .first()
                    .map(|(root, _)| root.clone())
                    .unwrap_or_else(|| {
                        latest
                            .thread_root_message_id
                            .clone()
                            .unwrap_or_else(|| latest_id.clone())
                    });
                // Anchor on that topic's most recent message for context (the reply is NOT
                // chained to it — see settle) and never let that author answer themselves.
                let anchor = messages
                    .iter()
                    .filter(|message| {
                        message
                            .thread_root_message_id
                            .as_deref()
                            .unwrap_or(message.message_id.as_str())
                            == target_root.as_str()
                    })
                    .max_by_key(|message| message.created_at_ms)
                    .unwrap_or(latest);
                (
                    target_root,
                    anchor.message_id.clone(),
                    anchor.sender_session_id.clone(),
                )
            };

            state.last_turn_at_ms = now;
            let granted = match self
                .drive_channel_autonomous_turn(
                    &channel_id,
                    &seed_thread_root,
                    &seed_origin_id,
                    exclude_session_id.as_deref(),
                    new_topic,
                    &mut state.last_granted,
                )
                .await
            {
                Ok(run) => run.is_some(),
                Err(error) => {
                    warn!(channel_id = %channel_id, error = ?error, "autonomous channel turn failed");
                    false
                }
            };
            if granted {
                state.pending_grant = true;
                // Note: we deliberately do NOT reset consecutive_silent here for a new topic.
                // new_topic is forced true exactly in the silent cases (streak >= silence limit,
                // or no live topic to join), so resetting on grant would peg the streak below
                // the dormancy limit forever and a quiet channel would pay for a turn every
                // interval indefinitely. Instead: if the fresh topic actually gets a message the
                // streak resets naturally next poll (the `advanced` check); if it too is met with
                // silence, the streak keeps climbing until the channel goes dormant.
            }
        }
        // Drop bookkeeping for channels that are gone or no longer autonomous.
        states.retain(|channel_id, _| seen.contains(channel_id));
        Ok(())
    }

    /// Grants one eligible agent a free-form speaking turn seeded on the channel's latest
    /// message, so members keep talking to each other with no human trigger. When `new_topic`
    /// is set the agent opens a brand-new top-level topic in the main feed instead of replying
    /// inside the seeded thread.
    async fn drive_channel_autonomous_turn(
        self: &Arc<Self>,
        channel_id: &str,
        seed_thread_root: &str,
        seed_origin_message_id: &str,
        exclude_session_id: Option<&str>,
        new_topic: bool,
        last_granted: &mut std::collections::HashMap<String, u64>,
    ) -> Result<Option<RunView>> {
        let transition_lock = self.channel_turn_transition_lock(channel_id).await;
        let _transition_guard = transition_lock.lock().await;
        let channel = self.channel_service.get_channel(channel_id).await?;
        if channel.summary.paused || !Self::channel_autonomous_enabled_for(channel_id) {
            return Ok(None);
        }
        let mut leases = self.channel_leases_map(channel_id).await?;
        // Never overlap autonomous turns: skip while any turn is already in flight.
        if leases.values().any(|lease| lease.active_run_id.is_some()) {
            return Ok(None);
        }
        let messages = self.channel_service.list_messages(channel_id, None).await?;
        if messages.is_empty() {
            return Ok(None);
        }
        let candidates = self
            .rank_channel_session_candidates(
                &channel,
                seed_thread_root,
                exclude_session_id,
                &[],
                Some(seed_origin_message_id),
            )
            .await?;
        if candidates.is_empty() {
            return Ok(None);
        }
        // Fair rotation: least-recently-GRANTED first, so an agent that abstained (and thus left
        // no message to key on) still rotates to the back instead of being re-picked forever;
        // then least-recently-spoke, then id — all deterministic, no RNG.
        let mut last_spoke: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        for message in &messages {
            if let Some(session_id) = message.sender_session_id.as_ref() {
                let entry = last_spoke.entry(session_id.clone()).or_insert(0);
                *entry = (*entry).max(message.created_at_ms);
            }
        }
        let holder_session_id = candidates
            .into_iter()
            .min_by_key(|session_id| {
                (
                    last_granted.get(session_id).copied().unwrap_or(0),
                    last_spoke.get(session_id).copied().unwrap_or(0),
                    session_id.clone(),
                )
            })
            .expect("candidates checked non-empty above");
        let holder_for_grant = holder_session_id.clone();
        let mut lease = crate::ChannelTurnLeaseView {
            turn_id: self.channel_service.next_turn_id(),
            channel_id: channel.summary.channel_id.clone(),
            thread_root_message_id: seed_thread_root.to_string(),
            origin_message_id: seed_origin_message_id.to_string(),
            holder_session_id,
            active_run_id: None,
            superseded_run_id: None,
            superseded_turn_id: None,
            remaining_reply_budget: 1,
            expires_at_ms: now_ms() + channel.autonomy_policy.lease_timeout_ms,
            queued_candidate_session_ids: Vec::new(),
            current_human_origin_message_id: None,
            last_human_message_id: None,
            last_human_priority_session_ids: Vec::new(),
            agent_reply_count_since_last_human: 0,
        };
        let run = self
            .schedule_channel_delivery_for_lease(&channel, &mut lease, true, new_topic)
            .await?;
        if let Some(run) = run {
            // Record the grant only once it actually scheduled, so fairness tracks turns given,
            // not turns attempted.
            last_granted.insert(holder_for_grant, now_ms());
            leases.insert(lease.turn_id.clone(), lease);
            self.replace_channel_leases(channel_id, leases).await?;
            return Ok(Some(run));
        }
        Ok(None)
    }

    pub(crate) async fn restore_channel_stimulus_worker_on_boot(&self) -> Result<()> {
        let now = now_ms();
        for channel in self.channel_service.list_channels(None).await {
            let channel_id = channel.summary.channel_id;
            let messages = self
                .channel_service
                .list_messages(&channel_id, None)
                .await?;
            let root_message_ids = messages
                .iter()
                .filter(|message| message.thread_root_message_id.is_none())
                .map(|message| message.message_id.clone())
                .collect::<std::collections::BTreeSet<_>>();
            let stimuli = self
                .channel_service
                .list_stimuli(&channel_id, None, None)
                .await?;
            let thread_states = self.channel_service.list_thread_states(&channel_id).await?;

            let mut repaired_states = std::collections::BTreeMap::new();
            for mut state in thread_states {
                if !root_message_ids.contains(&state.thread_root_message_id) {
                    continue;
                }
                let thread_root_message_id = state.thread_root_message_id.clone();
                state.progress_snapshots.retain(|snapshot| {
                    snapshot
                        .latest_message_id
                        .as_deref()
                        .is_none_or(|message_id| {
                            messages
                                .iter()
                                .find(|message| message.message_id == message_id)
                                .is_some_and(|message| {
                                    canonical_thread_root_for_materialized_message(message)
                                        .as_deref()
                                        == Some(thread_root_message_id.as_str())
                                        && message
                                            .metadata
                                            .get("progress_key")
                                            .and_then(Value::as_str)
                                            == Some(snapshot.progress_key.as_str())
                                })
                        })
                });
                let mut repaired_bindings = Vec::new();
                for binding in state.bindings {
                    if self
                        .channel_binding_matches_thread(
                            &channel_id,
                            &thread_root_message_id,
                            &binding,
                        )
                        .await?
                    {
                        repaired_bindings.push(binding);
                    }
                }
                state.bindings = repaired_bindings;
                repaired_states.insert(thread_root_message_id, state);
            }
            for stimulus in &stimuli {
                let Some((_, message)) =
                    find_materialized_equivalent_stimulus(&messages, &stimuli, stimulus)
                else {
                    continue;
                };
                let Some(thread_root_message_id) =
                    canonical_thread_root_for_materialized_message(&message)
                else {
                    continue;
                };
                if !root_message_ids.contains(&thread_root_message_id) {
                    continue;
                }
                let candidate = recovered_thread_state_from_materialized_stimulus(
                    &channel_id,
                    &thread_root_message_id,
                    stimulus,
                    &message,
                );
                repaired_states
                    .entry(thread_root_message_id)
                    .and_modify(|existing| merge_recovered_thread_state(existing, &candidate))
                    .or_insert(candidate);
            }
            let mut repaired_states = repaired_states.into_values().collect::<Vec<_>>();
            repaired_states.sort_by(|left, right| {
                left.thread_root_message_id
                    .cmp(&right.thread_root_message_id)
            });
            self.channel_service
                .replace_thread_states(&channel_id, repaired_states)
                .await?;

            for stimulus in stimuli {
                if stimulus
                    .thread_root_message_id
                    .as_ref()
                    .is_some_and(|thread_root_message_id| {
                        !root_message_ids.contains(thread_root_message_id)
                    })
                    && matches!(
                        stimulus.state,
                        crate::channels::ChannelStimulusState::Pending
                            | crate::channels::ChannelStimulusState::Claimed
                    )
                {
                    let _ = self
                        .channel_service
                        .set_stimulus_state(
                            &channel_id,
                            &stimulus.stimulus_id,
                            crate::channels::ChannelStimulusState::Cancelled,
                            now,
                            Some("thread-scoped stimulus lost its canonical root".to_string()),
                        )
                        .await?;
                    continue;
                }
                if stimulus.state == crate::channels::ChannelStimulusState::Claimed {
                    let _ = self
                        .channel_service
                        .update_stimulus(&channel_id, &stimulus.stimulus_id, move |stimulus| {
                            stimulus.state = crate::channels::ChannelStimulusState::Pending;
                            stimulus.claimed_at_ms = None;
                            stimulus.available_at_ms = now;
                            stimulus.last_error = Some("requeued after daemon restart".to_string());
                            Ok(true)
                        })
                        .await?;
                }
            }
        }
        for schedule in self.schedule_service.list_schedules(None).await {
            let Some(created_by_run_id) = schedule.created_by_run_id.as_deref() else {
                continue;
            };
            if self
                .channel_service
                .thread_for_binding(
                    crate::channels::ChannelWorkBindingKind::Schedule,
                    &schedule.schedule_id,
                )
                .await
                .is_none()
            {
                self.bind_schedule_to_channel_thread_from_run(
                    &schedule.schedule_id,
                    created_by_run_id,
                )
                .await?;
            }
        }
        for record in self.supervisor.list() {
            let Some(spawned_by_run_id) = record.spawned_by_run_id.as_deref() else {
                continue;
            };
            if self
                .channel_service
                .thread_for_binding(
                    crate::channels::ChannelWorkBindingKind::SidechainAgent,
                    &record.id.0,
                )
                .await
                .is_none()
            {
                self.bind_sidechain_to_channel_thread_from_run(
                    &record.id.0,
                    Some(spawned_by_run_id),
                )
                .await?;
            }
        }
        self.channel_service.notify().notify_waiters();
        Ok(())
    }

    async fn channel_stimulus_worker_loop(self: Arc<Self>) {
        loop {
            match self.channel_stimulus_step().await {
                Ok(Some(deadline_ms)) => {
                    let wait = deadline_ms.saturating_sub(now_ms()).max(1);
                    tokio::select! {
                        _ = self.channel_service.notify().notified() => {}
                        _ = sleep_until(Instant::now() + Duration::from_millis(wait)) => {}
                    }
                }
                Ok(None) => {
                    self.channel_service.notify().notified().await;
                }
                Err(error) => {
                    error!(error = ?error, "channel stimulus worker error");
                    tokio::select! {
                        _ = self.channel_service.notify().notified() => {}
                        _ = sleep_until(Instant::now() + Duration::from_millis(Self::CHANNEL_STIMULUS_ERROR_RETRY_BACKOFF_MS)) => {}
                    }
                }
            }
        }
    }

    async fn channel_stimulus_step(self: &Arc<Self>) -> Result<Option<u64>> {
        let snapshot = self
            .channel_service
            .stimulus_scheduler_snapshot(now_ms())
            .await;
        for due in snapshot.due {
            if let Err(error) = self
                .process_due_channel_stimulus(&due.channel_id, &due.stimulus_id)
                .await
            {
                error!(
                    channel_id = %due.channel_id,
                    stimulus_id = %due.stimulus_id,
                    error = ?error,
                    "failed to process due channel stimulus"
                );
                let retry_at_ms = now_ms() + Self::CHANNEL_STIMULUS_ERROR_RETRY_BACKOFF_MS;
                let message = error.to_string();
                let _ = self
                    .channel_service
                    .update_stimulus(&due.channel_id, &due.stimulus_id, move |stimulus| {
                        stimulus.state = crate::channels::ChannelStimulusState::Pending;
                        stimulus.available_at_ms = retry_at_ms;
                        stimulus.last_error = Some(message.clone());
                        Ok(true)
                    })
                    .await;
            }
        }
        Ok(self
            .channel_service
            .stimulus_scheduler_snapshot(now_ms())
            .await
            .next_due_at_ms)
    }

    async fn process_due_channel_stimulus(
        self: &Arc<Self>,
        channel_id: &str,
        stimulus_id: &str,
    ) -> Result<()> {
        let now = now_ms();
        let Some(_) = self
            .channel_service
            .claim_stimulus(channel_id, stimulus_id, now)
            .await?
        else {
            return Ok(());
        };
        let stimulus = self
            .channel_service
            .get_stimulus(channel_id, stimulus_id)
            .await?;
        if stimulus.state != crate::channels::ChannelStimulusState::Claimed {
            return Ok(());
        }
        if stimulus
            .expires_at_ms
            .is_some_and(|expires_at_ms| expires_at_ms <= now)
        {
            let _ = self
                .channel_service
                .set_stimulus_state(
                    channel_id,
                    stimulus_id,
                    crate::channels::ChannelStimulusState::Cancelled,
                    now,
                    Some("stimulus expired before dispatch".to_string()),
                )
                .await?;
            return Ok(());
        }

        let channel = self.channel_service.get_channel(channel_id).await?;
        let channel_messages = self.channel_service.list_messages(channel_id, None).await?;
        if channel.summary.paused {
            let retry_at_ms = now + Self::CHANNEL_STIMULUS_ERROR_RETRY_BACKOFF_MS;
            let _ = self
                .channel_service
                .update_stimulus(channel_id, stimulus_id, move |stimulus| {
                    stimulus.state = crate::channels::ChannelStimulusState::Pending;
                    stimulus.available_at_ms = retry_at_ms;
                    stimulus.last_error = Some("channel autonomy is paused".to_string());
                    Ok(true)
                })
                .await?;
            return Ok(());
        }

        let thread_states = self.channel_service.list_thread_states(channel_id).await?;
        let channel_stimuli = self
            .channel_service
            .list_stimuli(channel_id, None, None)
            .await?;
        let existing_root = stimulus.thread_root_message_id.clone().or_else(|| {
            stimulus.dedupe_key.as_deref().and_then(|dedupe_key| {
                thread_states
                    .iter()
                    .find(|state| {
                        state.initiative_key.as_deref() == Some(dedupe_key)
                            && state.status != crate::channels::ChannelThreadWorkStatus::Superseded
                            && state.status != crate::channels::ChannelThreadWorkStatus::Completed
                    })
                    .map(|state| state.thread_root_message_id.clone())
            })
        });
        let materialized_message =
            find_materialized_equivalent_stimulus(&channel_messages, &channel_stimuli, &stimulus)
                .map(|(_, message)| message);

        let open_new_root = existing_root.is_none()
            && stimulus.scope == crate::channels::ChannelStimulusScope::Channel;
        if open_new_root && materialized_message.is_none() {
            if let Some(retry_at_ms) =
                autonomous_root_quiet_period_deadline(&channel, &thread_states, now)
            {
                let _ = self
                    .channel_service
                    .update_stimulus(channel_id, stimulus_id, move |stimulus| {
                        stimulus.state = crate::channels::ChannelStimulusState::Pending;
                        stimulus.available_at_ms = retry_at_ms;
                        stimulus.last_error =
                            Some("autonomous root quiet period active".to_string());
                        Ok(true)
                    })
                    .await?;
                return Ok(());
            }
            let active_autonomous_roots = thread_states
                .iter()
                .filter(|state| {
                    state.topic_kind != crate::channels::ChannelThreadTopicKind::Human
                        && state.status != crate::channels::ChannelThreadWorkStatus::Completed
                        && state.status != crate::channels::ChannelThreadWorkStatus::Superseded
                })
                .count() as u32;
            if active_autonomous_roots >= channel.autonomy_policy.max_active_autonomous_roots {
                let _ = self
                    .channel_service
                    .set_stimulus_state(
                        channel_id,
                        stimulus_id,
                        crate::channels::ChannelStimulusState::Cancelled,
                        now,
                        Some("channel autonomous root budget is exhausted".to_string()),
                    )
                    .await?;
                return Ok(());
            }
            let recent_autonomous_roots = channel_messages
                .iter()
                .filter(|message| {
                    message.thread_root_message_id.is_none()
                        && message.created_at_ms >= now.saturating_sub(3_600_000)
                        && message.metadata.get("source_kind").and_then(Value::as_str)
                            == Some("channel_stimulus")
                })
                .count() as u32;
            if recent_autonomous_roots >= channel.autonomy_policy.max_autonomous_root_posts_per_hour
            {
                let _ = self
                    .channel_service
                    .set_stimulus_state(
                        channel_id,
                        stimulus_id,
                        crate::channels::ChannelStimulusState::Cancelled,
                        now,
                        Some("channel autonomous root post budget is exhausted".to_string()),
                    )
                    .await?;
                return Ok(());
            }
        }

        let stimulus = self
            .channel_service
            .get_stimulus(channel_id, stimulus_id)
            .await?;
        if stimulus.state != crate::channels::ChannelStimulusState::Claimed {
            return Ok(());
        }
        let (stimulus_sender, stimulus_sender_session_id) =
            self.stimulus_sender_for_channel(&channel, &stimulus);
        let reused_materialized_message = materialized_message.is_some();
        let (message, thread_root_message_id) = if let Some(message) = materialized_message {
            let thread_root_message_id = canonical_thread_root_for_materialized_message(&message)
                .or(existing_root.clone())
                .ok_or_else(|| {
                    anyhow!(
                        "materialized stimulus {} is missing its canonical thread root",
                        stimulus.stimulus_id
                    )
                })?;
            (message, thread_root_message_id)
        } else if let Some(thread_root_message_id) = existing_root {
            if stimulus.visibility_hint == crate::channels::ChannelStimulusVisibilityHint::Main {
                let message = self
                    .channel_service
                    .post_message(CreateChannelMessageRecord {
                        channel_id: channel.summary.channel_id.clone(),
                        message_id: self.channel_service.next_message_id(),
                        sender: stimulus_sender.clone(),
                        sender_session_id: stimulus_sender_session_id.clone(),
                        addressed_member_ids: stimulus.addressed_member_ids.clone(),
                        reply_to_message_id: None,
                        requested_thread_root_message_id: None,
                        output: RichOutput {
                            content: stimulus.content.clone(),
                            parts: Vec::new(),
                            artifacts: Vec::new(),
                        }
                        .normalized(),
                        created_at_ms: now,
                        metadata: json!({
                            "source_kind": "channel_stimulus",
                            "stimulus_id": stimulus.stimulus_id,
                            "stimulus_kind": stimulus.kind,
                            "source_ref": stimulus.source_ref,
                            "progress_key": stimulus.progress_key,
                            "presentation": "main_summary",
                            "canonical_thread_root_message_id": thread_root_message_id,
                        }),
                    })
                    .await?;
                (message, thread_root_message_id)
            } else {
                let message = self
                    .channel_service
                    .post_message(CreateChannelMessageRecord {
                        channel_id: channel.summary.channel_id.clone(),
                        message_id: self.channel_service.next_message_id(),
                        sender: stimulus_sender.clone(),
                        sender_session_id: stimulus_sender_session_id.clone(),
                        addressed_member_ids: stimulus.addressed_member_ids.clone(),
                        reply_to_message_id: Some(thread_root_message_id.clone()),
                        requested_thread_root_message_id: Some(thread_root_message_id.clone()),
                        output: RichOutput {
                            content: stimulus.content.clone(),
                            parts: Vec::new(),
                            artifacts: Vec::new(),
                        }
                        .normalized(),
                        created_at_ms: now,
                        metadata: json!({
                            "source_kind": "channel_stimulus",
                            "stimulus_id": stimulus.stimulus_id,
                            "stimulus_kind": stimulus.kind,
                            "source_ref": stimulus.source_ref,
                            "progress_key": stimulus.progress_key,
                            "presentation": "thread",
                        }),
                    })
                    .await?;
                (message, thread_root_message_id)
            }
        } else {
            let message = self
                .channel_service
                .post_message(CreateChannelMessageRecord {
                    channel_id: channel.summary.channel_id.clone(),
                    message_id: self.channel_service.next_message_id(),
                    sender: stimulus_sender,
                    sender_session_id: stimulus_sender_session_id,
                    addressed_member_ids: stimulus.addressed_member_ids.clone(),
                    reply_to_message_id: None,
                    requested_thread_root_message_id: None,
                    output: RichOutput {
                        content: stimulus.content.clone(),
                        parts: Vec::new(),
                        artifacts: Vec::new(),
                    }
                    .normalized(),
                    created_at_ms: now,
                    metadata: json!({
                        "source_kind": "channel_stimulus",
                        "stimulus_id": stimulus.stimulus_id,
                        "stimulus_kind": stimulus.kind,
                        "source_ref": stimulus.source_ref,
                        "progress_key": stimulus.progress_key,
                        "presentation": "main",
                    }),
                })
                .await?;
            let thread_root_message_id = message.message_id.clone();
            self.channel_service
                .upsert_thread_state(channel_id, &thread_root_message_id, |_| {
                    Ok(crate::channels::ChannelThreadWorkStateView {
                        channel_id: channel_id.to_string(),
                        thread_root_message_id: thread_root_message_id.clone(),
                        topic_kind: thread_topic_kind_for_stimulus(&stimulus),
                        status: thread_status_for_stimulus(&stimulus),
                        owner_session_id: None,
                        initiative_key: stimulus.dedupe_key.clone(),
                        source_kind: stimulus.source_kind.clone(),
                        source_ref: stimulus.source_ref.clone(),
                        last_stimulus_id: Some(stimulus.stimulus_id.clone()),
                        last_signal_at_ms: now,
                        last_main_promotion_at_ms: Some(now),
                        bindings: Vec::new(),
                        progress_snapshots: Vec::new(),
                        metadata: stimulus.metadata.clone(),
                    })
                })
                .await?;
            (message, thread_root_message_id)
        };
        let signal_at_ms = message.created_at_ms;

        self.channel_service
            .upsert_thread_state(channel_id, &thread_root_message_id, |existing| {
                let mut state = existing.unwrap_or(crate::channels::ChannelThreadWorkStateView {
                    channel_id: channel_id.to_string(),
                    thread_root_message_id: thread_root_message_id.clone(),
                    topic_kind: thread_topic_kind_for_stimulus(&stimulus),
                    status: thread_status_for_stimulus(&stimulus),
                    owner_session_id: None,
                    initiative_key: stimulus.dedupe_key.clone(),
                    source_kind: stimulus.source_kind.clone(),
                    source_ref: stimulus.source_ref.clone(),
                    last_stimulus_id: None,
                    last_signal_at_ms: signal_at_ms,
                    last_main_promotion_at_ms: None,
                    bindings: Vec::new(),
                    progress_snapshots: Vec::new(),
                    metadata: stimulus.metadata.clone(),
                });
                state.status =
                    merge_thread_work_status(state.status, thread_status_for_stimulus(&stimulus));
                state.last_stimulus_id = Some(stimulus.stimulus_id.clone());
                state.last_signal_at_ms = state.last_signal_at_ms.max(signal_at_ms);
                if message.thread_root_message_id.is_none() {
                    state.last_main_promotion_at_ms = Some(
                        state
                            .last_main_promotion_at_ms
                            .map_or(signal_at_ms, |current| current.max(signal_at_ms)),
                    );
                    state.initiative_key = stimulus.dedupe_key.clone().or(state.initiative_key);
                }
                if state.source_kind.is_none() {
                    state.source_kind = stimulus.source_kind.clone();
                }
                if state.source_ref.is_none() {
                    state.source_ref = stimulus.source_ref.clone();
                }
                if state.metadata.is_null() && !stimulus.metadata.is_null() {
                    state.metadata = stimulus.metadata.clone();
                }
                Ok(state)
            })
            .await?;
        if let (Some(progress_key), Some(source_kind), Some(source_ref)) = (
            stimulus.progress_key.clone(),
            stimulus.source_kind.clone(),
            stimulus.source_ref.clone(),
        ) {
            self.channel_service
                .upsert_progress_snapshot(
                    channel_id,
                    &thread_root_message_id,
                    crate::channels::ChannelProgressSnapshotView {
                        progress_key,
                        source_kind,
                        source_ref,
                        latest_message_id: Some(message.message_id.clone()),
                        updated_at_ms: signal_at_ms,
                        summary_digest: Some(message.output.content.clone()),
                    },
                )
                .await?;
        }
        self.channel_service
            .set_stimulus_state(
                channel_id,
                stimulus_id,
                crate::channels::ChannelStimulusState::Dispatched,
                now,
                None,
            )
            .await?;

        let promoted_existing_root_to_main = message
            .metadata
            .get("canonical_thread_root_message_id")
            .and_then(Value::as_str)
            .is_some();
        let should_wake_thread = !promoted_existing_root_to_main
            && !matches!(
                stimulus.kind,
                crate::channels::ChannelStimulusKind::ResultSummary
                    | crate::channels::ChannelStimulusKind::SupersessionNotice
            );
        let already_has_social_follow_up = reused_materialized_message
            && thread_has_session_reply_after(
                &channel_messages,
                &thread_root_message_id,
                &message.message_id,
            );
        if should_wake_thread {
            if already_has_social_follow_up {
                return Ok(());
            }
            if let Some(run) = self.drive_channel_after_public_message(&message).await? {
                let _ = self
                    .channel_service
                    .upsert_thread_state(channel_id, &thread_root_message_id, |existing| {
                        let mut state = existing.ok_or_else(|| {
                            anyhow!("missing thread state for {thread_root_message_id}")
                        })?;
                        state.owner_session_id = Some(run.session_id.clone());
                        Ok(state)
                    })
                    .await;
            }
        }
        Ok(())
    }

    pub(crate) fn spawn_channel_lease_worker(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            state.channel_lease_worker_loop().await;
        })
    }

    pub(crate) async fn restore_channel_lease_worker_on_boot(&self) -> Result<()> {
        self.channel_service.notify().notify_waiters();
        Ok(())
    }

    async fn channel_lease_worker_loop(self: Arc<Self>) {
        loop {
            match self.channel_lease_step().await {
                Ok(Some(deadline_ms)) => {
                    let wait = deadline_ms.saturating_sub(now_ms()).max(1);
                    tokio::select! {
                        _ = self.channel_service.notify().notified() => {}
                        _ = sleep_until(Instant::now() + Duration::from_millis(wait)) => {}
                    }
                }
                Ok(None) => {
                    self.channel_service.notify().notified().await;
                }
                Err(error) => {
                    error!(error = ?error, "channel lease worker error");
                    tokio::select! {
                        _ = self.channel_service.notify().notified() => {}
                        _ = sleep_until(Instant::now() + Duration::from_millis(Self::CHANNEL_LEASE_ERROR_RETRY_BACKOFF_MS)) => {}
                    }
                }
            }
        }
    }

    async fn channel_lease_step(self: &Arc<Self>) -> Result<Option<u64>> {
        let snapshot = self
            .channel_service
            .lease_scheduler_snapshot(now_ms())
            .await;
        for expired in snapshot.expired {
            self.process_expired_channel_lease(&expired.channel_id, &expired.turn_id)
                .await?;
        }
        Ok(self
            .channel_service
            .lease_scheduler_snapshot(now_ms())
            .await
            .next_expiry_ms)
    }

    async fn process_expired_channel_lease(
        self: &Arc<Self>,
        channel_id: &str,
        turn_id: &str,
    ) -> Result<()> {
        let transition_lock = self.channel_turn_transition_lock(channel_id).await;
        let transition_guard = transition_lock.lock().await;
        let channel = self.channel_service.get_channel(channel_id).await?;
        let mut leases = self.channel_leases_map(channel_id).await?;
        let Some(mut lease) = leases.remove(turn_id) else {
            return Ok(());
        };
        let thread_root_message_id = lease.thread_root_message_id.clone();
        if lease.expires_at_ms > now_ms() {
            leases.insert(lease.turn_id.clone(), lease);
            self.replace_channel_leases(channel_id, leases).await?;
            return Ok(());
        }

        let mut exclude_session_id = Some(lease.holder_session_id.clone());
        let mut active_run_still_running = false;
        if let Some(active_run_id) = lease.active_run_id.clone() {
            match self.run_record(&active_run_id).await {
                Ok(record) if record.view.status.is_terminal() => {
                    drop(leases);
                    drop(transition_guard);
                    self.settle_channel_delivery_run(&active_run_id).await?;
                    return Ok(());
                }
                Ok(_) => {
                    active_run_still_running = true;
                }
                Err(_) => {
                    lease.active_run_id = None;
                }
            }
        }

        let thread_messages = self
            .channel_service
            .list_messages(channel_id, Some(&thread_root_message_id))
            .await?;
        let active_run_id_before_handoff = lease.active_run_id.clone();
        let active_lease_before_handoff = active_run_still_running.then(|| lease.clone());
        let current_human_origin_message_id = lease
            .current_human_origin_message_id
            .clone()
            .or_else(|| {
                resolve_human_origin_message_id(&thread_messages, &lease.origin_message_id, None)
            })
            .unwrap_or_else(|| lease.origin_message_id.clone());
        let pending_human_origin = lease
            .last_human_message_id
            .clone()
            .filter(|message_id| message_id != &current_human_origin_message_id);
        if let Some(next_human_origin) = pending_human_origin {
            lease.current_human_origin_message_id = Some(next_human_origin.clone());
            lease.origin_message_id = next_human_origin;
            lease.remaining_reply_budget =
                channel.autonomy_policy.max_agent_replies_per_human_message;
            lease.agent_reply_count_since_last_human = 0;
            lease.queued_candidate_session_ids = self
                .rank_channel_session_candidates(
                    &channel,
                    &thread_root_message_id,
                    exclude_session_id.as_deref(),
                    &lease.last_human_priority_session_ids,
                    lease.last_human_message_id.as_deref(),
                )
                .await?;
        }

        let holder_session_id = if active_run_still_running {
            let mut handoff_lease = lease.clone();
            handoff_lease.active_run_id = None;
            match self
                .next_channel_candidate_from_queue(
                    &channel,
                    &mut handoff_lease,
                    exclude_session_id.as_deref(),
                )
                .await?
            {
                Some(holder_session_id) => {
                    lease = handoff_lease;
                    holder_session_id
                }
                None => {
                    let mut active_lease = active_lease_before_handoff.unwrap_or(lease);
                    active_lease.queued_candidate_session_ids =
                        handoff_lease.queued_candidate_session_ids;
                    active_lease.expires_at_ms =
                        now_ms() + channel.autonomy_policy.lease_timeout_ms;
                    leases.insert(active_lease.turn_id.clone(), active_lease);
                    self.replace_channel_leases(channel_id, leases).await?;
                    return Ok(());
                }
            }
        } else {
            let Some(holder_session_id) = self
                .next_channel_candidate_from_queue(
                    &channel,
                    &mut lease,
                    exclude_session_id.take().as_deref(),
                )
                .await?
            else {
                self.replace_channel_leases(channel_id, leases).await?;
                return Ok(());
            };
            holder_session_id
        };

        let superseded_turn_id = active_run_still_running.then(|| lease.turn_id.clone());
        let stale_run_id_to_cancel = active_run_still_running
            .then_some(active_run_id_before_handoff)
            .flatten();
        lease.turn_id = self.channel_service.next_turn_id();
        lease.holder_session_id = holder_session_id;
        lease.superseded_run_id = stale_run_id_to_cancel.clone();
        lease.superseded_turn_id = superseded_turn_id;
        lease.expires_at_ms = now_ms() + channel.autonomy_policy.lease_timeout_ms;
        let next_run = self
            .schedule_channel_delivery_for_lease(&channel, &mut lease, false, false)
            .await?;
        let handoff_started = next_run.is_some();
        if handoff_started {
            leases.insert(lease.turn_id.clone(), lease);
        }
        self.replace_channel_leases(channel_id, leases).await?;
        drop(transition_guard);
        if handoff_started
            && let Some(stale_run_id) = stale_run_id_to_cancel
            && let Err(error) = self.cancel_run(&stale_run_id).await
        {
            warn!(
                channel_id = %channel_id,
                run_id = %stale_run_id,
                error = ?error,
                "failed to cancel stale channel-delivery run after handoff"
            );
        }
        Ok(())
    }

    async fn recover_orphaned_human_channel_messages(self: &Arc<Self>) -> Result<()> {
        let channels = self.list_channels(None).await?;
        for channel in channels {
            let leases = self.channel_leases_map(&channel.summary.channel_id).await?;
            let messages = self
                .channel_service
                .list_messages(&channel.summary.channel_id, None)
                .await?;
            let mut latest_human_by_thread = BTreeMap::<String, crate::ChannelMessageView>::new();
            for message in messages
                .into_iter()
                .filter(|message| message.sender_session_id.is_none())
            {
                let thread_root = message
                    .thread_root_message_id
                    .clone()
                    .unwrap_or_else(|| message.message_id.clone());
                latest_human_by_thread.insert(thread_root, message);
            }
            for (thread_root_message_id, message) in latest_human_by_thread {
                if thread_has_lease(&leases, &thread_root_message_id) {
                    continue;
                }
                let thread_messages = self
                    .channel_service
                    .list_messages(&channel.summary.channel_id, Some(&thread_root_message_id))
                    .await?;
                let already_answered = thread_messages.iter().any(|candidate| {
                    candidate.sender_session_id.is_some()
                        && candidate.created_at_ms >= message.created_at_ms
                });
                if already_answered {
                    continue;
                }
                let _ = self.drive_channel_after_public_message(&message).await?;
            }
        }
        Ok(())
    }

    async fn recover_orphaned_channel_delivery_leases(self: &Arc<Self>) -> Result<()> {
        let channels = self.list_channels(None).await?;
        for channel in channels {
            let transition_lock = self
                .channel_turn_transition_lock(&channel.summary.channel_id)
                .await;
            let transition_guard = transition_lock.lock().await;
            let mut leases = self.channel_leases_map(&channel.summary.channel_id).await?;
            let mut changed = false;
            let mut run_ids_to_cancel_after_unlock = std::collections::BTreeSet::new();
            for member in channel
                .members
                .iter()
                .filter(|member| member.member_kind == crate::ChannelMemberKind::Session)
            {
                let Some(session_id) = member.session_id.as_deref() else {
                    continue;
                };
                for run in self.list_runs(Some(session_id)).await? {
                    if run.kind != DaemonRunKind::ChannelDelivery || run.status.is_terminal() {
                        continue;
                    }
                    let Some(metadata) = run.input_metadata.as_ref() else {
                        continue;
                    };
                    if metadata.get("channel_id").and_then(Value::as_str)
                        != Some(channel.summary.channel_id.as_str())
                    {
                        continue;
                    }
                    let Some(thread_root_message_id) = metadata
                        .get("thread_root_message_id")
                        .and_then(Value::as_str)
                    else {
                        continue;
                    };
                    if leases.values().any(|lease| {
                        lease.thread_root_message_id == thread_root_message_id
                            && lease.active_run_id.as_deref() == Some(run.run_id.as_str())
                    }) {
                        continue;
                    }
                    let Some(origin_message_id) =
                        metadata.get("origin_message_id").and_then(Value::as_str)
                    else {
                        continue;
                    };
                    let Some(turn_id) = metadata.get("turn_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let superseded_run_id = metadata
                        .get("superseded_run_id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let superseded_turn_id = metadata
                        .get("superseded_turn_id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    if channel_thread_has_superseding_delivery_lease(
                        &leases,
                        thread_root_message_id,
                        &run.run_id,
                        turn_id,
                    ) {
                        run_ids_to_cancel_after_unlock.insert(run.run_id.clone());
                        continue;
                    }
                    if superseded_run_id.is_some() || superseded_turn_id.is_some() {
                        let previous_lease_count = leases.len();
                        leases.retain(|_, lease| {
                            lease.thread_root_message_id != thread_root_message_id
                                || (superseded_run_id.as_deref().is_none_or(|run_id| {
                                    lease.active_run_id.as_deref() != Some(run_id)
                                }) && superseded_turn_id
                                    .as_deref()
                                    .is_none_or(|turn_id| lease.turn_id != turn_id))
                        });
                        changed |= leases.len() != previous_lease_count;
                        if let Some(superseded_run_id) = superseded_run_id.as_deref()
                            && superseded_run_id != run.run_id
                        {
                            run_ids_to_cancel_after_unlock.insert(superseded_run_id.to_string());
                        }
                    }
                    if active_channel_lease_count_for_thread(&leases, thread_root_message_id)
                        >= channel.autonomy_policy.max_parallel_public_speakers
                    {
                        continue;
                    }
                    let Ok(origin_message) = self
                        .channel_service
                        .get_message(&channel.summary.channel_id, origin_message_id)
                        .await
                    else {
                        continue;
                    };
                    let thread_messages = self
                        .channel_service
                        .list_messages(&channel.summary.channel_id, Some(thread_root_message_id))
                        .await?;
                    let latest_human_message = thread_messages
                        .iter()
                        .rev()
                        .find(|message| message.sender_session_id.is_none());
                    let last_human_message_id =
                        latest_human_message.map(|message| message.message_id.clone());
                    let last_human_priority_session_ids =
                        if let Some(message) = latest_human_message {
                            self.effective_priority_session_ids_for_human_message(
                                &channel, message, None,
                            )
                            .await?
                        } else {
                            Vec::new()
                        };
                    let agent_reply_count_since_last_human = latest_human_message
                        .map(|message| {
                            thread_messages
                                .iter()
                                .filter(|candidate| {
                                    candidate.sender_session_id.is_some()
                                        && candidate.created_at_ms >= message.created_at_ms
                                })
                                .count() as u32
                        })
                        .unwrap_or(0);
                    let queued_candidate_session_ids = self
                        .rank_channel_session_candidates(
                            &channel,
                            thread_root_message_id,
                            Some(session_id),
                            &last_human_priority_session_ids,
                            last_human_message_id.as_deref(),
                        )
                        .await?;
                    leases.insert(
                        turn_id.to_string(),
                        crate::ChannelTurnLeaseView {
                            turn_id: turn_id.to_string(),
                            channel_id: channel.summary.channel_id.clone(),
                            thread_root_message_id: thread_root_message_id.to_string(),
                            origin_message_id: origin_message.message_id.clone(),
                            holder_session_id: session_id.to_string(),
                            active_run_id: Some(run.run_id.clone()),
                            superseded_run_id,
                            superseded_turn_id,
                            remaining_reply_budget: channel
                                .autonomy_policy
                                .max_agent_replies_per_human_message
                                .saturating_sub(agent_reply_count_since_last_human),
                            expires_at_ms: now_ms() + channel.autonomy_policy.lease_timeout_ms,
                            queued_candidate_session_ids,
                            current_human_origin_message_id: resolve_human_origin_message_id(
                                &thread_messages,
                                origin_message_id,
                                metadata
                                    .get("human_origin_message_id")
                                    .and_then(Value::as_str),
                            ),
                            last_human_message_id,
                            last_human_priority_session_ids,
                            agent_reply_count_since_last_human,
                        },
                    );
                    changed = true;
                }
            }
            if changed {
                self.replace_channel_leases(&channel.summary.channel_id, leases)
                    .await?;
            }
            drop(transition_guard);
            for run_id in run_ids_to_cancel_after_unlock {
                if let Err(error) = self.cancel_run(&run_id).await {
                    warn!(
                        channel_id = %channel.summary.channel_id,
                        run_id = %run_id,
                        error = ?error,
                        "failed to cancel run superseded during channel lease recovery"
                    );
                }
            }
        }
        Ok(())
    }

    async fn schedule_channel_delivery_for_lease(
        self: &Arc<Self>,
        channel: &crate::ChannelView,
        lease: &mut crate::ChannelTurnLeaseView,
        autonomous: bool,
        new_topic: bool,
    ) -> Result<Option<RunView>> {
        if lease.remaining_reply_budget == 0 || lease.holder_session_id.is_empty() {
            return Ok(None);
        }
        let session_id = lease.holder_session_id.clone();
        if self
            .list_runs(Some(&session_id))
            .await?
            .into_iter()
            .any(|run| {
                !run.status.is_terminal()
                    && run.kind == DaemonRunKind::ChannelDelivery
                    && run
                        .input_metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get("thread_root_message_id"))
                        .and_then(Value::as_str)
                        == Some(lease.thread_root_message_id.as_str())
            })
        {
            return Ok(None);
        }
        let agent_id = self.agent_id_for_session(&session_id).await?;
        let origin_message = self
            .channel_service
            .get_message(&channel.summary.channel_id, &lease.origin_message_id)
            .await?;
        let thread_messages = self
            .channel_service
            .list_messages(
                &channel.summary.channel_id,
                Some(&lease.thread_root_message_id),
            )
            .await?;
        let human_origin_message_id = lease
            .current_human_origin_message_id
            .clone()
            .or_else(|| {
                resolve_human_origin_message_id(
                    &thread_messages,
                    &lease.origin_message_id,
                    lease.last_human_message_id.as_deref(),
                )
            })
            .unwrap_or_else(|| lease.origin_message_id.clone());
        let human_origin_message = self
            .channel_service
            .get_message(&channel.summary.channel_id, &human_origin_message_id)
            .await?;
        let policy = self.effective_session_route_policy(&session_id).await?;
        let (resolved_provider, resolved_model) = {
            let _runtime_config_snapshot = self.runtime_config_service.snapshot_guard().await;
            let (resolved_provider, resolved_generation) = self
                .resolve_generation_route_for_session(
                    &session_id,
                    policy.provider.clone(),
                    policy.generation.clone(),
                )
                .await?;
            let resolved_model = resolved_generation
                .as_ref()
                .and_then(|generation| generation.model.clone());
            (resolved_provider, resolved_model)
        };
        let request = crate::ChannelDeliveryRunRequest {
            channel_id: channel.summary.channel_id.clone(),
            thread_root_message_id: lease.thread_root_message_id.clone(),
            origin_message_id: lease.origin_message_id.clone(),
            human_origin_message_id,
            turn_id: lease.turn_id.clone(),
            addressed_member_ids: human_origin_message.addressed_member_ids.clone(),
            provider: resolved_provider.clone(),
            model: resolved_model.clone(),
            autonomous,
            autonomous_new_topic: new_topic,
        };
        let mut request_summary = crate::summarize_channel_delivery_request(
            &request,
            &origin_message.sender.id,
            Some(&origin_message.output.content),
        );
        request_summary.provider = resolved_provider;
        request_summary.model = resolved_model;
        let record = RunRecord {
            view: RunView {
                run_id: self.next_run_id(),
                session_id: session_id.clone(),
                agent_id: agent_id.0.clone(),
                kind: DaemonRunKind::ChannelDelivery,
                status: DaemonRunStatus::Queued,
                submitted_at_ms: now_ms(),
                updated_at_ms: now_ms(),
                started_at_ms: None,
                finished_at_ms: None,
                queued_position: None,
                request: request_summary,
                input_attachments: channel_delivery_attachment_refs(
                    &channel,
                    &origin_message,
                    &human_origin_message,
                    self,
                )?,
                input_metadata: Some(json!({
                    "channel_id": request.channel_id,
                    "thread_root_message_id": request.thread_root_message_id,
                    "origin_message_id": request.origin_message_id,
                    "human_origin_message_id": request.human_origin_message_id,
                    "turn_id": request.turn_id,
                    "addressed_member_ids": request.addressed_member_ids,
                    "superseded_run_id": lease.superseded_run_id.clone(),
                    "superseded_turn_id": lease.superseded_turn_id.clone(),
                })),
                pending_approval_ids: Vec::new(),
                pending_approvals: Vec::new(),
                pending_question_ids: Vec::new(),
                pending_questions: Vec::new(),
                outputs: Vec::new(),
                deliveries: Vec::new(),
                error: None,
            },
            reply_targets: Vec::new(),
            payload: RunRequestPayload::ChannelDelivery { request },
        };
        let view = self.schedule_run(record).await?;
        lease.active_run_id = Some(view.run_id.clone());
        Ok(Some(view))
    }

    pub(super) async fn execute_channel_delivery_run(
        self: &Arc<Self>,
        run_id: &str,
        session_id: &str,
        agent_id: &AgentId,
        request: crate::ChannelDeliveryRunRequest,
        provider: Option<String>,
        model: Option<String>,
    ) -> Result<ManagedAgentSnapshot> {
        let input = self
            .build_channel_input_envelope(session_id, &request)
            .await?;
        let mut generation = self
            .effective_session_route_policy(session_id)
            .await?
            .generation
            .unwrap_or_default();
        if model.is_some() || request.model.is_some() {
            generation.model = model.clone().or(request.model.clone());
        }
        self.orchestrator
            .submit_input_for_run(
                agent_id,
                input,
                generation,
                Some(run_id.to_string()),
                provider.or(request.provider.clone()),
                model.or(request.model.clone()),
            )
            .await
    }

    async fn build_channel_input_envelope(
        &self,
        session_id: &str,
        request: &crate::ChannelDeliveryRunRequest,
    ) -> Result<InputEnvelope> {
        let channel = self
            .channel_service
            .get_channel(&request.channel_id)
            .await?;
        let origin_message = self
            .channel_service
            .get_message(&request.channel_id, &request.origin_message_id)
            .await?;
        let thread_messages = self
            .channel_service
            .list_messages(&request.channel_id, Some(&request.thread_root_message_id))
            .await?;
        let human_origin_message_id = resolve_human_origin_message_id(
            &thread_messages,
            &request.origin_message_id,
            normalized_human_origin_message_id(request),
        )
        .unwrap_or_else(|| request.origin_message_id.clone());
        let human_origin_message = self
            .channel_service
            .get_message(&request.channel_id, &human_origin_message_id)
            .await?;
        // For autonomous turns, build the situational briefing (date, the member's other
        // rooms, the channel's board of topics, ambient pulse) so the agent sees the whole
        // room, not just the seeded thread. Human turns keep the focused thread view.
        let autonomous_briefing = if request.autonomous {
            Some(
                self.build_channel_autonomous_briefing(&channel, session_id)
                    .await?,
            )
        } else {
            None
        };
        let thread_context = render_channel_thread_context(
            &channel,
            session_id,
            &origin_message,
            &human_origin_message,
            &thread_messages,
            request.autonomous,
            request.autonomous_new_topic,
            autonomous_briefing.as_deref(),
        );
        let attachments = channel_delivery_attachment_refs(
            &channel,
            &origin_message,
            &human_origin_message,
            self,
        )?;
        let reply = ReplyHandle {
            plugin: "daemon".to_string(),
            address: session_id.to_string(),
        };
        Ok(InputEnvelope {
            source: SourceRef {
                plugin: "daemon".to_string(),
                kind: "channel".to_string(),
            },
            conversation: ConversationKey {
                session_id: session_id.to_string(),
                thread_id: None,
            },
            actor: origin_message.sender.clone(),
            payload: InputPayload::Text {
                content: thread_context,
            },
            attachments,
            metadata: json!({
                "channel_id": request.channel_id,
                "thread_root_message_id": request.thread_root_message_id,
                "origin_message_id": request.origin_message_id,
                "human_origin_message_id": human_origin_message_id,
                "turn_id": request.turn_id,
            }),
            reply_targets: vec![reply.clone()],
            reply: Some(reply),
        })
    }

    /// Builds the autonomous-turn "situational briefing": when it is, the member's other
    /// rooms, the channel's board of live topics, and the ambient pulse — so an agent acts on
    /// a view of the whole room instead of reflexively answering the previous line.
    async fn build_channel_autonomous_briefing(
        &self,
        channel: &crate::ChannelView,
        session_id: &str,
    ) -> Result<String> {
        let now = now_ms();
        let mut lines: Vec<String> = vec![format!("Right now it's {}.", channel_moment_label())];

        // The member's other rooms — awareness only; they cannot post there from this turn.
        let mut siblings: Vec<String> = Vec::new();
        for other in self.channel_service.list_channels(None).await {
            if other.summary.channel_id == channel.summary.channel_id {
                continue;
            }
            let is_member = other
                .members
                .iter()
                .any(|member| member.session_id.as_deref() == Some(session_id));
            if !is_member {
                continue;
            }
            let purpose = other
                .summary
                .purpose
                .clone()
                .or_else(|| other.summary.description.clone())
                .unwrap_or_default();
            let purpose = channel_briefing_snippet(&purpose, 80);
            if purpose.is_empty() {
                siblings.push(format!("- #{}", other.summary.title));
            } else {
                siblings.push(format!("- #{}: {}", other.summary.title, purpose));
            }
        }
        if !siblings.is_empty() {
            lines.push(
                "Your other rooms (context you can draw on, though you can't post there from here):"
                    .to_string(),
            );
            lines.extend(siblings);
        }

        let messages = self
            .channel_service
            .list_messages(&channel.summary.channel_id, None)
            .await?;

        // The board: each top-level topic with its gist, who's active, and how stale it is.
        let mut board: Vec<(u64, String)> = Vec::new();
        for root in messages
            .iter()
            .filter(|message| message.thread_root_message_id.is_none())
        {
            let in_topic: Vec<&crate::ChannelMessageView> = messages
                .iter()
                .filter(|message| {
                    message.message_id == root.message_id
                        || message.thread_root_message_id.as_deref()
                            == Some(root.message_id.as_str())
                })
                .collect();
            let last_activity = in_topic
                .iter()
                .map(|message| message.created_at_ms)
                .max()
                .unwrap_or(root.created_at_ms);
            let mut participants: Vec<String> = Vec::new();
            for message in &in_topic {
                if message.sender_session_id.is_some() {
                    let name = message
                        .sender
                        .display_name
                        .clone()
                        .unwrap_or_else(|| message.sender.id.clone());
                    if !participants.contains(&name) {
                        participants.push(name);
                    }
                }
            }
            let who = if participants.is_empty() {
                "no replies yet".to_string()
            } else {
                participants.join(", ")
            };
            let mine = in_topic
                .iter()
                .any(|message| message.sender_session_id.as_deref() == Some(session_id));
            let mark = if mine { " · you're in this one" } else { "" };
            board.push((
                last_activity,
                format!(
                    "- {} · {} · {}{} · [{}]",
                    channel_briefing_snippet(&root.output.content, 90),
                    who,
                    channel_briefing_age(now, last_activity),
                    mark,
                    root.message_id
                ),
            ));
        }
        board.sort_by(|a, b| b.0.cmp(&a.0)); // most-recently-active first
        board.truncate(Self::CHANNEL_HEARTBEAT_BOARD_TOPICS);
        if !board.is_empty() {
            lines.push("Topics in play here, most recent first:".to_string());
            lines.extend(board.into_iter().map(|(_, line)| line));
        }

        // Ambient pulse: the last few lines, explicitly not a to-do.
        let pulse: Vec<&crate::ChannelMessageView> = messages
            .iter()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if !pulse.is_empty() {
            lines.push(
                "Latest lines (just for the vibe, you don't owe the last one a reply):"
                    .to_string(),
            );
            for message in pulse {
                let sender = message
                    .sender
                    .display_name
                    .clone()
                    .unwrap_or_else(|| message.sender.id.clone());
                let you = if message.sender_session_id.as_deref() == Some(session_id) {
                    " (you)"
                } else {
                    ""
                };
                lines.push(format!(
                    "  [{}] {sender}{you}: {}",
                    message.message_id,
                    channel_briefing_snippet(&message.output.content, 140)
                ));
            }
        }

        Ok(lines.join("\n"))
    }

    async fn rank_channel_session_candidates(
        &self,
        channel: &crate::ChannelView,
        thread_root_message_id: &str,
        exclude_session_id: Option<&str>,
        priority_session_ids: &[String],
        human_origin_message_id: Option<&str>,
    ) -> Result<Vec<String>> {
        let now = now_ms();
        let thread_messages = self
            .channel_service
            .list_messages(&channel.summary.channel_id, Some(thread_root_message_id))
            .await?;
        let (commented_session_ids, reacted_session_ids) =
            thread_social_participants_since_human(&thread_messages, human_origin_message_id);
        let mut ranked = Vec::new();
        for member in channel.members.iter().filter(|member| {
            member.member_kind == crate::ChannelMemberKind::Session
                && !member.muted
                && member.participation_mode != crate::ChannelParticipationMode::ManualOnly
        }) {
            let Some(session_id) = member.session_id.as_deref() else {
                continue;
            };
            if exclude_session_id == Some(session_id) {
                continue;
            }
            let priority_rank = priority_session_ids
                .iter()
                .position(|candidate| candidate == session_id);
            let last_message_at = thread_messages
                .iter()
                .rev()
                .find(|message| message.sender_session_id.as_deref() == Some(session_id))
                .map(|message| message.created_at_ms);
            if last_message_at.is_some_and(|timestamp| {
                now.saturating_sub(timestamp) < channel.autonomy_policy.member_cooldown_ms
            }) && priority_rank.is_none()
            {
                continue;
            }
            let preferred_bias = match member.participation_mode {
                crate::ChannelParticipationMode::PreferSelected if priority_rank.is_some() => 0_u8,
                crate::ChannelParticipationMode::PreferSelected => 1,
                crate::ChannelParticipationMode::AlwaysListen => 1,
                crate::ChannelParticipationMode::SelectedOnly => 0,
                crate::ChannelParticipationMode::ManualOnly => unreachable!(),
            };
            let already_commented = commented_session_ids.contains(&session_id.to_string());
            let already_reacted = reacted_session_ids.contains(&session_id.to_string());
            let active_runs = self
                .list_runs(Some(session_id))
                .await?
                .into_iter()
                .filter(|run| !run.status.is_terminal())
                .count();
            ranked.push((
                priority_rank.unwrap_or(usize::MAX),
                already_commented,
                already_reacted,
                preferred_bias,
                last_message_at
                    .map(|timestamp| u64::MAX - timestamp)
                    .unwrap_or(u64::MAX),
                active_runs,
                session_id.to_string(),
            ));
        }
        ranked.sort();
        Ok(ranked
            .into_iter()
            .map(|(_, _, _, _, _, _, session_id)| session_id)
            .collect())
    }

    async fn channel_leases_map(
        &self,
        channel_id: &str,
    ) -> Result<BTreeMap<String, crate::ChannelTurnLeaseView>> {
        Ok(self
            .channel_service
            .list_leases(channel_id)
            .await?
            .into_iter()
            .map(|lease| (lease.turn_id.clone(), lease))
            .collect())
    }

    async fn channel_turn_transition_lock(&self, channel_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.channel_turn_transition_locks.lock().await;
        locks
            .entry(channel_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    async fn replace_channel_leases(
        &self,
        channel_id: &str,
        leases: BTreeMap<String, crate::ChannelTurnLeaseView>,
    ) -> Result<()> {
        self.channel_service
            .replace_leases(channel_id, leases.into_values().collect())
            .await
    }

    async fn next_channel_candidate_from_queue(
        &self,
        channel: &crate::ChannelView,
        lease: &mut crate::ChannelTurnLeaseView,
        exclude_session_id: Option<&str>,
    ) -> Result<Option<String>> {
        while let Some(candidate_session_id) = lease.queued_candidate_session_ids.first().cloned() {
            lease.queued_candidate_session_ids.remove(0);
            if exclude_session_id == Some(candidate_session_id.as_str()) {
                continue;
            }
            if !self
                .channel_service
                .channel_has_session(&channel.summary.channel_id, &candidate_session_id)
                .await
            {
                continue;
            }
            let Some(member) = channel
                .members
                .iter()
                .find(|member| member.session_id.as_deref() == Some(candidate_session_id.as_str()))
            else {
                continue;
            };
            if member.muted
                || member.participation_mode == crate::ChannelParticipationMode::ManualOnly
            {
                continue;
            }
            let has_active_turn = self
                .list_runs(Some(&candidate_session_id))
                .await?
                .into_iter()
                .any(|run| {
                    !run.status.is_terminal()
                        && run.kind == DaemonRunKind::ChannelDelivery
                        && run
                            .input_metadata
                            .as_ref()
                            .and_then(|metadata| metadata.get("thread_root_message_id"))
                            .and_then(Value::as_str)
                            == Some(lease.thread_root_message_id.as_str())
                });
            if has_active_turn {
                continue;
            }
            return Ok(Some(candidate_session_id));
        }
        Ok(None)
    }

    async fn effective_priority_session_ids_for_human_message(
        &self,
        channel: &crate::ChannelView,
        message: &crate::ChannelMessageView,
        existing_lease: Option<&crate::ChannelTurnLeaseView>,
    ) -> Result<Vec<String>> {
        let explicit = prioritized_channel_session_ids(channel, &message.addressed_member_ids);
        if !explicit.is_empty() {
            return Ok(explicit);
        }

        let message_session_ids = self
            .message_structural_priority_session_ids(channel, message)
            .await?;
        if !message_session_ids.is_empty() {
            return Ok(message_session_ids);
        }

        if let Some(lease) = existing_lease
            && !lease.last_human_priority_session_ids.is_empty()
        {
            return Ok(lease.last_human_priority_session_ids.clone());
        }

        Ok(Vec::new())
    }

    async fn message_structural_priority_session_ids(
        &self,
        channel: &crate::ChannelView,
        message: &crate::ChannelMessageView,
    ) -> Result<Vec<String>> {
        if let Some(session_id) = self
            .reply_target_session_id(channel, message.reply_to_message_id.as_deref())
            .await?
        {
            return Ok(vec![session_id]);
        }

        let thread_root_message_id = message
            .thread_root_message_id
            .clone()
            .unwrap_or_else(|| message.message_id.clone());
        let thread_messages = self
            .channel_service
            .list_messages(&channel.summary.channel_id, Some(&thread_root_message_id))
            .await?;
        if let Some(session_id) =
            latest_agent_sender_before_message(&thread_messages, &message.message_id)
        {
            return Ok(vec![session_id]);
        }

        if message.thread_root_message_id.is_none() {
            let channel_messages = self
                .channel_service
                .list_messages(&channel.summary.channel_id, None)
                .await?;
            if let Some(session_id) =
                previous_agent_sender_in_channel(&channel_messages, &message.message_id)
            {
                return Ok(vec![session_id]);
            }
        }

        Ok(Vec::new())
    }

    async fn reply_target_session_id(
        &self,
        channel: &crate::ChannelView,
        reply_to_message_id: Option<&str>,
    ) -> Result<Option<String>> {
        let Some(reply_to_message_id) = reply_to_message_id else {
            return Ok(None);
        };
        let Ok(parent) = self
            .channel_service
            .get_message(&channel.summary.channel_id, reply_to_message_id)
            .await
        else {
            return Ok(None);
        };
        let Some(session_id) = parent.sender_session_id else {
            return Ok(None);
        };
        Ok(channel
            .members
            .iter()
            .find(|member| member.session_id.as_deref() == Some(session_id.as_str()))
            .map(|_| session_id))
    }
}

fn normalized_channel_input_items(
    content: Option<&str>,
    input_items: &[SubmitInputItemRequest],
) -> Result<Vec<SubmitInputItemRequest>> {
    if !input_items.is_empty() {
        crate::api::validate_submit_input_items(input_items)?;
        return Ok(input_items.to_vec());
    }
    let content = content.unwrap_or_default().trim();
    anyhow::ensure!(
        !content.is_empty(),
        "channel messages require content or input_items"
    );
    Ok(vec![SubmitInputItemRequest::Text {
        text: content.to_string(),
    }])
}

fn validate_channel_autonomy_policy(policy: &crate::ChannelAutonomyPolicy) -> Result<()> {
    anyhow::ensure!(
        policy.max_parallel_public_speakers > 0,
        "max_parallel_public_speakers must be greater than zero"
    );
    anyhow::ensure!(
        policy.max_agent_replies_per_human_message > 0,
        "max_agent_replies_per_human_message must be greater than zero"
    );
    anyhow::ensure!(
        policy.lease_timeout_ms > 0,
        "lease_timeout_ms must be greater than zero"
    );
    anyhow::ensure!(
        policy.max_pending_stimuli > 0,
        "max_pending_stimuli must be greater than zero"
    );
    Ok(())
}

fn prioritized_channel_session_ids(
    channel: &crate::ChannelView,
    addressed_member_ids: &[String],
) -> Vec<String> {
    let mut prioritized = Vec::new();
    for member_id in addressed_member_ids {
        let Some(session_id) = channel
            .members
            .iter()
            .find(|member| member.member_id == *member_id)
            .and_then(|member| member.session_id.clone())
        else {
            continue;
        };
        if !prioritized.contains(&session_id) {
            prioritized.push(session_id);
        }
    }
    prioritized
}

fn latest_agent_sender_before_message(
    thread_messages: &[crate::ChannelMessageView],
    message_id: &str,
) -> Option<String> {
    let index = thread_messages
        .iter()
        .position(|message| message.message_id == message_id)?;
    thread_messages[..index]
        .iter()
        .rev()
        .find_map(|message| message.sender_session_id.clone())
}

fn previous_agent_sender_in_channel(
    channel_messages: &[crate::ChannelMessageView],
    message_id: &str,
) -> Option<String> {
    let index = channel_messages
        .iter()
        .position(|message| message.message_id == message_id)?;
    channel_messages[..index]
        .iter()
        .rev()
        .find_map(|message| message.sender_session_id.clone())
}

fn active_channel_lease_count_for_thread(
    leases: &BTreeMap<String, crate::ChannelTurnLeaseView>,
    thread_root_message_id: &str,
) -> u32 {
    leases
        .values()
        .filter(|lease| {
            lease.thread_root_message_id == thread_root_message_id && lease.active_run_id.is_some()
        })
        .count() as u32
}

fn active_channel_lease_count_for_human_origin(
    leases: &BTreeMap<String, crate::ChannelTurnLeaseView>,
    thread_root_message_id: &str,
    human_origin_message_id: &str,
) -> u32 {
    leases
        .values()
        .filter(|lease| {
            lease.thread_root_message_id == thread_root_message_id
                && lease.active_run_id.is_some()
                && lease.current_human_origin_message_id.as_deref() == Some(human_origin_message_id)
        })
        .count() as u32
}

fn active_channel_holder_session_ids_for_thread(
    leases: &BTreeMap<String, crate::ChannelTurnLeaseView>,
    thread_root_message_id: &str,
) -> Vec<String> {
    let mut session_ids = leases
        .values()
        .filter(|lease| {
            lease.thread_root_message_id == thread_root_message_id && lease.active_run_id.is_some()
        })
        .map(|lease| lease.holder_session_id.clone())
        .collect::<Vec<_>>();
    session_ids.sort();
    session_ids.dedup();
    session_ids
}

fn channel_thread_has_superseding_delivery_lease(
    leases: &BTreeMap<String, crate::ChannelTurnLeaseView>,
    thread_root_message_id: &str,
    run_id: &str,
    turn_id: &str,
) -> bool {
    leases.values().any(|lease| {
        lease.thread_root_message_id == thread_root_message_id
            && (lease.superseded_run_id.as_deref() == Some(run_id)
                || lease.superseded_turn_id.as_deref() == Some(turn_id))
    })
}

fn thread_has_lease(
    leases: &BTreeMap<String, crate::ChannelTurnLeaseView>,
    thread_root_message_id: &str,
) -> bool {
    leases
        .values()
        .any(|lease| lease.thread_root_message_id == thread_root_message_id)
}

fn agent_reply_count_since_human_origin(
    thread_messages: &[crate::ChannelMessageView],
    human_origin_message_id: &str,
) -> u32 {
    let Some(origin_index) = thread_messages
        .iter()
        .position(|message| message.message_id == human_origin_message_id)
    else {
        return 0;
    };
    thread_messages[origin_index + 1..]
        .iter()
        .filter(|message| message.sender_session_id.is_some())
        .count() as u32
}

fn remaining_channel_reply_budget_for_human_origin(
    channel: &crate::ChannelView,
    thread_messages: &[crate::ChannelMessageView],
    leases: &BTreeMap<String, crate::ChannelTurnLeaseView>,
    thread_root_message_id: &str,
    human_origin_message_id: &str,
) -> u32 {
    channel
        .autonomy_policy
        .max_agent_replies_per_human_message
        .saturating_sub(agent_reply_count_since_human_origin(
            thread_messages,
            human_origin_message_id,
        ))
        .saturating_sub(active_channel_lease_count_for_human_origin(
            leases,
            thread_root_message_id,
            human_origin_message_id,
        ))
}

fn autonomous_root_quiet_period_deadline(
    channel: &crate::ChannelView,
    thread_states: &[crate::channels::ChannelThreadWorkStateView],
    now_ms: u64,
) -> Option<u64> {
    let latest_root_at_ms = thread_states
        .iter()
        .filter(|state| state.topic_kind != crate::channels::ChannelThreadTopicKind::Human)
        .filter_map(|state| {
            state
                .last_main_promotion_at_ms
                .or(Some(state.last_signal_at_ms))
        })
        .max()?;
    let deadline =
        latest_root_at_ms.saturating_add(channel.autonomy_policy.quiet_period_ms_after_root_post);
    (deadline > now_ms).then_some(deadline)
}

fn thread_topic_kind_for_stimulus(
    stimulus: &crate::channels::ChannelStimulusView,
) -> crate::channels::ChannelThreadTopicKind {
    match stimulus.kind {
        crate::channels::ChannelStimulusKind::ResultSummary
        | crate::channels::ChannelStimulusKind::SupersessionNotice => {
            crate::channels::ChannelThreadTopicKind::Summary
        }
        _ => crate::channels::ChannelThreadTopicKind::AutonomousRoot,
    }
}

fn thread_status_for_stimulus(
    stimulus: &crate::channels::ChannelStimulusView,
) -> crate::channels::ChannelThreadWorkStatus {
    match stimulus.kind {
        crate::channels::ChannelStimulusKind::ReviewCompleted => {
            crate::channels::ChannelThreadWorkStatus::Reviewing
        }
        crate::channels::ChannelStimulusKind::ResultSummary => {
            crate::channels::ChannelThreadWorkStatus::Completed
        }
        crate::channels::ChannelStimulusKind::SupersessionNotice => {
            crate::channels::ChannelThreadWorkStatus::Superseded
        }
        _ => crate::channels::ChannelThreadWorkStatus::Active,
    }
}

fn merge_thread_work_status(
    current: crate::channels::ChannelThreadWorkStatus,
    next: crate::channels::ChannelThreadWorkStatus,
) -> crate::channels::ChannelThreadWorkStatus {
    use crate::channels::ChannelThreadWorkStatus::{
        Active, Completed, Proposed, Reviewing, Superseded,
    };

    fn rank(status: &crate::channels::ChannelThreadWorkStatus) -> u8 {
        match status {
            Proposed => 0,
            Active => 1,
            Reviewing => 2,
            Completed => 3,
            Superseded => 4,
        }
    }

    if rank(&next) >= rank(&current) {
        next
    } else {
        current
    }
}

fn merge_recovered_thread_state(
    existing: &mut crate::channels::ChannelThreadWorkStateView,
    candidate: &crate::channels::ChannelThreadWorkStateView,
) {
    let candidate_is_newer = candidate.last_signal_at_ms >= existing.last_signal_at_ms;
    existing.status = merge_thread_work_status(existing.status.clone(), candidate.status.clone());
    existing.last_signal_at_ms = existing.last_signal_at_ms.max(candidate.last_signal_at_ms);
    existing.last_main_promotion_at_ms = max_optional_timestamp(
        existing.last_main_promotion_at_ms,
        candidate.last_main_promotion_at_ms,
    );
    if candidate_is_newer {
        existing.topic_kind = candidate.topic_kind.clone();
        existing.last_stimulus_id = candidate.last_stimulus_id.clone();
        existing.initiative_key = candidate
            .initiative_key
            .clone()
            .or(existing.initiative_key.clone());
        existing.source_kind = candidate
            .source_kind
            .clone()
            .or(existing.source_kind.clone());
        existing.source_ref = candidate.source_ref.clone().or(existing.source_ref.clone());
        if !candidate.metadata.is_null() {
            existing.metadata = candidate.metadata.clone();
        }
    } else {
        if existing.initiative_key.is_none() {
            existing.initiative_key = candidate.initiative_key.clone();
        }
        if existing.source_kind.is_none() {
            existing.source_kind = candidate.source_kind.clone();
        }
        if existing.source_ref.is_none() {
            existing.source_ref = candidate.source_ref.clone();
        }
        if existing.metadata.is_null() && !candidate.metadata.is_null() {
            existing.metadata = candidate.metadata.clone();
        }
    }
    for candidate_binding in &candidate.bindings {
        let already_present = existing.bindings.iter().any(|binding| {
            binding.binding_kind == candidate_binding.binding_kind
                && binding.binding_ref == candidate_binding.binding_ref
        });
        if !already_present {
            existing.bindings.push(candidate_binding.clone());
        }
    }
    for candidate_snapshot in &candidate.progress_snapshots {
        if let Some(existing_snapshot) = existing
            .progress_snapshots
            .iter_mut()
            .find(|snapshot| snapshot.progress_key == candidate_snapshot.progress_key)
        {
            if candidate_snapshot.updated_at_ms >= existing_snapshot.updated_at_ms {
                *existing_snapshot = candidate_snapshot.clone();
            }
        } else {
            existing.progress_snapshots.push(candidate_snapshot.clone());
        }
    }
}

fn max_optional_timestamp(current: Option<u64>, next: Option<u64>) -> Option<u64> {
    match (current, next) {
        (Some(current), Some(next)) => Some(current.max(next)),
        (Some(current), None) => Some(current),
        (None, Some(next)) => Some(next),
        (None, None) => None,
    }
}

fn find_materialized_stimulus_message(
    messages: &[crate::ChannelMessageView],
    stimulus_id: &str,
) -> Option<crate::ChannelMessageView> {
    messages
        .iter()
        .find(|message| {
            message.metadata.get("stimulus_id").and_then(Value::as_str) == Some(stimulus_id)
        })
        .cloned()
}

fn find_materialized_equivalent_stimulus<'a>(
    messages: &[crate::ChannelMessageView],
    stimuli: &'a [crate::channels::ChannelStimulusView],
    target: &'a crate::channels::ChannelStimulusView,
) -> Option<(
    &'a crate::channels::ChannelStimulusView,
    crate::ChannelMessageView,
)> {
    if let Some(message) = find_materialized_stimulus_message(messages, &target.stimulus_id) {
        return Some((target, message));
    }
    stimuli
        .iter()
        .filter(|candidate| {
            candidate.stimulus_id != target.stimulus_id
                && stimuli_materialize_equivalent(candidate, target)
        })
        .filter_map(|candidate| {
            find_materialized_stimulus_message(messages, &candidate.stimulus_id)
                .map(|message| (candidate, message))
        })
        .max_by(|(_, left_message), (_, right_message)| {
            left_message
                .created_at_ms
                .cmp(&right_message.created_at_ms)
                .then_with(|| left_message.message_id.cmp(&right_message.message_id))
        })
}

fn stimuli_materialize_equivalent(
    left: &crate::channels::ChannelStimulusView,
    right: &crate::channels::ChannelStimulusView,
) -> bool {
    left.channel_id == right.channel_id
        && left.scope == right.scope
        && left.thread_root_message_id == right.thread_root_message_id
        && left.visibility_hint == right.visibility_hint
        && left.kind == right.kind
        && left.content == right.content
        && left.addressed_member_ids == right.addressed_member_ids
        && left.sender_session_id == right.sender_session_id
        && left.sender_actor_id == right.sender_actor_id
        && left.sender_display_name == right.sender_display_name
        && left.source_kind == right.source_kind
        && left.source_ref == right.source_ref
        && left.dedupe_key == right.dedupe_key
        && left.progress_key == right.progress_key
        && left.metadata == right.metadata
}

fn canonical_thread_root_for_materialized_message(
    message: &crate::ChannelMessageView,
) -> Option<String> {
    message
        .metadata
        .get("canonical_thread_root_message_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| message.thread_root_message_id.clone())
        .or_else(|| Some(message.message_id.clone()))
}

fn recovered_thread_state_from_materialized_stimulus(
    channel_id: &str,
    thread_root_message_id: &str,
    stimulus: &crate::channels::ChannelStimulusView,
    message: &crate::ChannelMessageView,
) -> crate::channels::ChannelThreadWorkStateView {
    let mut progress_snapshots = Vec::new();
    if let (Some(progress_key), Some(source_kind), Some(source_ref)) = (
        stimulus.progress_key.clone(),
        stimulus.source_kind.clone(),
        stimulus.source_ref.clone(),
    ) {
        progress_snapshots.push(crate::channels::ChannelProgressSnapshotView {
            progress_key,
            source_kind,
            source_ref,
            latest_message_id: Some(message.message_id.clone()),
            updated_at_ms: message.created_at_ms,
            summary_digest: Some(message.output.content.clone()),
        });
    }
    crate::channels::ChannelThreadWorkStateView {
        channel_id: channel_id.to_string(),
        thread_root_message_id: thread_root_message_id.to_string(),
        topic_kind: thread_topic_kind_for_stimulus(stimulus),
        status: thread_status_for_stimulus(stimulus),
        owner_session_id: None,
        initiative_key: stimulus.dedupe_key.clone(),
        source_kind: stimulus.source_kind.clone(),
        source_ref: stimulus.source_ref.clone(),
        last_stimulus_id: Some(stimulus.stimulus_id.clone()),
        last_signal_at_ms: message.created_at_ms,
        last_main_promotion_at_ms: message
            .thread_root_message_id
            .is_none()
            .then_some(message.created_at_ms),
        bindings: Vec::new(),
        progress_snapshots,
        metadata: stimulus.metadata.clone(),
    }
}

fn thread_has_session_reply_after(
    messages: &[crate::ChannelMessageView],
    thread_root_message_id: &str,
    after_message_id: &str,
) -> bool {
    let Some(after_index) = messages
        .iter()
        .position(|message| message.message_id == after_message_id)
    else {
        return false;
    };
    messages.iter().skip(after_index + 1).any(|message| {
        message.sender_session_id.is_some()
            && (message.thread_root_message_id.as_deref() == Some(thread_root_message_id)
                || message.message_id == thread_root_message_id)
    })
}

fn thread_social_participants_since_human(
    thread_messages: &[crate::ChannelMessageView],
    human_origin_message_id: Option<&str>,
) -> (Vec<String>, Vec<String>) {
    let Some(human_origin_message_id) = human_origin_message_id else {
        return (Vec::new(), Vec::new());
    };
    let Some(origin_index) = thread_messages
        .iter()
        .position(|message| message.message_id == human_origin_message_id)
    else {
        return (Vec::new(), Vec::new());
    };
    let mut commented_session_ids = Vec::new();
    let mut reacted_session_ids = Vec::new();
    for reaction in &thread_messages[origin_index].reactions {
        for actor_id in &reaction.actor_ids {
            if !reacted_session_ids.contains(actor_id) {
                reacted_session_ids.push(actor_id.clone());
            }
        }
    }
    for message in &thread_messages[origin_index + 1..] {
        if let Some(session_id) = message.sender_session_id.as_ref()
            && !commented_session_ids.contains(session_id)
        {
            commented_session_ids.push(session_id.clone());
        }
        for reaction in &message.reactions {
            for actor_id in &reaction.actor_ids {
                if !reacted_session_ids.contains(actor_id) {
                    reacted_session_ids.push(actor_id.clone());
                }
            }
        }
    }
    (commented_session_ids, reacted_session_ids)
}

fn trim_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn normalized_human_origin_message_id(request: &crate::ChannelDeliveryRunRequest) -> Option<&str> {
    (!request.human_origin_message_id.trim().is_empty())
        .then_some(request.human_origin_message_id.as_str())
}

fn resolve_human_origin_message_id(
    thread_messages: &[crate::ChannelMessageView],
    origin_message_id: &str,
    stored_human_origin_message_id: Option<&str>,
) -> Option<String> {
    if let Some(human_origin_message_id) = stored_human_origin_message_id
        && thread_messages
            .iter()
            .any(|message| message.message_id == human_origin_message_id)
    {
        return Some(human_origin_message_id.to_string());
    }
    let origin_index = thread_messages
        .iter()
        .position(|message| message.message_id == origin_message_id)?;
    let origin_message = &thread_messages[origin_index];
    if origin_message.sender_session_id.is_none() {
        return Some(origin_message.message_id.clone());
    }
    thread_messages[..origin_index]
        .iter()
        .rev()
        .find(|message| message.sender_session_id.is_none())
        .map(|message| message.message_id.clone())
}

/// A human-readable "when it is" label (weekday, date, local time, part of day) for the
/// autonomous briefing, so agents are grounded in time like a colleague reading Slack.
fn channel_moment_label() -> String {
    use chrono::Timelike;
    let now = chrono::Local::now();
    let part = match now.hour() {
        5..=11 => "morning",
        12..=16 => "afternoon",
        17..=21 => "evening",
        _ => "late night",
    };
    format!("{}, {part}", now.format("%A %-d %B %Y, %H:%M %Z"))
}

/// Collapses whitespace and truncates to a rough character budget with an ellipsis, for the
/// one-line gists and snippets on the briefing board.
fn channel_briefing_snippet(content: &str, max_chars: usize) -> String {
    let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max_chars {
        let truncated: String = flat.chars().take(max_chars).collect();
        format!("{truncated}…")
    } else {
        flat
    }
}

/// A compact relative-age label ("just now", "12m ago", "3h ago", "2d ago").
fn channel_briefing_age(now_ms: u64, then_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(then_ms) / 1_000;
    if seconds < 90 {
        "just now".to_string()
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

fn render_channel_thread_context(
    channel: &crate::ChannelView,
    session_id: &str,
    origin_message: &crate::ChannelMessageView,
    human_origin_message: &crate::ChannelMessageView,
    thread_messages: &[crate::ChannelMessageView],
    autonomous: bool,
    new_topic: bool,
    autonomous_briefing: Option<&str>,
) -> String {
    let mut lines = vec![
        format!("[Channel {}]", channel.summary.title),
        format!("Channel ID: {}", channel.summary.channel_id),
    ];
    if let Some(description) = channel.summary.description.as_deref() {
        lines.push(format!("Description: {description}"));
    }
    if let Some(purpose) = channel.summary.purpose.as_deref() {
        lines.push(format!("Purpose: {purpose}"));
    }
    if !channel.members.is_empty() {
        lines.push("Members:".to_string());
        for member in &channel.members {
            let role = member
                .role
                .as_deref()
                .map(|role| format!(" ({role})"))
                .unwrap_or_default();
            lines.push(format!("- {}{}", member.display_name, role));
        }
    }

    // Human-turn framing (addressed members, the authoritative human request, anti-pile-on)
    // is meaningless for an autonomous turn and only re-anchors it on "the last message".
    if !autonomous {
        if !human_origin_message.addressed_member_ids.is_empty() {
            let addressed_names = channel
                .members
                .iter()
                .filter(|member| {
                    human_origin_message
                        .addressed_member_ids
                        .contains(&member.member_id)
                })
                .map(|member| member.display_name.clone())
                .collect::<Vec<_>>();
            if !addressed_names.is_empty() {
                lines.push(format!(
                    "Explicitly addressed members: {}.",
                    addressed_names.join(", ")
                ));
            }
            if channel.members.iter().any(|member| {
                member.session_id.as_deref() == Some(session_id)
                    && human_origin_message
                        .addressed_member_ids
                        .contains(&member.member_id)
            }) {
                lines.push("The current human message explicitly addressed you first. Reply before other members if you have a useful answer.".to_string());
            }
        }
        if human_origin_message.message_id != origin_message.message_id {
            lines.push(format!(
                "The current public turn still serves the human request in message {}. Keep that human message and its attachments authoritative even when replying to later agent comments.",
                human_origin_message.message_id
            ));
        }
        let follow_up_agent_messages = thread_messages
            .iter()
            .filter(|message| {
                message.sender_session_id.is_some()
                    && message.created_at_ms >= human_origin_message.created_at_ms
            })
            .count();
        if follow_up_agent_messages > 0
            && origin_message.sender_session_id.is_some()
            && origin_message.sender_session_id.as_deref() != Some(session_id)
        {
            lines.push("Another agent already replied to this human turn. Prefer set_channel_reaction unless you have a materially distinct comment or a useful correction.".to_string());
        }
    }

    if autonomous {
        // The situational briefing (date, your other rooms, the board of topics, the ambient
        // pulse) gives the agent a view of the whole room instead of just the latest line.
        if let Some(briefing) = autonomous_briefing {
            if !briefing.trim().is_empty() {
                lines.push(briefing.trim_end().to_string());
            }
        }
        lines.push(String::new());
        if new_topic {
            lines.push("There is a lull and nobody is waiting on you. If something is genuinely on your mind, just say it, the way you would drop a thought into a work chat. It will start its own thread on its own. Say it straight, in your own voice, with no preamble announcing that it is a new topic and no time-of-day opener. As long or as short as it deserves. If nothing is really on your mind, say nothing and finish without emit_output.".to_string());
        } else {
            lines.push("The floor is open and it's yours if you want it. You're a member here with your own vantage point, not a reply bot. Any of these is fair game, and none is required:".to_string());
            lines.push("- pick up any topic on the board above, not only the newest, and add the next real thing to it".to_string());
            lines.push("- just react (set_channel_reaction on a message id) when a nod says enough".to_string());
            lines.push("- bring in outside signal: your own expertise, or something you're chewing on in one of your other rooms".to_string());
            lines.push("- push back or disagree if you genuinely see it differently. You do not have to be agreeable.".to_string());
            lines.push("- ask the room a real question".to_string());
            lines.push("- or stay quiet and let it breathe. Saying nothing is a normal, common move here.".to_string());
            lines.push("Write in your own voice, as long or as short as it deserves, sometimes just a line or a reaction. Do not reflexively open with the last speaker's name and do not just restate the last message. Follow whichever thread actually pulls you.".to_string());
        }
    } else {
        // Human-triggered turn: keep the focused single-thread view and direct reply framing.
        lines.push("Use emit_output to publish a public channel reply. If you only agree or acknowledge, prefer set_channel_reaction. If you need canonical message ids or the full thread state before reacting, call read_channel_thread. If you have nothing useful to add, finish without emit_output.".to_string());
        lines.push("Recent thread:".to_string());
        for message in thread_messages
            .iter()
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            let sender = message
                .sender
                .display_name
                .clone()
                .unwrap_or_else(|| message.sender.id.clone());
            let reaction_summary = if message.reactions.is_empty() {
                String::new()
            } else {
                let joined = message
                    .reactions
                    .iter()
                    .map(|reaction| format!("{} x{}", reaction.emoji, reaction.count))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(" [reactions: {joined}]")
            };
            lines.push(format!(
                "- [{}] {sender}: {}{}",
                message.message_id, message.output.content, reaction_summary
            ));
        }
        let origin_sender = origin_message
            .sender
            .display_name
            .clone()
            .unwrap_or_else(|| origin_message.sender.id.clone());
        lines.push(format!(
            "Respond to message {} from {}.",
            origin_message.message_id, origin_sender
        ));
    }
    lines.join("\n")
}

fn starts_with_member_alias(content: &str, alias: &str) -> bool {
    let content = content.trim_start();
    let Some(rest) = content.strip_prefix(alias) else {
        return false;
    };
    rest.is_empty()
        || rest
            .chars()
            .next()
            .is_some_and(|ch| ch.is_whitespace() || ",:;!?.".contains(ch))
}

fn normalize_address_alias(value: &str) -> String {
    value
        .trim()
        .trim_matches(|ch: char| matches!(ch, '@' | ',' | ':' | ';' | '!' | '?' | '.'))
        .to_ascii_lowercase()
}

fn channel_delivery_attachment_refs(
    channel: &crate::ChannelView,
    origin_message: &crate::ChannelMessageView,
    human_origin_message: &crate::ChannelMessageView,
    state: &DaemonState<impl kheish_core::ModelDriver + Send + Sync + 'static>,
) -> Result<Vec<AttachmentRef>> {
    let mut attachments = channel
        .pinned_asset_ids
        .iter()
        .filter_map(|asset_id| state.assets.get(asset_id))
        .map(|asset| asset.attachment_ref())
        .collect::<Vec<_>>();
    for message in [human_origin_message, origin_message] {
        for part in &message.output.parts {
            if let kheish_types::ContentPart::Attachment { attachment } = part
                && !attachments
                    .iter()
                    .any(|existing| existing.id == attachment.id)
            {
                attachments.push(attachment.clone());
            }
        }
        for attachment in &message.output.artifacts {
            if !attachments
                .iter()
                .any(|existing| existing.id == attachment.id)
            {
                attachments.push(attachment.clone());
            }
        }
    }
    Ok(attachments)
}

#[cfg(test)]
mod helper_tests {
    use super::{resolve_human_origin_message_id, thread_social_participants_since_human};
    use kheish_types::{ActorRef, RichOutput};
    use serde_json::Value;

    fn message(
        message_id: &str,
        sender_session_id: Option<&str>,
        reactions: Vec<crate::ChannelReactionView>,
    ) -> crate::ChannelMessageView {
        crate::ChannelMessageView {
            message_id: message_id.to_string(),
            channel_id: "channel-test".to_string(),
            thread_root_message_id: Some("channel-message-1".to_string()),
            reply_to_message_id: None,
            sender: ActorRef {
                id: sender_session_id.unwrap_or("alice").to_string(),
                display_name: Some(sender_session_id.unwrap_or("Alice").to_string()),
            },
            sender_session_id: sender_session_id.map(ToString::to_string),
            addressed_member_ids: Vec::new(),
            output: RichOutput::text(message_id),
            created_at_ms: 1,
            reactions,
            metadata: Value::Null,
        }
    }

    #[test]
    fn resolve_human_origin_message_id_falls_back_to_latest_human_before_agent_origin() {
        let thread_messages = vec![
            message("channel-message-1", None, Vec::new()),
            message("channel-message-2", Some("aurora-room"), Vec::new()),
            message("channel-message-3", Some("atlas-room"), Vec::new()),
        ];

        assert_eq!(
            resolve_human_origin_message_id(&thread_messages, "channel-message-3", None),
            Some("channel-message-1".to_string())
        );
        assert_eq!(
            resolve_human_origin_message_id(
                &thread_messages,
                "channel-message-3",
                Some("channel-message-1"),
            ),
            Some("channel-message-1".to_string())
        );
    }

    #[test]
    fn thread_social_participants_since_human_counts_reactions_on_human_origin_message() {
        let thread_messages = vec![
            message(
                "channel-message-1",
                None,
                vec![crate::ChannelReactionView {
                    emoji: "🔥".to_string(),
                    actor_ids: vec!["atlas-room".to_string()],
                    count: 1,
                }],
            ),
            message("channel-message-2", Some("aurora-room"), Vec::new()),
        ];

        let (commented, reacted) =
            thread_social_participants_since_human(&thread_messages, Some("channel-message-1"));
        assert_eq!(commented, vec!["aurora-room".to_string()]);
        assert_eq!(reacted, vec!["atlas-room".to_string()]);
    }
}
