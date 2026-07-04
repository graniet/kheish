//! Session state persistence methods implemented on [`DaemonState`].

use kheish_types::{CapabilityScope, CredentialScope, SessionExecutionIdentity};

use super::*;

fn merge_generic_session_control_state(
    current: &SessionControlState,
    incoming: SessionControlState,
    archived_task_ids: &BTreeSet<String>,
) -> SessionControlState {
    SessionControlState {
        plan_mode: current.plan_mode,
        session_permission_mode: current.session_permission_mode.clone(),
        session_permission_updates: current.session_permission_updates.clone(),
        pre_plan_mode: current.pre_plan_mode.clone(),
        plan_artifact: current.plan_artifact.clone(),
        todos: incoming.todos,
        tasks: merge_session_tasks(&current.tasks, incoming.tasks, archived_task_ids),
        // The archive tally is daemon-owned; the archival step refreshes it
        // after this merge.
        archived_tasks: current.archived_tasks,
    }
}

fn merge_session_tasks(
    current: &[kheish_types::TaskRecord],
    incoming: Vec<kheish_types::TaskRecord>,
    archived_task_ids: &BTreeSet<String>,
) -> Vec<kheish_types::TaskRecord> {
    let current_by_id = current
        .iter()
        .cloned()
        .map(|task| (task.id.clone(), task))
        .collect::<BTreeMap<_, _>>();
    let mut merged = Vec::new();
    let mut seen = BTreeSet::new();
    for incoming in incoming {
        let task_id = incoming.id.clone();
        // An archived id must never re-enter the hot state: a run that still
        // holds a pre-archival snapshot would otherwise resurrect the task
        // through this disk-union merge (deleted tasks included).
        if archived_task_ids.contains(&task_id) {
            continue;
        }
        let task = match current_by_id.get(&task_id) {
            Some(current) => merge_session_task(current, &incoming),
            None => incoming,
        };
        seen.insert(task_id);
        merged.push(task);
    }
    for current in current {
        if archived_task_ids.contains(&current.id) {
            continue;
        }
        if seen.insert(current.id.clone()) {
            merged.push(current.clone());
        }
    }
    merged
}

fn merge_session_task(
    current: &kheish_types::TaskRecord,
    incoming: &kheish_types::TaskRecord,
) -> kheish_types::TaskRecord {
    if task_is_terminal(&current.status) && !task_is_terminal(&incoming.status) {
        return current.clone();
    }
    if task_is_terminal(&current.status)
        && task_is_terminal(&incoming.status)
        && current.updated_at_ms > incoming.updated_at_ms
    {
        return current.clone();
    }
    incoming.clone()
}

fn task_is_terminal(status: &kheish_types::TaskStatus) -> bool {
    matches!(
        status,
        kheish_types::TaskStatus::Completed
            | kheish_types::TaskStatus::Failed
            | kheish_types::TaskStatus::Cancelled
    )
}

/// A terminal task is archived once nothing can still mutate it: a shell task
/// whose process shutdown may be retried on a future boot stays hot until the
/// retry settles.
fn task_should_archive(task: &kheish_types::TaskRecord) -> bool {
    task_is_terminal(&task.status)
        && !crate::services::background_shell_task_shutdown_unsettled(task)
}

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    fn persisted_permission_mode_name(mode: &PermissionMode) -> &'static str {
        match mode {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::BypassPermissions => "bypassPermissions",
            PermissionMode::Plan => "plan",
            PermissionMode::DontAsk => "dontAsk",
        }
    }

    pub(super) fn resolve_generation_route(
        &self,
        provider: Option<&str>,
        generation: Option<ModelGenerationConfig>,
    ) -> Result<(Option<String>, Option<ModelGenerationConfig>)> {
        let Some(control) = self.model_control.as_ref() else {
            if provider.is_some() {
                anyhow::bail!("provider override requires daemon model routing");
            }
            return Ok((None, generation));
        };

        let explicit_model = generation.as_ref().and_then(|value| value.model.as_deref());
        let resolved = control.resolve_route(provider, explicit_model)?;
        let mut generation = generation.unwrap_or_default();
        generation.model = Some(resolved.model.clone());
        if let Some(fallback_model) = generation.fallback_model.clone() {
            let fallback =
                control.resolve_route(Some(&resolved.route_id), Some(&fallback_model))?;
            generation.fallback_model = Some(fallback.model);
        }
        Ok((Some(resolved.route_id), Some(generation)))
    }

    pub(crate) async fn load_session_control_state(
        &self,
        session_id: &str,
    ) -> Result<SessionControlState> {
        self.session_service
            .load_session_control_state(session_id)
            .await
    }

    pub(crate) async fn repair_session_task_summary_index(&self) -> Result<()> {
        self.session_service
            .repair_session_task_summary_index()
            .await
    }

    pub(crate) async fn load_session_route_policy(
        &self,
        session_id: &str,
    ) -> Result<SessionRoutePolicy> {
        self.session_service
            .load_session_route_policy(session_id)
            .await
    }

    pub(crate) async fn load_session_operator_config(
        &self,
        session_id: &str,
    ) -> Result<SessionOperatorConfig> {
        self.session_service
            .load_session_operator_config(session_id)
            .await
    }

    pub(crate) async fn load_session_tool_overrides(
        &self,
        session_id: &str,
    ) -> Result<kheish_types::SessionToolOverrides> {
        self.session_service
            .load_session_tool_overrides(session_id)
            .await
    }

    pub(crate) async fn save_session_tool_overrides(
        &self,
        session_id: &str,
        overrides: &kheish_types::SessionToolOverrides,
    ) -> Result<kheish_types::SessionToolOverrides> {
        self.session_service
            .save_session_tool_overrides(session_id, overrides)
            .await
    }

    pub(crate) async fn load_session_output_contract(
        &self,
        session_id: &str,
    ) -> Result<Option<kheish_types::StructuredOutputContract>> {
        self.session_service
            .load_session_output_contract(session_id)
            .await
    }

    pub(crate) async fn save_session_output_contract(
        &self,
        session_id: &str,
        contract: Option<&kheish_types::StructuredOutputContract>,
    ) -> Result<()> {
        self.session_service
            .save_session_output_contract(session_id, contract)
            .await
    }

    pub(crate) async fn load_session_capability_scope(
        &self,
        session_id: &str,
    ) -> Result<CapabilityScope> {
        self.session_service
            .load_session_capability_scope(session_id)
            .await
    }

    pub(crate) async fn load_session_credential_scope(
        &self,
        session_id: &str,
    ) -> Result<CredentialScope> {
        self.session_service
            .load_session_credential_scope(session_id)
            .await
    }

    pub(super) async fn load_hook_runtime_state(
        &self,
        session_id: &str,
    ) -> Result<HookRuntimeState> {
        self.session_service
            .load_hook_runtime_state(session_id)
            .await
    }

    pub(super) async fn save_session_route_policy(
        &self,
        session_id: &str,
        state: SessionRoutePolicy,
    ) -> Result<SessionRoutePolicy> {
        self.session_service
            .save_session_route_policy(session_id, &state)
            .await
    }

    pub(super) async fn save_session_operator_config(
        &self,
        session_id: &str,
        config: SessionOperatorConfig,
    ) -> Result<SessionOperatorConfig> {
        self.session_service
            .save_session_operator_config(session_id, &config)
            .await
    }

    pub(super) async fn save_session_capability_scope(
        &self,
        session_id: &str,
        scope: CapabilityScope,
    ) -> Result<CapabilityScope> {
        self.session_service
            .save_session_capability_scope(session_id, &scope)
            .await
    }

    pub(super) async fn save_session_credential_scope(
        &self,
        session_id: &str,
        scope: CredentialScope,
    ) -> Result<CredentialScope> {
        self.session_service
            .save_session_credential_scope(session_id, &scope)
            .await
    }

    pub(super) async fn save_session_execution_identity(
        &self,
        session_id: &str,
        identity: SessionExecutionIdentity,
    ) -> Result<SessionExecutionIdentity> {
        self.session_service
            .save_session_execution_identity(session_id, &identity)
            .await
    }

    pub(super) async fn save_session_control_state(
        &self,
        session_id: &str,
        state: SessionControlState,
    ) -> Result<SessionControlState> {
        let (previous, state) = {
            let _guard = self.session_service.session_control_lock().lock().await;
            let previous = self
                .session_service
                .load_session_control_state(session_id)
                .await?;
            let archived = self.session_service.archived_task_index(session_id).await?;
            let state = merge_generic_session_control_state(&previous, state, &archived.ids);
            let state = self
                .archive_and_persist_control_state_locked(session_id, state, &previous)
                .await?;
            (previous, state)
        };
        let agent_id = self.session_service.session_agent_id(session_id).await;
        let previous_tasks = previous
            .tasks
            .iter()
            .map(|task| (task.id.clone(), task.clone()))
            .collect::<BTreeMap<_, _>>();
        let current_tasks = state
            .tasks
            .iter()
            .map(|task| (task.id.clone(), task.clone()))
            .collect::<BTreeMap<_, _>>();
        for task in state
            .tasks
            .iter()
            .filter(|task| !previous_tasks.contains_key(&task.id))
        {
            let _ = self
                .dispatch_daemon_hook(
                    HookEventName::TaskCreated,
                    Some(task.title.clone()),
                    Some(session_id.to_string()),
                    agent_id.clone(),
                    None,
                    json!({ "task": task }),
                )
                .await;
        }
        for task in state.tasks.iter().filter(|task| {
            let Some(previous) = current_tasks
                .get(&task.id)
                .and_then(|_| previous_tasks.get(&task.id))
            else {
                return false;
            };
            previous.status != task.status
                && matches!(task.status, kheish_types::TaskStatus::Completed)
        }) {
            let _ = self
                .dispatch_daemon_hook(
                    HookEventName::TaskCompleted,
                    Some(task.title.clone()),
                    Some(session_id.to_string()),
                    agent_id.clone(),
                    None,
                    json!({
                        "previous": previous_tasks.get(&task.id),
                        "current": task,
                    }),
                )
                .await;
        }
        Ok(state)
    }

    /// Archives every settled terminal task and persists the bounded hot
    /// state; callers must hold the session control lock. Returns the
    /// pre-strip state so callers still observe the transition they made.
    async fn archive_and_persist_control_state_locked(
        &self,
        session_id: &str,
        mut state: SessionControlState,
        previous: &SessionControlState,
    ) -> Result<SessionControlState> {
        let archived_at_ms = crate::now_ms();
        let newly_archived = state
            .tasks
            .iter()
            .filter(|task| task_should_archive(task))
            .map(|task| kheish_types::ArchivedTaskRecord {
                task: task.clone(),
                archived_at_ms,
                reason: kheish_types::TaskArchiveReason::Terminal,
            })
            .collect::<Vec<_>>();
        state.archived_tasks = self
            .session_service
            .archive_session_tasks(session_id, &newly_archived)
            .await?;
        let mut persisted = state.clone();
        persisted.tasks.retain(|task| !task_should_archive(task));
        if &persisted != previous {
            self.session_service
                .save_session_control_state(session_id, &persisted)
                .await?;
        }
        self.session_service
            .remember_task_summary(session_id, &persisted)
            .await?;
        Ok(state)
    }

    /// Deletes one live task, leaving a tombstone in the archive so the
    /// disk-union merge can never resurrect it.
    pub(crate) async fn delete_session_task(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<kheish_types::TaskRecord> {
        let _guard = self.session_service.session_control_lock().lock().await;
        let previous = self
            .session_service
            .load_session_control_state(session_id)
            .await?;
        let mut state = previous.clone();
        let Some(index) = state.tasks.iter().position(|task| task.id == task_id) else {
            let archived = self.session_service.archived_task_index(session_id).await?;
            if archived.ids.contains(task_id) {
                anyhow::bail!("task {task_id} is archived and immutable");
            }
            anyhow::bail!("unknown task {task_id}");
        };
        if crate::shell_tasks::background_shell_metadata(&state.tasks[index]).is_some()
            && !task_is_terminal(&state.tasks[index].status)
        {
            anyhow::bail!(
                "daemon-managed shell task {task_id} is live; stop it before deleting it"
            );
        }
        let deleted = state.tasks.remove(index);
        state.archived_tasks = self
            .session_service
            .archive_session_tasks(
                session_id,
                &[kheish_types::ArchivedTaskRecord {
                    task: deleted.clone(),
                    archived_at_ms: crate::now_ms(),
                    reason: kheish_types::TaskArchiveReason::Deleted,
                }],
            )
            .await?;
        self.archive_and_persist_control_state_locked(session_id, state, &previous)
            .await?;
        Ok(deleted)
    }

    /// Returns the archived-task index (ids plus terminal tally) of one session.
    pub(crate) async fn archived_session_task_index(
        &self,
        session_id: &str,
    ) -> Result<std::sync::Arc<crate::services::ArchivedTaskIndex>> {
        self.session_service.archived_task_index(session_id).await
    }

    /// Loads the archived task records of one session, in archival order.
    pub(crate) async fn load_archived_session_tasks(
        &self,
        session_id: &str,
    ) -> Result<Vec<kheish_types::ArchivedTaskRecord>> {
        self.session_service
            .load_archived_session_tasks(session_id)
            .await
    }

    /// Finds the archived snapshot of one terminal task, when present.
    /// Deleted tombstones stay hidden.
    pub(crate) async fn find_archived_session_task(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<Option<kheish_types::TaskRecord>> {
        let archived = self.session_service.archived_task_index(session_id).await?;
        if !archived.ids.contains(task_id) {
            return Ok(None);
        }
        Ok(crate::services::latest_archived_terminal_task(
            self.session_service
                .load_archived_session_tasks(session_id)
                .await?,
            task_id,
        ))
    }

    pub(crate) async fn apply_requested_session_permission_mode(
        &self,
        session_id: &str,
        requested_mode: Option<Option<PermissionMode>>,
    ) -> Result<()> {
        let Some(mode) = requested_mode else {
            return Ok(());
        };
        let _guard = self.session_service.session_control_lock().lock().await;
        let mut state = self
            .session_service
            .load_session_control_state(session_id)
            .await?;
        let persisted_mode = mode.as_ref().map(Self::persisted_permission_mode_name);
        let mut changed = false;
        if matches!(mode, Some(PermissionMode::Plan)) {
            if !state.plan_mode {
                let previous_mode = self
                    .permissions
                    .session_mode(session_id)
                    .unwrap_or_else(|| self.permissions.mode());
                state.pre_plan_mode =
                    Some(Self::persisted_permission_mode_name(&previous_mode).to_string());
                changed = true;
            } else if state.pre_plan_mode.is_none() {
                state.pre_plan_mode = Some("default".to_string());
                changed = true;
            }
            if !state.plan_mode {
                state.plan_mode = true;
                changed = true;
            }
        } else {
            if state.plan_mode {
                state.plan_mode = false;
                changed = true;
            }
            if state.pre_plan_mode.take().is_some() {
                changed = true;
            }
        }
        if state.session_permission_mode.as_deref() != persisted_mode {
            state.session_permission_mode = persisted_mode.map(ToString::to_string);
            changed = true;
        }
        if changed {
            self.session_service
                .save_session_control_state(session_id, &state)
                .await?;
        }
        self.permissions
            .set_session_mode(session_id.to_string(), mode);
        Ok(())
    }

    pub(crate) async fn persist_session_permission_updates(
        &self,
        session_id: &str,
        updates: Vec<kheish_types::HookPermissionUpdate>,
    ) -> Result<()> {
        let _guard = self.session_service.session_control_lock().lock().await;
        let mut state = self
            .session_service
            .load_session_control_state(session_id)
            .await?;
        if state.session_permission_updates != updates {
            state.session_permission_updates = updates;
            self.session_service
                .save_session_control_state(session_id, &state)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn replace_session_permission_updates(
        &self,
        session_id: &str,
        updates: Vec<kheish_types::HookPermissionUpdate>,
    ) -> Result<()> {
        let _guard = self.session_service.session_control_lock().lock().await;
        let mut state = self
            .session_service
            .load_session_control_state(session_id)
            .await?;
        if state.session_permission_updates != updates {
            state.session_permission_updates = updates.clone();
            self.session_service
                .save_session_control_state(session_id, &state)
                .await?;
        }
        self.permissions
            .replace_session_rule_updates(session_id.to_string(), &updates);
        Ok(())
    }

    pub(crate) async fn enter_session_plan_mode(
        &self,
        session_id: &str,
    ) -> Result<SessionControlState> {
        let _guard = self.session_service.session_control_lock().lock().await;
        let mut state = self
            .session_service
            .load_session_control_state(session_id)
            .await?;
        let was_in_plan_mode = state.plan_mode;
        state.plan_mode = true;
        if !matches!(state.session_permission_mode.as_deref(), Some("plan")) {
            let previous_mode = self
                .permissions
                .session_mode(session_id)
                .unwrap_or_else(|| self.permissions.mode());
            if !was_in_plan_mode || state.pre_plan_mode.is_none() {
                state.pre_plan_mode =
                    Some(Self::persisted_permission_mode_name(&previous_mode).to_string());
            }
        }
        state.session_permission_mode = Some("plan".to_string());
        self.session_service
            .save_session_control_state(session_id, &state)
            .await?;
        self.permissions
            .set_session_mode(session_id.to_string(), Some(PermissionMode::Plan));
        Ok(state)
    }

    pub(crate) async fn exit_session_plan_mode(
        &self,
        session_id: &str,
        plan: String,
        summary: Option<String>,
    ) -> Result<crate::control_tools::ExitPlanModeOutcome> {
        let _guard = self.session_service.session_control_lock().lock().await;
        let mut state = self
            .session_service
            .load_session_control_state(session_id)
            .await?;
        let now = crate::now_ms();
        let plan_id = state
            .plan_artifact
            .as_ref()
            .map(|artifact| artifact.id.clone())
            .unwrap_or_else(|| format!("plan-{now}"));
        let created_at_ms = state
            .plan_artifact
            .as_ref()
            .map(|artifact| artifact.created_at_ms)
            .unwrap_or(now);
        state.plan_artifact = Some(kheish_types::PlanArtifact {
            id: plan_id,
            content: plan,
            summary,
            created_at_ms,
            updated_at_ms: now,
        });
        state.plan_mode = false;
        let restore_mode = state
            .pre_plan_mode
            .as_deref()
            .and_then(parse_permission_mode);
        let restored_permission_mode = restore_mode
            .as_ref()
            .map(Self::persisted_permission_mode_name);
        state.pre_plan_mode = None;
        state.session_permission_mode = restored_permission_mode.map(ToString::to_string);
        self.session_service
            .save_session_control_state(session_id, &state)
            .await?;
        self.permissions
            .set_session_mode(session_id.to_string(), restore_mode.clone());
        Ok(crate::control_tools::ExitPlanModeOutcome {
            state,
            restored_permission_mode: restore_mode,
        })
    }

    pub(crate) async fn restore_session_permission_state(&self) -> Result<()> {
        let session_ids = self.session_service.session_ids().await;
        for session_id in session_ids {
            let _guard = self.session_service.session_control_lock().lock().await;
            let mut state = self
                .session_service
                .load_session_control_state(&session_id)
                .await?;
            let mode = if state.plan_mode {
                Some(PermissionMode::Plan)
            } else {
                state
                    .session_permission_mode
                    .as_deref()
                    .and_then(parse_permission_mode)
            };
            let mut changed = false;
            if matches!(mode, Some(PermissionMode::Plan)) {
                if !state.plan_mode {
                    state.plan_mode = true;
                    changed = true;
                }
                if state.pre_plan_mode.is_none() {
                    state.pre_plan_mode = Some("default".to_string());
                    changed = true;
                }
                if state.session_permission_mode.as_deref() != Some("plan") {
                    state.session_permission_mode = Some("plan".to_string());
                    changed = true;
                }
            }
            if changed {
                self.session_service
                    .save_session_control_state(&session_id, &state)
                    .await?;
            }
            self.permissions.set_session_mode(session_id.clone(), mode);
            self.permissions
                .replace_session_rule_updates(session_id, &state.session_permission_updates);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn generic_session_control_saves_preserve_plan_and_permission_fields() {
        let current = SessionControlState {
            plan_mode: true,
            session_permission_mode: Some("plan".to_string()),
            session_permission_updates: vec![kheish_types::HookPermissionUpdate {
                scope: kheish_types::HookPermissionUpdateScope::Session,
                tool_name_pattern: "bash".to_string(),
                behavior: kheish_types::HookPermissionUpdateBehavior::Allow,
                reason: None,
            }],
            pre_plan_mode: Some("bypassPermissions".to_string()),
            plan_artifact: Some(kheish_types::PlanArtifact {
                id: "plan-1".to_string(),
                content: "existing plan".to_string(),
                summary: Some("existing summary".to_string()),
                created_at_ms: 1,
                updated_at_ms: 2,
            }),
            todos: vec![kheish_types::TodoItem {
                id: "todo-old".to_string(),
                content: "old".to_string(),
                completed: false,
            }],
            tasks: vec![kheish_types::TaskRecord {
                id: "task-old".to_string(),
                title: "old task".to_string(),
                description: String::new(),
                status: kheish_types::TaskStatus::Pending,
                owner_agent_id: None,
                blocked_by: Vec::new(),
                blocks: Vec::new(),
                output: None,
                metadata: json!(null),
                created_at_ms: 1,
                updated_at_ms: 1,
            }],
            archived_tasks: Default::default(),
        };
        let incoming = SessionControlState {
            todos: vec![kheish_types::TodoItem {
                id: "todo-new".to_string(),
                content: "new".to_string(),
                completed: true,
            }],
            tasks: vec![kheish_types::TaskRecord {
                id: "task-new".to_string(),
                title: "new task".to_string(),
                description: String::new(),
                status: kheish_types::TaskStatus::Completed,
                owner_agent_id: None,
                blocked_by: Vec::new(),
                blocks: Vec::new(),
                output: Some("done".to_string()),
                metadata: json!(null),
                created_at_ms: 3,
                updated_at_ms: 4,
            }],
            ..SessionControlState::default()
        };

        let merged = merge_generic_session_control_state(&current, incoming, &BTreeSet::new());

        assert!(merged.plan_mode);
        assert_eq!(merged.pre_plan_mode.as_deref(), Some("bypassPermissions"));
        assert_eq!(merged.session_permission_mode.as_deref(), Some("plan"));
        assert_eq!(
            merged.session_permission_updates,
            current.session_permission_updates
        );
        assert_eq!(merged.plan_artifact, current.plan_artifact);
        assert_eq!(merged.todos.len(), 1);
        assert_eq!(merged.todos[0].id, "todo-new");
        assert_eq!(merged.tasks.len(), 2);
        assert_eq!(merged.tasks[0].id, "task-new");
        assert_eq!(merged.tasks[1].id, "task-old");
    }

    #[test]
    fn generic_session_control_merge_does_not_revert_terminal_tasks() {
        let current = SessionControlState {
            tasks: vec![kheish_types::TaskRecord {
                id: "task-1".to_string(),
                title: "current".to_string(),
                description: String::new(),
                status: kheish_types::TaskStatus::Completed,
                owner_agent_id: None,
                blocked_by: Vec::new(),
                blocks: Vec::new(),
                output: Some("done".to_string()),
                metadata: json!({"exit_code": 0}),
                created_at_ms: 1,
                updated_at_ms: 10,
            }],
            ..SessionControlState::default()
        };
        let incoming = SessionControlState {
            tasks: vec![kheish_types::TaskRecord {
                id: "task-1".to_string(),
                title: "stale".to_string(),
                description: String::new(),
                status: kheish_types::TaskStatus::InProgress,
                owner_agent_id: None,
                blocked_by: Vec::new(),
                blocks: Vec::new(),
                output: None,
                metadata: json!({}),
                created_at_ms: 1,
                updated_at_ms: 5,
            }],
            ..SessionControlState::default()
        };

        let merged = merge_generic_session_control_state(&current, incoming, &BTreeSet::new());

        assert_eq!(merged.tasks.len(), 1);
        assert_eq!(merged.tasks[0].status, kheish_types::TaskStatus::Completed);
        assert_eq!(merged.tasks[0].updated_at_ms, 10);
        assert_eq!(merged.tasks[0].output.as_deref(), Some("done"));
    }

    #[test]
    fn generic_session_control_merge_never_resurrects_archived_tasks() {
        let task = |id: &str| kheish_types::TaskRecord {
            id: id.to_string(),
            title: id.to_string(),
            description: String::new(),
            status: kheish_types::TaskStatus::InProgress,
            owner_agent_id: None,
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            output: None,
            metadata: json!(null),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        // A pre-archival run snapshot re-sends an archived task, and a stale
        // on-disk state still carries a deleted one.
        let current = SessionControlState {
            tasks: vec![task("task-deleted"), task("task-live")],
            ..SessionControlState::default()
        };
        let incoming = SessionControlState {
            tasks: vec![task("task-archived"), task("task-live")],
            ..SessionControlState::default()
        };
        let archived_ids =
            BTreeSet::from(["task-archived".to_string(), "task-deleted".to_string()]);

        let merged = merge_generic_session_control_state(&current, incoming, &archived_ids);

        assert_eq!(
            merged
                .tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            vec!["task-live"]
        );
    }
}
