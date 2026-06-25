mod live_support;

use anyhow::Result;

use live_support::{
    LiveProviderKind, run_agent_hook_block_scenario, run_agent_lifecycle_hook_scenario,
    run_agent_profile_shell_surface_scenario, run_anthropic_claude_code_account_auth_scenario,
    run_attachment_inputs_scenario, run_attachment_restart_replay_scenario,
    run_automatic_learning_policy_scenario, run_background_shell_stop_scenario,
    run_background_shell_task_scenario, run_command_hook_lifecycle_scenario,
    run_connector_authentication_scenario, run_connector_parent_child_route_restart_scenario,
    run_connector_parent_child_route_scenario, run_connector_restart_question_scenario,
    run_foreground_shell_interrupt_scenario, run_generated_image_http_output_scenario,
    run_generated_image_output_scenario, run_generated_image_slack_output_scenario,
    run_generated_image_telegram_output_scenario, run_http_multimodal_connector_scenario,
    run_http_output_retry_restart_scenario, run_http_output_retry_scenario,
    run_learning_governance_scenario, run_learning_judge_scenario,
    run_mailbox_mixed_provider_web_search_scenario, run_mcp_linear_profile_scenario,
    run_mcp_openai_docs_scenario, run_mixed_ingress_shared_session_scenario,
    run_permission_hook_write_scenario, run_procedural_learning_skill_promotion_scenario,
    run_prompt_hook_block_scenario, run_recovered_memory_debug_scenario,
    run_repo_validation_skill_marker_scenario, run_schedule_recurring_scenario,
    run_semantic_capture_automatic_publication_scenario, run_semantic_capture_scenario,
    run_session_memory_context_and_visible_skills_scenario, run_skill_fork_scenario,
    run_skill_inline_restart_scenario, run_slack_fanout_connector_scenario,
    run_slack_multimodal_connector_scenario, run_subagent_mailbox_question_decline_scenario,
    run_subagent_mailbox_question_restart_scenario, run_subagent_mailbox_question_scenario,
    run_subagent_mixed_provider_web_search_scenario, run_subagent_parent_wake_scenario,
    run_subagent_web_search_scenario, run_telegram_http_connector_scenario,
    run_telegram_multimodal_connector_scenario, run_telegram_polling_http_fanout_scenario,
    run_telegram_polling_multimodal_connector_scenario, run_telegram_polling_restart_scenario,
    run_telegram_polling_self_output_scenario, run_telegram_polling_shared_session_scenario,
    run_user_question_decline_scenario, run_user_question_scenario,
    run_wake_after_restart_scenario, run_wake_after_scenario,
    run_web_search_backend_routing_scenario, run_web_search_sourced_answer_scenario,
    run_web_search_sourced_post_scenario,
};

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_command_hooks_cover_session_lifecycle() -> Result<()> {
    run_command_hook_lifecycle_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_prompt_hooks_can_block_input() -> Result<()> {
    run_prompt_hook_block_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_agent_hooks_can_run_tools_and_block_input() -> Result<()> {
    run_agent_hook_block_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_permission_hooks_can_auto_allow_writes() -> Result<()> {
    run_permission_hook_write_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_agent_lifecycle_hooks_cover_sidechains() -> Result<()> {
    run_agent_lifecycle_hook_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_background_shell_tasks_can_be_observed() -> Result<()> {
    run_background_shell_task_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_background_shell_tasks_can_be_stopped() -> Result<()> {
    run_background_shell_stop_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_foreground_shell_tasks_survive_interrupts() -> Result<()> {
    run_foreground_shell_interrupt_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_agent_profiles_cover_shell_task_follow_up() -> Result<()> {
    run_agent_profile_shell_surface_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_mcp_openai_docs_tools_are_usable() -> Result<()> {
    run_mcp_openai_docs_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_mcp_linear_tools_are_usable() -> Result<()> {
    run_mcp_linear_profile_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_inline_skills_survive_restart() -> Result<()> {
    run_skill_inline_restart_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_fork_skills_spawn_child_agents() -> Result<()> {
    run_skill_fork_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_repo_validation_skill_returns_readiness_marker() -> Result<()> {
    run_repo_validation_skill_marker_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_attachment_inputs_cover_supported_file_types() -> Result<()> {
    run_attachment_inputs_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_attachment_inputs_replay_after_restart_across_provider_routes() -> Result<()>
{
    run_attachment_restart_replay_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_recovered_memory_is_present_in_provider_requests() -> Result<()> {
    run_recovered_memory_debug_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_learning_governance_flows_work_end_to_end() -> Result<()> {
    run_learning_governance_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_learning_automation_policy_flows_work_end_to_end() -> Result<()> {
    run_automatic_learning_policy_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_learning_judge_reviews_automatic_publication() -> Result<()> {
    run_learning_judge_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_semantic_capture_creates_daemon_candidates() -> Result<()> {
    run_semantic_capture_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_semantic_capture_can_auto_publish_daemon_origin_candidates() -> Result<()> {
    run_semantic_capture_automatic_publication_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_session_memory_context_and_visible_skills_are_projected() -> Result<()> {
    run_session_memory_context_and_visible_skills_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_procedural_learning_can_be_promoted_into_a_restartable_skill() -> Result<()>
{
    run_procedural_learning_skill_promotion_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_http_connector_accepts_multimodal_inputs() -> Result<()> {
    run_http_multimodal_connector_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_slack_connector_accepts_multimodal_inputs() -> Result<()> {
    run_slack_multimodal_connector_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_telegram_connector_accepts_multimodal_inputs() -> Result<()> {
    run_telegram_multimodal_connector_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_telegram_polling_accepts_multimodal_inputs() -> Result<()> {
    run_telegram_polling_multimodal_connector_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_can_generate_image_outputs() -> Result<()> {
    run_generated_image_output_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_generated_images_can_fan_out_to_http() -> Result<()> {
    run_generated_image_http_output_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_generated_images_can_fan_out_to_telegram() -> Result<()> {
    run_generated_image_telegram_output_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_generated_images_can_fan_out_to_slack() -> Result<()> {
    run_generated_image_slack_output_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_web_search_can_answer_with_sources() -> Result<()> {
    run_web_search_sourced_answer_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_web_search_can_write_a_sourced_post() -> Result<()> {
    run_web_search_sourced_post_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_subagents_can_use_web_search() -> Result<()> {
    run_subagent_web_search_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_web_search_prefers_native_backend_when_available() -> Result<()> {
    run_web_search_backend_routing_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_subagents_can_route_web_search_to_a_different_provider() -> Result<()> {
    run_subagent_mixed_provider_web_search_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_mailbox_runs_preserve_the_child_web_search_route() -> Result<()> {
    run_mailbox_mixed_provider_web_search_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_can_pause_for_user_questions_and_resume() -> Result<()> {
    run_user_question_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_can_handle_declined_user_questions() -> Result<()> {
    run_user_question_decline_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_can_schedule_one_wake_after() -> Result<()> {
    run_wake_after_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_wake_after_survives_restart() -> Result<()> {
    run_wake_after_restart_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_can_run_recurring_schedules() -> Result<()> {
    run_schedule_recurring_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_subagents_can_schedule_parent_wakeups() -> Result<()> {
    run_subagent_parent_wake_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_subagents_can_route_questions_back_to_the_parent() -> Result<()> {
    run_subagent_mailbox_question_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_subagents_can_resume_parent_questions_after_restart() -> Result<()> {
    run_subagent_mailbox_question_restart_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_subagents_can_handle_declined_parent_questions() -> Result<()> {
    run_subagent_mailbox_question_decline_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_telegram_ingress_can_route_to_http_output() -> Result<()> {
    run_telegram_http_connector_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_http_output_retries_after_transient_failure() -> Result<()> {
    run_http_output_retry_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_http_output_survives_restart_while_pending_delivery() -> Result<()> {
    run_http_output_retry_restart_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_telegram_polling_can_reply_in_the_same_chat() -> Result<()> {
    run_telegram_polling_self_output_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_telegram_polling_can_fan_out_to_http() -> Result<()> {
    run_telegram_polling_http_fanout_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_telegram_polling_preserves_session_memory() -> Result<()> {
    run_telegram_polling_shared_session_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_telegram_polling_survives_restart_without_replay() -> Result<()> {
    run_telegram_polling_restart_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_slack_ingress_can_fan_out_to_slack_and_http() -> Result<()> {
    run_slack_fanout_connector_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_mixed_ingress_can_share_one_session() -> Result<()> {
    run_mixed_ingress_shared_session_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_connector_routes_survive_question_restart() -> Result<()> {
    run_connector_restart_question_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_connector_routes_survive_parent_child_clarification() -> Result<()> {
    run_connector_parent_child_route_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_connector_authentication_is_enforced() -> Result<()> {
    run_connector_authentication_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_connector_routes_survive_parent_child_restart() -> Result<()> {
    run_connector_parent_child_route_restart_scenario(LiveProviderKind::Anthropic).await
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_claude_code_account_auth_can_drive_the_daemon() -> Result<()> {
    run_anthropic_claude_code_account_auth_scenario().await
}
