//! CLI dispatch and shared helpers for the daemon binary.

pub(crate) mod app;
pub(crate) mod builders;
pub(crate) mod commands;
pub(crate) mod exit;
pub(crate) mod http;
pub(crate) mod io;
pub(crate) mod output;
pub(crate) mod routing;
pub(crate) mod secrets;
pub(crate) mod serve;
pub(crate) mod state_lock;
pub(crate) mod wait;

pub(crate) use builders::{
    build_completion_requirements, build_observation_materialization_request,
    build_observation_schedule_create_request, build_schedule_create_request,
    build_session_route_policy, build_subtask_request,
};
pub(crate) use exit::exit_code_for_error;
pub(crate) use http::DaemonHttpClient;
pub(crate) use io::{
    build_session_input_attachments, inline_asset_upload_from_path, read_json_input,
    read_optional_json_input, read_optional_text_input, read_optional_typed_json_input,
    read_required_typed_json_input, read_text_input, url_encode_component, url_encode_path_segment,
};
pub(crate) use output::Printer;
pub(crate) use routing::{
    current_routes_file_sha256, fetch_known_route_ids, normalize_provider_and_generation,
    parse_model_selector, read_route_inventory_metadata, write_route_inventory_metadata,
};
pub(crate) use secrets::{
    daemon_control_plane_is_reachable, default_codex_auth_path,
    ensure_secret_manager_master_key_configured, global_auth_store_path, read_secret_arg,
    resolve_cli_control_plane_token,
};
pub(crate) use wait::{
    collect_pending_approvals, collect_pending_questions, find_pending_approval_run_id,
    find_pending_question_run_id, load_question_answers, resolve_all_approvals, resolve_approval,
    wait_for_all_runs_after_approval_resolution, wait_for_run,
    wait_for_run_after_approval_resolution,
};
