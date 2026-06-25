//! Task command handlers.

use anyhow::Result;

/// Handles `tasks ...`.
pub(crate) async fn run_tasks_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::TasksCommand,
) -> Result<()> {
    match command {
        crate::TasksCommand::List {
            session_id,
            status,
            pagination,
        } => {
            let session_id = crate::cli::url_encode_path_segment(&session_id);
            let mut path = format!("/v1/sessions/{session_id}/tasks");
            let mut params = Vec::new();
            if let Some(status) = status {
                params.push(format!("status={}", status.as_str()));
            }
            pagination.append_query_params(&mut params);
            if !params.is_empty() {
                path.push('?');
                path.push_str(&params.join("&"));
            }
            if pagination.wants_page() {
                let tasks = client
                    .get_list_page_compat::<kheish_types::TaskRecord>(&path)
                    .await?;
                printer.print(&tasks)
            } else {
                let tasks = client
                    .get_json::<Vec<kheish_types::TaskRecord>>(&path)
                    .await?;
                printer.print(&tasks)
            }
        }
        crate::TasksCommand::Get {
            session_id,
            task_id,
        } => {
            let session_id = crate::cli::url_encode_path_segment(&session_id);
            let task_id = crate::cli::url_encode_path_segment(&task_id);
            let task = client
                .get_json::<kheish_types::TaskRecord>(&format!(
                    "/v1/sessions/{session_id}/tasks/{task_id}"
                ))
                .await?;
            printer.print(&task)
        }
        crate::TasksCommand::Output(args) => {
            let session_id = crate::cli::url_encode_path_segment(&args.session_id);
            let task_id = crate::cli::url_encode_path_segment(&args.task_id);
            let mut path = format!(
                "/v1/sessions/{}/tasks/{}/output?wait={}&timeout_ms={}",
                session_id, task_id, args.wait, args.timeout_ms
            );
            if let Some(tail_bytes) = args.tail_bytes {
                path.push_str(&format!("&tail_bytes={tail_bytes}"));
            }
            if args.full {
                path.push_str("&full=true");
            }
            let output = client
                .get_json::<kheish_daemon::TaskOutputView>(&path)
                .await?;
            printer.print(&output)
        }
        crate::TasksCommand::Stop(args) => {
            let session_id = crate::cli::url_encode_path_segment(&args.session_id);
            let task_id = crate::cli::url_encode_path_segment(&args.task_id);
            let task = client
                .post_json::<_, kheish_types::TaskRecord>(
                    &format!("/v1/sessions/{session_id}/tasks/{task_id}/stop"),
                    &kheish_daemon::StopTaskRequest {
                        reason: args.reason,
                    },
                )
                .await?;
            printer.print(&task)
        }
    }
}
