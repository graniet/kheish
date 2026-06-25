//! Project lifecycle methods implemented on [`DaemonState`].

use super::*;
use kheish_runtime::ModelGenerationConfig;
use kheish_types::{Role, SessionEvent, TaskStatus};
use serde_json::{Map, Value};

const PROJECT_CONTROL_MEMBER_ID: &str = "project-control";
const PROJECT_CONTROL_DISPLAY_NAME: &str = "Project Control";

fn trim_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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

fn mirrored_channel_member_prefix(project_id: &str) -> String {
    format!("project:{project_id}:")
}

fn mirrored_channel_member_id(project_id: &str, member_id: &str) -> String {
    format!(
        "{}{}",
        mirrored_channel_member_prefix(project_id),
        member_id
    )
}

fn project_task_status_for_run(status: &crate::DaemonRunStatus) -> TaskStatus {
    match status {
        crate::DaemonRunStatus::Queued => TaskStatus::Pending,
        crate::DaemonRunStatus::Running
        | crate::DaemonRunStatus::WaitingForApproval
        | crate::DaemonRunStatus::WaitingForUserQuestion => TaskStatus::InProgress,
        crate::DaemonRunStatus::Completed => TaskStatus::Completed,
        crate::DaemonRunStatus::Failed | crate::DaemonRunStatus::Interrupted => TaskStatus::Failed,
        crate::DaemonRunStatus::Cancelled => TaskStatus::Cancelled,
    }
}

fn ensure_unique_project_session_members(
    project_id: &str,
    members: &[crate::projects::ProjectMemberView],
) -> Result<()> {
    let mut seen = Vec::<(&str, &str)>::new();
    for member in members {
        let Some(session_id) = member.session_id.as_deref() else {
            continue;
        };
        if let Some((existing_member_id, _)) = seen
            .iter()
            .find(|(_, existing_session_id)| *existing_session_id == session_id)
        {
            anyhow::bail!(
                "session {session_id} is already registered as another project member in project {project_id} ({existing_member_id})"
            );
        }
        seen.push((member.member_id.as_str(), session_id));
    }
    Ok(())
}

fn default_project_task_kickoff_message(
    project: &crate::projects::ProjectView,
    task: &crate::projects::ProjectTaskView,
    assignee: &crate::projects::ProjectMemberView,
) -> String {
    let mut lines = vec![
        format!(
            "Project {} assigned task {} to {}.",
            project.summary.display_name, task.project_task_id, assignee.display_name
        ),
        format!("Task: {}.", task.title),
    ];
    if !task.description.is_empty() {
        lines.push(format!("Description: {}.", task.description));
    }
    lines.push("Use this thread for project-visible coordination.".to_string());
    lines.join(" ")
}

fn render_project_task_start_prompt(
    project: &crate::projects::ProjectView,
    task: &crate::projects::ProjectTaskView,
    assignee: &crate::projects::ProjectMemberView,
    discussion: Option<&crate::projects::ProjectTaskDiscussionRef>,
) -> String {
    let mut lines = vec![
        format!(
            "You are assigned project task {} in project {}.",
            task.project_task_id, project.summary.display_name
        ),
        format!("Task title: {}", task.title),
    ];
    if let Some(description) = project.summary.description.as_deref() {
        lines.push(format!("Project description: {}", description));
    }
    if !task.description.is_empty() {
        lines.push(format!("Task description: {}", task.description));
    }
    if let Some(role) = assignee.role.as_deref() {
        lines.push(format!("Your project role: {}", role));
    }
    if !task.blocked_by.is_empty() {
        lines.push(format!(
            "Declared dependencies already satisfied: {}",
            task.blocked_by.join(", ")
        ));
    }
    if let Some(discussion) = discussion {
        lines.push(format!(
            "Public coordination channel: {} thread {}. Use the channel messaging tools for project-visible updates.",
            discussion.channel_id, discussion.thread_root_message_id
        ));
    }
    if !project.members.is_empty() {
        let roster = project
            .members
            .iter()
            .map(|member| {
                let role = member
                    .role
                    .as_deref()
                    .map(|role| format!(" ({role})"))
                    .unwrap_or_default();
                format!("{}{}", member.display_name, role)
            })
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("Project members: {}", roster));
    }
    lines.push(
        "Use spawn_agent, mailbox, schedules, and channel tools when they materially help you complete the task."
            .to_string(),
    );
    lines.push(
        "When you reach a meaningful milestone, communicate it in the project thread if one is available."
            .to_string(),
    );
    lines.join("\n")
}

fn build_project_task_run_metadata(
    project_id: &str,
    task_id: &str,
    discussion: Option<&crate::projects::ProjectTaskDiscussionRef>,
    request_metadata: Value,
) -> Value {
    let mut metadata = Map::new();
    metadata.insert(
        "project_id".to_string(),
        Value::String(project_id.to_string()),
    );
    metadata.insert(
        "project_task_id".to_string(),
        Value::String(task_id.to_string()),
    );
    metadata.insert(
        "source_plugin".to_string(),
        Value::String("project_task".to_string()),
    );
    if let Some(discussion) = discussion {
        metadata.insert(
            "discussion_channel_id".to_string(),
            Value::String(discussion.channel_id.clone()),
        );
        metadata.insert(
            "discussion_thread_root_message_id".to_string(),
            Value::String(discussion.thread_root_message_id.clone()),
        );
    }
    if !request_metadata.is_null() {
        metadata.insert("operator_metadata".to_string(), request_metadata);
    }
    Value::Object(metadata)
}

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(crate) async fn list_projects(
        &self,
        query: Option<&str>,
        status: Option<&crate::projects::ProjectStatus>,
        member_session_id: Option<&str>,
        channel_id: Option<&str>,
    ) -> Result<Vec<crate::projects::ProjectView>> {
        if let Some(channel_id) = channel_id {
            let _ = self.channel_service.get_channel(channel_id).await?;
        }
        Ok(self
            .project_service
            .list_projects(query, status, member_session_id, channel_id)
            .await)
    }

    pub(crate) async fn get_project(
        &self,
        project_id: &str,
    ) -> Result<crate::projects::ProjectView> {
        self.project_service.get_project(project_id).await
    }

    pub(crate) async fn create_project(
        &self,
        request: crate::CreateProjectRequest,
    ) -> Result<crate::projects::ProjectView> {
        anyhow::ensure!(
            !request.display_name.trim().is_empty(),
            "display_name is required"
        );
        let now = now_ms();
        let mut members = Vec::with_capacity(request.members.len());
        for member in request.members {
            members.push(self.project_member_from_request(&member, now).await?);
        }
        let mut channel_links = Vec::with_capacity(request.channel_links.len());
        for link in request.channel_links {
            channel_links.push(self.project_channel_link_from_request(&link, now).await?);
        }
        let project_id = request
            .project_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| self.project_service.next_project_id());
        ensure_unique_project_session_members(&project_id, &members)?;
        if !channel_links.iter().any(|link| link.default_for_new_tasks)
            && let Some(first) = channel_links.first_mut()
        {
            first.default_for_new_tasks = true;
        }
        let project = self
            .project_service
            .create_project(crate::projects::ProjectView {
                summary: crate::projects::ProjectSummaryView {
                    project_id,
                    display_name: request.display_name.trim().to_string(),
                    description: trim_optional_string(request.description),
                    status: request.status.unwrap_or_default(),
                    member_count: members.len() as u64,
                    channel_count: channel_links.len() as u64,
                    task_count: 0,
                    active_task_count: 0,
                    created_at_ms: now,
                    updated_at_ms: now,
                },
                members,
                channel_links,
                metadata: request.metadata,
            })
            .await?;
        self.finalize_created_project(project).await
    }

    pub(crate) async fn update_project(
        &self,
        project_id: &str,
        request: crate::UpdateProjectRequest,
    ) -> Result<crate::projects::ProjectView> {
        if let Some(display_name) = request.display_name.as_deref() {
            anyhow::ensure!(!display_name.trim().is_empty(), "display_name is required");
        }
        let display_name = request
            .display_name
            .map(|value: String| value.trim().to_string());
        let description = request
            .description
            .and_then(|value: String| trim_optional_string(Some(value)));
        let status = request.status;
        let metadata = request.metadata;
        self.project_service
            .update_project(project_id, move |project| {
                let mut changed = false;
                if let Some(display_name) = display_name.as_deref()
                    && project.summary.display_name != display_name
                {
                    project.summary.display_name = display_name.to_string();
                    changed = true;
                }
                if let Some(description) = description.as_ref()
                    && project.summary.description.as_deref() != Some(description.as_str())
                {
                    project.summary.description = Some(description.clone());
                    changed = true;
                }
                if let Some(status) = status.as_ref()
                    && project.summary.status != *status
                {
                    project.summary.status = status.clone();
                    changed = true;
                }
                if let Some(metadata) = metadata.as_ref()
                    && project.metadata != *metadata
                {
                    project.metadata = metadata.clone();
                    changed = true;
                }
                if changed {
                    project.summary.updated_at_ms = now_ms();
                }
                Ok(changed)
            })
            .await
    }

    pub(crate) async fn delete_project(&self, project_id: &str) -> Result<()> {
        let project = self.project_service.get_project(project_id).await?;
        self.reconcile_project_channel_members(Some(&project), None)
            .await?;
        self.project_service.delete_project(project_id).await
    }

    pub(crate) async fn upsert_project_member(
        &self,
        project_id: &str,
        request: crate::ProjectMemberRequest,
    ) -> Result<crate::projects::ProjectView> {
        let previous_project = self.project_service.get_project(project_id).await?;
        let joined_at_ms = previous_project
            .members
            .iter()
            .find(|member| member.member_id == request.member_id)
            .map(|member| member.joined_at_ms)
            .unwrap_or_else(now_ms);
        let member = self
            .project_member_from_request(&request, joined_at_ms)
            .await?;
        if let Some(session_id) = member.session_id.as_deref()
            && previous_project.members.iter().any(|existing| {
                existing.member_id != member.member_id
                    && existing.session_id.as_deref() == Some(session_id)
            })
        {
            anyhow::bail!(
                "session {session_id} is already registered as another project member in project {project_id}"
            );
        }
        let project = self
            .project_service
            .update_project(project_id, move |project| {
                let mut changed = false;
                if let Some(existing) = project
                    .members
                    .iter_mut()
                    .find(|existing| existing.member_id == member.member_id)
                {
                    if *existing != member {
                        *existing = member.clone();
                        changed = true;
                    }
                } else {
                    project.members.push(member.clone());
                    changed = true;
                }
                if changed {
                    project.summary.updated_at_ms = now_ms();
                }
                Ok(changed)
            })
            .await?;
        self.finalize_updated_project(previous_project, project)
            .await
    }

    pub(crate) async fn remove_project_member(
        &self,
        project_id: &str,
        member_id: &str,
    ) -> Result<crate::projects::ProjectView> {
        let previous_project = self.project_service.get_project(project_id).await?;
        let active_tasks = self
            .project_service
            .nonterminal_task_ids_for_member(project_id, member_id)
            .await?;
        if !active_tasks.is_empty() {
            anyhow::bail!(
                "cannot remove project member {member_id}; it still owns tasks {}",
                active_tasks.join(", ")
            );
        }
        let project = self
            .project_service
            .update_project(project_id, move |project| {
                let len_before = project.members.len();
                project
                    .members
                    .retain(|member| member.member_id != member_id);
                if project.members.len() == len_before {
                    anyhow::bail!("unknown project member {member_id}");
                }
                project.summary.updated_at_ms = now_ms();
                Ok(true)
            })
            .await?;
        self.finalize_updated_project(previous_project, project)
            .await
    }

    pub(crate) async fn upsert_project_channel_link(
        &self,
        project_id: &str,
        request: crate::ProjectChannelLinkRequest,
    ) -> Result<crate::projects::ProjectView> {
        let previous_project = self.project_service.get_project(project_id).await?;
        let linked_at_ms = previous_project
            .channel_links
            .iter()
            .find(|link| link.channel_id == request.channel_id)
            .map(|link| link.linked_at_ms)
            .unwrap_or_else(now_ms);
        let link = self
            .project_channel_link_from_request(&request, linked_at_ms)
            .await?;
        let project = self
            .project_service
            .update_project(project_id, move |project| {
                let default_for_new_tasks = link.default_for_new_tasks;
                let mut changed = false;
                if default_for_new_tasks {
                    for existing in &mut project.channel_links {
                        if existing.channel_id != link.channel_id && existing.default_for_new_tasks
                        {
                            existing.default_for_new_tasks = false;
                            changed = true;
                        }
                    }
                }
                if let Some(existing) = project
                    .channel_links
                    .iter_mut()
                    .find(|existing| existing.channel_id == link.channel_id)
                {
                    if *existing != link {
                        *existing = link.clone();
                        changed = true;
                    }
                } else {
                    project.channel_links.push(link.clone());
                    changed = true;
                }
                if changed {
                    project.summary.updated_at_ms = now_ms();
                }
                Ok(changed)
            })
            .await?;
        self.finalize_updated_project(previous_project, project)
            .await
    }

    pub(crate) async fn remove_project_channel_link(
        &self,
        project_id: &str,
        channel_id: &str,
    ) -> Result<crate::projects::ProjectView> {
        let previous_project = self.project_service.get_project(project_id).await?;
        let active_tasks = self
            .project_service
            .nonterminal_task_ids_for_channel(project_id, channel_id)
            .await?;
        if !active_tasks.is_empty() {
            anyhow::bail!(
                "cannot unlink channel {channel_id}; it is still referenced by tasks {}",
                active_tasks.join(", ")
            );
        }
        let project = self
            .project_service
            .update_project(project_id, move |project| {
                let len_before = project.channel_links.len();
                project
                    .channel_links
                    .retain(|link| link.channel_id != channel_id);
                if project.channel_links.len() == len_before {
                    anyhow::bail!("unknown project channel {channel_id}");
                }
                if !project
                    .channel_links
                    .iter()
                    .any(|link| link.default_for_new_tasks)
                    && let Some(first) = project.channel_links.first_mut()
                {
                    first.default_for_new_tasks = true;
                }
                project.summary.updated_at_ms = now_ms();
                Ok(true)
            })
            .await?;
        self.reconcile_project_channel_members(Some(&previous_project), Some(&project))
            .await?;
        Ok(project)
    }

    pub(crate) async fn list_project_tasks(
        &self,
        project_id: &str,
        query: Option<&str>,
        status: Option<&TaskStatus>,
        assignee_member_id: Option<&str>,
    ) -> Result<Vec<crate::projects::ProjectTaskView>> {
        self.project_service
            .list_tasks(project_id, query, status, assignee_member_id)
            .await
    }

    pub(crate) async fn get_project_task(
        &self,
        project_id: &str,
        task_id: &str,
    ) -> Result<crate::projects::ProjectTaskView> {
        self.project_service.get_task(project_id, task_id).await
    }

    pub(crate) async fn create_project_task(
        &self,
        project_id: &str,
        request: crate::CreateProjectTaskRequest,
    ) -> Result<crate::projects::ProjectTaskView> {
        anyhow::ensure!(!request.title.trim().is_empty(), "title is required");
        let project = self.project_service.get_project(project_id).await?;
        self.ensure_project_accepts_new_work(&project)?;
        let now = now_ms();
        let project_task_id = request
            .project_task_id
            .filter(|value: &String| !value.trim().is_empty())
            .unwrap_or_else(|| self.project_service.next_task_id());
        let assignee = self
            .resolve_project_task_assignee(project_id, &project, &request.assignment)
            .await?;
        let discussion = self
            .resolve_project_task_discussion(
                &project,
                request.discussion_channel_id.as_deref(),
                request.discussion_thread_root_message_id.as_deref(),
            )
            .await?;
        let blocked_by = self
            .normalize_project_task_dependencies(project_id, &project_task_id, request.blocked_by)
            .await?;
        let assignee_member_id = assignee.as_ref().map(|member| member.member_id.clone());
        let primary_session_id = assignee
            .as_ref()
            .and_then(|member| member.session_id.clone());
        let latest_run_id = self
            .validate_project_task_latest_run(
                assignee
                    .as_ref()
                    .and_then(|member| member.session_id.as_deref()),
                request.latest_run_id,
            )
            .await?;
        let (status, output) = if let Some(latest_run_id) = latest_run_id.as_deref() {
            let run = self.get_run(latest_run_id).await?;
            anyhow::ensure!(
                run.status.is_terminal(),
                "project task latest_run_id {latest_run_id} must be terminal when creating a task"
            );
            (
                project_task_status_for_run(&run.status),
                self.canonical_project_task_output_for_run(&run).await?,
            )
        } else {
            (
                request.status.unwrap_or(TaskStatus::Pending),
                trim_optional_string(request.output),
            )
        };
        self.project_service
            .create_task(crate::projects::ProjectTaskView {
                project_task_id,
                project_id: project_id.to_string(),
                title: request.title.trim().to_string(),
                description: request.description.trim().to_string(),
                status,
                assignee_member_id,
                primary_session_id,
                latest_run_id,
                discussion,
                blocked_by,
                output,
                created_at_ms: now,
                updated_at_ms: now,
                metadata: request.metadata,
            })
            .await
    }

    pub(crate) async fn update_project_task(
        &self,
        project_id: &str,
        task_id: &str,
        request: crate::UpdateProjectTaskRequest,
    ) -> Result<crate::projects::ProjectTaskView> {
        if let Some(title) = request.title.as_deref() {
            anyhow::ensure!(!title.trim().is_empty(), "title is required");
        }
        let project = self.project_service.get_project(project_id).await?;
        let current_task = self.project_service.get_task(project_id, task_id).await?;
        let assignment_update_requested = request.clear_assignment
            || request.assignment.assignee_member_id.is_some()
            || request.assignment.assignee_session_id.is_some()
            || request.assignment.assignee_agent_id.is_some();
        let blocked_by_update_requested = request.blocked_by.is_some();
        let requested_latest_run_id = request
            .latest_run_id
            .as_ref()
            .and_then(|value| trim_optional_string(Some(value.clone())));
        let manual_output_update_requested = request.clear_output
            || request
                .output
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty());
        let latest_run_replacement_requested =
            requested_latest_run_id
                .as_deref()
                .is_some_and(|latest_run_id| {
                    current_task.latest_run_id.as_deref() != Some(latest_run_id)
                });
        anyhow::ensure!(
            !(manual_output_update_requested
                && current_task.latest_run_id.is_some()
                && !latest_run_replacement_requested),
            "project task {task_id} output is derived from latest_run_id; replace latest_run_id before changing output"
        );
        let status_update_requested = request.status.is_some();
        let protected_update_requested = assignment_update_requested
            || blocked_by_update_requested
            || latest_run_replacement_requested
            || status_update_requested
            || manual_output_update_requested;
        if protected_update_requested {
            self.ensure_project_task_has_no_active_run(&current_task)
                .await?;
        }
        let assignee = self
            .resolve_project_task_assignee(project_id, &project, &request.assignment)
            .await?;
        let discussion = if request.clear_discussion {
            Some(None)
        } else if request.discussion_channel_id.is_some()
            || request.discussion_thread_root_message_id.is_some()
        {
            Some(
                self.resolve_project_task_discussion(
                    &project,
                    request.discussion_channel_id.as_deref(),
                    request.discussion_thread_root_message_id.as_deref(),
                )
                .await?,
            )
        } else {
            None
        };
        let blocked_by = match request.blocked_by {
            Some(blocked_by) => Some(
                self.normalize_project_task_dependencies(project_id, task_id, blocked_by)
                    .await?,
            ),
            None => None,
        };
        let title = request.title.map(|value: String| value.trim().to_string());
        let description = request
            .description
            .map(|value: String| value.trim().to_string());
        let effective_session_id = if request.clear_assignment {
            None
        } else {
            assignee
                .as_ref()
                .and_then(|member| member.session_id.as_deref())
                .or(current_task.primary_session_id.as_deref())
        };
        let latest_run_id = self
            .validate_project_task_latest_run(effective_session_id, request.latest_run_id)
            .await?;
        let latest_run_projection = if let Some(latest_run_id) = latest_run_id.as_ref() {
            let run = self.get_run(latest_run_id).await?;
            anyhow::ensure!(
                run.status.is_terminal(),
                "project task latest_run_id {latest_run_id} must be terminal when updating a task"
            );
            Some((
                project_task_status_for_run(&run.status),
                self.canonical_project_task_output_for_run(&run).await?,
            ))
        } else {
            None
        };
        let status = latest_run_projection
            .as_ref()
            .map(|(status, _)| status.clone())
            .or(request.status);
        let output = if latest_run_projection.is_none() {
            request
                .output
                .and_then(|value| trim_optional_string(Some(value)))
        } else {
            None
        };
        let derived_output = latest_run_projection.map(|(_, output)| output);
        let metadata = request.metadata;
        let clear_assignment = request.clear_assignment;
        let clear_output = latest_run_id.is_none() && request.clear_output;
        let expected_latest_run_id = current_task.latest_run_id.clone();
        self.project_service
            .update_task(project_id, task_id, move |task| {
                anyhow::ensure!(
                    !protected_update_requested || task.latest_run_id == expected_latest_run_id,
                    "project task {task_id} changed while update was being prepared"
                );
                let mut changed = false;
                if let Some(title) = title.as_deref()
                    && task.title != title
                {
                    task.title = title.to_string();
                    changed = true;
                }
                if let Some(description) = description.as_deref()
                    && task.description != description
                {
                    task.description = description.to_string();
                    changed = true;
                }
                if let Some(status) = status.as_ref()
                    && task.status != *status
                {
                    task.status = status.clone();
                    changed = true;
                }
                if clear_assignment
                    && (task.assignee_member_id.is_some() || task.primary_session_id.is_some())
                {
                    task.assignee_member_id = None;
                    task.primary_session_id = None;
                    changed = true;
                }
                if let Some(assignee) = assignee.as_ref() {
                    if task.assignee_member_id.as_deref() != Some(assignee.member_id.as_str())
                        || task.primary_session_id != assignee.session_id
                    {
                        task.assignee_member_id = Some(assignee.member_id.clone());
                        task.primary_session_id = assignee.session_id.clone();
                        changed = true;
                    }
                }
                if let Some(latest_run_id) = latest_run_id.as_ref() {
                    if task.latest_run_id.as_deref() != Some(latest_run_id.as_str()) {
                        task.latest_run_id = Some(latest_run_id.clone());
                        changed = true;
                    }
                }
                if let Some(derived_output) = derived_output.as_ref() {
                    if &task.output != derived_output {
                        task.output = derived_output.clone();
                        changed = true;
                    }
                } else {
                    if clear_output && task.output.is_some() {
                        task.output = None;
                        changed = true;
                    }
                    if let Some(output) = output.as_ref()
                        && task.output.as_deref() != Some(output.as_str())
                    {
                        task.output = Some(output.clone());
                        changed = true;
                    }
                }
                if let Some(blocked_by) = blocked_by.as_ref()
                    && task.blocked_by != *blocked_by
                {
                    task.blocked_by = blocked_by.clone();
                    changed = true;
                }
                if let Some(discussion) = discussion.as_ref()
                    && task.discussion != *discussion
                {
                    task.discussion = discussion.clone();
                    changed = true;
                }
                if let Some(metadata) = metadata.as_ref()
                    && task.metadata != *metadata
                {
                    task.metadata = metadata.clone();
                    changed = true;
                }
                if changed {
                    task.updated_at_ms = now_ms();
                }
                Ok(changed)
            })
            .await
    }

    pub(crate) async fn delete_project_task(
        &self,
        project_id: &str,
        task_id: &str,
    ) -> Result<bool> {
        let dependents = self
            .project_service
            .list_tasks(project_id, None, None, None)
            .await?
            .into_iter()
            .filter(|task| {
                task.blocked_by
                    .iter()
                    .any(|dependency| dependency == task_id)
            })
            .map(|task| task.project_task_id)
            .collect::<Vec<_>>();
        if !dependents.is_empty() {
            anyhow::bail!(
                "cannot delete project task {task_id}; it still blocks tasks {}",
                dependents.join(", ")
            );
        }
        self.project_service.delete_task(project_id, task_id).await
    }

    pub(crate) async fn start_project_task(
        self: &Arc<Self>,
        project_id: &str,
        task_id: &str,
        request: crate::StartProjectTaskRequest,
    ) -> Result<crate::RunView> {
        let project = self.project_service.get_project(project_id).await?;
        self.ensure_project_accepts_new_work(&project)?;
        let task = self.project_service.get_task(project_id, task_id).await?;
        anyhow::ensure!(
            !matches!(
                task.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            ),
            "project task {task_id} is terminal and must be reopened before it can be started again"
        );
        if let Some(latest_run_id) = task.latest_run_id.as_deref() {
            let latest_run = match self.get_run(latest_run_id).await {
                Ok(latest_run) => latest_run,
                Err(_) => {
                    anyhow::bail!(
                        "project task {task_id} already has active run reservation {latest_run_id}"
                    );
                }
            };
            anyhow::ensure!(
                latest_run.status.is_terminal(),
                "project task {task_id} already has active run {latest_run_id}"
            );
        }
        self.ensure_project_task_dependencies_ready(project_id, &task)
            .await?;
        let assignee_member_id = task
            .assignee_member_id
            .as_deref()
            .ok_or_else(|| anyhow!("project task {task_id} is not assigned"))?;
        let assignee = self
            .project_service
            .member(project_id, assignee_member_id)
            .await?;
        anyhow::ensure!(
            assignee.member_kind == crate::ChannelMemberKind::Session,
            "project task assignees must be session-backed members"
        );
        let session_id = assignee.session_id.clone().ok_or_else(|| {
            anyhow!(
                "project member {} does not have a session",
                assignee.member_id
            )
        })?;
        let run_id = self.next_run_id();
        let reservation = self
            .project_service
            .reserve_task_start(
                project_id,
                task_id,
                task.latest_run_id.as_deref(),
                task.assignee_member_id.as_deref(),
                &run_id,
                &session_id,
                task.discussion.clone(),
            )
            .await
            .context("failed to reserve project-task start")?;
        let discussion = self
            .ensure_project_task_discussion(
                &project,
                &task,
                &assignee,
                request.kickoff_message.as_deref(),
            )
            .await;
        let discussion = match discussion {
            Ok(discussion) => discussion,
            Err(error) => {
                let _ = self
                    .project_service
                    .update_task(project_id, task_id, |task| {
                        if task.latest_run_id.as_deref() != Some(run_id.as_str()) {
                            return Ok(false);
                        }
                        *task = reservation.previous.clone();
                        Ok(true)
                    })
                    .await;
                return Err(error);
            }
        };
        if task.discussion != discussion
            && let Err(error) = self
                .project_service
                .update_task(project_id, task_id, |task| {
                    if task.latest_run_id.as_deref() != Some(run_id.as_str()) {
                        return Ok(false);
                    }
                    task.discussion = discussion.clone();
                    task.updated_at_ms = now_ms();
                    Ok(true)
                })
                .await
        {
            let _ = self
                .project_service
                .update_task(project_id, task_id, |task| {
                    if task.latest_run_id.as_deref() != Some(run_id.as_str()) {
                        return Ok(false);
                    }
                    *task = reservation.previous.clone();
                    Ok(true)
                })
                .await;
            return Err(error).context("failed to persist project-task discussion before start");
        }

        let provider = request.provider;
        let generation = request.model.map(|model| ModelGenerationConfig {
            model: Some(model),
            ..ModelGenerationConfig::default()
        });
        let prompt =
            render_project_task_start_prompt(&project, &task, &assignee, discussion.as_ref());
        let run = self
            .submit_input_run_with_preallocated_id(
                &session_id,
                crate::SubmitInputRequest {
                    provider,
                    source_plugin: Some("project_task".to_string()),
                    source_kind: Some("project_task".to_string()),
                    actor_id: Some(PROJECT_CONTROL_MEMBER_ID.to_string()),
                    content: prompt,
                    input_items: Vec::new(),
                    attachments: Vec::new(),
                    generation,
                    completion_requirements: None,
                    metadata: Some(build_project_task_run_metadata(
                        project_id,
                        task_id,
                        discussion.as_ref(),
                        request.metadata,
                    )),
                    binding_keys: Vec::new(),
                    reply_targets: Vec::new(),
                    reply_plugin: None,
                    reply_address: None,
                },
                run_id.clone(),
            )
            .await;
        let run = match run {
            Ok(run) => run,
            Err(error) => {
                let _ = self
                    .project_service
                    .update_task(project_id, task_id, |task| {
                        if task.latest_run_id.as_deref() != Some(run_id.as_str()) {
                            return Ok(false);
                        }
                        *task = reservation.previous.clone();
                        Ok(true)
                    })
                    .await;
                return Err(error);
            }
        };
        self.sync_project_tasks_for_run_or_warn(&run, "start_project_task")
            .await;
        Ok(run)
    }

    pub(crate) async fn sync_project_tasks_for_run(&self, run: &crate::RunView) -> Result<()> {
        let tasks = self.project_service.tasks_for_latest_run(&run.run_id).await;
        if tasks.is_empty() {
            return Ok(());
        }
        let output = self.canonical_project_task_output_for_run(run).await?;
        let desired_status = project_task_status_for_run(&run.status);
        for linked_task in tasks {
            self.project_service
                .update_task(
                    &linked_task.project_id,
                    &linked_task.project_task_id,
                    |task| {
                        if task.latest_run_id.as_deref() != Some(run.run_id.as_str()) {
                            return Ok(false);
                        }
                        let mut changed = false;
                        if task.primary_session_id.as_deref() != Some(run.session_id.as_str()) {
                            task.primary_session_id = Some(run.session_id.clone());
                            changed = true;
                        }
                        if task.status != desired_status {
                            task.status = desired_status.clone();
                            changed = true;
                        }
                        if let Some(output) = output.as_ref()
                            && task.output.as_deref() != Some(output.as_str())
                        {
                            task.output = Some(output.clone());
                            changed = true;
                        }
                        if output.is_none() && run.status.is_terminal() && task.output.is_some() {
                            task.output = None;
                            changed = true;
                        }
                        if changed {
                            task.updated_at_ms = now_ms();
                        }
                        Ok(changed)
                    },
                )
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn sync_project_tasks_for_run_or_warn(
        &self,
        run: &crate::RunView,
        context: &str,
    ) {
        if let Err(error) = self.sync_project_tasks_for_run(run).await {
            tracing::warn!(
                session_id = %run.session_id,
                run_id = %run.run_id,
                context,
                error = %error,
                "failed to sync project tasks for run"
            );
        }
    }

    pub(crate) async fn reconcile_project_tasks_from_runs_on_boot(&self) -> Result<()> {
        let tasks = self.project_service.tasks_with_latest_runs().await;
        for task in tasks {
            let Some(run_id) = task.latest_run_id.as_deref() else {
                continue;
            };
            match self.get_run(run_id).await {
                Ok(run) => self.sync_project_tasks_for_run(&run).await?,
                Err(error) => {
                    tracing::warn!(
                        project_id = %task.project_id,
                        task_id = %task.project_task_id,
                        run_id = %run_id,
                        error = %error,
                        "project task references a missing latest run during boot reconciliation"
                    );
                    self.project_service
                        .update_task(&task.project_id, &task.project_task_id, |task| {
                            if task.latest_run_id.as_deref() != Some(run_id) {
                                return Ok(false);
                            }
                            task.status = TaskStatus::Failed;
                            task.latest_run_id = None;
                            task.output = Some(format!(
                                "Project task start reservation {run_id} was orphaned before the run was created."
                            ));
                            task.updated_at_ms = now_ms();
                            Ok(true)
                        })
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn canonical_project_task_output_for_run(
        &self,
        run: &crate::RunView,
    ) -> Result<Option<String>> {
        if let Some(output) = run
            .outputs
            .iter()
            .rev()
            .find(|output| !output.content.trim().is_empty())
        {
            return Ok(Some(output.content.clone()));
        }
        match run.status {
            crate::DaemonRunStatus::Completed => {
                let started_at_ms = run.started_at_ms.unwrap_or(run.submitted_at_ms);
                let finished_at_ms = run.finished_at_ms.unwrap_or(run.updated_at_ms);
                let session = self.session_service.load_session(&run.session_id).await?;
                Ok(session.journal.iter().rev().find_map(|entry| {
                    let SessionEvent::MessageAppended { message } = &entry.event else {
                        return None;
                    };
                    if message.role != Role::Assistant || message.content.trim().is_empty() {
                        return None;
                    }
                    let timestamp_ms = message.timestamp_ms?;
                    (timestamp_ms >= started_at_ms && timestamp_ms <= finished_at_ms)
                        .then(|| message.content.clone())
                }))
            }
            crate::DaemonRunStatus::Failed
            | crate::DaemonRunStatus::Interrupted
            | crate::DaemonRunStatus::Cancelled => Ok(run
                .error
                .as_ref()
                .map(|error| error.trim().to_string())
                .filter(|error| !error.is_empty())
                .or_else(|| {
                    Some(match run.status {
                        crate::DaemonRunStatus::Failed => "Run failed".to_string(),
                        crate::DaemonRunStatus::Interrupted => "Run interrupted".to_string(),
                        crate::DaemonRunStatus::Cancelled => "Run cancelled".to_string(),
                        _ => unreachable!(),
                    })
                })),
            _ => Ok(None),
        }
    }

    pub(crate) async fn reject_channel_project_dependencies(&self, channel_id: &str) -> Result<()> {
        let referenced = self
            .project_service
            .project_ids_for_channel(channel_id)
            .await;
        if !referenced.is_empty() {
            anyhow::bail!(
                "cannot delete channel {channel_id}; it is still referenced by projects {}",
                referenced.join(", ")
            );
        }
        Ok(())
    }

    pub(crate) async fn reject_session_project_dependencies(&self, session_id: &str) -> Result<()> {
        let referenced_projects = self
            .project_service
            .project_ids_for_session(session_id)
            .await;
        if !referenced_projects.is_empty() {
            anyhow::bail!(
                "cannot end session {session_id}; it is still a project member in {}",
                referenced_projects.join(", ")
            );
        }
        let referenced_tasks = self
            .project_service
            .nonterminal_task_ids_for_session(session_id)
            .await;
        if !referenced_tasks.is_empty() {
            anyhow::bail!(
                "cannot end session {session_id}; it is still assigned to project tasks {}",
                referenced_tasks.join(", ")
            );
        }
        Ok(())
    }

    async fn project_member_from_request(
        &self,
        request: &crate::ProjectMemberRequest,
        joined_at_ms: u64,
    ) -> Result<crate::projects::ProjectMemberView> {
        anyhow::ensure!(
            !request.member_id.trim().is_empty(),
            "member_id is required"
        );
        anyhow::ensure!(
            !request.display_name.trim().is_empty(),
            "display_name is required"
        );
        let resolved_session_id = match request.member_kind {
            crate::ChannelMemberKind::HumanActor => None,
            crate::ChannelMemberKind::Session => Some(
                self.resolve_project_member_session_id(
                    request.member_id.trim(),
                    request.session_id.as_deref(),
                    request.agent_id.as_deref(),
                )
                .await?,
            ),
        };
        let resolved_actor_id = match request.member_kind {
            crate::ChannelMemberKind::HumanActor => Some(
                request
                    .actor_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| request.member_id.trim())
                    .to_string(),
            ),
            crate::ChannelMemberKind::Session => None,
        };
        Ok(crate::projects::ProjectMemberView {
            member_id: request.member_id.trim().to_string(),
            member_kind: request.member_kind.clone(),
            display_name: request.display_name.trim().to_string(),
            session_id: resolved_session_id,
            actor_id: resolved_actor_id,
            role: trim_optional_string(request.role.clone()),
            expertise_tags: request
                .expertise_tags
                .iter()
                .map(|tag: &String| tag.trim().to_string())
                .filter(|tag: &String| !tag.is_empty())
                .collect(),
            joined_at_ms,
            metadata: request.metadata.clone(),
        })
    }

    async fn project_channel_link_from_request(
        &self,
        request: &crate::ProjectChannelLinkRequest,
        linked_at_ms: u64,
    ) -> Result<crate::projects::ProjectChannelLinkView> {
        anyhow::ensure!(
            !request.channel_id.trim().is_empty(),
            "channel_id is required"
        );
        self.channel_service
            .get_channel(&request.channel_id)
            .await?;
        Ok(crate::projects::ProjectChannelLinkView {
            channel_id: request.channel_id.trim().to_string(),
            role: trim_optional_string(request.role.clone()),
            default_for_new_tasks: request.default_for_new_tasks.unwrap_or(false),
            mirror_members: request.mirror_members.unwrap_or(false),
            linked_at_ms,
            metadata: request.metadata.clone(),
        })
    }

    async fn resolve_project_member_session_id(
        &self,
        member_id: &str,
        session_id: Option<&str>,
        agent_id: Option<&str>,
    ) -> Result<String> {
        match (
            session_id
                .map(str::trim)
                .filter(|value: &&str| !value.is_empty()),
            agent_id.map(str::trim).filter(|value| !value.is_empty()),
        ) {
            (Some(session_id), None) => {
                self.agent_id_for_session(session_id).await?;
                Ok(session_id.to_string())
            }
            (None, Some(agent_id)) => {
                let record = self
                    .supervisor
                    .get(&AgentId(agent_id.to_string()))
                    .ok_or_else(|| anyhow!("unknown agent {agent_id}"))?;
                Ok(record.conversation.session_id)
            }
            (None, None) => {
                self.agent_id_for_session(member_id).await?;
                Ok(member_id.to_string())
            }
            (Some(_), Some(_)) => anyhow::bail!("session_id and agent_id are mutually exclusive"),
        }
    }

    async fn resolve_project_task_assignee(
        &self,
        project_id: &str,
        project: &crate::projects::ProjectView,
        request: &crate::ProjectTaskAssignmentRequest,
    ) -> Result<Option<crate::projects::ProjectMemberView>> {
        match (
            request
                .assignee_member_id
                .as_deref()
                .map(str::trim)
                .filter(|value: &&str| !value.is_empty()),
            request
                .assignee_session_id
                .as_deref()
                .map(str::trim)
                .filter(|value: &&str| !value.is_empty()),
            request
                .assignee_agent_id
                .as_deref()
                .map(str::trim)
                .filter(|value: &&str| !value.is_empty()),
        ) {
            (None, None, None) => Ok(None),
            (Some(member_id), None, None) => self
                .project_service
                .member(project_id, member_id)
                .await
                .map(Some),
            (None, Some(session_id), None) => self
                .project_service
                .member_for_session(project_id, session_id)
                .await
                .ok_or_else(|| anyhow!("session {session_id} is not a project member"))
                .map(Some),
            (None, None, Some(agent_id)) => {
                let record = self
                    .supervisor
                    .get(&AgentId(agent_id.to_string()))
                    .ok_or_else(|| anyhow!("unknown agent {agent_id}"))?;
                self.project_service
                    .member_for_session(project_id, &record.conversation.session_id)
                    .await
                    .ok_or_else(|| {
                        anyhow!(
                            "agent {agent_id} belongs to session {}, which is not a project member",
                            record.conversation.session_id
                        )
                    })
                    .map(Some)
            }
            _ => anyhow::bail!(
                "only one of assignee_member_id, assignee_session_id, or assignee_agent_id may be set"
            ),
        }
        .and_then(|member| {
            if let Some(member) = member.as_ref()
                && member.member_kind != crate::ChannelMemberKind::Session
            {
                anyhow::bail!("task assignees must be session-backed project members");
            }
            if let Some(member) = member.as_ref()
                && !project.members.iter().any(|existing| existing.member_id == member.member_id)
            {
                anyhow::bail!("unknown project member {}", member.member_id);
            }
            Ok(member)
        })
    }

    async fn resolve_project_task_discussion(
        &self,
        project: &crate::projects::ProjectView,
        channel_id: Option<&str>,
        thread_root_message_id: Option<&str>,
    ) -> Result<Option<crate::projects::ProjectTaskDiscussionRef>> {
        match (
            channel_id.map(str::trim).filter(|value| !value.is_empty()),
            thread_root_message_id
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ) {
            (None, None) => Ok(None),
            (Some(channel_id), Some(thread_root_message_id)) => {
                anyhow::ensure!(
                    project
                        .channel_links
                        .iter()
                        .any(|link| link.channel_id == channel_id),
                    "channel {channel_id} is not linked to project {}",
                    project.summary.project_id
                );
                let _ = self
                    .channel_service
                    .get_message(channel_id, thread_root_message_id)
                    .await?;
                Ok(Some(crate::projects::ProjectTaskDiscussionRef {
                    channel_id: channel_id.to_string(),
                    thread_root_message_id: thread_root_message_id.to_string(),
                }))
            }
            _ => anyhow::bail!(
                "discussion_channel_id and discussion_thread_root_message_id must be set together"
            ),
        }
    }

    async fn normalize_project_task_dependencies(
        &self,
        project_id: &str,
        task_id: &str,
        blocked_by: Vec<String>,
    ) -> Result<Vec<String>> {
        let mut normalized = Vec::new();
        for dependency in blocked_by {
            let dependency = dependency.trim();
            if dependency.is_empty() {
                continue;
            }
            anyhow::ensure!(
                dependency != task_id,
                "project task {task_id} cannot depend on itself"
            );
            let task = self
                .project_service
                .get_task(project_id, dependency)
                .await?;
            anyhow::ensure!(
                task.project_task_id == dependency,
                "unknown project task dependency {dependency}"
            );
            if !normalized.iter().any(|existing| existing == dependency) {
                normalized.push(dependency.to_string());
            }
        }
        self.ensure_project_task_dependencies_acyclic(project_id, task_id, &normalized)
            .await?;
        Ok(normalized)
    }

    async fn ensure_project_task_dependencies_ready(
        &self,
        project_id: &str,
        task: &crate::projects::ProjectTaskView,
    ) -> Result<()> {
        let mut pending = Vec::new();
        for dependency in &task.blocked_by {
            let dependency_task = self
                .project_service
                .get_task(project_id, dependency)
                .await?;
            if dependency_task.status != TaskStatus::Completed {
                pending.push(format!(
                    "{} ({})",
                    dependency,
                    task_status_label(&dependency_task.status)
                ));
            }
        }
        anyhow::ensure!(
            pending.is_empty(),
            "project task {} is still blocked by {}",
            task.project_task_id,
            pending.join(", ")
        );
        Ok(())
    }

    async fn ensure_project_task_dependencies_acyclic(
        &self,
        project_id: &str,
        task_id: &str,
        blocked_by: &[String],
    ) -> Result<()> {
        fn reaches_target(
            current: &str,
            target: &str,
            graph: &BTreeMap<String, Vec<String>>,
            visiting: &mut Vec<String>,
        ) -> bool {
            if current == target {
                return true;
            }
            if visiting.iter().any(|entry| entry == current) {
                return false;
            }
            visiting.push(current.to_string());
            let found = graph.get(current).is_some_and(|dependencies| {
                dependencies
                    .iter()
                    .any(|dependency| reaches_target(dependency, target, graph, visiting))
            });
            visiting.pop();
            found
        }

        let mut graph = self
            .project_service
            .list_tasks(project_id, None, None, None)
            .await?
            .into_iter()
            .map(|task| (task.project_task_id, task.blocked_by))
            .collect::<BTreeMap<_, _>>();
        graph.insert(task_id.to_string(), blocked_by.to_vec());
        anyhow::ensure!(
            !blocked_by.iter().any(|dependency| reaches_target(
                dependency,
                task_id,
                &graph,
                &mut Vec::new()
            )),
            "project task {task_id} introduces a dependency cycle"
        );
        Ok(())
    }

    fn ensure_project_accepts_new_work(
        &self,
        project: &crate::projects::ProjectView,
    ) -> Result<()> {
        anyhow::ensure!(
            matches!(project.summary.status, crate::ProjectStatus::Active),
            "project {} is {} and does not accept new work",
            project.summary.project_id,
            match project.summary.status {
                crate::ProjectStatus::Active => "active",
                crate::ProjectStatus::Paused => "paused",
                crate::ProjectStatus::Completed => "completed",
                crate::ProjectStatus::Archived => "archived",
            }
        );
        Ok(())
    }

    async fn reconcile_project_channel_members(
        &self,
        previous_project: Option<&crate::projects::ProjectView>,
        current_project: Option<&crate::projects::ProjectView>,
    ) -> Result<()> {
        let Some(project_id) = current_project
            .map(|project| project.summary.project_id.as_str())
            .or_else(|| previous_project.map(|project| project.summary.project_id.as_str()))
        else {
            return Ok(());
        };
        let mut affected_channel_ids = Vec::new();
        for channel_id in previous_project
            .into_iter()
            .flat_map(|project| project.channel_links.iter())
            .filter(|link| link.mirror_members)
            .map(|link| link.channel_id.as_str())
            .chain(
                current_project
                    .into_iter()
                    .flat_map(|project| project.channel_links.iter())
                    .filter(|link| link.mirror_members)
                    .map(|link| link.channel_id.as_str()),
            )
        {
            if !affected_channel_ids
                .iter()
                .any(|existing| existing == channel_id)
            {
                affected_channel_ids.push(channel_id.to_string());
            }
        }
        for channel_id in affected_channel_ids {
            let should_mirror = current_project.is_some_and(|project| {
                project
                    .channel_links
                    .iter()
                    .any(|link| link.channel_id == channel_id && link.mirror_members)
            });
            if should_mirror {
                if let Some(project) = current_project {
                    self.sync_project_members_to_channel(project, &channel_id)
                        .await?;
                }
            } else {
                self.remove_project_members_from_channel(project_id, &channel_id)
                    .await?;
            }
        }
        Ok(())
    }

    async fn finalize_created_project(
        &self,
        project: crate::projects::ProjectView,
    ) -> Result<crate::projects::ProjectView> {
        if let Err(error) = self
            .reconcile_project_channel_members(None, Some(&project))
            .await
        {
            let rollback_channel_error = self
                .reconcile_project_channel_members(Some(&project), None)
                .await
                .err();
            let rollback_project_error = self
                .project_service
                .delete_project(&project.summary.project_id)
                .await
                .err();
            let mut details = Vec::new();
            if let Some(rollback_channel_error) = rollback_channel_error {
                details.push(format!("channel rollback failed: {rollback_channel_error}"));
            }
            if let Some(rollback_project_error) = rollback_project_error {
                details.push(format!("project rollback failed: {rollback_project_error}"));
            }
            let suffix = if details.is_empty() {
                String::new()
            } else {
                format!("; {}", details.join("; "))
            };
            anyhow::bail!(
                "failed to synchronize mirrored project members after creating project {}: {error}{suffix}",
                project.summary.project_id
            );
        }
        Ok(project)
    }

    async fn finalize_updated_project(
        &self,
        previous_project: crate::projects::ProjectView,
        project: crate::projects::ProjectView,
    ) -> Result<crate::projects::ProjectView> {
        if let Err(error) = self
            .reconcile_project_channel_members(Some(&previous_project), Some(&project))
            .await
        {
            let rollback_project = previous_project.clone();
            let rollback_project_id = rollback_project.summary.project_id.clone();
            let rollback_project_error = self
                .project_service
                .update_project(&rollback_project_id, move |current| {
                    *current = rollback_project.clone();
                    Ok(true)
                })
                .await
                .err();
            let rollback_channel_error = self
                .reconcile_project_channel_members(Some(&project), Some(&previous_project))
                .await
                .err();
            let mut details = Vec::new();
            if let Some(rollback_project_error) = rollback_project_error {
                details.push(format!("project rollback failed: {rollback_project_error}"));
            }
            if let Some(rollback_channel_error) = rollback_channel_error {
                details.push(format!("channel rollback failed: {rollback_channel_error}"));
            }
            let suffix = if details.is_empty() {
                String::new()
            } else {
                format!("; {}", details.join("; "))
            };
            anyhow::bail!(
                "failed to synchronize mirrored project members after updating project {}: {error}{suffix}",
                project.summary.project_id
            );
        }
        Ok(project)
    }

    async fn sync_project_members_to_channel(
        &self,
        project: &crate::projects::ProjectView,
        channel_id: &str,
    ) -> Result<()> {
        let channel = self.channel_service.get_channel(channel_id).await?;
        let desired_member_ids = project
            .members
            .iter()
            .map(|member| {
                mirrored_channel_member_id(&project.summary.project_id, &member.member_id)
            })
            .collect::<Vec<_>>();
        let prefix = mirrored_channel_member_prefix(&project.summary.project_id);
        for member in channel.members.iter().filter(|member| {
            member.member_id.starts_with(&prefix)
                && !desired_member_ids
                    .iter()
                    .any(|desired| desired == &member.member_id)
        }) {
            self.remove_channel_member(channel_id, &member.member_id)
                .await?;
        }
        for member in &project.members {
            self.upsert_channel_member(
                channel_id,
                crate::ChannelMemberRequest {
                    member_id: mirrored_channel_member_id(
                        &project.summary.project_id,
                        &member.member_id,
                    ),
                    member_kind: member.member_kind.clone(),
                    display_name_mode: Some(crate::ChannelMemberDisplayNameMode::Manual),
                    display_name: member.display_name.clone(),
                    session_id: member.session_id.clone(),
                    actor_id: member.actor_id.clone(),
                    role: member.role.clone(),
                    expertise_tags: member.expertise_tags.clone(),
                    participation_mode: Some(crate::ChannelParticipationMode::ManualOnly),
                    muted: None,
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn remove_project_members_from_channel(
        &self,
        project_id: &str,
        channel_id: &str,
    ) -> Result<()> {
        let channel = self.channel_service.get_channel(channel_id).await?;
        let prefix = mirrored_channel_member_prefix(project_id);
        for member in channel
            .members
            .iter()
            .filter(|member| member.member_id.starts_with(&prefix))
        {
            self.remove_channel_member(channel_id, &member.member_id)
                .await?;
        }
        Ok(())
    }

    async fn ensure_project_task_discussion(
        self: &Arc<Self>,
        project: &crate::projects::ProjectView,
        task: &crate::projects::ProjectTaskView,
        assignee: &crate::projects::ProjectMemberView,
        kickoff_message: Option<&str>,
    ) -> Result<Option<crate::projects::ProjectTaskDiscussionRef>> {
        if let Some(existing) = task.discussion.clone() {
            let discussion_still_valid = project
                .channel_links
                .iter()
                .any(|link| link.channel_id == existing.channel_id)
                && self
                    .channel_service
                    .get_message(&existing.channel_id, &existing.thread_root_message_id)
                    .await
                    .is_ok();
            if discussion_still_valid {
                return Ok(Some(existing));
            }
        }
        let Some(default_channel) = project
            .channel_links
            .iter()
            .find(|link| link.default_for_new_tasks)
        else {
            return Ok(None);
        };
        let addressed_member_ids = if default_channel.mirror_members {
            self.sync_project_members_to_channel(project, &default_channel.channel_id)
                .await?;
            vec![mirrored_channel_member_id(
                &project.summary.project_id,
                &assignee.member_id,
            )]
        } else {
            Vec::new()
        };
        self.ensure_project_control_channel_member(&default_channel.channel_id)
            .await?;
        let content = kickoff_message
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| default_project_task_kickoff_message(project, task, assignee));
        let message = self
            .post_channel_message(
                &default_channel.channel_id,
                crate::PostChannelMessageRequest {
                    sender_actor_id: PROJECT_CONTROL_MEMBER_ID.to_string(),
                    sender_display_name: Some(PROJECT_CONTROL_DISPLAY_NAME.to_string()),
                    sender_session_id: None,
                    thread_root_message_id: None,
                    reply_to_message_id: None,
                    addressed_member_ids,
                    input_items: Vec::new(),
                    content: Some(content),
                    metadata: serde_json::json!({
                        "project_id": project.summary.project_id,
                        "project_task_id": task.project_task_id,
                        "source_plugin": "project_task"
                    }),
                },
            )
            .await?;
        Ok(Some(crate::projects::ProjectTaskDiscussionRef {
            channel_id: default_channel.channel_id.clone(),
            thread_root_message_id: message.message_id,
        }))
    }

    async fn ensure_project_control_channel_member(&self, channel_id: &str) -> Result<()> {
        self.upsert_channel_member(
            channel_id,
            crate::ChannelMemberRequest {
                member_id: PROJECT_CONTROL_MEMBER_ID.to_string(),
                member_kind: crate::ChannelMemberKind::HumanActor,
                display_name_mode: Some(crate::ChannelMemberDisplayNameMode::Manual),
                display_name: PROJECT_CONTROL_DISPLAY_NAME.to_string(),
                session_id: None,
                actor_id: Some(PROJECT_CONTROL_MEMBER_ID.to_string()),
                role: Some("project_control".to_string()),
                expertise_tags: vec!["project".to_string(), "coordination".to_string()],
                participation_mode: Some(crate::ChannelParticipationMode::ManualOnly),
                muted: Some(true),
            },
        )
        .await?;
        Ok(())
    }

    async fn validate_project_task_latest_run(
        &self,
        session_id: Option<&str>,
        latest_run_id: Option<String>,
    ) -> Result<Option<String>> {
        let Some(latest_run_id) = trim_optional_string(latest_run_id) else {
            return Ok(None);
        };
        let expected_session_id = session_id.ok_or_else(|| {
            anyhow!("latest_run_id requires a session-backed project-task assignee")
        })?;
        let run = self.get_run(&latest_run_id).await?;
        anyhow::ensure!(
            run.session_id == expected_session_id,
            "run {latest_run_id} belongs to session {}, not assignee session {expected_session_id}",
            run.session_id
        );
        Ok(Some(latest_run_id))
    }

    async fn ensure_project_task_has_no_active_run(
        &self,
        task: &crate::projects::ProjectTaskView,
    ) -> Result<()> {
        let Some(latest_run_id) = task.latest_run_id.as_deref() else {
            return Ok(());
        };
        match self.get_run(latest_run_id).await {
            Ok(run) if run.status.is_terminal() => Ok(()),
            Ok(_) => {
                anyhow::bail!(
                    "project task {} already has active run {latest_run_id}; finish or cancel it before changing assignment, dependencies, status, or latest_run_id",
                    task.project_task_id
                );
            }
            Err(_)
                if matches!(
                    task.status,
                    TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
                ) =>
            {
                Ok(())
            }
            Err(_) => {
                anyhow::bail!(
                    "project task {} already has active run reservation {latest_run_id}; finish or cancel it before changing assignment, dependencies, status, or latest_run_id",
                    task.project_task_id
                );
            }
        }
    }
}
