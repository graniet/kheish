use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, bail};
use kheish_session::{FileSessionStore, PersistedSessionRecord};
use kheish_types::{
    Role, SESSION_GOAL_METADATA_KEY, SessionEvent, SessionGoal, SessionGoalStatus,
    SessionGoalUsageAccount,
};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::events::{DaemonEvent, DaemonEventBus};
use crate::runs::{RunView, now_ms};

const MAX_GOAL_OBJECTIVE_CHARS: usize = 8_000;

/// Partial mutation for one existing session goal.
#[derive(Clone, Debug, Default)]
pub(crate) struct SessionGoalPatch {
    pub(crate) objective: Option<String>,
    pub(crate) status: Option<SessionGoalStatus>,
    pub(crate) token_budget: Option<Option<u64>>,
    pub(crate) expected_goal_id: Option<String>,
    pub(crate) expected_version: Option<u64>,
    pub(crate) expected_definition_version: Option<u64>,
    pub(crate) completed_by_run_id: Option<String>,
}

/// Owns durable session goal state stored in session metadata.
pub(crate) struct GoalService {
    sessions: Arc<FileSessionStore>,
    events: DaemonEventBus,
    locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    next_goal_id: AtomicU64,
}

impl GoalService {
    pub(crate) fn new(sessions: Arc<FileSessionStore>, events: DaemonEventBus) -> Self {
        Self {
            sessions,
            events,
            locks: Mutex::new(BTreeMap::new()),
            next_goal_id: AtomicU64::new(0),
        }
    }

    async fn session_lock(&self, session_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn next_goal_id(&self, now_ms: u64) -> String {
        let seq = self.next_goal_id.fetch_add(1, Ordering::Relaxed) + 1;
        format!("goal-{now_ms}-{seq}")
    }

    pub(crate) async fn load_session_goal(&self, session_id: &str) -> Result<Option<SessionGoal>> {
        self.sessions
            .load_metadata_value(session_id, SESSION_GOAL_METADATA_KEY)
            .await?
            .filter(|value| !value.is_null())
            .map(serde_json::from_value)
            .transpose()
            .map_err(Into::into)
    }

    async fn persist_session_goal(
        &self,
        session_id: &str,
        goal: Option<&SessionGoal>,
    ) -> Result<Option<SessionGoal>> {
        let value = goal
            .map(serde_json::to_value)
            .transpose()?
            .unwrap_or(Value::Null);
        self.sessions
            .append(
                session_id,
                PersistedSessionRecord::Metadata {
                    key: SESSION_GOAL_METADATA_KEY.to_string(),
                    value,
                },
            )
            .await?;
        Ok(goal.cloned())
    }

    fn publish_session_goal(&self, session_id: &str, goal: Option<SessionGoal>) {
        self.events.publish(DaemonEvent::SessionGoalUpdated {
            session_id: session_id.to_string(),
            goal,
        });
    }

    pub(crate) async fn create_session_goal(
        &self,
        session_id: &str,
        objective: String,
        token_budget: Option<u64>,
        status: SessionGoalStatus,
        created_by_run_id: Option<String>,
    ) -> Result<SessionGoal> {
        let saved = {
            let lock = self.session_lock(session_id).await;
            let _guard = lock.lock().await;
            if self.load_session_goal(session_id).await?.is_some() {
                bail!("session already has a goal");
            }
            self.replace_session_goal_locked(
                session_id,
                objective,
                token_budget,
                status,
                created_by_run_id,
            )
            .await?
        };
        self.publish_session_goal(session_id, Some(saved.clone()));
        Ok(saved)
    }

    pub(crate) async fn set_session_goal(
        &self,
        session_id: &str,
        objective: String,
        token_budget: Option<u64>,
        status: SessionGoalStatus,
    ) -> Result<SessionGoal> {
        let saved = {
            let lock = self.session_lock(session_id).await;
            let _guard = lock.lock().await;
            self.replace_session_goal_locked(session_id, objective, token_budget, status, None)
                .await?
        };
        self.publish_session_goal(session_id, Some(saved.clone()));
        Ok(saved)
    }

    async fn replace_session_goal_locked(
        &self,
        session_id: &str,
        objective: String,
        token_budget: Option<u64>,
        status: SessionGoalStatus,
        created_by_run_id: Option<String>,
    ) -> Result<SessionGoal> {
        let objective = validate_objective(objective)?;
        validate_token_budget(token_budget)?;
        let now = now_ms();
        let goal = SessionGoal {
            goal_id: self.next_goal_id(now),
            session_id: session_id.to_string(),
            objective,
            status,
            token_budget,
            tokens_used: 0,
            time_used_ms: 0,
            created_at_ms: now,
            updated_at_ms: now,
            version: 1,
            definition_version: 1,
            created_by_run_id,
            accounted_usage: BTreeMap::new(),
            last_continuation_run_id: None,
            budget_limited_by_run_id: None,
            budget_wrapup_run_id: None,
            completed_by_run_id: None,
        };
        Ok(self
            .persist_session_goal(session_id, Some(&goal))
            .await?
            .expect("saved goal must be present"))
    }

    pub(crate) async fn update_session_goal(
        &self,
        session_id: &str,
        patch: SessionGoalPatch,
    ) -> Result<SessionGoal> {
        let saved = {
            let lock = self.session_lock(session_id).await;
            let _guard = lock.lock().await;
            let mut goal = self
                .load_session_goal(session_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("session has no goal"))?;
            if let Some(expected) = &patch.expected_goal_id {
                if expected != &goal.goal_id {
                    bail!("session goal changed");
                }
            }
            if let Some(expected) = patch.expected_version {
                if expected != goal.version {
                    bail!("session goal version changed");
                }
            }
            let current_definition_version = goal.binding_version();
            if let Some(expected) = patch.expected_definition_version {
                if expected != current_definition_version {
                    bail!("session goal version changed");
                }
            }
            let changes_definition =
                patch.objective.is_some() || patch.status.is_some() || patch.token_budget.is_some();
            if let Some(objective) = patch.objective {
                goal.objective = validate_objective(objective)?;
            }
            if let Some(token_budget) = patch.token_budget {
                validate_token_budget(token_budget)?;
                goal.token_budget = token_budget;
            }
            if let Some(status) = patch.status {
                goal.status = status;
            }
            if patch.completed_by_run_id.is_some() {
                goal.completed_by_run_id = patch.completed_by_run_id;
            }
            if changes_definition {
                goal.definition_version = current_definition_version.saturating_add(1);
            } else if goal.definition_version == 0 {
                goal.definition_version = current_definition_version;
            }
            goal.version = goal.version.saturating_add(1);
            goal.updated_at_ms = now_ms();
            self.persist_session_goal(session_id, Some(&goal))
                .await?
                .expect("saved goal must be present")
        };
        self.publish_session_goal(session_id, Some(saved.clone()));
        Ok(saved)
    }

    pub(crate) async fn clear_session_goal(&self, session_id: &str) -> Result<()> {
        {
            let lock = self.session_lock(session_id).await;
            let _guard = lock.lock().await;
            self.persist_session_goal(session_id, None).await?;
        }
        self.publish_session_goal(session_id, None);
        Ok(())
    }

    pub(crate) async fn mark_continuation_scheduled(
        &self,
        session_id: &str,
        run_id: &str,
        budget_wrapup: bool,
        expected_goal_id: &str,
        expected_version: u64,
    ) -> Result<Option<SessionGoal>> {
        let saved = {
            let lock = self.session_lock(session_id).await;
            let _guard = lock.lock().await;
            let Some(mut goal) = self.load_session_goal(session_id).await? else {
                return Ok(None);
            };
            if goal.goal_id != expected_goal_id || goal.version != expected_version {
                return Ok(None);
            }
            let current_budget_wrapup = goal.status == SessionGoalStatus::BudgetLimited
                && goal.budget_wrapup_run_id.is_none();
            if budget_wrapup != current_budget_wrapup {
                return Ok(None);
            }
            let still_schedulable = goal.should_continue() || current_budget_wrapup;
            if !still_schedulable {
                return Ok(None);
            }
            if goal.last_continuation_run_id.as_deref() == Some(run_id) {
                return Ok(Some(goal));
            }
            goal.last_continuation_run_id = Some(run_id.to_string());
            if budget_wrapup {
                goal.budget_wrapup_run_id = Some(run_id.to_string());
            }
            goal.version = goal.version.saturating_add(1);
            goal.updated_at_ms = now_ms();
            self.persist_session_goal(session_id, Some(&goal)).await?
        };
        self.publish_session_goal(session_id, saved.clone());
        Ok(saved)
    }

    pub(crate) async fn pause_active_continuation_after_failure(
        &self,
        session_id: &str,
        expected_goal_id: &str,
        expected_definition_version: u64,
        expected_run_id: &str,
    ) -> Result<Option<SessionGoal>> {
        let saved = {
            let lock = self.session_lock(session_id).await;
            let _guard = lock.lock().await;
            let Some(mut goal) = self.load_session_goal(session_id).await? else {
                return Ok(None);
            };
            if goal.goal_id != expected_goal_id
                || goal.binding_version() != expected_definition_version
                || goal.status != SessionGoalStatus::Active
                || goal.last_continuation_run_id.as_deref() != Some(expected_run_id)
            {
                return Ok(None);
            }
            let current_definition_version = goal.binding_version();
            goal.status = SessionGoalStatus::Paused;
            goal.definition_version = current_definition_version.saturating_add(1);
            goal.version = goal.version.saturating_add(1);
            goal.updated_at_ms = now_ms();
            self.persist_session_goal(session_id, Some(&goal)).await?
        };
        self.publish_session_goal(session_id, saved.clone());
        Ok(saved)
    }

    pub(crate) async fn account_run_view(
        &self,
        session_id: &str,
        run: &RunView,
    ) -> Result<Option<SessionGoal>> {
        let saved = {
            let lock = self.session_lock(session_id).await;
            let _guard = lock.lock().await;
            let Some(mut goal) = self.load_session_goal(session_id).await? else {
                return Ok(None);
            };
            let started_at_ms = run.started_at_ms.unwrap_or(run.submitted_at_ms);
            let finished_at_ms = run.finished_at_ms.unwrap_or(run.updated_at_ms);
            if goal.status == SessionGoalStatus::Complete
                && goal.completed_by_run_id.as_deref() != Some(run.run_id.as_str())
            {
                return Ok(Some(goal));
            }

            let mut changed = false;
            let stored = self.sessions.load(session_id).await?;
            let mut max_message_usage_tokens = 0u64;
            for entry in stored.journal {
                let SessionEvent::MessageAppended { message } = entry.event else {
                    continue;
                };
                if message.role != Role::Assistant {
                    continue;
                }
                let timestamp_ms = message.timestamp_ms.unwrap_or(entry.timestamp_ms);
                if timestamp_ms < goal.created_at_ms
                    || timestamp_ms < started_at_ms
                    || timestamp_ms > finished_at_ms
                {
                    continue;
                }
                let Some(usage) = message.api_usage else {
                    continue;
                };
                let tokens = usage.input_tokens.saturating_add(usage.output_tokens);
                max_message_usage_tokens = max_message_usage_tokens.max(tokens);
            }
            let tokens_key = format!("run:{}:tokens", run.run_id);
            if max_message_usage_tokens > 0 && !goal.accounted_usage.contains_key(&tokens_key) {
                goal.tokens_used = goal.tokens_used.saturating_add(max_message_usage_tokens);
                goal.accounted_usage.insert(
                    tokens_key,
                    SessionGoalUsageAccount {
                        tokens: max_message_usage_tokens,
                        time_ms: 0,
                    },
                );
                changed = true;
            }

            if let (Some(started), Some(finished)) = (run.started_at_ms, run.finished_at_ms) {
                let key = format!("run:{}:time", run.run_id);
                if !goal.accounted_usage.contains_key(&key) {
                    let time_ms = finished.saturating_sub(started);
                    goal.time_used_ms = goal.time_used_ms.saturating_add(time_ms);
                    goal.accounted_usage
                        .insert(key, SessionGoalUsageAccount { tokens: 0, time_ms });
                    changed = true;
                }
            }

            if goal.status == SessionGoalStatus::Active
                && goal
                    .token_budget
                    .map(|budget| goal.tokens_used >= budget)
                    .unwrap_or(false)
            {
                goal.status = SessionGoalStatus::BudgetLimited;
                goal.budget_limited_by_run_id = Some(run.run_id.clone());
                changed = true;
            }

            if !changed {
                return Ok(Some(goal));
            }
            if goal.definition_version == 0 {
                goal.definition_version = goal.binding_version();
            }
            goal.version = goal.version.saturating_add(1);
            goal.updated_at_ms = now_ms();
            self.persist_session_goal(session_id, Some(&goal)).await?
        };
        self.publish_session_goal(session_id, saved.clone());
        Ok(saved)
    }
}

fn validate_objective(objective: String) -> Result<String> {
    let trimmed = objective.trim();
    if trimmed.is_empty() {
        bail!("goal objective must not be empty");
    }
    if trimmed.chars().count() > MAX_GOAL_OBJECTIVE_CHARS {
        bail!("goal objective must be at most {MAX_GOAL_OBJECTIVE_CHARS} characters");
    }
    Ok(trimmed.to_string())
}

fn validate_token_budget(token_budget: Option<u64>) -> Result<()> {
    if matches!(token_budget, Some(0)) {
        bail!("goal token budget must be greater than zero");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kheish_session::{FileSessionStore, PersistedSessionRecord};
    use kheish_types::{MessageRecord, ModelUsage};
    use tempfile::tempdir;

    use super::*;
    use crate::runs::{DaemonRunKind, DaemonRunStatus, RunRequestSummary};

    fn service(root: &std::path::Path) -> GoalService {
        GoalService::new(
            Arc::new(FileSessionStore::new(root.join("sessions"))),
            DaemonEventBus::new(16),
        )
    }

    fn completed_run(
        session_id: &str,
        run_id: &str,
        started_at_ms: u64,
        finished_at_ms: u64,
    ) -> RunView {
        RunView {
            run_id: run_id.to_string(),
            session_id: session_id.to_string(),
            agent_id: "agent-1".to_string(),
            kind: DaemonRunKind::Input,
            status: DaemonRunStatus::Completed,
            submitted_at_ms: started_at_ms,
            updated_at_ms: finished_at_ms,
            started_at_ms: Some(started_at_ms),
            finished_at_ms: Some(finished_at_ms),
            queued_position: None,
            request: RunRequestSummary {
                source_plugin: "test".to_string(),
                source_kind: "test".to_string(),
                actor_id: "user".to_string(),
                text_preview: Some("test".to_string()),
                provider: None,
                model: None,
                approval_count: None,
                question_count: None,
            },
            input_attachments: Vec::new(),
            input_metadata: None,
            pending_approval_ids: Vec::new(),
            pending_approvals: Vec::new(),
            pending_question_ids: Vec::new(),
            pending_questions: Vec::new(),
            outputs: Vec::new(),
            deliveries: Vec::new(),
            error: None,
        }
    }

    async fn append_assistant_usage(
        service: &GoalService,
        session_id: &str,
        message_id: &str,
        timestamp_ms: u64,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<()> {
        service
            .sessions
            .append(
                session_id,
                PersistedSessionRecord::Event {
                    entry: kheish_types::LogEntry {
                        offset: timestamp_ms,
                        timestamp_ms,
                        event: SessionEvent::MessageAppended {
                            message: MessageRecord {
                                id: message_id.to_string(),
                                role: Role::Assistant,
                                content: "done".to_string(),
                                pinned: false,
                                provider_response_id: None,
                                api_usage: Some(ModelUsage {
                                    input_tokens,
                                    output_tokens,
                                    cost_usd: 0.0,
                                }),
                                offset: Some(timestamp_ms),
                                timestamp_ms: Some(timestamp_ms),
                                provider_context: None,
                            },
                        },
                    },
                },
            )
            .await
    }

    #[tokio::test]
    async fn goal_service_persists_and_clears_session_goal() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        let goal = service
            .create_session_goal(
                "session-1",
                "Ship the report".to_string(),
                Some(100),
                SessionGoalStatus::Active,
                None,
            )
            .await?;

        assert_eq!(goal.version, 1);
        assert_eq!(goal.definition_version, 1);
        assert_eq!(
            service
                .load_session_goal("session-1")
                .await?
                .as_ref()
                .map(|goal| goal.objective.as_str()),
            Some("Ship the report")
        );

        service.clear_session_goal("session-1").await?;
        assert!(service.load_session_goal("session-1").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn goal_service_accounts_usage_once_and_budget_limits() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        let goal = service
            .create_session_goal(
                "session-1",
                "Use tokens".to_string(),
                Some(10),
                SessionGoalStatus::Active,
                None,
            )
            .await?;

        append_assistant_usage(&service, "session-1", "msg-1", goal.created_at_ms + 1, 4, 3)
            .await?;
        let run = completed_run(
            "session-1",
            "run-1",
            goal.created_at_ms,
            goal.created_at_ms + 10,
        );
        let accounted = service
            .account_run_view("session-1", &run)
            .await?
            .expect("goal should remain");
        assert_eq!(accounted.tokens_used, 7);
        assert_eq!(accounted.status, SessionGoalStatus::Active);
        assert_eq!(accounted.definition_version, goal.definition_version);

        let accounted_again = service
            .account_run_view("session-1", &run)
            .await?
            .expect("goal should remain");
        assert_eq!(accounted_again.tokens_used, 7);

        append_assistant_usage(
            &service,
            "session-1",
            "msg-2",
            goal.created_at_ms + 20,
            1,
            3,
        )
        .await?;
        let run = completed_run(
            "session-1",
            "run-2",
            goal.created_at_ms + 15,
            goal.created_at_ms + 25,
        );
        let budget_limited = service
            .account_run_view("session-1", &run)
            .await?
            .expect("goal should remain");
        assert_eq!(budget_limited.tokens_used, 11);
        assert_eq!(budget_limited.status, SessionGoalStatus::BudgetLimited);
        assert_eq!(budget_limited.remaining_tokens(), Some(0));
        assert_eq!(budget_limited.definition_version, goal.definition_version);
        Ok(())
    }

    #[tokio::test]
    async fn goal_service_allows_bound_completion_after_accounting_version_changes() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        let goal = service
            .create_session_goal(
                "session-1",
                "Finish after queued work".to_string(),
                None,
                SessionGoalStatus::Active,
                None,
            )
            .await?;

        let run = completed_run(
            "session-1",
            "run-accounted",
            goal.created_at_ms,
            goal.created_at_ms + 10,
        );
        let accounted = service
            .account_run_view("session-1", &run)
            .await?
            .expect("goal should remain");
        assert!(accounted.version > goal.version);
        assert_eq!(accounted.definition_version, goal.definition_version);

        let completed = service
            .update_session_goal(
                "session-1",
                SessionGoalPatch {
                    status: Some(SessionGoalStatus::Complete),
                    expected_goal_id: Some(goal.goal_id.clone()),
                    expected_definition_version: Some(goal.definition_version),
                    completed_by_run_id: Some("run-queued".to_string()),
                    ..SessionGoalPatch::default()
                },
            )
            .await?;

        assert_eq!(completed.status, SessionGoalStatus::Complete);
        assert_eq!(completed.completed_by_run_id.as_deref(), Some("run-queued"));
        assert_eq!(completed.definition_version, goal.definition_version + 1);
        Ok(())
    }

    #[tokio::test]
    async fn goal_service_backfills_legacy_definition_version_before_accounting() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        let mut goal = service
            .create_session_goal(
                "session-1",
                "Legacy queued work".to_string(),
                None,
                SessionGoalStatus::Active,
                None,
            )
            .await?;
        goal.definition_version = 0;
        service
            .persist_session_goal("session-1", Some(&goal))
            .await?;

        let legacy_binding_version = goal.version;
        let run = completed_run(
            "session-1",
            "run-accounted",
            goal.created_at_ms,
            goal.created_at_ms + 10,
        );
        let accounted = service
            .account_run_view("session-1", &run)
            .await?
            .expect("goal should remain");

        assert_eq!(accounted.definition_version, legacy_binding_version);
        assert!(accounted.version > legacy_binding_version);

        let completed = service
            .update_session_goal(
                "session-1",
                SessionGoalPatch {
                    status: Some(SessionGoalStatus::Complete),
                    expected_goal_id: Some(goal.goal_id.clone()),
                    expected_definition_version: Some(legacy_binding_version),
                    completed_by_run_id: Some("run-queued".to_string()),
                    ..SessionGoalPatch::default()
                },
            )
            .await?;

        assert_eq!(completed.status, SessionGoalStatus::Complete);
        Ok(())
    }

    #[tokio::test]
    async fn goal_service_rejects_bound_completion_after_definition_changes() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        let goal = service
            .create_session_goal(
                "session-1",
                "Original objective".to_string(),
                None,
                SessionGoalStatus::Active,
                None,
            )
            .await?;

        let edited = service
            .update_session_goal(
                "session-1",
                SessionGoalPatch {
                    objective: Some("Changed objective".to_string()),
                    expected_goal_id: Some(goal.goal_id.clone()),
                    expected_version: Some(goal.version),
                    ..SessionGoalPatch::default()
                },
            )
            .await?;
        assert_eq!(edited.definition_version, goal.definition_version + 1);

        let stale_completion = service
            .update_session_goal(
                "session-1",
                SessionGoalPatch {
                    status: Some(SessionGoalStatus::Complete),
                    expected_goal_id: Some(goal.goal_id.clone()),
                    expected_definition_version: Some(goal.definition_version),
                    completed_by_run_id: Some("run-stale".to_string()),
                    ..SessionGoalPatch::default()
                },
            )
            .await;

        assert!(stale_completion.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn goal_service_reserves_matching_continuation() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        let goal = service
            .create_session_goal(
                "session-1",
                "Keep going".to_string(),
                None,
                SessionGoalStatus::Active,
                None,
            )
            .await?;

        let scheduled = service
            .mark_continuation_scheduled(
                "session-1",
                "run-continue",
                false,
                &goal.goal_id,
                goal.version,
            )
            .await?
            .expect("matching active goal should be reservable");

        assert_eq!(
            scheduled.last_continuation_run_id.as_deref(),
            Some("run-continue")
        );
        assert_eq!(scheduled.version, goal.version + 1);
        Ok(())
    }

    #[tokio::test]
    async fn goal_service_rejects_stale_continuation_after_goal_changes() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        let goal = service
            .create_session_goal(
                "session-1",
                "Keep going".to_string(),
                None,
                SessionGoalStatus::Active,
                None,
            )
            .await?;

        service
            .update_session_goal(
                "session-1",
                SessionGoalPatch {
                    status: Some(SessionGoalStatus::Complete),
                    expected_goal_id: Some(goal.goal_id.clone()),
                    expected_version: Some(goal.version),
                    ..SessionGoalPatch::default()
                },
            )
            .await?;

        let scheduled = service
            .mark_continuation_scheduled(
                "session-1",
                "run-stale",
                false,
                &goal.goal_id,
                goal.version,
            )
            .await?;
        let stored = service
            .load_session_goal("session-1")
            .await?
            .expect("goal should remain");

        assert!(scheduled.is_none());
        assert_eq!(stored.status, SessionGoalStatus::Complete);
        assert_eq!(stored.last_continuation_run_id, None);
        Ok(())
    }

    #[tokio::test]
    async fn goal_service_rejects_budget_wrapup_mismatch() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        let goal = service
            .create_session_goal(
                "session-1",
                "Wrap up only".to_string(),
                Some(1),
                SessionGoalStatus::BudgetLimited,
                None,
            )
            .await?;

        let scheduled = service
            .mark_continuation_scheduled(
                "session-1",
                "run-wrong-kind",
                false,
                &goal.goal_id,
                goal.version,
            )
            .await?;
        let stored = service
            .load_session_goal("session-1")
            .await?
            .expect("goal should remain");

        assert!(scheduled.is_none());
        assert_eq!(stored.budget_wrapup_run_id, None);
        assert_eq!(stored.last_continuation_run_id, None);
        Ok(())
    }

    #[tokio::test]
    async fn goal_service_pauses_matching_active_continuation_under_lock() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        let goal = service
            .create_session_goal(
                "session-1",
                "Keep going".to_string(),
                None,
                SessionGoalStatus::Active,
                None,
            )
            .await?;
        let scheduled = service
            .mark_continuation_scheduled(
                "session-1",
                "run-continue",
                false,
                &goal.goal_id,
                goal.version,
            )
            .await?
            .expect("continuation should schedule");

        let paused = service
            .pause_active_continuation_after_failure(
                "session-1",
                &scheduled.goal_id,
                scheduled.binding_version(),
                "run-continue",
            )
            .await?
            .expect("matching active continuation should pause");

        assert_eq!(paused.status, SessionGoalStatus::Paused);
        assert_eq!(
            paused.last_continuation_run_id.as_deref(),
            Some("run-continue")
        );
        assert_eq!(
            paused.definition_version,
            scheduled.binding_version().saturating_add(1)
        );
        Ok(())
    }

    #[tokio::test]
    async fn goal_service_rejects_stale_continuation_pause() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        let goal = service
            .create_session_goal(
                "session-1",
                "Keep going".to_string(),
                None,
                SessionGoalStatus::Active,
                None,
            )
            .await?;
        service
            .mark_continuation_scheduled(
                "session-1",
                "run-current",
                false,
                &goal.goal_id,
                goal.version,
            )
            .await?
            .expect("continuation should schedule");

        let paused = service
            .pause_active_continuation_after_failure(
                "session-1",
                &goal.goal_id,
                goal.binding_version(),
                "run-stale",
            )
            .await?;
        let stored = service
            .load_session_goal("session-1")
            .await?
            .expect("goal should remain");

        assert!(paused.is_none());
        assert_eq!(stored.status, SessionGoalStatus::Active);
        assert_eq!(
            stored.last_continuation_run_id.as_deref(),
            Some("run-current")
        );
        Ok(())
    }

    #[tokio::test]
    async fn goal_service_rejects_stale_definition_continuation_pause() -> Result<()> {
        let temp = tempdir()?;
        let service = service(temp.path());
        let goal = service
            .create_session_goal(
                "session-1",
                "Keep going".to_string(),
                None,
                SessionGoalStatus::Active,
                None,
            )
            .await?;
        let scheduled = service
            .mark_continuation_scheduled(
                "session-1",
                "run-current",
                false,
                &goal.goal_id,
                goal.version,
            )
            .await?
            .expect("continuation should schedule");
        service
            .update_session_goal(
                "session-1",
                SessionGoalPatch {
                    objective: Some("Changed objective".to_string()),
                    expected_goal_id: Some(goal.goal_id.clone()),
                    expected_definition_version: Some(scheduled.binding_version()),
                    ..SessionGoalPatch::default()
                },
            )
            .await?;

        let paused = service
            .pause_active_continuation_after_failure(
                "session-1",
                &goal.goal_id,
                scheduled.binding_version(),
                "run-current",
            )
            .await?;
        let stored = service
            .load_session_goal("session-1")
            .await?
            .expect("goal should remain");

        assert!(paused.is_none());
        assert_eq!(stored.status, SessionGoalStatus::Active);
        assert_eq!(stored.objective, "Changed objective");
        Ok(())
    }
}
