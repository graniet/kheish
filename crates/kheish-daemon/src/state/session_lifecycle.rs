//! Session lifecycle, mailbox, and topology methods implemented on [`DaemonState`].

use super::*;
use crate::problems::DaemonProblem;

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(crate) async fn session_is_idle_for_topology_mutation(
        &self,
        session_id: &str,
    ) -> Result<bool> {
        let has_background_work = self
            .run_service
            .session_state(session_id)
            .await
            .map(|state| state.active_run_id.is_some() || !state.queued_run_ids.is_empty())
            .unwrap_or(false);
        if has_background_work {
            return Ok(false);
        }
        if self
            .run_service
            .has_pending_topology_submission(session_id)
            .await
        {
            return Ok(false);
        }
        let agent_id = self.agent_id_for_session(session_id).await?;
        if self.supervisor.mailbox_len(&agent_id) > 0 {
            return Ok(false);
        }
        if self
            .supervisor
            .descendants_of(&agent_id)
            .into_iter()
            .any(|record| self.orchestrator.has_runtime(&record.id))
        {
            return Ok(false);
        }
        let snapshot = self.live_snapshot(&agent_id).await?;
        Ok(snapshot.pending_approvals.is_empty()
            && snapshot.pending_questions.is_empty()
            && !matches!(
                snapshot.agent.status,
                AgentStatus::WaitingForApproval | AgentStatus::WaitingForUserInput
            ))
    }

    pub(super) async fn session_conversation_key(
        &self,
        session_id: &str,
    ) -> Result<ConversationKey> {
        let agent_id = self.agent_id_for_session(session_id).await?;
        let snapshot = self
            .supervisor
            .get(&agent_id)
            .ok_or_else(|| anyhow!("unknown agent {}", agent_id.0))?;
        Ok(snapshot.conversation)
    }

    fn next_mailbox_message_id() -> String {
        format!("mailbox-{}-{:016x}", now_ms(), rand::random::<u64>())
    }

    pub(crate) async fn post_mailbox(
        self: &Arc<Self>,
        mut request: PostMailboxRequest,
    ) -> Result<PostMailboxResponse> {
        let recipient = AgentId(request.to_agent_id);
        let recipient_record = self
            .supervisor
            .get(&recipient)
            .ok_or_else(|| anyhow!("unknown agent {}", recipient.0))?;
        anyhow::ensure!(
            recipient_record.closed_at_ms.is_none() && self.orchestrator.has_runtime(&recipient),
            "agent {} is closed",
            recipient.0
        );
        let sender = AgentId(request.from_agent_id.clone());
        let sender_record = self
            .supervisor
            .get(&sender)
            .ok_or_else(|| anyhow!("unknown agent {}", sender.0))?;
        self.normalize_mailbox_payload(
            &sender_record.conversation.session_id,
            &mut request.payload,
        )
        .await?;
        let message_id = request
            .message_id
            .clone()
            .map(|message_id| message_id.trim().to_string())
            .filter(|message_id| !message_id.is_empty())
            .unwrap_or_else(Self::next_mailbox_message_id);
        if let Some(requested_id) = request.message_id.as_deref() {
            anyhow::ensure!(
                !requested_id.trim().is_empty(),
                "mailbox message_id must not be empty"
            );
        }
        let now = now_ms();
        let expires_at_ms = request
            .ttl_ms
            .map(|ttl_ms| {
                anyhow::ensure!(ttl_ms > 0, "mailbox ttl_ms must be greater than zero");
                Ok(now.saturating_add(ttl_ms))
            })
            .transpose()?;
        let message = MailboxMessage::new(
            message_id.clone(),
            sender,
            recipient.clone(),
            request.subject,
            request.payload,
            now,
            expires_at_ms,
        );
        {
            let _mailbox_guard = self.mailbox_topology_lock.lock().await;
            let duplicate_pending = !self.supervisor.post_if_absent(message.clone());
            let duplicate_durable = !duplicate_pending
                && self
                    .run_service
                    .session_has_mailbox_delivery_message_id(
                        &recipient_record.conversation.session_id,
                        &recipient.0,
                        &message_id,
                    )
                    .await?;
            if duplicate_durable && self.supervisor.ack_mailbox_message(&recipient, &message) {
                self.persist_topology().await?;
            }
            if duplicate_pending || duplicate_durable {
                return Ok(PostMailboxResponse {
                    accepted: true,
                    message_id,
                    duplicate: true,
                });
            }
            debug!(
                to_agent_id = %recipient.0,
                session_id = %recipient_record.conversation.session_id,
                mailbox_depth = self.supervisor.mailbox_len(&recipient),
                message_id = %message_id,
                "posted mailbox message"
            );
            self.persist_topology().await?;
        }
        let _ = self.schedule_mailbox_run(&recipient).await?;
        Ok(PostMailboxResponse {
            accepted: true,
            message_id,
            duplicate: false,
        })
    }

    pub(crate) async fn interrupt_session(
        self: &Arc<Self>,
        session_id: &str,
    ) -> Result<InterruptSessionResponse> {
        let agent_id = self.agent_id_for_session(session_id).await?;
        info!(
            session_id = %session_id,
            agent_id = %agent_id.0,
            "interrupting daemon session"
        );
        let waiting_run_id = {
            let active_run_id = self.run_service.active_run_id(session_id).await;
            match active_run_id {
                Some(run_id)
                    if matches!(
                        self.run_service
                            .get_run(&run_id)
                            .await
                            .ok()
                            .map(|record| record.status),
                        Some(
                            DaemonRunStatus::WaitingForApproval
                                | DaemonRunStatus::WaitingForUserQuestion
                        )
                    ) =>
                {
                    Some(run_id)
                }
                _ => None,
            }
        };
        let result = self.orchestrator.interrupt(&agent_id).await?;
        self.persist_topology().await?;
        if let Some(run_id) = waiting_run_id {
            self.interrupt_run(&run_id).await?;
        }
        if let Some(goal) = self.load_session_goal(session_id).await? {
            if goal.status == kheish_types::SessionGoalStatus::Active {
                let _ = self
                    .goal_service
                    .update_session_goal(
                        session_id,
                        SessionGoalPatch {
                            status: Some(kheish_types::SessionGoalStatus::Paused),
                            ..SessionGoalPatch::default()
                        },
                    )
                    .await;
            }
        }
        self.events.publish(DaemonEvent::Interrupted {
            session_id: session_id.to_string(),
            agent_id: agent_id.0.clone(),
        });
        self.publish_snapshot_for_session(session_id, &result.snapshot);
        Ok(InterruptSessionResponse {
            interrupted: result.interrupted,
            snapshot: result.snapshot,
        })
    }

    pub(crate) async fn end_session(
        self: &Arc<Self>,
        session_id: &str,
        reason: Option<String>,
    ) -> Result<SessionView> {
        self.reject_session_project_dependencies(session_id).await?;
        if !self
            .session_is_idle_for_topology_mutation(session_id)
            .await?
        {
            anyhow::bail!("session has non-terminal work or live descendants");
        }

        let agent_id = self.agent_id_for_session(session_id).await?;
        let view = self.session_view(session_id, &agent_id).await?;
        let hook_runtime = self.load_hook_runtime_state(session_id).await?;
        let _ = self
            .dispatch_daemon_hook(
                HookEventName::SessionEnd,
                Some(reason.clone().unwrap_or_else(|| "ended".to_string())),
                Some(session_id.to_string()),
                Some(agent_id.0.clone()),
                None,
                json!({
                    "reason": reason.clone().unwrap_or_else(|| "ended".to_string()),
                    "session": view,
                    "hook_runtime": hook_runtime,
                }),
            )
            .await;
        let mut close_owned_worktree_agent = false;
        if let Some(worktree_path) = view
            .snapshot
            .agent
            .fork_context
            .as_ref()
            .and_then(|fork| fork.worktree_path.clone())
        {
            let _ = self
                .dispatch_daemon_hook(
                    HookEventName::WorktreeRemove,
                    Some(worktree_path.clone()),
                    Some(session_id.to_string()),
                    Some(agent_id.0.clone()),
                    None,
                    json!({
                        "session_id": session_id,
                        "agent_id": agent_id.0.clone(),
                        "worktree_path": worktree_path,
                    }),
                )
                .await;
            if let Some(ownership) = view.snapshot.agent.daemon_owned_worktree.as_ref() {
                self.remove_daemon_owned_git_worktree(ownership)
                    .await
                    .with_context(|| {
                        format!(
                            "failed to remove daemon-owned worktree {} after session end",
                            worktree_path
                        )
                    })?;
                close_owned_worktree_agent = true;
            }
        }
        if close_owned_worktree_agent {
            let closed_at_ms = now_ms();
            let updated = self.supervisor.update_record(&agent_id, |agent| {
                if agent.settled_at_ms.is_none() {
                    agent.settled_at_ms = Some(closed_at_ms);
                }
                if agent.closed_at_ms.is_none() {
                    agent.closed_at_ms = Some(closed_at_ms);
                }
            })?;
            self.supervisor.record_lifecycle_event(
                "agent_closed",
                &updated,
                reason.as_deref().or(Some("end_session")),
            );
            let _ = self.orchestrator.close_runtime(&agent_id);
            self.persist_topology().await?;
        }
        info!(
            session_id = %session_id,
            agent_id = %agent_id.0,
            reason = %reason.clone().unwrap_or_else(|| "ended".to_string()),
            "ended daemon session"
        );
        self.session_view(session_id, &agent_id).await
    }

    pub(crate) async fn drain_mailbox(&self, agent_id: &str) -> Result<Vec<MailboxMessage>> {
        Ok(self.supervisor.peek_mailbox(&AgentId(agent_id.to_string())))
    }

    pub(crate) async fn mailbox_dead_letters(&self, agent_id: &str) -> Result<Vec<MailboxMessage>> {
        Ok(self
            .supervisor
            .mailbox_dead_letters(&AgentId(agent_id.to_string())))
    }

    pub(crate) async fn ack_mailbox_message(
        &self,
        agent_id: &str,
        message_id: &str,
    ) -> Result<AckMailboxResponse> {
        let agent = AgentId(agent_id.to_string());
        anyhow::ensure!(
            self.supervisor.get(&agent).is_some(),
            "unknown agent {}",
            agent.0
        );
        let message_id = message_id.trim().to_string();
        anyhow::ensure!(
            !message_id.is_empty(),
            "mailbox message_id must not be empty"
        );
        let acknowledged = {
            let _mailbox_guard = self.mailbox_topology_lock.lock().await;
            let acknowledged = self
                .supervisor
                .ack_mailbox_message_ids(&agent, std::slice::from_ref(&message_id))
                > 0;
            if acknowledged {
                self.persist_topology().await?;
            }
            acknowledged
        };
        Ok(AckMailboxResponse {
            acknowledged,
            agent_id: agent.0,
            message_id,
        })
    }

    pub(super) async fn persist_topology(&self) -> Result<()> {
        self.session_service.persist_index().await?;
        self.store.save_supervisor(&self.supervisor.snapshot())?;
        Ok(())
    }

    pub(super) fn publish_snapshot(&self, view: &SessionView) {
        self.publish_snapshot_for_session(&view.session_id, &view.snapshot);
    }

    pub(super) fn publish_snapshot_for_session(
        &self,
        session_id: &str,
        snapshot: &ManagedAgentSnapshot,
    ) {
        self.publish_agent_state(
            session_id,
            &snapshot.agent.id,
            snapshot.agent.status.clone(),
            snapshot.pending_approvals.len(),
            snapshot.pending_questions.len(),
        );
        self.events.publish(DaemonEvent::SessionSnapshot {
            session_id: session_id.to_string(),
            snapshot: snapshot.clone(),
        });
    }

    pub(super) fn publish_agent_state(
        &self,
        session_id: &str,
        agent_id: &AgentId,
        status: kheish_agent::AgentStatus,
        pending_approvals: usize,
        pending_questions: usize,
    ) {
        self.events.publish(DaemonEvent::SessionStateChanged {
            session_id: session_id.to_string(),
            agent_id: agent_id.0.clone(),
            status,
            pending_approvals,
            pending_questions,
        });
    }

    pub(crate) async fn agent_id_for_session(&self, session_id: &str) -> Result<AgentId> {
        self.session_service
            .session_agent_id(session_id)
            .await
            .map(AgentId)
            .ok_or_else(|| {
                anyhow::Error::from(DaemonProblem::session_not_found(format!(
                    "unknown session {session_id}"
                )))
            })
    }
}
