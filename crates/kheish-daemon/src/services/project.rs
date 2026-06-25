use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};
use tokio::sync::Mutex;

use crate::projects::{
    FileProjectStore, ProjectMemberView, ProjectStatus, ProjectTaskDiscussionRef, ProjectTaskView,
    ProjectView,
};
use kheish_types::TaskStatus;

#[derive(Clone, Debug)]
pub(crate) struct ProjectTaskStartReservation {
    pub(crate) previous: ProjectTaskView,
}

#[derive(Clone, Debug, Default)]
struct ProjectState {
    projects: BTreeMap<String, ProjectView>,
    tasks: BTreeMap<String, ProjectTaskView>,
    tasks_by_project: BTreeMap<String, BTreeSet<String>>,
    projects_by_session: BTreeMap<String, BTreeSet<String>>,
    projects_by_channel: BTreeMap<String, BTreeSet<String>>,
}

impl ProjectState {
    fn new(
        projects: BTreeMap<String, ProjectView>,
        tasks: BTreeMap<String, ProjectTaskView>,
    ) -> Self {
        let mut state = Self {
            projects,
            tasks,
            tasks_by_project: BTreeMap::new(),
            projects_by_session: BTreeMap::new(),
            projects_by_channel: BTreeMap::new(),
        };
        state.rebuild_indexes();
        state
    }

    fn rebuild_indexes(&mut self) {
        self.tasks_by_project.clear();
        self.projects_by_session.clear();
        self.projects_by_channel.clear();
        for task in self.tasks.values() {
            self.tasks_by_project
                .entry(task.project_id.clone())
                .or_default()
                .insert(task.project_task_id.clone());
        }
        for project in self.projects.values_mut() {
            project.summary.member_count = project.members.len() as u64;
            project.summary.channel_count = project.channel_links.len() as u64;
            let task_ids = self
                .tasks_by_project
                .get(&project.summary.project_id)
                .cloned()
                .unwrap_or_default();
            project.summary.task_count = task_ids.len() as u64;
            project.summary.active_task_count = task_ids
                .iter()
                .filter_map(|task_id| self.tasks.get(task_id))
                .filter(|task| !is_terminal_task_status(&task.status))
                .count() as u64;
            for member in &project.members {
                if let Some(session_id) = member.session_id.as_deref() {
                    self.projects_by_session
                        .entry(session_id.to_string())
                        .or_default()
                        .insert(project.summary.project_id.clone());
                }
            }
            for link in &project.channel_links {
                self.projects_by_channel
                    .entry(link.channel_id.clone())
                    .or_default()
                    .insert(project.summary.project_id.clone());
            }
        }
    }
}

fn is_terminal_task_status(status: &TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    )
}

fn task_status_label(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn validate_project_task_dependencies(state: &ProjectState) -> Result<()> {
    for task in state.tasks.values() {
        for dependency in &task.blocked_by {
            anyhow::ensure!(
                dependency != &task.project_task_id,
                "project task {} cannot depend on itself",
                task.project_task_id
            );
            let dependency_task = state.tasks.get(dependency).ok_or_else(|| {
                anyhow!(
                    "unknown project task dependency {} for task {}",
                    dependency,
                    task.project_task_id
                )
            })?;
            anyhow::ensure!(
                dependency_task.project_id == task.project_id,
                "project task dependency {} does not belong to project {}",
                dependency,
                task.project_id
            );
        }
    }

    for task in state.tasks.values() {
        let mut visiting = BTreeSet::new();
        if project_task_dependency_reaches(
            state,
            &task.project_id,
            &task.project_task_id,
            &task.project_task_id,
            &mut visiting,
        ) {
            anyhow::bail!(
                "project task {} dependency update introduces a dependency cycle",
                task.project_task_id
            );
        }
    }
    Ok(())
}

fn project_task_dependency_reaches(
    state: &ProjectState,
    project_id: &str,
    current_task_id: &str,
    target_task_id: &str,
    visiting: &mut BTreeSet<String>,
) -> bool {
    if !visiting.insert(current_task_id.to_string()) {
        return false;
    }
    let Some(task) = state.tasks.get(current_task_id) else {
        return false;
    };
    if task.project_id != project_id {
        return false;
    }
    task.blocked_by.iter().any(|dependency| {
        dependency == target_task_id
            || project_task_dependency_reaches(
                state,
                project_id,
                dependency,
                target_task_id,
                visiting,
            )
    })
}

/// Owns durable projects, project members, linked channels, and project tasks.
pub(crate) struct ProjectService {
    store: FileProjectStore,
    state: Mutex<ProjectState>,
    next_project_id: AtomicU64,
    next_task_id: AtomicU64,
}

impl ProjectService {
    /// Creates a new project service backed by persisted daemon state.
    pub(crate) fn new(
        store: FileProjectStore,
        projects: BTreeMap<String, ProjectView>,
        tasks: BTreeMap<String, ProjectTaskView>,
        next_project_id: AtomicU64,
        next_task_id: AtomicU64,
    ) -> Self {
        Self {
            store,
            state: Mutex::new(ProjectState::new(projects, tasks)),
            next_project_id,
            next_task_id,
        }
    }

    /// Returns one fresh daemon-managed project identifier.
    pub(crate) fn next_project_id(&self) -> String {
        format!(
            "project-{}",
            self.next_project_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Returns one fresh daemon-managed project-task identifier.
    pub(crate) fn next_task_id(&self) -> String {
        format!(
            "project-task-{}",
            self.next_task_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Lists projects filtered by optional query, status, session member, and linked channel.
    pub(crate) async fn list_projects(
        &self,
        query: Option<&str>,
        status: Option<&ProjectStatus>,
        member_session_id: Option<&str>,
        channel_id: Option<&str>,
    ) -> Vec<ProjectView> {
        let query = query
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        let state = self.state.lock().await;
        let allowed_by_session = member_session_id.map(|session_id| {
            state
                .projects_by_session
                .get(session_id)
                .cloned()
                .unwrap_or_default()
        });
        let allowed_by_channel = channel_id.map(|channel_id| {
            state
                .projects_by_channel
                .get(channel_id)
                .cloned()
                .unwrap_or_default()
        });
        let mut projects = state
            .projects
            .values()
            .filter(|project| {
                status.is_none_or(|status| &project.summary.status == status)
                    && allowed_by_session
                        .as_ref()
                        .is_none_or(|allowed| allowed.contains(&project.summary.project_id))
                    && allowed_by_channel
                        .as_ref()
                        .is_none_or(|allowed| allowed.contains(&project.summary.project_id))
                    && query.as_ref().is_none_or(|query| {
                        project
                            .summary
                            .project_id
                            .to_ascii_lowercase()
                            .contains(query)
                            || project
                                .summary
                                .display_name
                                .to_ascii_lowercase()
                                .contains(query)
                            || project
                                .summary
                                .description
                                .as_deref()
                                .unwrap_or_default()
                                .to_ascii_lowercase()
                                .contains(query)
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        projects.sort_by(|left, right| left.summary.project_id.cmp(&right.summary.project_id));
        projects
    }

    /// Returns one project by identifier.
    pub(crate) async fn get_project(&self, project_id: &str) -> Result<ProjectView> {
        self.state
            .lock()
            .await
            .projects
            .get(project_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown project {project_id}"))
    }

    /// Persists one new project record.
    pub(crate) async fn create_project(&self, project: ProjectView) -> Result<ProjectView> {
        let mut state = self.state.lock().await;
        if state.projects.contains_key(&project.summary.project_id) {
            bail!("project {} already exists", project.summary.project_id);
        }
        self.store.save_project(&project)?;
        state
            .projects
            .insert(project.summary.project_id.clone(), project.clone());
        state.rebuild_indexes();
        Ok(project)
    }

    /// Updates one persisted project record in place.
    pub(crate) async fn update_project(
        &self,
        project_id: &str,
        update: impl FnOnce(&mut ProjectView) -> Result<bool>,
    ) -> Result<ProjectView> {
        let mut state = self.state.lock().await;
        let project = state
            .projects
            .get_mut(project_id)
            .ok_or_else(|| anyhow!("unknown project {project_id}"))?;
        let previous = project.clone();
        if !update(project)? {
            return Ok(previous);
        }
        state.rebuild_indexes();
        let updated = state
            .projects
            .get(project_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown project {project_id}"))?;
        if let Err(error) = self.store.save_project(&updated) {
            state
                .projects
                .insert(project_id.to_string(), previous.clone());
            state.rebuild_indexes();
            return Err(error);
        }
        Ok(updated)
    }

    /// Removes one persisted project record when it has no remaining tasks.
    pub(crate) async fn delete_project(&self, project_id: &str) -> Result<()> {
        let mut state = self.state.lock().await;
        let task_ids = state
            .tasks_by_project
            .get(project_id)
            .cloned()
            .unwrap_or_default();
        if !task_ids.is_empty() {
            bail!(
                "cannot delete project {project_id}; it still has tasks {}",
                task_ids.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
        let removed = state
            .projects
            .remove(project_id)
            .ok_or_else(|| anyhow!("unknown project {project_id}"))?;
        if let Err(error) = self.store.delete_project(project_id) {
            state.projects.insert(project_id.to_string(), removed);
            state.rebuild_indexes();
            return Err(error);
        }
        state.rebuild_indexes();
        Ok(())
    }

    /// Lists project tasks filtered by optional query, status, and assignee.
    pub(crate) async fn list_tasks(
        &self,
        project_id: &str,
        query: Option<&str>,
        status: Option<&TaskStatus>,
        assignee_member_id: Option<&str>,
    ) -> Result<Vec<ProjectTaskView>> {
        let query = query
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        let state = self.state.lock().await;
        if !state.projects.contains_key(project_id) {
            bail!("unknown project {project_id}");
        }
        let mut tasks = state
            .tasks_by_project
            .get(project_id)
            .into_iter()
            .flat_map(|task_ids| task_ids.iter())
            .filter_map(|task_id| state.tasks.get(task_id))
            .filter(|task| {
                status.is_none_or(|status| &task.status == status)
                    && assignee_member_id.is_none_or(|assignee_member_id| {
                        task.assignee_member_id.as_deref() == Some(assignee_member_id)
                    })
                    && query.as_ref().is_none_or(|query| {
                        task.project_task_id.to_ascii_lowercase().contains(query)
                            || task.title.to_ascii_lowercase().contains(query)
                            || task.description.to_ascii_lowercase().contains(query)
                            || task
                                .output
                                .as_deref()
                                .unwrap_or_default()
                                .to_ascii_lowercase()
                                .contains(query)
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        tasks.sort_by(|left, right| left.project_task_id.cmp(&right.project_task_id));
        Ok(tasks)
    }

    /// Returns one project task by identifier.
    pub(crate) async fn get_task(
        &self,
        project_id: &str,
        task_id: &str,
    ) -> Result<ProjectTaskView> {
        let state = self.state.lock().await;
        let task = state
            .tasks
            .get(task_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown project task {task_id}"))?;
        anyhow::ensure!(
            task.project_id == project_id,
            "project task {task_id} does not belong to project {project_id}"
        );
        Ok(task)
    }

    /// Returns all project tasks whose latest run matches the supplied run identifier.
    pub(crate) async fn tasks_for_latest_run(&self, run_id: &str) -> Vec<ProjectTaskView> {
        let mut tasks = self
            .state
            .lock()
            .await
            .tasks
            .values()
            .filter(|task| task.latest_run_id.as_deref() == Some(run_id))
            .cloned()
            .collect::<Vec<_>>();
        tasks.sort_by(|left, right| {
            left.project_id
                .cmp(&right.project_id)
                .then_with(|| left.project_task_id.cmp(&right.project_task_id))
        });
        tasks
    }

    /// Returns all project tasks that currently reference a latest run.
    pub(crate) async fn tasks_with_latest_runs(&self) -> Vec<ProjectTaskView> {
        let mut tasks = self
            .state
            .lock()
            .await
            .tasks
            .values()
            .filter(|task| task.latest_run_id.is_some())
            .cloned()
            .collect::<Vec<_>>();
        tasks.sort_by(|left, right| {
            left.project_id
                .cmp(&right.project_id)
                .then_with(|| left.project_task_id.cmp(&right.project_task_id))
        });
        tasks
    }

    /// Persists one new project-task record.
    pub(crate) async fn create_task(&self, task: ProjectTaskView) -> Result<ProjectTaskView> {
        let mut state = self.state.lock().await;
        if state.tasks.contains_key(&task.project_task_id) {
            bail!("project task {} already exists", task.project_task_id);
        }
        let previous_project = state
            .projects
            .get(&task.project_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown project {}", task.project_id))?;
        state
            .tasks
            .insert(task.project_task_id.clone(), task.clone());
        if let Err(error) = validate_project_task_dependencies(&state) {
            state.tasks.remove(&task.project_task_id);
            return Err(error);
        }
        if let Err(error) = self.store.save_task(&task) {
            state.tasks.remove(&task.project_task_id);
            return Err(error);
        }
        state.rebuild_indexes();
        if let Some(project) = state.projects.get_mut(&task.project_id) {
            project.summary.updated_at_ms = crate::now_ms();
        }
        let updated_project = state
            .projects
            .get(&task.project_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown project {}", task.project_id))?;
        if let Err(error) = self.store.save_project(&updated_project) {
            state.tasks.remove(&task.project_task_id);
            state.projects.insert(
                previous_project.summary.project_id.clone(),
                previous_project,
            );
            state.rebuild_indexes();
            return match self.store.delete_task(&task.project_task_id) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(anyhow!(
                    "failed to persist project summary after creating task {}; rollback also failed: {rollback_error}",
                    task.project_task_id
                )),
            };
        }
        Ok(task)
    }

    /// Updates one persisted project-task record in place.
    pub(crate) async fn update_task(
        &self,
        project_id: &str,
        task_id: &str,
        update: impl FnOnce(&mut ProjectTaskView) -> Result<bool>,
    ) -> Result<ProjectTaskView> {
        let mut state = self.state.lock().await;
        let previous_project = state
            .projects
            .get(project_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown project {project_id}"))?;
        let task = state
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| anyhow!("unknown project task {task_id}"))?;
        anyhow::ensure!(
            task.project_id == project_id,
            "project task {task_id} does not belong to project {project_id}"
        );
        let previous_task = task.clone();
        if !update(task)? {
            return Ok(previous_task);
        }
        if let Err(error) = validate_project_task_dependencies(&state) {
            state
                .tasks
                .insert(task_id.to_string(), previous_task.clone());
            state.rebuild_indexes();
            return Err(error);
        }
        state.rebuild_indexes();
        if let Some(project) = state.projects.get_mut(project_id) {
            project.summary.updated_at_ms = crate::now_ms();
        }
        let updated_task = state
            .tasks
            .get(task_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown project task {task_id}"))?;
        let updated_project = state
            .projects
            .get(project_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown project {project_id}"))?;
        if let Err(error) = self.store.save_task(&updated_task) {
            state
                .tasks
                .insert(task_id.to_string(), previous_task.clone());
            state.projects.insert(
                previous_project.summary.project_id.clone(),
                previous_project,
            );
            state.rebuild_indexes();
            return Err(error);
        }
        if let Err(error) = self.store.save_project(&updated_project) {
            state
                .tasks
                .insert(task_id.to_string(), previous_task.clone());
            state.projects.insert(
                previous_project.summary.project_id.clone(),
                previous_project,
            );
            state.rebuild_indexes();
            return match self.store.save_task(&previous_task) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(anyhow!(
                    "failed to persist project summary after updating task {task_id}; rollback also failed: {rollback_error}"
                )),
            };
        }
        Ok(updated_task)
    }

    /// Reserves a task start before the run is submitted, closing the double-start window.
    pub(crate) async fn reserve_task_start(
        &self,
        project_id: &str,
        task_id: &str,
        expected_latest_run_id: Option<&str>,
        expected_assignee_member_id: Option<&str>,
        run_id: &str,
        session_id: &str,
        discussion: Option<ProjectTaskDiscussionRef>,
    ) -> Result<ProjectTaskStartReservation> {
        let mut state = self.state.lock().await;
        let project = state
            .projects
            .get(project_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown project {project_id}"))?;
        anyhow::ensure!(
            project.summary.status == ProjectStatus::Active,
            "project {} is {} and cannot accept new work",
            project.summary.project_id,
            format!("{:?}", project.summary.status).to_ascii_lowercase()
        );
        let previous_project = project.clone();
        let current_task = state
            .tasks
            .get(task_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown project task {task_id}"))?;
        anyhow::ensure!(
            current_task.project_id == project_id,
            "project task {task_id} does not belong to project {project_id}"
        );
        anyhow::ensure!(
            !is_terminal_task_status(&current_task.status),
            "project task {task_id} is terminal and must be reopened before it can be started again"
        );
        anyhow::ensure!(
            current_task.latest_run_id.as_deref() == expected_latest_run_id,
            "project task {task_id} changed while start was being prepared"
        );
        anyhow::ensure!(
            current_task.assignee_member_id.as_deref() == expected_assignee_member_id,
            "project task {task_id} assignment changed while start was being prepared"
        );
        let assignee_member_id = current_task
            .assignee_member_id
            .as_deref()
            .ok_or_else(|| anyhow!("project task {task_id} is not assigned"))?;
        let assignee = project
            .members
            .iter()
            .find(|member| member.member_id == assignee_member_id)
            .ok_or_else(|| anyhow!("unknown project member {assignee_member_id}"))?;
        anyhow::ensure!(
            assignee.member_kind == crate::ChannelMemberKind::Session,
            "project task assignees must be session-backed members"
        );
        anyhow::ensure!(
            assignee.session_id.as_deref() == Some(session_id),
            "project task {task_id} assignment changed while start was being prepared"
        );
        let mut pending_dependencies = Vec::new();
        for dependency in &current_task.blocked_by {
            let Some(dependency_task) = state.tasks.get(dependency) else {
                bail!("unknown project task dependency {dependency}");
            };
            anyhow::ensure!(
                dependency_task.project_id == project_id,
                "project task dependency {dependency} does not belong to project {project_id}"
            );
            if dependency_task.status != TaskStatus::Completed {
                pending_dependencies.push(format!(
                    "{} ({})",
                    dependency,
                    task_status_label(&dependency_task.status)
                ));
            }
        }
        anyhow::ensure!(
            pending_dependencies.is_empty(),
            "project task {task_id} is still blocked by {}",
            pending_dependencies.join(", ")
        );
        let task = state
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| anyhow!("unknown project task {task_id}"))?;
        let previous_task = current_task;
        task.latest_run_id = Some(run_id.to_string());
        task.primary_session_id = Some(session_id.to_string());
        task.status = TaskStatus::Pending;
        task.discussion = discussion;
        task.output = None;
        task.updated_at_ms = crate::now_ms();
        state.rebuild_indexes();
        if let Some(project) = state.projects.get_mut(project_id) {
            project.summary.updated_at_ms = crate::now_ms();
        }
        let reserved_task = state
            .tasks
            .get(task_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown project task {task_id}"))?;
        let updated_project = state
            .projects
            .get(project_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown project {project_id}"))?;
        if let Err(error) = self.store.save_task(&reserved_task) {
            state
                .tasks
                .insert(task_id.to_string(), previous_task.clone());
            state.projects.insert(
                previous_project.summary.project_id.clone(),
                previous_project,
            );
            state.rebuild_indexes();
            return Err(error);
        }
        if let Err(error) = self.store.save_project(&updated_project) {
            state
                .tasks
                .insert(task_id.to_string(), previous_task.clone());
            state.projects.insert(
                previous_project.summary.project_id.clone(),
                previous_project,
            );
            state.rebuild_indexes();
            return match self.store.save_task(&previous_task) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(anyhow!(
                    "failed to persist project summary after reserving task {task_id}; rollback also failed: {rollback_error}"
                )),
            };
        }
        Ok(ProjectTaskStartReservation {
            previous: previous_task,
        })
    }

    /// Removes one persisted project-task record.
    pub(crate) async fn delete_task(&self, project_id: &str, task_id: &str) -> Result<bool> {
        let mut state = self.state.lock().await;
        let previous_project = state
            .projects
            .get(project_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown project {project_id}"))?;
        let Some(previous_task) = state.tasks.get(task_id).cloned() else {
            return Ok(false);
        };
        anyhow::ensure!(
            previous_task.project_id == project_id,
            "project task {task_id} does not belong to project {project_id}"
        );
        let dependents = state
            .tasks
            .values()
            .filter(|task| task.project_id == project_id)
            .filter(|task| {
                task.blocked_by
                    .iter()
                    .any(|dependency| dependency == task_id)
            })
            .map(|task| task.project_task_id.clone())
            .collect::<Vec<_>>();
        if !dependents.is_empty() {
            bail!(
                "cannot delete project task {task_id}; it still blocks tasks {}",
                dependents.join(", ")
            );
        }
        self.store.delete_task(task_id)?;
        state.tasks.remove(task_id);
        state.rebuild_indexes();
        if let Some(project) = state.projects.get_mut(project_id) {
            project.summary.updated_at_ms = crate::now_ms();
        }
        let updated_project = state
            .projects
            .get(project_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown project {project_id}"))?;
        if let Err(error) = self.store.save_project(&updated_project) {
            state
                .tasks
                .insert(task_id.to_string(), previous_task.clone());
            state.projects.insert(
                previous_project.summary.project_id.clone(),
                previous_project,
            );
            state.rebuild_indexes();
            return match self.store.save_task(&previous_task) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(anyhow!(
                    "failed to persist project summary after deleting task {task_id}; rollback also failed: {rollback_error}"
                )),
            };
        }
        Ok(true)
    }

    /// Returns the project identifiers that currently reference one channel.
    pub(crate) async fn project_ids_for_channel(&self, channel_id: &str) -> Vec<String> {
        self.state
            .lock()
            .await
            .projects_by_channel
            .get(channel_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect()
    }

    /// Returns the project identifiers that currently reference one session member.
    pub(crate) async fn project_ids_for_session(&self, session_id: &str) -> Vec<String> {
        self.state
            .lock()
            .await
            .projects_by_session
            .get(session_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect()
    }

    /// Returns the project member bound to one session when it exists.
    pub(crate) async fn member_for_session(
        &self,
        project_id: &str,
        session_id: &str,
    ) -> Option<ProjectMemberView> {
        self.state
            .lock()
            .await
            .projects
            .get(project_id)
            .and_then(|project| {
                project
                    .members
                    .iter()
                    .find(|member| member.session_id.as_deref() == Some(session_id))
                    .cloned()
            })
    }

    /// Returns one project member by identifier.
    pub(crate) async fn member(
        &self,
        project_id: &str,
        member_id: &str,
    ) -> Result<ProjectMemberView> {
        let project = self.get_project(project_id).await?;
        project
            .members
            .into_iter()
            .find(|member| member.member_id == member_id)
            .ok_or_else(|| anyhow!("unknown project member {member_id}"))
    }

    /// Returns the non-terminal task identifiers currently assigned to one project member.
    pub(crate) async fn nonterminal_task_ids_for_member(
        &self,
        project_id: &str,
        member_id: &str,
    ) -> Result<Vec<String>> {
        Ok(self
            .list_tasks(project_id, None, None, Some(member_id))
            .await?
            .into_iter()
            .filter(|task| !is_terminal_task_status(&task.status))
            .map(|task| task.project_task_id)
            .collect())
    }

    /// Returns the non-terminal task identifiers currently anchored to one channel.
    pub(crate) async fn nonterminal_task_ids_for_channel(
        &self,
        project_id: &str,
        channel_id: &str,
    ) -> Result<Vec<String>> {
        Ok(self
            .list_tasks(project_id, None, None, None)
            .await?
            .into_iter()
            .filter(|task| {
                !is_terminal_task_status(&task.status)
                    && task
                        .discussion
                        .as_ref()
                        .is_some_and(|discussion| discussion.channel_id == channel_id)
            })
            .map(|task| task.project_task_id)
            .collect())
    }

    /// Returns the non-terminal project-task identifiers currently assigned to one session.
    pub(crate) async fn nonterminal_task_ids_for_session(&self, session_id: &str) -> Vec<String> {
        let state = self.state.lock().await;
        state
            .tasks
            .values()
            .filter(|task| {
                task.primary_session_id.as_deref() == Some(session_id)
                    && !is_terminal_task_status(&task.status)
            })
            .map(|task| task.project_task_id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serde_json::{Value, json};
    use tempfile::tempdir;

    use super::*;
    use crate::ProjectSummaryView;

    fn fixture_project(project_id: &str) -> ProjectView {
        ProjectView {
            summary: ProjectSummaryView {
                project_id: project_id.to_string(),
                display_name: "alpha".to_string(),
                description: Some("desc".to_string()),
                status: ProjectStatus::Active,
                member_count: 1,
                channel_count: 0,
                task_count: 0,
                active_task_count: 0,
                created_at_ms: 1,
                updated_at_ms: 1,
            },
            members: vec![ProjectMemberView {
                member_id: "member-1".to_string(),
                member_kind: crate::ChannelMemberKind::Session,
                display_name: "agent".to_string(),
                session_id: Some("session-1".to_string()),
                actor_id: None,
                role: Some("worker".to_string()),
                expertise_tags: vec!["rust".to_string()],
                joined_at_ms: 1,
                metadata: Value::Null,
            }],
            channel_links: Vec::new(),
            metadata: json!({"fixture": true}),
        }
    }

    #[tokio::test]
    async fn create_task_updates_project_summary_counts() -> Result<()> {
        let temp = tempdir()?;
        let store = FileProjectStore::new(temp.path());
        let service = ProjectService::new(
            store,
            BTreeMap::from([("project-1".to_string(), fixture_project("project-1"))]),
            BTreeMap::new(),
            AtomicU64::new(2),
            AtomicU64::new(2),
        );
        service
            .create_task(ProjectTaskView {
                project_task_id: "project-task-1".to_string(),
                project_id: "project-1".to_string(),
                title: "work".to_string(),
                description: "desc".to_string(),
                status: TaskStatus::Pending,
                assignee_member_id: Some("member-1".to_string()),
                primary_session_id: Some("session-1".to_string()),
                latest_run_id: None,
                discussion: None,
                blocked_by: Vec::new(),
                output: None,
                created_at_ms: 2,
                updated_at_ms: 2,
                metadata: Value::Null,
            })
            .await?;
        let project = service.get_project("project-1").await?;
        assert_eq!(project.summary.task_count, 1);
        assert_eq!(project.summary.active_task_count, 1);
        assert!(project.summary.updated_at_ms > 1);
        Ok(())
    }

    #[tokio::test]
    async fn delete_project_rejects_when_tasks_remain() -> Result<()> {
        let temp = tempdir()?;
        let store = FileProjectStore::new(temp.path());
        let service = ProjectService::new(
            store,
            BTreeMap::from([("project-1".to_string(), fixture_project("project-1"))]),
            BTreeMap::from([(
                "project-task-1".to_string(),
                ProjectTaskView {
                    project_task_id: "project-task-1".to_string(),
                    project_id: "project-1".to_string(),
                    title: "work".to_string(),
                    description: "desc".to_string(),
                    status: TaskStatus::Pending,
                    assignee_member_id: Some("member-1".to_string()),
                    primary_session_id: Some("session-1".to_string()),
                    latest_run_id: None,
                    discussion: None,
                    blocked_by: Vec::new(),
                    output: None,
                    created_at_ms: 2,
                    updated_at_ms: 2,
                    metadata: Value::Null,
                },
            )]),
            AtomicU64::new(2),
            AtomicU64::new(2),
        );
        let error = service
            .delete_project("project-1")
            .await
            .expect_err("delete should fail while tasks remain");
        assert!(error.to_string().contains("still has tasks"));
        Ok(())
    }

    #[tokio::test]
    async fn reserve_task_start_revalidates_project_assignment_and_dependencies() -> Result<()> {
        let temp = tempdir()?;
        let store = FileProjectStore::new(temp.path());
        let task_a = ProjectTaskView {
            project_task_id: "project-task-a".to_string(),
            project_id: "project-1".to_string(),
            title: "dependency".to_string(),
            description: "desc".to_string(),
            status: TaskStatus::Pending,
            assignee_member_id: Some("member-1".to_string()),
            primary_session_id: Some("session-1".to_string()),
            latest_run_id: None,
            discussion: None,
            blocked_by: Vec::new(),
            output: None,
            created_at_ms: 2,
            updated_at_ms: 2,
            metadata: Value::Null,
        };
        let task_b = ProjectTaskView {
            project_task_id: "project-task-b".to_string(),
            project_id: "project-1".to_string(),
            title: "blocked".to_string(),
            description: "desc".to_string(),
            status: TaskStatus::Pending,
            assignee_member_id: Some("member-1".to_string()),
            primary_session_id: Some("session-1".to_string()),
            latest_run_id: None,
            discussion: None,
            blocked_by: vec!["project-task-a".to_string()],
            output: None,
            created_at_ms: 3,
            updated_at_ms: 3,
            metadata: Value::Null,
        };
        let service = ProjectService::new(
            store,
            BTreeMap::from([("project-1".to_string(), fixture_project("project-1"))]),
            BTreeMap::from([
                (task_a.project_task_id.clone(), task_a),
                (task_b.project_task_id.clone(), task_b),
            ]),
            AtomicU64::new(2),
            AtomicU64::new(3),
        );

        let blocked = service
            .reserve_task_start(
                "project-1",
                "project-task-b",
                None,
                Some("member-1"),
                "run-1",
                "session-1",
                None,
            )
            .await
            .expect_err("dependency should block reservation");
        assert!(blocked.to_string().contains("still blocked"));

        service
            .update_task("project-1", "project-task-a", |task| {
                task.status = TaskStatus::Completed;
                Ok(true)
            })
            .await?;
        service
            .update_project("project-1", |project| {
                project.summary.status = ProjectStatus::Paused;
                Ok(true)
            })
            .await?;
        let paused = service
            .reserve_task_start(
                "project-1",
                "project-task-b",
                None,
                Some("member-1"),
                "run-2",
                "session-1",
                None,
            )
            .await
            .expect_err("paused project should block reservation");
        assert!(paused.to_string().contains("cannot accept new work"));

        service
            .update_project("project-1", |project| {
                project.summary.status = ProjectStatus::Active;
                Ok(true)
            })
            .await?;
        let stale_assignment = service
            .reserve_task_start(
                "project-1",
                "project-task-b",
                None,
                Some("other-member"),
                "run-3",
                "session-1",
                None,
            )
            .await
            .expect_err("stale assignment should block reservation");
        assert!(stale_assignment.to_string().contains("assignment changed"));

        let reservation = service
            .reserve_task_start(
                "project-1",
                "project-task-b",
                None,
                Some("member-1"),
                "run-4",
                "session-1",
                None,
            )
            .await?;
        assert_eq!(reservation.previous.latest_run_id, None);
        let reserved = service.get_task("project-1", "project-task-b").await?;
        assert_eq!(reserved.latest_run_id.as_deref(), Some("run-4"));
        assert_eq!(reserved.status, TaskStatus::Pending);
        Ok(())
    }
}
