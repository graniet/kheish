mod live_support;

use anyhow::Result;

use live_support::{
    LiveProviderKind, run_attachment_inputs_scenario, run_attachment_restart_replay_scenario,
    run_edited_image_png_output_scenario, run_forced_tool_call_scenario,
    run_generated_image_http_output_scenario, run_generated_image_output_scenario,
    run_generated_image_slack_output_scenario, run_generated_image_telegram_output_scenario,
    run_mailbox_mixed_provider_web_search_scenario, run_structured_output_strict_scenario,
    run_subagent_mixed_provider_web_search_scenario, run_text_response_scenario,
    run_web_search_backend_routing_scenario, run_web_search_local_fallback_scenario,
};

#[tokio::test(flavor = "multi_thread")]
async fn xai_live_text_responses_work() -> Result<()> {
    run_text_response_scenario(LiveProviderKind::XAi).await
}

#[tokio::test(flavor = "multi_thread")]
async fn xai_live_attachment_inputs_cover_supported_file_types() -> Result<()> {
    run_attachment_inputs_scenario(LiveProviderKind::XAi).await
}

#[tokio::test(flavor = "multi_thread")]
async fn xai_live_attachment_inputs_replay_after_restart_across_provider_routes() -> Result<()> {
    run_attachment_restart_replay_scenario(LiveProviderKind::XAi).await
}

#[tokio::test(flavor = "multi_thread")]
async fn xai_live_forced_tool_call_uses_function_shape() -> Result<()> {
    run_forced_tool_call_scenario(LiveProviderKind::XAi).await
}

#[tokio::test(flavor = "multi_thread")]
async fn xai_live_structured_output_uses_strict_schema() -> Result<()> {
    run_structured_output_strict_scenario(LiveProviderKind::XAi).await
}

#[tokio::test(flavor = "multi_thread")]
async fn xai_live_can_generate_image_outputs() -> Result<()> {
    run_generated_image_output_scenario(LiveProviderKind::XAi).await
}

#[tokio::test(flavor = "multi_thread")]
async fn xai_live_can_edit_image_outputs() -> Result<()> {
    run_edited_image_png_output_scenario(LiveProviderKind::XAi).await
}

#[tokio::test(flavor = "multi_thread")]
async fn xai_live_generated_images_can_fan_out_to_http() -> Result<()> {
    run_generated_image_http_output_scenario(LiveProviderKind::XAi).await
}

#[tokio::test(flavor = "multi_thread")]
async fn xai_live_generated_images_can_fan_out_to_telegram() -> Result<()> {
    run_generated_image_telegram_output_scenario(LiveProviderKind::XAi).await
}

#[tokio::test(flavor = "multi_thread")]
async fn xai_live_generated_images_can_fan_out_to_slack() -> Result<()> {
    run_generated_image_slack_output_scenario(LiveProviderKind::XAi).await
}

#[tokio::test(flavor = "multi_thread")]
async fn xai_live_web_search_prefers_provider_native_backend() -> Result<()> {
    run_web_search_backend_routing_scenario(LiveProviderKind::XAi).await
}

#[tokio::test(flavor = "multi_thread")]
async fn xai_live_web_search_falls_back_locally_for_unsupported_inputs() -> Result<()> {
    run_web_search_local_fallback_scenario(LiveProviderKind::XAi).await
}

#[tokio::test(flavor = "multi_thread")]
async fn xai_live_subagents_can_route_web_search_to_a_different_provider() -> Result<()> {
    run_subagent_mixed_provider_web_search_scenario(LiveProviderKind::XAi).await
}

#[tokio::test(flavor = "multi_thread")]
async fn xai_live_mailbox_runs_preserve_the_child_web_search_route() -> Result<()> {
    run_mailbox_mixed_provider_web_search_scenario(LiveProviderKind::XAi).await
}
