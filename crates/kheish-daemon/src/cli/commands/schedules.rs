//! Schedule command handlers.

use anyhow::Result;

/// Handles `schedules ...`.
pub(crate) async fn run_schedules_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::SchedulesCommand,
) -> Result<()> {
    match command {
        crate::SchedulesCommand::List {
            session_id,
            pagination,
        } => {
            let mut path = "/v1/schedules".to_string();
            let mut params = Vec::new();
            if let Some(session_id) = session_id.filter(|value| !value.trim().is_empty()) {
                params.push(format!(
                    "session_id={}",
                    crate::cli::url_encode_component(&session_id)
                ));
            }
            pagination.append_query_params(&mut params);
            if !params.is_empty() {
                path.push('?');
                path.push_str(&params.join("&"));
            }
            if pagination.wants_page() {
                let schedules = client
                    .get_list_page_compat::<kheish_daemon::ScheduleView>(&path)
                    .await?;
                printer.print(&schedules)
            } else {
                let schedules = client
                    .get_json::<Vec<kheish_daemon::ScheduleView>>(&path)
                    .await?;
                printer.print(&schedules)
            }
        }
        crate::SchedulesCommand::Get { schedule_id } => {
            let schedule_id = crate::cli::url_encode_path_segment(&schedule_id);
            let schedule = client
                .get_json::<kheish_daemon::ScheduleView>(&format!("/v1/schedules/{schedule_id}"))
                .await?;
            printer.print(&schedule)
        }
        crate::SchedulesCommand::Create(args) => {
            let route_ids = crate::cli::fetch_known_route_ids(client).await?;
            let request = crate::cli::build_schedule_create_request(&args, &route_ids).await?;
            let schedule = client
                .post_json::<_, kheish_daemon::ScheduleView>("/v1/schedules", &request)
                .await?;
            printer.print(&schedule)
        }
        crate::SchedulesCommand::Cancel { schedule_id } => {
            let schedule_id = crate::cli::url_encode_path_segment(&schedule_id);
            let response = client
                .post_json::<_, kheish_daemon::ScheduleMutationResponse>(
                    &format!("/v1/schedules/{schedule_id}/cancel"),
                    &serde_json::json!({}),
                )
                .await?;
            printer.print(&response.schedule)
        }
        crate::SchedulesCommand::Pause { schedule_id } => {
            let schedule_id = crate::cli::url_encode_path_segment(&schedule_id);
            let response = client
                .post_json::<_, kheish_daemon::ScheduleMutationResponse>(
                    &format!("/v1/schedules/{schedule_id}/pause"),
                    &serde_json::json!({}),
                )
                .await?;
            printer.print(&response.schedule)
        }
        crate::SchedulesCommand::Resume { schedule_id } => {
            let schedule_id = crate::cli::url_encode_path_segment(&schedule_id);
            let response = client
                .post_json::<_, kheish_daemon::ScheduleMutationResponse>(
                    &format!("/v1/schedules/{schedule_id}/resume"),
                    &serde_json::json!({}),
                )
                .await?;
            printer.print(&response.schedule)
        }
        crate::SchedulesCommand::TriggerNow { schedule_id } => {
            let schedule_id = crate::cli::url_encode_path_segment(&schedule_id);
            let response = client
                .post_json::<_, kheish_daemon::ScheduleMutationResponse>(
                    &format!("/v1/schedules/{schedule_id}/trigger"),
                    &serde_json::json!({}),
                )
                .await?;
            printer.print(&response.schedule)
        }
    }
}
