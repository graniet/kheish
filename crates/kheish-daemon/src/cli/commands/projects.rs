//! Project command handlers.

use anyhow::Result;
use serde_json::Value;

/// Handles `projects ...`.
pub(crate) async fn run_projects_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::ProjectsCommand,
) -> Result<()> {
    match command {
        crate::ProjectsCommand::List {
            query,
            member_session_id,
            channel_id,
            status,
        } => {
            let projects = client
                .get_json_with_query::<_, Vec<kheish_daemon::ProjectView>>(
                    "/v1/projects",
                    &kheish_daemon::ProjectListQuery {
                        query,
                        member_session_id,
                        channel_id,
                        status: status.map(Into::into),
                    },
                )
                .await?;
            printer.print(&projects)
        }
        crate::ProjectsCommand::Get { project_id } => {
            let project_id = crate::cli::url_encode_path_segment(&project_id);
            let project = client
                .get_json::<kheish_daemon::ProjectView>(&format!("/v1/projects/{project_id}"))
                .await?;
            printer.print(&project)
        }
        crate::ProjectsCommand::Create(args) => {
            let members = crate::cli::read_optional_typed_json_input::<
                Vec<kheish_daemon::ProjectMemberRequest>,
            >(args.members_json.as_deref(), args.members_file.as_deref())
            .await?
            .unwrap_or_default();
            let channel_links = crate::cli::read_optional_typed_json_input::<
                Vec<kheish_daemon::ProjectChannelLinkRequest>,
            >(
                args.channel_links_json.as_deref(),
                args.channel_links_file.as_deref(),
            )
            .await?
            .unwrap_or_default();
            let metadata = crate::cli::read_optional_json_input(
                args.metadata_json.as_deref(),
                args.metadata_file.as_deref(),
            )
            .await?
            .unwrap_or(Value::Null);
            let project = client
                .post_json::<_, kheish_daemon::ProjectView>(
                    "/v1/projects",
                    &kheish_daemon::CreateProjectRequest {
                        project_id: args.project_id,
                        display_name: args.display_name,
                        description: args.description,
                        status: args.status.map(Into::into),
                        members,
                        channel_links,
                        metadata,
                    },
                )
                .await?;
            printer.print(&project)
        }
        crate::ProjectsCommand::Update(args) => {
            let metadata = crate::cli::read_optional_json_input(
                args.metadata_json.as_deref(),
                args.metadata_file.as_deref(),
            )
            .await?;
            let project_id = crate::cli::url_encode_path_segment(&args.project_id);
            let project = client
                .put_json::<_, kheish_daemon::ProjectView>(
                    &format!("/v1/projects/{project_id}"),
                    &kheish_daemon::UpdateProjectRequest {
                        display_name: args.display_name,
                        description: args.description,
                        status: args.status.map(Into::into),
                        metadata,
                    },
                )
                .await?;
            printer.print(&project)
        }
        crate::ProjectsCommand::Delete { project_id } => {
            let project_id = crate::cli::url_encode_path_segment(&project_id);
            let response = client
                .delete_json::<Value>(&format!("/v1/projects/{project_id}"))
                .await?;
            printer.print(&response)
        }
        crate::ProjectsCommand::Members { command } => match command {
            crate::ProjectMembersCommand::List { project_id } => {
                let project_id = crate::cli::url_encode_path_segment(&project_id);
                let members = client
                    .get_json::<Vec<kheish_daemon::ProjectMemberView>>(&format!(
                        "/v1/projects/{project_id}/members"
                    ))
                    .await?;
                printer.print(&members)
            }
            crate::ProjectMembersCommand::Upsert(args) => {
                let metadata = crate::cli::read_optional_json_input(
                    args.metadata_json.as_deref(),
                    args.metadata_file.as_deref(),
                )
                .await?
                .unwrap_or(Value::Null);
                let project_id = crate::cli::url_encode_path_segment(&args.project_id);
                let project = client
                    .post_json::<_, kheish_daemon::ProjectView>(
                        &format!("/v1/projects/{project_id}/members"),
                        &kheish_daemon::ProjectMemberRequest {
                            member_id: args.member_id,
                            member_kind: args.member_kind.into(),
                            display_name: args.display_name,
                            session_id: args.session_id,
                            agent_id: args.agent_id,
                            actor_id: args.actor_id,
                            role: args.role,
                            expertise_tags: args.expertise_tags,
                            metadata,
                        },
                    )
                    .await?;
                printer.print(&project)
            }
            crate::ProjectMembersCommand::Remove {
                project_id,
                member_id,
            } => {
                let project_id = crate::cli::url_encode_path_segment(&project_id);
                let member_id = crate::cli::url_encode_path_segment(&member_id);
                let project = client
                    .delete_json::<kheish_daemon::ProjectView>(&format!(
                        "/v1/projects/{project_id}/members/{member_id}"
                    ))
                    .await?;
                printer.print(&project)
            }
        },
        crate::ProjectsCommand::Channels { command } => match command {
            crate::ProjectChannelsCommand::List { project_id } => {
                let project_id = crate::cli::url_encode_path_segment(&project_id);
                let channels = client
                    .get_json::<Vec<kheish_daemon::ProjectChannelLinkView>>(&format!(
                        "/v1/projects/{project_id}/channels"
                    ))
                    .await?;
                printer.print(&channels)
            }
            crate::ProjectChannelsCommand::Link(args) => {
                let metadata = crate::cli::read_optional_json_input(
                    args.metadata_json.as_deref(),
                    args.metadata_file.as_deref(),
                )
                .await?
                .unwrap_or(Value::Null);
                let project_id = crate::cli::url_encode_path_segment(&args.project_id);
                let project = client
                    .post_json::<_, kheish_daemon::ProjectView>(
                        &format!("/v1/projects/{project_id}/channels"),
                        &kheish_daemon::ProjectChannelLinkRequest {
                            channel_id: args.channel_id,
                            role: args.role,
                            default_for_new_tasks: args.default_for_new_tasks,
                            mirror_members: args.mirror_members,
                            metadata,
                        },
                    )
                    .await?;
                printer.print(&project)
            }
            crate::ProjectChannelsCommand::Unlink {
                project_id,
                channel_id,
            } => {
                let project_id = crate::cli::url_encode_path_segment(&project_id);
                let channel_id = crate::cli::url_encode_path_segment(&channel_id);
                let project = client
                    .delete_json::<kheish_daemon::ProjectView>(&format!(
                        "/v1/projects/{project_id}/channels/{channel_id}"
                    ))
                    .await?;
                printer.print(&project)
            }
        },
        crate::ProjectsCommand::Tasks { command } => match command {
            crate::ProjectTasksCommand::List {
                project_id,
                query,
                status,
                assignee_member_id,
            } => {
                let project_id = crate::cli::url_encode_path_segment(&project_id);
                let tasks = client
                    .get_json_with_query::<_, Vec<kheish_daemon::ProjectTaskView>>(
                        &format!("/v1/projects/{project_id}/tasks"),
                        &kheish_daemon::ProjectTaskListQuery {
                            query,
                            status: status.map(crate::TaskStatusArg::into_task_status),
                            assignee_member_id,
                        },
                    )
                    .await?;
                printer.print(&tasks)
            }
            crate::ProjectTasksCommand::Get {
                project_id,
                task_id,
            } => {
                let project_id = crate::cli::url_encode_path_segment(&project_id);
                let task_id = crate::cli::url_encode_path_segment(&task_id);
                let task = client
                    .get_json::<kheish_daemon::ProjectTaskView>(&format!(
                        "/v1/projects/{project_id}/tasks/{task_id}"
                    ))
                    .await?;
                printer.print(&task)
            }
            crate::ProjectTasksCommand::Create(args) => {
                let metadata = crate::cli::read_optional_json_input(
                    args.metadata_json.as_deref(),
                    args.metadata_file.as_deref(),
                )
                .await?
                .unwrap_or(Value::Null);
                let project_id = crate::cli::url_encode_path_segment(&args.project_id);
                let task = client
                    .post_json::<_, kheish_daemon::ProjectTaskView>(
                        &format!("/v1/projects/{project_id}/tasks"),
                        &kheish_daemon::CreateProjectTaskRequest {
                            project_task_id: args.project_task_id,
                            title: args.title,
                            description: args.description,
                            status: args.status.map(crate::TaskStatusArg::into_task_status),
                            assignment: kheish_daemon::ProjectTaskAssignmentRequest {
                                assignee_member_id: args.assignee_member_id,
                                assignee_session_id: args.assignee_session_id,
                                assignee_agent_id: args.assignee_agent_id,
                            },
                            discussion_channel_id: args.discussion_channel_id,
                            discussion_thread_root_message_id: args
                                .discussion_thread_root_message_id,
                            blocked_by: args.blocked_by,
                            latest_run_id: args.latest_run_id,
                            output: args.task_output,
                            metadata,
                        },
                    )
                    .await?;
                printer.print(&task)
            }
            crate::ProjectTasksCommand::Update(args) => {
                let metadata = crate::cli::read_optional_json_input(
                    args.metadata_json.as_deref(),
                    args.metadata_file.as_deref(),
                )
                .await?;
                let project_id = crate::cli::url_encode_path_segment(&args.project_id);
                let task_id = crate::cli::url_encode_path_segment(&args.task_id);
                let task = client
                    .put_json::<_, kheish_daemon::ProjectTaskView>(
                        &format!("/v1/projects/{project_id}/tasks/{task_id}"),
                        &kheish_daemon::UpdateProjectTaskRequest {
                            title: args.title,
                            description: args.description,
                            status: args.status.map(crate::TaskStatusArg::into_task_status),
                            assignment: kheish_daemon::ProjectTaskAssignmentRequest {
                                assignee_member_id: args.assignee_member_id,
                                assignee_session_id: args.assignee_session_id,
                                assignee_agent_id: args.assignee_agent_id,
                            },
                            clear_assignment: args.clear_assignment,
                            discussion_channel_id: args.discussion_channel_id,
                            discussion_thread_root_message_id: args
                                .discussion_thread_root_message_id,
                            clear_discussion: args.clear_discussion,
                            blocked_by: args.replace_blocked_by.then_some(args.blocked_by),
                            latest_run_id: args.latest_run_id,
                            output: args.task_output,
                            clear_output: args.clear_output,
                            metadata,
                        },
                    )
                    .await?;
                printer.print(&task)
            }
            crate::ProjectTasksCommand::Start(args) => {
                let metadata = crate::cli::read_optional_json_input(
                    args.metadata_json.as_deref(),
                    args.metadata_file.as_deref(),
                )
                .await?
                .unwrap_or(Value::Null);
                let project_id = crate::cli::url_encode_path_segment(&args.project_id);
                let task_id = crate::cli::url_encode_path_segment(&args.task_id);
                let run = client
                    .post_json::<_, kheish_daemon::RunView>(
                        &format!("/v1/projects/{project_id}/tasks/{task_id}/start"),
                        &kheish_daemon::StartProjectTaskRequest {
                            provider: args.provider,
                            model: args.model,
                            kickoff_message: args.kickoff_message,
                            metadata,
                        },
                    )
                    .await?;
                let run = if args.wait {
                    crate::cli::wait_for_run(client, &run.run_id, args.poll_interval_ms).await?
                } else {
                    run
                };
                printer.print(&run)
            }
            crate::ProjectTasksCommand::Delete {
                project_id,
                task_id,
            } => {
                let project_id = crate::cli::url_encode_path_segment(&project_id);
                let task_id = crate::cli::url_encode_path_segment(&task_id);
                let response = client
                    .delete_json::<Value>(&format!("/v1/projects/{project_id}/tasks/{task_id}"))
                    .await?;
                printer.print(&response)
            }
        },
    }
}
