//! Run lifecycle methods implemented on [`DaemonState`].

use super::*;
use crate::problems::DaemonProblem;
use sha2::{Digest, Sha256};

const RUN_SUMMARY_CANDIDATE_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const USER_QUESTION_EXPIRATION_POLL_MS: u64 = 1_000;
const USER_QUESTION_EXPIRATION_ERROR_RETRY_MS: u64 = 500;
const SESSION_RUN_IDEMPOTENCY_WAIT_MS: u64 = 2_000;
const SESSION_RUN_IDEMPOTENCY_POLL_MS: u64 = 25;
const DEBUG_RETENTION_ERROR_RETRY_MS: u64 = 60_000;
const APPROVAL_OPERATION: &str = "approval";
const USER_QUESTION_OPERATION: &str = "user_question";
const USER_QUESTION_CANCEL_OPERATION: &str = "user_question_cancel";
const MAILBOX_MAX_DELIVERY_ATTEMPTS: u32 = 3;

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(crate) fn spawn_user_question_expiration_worker(
        self: &Arc<Self>,
    ) -> tokio::task::JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            state.user_question_expiration_worker_loop().await;
        })
    }

    pub(crate) fn spawn_debug_retention_worker(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            state.debug_retention_worker_loop().await;
        })
    }

    pub(crate) async fn prune_expired_debug_evidence_on_boot(self: &Arc<Self>) -> Result<()> {
        if let Some(response) = self
            .run_service
            .prune_expired_debug_evidence(now_ms())
            .await?
            && response.pruned_debug_bytes > 0
        {
            info!(
                pruned_runs = response.pruned_debug_run_ids.len(),
                pruned_bytes = response.pruned_debug_bytes,
                "pruned expired terminal-run debug capture evidence on boot"
            );
        }
        Ok(())
    }

    async fn debug_retention_worker_loop(self: Arc<Self>) {
        loop {
            let interval_ms = self.run_service.debug_retention_interval_ms().max(1);
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
            match self
                .run_service
                .prune_expired_debug_evidence(now_ms())
                .await
            {
                Ok(Some(response)) if response.pruned_debug_bytes > 0 => {
                    info!(
                        pruned_runs = response.pruned_debug_run_ids.len(),
                        pruned_bytes = response.pruned_debug_bytes,
                        "pruned expired terminal-run debug capture evidence"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    error!(
                        error = ?error,
                        "failed to prune expired terminal-run debug capture evidence"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(
                        DEBUG_RETENTION_ERROR_RETRY_MS,
                    ))
                    .await;
                }
            }
        }
    }

    pub(crate) async fn expire_due_user_questions(
        self: &Arc<Self>,
        now_ms: u64,
    ) -> Result<Option<u64>> {
        let snapshot = self
            .run_service
            .pending_question_expiration_snapshot(now_ms);
        let mut first_error = None;
        for run_id in snapshot.due_run_ids {
            if let Err(error) = self.expire_waiting_user_question_run(&run_id, now_ms).await {
                error!(
                    run_id = %run_id,
                    error = ?error,
                    "failed to expire waiting user-question run"
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(self
            .run_service
            .pending_question_expiration_snapshot(now_ms)
            .next_expiry_ms)
    }

    async fn ensure_selected_route_ready(&self, request: &SubmitInputRequest) -> Result<()> {
        self.ensure_resolved_route_ready(request.provider.as_deref())
            .await
    }

    async fn ensure_resolved_route_ready_unlocked(&self, route_id: Option<&str>) -> Result<()> {
        self.ensure_resolved_route_ready_with(route_id, true).await
    }

    async fn ensure_resolved_route_ready(&self, route_id: Option<&str>) -> Result<()> {
        self.ensure_resolved_route_ready_with(route_id, false).await
    }

    async fn ensure_resolved_route_ready_with(
        &self,
        route_id: Option<&str>,
        runtime_snapshot_locked: bool,
    ) -> Result<()> {
        let Some(route_id) = route_id else {
            return Ok(());
        };
        let readiness = if runtime_snapshot_locked {
            self.provider_route_readiness_for_route_id_unlocked(route_id)
                .await
        } else {
            self.provider_route_readiness_for_route_id(route_id).await
        };
        let Some(mut readiness) = readiness else {
            let runtime = if runtime_snapshot_locked {
                self.runtime_settings_unlocked()
            } else {
                self.runtime_settings().await
            };
            if !runtime.routes.is_empty() {
                anyhow::bail!(
                    "route `{route_id}` is not ready: route is not present in runtime route inventory. Action: choose one of the configured routes or reload route configuration."
                );
            }
            return Ok(());
        };
        if readiness.state != crate::DaemonStatusProbeState::Error {
            return Ok(());
        }
        if self.try_refresh_expired_route_auth(&readiness).await? {
            let refreshed = if runtime_snapshot_locked {
                self.provider_route_readiness_for_route_id_unlocked(route_id)
                    .await
            } else {
                self.provider_route_readiness_for_route_id(route_id).await
            };
            let Some(refreshed) = refreshed else {
                anyhow::bail!(
                    "route `{route_id}` is not ready: route disappeared after auth refresh. Action: choose one of the configured routes or reload route configuration."
                );
            };
            if refreshed.state != crate::DaemonStatusProbeState::Error {
                return Ok(());
            }
            readiness = refreshed;
        }
        let action = readiness
            .action
            .as_deref()
            .map(|action| format!(" Action: {action}."))
            .unwrap_or_default();
        anyhow::bail!(
            "route `{}` is not ready: {}{}",
            readiness.route_id,
            readiness.message,
            action
        );
    }

    async fn try_refresh_expired_route_auth(
        &self,
        readiness: &crate::DaemonProviderRouteReadinessView,
    ) -> Result<bool> {
        if readiness.code != "route_auth_expired"
            || readiness.auth_mode != Some(kheish_auth::AuthMode::OAuthAccount)
        {
            return Ok(false);
        }
        let Some(auth_ref) = readiness.auth_ref.as_deref() else {
            return Ok(false);
        };
        self.refresh_auth_slot(auth_ref).await.with_context(|| {
            format!(
                "route `{}` auth_ref `{auth_ref}` is expired and automatic refresh failed",
                readiness.route_id
            )
        })?;
        Ok(true)
    }

    async fn user_question_expiration_worker_loop(self: Arc<Self>) {
        loop {
            match self.expire_due_user_questions(now_ms()).await {
                Ok(next_expiry_ms) => {
                    let wait = next_expiry_ms
                        .map(|deadline| deadline.saturating_sub(now_ms()).max(1))
                        .unwrap_or(USER_QUESTION_EXPIRATION_POLL_MS)
                        .min(USER_QUESTION_EXPIRATION_POLL_MS);
                    tokio::select! {
                        _ = self.run_service.pending_question_notify().notified() => {}
                        _ = sleep_until(Instant::now() + Duration::from_millis(wait)) => {}
                    }
                }
                Err(error) => {
                    error!(error = ?error, "user-question expiration worker error");
                    tokio::select! {
                        _ = self.run_service.pending_question_notify().notified() => {}
                        _ = sleep_until(
                            Instant::now()
                                + Duration::from_millis(USER_QUESTION_EXPIRATION_ERROR_RETRY_MS),
                        ) => {}
                    }
                }
            }
        }
    }

    async fn expire_waiting_user_question_run(
        self: &Arc<Self>,
        run_id: &str,
        now_ms: u64,
    ) -> Result<RunView> {
        let record = self.run_record(run_id).await?;
        if record.view.status != DaemonRunStatus::WaitingForUserQuestion {
            return Ok(record.view);
        }
        let Some(expired_request) = record
            .view
            .pending_questions
            .iter()
            .find(|question| {
                question
                    .expires_at_ms
                    .is_some_and(|expires_at_ms| expires_at_ms <= now_ms)
            })
            .cloned()
        else {
            return Ok(record.view);
        };
        if let RunRequestPayload::ParentClarification {
            request: clarification,
            completion,
        } = &record.payload
        {
            if let Some(resolution) = completion.resolution.clone() {
                let reason = parent_clarification_reason_for_existing(completion, &resolution);
                return self
                    .complete_parent_clarification_run(
                        run_id,
                        &record.view.session_id,
                        &AgentId(record.view.agent_id.clone()),
                        clarification.clone(),
                        resolution,
                        reason,
                    )
                    .await;
            }
            return self
                .complete_parent_clarification_run(
                    run_id,
                    &record.view.session_id,
                    &AgentId(record.view.agent_id.clone()),
                    clarification.clone(),
                    UserQuestionResolution {
                        request_id: expired_request.id.clone(),
                        answers: Vec::new(),
                        declined: true,
                        justification: Some(format!(
                            "expired at {}",
                            expired_request.expires_at_ms.unwrap_or(now_ms)
                        )),
                    },
                    ParentClarificationCompletionReason::Expired {
                        expires_at_ms: expired_request.expires_at_ms.unwrap_or(now_ms),
                    },
                )
                .await;
        }
        let reason = format!(
            "user-question request {} expired at {}",
            expired_request.id,
            expired_request.expires_at_ms.unwrap_or(now_ms)
        );
        self.cancel_run_with_error(run_id, Some(reason)).await
    }

    async fn complete_or_decline_waiting_parent_clarification(
        self: &Arc<Self>,
        run_id: &str,
        justification: String,
        reason: ParentClarificationCompletionReason,
    ) -> Result<Option<RunView>> {
        let record = self.run_record(run_id).await?;
        if record.view.status.is_terminal() {
            return Ok(None);
        }
        let RunRequestPayload::ParentClarification {
            request: clarification,
            completion,
        } = &record.payload
        else {
            return Ok(None);
        };
        let resolution = if let Some(resolution) = completion.resolution.clone() {
            resolution
        } else if record.view.status == DaemonRunStatus::WaitingForUserQuestion {
            let Some(request) = record.view.pending_questions.first() else {
                return Ok(None);
            };
            UserQuestionResolution {
                request_id: request.id.clone(),
                answers: Vec::new(),
                declined: true,
                justification: Some(justification),
            }
        } else {
            return Ok(None);
        };
        let view = self
            .complete_parent_clarification_run(
                run_id,
                &record.view.session_id,
                &AgentId(record.view.agent_id.clone()),
                clarification.clone(),
                resolution,
                completion.reason.clone().unwrap_or(reason),
            )
            .await?;
        Ok(Some(view))
    }

    pub(super) async fn schedule_mailbox_run(
        self: &Arc<Self>,
        agent_id: &AgentId,
    ) -> Result<Option<RunView>> {
        let Some(agent) = self.supervisor.get(agent_id) else {
            anyhow::bail!("unknown agent {}", agent_id.0);
        };
        if agent.closed_at_ms.is_some() {
            if self
                .supervisor
                .dead_letter_mailbox(agent_id, "agent closed before mailbox delivery")
                > 0
            {
                self.persist_topology().await?;
            }
            return Ok(None);
        }
        if !self.orchestrator.has_runtime(agent_id) {
            return Ok(None);
        }
        let mailbox_key = agent_id.0.clone();
        if !self
            .subagent_service
            .try_acquire_mailbox_slot(&mailbox_key)
            .await
        {
            return Ok(None);
        }

        let result = async {
            let _mailbox_guard = self.mailbox_topology_lock.lock().await;
            let agent = self
                .supervisor
                .get(agent_id)
                .ok_or_else(|| anyhow!("unknown agent {}", agent_id.0))?;
            if agent.closed_at_ms.is_some() {
                if self
                    .supervisor
                    .dead_letter_mailbox(agent_id, "agent closed before mailbox delivery")
                    > 0
                {
                    self.persist_topology().await?;
                }
                return Ok(None);
            }
            if !self.orchestrator.has_runtime(agent_id) {
                return Ok(None);
            }
            let session_id = agent.conversation.session_id.clone();
            if self.run_service.has_pending_mailbox_delivery(&session_id) {
                return Ok(None);
            }

            let mut messages = self.supervisor.peek_mailbox(agent_id);
            if messages.is_empty() {
                return Ok(None);
            }
            let now = now_ms();
            let (expired, active): (Vec<_>, Vec<_>) = messages
                .into_iter()
                .partition(|message| message.is_expired(now));
            if !expired.is_empty() {
                let expired_ids = expired
                    .iter()
                    .map(|message| message.id.clone())
                    .collect::<Vec<_>>();
                self.supervisor
                    .ack_mailbox_message_ids(agent_id, &expired_ids);
                self.supervisor.dead_letter_mailbox_messages(
                    agent_id,
                    expired,
                    "mailbox message expired before delivery",
                );
                self.persist_topology().await?;
            }
            messages = active;
            if messages.is_empty() {
                return Ok(None);
            }
            loop {
                let durable_prefix_len = self
                    .run_service
                    .session_mailbox_delivery_prefix_len(&session_id, &agent_id.0, &messages)
                    .await?;
                if durable_prefix_len == 0 {
                    break;
                }
                self.supervisor
                    .ack_mailbox_prefix(agent_id, durable_prefix_len);
                self.persist_topology().await?;
                messages = self.supervisor.peek_mailbox(agent_id);
                if messages.is_empty() {
                    return Ok(None);
                }
            }
            let run_id = self.next_run_id();
            let now = now_ms();
            let delivery_messages = messages
                .iter()
                .map(MailboxMessage::delivering_for_run)
                .collect::<Vec<_>>();
            let preview = messages.first().map(|message| message.subject.as_str());
            let input_attachments = self
                .input_attachment_refs_for_mailbox_messages(&session_id, &delivery_messages)
                .await?;
            let fork_provider = agent
                .fork_context
                .as_ref()
                .and_then(|fork_context| fork_context.provider.clone());
            let fork_generation = agent
                .fork_context
                .as_ref()
                .and_then(|fork_context| fork_context.generation.clone());
            let (provider, generation) = if fork_provider.is_some() || fork_generation.is_some() {
                (fork_provider, fork_generation)
            } else {
                let policy = self.effective_session_route_policy(&session_id).await?;
                (policy.provider, policy.generation)
            };
            let (resolved_provider, resolved_model) = {
                let _runtime_config_snapshot = self.runtime_config_service.snapshot_guard().await;
                let (resolved_provider, resolved_generation) = self
                    .resolve_generation_route_for_session(&session_id, provider, generation)
                    .await?;
                let resolved_model = resolved_generation
                    .as_ref()
                    .and_then(|generation| generation.model.clone());
                (resolved_provider, resolved_model)
            };
            let mut request = summarize_mailbox_request(&agent_id.0, messages.len(), preview);
            request.provider = resolved_provider;
            request.model = resolved_model;
            let record = RunRecord {
                view: RunView {
                    run_id: run_id.clone(),
                    session_id: session_id.clone(),
                    agent_id: agent_id.0.clone(),
                    kind: DaemonRunKind::MailboxDelivery,
                    status: DaemonRunStatus::Queued,
                    submitted_at_ms: now,
                    updated_at_ms: now,
                    started_at_ms: None,
                    finished_at_ms: None,
                    queued_position: None,
                    request,
                    input_attachments,
                    input_metadata: None,
                    pending_approval_ids: Vec::new(),
                    pending_approvals: Vec::new(),
                    pending_question_ids: Vec::new(),
                    pending_questions: Vec::new(),
                    outputs: Vec::new(),
                    deliveries: Vec::new(),
                    error: None,
                },
                reply_targets: Vec::new(),
                payload: RunRequestPayload::MailboxDelivery {
                    agent_id: agent_id.0.clone(),
                    messages: delivery_messages.clone(),
                },
            };
            let view = self.schedule_run(record).await?;
            let delivery_message_ids = delivery_messages
                .iter()
                .map(|message| message.id.clone())
                .collect::<Vec<_>>();
            self.supervisor
                .ack_mailbox_message_ids(agent_id, &delivery_message_ids);
            self.persist_topology().await?;
            Ok(Some(view))
        }
        .await;

        self.subagent_service
            .release_mailbox_slot(&mailbox_key)
            .await;
        result
    }

    pub(super) fn next_run_id(&self) -> String {
        self.run_service.next_run_id()
    }

    pub(crate) async fn submit_input_run(
        self: &Arc<Self>,
        session_id: &str,
        request: SubmitInputRequest,
    ) -> Result<RunView> {
        self.submit_input_run_inner(
            session_id,
            request,
            false,
            None,
            None,
            None,
            DaemonRunKind::Input,
            false,
        )
        .await
    }

    pub(crate) async fn submit_input_run_requiring_idle(
        self: &Arc<Self>,
        session_id: &str,
        request: SubmitInputRequest,
    ) -> Result<RunView> {
        let run_id = self.next_run_id();
        if !self
            .run_service
            .reserve_idle_submission_slot(session_id, &run_id)
            .await?
        {
            return Err(DaemonProblem::session_busy(format!(
                "session {session_id} is already processing background work"
            ))
            .into());
        }

        let result = self
            .submit_input_run_inner(
                session_id,
                request,
                false,
                Some(run_id.clone()),
                None,
                None,
                DaemonRunKind::Input,
                true,
            )
            .await;
        if result.is_err()
            && self
                .run_service
                .release_idle_submission_slot(session_id, &run_id)
                .await
            && let Err(error) = self.start_next_queued_run(session_id).await
        {
            warn!(
                session_id = %session_id,
                run_id = %run_id,
                error = %error,
                "failed to promote queued work after releasing failed idle submission reservation"
            );
        }
        result
    }

    pub(crate) async fn submit_input_run_with_preallocated_id(
        self: &Arc<Self>,
        session_id: &str,
        request: SubmitInputRequest,
        run_id: String,
    ) -> Result<RunView> {
        self.submit_input_run_inner(
            session_id,
            request,
            false,
            Some(run_id),
            None,
            None,
            DaemonRunKind::Input,
            false,
        )
        .await
    }

    pub(crate) async fn submit_input_run_idempotent(
        self: &Arc<Self>,
        session_id: &str,
        request: SubmitInputRequest,
        idempotency_key: &str,
    ) -> Result<RunView> {
        let idempotency_key = normalize_session_run_idempotency_key(idempotency_key)?;
        let key_hash = session_run_idempotency_key_hash(&idempotency_key);
        let receipt_key = session_run_idempotency_receipt_key(session_id, &key_hash);
        let request_fingerprint = submit_input_request_fingerprint(&request)?;

        if let Some(existing) = self
            .run_service
            .find_input_run_by_idempotency(session_id, &key_hash, &request_fingerprint)
            .await?
        {
            self.session_service
                .remember_session_run_idempotency(
                    &receipt_key,
                    &existing.run_id,
                    &request_fingerprint,
                )
                .await?;
            return Ok(existing);
        }

        if !self
            .try_acquire_session_run_idempotency_submission(&receipt_key)
            .await
        {
            return self
                .await_session_run_idempotency_submission(
                    session_id,
                    &receipt_key,
                    &key_hash,
                    &request_fingerprint,
                )
                .await;
        }

        let result = async {
            if let Some(existing) = self
                .run_service
                .find_input_run_by_idempotency(session_id, &key_hash, &request_fingerprint)
                .await?
            {
                self.session_service
                    .remember_session_run_idempotency(
                        &receipt_key,
                        &existing.run_id,
                        &request_fingerprint,
                    )
                    .await?;
                return Ok(existing);
            }

            let reservation = self
                .session_service
                .begin_session_run_idempotency(&receipt_key, &request_fingerprint, || {
                    self.next_run_id()
                })
                .await?;
            let run_id = match reservation {
                SessionRunIdempotencyReservation::Existing { run_id }
                | SessionRunIdempotencyReservation::Pending { run_id } => {
                    if let Ok(run) = self.run_service.get_run(&run_id).await {
                        self.session_service
                            .remember_session_run_idempotency(
                                &receipt_key,
                                &run.run_id,
                                &request_fingerprint,
                            )
                            .await?;
                        return Ok(run);
                    }
                    run_id
                }
                SessionRunIdempotencyReservation::Reserved { run_id } => run_id,
            };

            let idempotency = RunInputIdempotency {
                key_hash: key_hash.clone(),
                request_fingerprint: request_fingerprint.clone(),
            };
            let scheduled = self
                .submit_input_run_inner(
                    session_id,
                    request,
                    false,
                    Some(run_id.clone()),
                    Some(idempotency),
                    None,
                    DaemonRunKind::Input,
                    false,
                )
                .await;
            match scheduled {
                Ok(view) => {
                    self.session_service
                        .remember_session_run_idempotency(
                            &receipt_key,
                            &view.run_id,
                            &request_fingerprint,
                        )
                        .await?;
                    Ok(view)
                }
                Err(error) => {
                    if let Ok(run) = self.run_service.get_run(&run_id).await {
                        self.session_service
                            .remember_session_run_idempotency(
                                &receipt_key,
                                &run.run_id,
                                &request_fingerprint,
                            )
                            .await?;
                        return Ok(run);
                    }
                    let _ = self
                        .session_service
                        .forget_session_run_idempotency(&receipt_key)
                        .await;
                    Err(error)
                }
            }
        }
        .await;

        self.release_session_run_idempotency_submission(&receipt_key)
            .await;
        result
    }

    pub(crate) async fn submit_input_run_with_daemon_metadata(
        self: &Arc<Self>,
        session_id: &str,
        request: SubmitInputRequest,
    ) -> Result<RunView> {
        self.submit_input_run_inner(
            session_id,
            request,
            true,
            None,
            None,
            None,
            DaemonRunKind::Input,
            false,
        )
        .await
    }

    pub(crate) async fn submit_scheduled_input_run_with_daemon_metadata(
        self: &Arc<Self>,
        session_id: &str,
        request: SubmitInputRequest,
        scheduled_origin: ScheduledRunOrigin,
        run_id: String,
    ) -> Result<RunView> {
        self.submit_input_run_inner(
            session_id,
            request,
            true,
            Some(run_id),
            None,
            Some(scheduled_origin),
            DaemonRunKind::ScheduledInput,
            false,
        )
        .await
    }

    async fn schedule_goal_continuation_if_idle(
        self: &Arc<Self>,
        session_id: &str,
        previous_run_id: &str,
    ) -> Result<Option<RunView>> {
        let Some(goal) = self.load_session_goal(session_id).await? else {
            return Ok(None);
        };
        let budget_wrapup = goal.status == kheish_types::SessionGoalStatus::BudgetLimited
            && goal.budget_wrapup_run_id.is_none();
        if !goal.should_continue() && !budget_wrapup {
            return Ok(None);
        }
        if let Some(run_id) = goal.last_continuation_run_id.as_deref() {
            if let Ok(run) = self.run_service.get_run(run_id).await {
                if !run.status.is_terminal() {
                    return Ok(None);
                }
            }
        }

        let run_id = self.next_run_id();
        if !self
            .run_service
            .reserve_goal_continuation_slot(session_id, &run_id)
            .await?
        {
            return Ok(None);
        }
        let Some(scheduled_goal) = self
            .goal_service
            .mark_continuation_scheduled(
                session_id,
                &run_id,
                budget_wrapup,
                &goal.goal_id,
                goal.version,
            )
            .await?
        else {
            self.run_service
                .release_goal_continuation_slot(session_id, &run_id)
                .await;
            return Ok(None);
        };
        let content = if budget_wrapup {
            format!(
                "The active session goal `{}` reached its token budget. Do not start new substantive work. Summarize the current result, what was verified, and what remains.",
                scheduled_goal.goal_id
            )
        } else {
            format!(
                "Continue working on the active session goal `{}`. Inspect the goal state, continue the next useful step, verify what you can, and mark the goal complete only if it is actually achieved.",
                scheduled_goal.goal_id
            )
        };
        let request = SubmitInputRequest {
            provider: None,
            source_plugin: Some("daemon".to_string()),
            source_kind: Some("goal_continuation".to_string()),
            actor_id: Some("daemon".to_string()),
            content,
            input_items: Vec::new(),
            attachments: Vec::new(),
            generation: None,
            completion_requirements: None,
            metadata: Some(json!({
                "daemon": {
                    "goal_id": scheduled_goal.goal_id,
                    "goal_version": scheduled_goal.binding_version(),
                    "previous_run_id": previous_run_id,
                    "budget_wrapup": budget_wrapup,
                }
            })),
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
            reply_address: None,
        };
        let result = self
            .submit_input_run_inner(
                session_id,
                request,
                true,
                Some(run_id.clone()),
                None,
                None,
                DaemonRunKind::GoalContinuation,
                false,
            )
            .await
            .map(Some);
        self.run_service
            .release_goal_continuation_slot(session_id, &run_id)
            .await;
        result
    }

    async fn submit_input_run_inner(
        self: &Arc<Self>,
        session_id: &str,
        mut request: SubmitInputRequest,
        allow_daemon_metadata: bool,
        preallocated_run_id: Option<String>,
        idempotency: Option<RunInputIdempotency>,
        scheduled_origin: Option<ScheduledRunOrigin>,
        kind: DaemonRunKind,
        require_idle: bool,
    ) -> Result<RunView> {
        if !allow_daemon_metadata && contains_flow_metadata(&request.metadata) {
            anyhow::bail!("metadata key `{KHEISH_FLOW_METADATA_KEY}` is daemon-owned");
        }
        if !allow_daemon_metadata && contains_goal_daemon_metadata(&request.metadata) {
            anyhow::bail!("metadata key `daemon` is daemon-owned");
        }
        let agent_id = self.agent_id_for_session(session_id).await?;
        self.ensure_submit_input_request_has_payload(&request)?;
        self.validate_submit_input_request(session_id, &request)
            .await?;
        self.normalize_submit_input_request(session_id, &mut request)
            .await?;
        let input_attachments = self
            .input_attachment_refs_for_request(session_id, &request)
            .await?;
        let input_metadata = self
            .input_metadata_with_goal_binding(session_id, request.metadata.clone())
            .await?;
        let explicit_reply_targets = self.explicit_input_reply_targets(session_id, &request);
        let should_persist_connector_reply_targets = !explicit_reply_targets.is_empty()
            && !matches!(
                request.source_plugin.as_deref(),
                None | Some("daemon") | Some("scheduler")
            );
        let preallocated_run_id = preallocated_run_id;
        let run_id_was_preallocated = preallocated_run_id.is_some();
        let run_id = preallocated_run_id.unwrap_or_else(|| self.next_run_id());
        let reply_targets = self.resolve_run_reply_targets(session_id, &request).await?;
        let now = now_ms();
        {
            let _runtime_config_snapshot = self.runtime_config_service.snapshot_guard().await;
            let (resolved_provider, resolved_generation) = self
                .resolve_generation_route_for_session(
                    session_id,
                    request.provider.take(),
                    request.generation.take(),
                )
                .await?;
            request.provider = resolved_provider;
            request.generation = resolved_generation;
        }
        self.ensure_selected_route_ready(&request).await?;
        self.remember_session_bindings(session_id, request.binding_keys.clone())
            .await?;
        debug!(
            session_id = %session_id,
            agent_id = %agent_id.0,
            run_id = %run_id,
            provider = request.provider.as_deref(),
            model = request
                .generation
                .as_ref()
                .and_then(|generation| generation.model.as_deref()),
            reply_target_count = reply_targets.len(),
            "accepted input submission request"
        );
        let request_summary = summarize_input_request(&request);
        let payload = if let Some(origin) = scheduled_origin {
            RunRequestPayload::ScheduledInput {
                schedule_id: origin.schedule_id,
                fire_at_ms: origin.fire_at_ms,
                request,
            }
        } else {
            RunRequestPayload::Input {
                request,
                idempotency,
            }
        };
        let record = RunRecord {
            view: RunView {
                run_id: run_id.clone(),
                session_id: session_id.to_string(),
                agent_id: agent_id.0.clone(),
                kind,
                status: DaemonRunStatus::Queued,
                submitted_at_ms: now,
                updated_at_ms: now,
                started_at_ms: None,
                finished_at_ms: None,
                queued_position: None,
                request: request_summary,
                input_attachments,
                input_metadata,
                pending_approval_ids: Vec::new(),
                pending_approvals: Vec::new(),
                pending_question_ids: Vec::new(),
                pending_questions: Vec::new(),
                outputs: Vec::new(),
                deliveries: Vec::new(),
                error: None,
            },
            reply_targets,
            payload,
        };
        let mut owns_connector_reply_target_reservation = false;
        let should_persist_session_reply_targets = if should_persist_connector_reply_targets
            && require_idle
            && run_id_was_preallocated
        {
            self.validate_session_reply_targets(session_id, &explicit_reply_targets)
                .await?;
            true
        } else if should_persist_connector_reply_targets {
            match self
                .run_service
                .reserve_idle_submission_slot(session_id, &run_id)
                .await
            {
                Ok(true) => {
                    owns_connector_reply_target_reservation = true;
                    if let Err(error) = self
                        .validate_session_reply_targets(session_id, &explicit_reply_targets)
                        .await
                    {
                        if self
                            .run_service
                            .release_idle_submission_slot(session_id, &run_id)
                            .await
                            && let Err(promote_error) = self.start_next_queued_run(session_id).await
                        {
                            warn!(
                                session_id = %session_id,
                                run_id = %run_id,
                                error = %promote_error,
                                "failed to promote queued work after releasing invalid connector reply-target reservation"
                            );
                        }
                        return Err(error);
                    }
                    true
                }
                Ok(false) => false,
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        run_id = %run_id,
                        error = %error,
                        "failed closed while reserving idle slot for connector-derived session reply targets"
                    );
                    false
                }
            }
        } else {
            false
        };
        let scheduled = self
            .schedule_run_with_idle_policy(record, require_idle)
            .await;
        if scheduled.is_err()
            && owns_connector_reply_target_reservation
            && self
                .run_service
                .release_idle_submission_slot(session_id, &run_id)
                .await
            && let Err(error) = self.start_next_queued_run(session_id).await
        {
            warn!(
                session_id = %session_id,
                run_id = %run_id,
                error = %error,
                "failed to promote queued work after releasing failed connector reply-target reservation"
            );
        }
        let view = scheduled?;
        if should_persist_session_reply_targets {
            if let Err(error) = self
                .remember_session_reply_targets(session_id, explicit_reply_targets)
                .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    run_id = %view.run_id,
                    error = %error,
                    "failed to persist connector-derived session reply targets after scheduling input run"
                );
            }
        } else if should_persist_connector_reply_targets {
            tracing::debug!(
                session_id = %session_id,
                run_id = %view.run_id,
                "skipped connector-derived session reply-target persistence because the session is not idle"
            );
        }
        Ok(view)
    }

    async fn input_metadata_with_goal_binding(
        &self,
        session_id: &str,
        metadata: Option<Value>,
    ) -> Result<Option<Value>> {
        if contains_goal_daemon_metadata(&metadata) {
            return Ok(metadata);
        }
        let Some(goal) = self.load_session_goal(session_id).await? else {
            return Ok(metadata);
        };
        let goal_metadata = json!({
            "goal_id": goal.goal_id,
            "goal_version": goal.binding_version(),
        });
        match metadata {
            None => Ok(Some(json!({ "daemon": goal_metadata }))),
            Some(Value::Object(mut object)) => {
                object.insert("daemon".to_string(), goal_metadata);
                Ok(Some(Value::Object(object)))
            }
            Some(_) => anyhow::bail!("metadata must be an object when daemon metadata is attached"),
        }
    }

    async fn try_acquire_session_run_idempotency_submission(&self, receipt_key: &str) -> bool {
        let mut inflight = self.session_run_idempotency_inflight.lock().await;
        inflight.insert(receipt_key.to_string())
    }

    async fn release_session_run_idempotency_submission(&self, receipt_key: &str) {
        self.session_run_idempotency_inflight
            .lock()
            .await
            .remove(receipt_key);
    }

    async fn await_session_run_idempotency_submission(
        self: &Arc<Self>,
        session_id: &str,
        receipt_key: &str,
        key_hash: &str,
        request_fingerprint: &str,
    ) -> Result<RunView> {
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(SESSION_RUN_IDEMPOTENCY_WAIT_MS);
        loop {
            if let Some(run) = self
                .run_service
                .find_input_run_by_idempotency(session_id, key_hash, request_fingerprint)
                .await?
            {
                self.session_service
                    .remember_session_run_idempotency(receipt_key, &run.run_id, request_fingerprint)
                    .await?;
                return Ok(run);
            }

            if let Some(receipt) = self
                .session_service
                .session_run_idempotency_receipt(receipt_key)
                .await
            {
                if receipt.request_fingerprint() != request_fingerprint {
                    return Err(DaemonProblem::idempotency_conflict(
                        "session run idempotency key was reused with a different request payload",
                    )
                    .into());
                }
                if let Ok(run) = self.run_service.get_run(receipt.run_id()).await {
                    self.session_service
                        .remember_session_run_idempotency(
                            receipt_key,
                            &run.run_id,
                            request_fingerprint,
                        )
                        .await?;
                    return Ok(run);
                }
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(DaemonProblem::idempotency_conflict(
                    "session run idempotency key is already pending",
                )
                .into());
            }
            tokio::time::sleep(Duration::from_millis(SESSION_RUN_IDEMPOTENCY_POLL_MS)).await;
        }
    }

    pub(super) async fn request_parent_clarification(
        self: &Arc<Self>,
        requester_session_id: &str,
        requester_agent_id: &str,
        requester_run_id: Option<&str>,
        requester_tool_call_id: Option<&str>,
        request: UserQuestionRequest,
    ) -> Result<ParentClarificationToolResponse> {
        let requester = self
            .supervisor
            .get(&AgentId(requester_agent_id.to_string()))
            .ok_or_else(|| anyhow!("unknown agent {requester_agent_id}"))?;
        anyhow::ensure!(
            requester.conversation.session_id == requester_session_id,
            "agent {requester_agent_id} does not belong to session {requester_session_id}"
        );
        let parent_agent_id = requester
            .parent
            .clone()
            .ok_or_else(|| anyhow!("agent {requester_agent_id} has no parent agent"))?;
        let parent = self
            .supervisor
            .get(&parent_agent_id)
            .ok_or_else(|| anyhow!("unknown parent agent {}", parent_agent_id.0))?;
        if let Some(existing) = self
            .run_service
            .find_parent_clarification_by_request(
                requester_agent_id,
                requester_session_id,
                requester_run_id,
                requester_tool_call_id,
                &request.id,
            )
            .await
        {
            let RunRequestPayload::ParentClarification {
                request: existing_request,
                ..
            } = existing.payload
            else {
                unreachable!(
                    "find_parent_clarification_by_request only returns clarification runs"
                );
            };
            anyhow::ensure!(
                parent_clarification_requests_equivalent(&existing_request.request, &request),
                "parent clarification request {} was replayed with a different payload",
                request.id
            );
            return Ok(ParentClarificationToolResponse {
                parent_agent_id: existing.view.agent_id,
                parent_session_id: existing.view.session_id,
                requester_run_id: existing_request.requester_run_id,
                requester_tool_call_id: existing_request.requester_tool_call_id,
                run_id: existing.view.run_id,
                request_id: existing_request.request.id,
                response_message_type: PARENT_CLARIFICATION_ANSWER_MESSAGE_TYPE.to_string(),
            });
        }

        let run_id = self.next_run_id();
        let now = now_ms();
        let requester_project_ids = self
            .project_service
            .project_ids_for_session(requester_session_id)
            .await;
        let parent_project_ids = self
            .project_service
            .project_ids_for_session(&parent.conversation.session_id)
            .await;
        let requester_channel_ids = self.channel_ids_for_session(requester_session_id).await;
        let parent_channel_ids = self
            .channel_ids_for_session(&parent.conversation.session_id)
            .await;
        let clarification = ParentClarificationRunRequest {
            requester_agent_id: requester_agent_id.to_string(),
            requester_session_id: requester_session_id.to_string(),
            requester_run_id: requester_run_id.map(str::to_string),
            requester_tool_call_id: requester_tool_call_id
                .map(str::to_string)
                .or_else(|| Some(request.tool_call_id.clone())),
            requester_project_ids,
            requester_channel_ids,
            parent_project_ids,
            parent_channel_ids,
            request: request.clone(),
        };
        let reply_targets = self
            .session_reply_targets(&parent.conversation.session_id)
            .await;
        let view = self
            .schedule_run(RunRecord {
                view: RunView {
                    run_id: run_id.clone(),
                    session_id: parent.conversation.session_id.clone(),
                    agent_id: parent.id.0.clone(),
                    kind: DaemonRunKind::ParentClarification,
                    status: DaemonRunStatus::Queued,
                    submitted_at_ms: now,
                    updated_at_ms: now,
                    started_at_ms: None,
                    finished_at_ms: None,
                    queued_position: None,
                    request: summarize_parent_clarification_request(&clarification),
                    input_attachments: Vec::new(),
                    input_metadata: None,
                    pending_approval_ids: Vec::new(),
                    pending_approvals: Vec::new(),
                    pending_question_ids: Vec::new(),
                    pending_questions: Vec::new(),
                    outputs: Vec::new(),
                    deliveries: Vec::new(),
                    error: None,
                },
                reply_targets,
                payload: RunRequestPayload::ParentClarification {
                    request: clarification.clone(),
                    completion: Default::default(),
                },
            })
            .await?;
        info!(
            requester_agent_id = %requester_agent_id,
            requester_session_id = %requester_session_id,
            parent_agent_id = %parent.id.0,
            parent_session_id = %parent.conversation.session_id,
            run_id = %view.run_id,
            request_id = %request.id,
            question_count = request.questions.len(),
            "scheduled parent clarification run"
        );
        Ok(ParentClarificationToolResponse {
            parent_agent_id: parent.id.0,
            parent_session_id: parent.conversation.session_id,
            requester_run_id: clarification.requester_run_id,
            requester_tool_call_id: clarification.requester_tool_call_id,
            run_id: view.run_id,
            request_id: request.id,
            response_message_type: PARENT_CLARIFICATION_ANSWER_MESSAGE_TYPE.to_string(),
        })
    }

    async fn channel_ids_for_session(&self, session_id: &str) -> Vec<String> {
        self.channel_service
            .list_channels(None)
            .await
            .into_iter()
            .filter(|channel| {
                channel
                    .members
                    .iter()
                    .any(|member| member.session_id.as_deref() == Some(session_id))
            })
            .map(|channel| channel.summary.channel_id)
            .collect()
    }

    pub(crate) async fn resolve_approval_run(
        self: &Arc<Self>,
        session_id: &str,
        request: ResolveApprovalsRequest,
    ) -> Result<RunView> {
        if let Some(idempotency_key) = request.idempotency_key.as_deref() {
            let idempotency_key = normalize_session_run_idempotency_key(idempotency_key)?;
            let key_hash = session_run_idempotency_key_hash(&idempotency_key);
            let receipt_key =
                run_operation_idempotency_receipt_key(APPROVAL_OPERATION, session_id, &key_hash);
            if let Some(receipt) = self
                .session_service
                .run_operation_idempotency_receipt(&receipt_key)
                .await
                && matches!(receipt, SessionRunIdempotencyReceiptState::Submitted { .. })
            {
                let request = approval_request_without_idempotency(request);
                let request_fingerprint = approval_request_fingerprint(receipt.run_id(), &request)?;
                if receipt.request_fingerprint() != request_fingerprint {
                    return Err(DaemonProblem::idempotency_conflict(
                        "run operation idempotency key was reused with a different request payload",
                    )
                    .into());
                }
                return self.run_service.get_run(receipt.run_id()).await;
            }
        }
        let active_run_id = self.run_service.require_active_run_id(session_id).await?;
        self.resolve_run_approvals(&active_run_id, request).await
    }

    pub(crate) async fn resolve_run_approvals(
        self: &Arc<Self>,
        run_id: &str,
        request: ResolveApprovalsRequest,
    ) -> Result<RunView> {
        if let Some(idempotency_key) = request.idempotency_key.clone() {
            return self
                .resolve_run_approvals_idempotent(run_id, request, &idempotency_key)
                .await;
        }
        self.resume_waiting_run(run_id, request).await
    }

    async fn resolve_run_approvals_idempotent(
        self: &Arc<Self>,
        run_id: &str,
        request: ResolveApprovalsRequest,
        idempotency_key: &str,
    ) -> Result<RunView> {
        let idempotency_key = normalize_session_run_idempotency_key(idempotency_key)?;
        let key_hash = session_run_idempotency_key_hash(&idempotency_key);
        let request = approval_request_without_idempotency(request);
        let request_fingerprint = approval_request_fingerprint(run_id, &request)?;
        let session_id = self.run_service.run_record(run_id).await?.view.session_id;
        let receipt_key =
            run_operation_idempotency_receipt_key(APPROVAL_OPERATION, &session_id, &key_hash);

        if !self
            .try_acquire_session_run_idempotency_submission(&receipt_key)
            .await
        {
            return self
                .await_run_operation_idempotency_submission(
                    &receipt_key,
                    run_id,
                    &request_fingerprint,
                )
                .await;
        }

        let result = async {
            if let Some(view) = self
                .approval_idempotency_replay(&receipt_key, run_id, &request_fingerprint)
                .await?
            {
                return Ok(view);
            }

            let reservation = self
                .session_service
                .begin_run_operation_idempotency(&receipt_key, run_id, &request_fingerprint)
                .await?;
            match reservation {
                SessionRunIdempotencyReservation::Existing {
                    run_id: receipt_run_id,
                } => {
                    let existing = self.run_service.get_run(&receipt_run_id).await?;
                    if receipt_run_id != run_id
                        || existing.status != DaemonRunStatus::WaitingForApproval
                    {
                        return Ok(existing);
                    }
                }
                SessionRunIdempotencyReservation::Pending {
                    run_id: receipt_run_id,
                } if receipt_run_id != run_id => {
                    return self.run_service.get_run(&receipt_run_id).await;
                }
                SessionRunIdempotencyReservation::Pending { .. }
                | SessionRunIdempotencyReservation::Reserved { .. } => {}
            }

            if let Some(view) = self
                .approval_idempotency_replay(&receipt_key, run_id, &request_fingerprint)
                .await?
            {
                return Ok(view);
            }

            let resumed = self.resume_waiting_run(run_id, request).await;
            match resumed {
                Ok(view) => {
                    self.session_service
                        .remember_run_operation_idempotency(
                            &receipt_key,
                            &view.run_id,
                            &request_fingerprint,
                        )
                        .await?;
                    Ok(view)
                }
                Err(error) => {
                    if let Some(view) = self
                        .approval_idempotency_replay(&receipt_key, run_id, &request_fingerprint)
                        .await?
                    {
                        self.session_service
                            .remember_run_operation_idempotency(
                                &receipt_key,
                                &view.run_id,
                                &request_fingerprint,
                            )
                            .await?;
                        return Ok(view);
                    }
                    let _ = self
                        .session_service
                        .forget_run_operation_idempotency(&receipt_key)
                        .await;
                    Err(error)
                }
            }
        }
        .await;

        self.release_session_run_idempotency_submission(&receipt_key)
            .await;
        result
    }

    async fn approval_idempotency_replay(
        &self,
        receipt_key: &str,
        run_id: &str,
        request_fingerprint: &str,
    ) -> Result<Option<RunView>> {
        if let Some(receipt) = self
            .session_service
            .run_operation_idempotency_receipt(receipt_key)
            .await
        {
            if receipt.request_fingerprint() != request_fingerprint {
                return Err(DaemonProblem::idempotency_conflict(
                    "run operation idempotency key was reused with a different request payload",
                )
                .into());
            }
            if matches!(receipt, SessionRunIdempotencyReceiptState::Submitted { .. })
                && let Ok(run) = self.run_service.get_run(receipt.run_id()).await
            {
                return Ok(Some(run));
            }
        }

        let record = self.run_service.run_record(run_id).await?;
        if approval_resume_payload_fingerprint(&record)? == Some(request_fingerprint.to_string()) {
            self.session_service
                .remember_run_operation_idempotency(receipt_key, run_id, request_fingerprint)
                .await?;
            return Ok(Some(record.view));
        }
        Ok(None)
    }

    async fn await_run_operation_idempotency_submission(
        &self,
        receipt_key: &str,
        run_id: &str,
        request_fingerprint: &str,
    ) -> Result<RunView> {
        let deadline = Instant::now() + Duration::from_millis(SESSION_RUN_IDEMPOTENCY_WAIT_MS);
        loop {
            if let Some(view) = self
                .approval_idempotency_replay(receipt_key, run_id, request_fingerprint)
                .await?
            {
                return Ok(view);
            }
            if Instant::now() >= deadline {
                return Err(DaemonProblem::idempotency_conflict(
                    "run operation idempotency key is already pending",
                )
                .into());
            }
            tokio::time::sleep(Duration::from_millis(SESSION_RUN_IDEMPOTENCY_POLL_MS)).await;
        }
    }

    pub(crate) async fn resolve_user_question_run(
        self: &Arc<Self>,
        run_id: &str,
        request: ResolveUserQuestionRequest,
    ) -> Result<RunView> {
        if let Some(idempotency_key) = request.idempotency_key.clone() {
            return self
                .resolve_user_question_run_idempotent(run_id, request, &idempotency_key)
                .await;
        }
        self.resume_waiting_question(run_id, request).await
    }

    pub(super) async fn resolve_user_question(
        self: &Arc<Self>,
        session_id: &str,
        request: ResolveUserQuestionRequest,
    ) -> Result<RunView> {
        if let Some(idempotency_key) = request.idempotency_key.as_deref() {
            let idempotency_key = normalize_session_run_idempotency_key(idempotency_key)?;
            let key_hash = session_run_idempotency_key_hash(&idempotency_key);
            let receipt_key = run_operation_idempotency_receipt_key(
                USER_QUESTION_OPERATION,
                session_id,
                &key_hash,
            );
            if let Some(receipt) = self
                .session_service
                .run_operation_idempotency_receipt(&receipt_key)
                .await
                && matches!(receipt, SessionRunIdempotencyReceiptState::Submitted { .. })
            {
                let request = user_question_request_without_idempotency(request);
                let request_fingerprint =
                    user_question_request_fingerprint(receipt.run_id(), &request)?;
                if receipt.request_fingerprint() != request_fingerprint {
                    return Err(DaemonProblem::idempotency_conflict(
                        "run operation idempotency key was reused with a different request payload",
                    )
                    .into());
                }
                return self.run_service.get_run(receipt.run_id()).await;
            }
        }
        let active_run_id = match self.run_service.require_active_run_id(session_id).await {
            Ok(active_run_id) => active_run_id,
            Err(error) => {
                if let Some(expired) = self
                    .expired_user_question_terminal_error_for_session(
                        session_id,
                        &request.resolution.request_id,
                    )
                    .await?
                {
                    return Err(DaemonProblem::question_expired(expired).into());
                }
                if let Some(cancelled) = self
                    .cancelled_user_question_terminal_error_for_session(
                        session_id,
                        &request.resolution.request_id,
                    )
                    .await?
                {
                    return Err(DaemonProblem::question_state_conflict(cancelled).into());
                }
                return Err(error);
            }
        };
        let active_record = self.run_record(&active_run_id).await?;
        if !run_is_waiting_for_question_request(&active_record, &request.resolution.request_id) {
            if let Some(expired) = self
                .expired_user_question_terminal_error_for_session(
                    session_id,
                    &request.resolution.request_id,
                )
                .await?
            {
                return Err(DaemonProblem::question_expired(expired).into());
            }
            if let Some(cancelled) = self
                .cancelled_user_question_terminal_error_for_session(
                    session_id,
                    &request.resolution.request_id,
                )
                .await?
            {
                return Err(DaemonProblem::question_state_conflict(cancelled).into());
            }
        }
        self.resolve_user_question_run(&active_run_id, request)
            .await
    }

    async fn resolve_user_question_run_idempotent(
        self: &Arc<Self>,
        run_id: &str,
        request: ResolveUserQuestionRequest,
        idempotency_key: &str,
    ) -> Result<RunView> {
        let idempotency_key = normalize_session_run_idempotency_key(idempotency_key)?;
        let key_hash = session_run_idempotency_key_hash(&idempotency_key);
        let request = user_question_request_without_idempotency(request);
        let request_fingerprint = user_question_request_fingerprint(run_id, &request)?;
        let session_id = self.run_service.run_record(run_id).await?.view.session_id;
        let receipt_key =
            run_operation_idempotency_receipt_key(USER_QUESTION_OPERATION, &session_id, &key_hash);

        if !self
            .try_acquire_session_run_idempotency_submission(&receipt_key)
            .await
        {
            return self
                .await_user_question_idempotency_submission(
                    &receipt_key,
                    run_id,
                    &request_fingerprint,
                )
                .await;
        }

        let result = async {
            if let Some(view) = self
                .user_question_idempotency_replay(&receipt_key, run_id, &request_fingerprint)
                .await?
            {
                return Ok(view);
            }

            let reservation = self
                .session_service
                .begin_run_operation_idempotency(&receipt_key, run_id, &request_fingerprint)
                .await?;
            match reservation {
                SessionRunIdempotencyReservation::Existing {
                    run_id: receipt_run_id,
                } => {
                    let existing = self.run_service.get_run(&receipt_run_id).await?;
                    if receipt_run_id != run_id
                        || existing.status != DaemonRunStatus::WaitingForUserQuestion
                    {
                        return Ok(existing);
                    }
                }
                SessionRunIdempotencyReservation::Pending {
                    run_id: receipt_run_id,
                } if receipt_run_id != run_id => {
                    return self.run_service.get_run(&receipt_run_id).await;
                }
                SessionRunIdempotencyReservation::Pending { .. }
                | SessionRunIdempotencyReservation::Reserved { .. } => {}
            }

            let resumed = self.resume_waiting_question(run_id, request).await;
            match resumed {
                Ok(view) => {
                    self.session_service
                        .remember_run_operation_idempotency(
                            &receipt_key,
                            &view.run_id,
                            &request_fingerprint,
                        )
                        .await?;
                    Ok(view)
                }
                Err(error) => {
                    if let Some(view) = self
                        .user_question_idempotency_replay(
                            &receipt_key,
                            run_id,
                            &request_fingerprint,
                        )
                        .await?
                    {
                        self.session_service
                            .remember_run_operation_idempotency(
                                &receipt_key,
                                &view.run_id,
                                &request_fingerprint,
                            )
                            .await?;
                        return Ok(view);
                    }
                    let _ = self
                        .session_service
                        .forget_run_operation_idempotency(&receipt_key)
                        .await;
                    Err(error)
                }
            }
        }
        .await;

        self.release_session_run_idempotency_submission(&receipt_key)
            .await;
        result
    }

    async fn user_question_idempotency_replay(
        &self,
        receipt_key: &str,
        run_id: &str,
        request_fingerprint: &str,
    ) -> Result<Option<RunView>> {
        if let Some(receipt) = self
            .session_service
            .run_operation_idempotency_receipt(receipt_key)
            .await
        {
            if receipt.request_fingerprint() != request_fingerprint {
                return Err(DaemonProblem::idempotency_conflict(
                    "run operation idempotency key was reused with a different request payload",
                )
                .into());
            }
            if matches!(receipt, SessionRunIdempotencyReceiptState::Submitted { .. })
                && let Ok(run) = self.run_service.get_run(receipt.run_id()).await
                && run.status != DaemonRunStatus::WaitingForUserQuestion
            {
                return Ok(Some(run));
            }
        }

        let record = self.run_service.run_record(run_id).await?;
        if record.view.status != DaemonRunStatus::WaitingForUserQuestion
            && user_question_resume_payload_fingerprint(&record)?
                == Some(request_fingerprint.to_string())
        {
            self.session_service
                .remember_run_operation_idempotency(receipt_key, run_id, request_fingerprint)
                .await?;
            return Ok(Some(record.view));
        }
        Ok(None)
    }

    async fn await_user_question_idempotency_submission(
        &self,
        receipt_key: &str,
        run_id: &str,
        request_fingerprint: &str,
    ) -> Result<RunView> {
        let deadline = Instant::now() + Duration::from_millis(SESSION_RUN_IDEMPOTENCY_WAIT_MS);
        loop {
            if let Some(view) = self
                .user_question_idempotency_replay(receipt_key, run_id, request_fingerprint)
                .await?
            {
                return Ok(view);
            }
            if Instant::now() >= deadline {
                return Err(DaemonProblem::idempotency_conflict(
                    "run operation idempotency key is already pending",
                )
                .into());
            }
            tokio::time::sleep(Duration::from_millis(SESSION_RUN_IDEMPOTENCY_POLL_MS)).await;
        }
    }

    async fn expired_user_question_terminal_error_for_session(
        &self,
        session_id: &str,
        request_id: &str,
    ) -> Result<Option<String>> {
        let runs = self.run_service.list_runs(Some(session_id)).await?;
        for run in runs.into_iter().rev() {
            let record = self.run_service.run_record(&run.run_id).await?;
            if let Some(error) = expired_user_question_terminal_error(&record, request_id) {
                return Ok(Some(error));
            }
        }
        Ok(None)
    }

    async fn cancelled_user_question_terminal_error_for_session(
        &self,
        session_id: &str,
        request_id: &str,
    ) -> Result<Option<String>> {
        let runs = self.run_service.list_runs(Some(session_id)).await?;
        for run in runs.into_iter().rev() {
            let record = self.run_service.run_record(&run.run_id).await?;
            if let Some(error) = cancelled_user_question_terminal_error(&record, request_id) {
                return Ok(Some(error));
            }
        }
        Ok(None)
    }

    pub(super) async fn resume_waiting_run(
        self: &Arc<Self>,
        run_id: &str,
        request: ResolveApprovalsRequest,
    ) -> Result<RunView> {
        let resolutions = request.resolutions.clone();
        let view = self
            .run_service
            .resume_waiting_approval_run(run_id, request)
            .await?;
        let session_id = view.session_id.clone();
        let agent_id = AgentId(view.agent_id.clone());
        let _ = self
            .dispatch_daemon_hook(
                HookEventName::ElicitationResult,
                Some("approvals".to_string()),
                Some(session_id.clone()),
                Some(agent_id.0.clone()),
                Some(run_id.to_string()),
                json!({
                    "run": view,
                    "resolutions": resolutions,
                }),
            )
            .await;
        self.launch_run(run_id.to_string());
        self.publish_agent_state(
            &session_id,
            &agent_id,
            kheish_agent::AgentStatus::Running,
            0,
            0,
        );
        Ok(view)
    }

    pub(super) async fn resume_waiting_question(
        self: &Arc<Self>,
        run_id: &str,
        request: ResolveUserQuestionRequest,
    ) -> Result<RunView> {
        let run = self.run_record(run_id).await?;
        if run.view.status.is_terminal()
            && let Some(error) =
                expired_user_question_terminal_error(&run, &request.resolution.request_id)
        {
            return Err(DaemonProblem::question_expired(error).into());
        }
        let session_id = run.view.session_id.clone();
        let agent_id = AgentId(run.view.agent_id.clone());
        let resolution = request.resolution.clone();
        if let RunRequestPayload::ParentClarification {
            request: clarification,
            completion,
        } = &run.payload
        {
            if let Some(pending) = run
                .view
                .pending_questions
                .iter()
                .find(|question| question.id == resolution.request_id)
                .cloned()
            {
                if let Some(expires_at_ms) = pending.expires_at_ms {
                    let now = now_ms();
                    if expires_at_ms <= now {
                        let _ = self.expire_waiting_user_question_run(run_id, now).await?;
                        return Err(DaemonProblem::question_expired(format!(
                            "user-question request {} expired at {}",
                            pending.id, expires_at_ms
                        ))
                        .into());
                    }
                }
            } else if completion.resolution.as_ref() != Some(&resolution) {
                return Err(DaemonProblem::question_resolution_conflict(format!(
                    "run {run_id} was already resolved with a different answer"
                ))
                .into());
            }
            let reason = completion
                .reason
                .clone()
                .unwrap_or_else(|| parent_clarification_reason_for_direct_resolution(&resolution));
            return self
                .complete_parent_clarification_run(
                    run_id,
                    &session_id,
                    &agent_id,
                    clarification.clone(),
                    resolution,
                    reason,
                )
                .await;
        }
        if run.view.status != DaemonRunStatus::WaitingForUserQuestion {
            return Err(DaemonProblem::question_state_conflict(format!(
                "run {run_id} is not waiting for user input"
            ))
            .into());
        }
        let pending = run
            .view
            .pending_questions
            .iter()
            .find(|question| question.id == resolution.request_id)
            .ok_or_else(|| {
                anyhow!(
                    "user-question resolution {} does not match any pending request on run {run_id}",
                    resolution.request_id
                )
            })?
            .clone();
        if let Some(expires_at_ms) = pending.expires_at_ms {
            let now = now_ms();
            if expires_at_ms <= now {
                let _ = self.expire_waiting_user_question_run(run_id, now).await?;
                return Err(DaemonProblem::question_expired(format!(
                    "user-question request {} expired at {}",
                    pending.id, expires_at_ms
                ))
                .into());
            }
        }
        let _ = render_user_question_resolution(&pending, &resolution)?;
        let view = self
            .run_service
            .resume_waiting_user_question_run(run_id, request)
            .await?;
        let _ = self
            .dispatch_daemon_hook(
                HookEventName::ElicitationResult,
                Some("user_questions".to_string()),
                Some(session_id.clone()),
                Some(agent_id.0.clone()),
                Some(run_id.to_string()),
                json!({
                    "run": view,
                    "resolution": resolution,
                }),
            )
            .await;
        self.launch_run(run_id.to_string());
        self.publish_agent_state(
            &session_id,
            &agent_id,
            kheish_agent::AgentStatus::Running,
            0,
            0,
        );
        Ok(view)
    }

    pub(super) async fn schedule_run(self: &Arc<Self>, record: RunRecord) -> Result<RunView> {
        self.schedule_run_with_idle_policy(record, false).await
    }

    async fn schedule_run_with_idle_policy(
        self: &Arc<Self>,
        record: RunRecord,
        require_idle: bool,
    ) -> Result<RunView> {
        let _runtime_config_snapshot = self.runtime_config_service.snapshot_guard().await;
        self.ensure_resolved_route_ready_unlocked(record.view.request.provider.as_deref())
            .await?;
        let result = if require_idle {
            self.run_service.schedule_run_requiring_idle(record).await?
        } else {
            self.run_service.schedule_run(record).await?
        };
        let view = result.view;
        if result.started_immediately {
            info!(
                session_id = %view.session_id,
                agent_id = %view.agent_id,
                run_id = %view.run_id,
                run_kind = ?view.kind,
                provider = view.request.provider.as_deref(),
                model = view.request.model.as_deref(),
                "accepted and started daemon run"
            );
            self.publish_agent_state(
                &view.session_id,
                &AgentId(view.agent_id.clone()),
                kheish_agent::AgentStatus::Running,
                0,
                0,
            );
            self.launch_run(view.run_id.clone());
        } else {
            info!(
                session_id = %view.session_id,
                agent_id = %view.agent_id,
                run_id = %view.run_id,
                run_kind = ?view.kind,
                queue_position = view.queued_position,
                provider = view.request.provider.as_deref(),
                model = view.request.model.as_deref(),
                "accepted and queued daemon run"
            );
        }
        Ok(view)
    }

    pub(super) fn launch_run(self: &Arc<Self>, run_id: String) {
        self.debug.pin_run_level(&run_id, self.debug.level());
        let state = self.clone();
        tokio::spawn(async move {
            let result = state.execute_run(&run_id).await;
            if let Err(error) = result {
                let _ = state.fail_run(&run_id, error.to_string()).await;
            }
            state.clear_run_debug_level_if_terminal(&run_id).await;
        });
    }

    async fn clear_run_debug_level_if_terminal(&self, run_id: &str) {
        match self.run_service.run_record(run_id).await {
            Ok(record) if record.view.status.is_terminal() => self.debug.clear_run_level(run_id),
            Ok(_) => {}
            Err(error) => {
                debug!(
                    run_id = %run_id,
                    error = ?error,
                    "skipping debug run-level cleanup because run status could not be loaded"
                );
            }
        }
    }

    pub(super) async fn execute_run(self: &Arc<Self>, run_id: &str) -> Result<()> {
        let record = self.run_service.run_record(run_id).await?;
        if record.view.status != DaemonRunStatus::Running {
            debug!(
                session_id = %record.view.session_id,
                agent_id = %record.view.agent_id,
                run_id = %record.view.run_id,
                run_kind = ?record.view.kind,
                status = ?record.view.status,
                "skipping daemon run execution because the run is no longer running"
            );
            return Ok(());
        }
        self.ensure_resolved_route_ready(record.view.request.provider.as_deref())
            .await?;

        debug!(
            session_id = %record.view.session_id,
            agent_id = %record.view.agent_id,
            run_id = %record.view.run_id,
            run_kind = ?record.view.kind,
            provider = record.view.request.provider.as_deref(),
            model = record.view.request.model.as_deref(),
            "executing daemon run"
        );
        let session_id = record.view.session_id.clone();
        let agent_id = AgentId(record.view.agent_id.clone());
        let run_scope = ExecutionScope {
            session_id: session_id.clone(),
            agent_id: Some(agent_id.0.clone()),
            run_id: Some(run_id.to_string()),
            principal_id: Some(format!("agent:{}", agent_id.0)),
            provider: record.view.request.provider.clone(),
            model: record.view.request.model.clone(),
            ..ExecutionScope::default()
        };
        let result = scope_execution(run_scope, CancellationToken::new(), async {
            match record.payload.clone() {
                RunRequestPayload::Input { request, .. } => {
                    let request =
                        self.apply_fallback_reply_targets(&request, &record.reply_targets);
                    let input = self.build_input_envelope(&session_id, &request).await?;
                    self.orchestrator
                        .submit_input_for_run(
                            &agent_id,
                            input,
                            request.generation.clone().unwrap_or_default(),
                            Some(run_id.to_string()),
                            record.view.request.provider.clone(),
                            record.view.request.model.clone(),
                        )
                        .await
                }
                RunRequestPayload::ScheduledInput { request, .. } => {
                    let request =
                        self.apply_fallback_reply_targets(&request, &record.reply_targets);
                    let input = self.build_input_envelope(&session_id, &request).await?;
                    self.orchestrator
                        .submit_input_for_run(
                            &agent_id,
                            input,
                            request.generation.clone().unwrap_or_default(),
                            Some(run_id.to_string()),
                            record.view.request.provider.clone(),
                            record.view.request.model.clone(),
                        )
                        .await
                }
                RunRequestPayload::ObservationMaterialization { request } => {
                    self.execute_observation_materialization_run(
                        run_id,
                        &session_id,
                        &record.reply_targets,
                        request,
                    )
                    .await
                }
                RunRequestPayload::ScheduledObservationMaterialization { request, .. } => {
                    self.execute_observation_materialization_run(
                        run_id,
                        &session_id,
                        &record.reply_targets,
                        request,
                    )
                    .await
                }
                RunRequestPayload::MailboxDelivery { agent_id, messages } => {
                    self.execute_mailbox_run(
                        run_id,
                        &session_id,
                        &AgentId(agent_id),
                        messages,
                        record.view.request.provider.clone(),
                        record.view.request.model.clone(),
                    )
                    .await
                }
                RunRequestPayload::ChannelDelivery { request } => {
                    self.execute_channel_delivery_run(
                        run_id,
                        &session_id,
                        &agent_id,
                        request,
                        record.view.request.provider.clone(),
                        record.view.request.model.clone(),
                    )
                    .await
                }
                RunRequestPayload::ParentClarification { request, .. } => {
                    self.execute_parent_clarification_run(run_id, &session_id, &agent_id, request)
                        .await
                }
                RunRequestPayload::ApprovalResume { request, .. } => {
                    self.orchestrator
                        .resume_approvals_for_run(
                            &agent_id,
                            request.resolutions,
                            Some(run_id.to_string()),
                            record.view.request.provider.clone(),
                            record.view.request.model.clone(),
                        )
                        .await
                }
                RunRequestPayload::UserQuestionResume { request, .. } => {
                    self.orchestrator
                        .resume_user_question_for_run(
                            &agent_id,
                            request.resolution,
                            Some(run_id.to_string()),
                            record.view.request.provider.clone(),
                            record.view.request.model.clone(),
                        )
                        .await
                }
            }
        })
        .await;

        let outcome = match result {
            Ok(snapshot) => self.apply_run_snapshot(run_id, snapshot).await,
            Err(error) => {
                if error.to_string().contains("interrupted") {
                    self.interrupt_run(run_id).await
                } else {
                    self.pause_goal_after_permanent_continuation_error(&record.view, &error)
                        .await;
                    self.fail_run(run_id, error.to_string()).await
                }
            }
        };
        outcome
    }

    pub(super) async fn execute_mailbox_run(
        self: &Arc<Self>,
        run_id: &str,
        session_id: &str,
        agent_id: &AgentId,
        messages: Vec<MailboxMessage>,
        provider: Option<String>,
        model: Option<String>,
    ) -> Result<ManagedAgentSnapshot> {
        if messages.is_empty() {
            return self.orchestrator.snapshot(agent_id).await;
        }
        let input = self
            .build_mailbox_input_envelope(session_id, agent_id, &messages)
            .await?;
        let snapshot = self.orchestrator.snapshot(agent_id).await?;
        let mut generation = snapshot
            .agent
            .fork_context
            .as_ref()
            .and_then(|fork_context| fork_context.generation.clone())
            .unwrap_or_default();
        if model.is_some() {
            generation.model = model.clone();
        }
        self.orchestrator
            .submit_input_for_run(
                agent_id,
                input,
                generation,
                Some(run_id.to_string()),
                provider,
                model,
            )
            .await
    }

    pub(super) async fn execute_parent_clarification_run(
        self: &Arc<Self>,
        run_id: &str,
        _session_id: &str,
        agent_id: &AgentId,
        request: ParentClarificationRunRequest,
    ) -> Result<ManagedAgentSnapshot> {
        if self
            .run_service
            .mark_waiting_for_user_question(run_id, request.request)
            .await?
            .is_none()
        {
            return self.live_snapshot(agent_id).await;
        }
        self.live_snapshot(agent_id).await
    }

    pub(super) async fn complete_parent_clarification_run(
        self: &Arc<Self>,
        run_id: &str,
        session_id: &str,
        agent_id: &AgentId,
        request: ParentClarificationRunRequest,
        resolution: UserQuestionResolution,
        reason: ParentClarificationCompletionReason,
    ) -> Result<RunView> {
        let rendered = render_user_question_resolution(&request.request, &resolution)?;
        let _completion_guard = self
            .run_service
            .acquire_parent_clarification_completion_slot(run_id)
            .await;
        self.run_service
            .capture_parent_clarification_resolution(run_id, &resolution, reason.clone())
            .await?
            .ok_or_else(|| anyhow!("run {run_id} is not a parent clarification run"))?;
        let mut completion = self
            .run_service
            .parent_clarification_completion_state(run_id)
            .await?
            .ok_or_else(|| anyhow!("run {run_id} is not a parent clarification run"))?;
        let payload = json!({
            "type": PARENT_CLARIFICATION_ANSWER_MESSAGE_TYPE,
            "request_id": request.request.id,
            "declined": rendered["declined"],
            "summary": rendered["summary"],
            "answers": rendered["answers"],
            "justification": rendered["justification"],
        });
        let parent_answer_message_id =
            format!("parent-clarification-answer-{}", request.request.id);
        let pending_mailbox_message = MailboxMessage::new(
            parent_answer_message_id.clone(),
            agent_id.clone(),
            AgentId(request.requester_agent_id.clone()),
            PARENT_CLARIFICATION_ANSWER_SUBJECT.to_string(),
            payload.clone(),
            now_ms(),
            None,
        );
        let target_agent_id = request.requester_agent_id.clone();
        let target_agent = AgentId(target_agent_id.clone());
        let target_accepts_mailbox = self
            .supervisor
            .get(&target_agent)
            .is_some_and(|agent| agent.closed_at_ms.is_none())
            && self.orchestrator.has_runtime(&target_agent);
        if !target_accepts_mailbox {
            if !completion.mailbox_posted
                && self.supervisor.dead_letter_mailbox_messages(
                    &target_agent,
                    vec![pending_mailbox_message],
                    "parent clarification answer could not be delivered because the child agent is closed",
                ) > 0
            {
                self.persist_topology().await?;
            }
            completion = self
                .run_service
                .mark_parent_clarification_mailbox_posted(run_id)
                .await?
                .ok_or_else(|| anyhow!("run {run_id} is not a parent clarification run"))?;
        } else {
            let mailbox_pending = self
                .supervisor
                .peek_mailbox(&target_agent)
                .into_iter()
                .any(|message| message.matches_delivery_payload(&pending_mailbox_message));
            let mailbox_delivery_already_persisted = self
                .run_service
                .session_has_mailbox_delivery_message(
                    &request.requester_session_id,
                    &target_agent_id,
                    &agent_id.0,
                    PARENT_CLARIFICATION_ANSWER_SUBJECT,
                    &payload,
                )
                .await?;
            if mailbox_delivery_already_persisted {
                if mailbox_pending
                    && self
                        .supervisor
                        .ack_mailbox_message(&target_agent, &pending_mailbox_message)
                {
                    self.persist_topology().await?;
                }
                completion = self
                    .run_service
                    .mark_parent_clarification_mailbox_posted(run_id)
                    .await?
                    .ok_or_else(|| anyhow!("run {run_id} is not a parent clarification run"))?;
            } else if mailbox_pending {
                let _ = self.schedule_mailbox_run(&target_agent).await?;
            }
            if !mailbox_delivery_already_persisted && !mailbox_pending {
                self.post_mailbox(PostMailboxRequest {
                    message_id: Some(parent_answer_message_id),
                    from_agent_id: agent_id.0.clone(),
                    to_agent_id: request.requester_agent_id.clone(),
                    subject: PARENT_CLARIFICATION_ANSWER_SUBJECT.to_string(),
                    ttl_ms: None,
                    payload,
                })
                .await?;
                completion = self
                    .run_service
                    .mark_parent_clarification_mailbox_posted(run_id)
                    .await?
                    .ok_or_else(|| anyhow!("run {run_id} is not a parent clarification run"))?;
            }
        }

        let note = if resolution.declined {
            format!(
                "Declined parent clarification request {} for agent {}.",
                request.request.id, request.requester_agent_id
            )
        } else {
            format!(
                "Forwarded parent clarification answer for {} to agent {}.",
                request.request.id, request.requester_agent_id
            )
        };
        if !completion.output_emitted
            && self
                .run_record(run_id)
                .await?
                .view
                .outputs
                .iter()
                .any(|output| output.content == note)
        {
            completion = self
                .run_service
                .mark_parent_clarification_output_emitted(run_id)
                .await?
                .ok_or_else(|| anyhow!("run {run_id} is not a parent clarification run"))?;
        }
        if !completion.output_emitted {
            self.emit_session_output(session_id, Some(run_id), RichOutput::text(note), None)
                .await?;
            completion = self
                .run_service
                .mark_parent_clarification_output_emitted(run_id)
                .await?
                .ok_or_else(|| anyhow!("run {run_id} is not a parent clarification run"))?;
        }
        if !completion.resolution_recorded {
            let resolution_event = RunEvent::ParentClarificationResolved {
                requester_agent_id: request.requester_agent_id.clone(),
                requester_session_id: request.requester_session_id.clone(),
                request_id: resolution.request_id.clone(),
                declined: resolution.declined,
                resolution: resolution.clone(),
            };
            self.run_service
                .append_run_event_once(&self.run_record(run_id).await?.view, resolution_event)?;
            completion = self
                .run_service
                .mark_parent_clarification_resolution_recorded(run_id)
                .await?
                .ok_or_else(|| anyhow!("run {run_id} is not a parent clarification run"))?;
        }
        if self.run_record(run_id).await?.view.status != DaemonRunStatus::Completed {
            self.run_service.mark_completed(run_id).await?;
        }
        let record = self.run_record(run_id).await?;
        if self.run_service.run_has_completed_event(run_id)? {
            completion = self
                .run_service
                .mark_parent_clarification_completion_recorded(run_id)
                .await?
                .ok_or_else(|| anyhow!("run {run_id} is not a parent clarification run"))?;
        }
        if !completion.hook_dispatched {
            let completed_view = record.view.clone();
            let _ = self
                .dispatch_daemon_hook(
                    HookEventName::ElicitationResult,
                    Some("user_questions".to_string()),
                    Some(session_id.to_string()),
                    Some(agent_id.0.clone()),
                    Some(run_id.to_string()),
                    json!({
                        "run": completed_view,
                        "resolution": resolution,
                        "requester_agent_id": request.requester_agent_id,
                    }),
                )
                .await;
            completion = self
                .run_service
                .mark_parent_clarification_hook_dispatched(run_id)
                .await?
                .ok_or_else(|| anyhow!("run {run_id} is not a parent clarification run"))?;
        }
        if !completion.completion_recorded {
            self.run_service
                .append_run_event_once(&record.view, RunEvent::Completed)?;
            let _ = self
                .run_service
                .mark_parent_clarification_completion_recorded(run_id)
                .await?
                .ok_or_else(|| anyhow!("run {run_id} is not a parent clarification run"))?;
        }
        self.publish_run(&self.run_record(run_id).await?.view);
        let snapshot = self.live_snapshot(agent_id).await?;
        self.publish_snapshot_for_session(session_id, &snapshot);
        self.finish_active_run(session_id, run_id).await?;
        Ok(self.run_record(run_id).await?.view)
    }

    pub(super) async fn apply_run_snapshot(
        self: &Arc<Self>,
        run_id: &str,
        snapshot: ManagedAgentSnapshot,
    ) -> Result<()> {
        let Some(update) = self.run_service.apply_snapshot(run_id, &snapshot).await? else {
            return Ok(());
        };
        let view = update.record.view.clone();
        info!(
            session_id = %view.session_id,
            agent_id = %view.agent_id,
            run_id = %view.run_id,
            status = ?view.status,
            pending_approvals = view.pending_approval_ids.len(),
            pending_questions = view.pending_question_ids.len(),
            outputs = view.outputs.len(),
            "applied daemon run snapshot"
        );
        if view.status == DaemonRunStatus::WaitingForUserQuestion {
            self.run_service.append_run_event_once(
                &view,
                RunEvent::WaitingForUserQuestion {
                    request_ids: view.pending_question_ids.clone(),
                    requests: snapshot.pending_questions.clone(),
                },
            )?;
            let _ = self
                .dispatch_daemon_hook(
                    HookEventName::Elicitation,
                    Some("user_questions".to_string()),
                    Some(view.session_id.clone()),
                    Some(view.agent_id.clone()),
                    Some(run_id.to_string()),
                    json!({
                        "run": view,
                        "pending_questions": snapshot.pending_questions,
                    }),
                )
                .await;
        } else if view.status == DaemonRunStatus::WaitingForApproval {
            self.run_service.append_run_event_once(
                &view,
                RunEvent::WaitingForApproval {
                    request_ids: view.pending_approval_ids.clone(),
                    requests: snapshot.pending_approvals.clone(),
                },
            )?;
            let _ = self
                .dispatch_daemon_hook(
                    HookEventName::Elicitation,
                    Some("approvals".to_string()),
                    Some(view.session_id.clone()),
                    Some(view.agent_id.clone()),
                    Some(run_id.to_string()),
                    json!({
                        "run": view,
                        "pending_approvals": snapshot.pending_approvals,
                    }),
                )
                .await;
        } else {
            self.run_service
                .append_run_event_once(&view, RunEvent::Completed)?;
        }
        self.sync_project_tasks_for_run_or_warn(&view, "apply_run_snapshot")
            .await;
        if view.status.is_terminal() {
            let _ = self
                .goal_service
                .account_run_view(&view.session_id, &view)
                .await
                .map_err(|error| {
                    tracing::warn!(
                        session_id = %view.session_id,
                        run_id = %view.run_id,
                        error = %error,
                        "failed to account session goal usage"
                    );
                    error
                });
        }
        self.publish_run(&view);
        self.publish_snapshot_for_session(&view.session_id, &snapshot);
        if snapshot.agent.retention == kheish_agent::ChildRetentionPolicy::CloseOnSettle {
            self.persist_topology().await?;
        }
        if update.record.is_channel_delivery_lineage() && view.status.is_terminal() {
            self.settle_channel_delivery_run(run_id).await?;
        }
        if let Some(session_id) = update.next_session_id {
            self.settle_scheduled_run(run_id).await?;
            self.finish_active_run(&session_id, run_id).await?;
        }
        self.persist_run_memory_record(&update.record).await;
        self.clear_run_spawn_count(run_id);
        if let Err(error) = self.reap_close_on_settle_agents().await {
            warn!(
                session_id = %view.session_id,
                agent_id = %view.agent_id,
                run_id = %view.run_id,
                error = %error,
                "failed to reap close_on_settle agents after run snapshot"
            );
        }
        if view.status.is_terminal() {
            self.schedule_service.notify().notify_waiters();
        }
        Ok(())
    }

    pub(super) async fn fail_run(self: &Arc<Self>, run_id: &str, error: String) -> Result<()> {
        let Some(record) = self.run_service.mark_failed(run_id, &error).await? else {
            return Ok(());
        };
        let session_id = record.view.session_id.clone();
        warn!(
            session_id = %record.view.session_id,
            agent_id = %record.view.agent_id,
            run_id = %record.view.run_id,
            run_kind = ?record.view.kind,
            error = %error,
            "daemon run failed"
        );
        self.persist_run_memory_record(&record).await;
        self.run_service.append_run_event_once(
            &record.view,
            RunEvent::Failed {
                error: error.clone(),
            },
        )?;
        self.sync_project_tasks_for_run_or_warn(&record.view, "fail_run")
            .await;
        self.publish_run(&record.view);
        if let Err(error) = self
            .fail_close_on_settle_descendants(
                &record.view.agent_id,
                &format!("parent run {run_id} failed: {error}"),
            )
            .await
        {
            warn!(
                session_id = %record.view.session_id,
                agent_id = %record.view.agent_id,
                run_id = %record.view.run_id,
                error = %error,
                "failed to force-close close_on_settle descendants after parent run failure"
            );
        }
        if record.is_channel_delivery_lineage() {
            self.settle_channel_delivery_run(run_id).await?;
        }
        let mailbox_retry_agent = self
            .requeue_failed_mailbox_delivery(&record, &error)
            .await?;
        self.settle_scheduled_run(run_id).await?;
        self.finish_active_run(&session_id, run_id).await?;
        self.clear_run_spawn_count(run_id);
        if let Some(agent_id) = mailbox_retry_agent {
            let _ = self.schedule_mailbox_run(&agent_id).await?;
        }
        if let Err(error) = self.reap_close_on_settle_agents().await {
            warn!(
                session_id = %record.view.session_id,
                agent_id = %record.view.agent_id,
                run_id = %record.view.run_id,
                error = %error,
                "failed to reap close_on_settle agents after failed run"
            );
        }
        self.schedule_service.notify().notify_waiters();
        Ok(())
    }

    async fn requeue_failed_mailbox_delivery(
        self: &Arc<Self>,
        record: &RunRecord,
        error: &str,
    ) -> Result<Option<AgentId>> {
        let RunRequestPayload::MailboxDelivery { agent_id, messages } = &record.payload else {
            return Ok(None);
        };
        if messages.is_empty() {
            return Ok(None);
        }

        let agent_id = AgentId(agent_id.clone());
        let _mailbox_guard = self.mailbox_topology_lock.lock().await;
        let now = now_ms();
        let agent_closed = self
            .supervisor
            .get(&agent_id)
            .map(|agent| agent.closed_at_ms.is_some())
            .unwrap_or(true);
        let mut expired_messages = Vec::new();
        let mut closed_agent_messages = Vec::new();
        let mut max_attempt_messages = Vec::new();
        let mut requeued = 0usize;

        for message in messages {
            if message.is_expired(now) {
                expired_messages.push(message.clone());
                continue;
            }
            if agent_closed {
                closed_agent_messages.push(message.clone());
                continue;
            }
            if message.delivery_attempts >= MAILBOX_MAX_DELIVERY_ATTEMPTS {
                max_attempt_messages.push(message.clone());
                continue;
            }
            if self.supervisor.post_if_absent(
                message.pending_after_delivery_error(format!("mailbox delivery failed: {error}")),
            ) {
                requeued += 1;
            }
        }

        let mut changed = requeued > 0;
        changed |= self.supervisor.dead_letter_mailbox_messages(
            &agent_id,
            expired_messages,
            "mailbox message expired after failed delivery",
        ) > 0;
        changed |= self.supervisor.dead_letter_mailbox_messages(
            &agent_id,
            closed_agent_messages,
            "agent closed after mailbox delivery failed",
        ) > 0;
        changed |= self.supervisor.dead_letter_mailbox_messages(
            &agent_id,
            max_attempt_messages,
            &format!(
                "mailbox delivery failed after {MAILBOX_MAX_DELIVERY_ATTEMPTS} attempts: {error}"
            ),
        ) > 0;
        if changed {
            self.persist_topology().await?;
        }

        Ok((requeued > 0).then_some(agent_id))
    }

    pub(super) async fn interrupt_run(self: &Arc<Self>, run_id: &str) -> Result<()> {
        if self
            .complete_or_decline_waiting_parent_clarification(
                run_id,
                "interrupted by operator".to_string(),
                ParentClarificationCompletionReason::Interrupted,
            )
            .await?
            .is_some()
        {
            return Ok(());
        }
        let Some(record) = self.run_service.mark_interrupted(run_id).await? else {
            return Ok(());
        };
        let session_id = record.view.session_id.clone();
        info!(
            session_id = %record.view.session_id,
            agent_id = %record.view.agent_id,
            run_id = %record.view.run_id,
            run_kind = ?record.view.kind,
            "daemon run interrupted"
        );
        self.persist_run_memory_record(&record).await;
        self.run_service
            .append_run_event_once(&record.view, RunEvent::Interrupted)?;
        self.sync_project_tasks_for_run_or_warn(&record.view, "interrupt_run")
            .await;
        self.publish_run(&record.view);
        if record.is_channel_delivery_lineage() {
            self.settle_channel_delivery_run(run_id).await?;
        }
        let mailbox_retry_agent = self
            .requeue_failed_mailbox_delivery(&record, "mailbox delivery interrupted")
            .await?;
        self.settle_scheduled_run(run_id).await?;
        self.finish_active_run(&session_id, run_id).await?;
        self.clear_run_spawn_count(run_id);
        if let Some(agent_id) = mailbox_retry_agent {
            let _ = self.schedule_mailbox_run(&agent_id).await?;
        }
        self.schedule_service.notify().notify_waiters();
        Ok(())
    }

    pub(crate) async fn cancel_run(self: &Arc<Self>, run_id: &str) -> Result<RunView> {
        self.cancel_run_with_error(run_id, None).await
    }

    pub(crate) async fn cancel_user_question_run(
        self: &Arc<Self>,
        run_id: &str,
        request_id: &str,
        request: CancelUserQuestionRequest,
    ) -> Result<RunView> {
        if let Some(idempotency_key) = request.idempotency_key.clone() {
            return self
                .cancel_user_question_run_idempotent(run_id, request_id, request, &idempotency_key)
                .await;
        }
        self.cancel_user_question_run_once(run_id, request_id, request.justification)
            .await
    }

    async fn cancel_user_question_run_idempotent(
        self: &Arc<Self>,
        run_id: &str,
        request_id: &str,
        request: CancelUserQuestionRequest,
        idempotency_key: &str,
    ) -> Result<RunView> {
        let idempotency_key = normalize_session_run_idempotency_key(idempotency_key)?;
        let key_hash = session_run_idempotency_key_hash(&idempotency_key);
        let request = user_question_cancel_request_without_idempotency(request);
        let request_fingerprint =
            user_question_cancel_request_fingerprint(run_id, request_id, &request)?;
        let terminal_error =
            cancelled_user_question_error(request_id, request.justification.as_deref());
        let session_id = self.run_service.run_record(run_id).await?.view.session_id;
        let receipt_key = run_operation_idempotency_receipt_key(
            USER_QUESTION_CANCEL_OPERATION,
            &session_id,
            &key_hash,
        );

        if !self
            .try_acquire_session_run_idempotency_submission(&receipt_key)
            .await
        {
            return self
                .await_user_question_cancel_idempotency_submission(
                    &receipt_key,
                    run_id,
                    &request_fingerprint,
                    request_id,
                    &request,
                    &terminal_error,
                )
                .await;
        }

        let result = async {
            if let Some(view) = self
                .user_question_cancel_idempotency_replay(
                    &receipt_key,
                    run_id,
                    &request_fingerprint,
                    request_id,
                    &request,
                    &terminal_error,
                )
                .await?
            {
                return Ok(view);
            }

            let reservation = self
                .session_service
                .begin_run_operation_idempotency(&receipt_key, run_id, &request_fingerprint)
                .await?;
            match reservation {
                SessionRunIdempotencyReservation::Existing {
                    run_id: receipt_run_id,
                } => {
                    return self.run_service.get_run(&receipt_run_id).await;
                }
                SessionRunIdempotencyReservation::Pending {
                    run_id: receipt_run_id,
                } if receipt_run_id != run_id => {
                    return self.run_service.get_run(&receipt_run_id).await;
                }
                SessionRunIdempotencyReservation::Pending { .. }
                | SessionRunIdempotencyReservation::Reserved { .. } => {}
            }

            let cancelled = self
                .cancel_user_question_run_once(run_id, request_id, request.justification.clone())
                .await;
            match cancelled {
                Ok(view) => {
                    self.session_service
                        .remember_run_operation_idempotency(
                            &receipt_key,
                            &view.run_id,
                            &request_fingerprint,
                        )
                        .await?;
                    Ok(view)
                }
                Err(error) => {
                    if let Some(view) = self
                        .user_question_cancel_idempotency_replay(
                            &receipt_key,
                            run_id,
                            &request_fingerprint,
                            request_id,
                            &request,
                            &terminal_error,
                        )
                        .await?
                    {
                        self.session_service
                            .remember_run_operation_idempotency(
                                &receipt_key,
                                &view.run_id,
                                &request_fingerprint,
                            )
                            .await?;
                        return Ok(view);
                    }
                    let _ = self
                        .session_service
                        .forget_run_operation_idempotency(&receipt_key)
                        .await;
                    Err(error)
                }
            }
        }
        .await;

        self.release_session_run_idempotency_submission(&receipt_key)
            .await;
        result
    }

    async fn user_question_cancel_idempotency_replay(
        &self,
        receipt_key: &str,
        run_id: &str,
        request_fingerprint: &str,
        request_id: &str,
        request: &CancelUserQuestionRequest,
        terminal_error: &str,
    ) -> Result<Option<RunView>> {
        if let Some(receipt) = self
            .session_service
            .run_operation_idempotency_receipt(receipt_key)
            .await
        {
            if receipt.request_fingerprint() != request_fingerprint {
                return Err(DaemonProblem::idempotency_conflict(
                    "run operation idempotency key was reused with a different request payload",
                )
                .into());
            }
            if matches!(receipt, SessionRunIdempotencyReceiptState::Submitted { .. })
                && let Ok(run) = self.run_service.get_run(receipt.run_id()).await
            {
                return Ok(Some(run));
            }
        }

        let record = self.run_service.run_record(run_id).await?;
        if user_question_cancel_terminal_matches(&record, request_id, request, terminal_error) {
            self.session_service
                .remember_run_operation_idempotency(receipt_key, run_id, request_fingerprint)
                .await?;
            return Ok(Some(record.view));
        }
        Ok(None)
    }

    async fn await_user_question_cancel_idempotency_submission(
        &self,
        receipt_key: &str,
        run_id: &str,
        request_fingerprint: &str,
        request_id: &str,
        request: &CancelUserQuestionRequest,
        terminal_error: &str,
    ) -> Result<RunView> {
        let deadline = Instant::now() + Duration::from_millis(SESSION_RUN_IDEMPOTENCY_WAIT_MS);
        loop {
            if let Some(view) = self
                .user_question_cancel_idempotency_replay(
                    receipt_key,
                    run_id,
                    request_fingerprint,
                    request_id,
                    request,
                    terminal_error,
                )
                .await?
            {
                return Ok(view);
            }
            if Instant::now() >= deadline {
                return Err(DaemonProblem::idempotency_conflict(
                    "run operation idempotency key is already pending",
                )
                .into());
            }
            tokio::time::sleep(Duration::from_millis(SESSION_RUN_IDEMPOTENCY_POLL_MS)).await;
        }
    }

    async fn cancel_user_question_run_once(
        self: &Arc<Self>,
        run_id: &str,
        request_id: &str,
        justification: Option<String>,
    ) -> Result<RunView> {
        let record = self.run_record(run_id).await?;
        if record.view.status.is_terminal()
            && let Some(error) = expired_user_question_terminal_error(&record, request_id)
        {
            return Err(DaemonProblem::question_expired(error).into());
        }
        if record.view.status.is_terminal()
            && let Some(error) = cancelled_user_question_terminal_error(&record, request_id)
        {
            return Err(DaemonProblem::question_state_conflict(error).into());
        }
        if record.view.status != DaemonRunStatus::WaitingForUserQuestion {
            return Err(DaemonProblem::question_state_conflict(format!(
                "run {run_id} is not waiting for user input"
            ))
            .into());
        }
        let pending = record
            .view
            .pending_questions
            .iter()
            .find(|question| question.id == request_id)
            .cloned()
            .ok_or_else(|| {
                DaemonProblem::question_request_mismatch(format!(
                    "user-question cancel {request_id} does not match any pending request on run {run_id}"
                ))
            })?;
        if let Some(expires_at_ms) = pending.expires_at_ms {
            let now = now_ms();
            if expires_at_ms <= now {
                let _ = self.expire_waiting_user_question_run(run_id, now).await?;
                return Err(DaemonProblem::question_expired(format!(
                    "user-question request {} expired at {}",
                    pending.id, expires_at_ms
                ))
                .into());
            }
        }
        if matches!(
            record.payload,
            RunRequestPayload::ParentClarification { .. }
        ) {
            return self
                .cancel_run_with_error(
                    run_id,
                    Some(justification.unwrap_or_else(|| "cancelled by operator".to_string())),
                )
                .await;
        }
        self.cancel_run_with_error(
            run_id,
            Some(cancelled_user_question_error(
                request_id,
                justification.as_deref(),
            )),
        )
        .await
    }

    async fn cancel_run_with_error(
        self: &Arc<Self>,
        run_id: &str,
        error: Option<String>,
    ) -> Result<RunView> {
        if let Some(view) = self
            .complete_or_decline_waiting_parent_clarification(
                run_id,
                error
                    .clone()
                    .unwrap_or_else(|| "cancelled by operator".to_string()),
                ParentClarificationCompletionReason::Cancelled,
            )
            .await?
        {
            return Ok(view);
        }
        let record = self.run_record(run_id).await?;
        if record.view.status.is_terminal() {
            self.debug.clear_run_level(run_id);
            return Ok(record.view);
        }

        let agent_id = AgentId(record.view.agent_id.clone());
        let cancelled = if let Some(error) = error {
            self.run_service
                .cancel_run_with_error(run_id, Some(error))
                .await?
        } else {
            self.run_service.cancel_run(run_id).await?
        };
        let Some(cancelled) = cancelled else {
            self.clear_run_debug_level_if_terminal(run_id).await;
            return Ok(self.run_record(run_id).await?.view);
        };
        let session_id = cancelled.record.view.session_id.clone();
        let is_active = cancelled.was_active;

        if is_active {
            let _ = self.orchestrator.interrupt(&agent_id).await?;
        }

        let record = cancelled.record;
        self.debug.clear_run_level(run_id);
        info!(
            session_id = %record.view.session_id,
            agent_id = %record.view.agent_id,
            run_id = %record.view.run_id,
            run_kind = ?record.view.kind,
            active = is_active,
            "daemon run cancelled"
        );
        self.persist_run_memory_record(&record).await;
        self.run_service
            .append_run_event_once(&record.view, RunEvent::Cancelled)?;
        self.sync_project_tasks_for_run_or_warn(&record.view, "cancel_run")
            .await;
        self.publish_run(&record.view);
        if record.is_channel_delivery_lineage() {
            self.settle_channel_delivery_run(run_id).await?;
        }
        let mailbox_retry_agent = self
            .requeue_failed_mailbox_delivery(
                &record,
                record
                    .view
                    .error
                    .as_deref()
                    .unwrap_or("mailbox delivery cancelled"),
            )
            .await?;
        self.settle_scheduled_run(run_id).await?;
        if is_active {
            self.start_next_queued_run(&session_id).await?;
        }
        if let Some(agent_id) = mailbox_retry_agent {
            let _ = self.schedule_mailbox_run(&agent_id).await?;
        }
        self.schedule_service.notify().notify_waiters();
        Ok(record.view)
    }

    async fn pause_goal_after_permanent_continuation_error(
        self: &Arc<Self>,
        run: &RunView,
        error: &anyhow::Error,
    ) {
        if run.kind != DaemonRunKind::GoalContinuation
            || !is_permanent_provider_continuation_error(error)
        {
            return;
        }
        let error_message = error.to_string();

        let daemon_metadata = run
            .input_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("daemon"))
            .and_then(Value::as_object);
        let Some(goal_id) = daemon_metadata
            .and_then(|daemon| daemon.get("goal_id"))
            .and_then(Value::as_str)
        else {
            warn!(
                session_id = %run.session_id,
                run_id = %run.run_id,
                error = %error_message,
                "permanent goal continuation failure did not include daemon goal metadata"
            );
            return;
        };
        let Some(goal_version) = daemon_metadata
            .and_then(|daemon| daemon.get("goal_version"))
            .and_then(Value::as_u64)
        else {
            warn!(
                session_id = %run.session_id,
                run_id = %run.run_id,
                goal_id = %goal_id,
                error = %error_message,
                "permanent goal continuation failure did not include daemon goal version"
            );
            return;
        };

        let paused_goal = match self
            .goal_service
            .pause_active_continuation_after_failure(
                &run.session_id,
                goal_id,
                goal_version,
                &run.run_id,
            )
            .await
        {
            Ok(Some(goal)) => goal,
            Ok(None) => return,
            Err(pause_error) => {
                warn!(
                    session_id = %run.session_id,
                    run_id = %run.run_id,
                    goal_id = %goal_id,
                    error = %pause_error,
                    "failed to pause session goal after permanent continuation failure"
                );
                return;
            }
        };

        warn!(
            session_id = %run.session_id,
            run_id = %run.run_id,
            goal_id = %paused_goal.goal_id,
            failure = %error_message,
            "paused session goal after permanent continuation failure"
        );
    }

    pub(super) async fn finish_active_run(
        self: &Arc<Self>,
        session_id: &str,
        run_id: &str,
    ) -> Result<()> {
        let result = self
            .run_service
            .finish_active_run(session_id, run_id)
            .await?;
        self.debug.clear_run_level(run_id);
        if let Some(view) = result.started_run {
            info!(
                session_id = %view.session_id,
                agent_id = %view.agent_id,
                run_id = %view.run_id,
                run_kind = ?view.kind,
                provider = view.request.provider.as_deref(),
                model = view.request.model.as_deref(),
                "started next queued daemon run"
            );
            self.publish_agent_state(
                session_id,
                &AgentId(view.agent_id.clone()),
                kheish_agent::AgentStatus::Running,
                0,
                0,
            );
            self.sync_project_tasks_for_run_or_warn(&view, "finish_active_run")
                .await;
            self.launch_run(view.run_id.clone());
        }
        if !result.finished_run_was_active {
            return Ok(());
        }
        if let Ok(agent_id) = self.agent_id_for_session(session_id).await {
            let _ = self.schedule_mailbox_run(&agent_id).await?;
            if result.session_idle {
                if let Some(view) = self
                    .schedule_goal_continuation_if_idle(session_id, run_id)
                    .await?
                {
                    info!(
                        session_id = %view.session_id,
                        agent_id = %view.agent_id,
                        run_id = %view.run_id,
                        "scheduled session goal continuation"
                    );
                    return Ok(());
                }
                if let Ok(snapshot) = self.orchestrator.snapshot(&agent_id).await {
                    if snapshot.agent.parent.is_some() {
                        let _ = self
                            .dispatch_daemon_hook(
                                HookEventName::TeammateIdle,
                                Some(
                                    snapshot
                                        .agent
                                        .fork_context
                                        .as_ref()
                                        .and_then(|fork| fork.team_name.clone())
                                        .unwrap_or_else(|| snapshot.agent.id.0.clone()),
                                ),
                                Some(session_id.to_string()),
                                Some(snapshot.agent.id.0.clone()),
                                Some(run_id.to_string()),
                                json!({
                                    "agent": snapshot.agent,
                                    "pending_approvals": snapshot.pending_approvals,
                                    "last_assistant_message": snapshot.last_assistant_message,
                                }),
                            )
                            .await;
                    }
                    if snapshot.agent.parent.is_some() {
                        let _ = self
                            .dispatch_daemon_hook(
                                HookEventName::SubagentStop,
                                Some(snapshot.agent.id.0.clone()),
                                Some(session_id.to_string()),
                                Some(snapshot.agent.id.0.clone()),
                                Some(run_id.to_string()),
                                json!({
                                    "agent": snapshot.agent,
                                    "pending_approvals": snapshot.pending_approvals,
                                }),
                            )
                            .await;
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) async fn start_next_queued_run(self: &Arc<Self>, session_id: &str) -> Result<()> {
        let Some(view) = self.run_service.start_next_queued_run(session_id).await? else {
            return Ok(());
        };
        info!(
            session_id = %view.session_id,
            agent_id = %view.agent_id,
            run_id = %view.run_id,
            run_kind = ?view.kind,
            provider = view.request.provider.as_deref(),
            model = view.request.model.as_deref(),
            "started next queued daemon run"
        );
        self.publish_agent_state(
            session_id,
            &AgentId(view.agent_id.clone()),
            kheish_agent::AgentStatus::Running,
            0,
            0,
        );
        self.sync_project_tasks_for_run_or_warn(&view, "start_next_queued_run")
            .await;
        self.launch_run(view.run_id.clone());
        Ok(())
    }

    pub(super) async fn refresh_session_queue(&self, session_id: &str) -> Result<()> {
        self.run_service.refresh_session_queue(session_id).await
    }

    pub(super) async fn run_record(&self, run_id: &str) -> Result<RunRecord> {
        self.run_service.run_record(run_id).await
    }

    pub(super) async fn recovered_memory_bundle(
        &self,
        session_id: &str,
        query: Option<&str>,
        usage: RecoveredMemoryBundleUsage,
    ) -> Option<RecoveredMemoryBundle> {
        let policy = self.run_memory.policy();
        if !policy.enabled || policy.max_prompt_entries == 0 {
            return None;
        }
        let record_prompt_metrics = matches!(usage, RecoveredMemoryBundleUsage::Prompt);
        let candidates = self
            .session_service
            .tracked_run_memory_entries(session_id)
            .await;
        if candidates.is_empty() {
            return None;
        }
        let now = now_ms();
        let mut ranked_entries = Vec::new();
        let mut valid_entries = 0usize;
        let mut invalid_run_ids = Vec::new();
        let mut expired_run_ids = Vec::new();
        for candidate in candidates {
            let run_id = candidate.run_id;
            if run_memory_entry_expired(candidate.recorded_at_ms, now, &policy) {
                expired_run_ids.push(run_id);
                continue;
            }
            let memory = match self.run_service.run_memory_store().load_run_memory(&run_id) {
                Ok(Some(memory)) => memory,
                Ok(None) => {
                    invalid_run_ids.push(run_id);
                    self.run_memory.record_skipped_unreadable();
                    continue;
                }
                Err(error) => {
                    warn!(
                        session_id,
                        run_id,
                        error = %error,
                        "skipping unreadable run memory record"
                    );
                    invalid_run_ids.push(run_id);
                    self.run_memory.record_skipped_unreadable();
                    continue;
                }
            };
            if run_memory_entry_expired(memory.memory.recorded_at_ms, now, &policy) {
                expired_run_ids.push(run_id);
                continue;
            }
            valid_entries = valid_entries.saturating_add(1);
            let rank = rank_run_memory_record(&memory, query);
            ranked_entries.push((
                rank,
                memory.memory.recorded_at_ms,
                memory.memory.run_id.clone(),
                memory.memory,
            ));
        }
        if record_prompt_metrics && query.is_some() {
            self.run_memory
                .record_ranked_candidates(ranked_entries.len());
        }

        let stale_run_ids = invalid_run_ids
            .iter()
            .chain(expired_run_ids.iter())
            .cloned()
            .collect::<Vec<_>>();
        if !stale_run_ids.is_empty() {
            if let Err(error) = self
                .session_service
                .forget_run_memories(session_id, &stale_run_ids)
                .await
            {
                warn!(
                    session_id,
                    error = %error,
                    "failed to prune invalid run memory pointers"
                );
            }
            for run_id in &stale_run_ids {
                if let Err(error) = self
                    .run_service
                    .run_memory_store()
                    .delete_run_memory(run_id)
                {
                    warn!(
                        session_id,
                        run_id,
                        error = %error,
                        "failed to delete stale run memory file"
                    );
                }
            }
        }
        self.run_memory.record_pruned_ttl(expired_run_ids.len());

        ranked_entries.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| right.1.cmp(&left.1))
                .then_with(|| right.2.cmp(&left.2))
        });
        let entries = ranked_entries
            .into_iter()
            .take(policy.max_prompt_entries)
            .map(|(_, _, _, memory)| memory)
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return None;
        }
        let omitted = valid_entries.saturating_sub(entries.len());
        if record_prompt_metrics {
            self.run_memory.record_prompt_limit_omitted(omitted);
        }
        Some(RecoveredMemoryBundle {
            entries,
            truncated: omitted > 0,
        })
    }

    pub(super) async fn persist_run_memory_record(&self, record: &RunRecord) {
        let policy = self.run_memory.policy();
        if !policy.enabled {
            if let Err(error) = self
                .run_service
                .run_memory_store()
                .delete_run_memory(&record.view.run_id)
            {
                warn!(
                    session_id = %record.view.session_id,
                    run_id = %record.view.run_id,
                    error = %error,
                    "failed to delete disabled run memory record"
                );
            }
            if let Err(error) = self
                .session_service
                .forget_run_memories(
                    &record.view.session_id,
                    std::slice::from_ref(&record.view.run_id),
                )
                .await
            {
                warn!(
                    session_id = %record.view.session_id,
                    run_id = %record.view.run_id,
                    error = %error,
                    "failed to forget disabled run memory pointer"
                );
            }
            return;
        }
        let Some((mut memory_record, redaction_count)) =
            build_run_memory_record_with_policy(record, &policy)
        else {
            return;
        };
        let semantic_capture_settings = self.learning_policy_service.semantic_capture_settings();
        let prior_semantic_capture = self
            .run_service
            .run_memory_store()
            .load_run_memory(&record.view.run_id)
            .ok()
            .flatten()
            .map(|existing| existing.semantic_capture);
        memory_record.semantic_capture = match prior_semantic_capture {
            Some(crate::memory::RunMemorySemanticCaptureState::Completed) => {
                crate::memory::RunMemorySemanticCaptureState::Completed
            }
            Some(crate::memory::RunMemorySemanticCaptureState::Skipped) => {
                crate::memory::RunMemorySemanticCaptureState::Skipped
            }
            _ if semantic_capture_settings.is_some() => {
                crate::memory::RunMemorySemanticCaptureState::Pending
            }
            _ => crate::memory::RunMemorySemanticCaptureState::Skipped,
        };
        memory_record.scope_keys = match self
            .learning_scopes_for_session(&record.view.session_id)
            .await
        {
            Ok(scopes) => scopes.into_iter().map(|scope| scope.scope_key()).collect(),
            Err(error) => {
                warn!(
                    session_id = %record.view.session_id,
                    run_id = %record.view.run_id,
                    error = %error,
                    "failed to resolve learning scopes for run memory; falling back to session scope"
                );
                vec![format!("session:{}", record.view.session_id)]
            }
        };
        let captured_at_ms = now_ms();
        if let Err(error) = self
            .run_service
            .run_memory_store()
            .save_run_memory(&memory_record)
        {
            warn!(
                session_id = %record.view.session_id,
                run_id = %record.view.run_id,
                error = %error,
                "failed to persist run memory record"
            );
            return;
        }
        self.run_memory.record_stored();
        self.run_memory.record_redacted_fields(redaction_count);

        let update = match self
            .session_service
            .remember_run_memory_record_with_policy(&memory_record, now_ms(), &policy)
            .await
        {
            Ok(update) => update,
            Err(error) => {
                warn!(
                    session_id = %record.view.session_id,
                    run_id = %record.view.run_id,
                    error = %error,
                    "failed to persist run memory index"
                );
                return;
            }
        };
        self.run_memory
            .record_pruned_ttl(update.pruned_ttl_run_ids.len());
        self.run_memory
            .record_pruned_overflow(update.pruned_overflow_run_ids.len());
        if self
            .learning_policy_service
            .capture_run_summary_candidates()
            .await
        {
            match self
                .learning_service
                .ensure_run_summary_candidate(
                    &record.view.session_id,
                    &record.view.agent_id,
                    &record.view.run_id,
                    &memory_record.memory.summary,
                    memory_record.memory.recorded_at_ms,
                    Some(captured_at_ms.saturating_add(RUN_SUMMARY_CANDIDATE_RETENTION_MS)),
                )
                .await
            {
                Ok(Some(candidate)) => {
                    self.learning_policy_service
                        .enqueue_candidate(candidate.candidate_id)
                        .await;
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(
                        session_id = %record.view.session_id,
                        run_id = %record.view.run_id,
                        error = %error,
                        "failed to persist run-summary learning candidate"
                    );
                }
            }
        }
        if let Some(settings) = semantic_capture_settings
            && memory_record.semantic_capture
                == crate::memory::RunMemorySemanticCaptureState::Pending
        {
            match self
                .capture_semantic_learning_candidates(record, &memory_record, &settings)
                .await
            {
                Ok(candidates) => {
                    for candidate_id in candidates {
                        self.learning_policy_service
                            .enqueue_candidate(candidate_id)
                            .await;
                    }
                    if let Err(error) = self
                        .set_run_memory_semantic_capture_state(
                            &record.view.run_id,
                            crate::memory::RunMemorySemanticCaptureState::Completed,
                        )
                        .await
                    {
                        warn!(
                            session_id = %record.view.session_id,
                            run_id = %record.view.run_id,
                            error = %error,
                            "failed to mark semantic capture completed"
                        );
                    }
                }
                Err(error) => {
                    warn!(
                        session_id = %record.view.session_id,
                        run_id = %record.view.run_id,
                        error = %error,
                        "failed to extract semantic learning candidates"
                    );
                }
            }
        }
        for run_id in &update.pruned_run_ids {
            if let Err(error) = self
                .run_service
                .run_memory_store()
                .delete_run_memory(run_id)
            {
                warn!(
                    session_id = %record.view.session_id,
                    run_id,
                    error = %error,
                    "failed to delete pruned run memory record"
                );
            }
        }
    }

    async fn capture_semantic_learning_candidates(
        &self,
        record: &RunRecord,
        memory_record: &RunMemoryRecord,
        settings: &crate::LearningSemanticCaptureConfig,
    ) -> Result<Vec<String>> {
        if self
            .learning_service
            .has_daemon_semantic_candidate_for_run(&record.view.run_id)
            .await
        {
            return Ok(Vec::new());
        }
        {
            let mut inflight = self.semantic_capture_runs.lock().await;
            if !inflight.insert(record.view.run_id.clone()) {
                return Ok(Vec::new());
            }
        }
        let result = self
            .capture_semantic_learning_candidates_inner(record, memory_record, settings)
            .await;
        self.semantic_capture_runs
            .lock()
            .await
            .remove(&record.view.run_id);
        result
    }

    async fn capture_semantic_learning_candidates_inner(
        &self,
        record: &RunRecord,
        memory_record: &RunMemoryRecord,
        settings: &crate::LearningSemanticCaptureConfig,
    ) -> Result<Vec<String>> {
        let drafts = self
            .learning_extraction_service
            .extract_semantic_candidates(record, memory_record, settings)
            .await?;
        let mut created = Vec::new();
        for draft in drafts {
            let candidate = LearningCandidateView {
                candidate_id: self.next_learning_candidate_id(),
                origin: crate::LearningCandidateOrigin::Daemon,
                scope: kheish_types::LearningScope {
                    kind: kheish_types::LearningScopeKind::Session,
                    id: record.view.session_id.clone(),
                },
                kind: draft.kind,
                sensitivity: kheish_types::LearningSensitivity::Scoped,
                content: draft.content,
                confidence: draft.confidence,
                source: kheish_types::LearningSourceRef {
                    run_id: Some(record.view.run_id.clone()),
                    session_id: Some(record.view.session_id.clone()),
                    agent_id: Some(record.view.agent_id.clone()),
                    ..kheish_types::LearningSourceRef::default()
                },
                evidence_refs: build_semantic_candidate_evidence_refs(record, memory_record),
                created_at_ms: memory_record.memory.recorded_at_ms,
                expires_at_ms: None,
                state: crate::LearningCandidateState::Pending,
                automation_review: None,
                published_learning_id: None,
            };
            if let Some(created_candidate) = self
                .learning_service
                .ensure_daemon_candidate(candidate)
                .await?
            {
                created.push(created_candidate.candidate_id);
            }
        }
        Ok(created)
    }

    async fn set_run_memory_semantic_capture_state(
        &self,
        run_id: &str,
        state: crate::memory::RunMemorySemanticCaptureState,
    ) -> Result<()> {
        let Some(mut record) = self
            .run_service
            .run_memory_store()
            .load_run_memory(run_id)?
        else {
            return Ok(());
        };
        if record.semantic_capture == state {
            return Ok(());
        }
        record.semantic_capture = state;
        self.run_service.run_memory_store().save_run_memory(&record)
    }

    pub(crate) async fn restore_semantic_capture_on_boot(&self) -> Result<()> {
        let Some(settings) = self.learning_policy_service.semantic_capture_settings() else {
            return Ok(());
        };
        let mut run_ids = std::collections::BTreeSet::new();
        for session_id in self.session_service.session_ids().await {
            for entry in self
                .session_service
                .tracked_run_memory_entries(&session_id)
                .await
            {
                run_ids.insert(entry.run_id);
            }
        }
        for run_id in run_ids {
            let Some(memory_record) = self
                .run_service
                .run_memory_store()
                .load_run_memory(&run_id)?
            else {
                continue;
            };
            if memory_record.semantic_capture
                != crate::memory::RunMemorySemanticCaptureState::Pending
            {
                continue;
            }
            let record = match self.run_record(&run_id).await {
                Ok(record) => record,
                Err(error) => {
                    warn!(run_id = %run_id, error = %error, "failed to load run for semantic capture replay");
                    continue;
                }
            };
            if !record.view.status.is_terminal() {
                continue;
            }
            match self
                .capture_semantic_learning_candidates(&record, &memory_record, &settings)
                .await
            {
                Ok(candidate_ids) => {
                    for candidate_id in candidate_ids {
                        self.learning_policy_service
                            .enqueue_candidate(candidate_id)
                            .await;
                    }
                    if let Err(error) = self
                        .set_run_memory_semantic_capture_state(
                            &run_id,
                            crate::memory::RunMemorySemanticCaptureState::Completed,
                        )
                        .await
                    {
                        warn!(run_id = %run_id, error = %error, "failed to persist semantic capture replay receipt");
                    }
                }
                Err(error) => {
                    warn!(run_id = %run_id, error = %error, "failed to replay semantic capture on boot");
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn restore_parent_clarification_completions_on_boot(
        self: &Arc<Self>,
    ) -> Result<()> {
        let resumable = self
            .run_service
            .incomplete_parent_clarification_completions()
            .await;
        for completion in resumable {
            let agent_id = AgentId(completion.agent_id.clone());
            if let Err(error) = self
                .complete_parent_clarification_run(
                    &completion.run_id,
                    &completion.session_id,
                    &agent_id,
                    completion.request,
                    completion.resolution,
                    completion.reason,
                )
                .await
            {
                warn!(
                    run_id = %completion.run_id,
                    error = %error,
                    "failed to replay incomplete parent clarification completion on boot"
                );
            }
        }
        Ok(())
    }

    pub(crate) async fn list_runs(&self, session_id: Option<&str>) -> Result<Vec<RunView>> {
        let mut runs = self.run_service.list_runs(session_id).await?;
        self.attach_delivery_views_to_runs(&mut runs, session_id)
            .await?;
        Ok(runs)
    }

    pub(crate) async fn prune_runs(
        &self,
        request: RunRetentionPruneRequest,
    ) -> Result<RunRetentionPruneResponse> {
        let response = self
            .run_service
            .prune_terminal_run_debug_evidence(
                request.older_than_ms,
                request.session_id.as_deref(),
                request.limit,
                request.dry_run,
                now_ms(),
            )
            .await?;
        Ok(response)
    }
}

fn build_semantic_candidate_evidence_refs(
    record: &RunRecord,
    memory_record: &RunMemoryRecord,
) -> Vec<kheish_types::LearningEvidenceRef> {
    let mut evidence_refs = memory_record
        .memory
        .artifact_ids
        .iter()
        .map(|artifact_id| kheish_types::LearningEvidenceRef {
            run_id: Some(record.view.run_id.clone()),
            artifact_id: Some(artifact_id.clone()),
            note: Some("source run artifact".to_string()),
        })
        .collect::<Vec<_>>();
    if evidence_refs.is_empty() {
        evidence_refs.push(kheish_types::LearningEvidenceRef {
            run_id: Some(record.view.run_id.clone()),
            artifact_id: None,
            note: Some("source run".to_string()),
        });
    }
    evidence_refs
}

fn parent_clarification_requests_equivalent(
    existing: &UserQuestionRequest,
    replayed: &UserQuestionRequest,
) -> bool {
    existing.id == replayed.id
        && existing.tool_call_id == replayed.tool_call_id
        && existing.questions == replayed.questions
}

fn run_is_waiting_for_question_request(record: &RunRecord, request_id: &str) -> bool {
    record.view.status == DaemonRunStatus::WaitingForUserQuestion
        && record
            .view
            .pending_questions
            .iter()
            .any(|question| question.id == request_id)
}

fn expired_user_question_terminal_error(record: &RunRecord, request_id: &str) -> Option<String> {
    if let Some(error) = record.view.error.as_ref() {
        let expected_prefix = format!("user-question request {request_id} expired at ");
        if error.starts_with(&expected_prefix) {
            return Some(error.clone());
        }
    }
    expired_parent_clarification_resolution_error(record, request_id)
}

fn cancelled_user_question_error(request_id: &str, justification: Option<&str>) -> String {
    match justification
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(justification) => {
            format!("user-question request {request_id} cancelled: {justification}")
        }
        None => format!("user-question request {request_id} cancelled"),
    }
}

fn cancelled_user_question_terminal_error(record: &RunRecord, request_id: &str) -> Option<String> {
    if let Some(error) = record.view.error.as_ref() {
        let expected_prefix = format!("user-question request {request_id} cancelled");
        if error.starts_with(&expected_prefix) {
            return Some(error.clone());
        }
    }
    let RunRequestPayload::ParentClarification { completion, .. } = &record.payload else {
        return None;
    };
    let resolution = completion.resolution.as_ref()?;
    if resolution.request_id != request_id {
        return None;
    }
    match completion.reason.as_ref() {
        Some(ParentClarificationCompletionReason::Cancelled) => {
            Some(format!("user-question request {request_id} cancelled"))
        }
        Some(ParentClarificationCompletionReason::Declined) => Some(format!(
            "user-question request {request_id} was already declined"
        )),
        Some(ParentClarificationCompletionReason::Interrupted) => Some(format!(
            "user-question request {request_id} was interrupted"
        )),
        _ => None,
    }
}

fn user_question_cancel_terminal_matches(
    record: &RunRecord,
    request_id: &str,
    request: &CancelUserQuestionRequest,
    terminal_error: &str,
) -> bool {
    if record.view.status == DaemonRunStatus::Cancelled
        && record.view.error.as_deref() == Some(terminal_error)
    {
        return true;
    }
    let RunRequestPayload::ParentClarification { completion, .. } = &record.payload else {
        return false;
    };
    let Some(resolution) = completion.resolution.as_ref() else {
        return false;
    };
    resolution.request_id == request_id
        && resolution.declined
        && resolution.answers.is_empty()
        && resolution.justification.as_deref()
            == Some(
                request
                    .justification
                    .as_deref()
                    .unwrap_or("cancelled by operator"),
            )
        && completion.reason == Some(ParentClarificationCompletionReason::Cancelled)
}

fn expired_parent_clarification_resolution_error(
    record: &RunRecord,
    request_id: &str,
) -> Option<String> {
    let RunRequestPayload::ParentClarification { completion, .. } = &record.payload else {
        return None;
    };
    let resolution = completion.resolution.as_ref()?;
    if resolution.request_id != request_id || !resolution.declined {
        return None;
    }
    if let Some(ParentClarificationCompletionReason::Expired { expires_at_ms }) =
        completion.reason.as_ref()
    {
        return Some(format!(
            "user-question request {} expired at {}",
            resolution.request_id, *expires_at_ms
        ));
    }
    let justification = resolution.justification.as_deref()?;
    if !justification.contains("expired at ") {
        return None;
    }
    Some(format!(
        "user-question request {} {}",
        resolution.request_id, justification
    ))
}

fn parent_clarification_reason_for_existing(
    completion: &ParentClarificationCompletionState,
    resolution: &UserQuestionResolution,
) -> ParentClarificationCompletionReason {
    completion
        .reason
        .clone()
        .unwrap_or_else(|| parent_clarification_reason_for_resolution(resolution))
}

fn parent_clarification_reason_for_direct_resolution(
    resolution: &UserQuestionResolution,
) -> ParentClarificationCompletionReason {
    if resolution.declined {
        ParentClarificationCompletionReason::Declined
    } else {
        ParentClarificationCompletionReason::Answered
    }
}

fn parent_clarification_reason_for_resolution(
    resolution: &UserQuestionResolution,
) -> ParentClarificationCompletionReason {
    if !resolution.declined {
        return ParentClarificationCompletionReason::Answered;
    }
    if let Some(expires_at_ms) = parse_expired_question_justification(&resolution.justification) {
        return ParentClarificationCompletionReason::Expired { expires_at_ms };
    }
    ParentClarificationCompletionReason::Declined
}

fn parse_expired_question_justification(justification: &Option<String>) -> Option<u64> {
    let text = justification.as_deref()?;
    let (_, suffix) = text.rsplit_once("expired at ")?;
    suffix
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<u64>().ok())
}

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(super) async fn wait_for_run_settled(self: &Arc<Self>, run_id: &str) -> Result<RunView> {
        self.run_service.wait_for_run_settled(run_id).await
    }

    pub(crate) async fn get_run(&self, run_id: &str) -> Result<RunView> {
        let mut run = self.run_service.get_run(run_id).await?;
        self.attach_delivery_views_to_run(&mut run).await?;
        Ok(run)
    }

    pub(crate) async fn find_run_by_connector_ingress_key(
        &self,
        ingress_key: &str,
    ) -> Option<RunView> {
        self.run_service
            .find_run_by_connector_ingress_key(ingress_key)
            .await
    }

    pub(crate) fn run_debug_view(&self, run_id: &str) -> Result<RunDebugView> {
        self.run_service.run_debug_view(run_id)
    }

    pub(crate) fn run_debug_artifact(&self, run_id: &str, artifact_id: &str) -> Result<String> {
        self.run_service.run_debug_artifact(run_id, artifact_id)
    }

    pub(crate) fn run_events(&self, run_id: &str) -> Result<Vec<RunEventEntry>> {
        self.run_service.run_events(run_id)
    }

    pub(crate) fn run_external_actions(
        &self,
        run_id: &str,
    ) -> Result<Vec<crate::services::ExternalActionAuditRecord>> {
        self.run_service.run_external_actions(run_id)
    }

    pub(super) fn publish_run(&self, run: &RunView) {
        self.run_service.publish_run(run);
    }
}

fn normalize_session_run_idempotency_key(key: &str) -> Result<String> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err(DaemonProblem::invalid_idempotency_key("idempotency key is required").into());
    }
    if trimmed.len() > 256 {
        return Err(DaemonProblem::invalid_idempotency_key(
            "idempotency key must not exceed 256 bytes",
        )
        .into());
    }
    if trimmed.chars().any(char::is_control) {
        return Err(DaemonProblem::invalid_idempotency_key(
            "idempotency key must not contain control characters",
        )
        .into());
    }
    Ok(trimmed.to_string())
}

fn session_run_idempotency_key_hash(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

fn session_run_idempotency_receipt_key(session_id: &str, key_hash: &str) -> String {
    format!("session-run:{session_id}:{key_hash}")
}

fn run_operation_idempotency_receipt_key(
    operation: &str,
    session_id: &str,
    key_hash: &str,
) -> String {
    format!("run-op:{operation}:{session_id}:{key_hash}")
}

fn submit_input_request_fingerprint(request: &SubmitInputRequest) -> Result<String> {
    let payload = serde_json::json!({
        "version": 1,
        "request": request,
    });
    let encoded = serde_json::to_vec(&payload)?;
    Ok(hex::encode(Sha256::digest(encoded)))
}

fn contains_goal_daemon_metadata(metadata: &Option<Value>) -> bool {
    metadata
        .as_ref()
        .and_then(Value::as_object)
        .is_some_and(|object| object.contains_key("daemon"))
}

fn is_permanent_provider_continuation_error(error: &anyhow::Error) -> bool {
    if error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<kheish_runtime::ProviderError>())
        .any(|provider_error| !provider_error.retryable)
    {
        return true;
    }

    is_permanent_provider_continuation_failure_message(&error.to_string())
}

fn is_permanent_provider_continuation_failure_message(error: &str) -> bool {
    if error.contains("insufficient_quota")
        || error.contains("context_length_exceeded")
        || error.contains("model output token budget exceeded")
    {
        return true;
    }

    let marker = "request failed with status ";
    let mut remaining = error;
    while let Some(index) = remaining.find(marker) {
        let after_marker = &remaining[index + marker.len()..];
        let digits = after_marker
            .chars()
            .take_while(|value| value.is_ascii_digit())
            .collect::<String>();
        if let Ok(status) = digits.parse::<u16>() {
            if (400..500).contains(&status) && !matches!(status, 408 | 409 | 429) {
                return true;
            }
        }
        remaining = after_marker;
    }

    false
}

fn approval_request_without_idempotency(
    mut request: ResolveApprovalsRequest,
) -> ResolveApprovalsRequest {
    request.idempotency_key = None;
    request
}

fn approval_request_fingerprint(run_id: &str, request: &ResolveApprovalsRequest) -> Result<String> {
    let payload = serde_json::json!({
        "version": 1,
        "operation": APPROVAL_OPERATION,
        "run_id": run_id,
        "request": approval_request_without_idempotency(request.clone()),
    });
    let encoded = serde_json::to_vec(&payload)?;
    Ok(hex::encode(Sha256::digest(encoded)))
}

fn approval_resume_payload_fingerprint(record: &RunRecord) -> Result<Option<String>> {
    let RunRequestPayload::ApprovalResume { request, .. } = &record.payload else {
        return Ok(None);
    };
    approval_request_fingerprint(&record.view.run_id, request).map(Some)
}

fn user_question_request_without_idempotency(
    mut request: ResolveUserQuestionRequest,
) -> ResolveUserQuestionRequest {
    request.idempotency_key = None;
    request
}

fn user_question_request_fingerprint(
    run_id: &str,
    request: &ResolveUserQuestionRequest,
) -> Result<String> {
    let payload = serde_json::json!({
        "version": 1,
        "operation": USER_QUESTION_OPERATION,
        "run_id": run_id,
        "request": user_question_request_without_idempotency(request.clone()),
    });
    let encoded = serde_json::to_vec(&payload)?;
    Ok(hex::encode(Sha256::digest(encoded)))
}

fn user_question_resume_payload_fingerprint(record: &RunRecord) -> Result<Option<String>> {
    let RunRequestPayload::UserQuestionResume { request, .. } = &record.payload else {
        return Ok(None);
    };
    user_question_request_fingerprint(&record.view.run_id, request).map(Some)
}

fn user_question_cancel_request_without_idempotency(
    mut request: CancelUserQuestionRequest,
) -> CancelUserQuestionRequest {
    request.idempotency_key = None;
    request
}

fn user_question_cancel_request_fingerprint(
    run_id: &str,
    request_id: &str,
    request: &CancelUserQuestionRequest,
) -> Result<String> {
    let payload = serde_json::json!({
        "version": 1,
        "operation": USER_QUESTION_CANCEL_OPERATION,
        "run_id": run_id,
        "request_id": request_id,
        "request": user_question_cancel_request_without_idempotency(request.clone()),
    });
    let encoded = serde_json::to_vec(&payload)?;
    Ok(hex::encode(Sha256::digest(encoded)))
}

#[cfg(test)]
mod goal_continuation_failure_tests {
    use kheish_runtime::ProviderError;

    use super::{
        is_permanent_provider_continuation_error,
        is_permanent_provider_continuation_failure_message,
    };

    #[test]
    fn non_retryable_provider_error_blocks_goal_continuation() {
        let error = anyhow::anyhow!(ProviderError {
            message: "OpenAI request failed with status 400: type=invalid_request_error"
                .to_string(),
            retryable: false,
            retry_after_ms: None,
        })
        .context("model call failed");

        assert!(is_permanent_provider_continuation_error(&error));
    }

    #[test]
    fn retryable_provider_error_does_not_block_goal_continuation() {
        let error = anyhow::anyhow!(ProviderError {
            message: "OpenAI request failed with status 429: type=rate_limit_exceeded".to_string(),
            retryable: true,
            retry_after_ms: Some(1_000),
        });

        assert!(!is_permanent_provider_continuation_error(&error));
    }

    #[test]
    fn permanent_provider_failure_messages_block_goal_continuation() {
        assert!(is_permanent_provider_continuation_failure_message(
            "OpenAI request failed with status 400: type=invalid_request_error"
        ));
        assert!(is_permanent_provider_continuation_failure_message(
            "OpenAI request failed with status 401: type=invalid_request_error"
        ));
        assert!(is_permanent_provider_continuation_failure_message(
            "model runtime exhausted retries: OpenAI request failed with status 429: type=insufficient_quota code=insufficient_quota"
        ));
        assert!(is_permanent_provider_continuation_failure_message(
            "model output token budget exceeded"
        ));
    }

    #[test]
    fn retryable_provider_failure_messages_do_not_block_goal_continuation() {
        assert!(!is_permanent_provider_continuation_failure_message(
            "OpenAI request failed with status 429: type=rate_limit_exceeded"
        ));
        assert!(!is_permanent_provider_continuation_failure_message(
            "OpenAI request failed with status 503: type=server_error"
        ));
        assert!(!is_permanent_provider_continuation_failure_message(
            "transport error: connection reset"
        ));
    }
}
