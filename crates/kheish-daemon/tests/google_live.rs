mod live_support;

use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use image::{ColorType, ImageBuffer, ImageEncoder, Rgb, codecs::jpeg::JpegEncoder};
use kheish_auth::AuthManager;
use kheish_daemon::{
    AdditionalImageBackendConfig, CreateDerivationRequest, DaemonConfig, DaemonRunStatus,
    DerivationProfile, DerivationSubject, DerivationTranscriptionOptions, ModelRouteConfig,
    SubmitInputItemRequest, SubmitInputRequest, build_provider_daemon,
};
use kheish_runtime::{GoogleProviderConfig, ModelGenerationConfig};
use kheish_types::{
    ResponseFormat, SessionEvent, StructuredFieldSchema, StructuredValueKind, ToolChoice,
};
use reqwest::Client;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::oneshot;

use live_support::{
    create_session, get_run_debug_artifact, get_session, get_session_events, import_asset,
    sample_wav_bytes, set_debug_level, submit_run_request, wait_for_run, wait_for_run_with_timeout,
};

const DEFAULT_GOOGLE_TEXT_MODEL: &str = "gemini-2.5-flash";
const DEFAULT_GOOGLE_IMAGE_MODEL: &str = "gemini-2.5-flash-image";
const GOOGLE_IMAGE_RUN_TIMEOUT: Duration = Duration::from_secs(300);

struct GoogleLiveHarness {
    _temp: TempDir,
    base_url: String,
    state_root: PathBuf,
    workspace_root: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    server_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for GoogleLiveHarness {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
    }
}

fn first_env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| std::env::var(key).ok())
}

fn google_provider_configs() -> Option<(GoogleProviderConfig, GoogleProviderConfig)> {
    let api_key = first_env(&["KHEISH_GOOGLE_API_KEY", "GOOGLE_API_KEY", "GEMINI_API_KEY"])?;
    let text_model = first_env(&[
        "KHEISH_GOOGLE_LIVE_MODEL",
        "KHEISH_GOOGLE_MODEL",
        "GOOGLE_MODEL",
    ])
    .unwrap_or_else(|| DEFAULT_GOOGLE_TEXT_MODEL.to_string());
    let image_model = first_env(&[
        "KHEISH_GOOGLE_IMAGE_MODEL",
        "GOOGLE_IMAGE_MODEL",
        "GEMINI_IMAGE_MODEL",
    ])
    .unwrap_or_else(|| DEFAULT_GOOGLE_IMAGE_MODEL.to_string());
    Some((
        GoogleProviderConfig::new(text_model, api_key.clone()),
        GoogleProviderConfig::new(image_model, api_key),
    ))
}

fn expected_google_image_model() -> String {
    first_env(&[
        "KHEISH_GOOGLE_IMAGE_MODEL",
        "GOOGLE_IMAGE_MODEL",
        "GEMINI_IMAGE_MODEL",
    ])
    .unwrap_or_else(|| DEFAULT_GOOGLE_IMAGE_MODEL.to_string())
}

async fn start_google_live_daemon() -> Result<Option<GoogleLiveHarness>> {
    let Some((provider, image_provider)) = google_provider_configs() else {
        eprintln!("Skipping Google live tests: no Google API key environment variable was set.");
        return Ok(None);
    };
    let temp = tempfile::tempdir()?;
    let state_root = temp.path().join("state");
    let workspace_root = temp.path().join("workspace");
    fs::create_dir_all(&workspace_root)?;
    let config = DaemonConfig::new(
        "127.0.0.1:0".parse::<SocketAddr>()?,
        &state_root,
        &workspace_root,
    );
    let auth_manager = AuthManager::new(state_root.join("auth/global-slots.json"))?;
    let (service, listener) = build_provider_daemon(
        config,
        vec![ModelRouteConfig::Google(provider)],
        vec![AdditionalImageBackendConfig::google(image_provider)],
        Vec::new(),
        auth_manager,
    )
    .await?;
    let address = listener.local_addr()?;
    let (shutdown, shutdown_rx) = oneshot::channel();
    let server_task = tokio::spawn(async move {
        let _ = service
            .serve_with_shutdown(listener, async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });
    Ok(Some(GoogleLiveHarness {
        _temp: temp,
        base_url: format!("http://{address}"),
        state_root,
        workspace_root,
        shutdown: Some(shutdown),
        server_task: Some(server_task),
    }))
}

fn sample_png_bytes() -> Result<Vec<u8>> {
    let image = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_pixel(32, 32, Rgb([255, 0, 0]));
    let mut cursor = std::io::Cursor::new(Vec::new());
    image.write_to(&mut cursor, image::ImageFormat::Png)?;
    Ok(cursor.into_inner())
}

fn sample_jpeg_bytes() -> Result<Vec<u8>> {
    let image = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_pixel(32, 32, Rgb([0, 0, 255]));
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, 90).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        ColorType::Rgb8.into(),
    )?;
    Ok(bytes)
}

fn sample_csv_bytes() -> Vec<u8> {
    b"room,width,height\nDining Room,6.5,3.4\nOther,9.4,2.9\n".to_vec()
}

fn sample_dxf_bytes() -> Vec<u8> {
    [
        "0",
        "SECTION",
        "2",
        "HEADER",
        "9",
        "$ACADVER",
        "1",
        "AC1015",
        "9",
        "$DWGCODEPAGE",
        "3",
        "ANSI_1252",
        "0",
        "ENDSEC",
        "0",
        "SECTION",
        "2",
        "ENTITIES",
        "0",
        "LWPOLYLINE",
        "8",
        "Walls",
        "70",
        "1",
        "10",
        "0.0",
        "20",
        "0.0",
        "10",
        "10.0",
        "20",
        "0.0",
        "10",
        "10.0",
        "20",
        "5.0",
        "10",
        "0.0",
        "20",
        "5.0",
        "0",
        "DIMENSION",
        "8",
        "Dims",
        "42",
        "3.2",
        "1",
        "3.20",
        "0",
        "TEXT",
        "8",
        "Labels",
        "1",
        "Dining Room",
        "10",
        "1.0",
        "20",
        "2.0",
        "0",
        "ENDSEC",
        "0",
        "EOF",
    ]
    .join("\n")
    .into_bytes()
}

fn generated_image_prompt(marker: &str) -> String {
    format!(
        "Create exactly one simple image of a solid black square on a plain white background. You must call generate_image to create the image. Then call emit_output with content exactly `{marker}`. Put the generated asset IDs in `artifact_ids` and set `include_artifacts_inline` to true so the image is both visible inline and retained in artifacts. Do not finish until emit_output has been called."
    )
}

fn edited_image_prompt(asset_id: &str, marker: &str) -> String {
    format!(
        "Edit the daemon-owned image asset `{asset_id}`. You must call edit_image with `image_asset_ids` containing exactly `{asset_id}` and a prompt that requests a neutral architectural edit without inventing new geometry. Do not call generate_image in this run. After edit_image returns, call emit_output with content exactly `{marker}`. Put the edited asset IDs in `artifact_ids` and set `include_artifacts_inline` to true so the edited image is both visible inline and retained in artifacts. Do not finish until emit_output has been called."
    )
}

fn edited_multi_image_prompt(
    primary_asset_id: &str,
    reference_asset_id: &str,
    marker: &str,
) -> String {
    format!(
        "Edit the daemon-owned image asset `{primary_asset_id}` using daemon-owned image asset `{reference_asset_id}` only as additional visual context. You must call edit_image with `image_asset_ids` containing exactly `{primary_asset_id}` first and `{reference_asset_id}` second. The prompt must request a neutral architectural edit without inventing new geometry and must preserve the source layout. Do not call generate_image in this run. After edit_image returns, call emit_output with content exactly `{marker}`. Put the edited asset IDs in `artifact_ids` and set `include_artifacts_inline` to true so the edited image is both visible inline and retained in artifacts. Do not finish until emit_output has been called."
    )
}

fn tool_succeeded(events: &kheish_daemon::SessionEventLogView, tool_name: &str) -> bool {
    events.session.journal.iter().any(|entry| {
        matches!(
            &entry.event,
            SessionEvent::ToolCallFinished { result }
                if result.tool_name.as_deref() == Some(tool_name) && !result.is_error
        )
    })
}

fn last_tool_result<'a>(
    events: &'a kheish_daemon::SessionEventLogView,
    tool_name: &str,
) -> Option<&'a kheish_types::ToolResultRecord> {
    events
        .session
        .journal
        .iter()
        .rev()
        .find_map(|entry| match &entry.event {
            SessionEvent::ToolCallFinished { result }
                if result.tool_name.as_deref() == Some(tool_name) && !result.is_error =>
            {
                Some(result)
            }
            _ => None,
        })
}

fn json_has_inline_base64_redaction(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            if let Some(inline_data) = map.get("inlineData").or_else(|| map.get("inline_data"))
                && inline_data["data"]["redacted"] == "<redacted base64>"
            {
                return true;
            }
            map.values().any(json_has_inline_base64_redaction)
        }
        Value::Array(items) => items.iter().any(json_has_inline_base64_redaction),
        _ => false,
    }
}

fn collect_debug_text(root: &Path) -> Result<String> {
    let mut text = String::new();
    let debug_root = root.join("debug");
    if !debug_root.exists() {
        return Ok(text);
    }
    let mut stack = vec![debug_root];
    while let Some(path) = stack.pop() {
        let metadata = fs::metadata(&path)?;
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
        } else if metadata.is_file()
            && let Ok(bytes) = fs::read(&path)
            && let Ok(content) = String::from_utf8(bytes)
        {
            text.push_str(&content);
            text.push('\n');
        }
    }
    Ok(text)
}

async fn assert_google_image_debug_artifacts_sanitized(
    client: &Client,
    base_url: &str,
    run_id: &str,
    state_root: &Path,
    artifact_prefix: &str,
    source_images: &[&[u8]],
) -> Result<()> {
    let request_artifact_id = format!("{artifact_prefix}-provider-request");
    let response_artifact_id = format!("{artifact_prefix}-provider-response");
    let request = get_run_debug_artifact(client, base_url, run_id, &request_artifact_id).await?;
    let response = get_run_debug_artifact(client, base_url, run_id, &response_artifact_id).await?;
    anyhow::ensure!(
        request["provider"].as_str() == Some("google")
            && request["headers"]["x-goog-api-key"] == "<redacted>",
        "Google image request artifact missing redacted auth header: {}",
        serde_json::to_string_pretty(&request)?
    );
    anyhow::ensure!(
        response["provider"].as_str() == Some("google")
            && json_has_inline_base64_redaction(&response),
        "Google image response artifact missing redacted inline image payload: {}",
        serde_json::to_string_pretty(&response)?
    );

    let rendered = format!(
        "{}\n{}",
        serde_json::to_string(&request)?,
        serde_json::to_string(&response)?
    );
    let debug_text = collect_debug_text(state_root)?;
    for key in ["KHEISH_GOOGLE_API_KEY", "GOOGLE_API_KEY", "GEMINI_API_KEY"] {
        if let Ok(secret) = std::env::var(key)
            && !secret.trim().is_empty()
        {
            anyhow::ensure!(
                !rendered.contains(secret.trim()),
                "Google image debug artifacts leaked {key}"
            );
            anyhow::ensure!(
                !debug_text.contains(secret.trim()),
                "Google image debug files leaked {key}"
            );
        }
    }
    anyhow::ensure!(
        !debug_text.contains("data:image/"),
        "Google image debug files leaked a data URL"
    );
    if !source_images.is_empty() {
        anyhow::ensure!(
            request["body"]
                .get("contents")
                .is_some_and(json_has_inline_base64_redaction),
            "Google image request artifact missing redacted inline source image: {}",
            serde_json::to_string_pretty(&request)?
        );
    }
    for source_image_bytes in source_images {
        let source_base64 = BASE64_STANDARD.encode(source_image_bytes);
        anyhow::ensure!(
            !rendered.contains(&source_base64),
            "Google image debug artifacts leaked source image base64"
        );
        anyhow::ensure!(
            !debug_text.contains(&source_base64),
            "Google image debug files leaked source image base64"
        );
    }
    Ok(())
}

async fn run_generated_image_case(base_url: &str, state_root: &Path) -> Result<()> {
    let client = Client::new();
    set_debug_level(&client, base_url, kheish_runtime::DebugCaptureLevel::Full).await?;
    let session_id = "google-live-generated-image";
    create_session(&client, base_url, session_id).await?;
    let run = submit_run_request(
        &client,
        base_url,
        session_id,
        SubmitInputRequest {
            source_plugin: None,
            source_kind: None,
            actor_id: None,
            provider: None,
            content: generated_image_prompt("GOOGLE_GENERATED_IMAGE_READY"),
            input_items: Vec::new(),
            attachments: Vec::new(),
            generation: Some(ModelGenerationConfig::default()),
            completion_requirements: None,
            metadata: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
            reply_address: None,
        },
    )
    .await?;
    let completed =
        wait_for_run_with_timeout(&client, base_url, &run.run_id, GOOGLE_IMAGE_RUN_TIMEOUT).await?;
    anyhow::ensure!(
        completed.status == DaemonRunStatus::Completed,
        "Google generated image run failed: {}",
        serde_json::to_string_pretty(&completed)?
    );
    let session = get_session(&client, base_url, session_id).await?;
    anyhow::ensure!(
        session.outputs.iter().any(|output| {
            output.content.contains("GOOGLE_GENERATED_IMAGE_READY") && !output.artifacts.is_empty()
        }),
        "Google generated image output missing visible artifacts: {}",
        serde_json::to_string_pretty(&session)?
    );
    let events = get_session_events(&client, base_url, session_id).await?;
    let generate_result = last_tool_result(&events, "generate_image")
        .ok_or_else(|| anyhow!("missing Google generate_image tool result"))?;
    anyhow::ensure!(
        tool_succeeded(&events, "generate_image"),
        "Google generated image run did not use generate_image successfully: {}",
        serde_json::to_string_pretty(&events)?
    );
    anyhow::ensure!(
        generate_result.output["provider"] == "google"
            && generate_result.output["model"] == expected_google_image_model(),
        "unexpected Google generate_image routing: {}",
        serde_json::to_string_pretty(&generate_result.output)?
    );
    assert_google_image_debug_artifacts_sanitized(
        &client,
        base_url,
        &run.run_id,
        state_root,
        "google-image-generation",
        &[],
    )
    .await?;
    Ok(())
}

async fn run_edited_image_case(
    base_url: &str,
    state_root: &Path,
    session_id: &str,
    file_name: &str,
    media_type: &str,
    bytes: &[u8],
    marker: &str,
) -> Result<()> {
    let client = Client::new();
    set_debug_level(&client, base_url, kheish_runtime::DebugCaptureLevel::Full).await?;
    create_session(&client, base_url, session_id).await?;
    let asset = import_asset(&client, base_url, file_name, media_type, bytes).await?;
    let run = submit_run_request(
        &client,
        base_url,
        session_id,
        SubmitInputRequest {
            source_plugin: None,
            source_kind: None,
            actor_id: None,
            provider: None,
            content: edited_image_prompt(&asset.asset_id, marker),
            input_items: Vec::new(),
            attachments: Vec::new(),
            generation: Some(ModelGenerationConfig::default()),
            completion_requirements: None,
            metadata: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
            reply_address: None,
        },
    )
    .await?;
    let completed =
        wait_for_run_with_timeout(&client, base_url, &run.run_id, GOOGLE_IMAGE_RUN_TIMEOUT).await?;
    anyhow::ensure!(
        completed.status == DaemonRunStatus::Completed,
        "Google edited image run failed: {}",
        serde_json::to_string_pretty(&completed)?
    );
    let session = get_session(&client, base_url, session_id).await?;
    anyhow::ensure!(
        session
            .outputs
            .iter()
            .any(|output| output.content.contains(marker) && !output.artifacts.is_empty()),
        "Google edited image output missing visible artifacts: {}",
        serde_json::to_string_pretty(&session)?
    );
    let events = get_session_events(&client, base_url, session_id).await?;
    let edit_result = last_tool_result(&events, "edit_image")
        .ok_or_else(|| anyhow!("missing Google edit_image tool result"))?;
    anyhow::ensure!(
        tool_succeeded(&events, "edit_image"),
        "Google edited image run did not use edit_image successfully: {}",
        serde_json::to_string_pretty(&events)?
    );
    anyhow::ensure!(
        !tool_succeeded(&events, "generate_image"),
        "Google edited image run unexpectedly used generate_image: {}",
        serde_json::to_string_pretty(&events)?
    );
    anyhow::ensure!(
        edit_result.output["provider"] == "google"
            && edit_result.output["model"] == expected_google_image_model(),
        "unexpected Google edit_image routing: {}",
        serde_json::to_string_pretty(&edit_result.output)?
    );
    let source_images = [bytes];
    assert_google_image_debug_artifacts_sanitized(
        &client,
        base_url,
        &run.run_id,
        state_root,
        "google-image-edit",
        &source_images,
    )
    .await?;
    Ok(())
}

async fn run_multi_image_edit_case(base_url: &str, state_root: &Path) -> Result<()> {
    let client = Client::new();
    set_debug_level(&client, base_url, kheish_runtime::DebugCaptureLevel::Full).await?;
    let session_id = "google-live-edited-image-multi";
    create_session(&client, base_url, session_id).await?;
    let primary_bytes = sample_png_bytes()?;
    let reference_bytes = sample_jpeg_bytes()?;
    let primary = import_asset(
        &client,
        base_url,
        "sample-plan.png",
        "image/png",
        &primary_bytes,
    )
    .await?;
    let reference = import_asset(
        &client,
        base_url,
        "sample-reference.jpg",
        "image/jpeg",
        &reference_bytes,
    )
    .await?;
    let run = submit_run_request(
        &client,
        base_url,
        session_id,
        SubmitInputRequest {
            source_plugin: None,
            source_kind: None,
            actor_id: None,
            provider: None,
            content: edited_multi_image_prompt(
                &primary.asset_id,
                &reference.asset_id,
                "GOOGLE_EDITED_IMAGE_MULTI_READY",
            ),
            input_items: Vec::new(),
            attachments: Vec::new(),
            generation: Some(ModelGenerationConfig::default()),
            completion_requirements: None,
            metadata: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
            reply_address: None,
        },
    )
    .await?;
    let completed =
        wait_for_run_with_timeout(&client, base_url, &run.run_id, GOOGLE_IMAGE_RUN_TIMEOUT).await?;
    anyhow::ensure!(
        completed.status == DaemonRunStatus::Completed,
        "Google multi-image edit run failed: {}",
        serde_json::to_string_pretty(&completed)?
    );
    let events = get_session_events(&client, base_url, session_id).await?;
    let edit_result = last_tool_result(&events, "edit_image")
        .ok_or_else(|| anyhow!("missing Google edit_image tool result"))?;
    anyhow::ensure!(
        edit_result.output["provider"] == "google"
            && edit_result.output["model"] == expected_google_image_model(),
        "unexpected provider/model in Google multi-image edit result: {}",
        serde_json::to_string_pretty(&edit_result.output)?
    );
    let source_images = [primary_bytes.as_slice(), reference_bytes.as_slice()];
    assert_google_image_debug_artifacts_sanitized(
        &client,
        base_url,
        &run.run_id,
        state_root,
        "google-image-edit",
        &source_images,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn google_live_structured_json_smoke() -> Result<()> {
    let Some(harness) = start_google_live_daemon().await? else {
        return Ok(());
    };
    let client = Client::new();
    set_debug_level(
        &client,
        &harness.base_url,
        kheish_runtime::DebugCaptureLevel::Full,
    )
    .await?;
    let session_id = "google-live-structured-json";
    create_session(&client, &harness.base_url, session_id).await?;
    let run = submit_run_request(
        &client,
        &harness.base_url,
        session_id,
        SubmitInputRequest {
            source_plugin: None,
            source_kind: None,
            actor_id: None,
            provider: None,
            content: "Return JSON for this schema. Set `marker` to exactly `GOOGLE_STRUCTURED_OK`. Set `score` to exactly 99. Use separate fields and no other fields.".to_string(),
            input_items: Vec::new(),
            attachments: Vec::new(),
            generation: Some(ModelGenerationConfig {
                tool_choice: ToolChoice::None,
                allow_parallel_tool_calls: false,
                response_format: ResponseFormat::StructuredJson {
                    schema: StructuredFieldSchema {
                        kind: StructuredValueKind::Object,
                        fields: BTreeMap::from([(
                            "marker".to_string(),
                            StructuredFieldSchema::new(StructuredValueKind::String),
                        ), (
                            "score".to_string(),
                            StructuredFieldSchema::new(StructuredValueKind::Number),
                        )]),
                        optional_fields: BTreeMap::new(),
                        items: None,
                    },
                },
                ..ModelGenerationConfig::default()
            }),
            completion_requirements: None,
            metadata: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
            reply_address: None,
        },
    )
    .await?;
    let completed = wait_for_run(&client, &harness.base_url, &run.run_id).await?;
    anyhow::ensure!(
        completed.status == DaemonRunStatus::Completed,
        "Google structured run failed: {}",
        serde_json::to_string_pretty(&completed)?
    );
    let output = completed
        .outputs
        .last()
        .map(|record| record.content.trim().to_string())
        .ok_or_else(|| anyhow!("Google structured run did not emit output"))?;
    let value: Value =
        serde_json::from_str(&output).map_err(|error| anyhow!("invalid JSON {output}: {error}"))?;
    anyhow::ensure!(
        value["marker"].as_str() == Some("GOOGLE_STRUCTURED_OK")
            && value["score"].as_f64() == Some(99.0)
            && value
                .as_object()
                .is_some_and(|object| object.keys().all(|key| key == "marker" || key == "score")),
        "unexpected Google structured output: {output}"
    );
    let provider_request = get_run_debug_artifact(
        &client,
        &harness.base_url,
        &run.run_id,
        "turn-0001-attempt-0001-provider-request",
    )
    .await?;
    anyhow::ensure!(
        provider_request["provider"].as_str() == Some("google")
            && provider_request["body"]["generationConfig"]["responseMimeType"]
                == "application/json"
            && provider_request["body"]["generationConfig"]["responseJsonSchema"]["properties"]["marker"]
                ["type"]
                == "string",
        "Google structured provider request did not include JSON schema controls: {}",
        serde_json::to_string_pretty(&provider_request)?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn google_live_forced_tool_call_smoke() -> Result<()> {
    let Some(harness) = start_google_live_daemon().await? else {
        return Ok(());
    };
    fs::write(
        harness.workspace_root.join("probe.txt"),
        "GOOGLE_TOOL_SENTINEL\n",
    )?;
    let client = Client::new();
    set_debug_level(
        &client,
        &harness.base_url,
        kheish_runtime::DebugCaptureLevel::Full,
    )
    .await?;
    let session_id = "google-live-forced-tool";
    create_session(&client, &harness.base_url, session_id).await?;
    let run = submit_run_request(
        &client,
        &harness.base_url,
        session_id,
        SubmitInputRequest {
            source_plugin: None,
            source_kind: None,
            actor_id: None,
            provider: None,
            content: "Use read_file exactly once on `probe.txt`. If it contains GOOGLE_TOOL_SENTINEL, reply exactly GOOGLE_TOOL_OK.".to_string(),
            input_items: Vec::new(),
            attachments: Vec::new(),
            generation: Some(ModelGenerationConfig {
                tool_choice: ToolChoice::Specific {
                    name: "read_file".to_string(),
                },
                allow_parallel_tool_calls: false,
                ..ModelGenerationConfig::default()
            }),
            completion_requirements: None,
            metadata: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
            reply_address: None,
        },
    )
    .await?;
    let completed = wait_for_run(&client, &harness.base_url, &run.run_id).await?;
    anyhow::ensure!(
        completed.status == DaemonRunStatus::Completed
            && completed
                .outputs
                .iter()
                .any(|output| output.content.contains("GOOGLE_TOOL_OK")),
        "Google forced-tool run failed or missed marker: {}",
        serde_json::to_string_pretty(&completed)?
    );
    let events = get_session_events(&client, &harness.base_url, session_id).await?;
    anyhow::ensure!(
        tool_succeeded(&events, "read_file"),
        "Google forced-tool run did not use read_file successfully: {}",
        serde_json::to_string_pretty(&events)?
    );
    let provider_request = get_run_debug_artifact(
        &client,
        &harness.base_url,
        &run.run_id,
        "turn-0001-attempt-0001-provider-request",
    )
    .await?;
    anyhow::ensure!(
        provider_request["body"]["toolConfig"]["functionCallingConfig"]["mode"] == "ANY"
            && provider_request["body"]["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"]
                == json!(["read_file"]),
        "Google forced-tool request did not pin read_file: {}",
        serde_json::to_string_pretty(&provider_request)?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn google_live_direct_png_vision_smoke() -> Result<()> {
    let Some(harness) = start_google_live_daemon().await? else {
        return Ok(());
    };
    let client = Client::new();
    set_debug_level(
        &client,
        &harness.base_url,
        kheish_runtime::DebugCaptureLevel::Full,
    )
    .await?;
    let session_id = "google-live-direct-png-vision";
    create_session(&client, &harness.base_url, session_id).await?;
    let asset = import_asset(
        &client,
        &harness.base_url,
        "red-square.png",
        "image/png",
        &sample_png_bytes()?,
    )
    .await?;
    let run = submit_run_request(
        &client,
        &harness.base_url,
        session_id,
        SubmitInputRequest {
            source_plugin: None,
            source_kind: None,
            actor_id: None,
            provider: None,
            content: String::new(),
            input_items: vec![
                SubmitInputItemRequest::Text {
                    text: "Return JSON for this schema. Inspect the attached image. Set `marker` to exactly `GOOGLE_VISION_RED`. Set `dominant_color` to exactly `red`. Use no other fields.".to_string(),
                },
                SubmitInputItemRequest::AssetReference {
                    asset_id: asset.asset_id,
                },
            ],
            attachments: Vec::new(),
            generation: Some(ModelGenerationConfig {
                tool_choice: ToolChoice::None,
                allow_parallel_tool_calls: false,
                response_format: ResponseFormat::StructuredJson {
                    schema: StructuredFieldSchema {
                        kind: StructuredValueKind::Object,
                        fields: BTreeMap::from([
                            (
                                "marker".to_string(),
                                StructuredFieldSchema::new(StructuredValueKind::String),
                            ),
                            (
                                "dominant_color".to_string(),
                                StructuredFieldSchema::new(StructuredValueKind::String),
                            ),
                        ]),
                        optional_fields: BTreeMap::new(),
                        items: None,
                    },
                },
                ..ModelGenerationConfig::default()
            }),
            completion_requirements: None,
            metadata: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
            reply_address: None,
        },
    )
    .await?;
    let completed = wait_for_run(&client, &harness.base_url, &run.run_id).await?;
    anyhow::ensure!(
        completed.status == DaemonRunStatus::Completed,
        "Google direct vision run failed: {}",
        serde_json::to_string_pretty(&completed)?
    );
    let output = completed
        .outputs
        .last()
        .map(|record| record.content.trim().to_string())
        .ok_or_else(|| anyhow!("Google direct vision run did not emit output"))?;
    let value: Value =
        serde_json::from_str(&output).map_err(|error| anyhow!("invalid JSON {output}: {error}"))?;
    anyhow::ensure!(
        value["marker"].as_str() == Some("GOOGLE_VISION_RED")
            && value["dominant_color"]
                .as_str()
                .is_some_and(|color| color.eq_ignore_ascii_case("red")),
        "unexpected Google direct vision output: {output}"
    );
    let provider_request = get_run_debug_artifact(
        &client,
        &harness.base_url,
        &run.run_id,
        "turn-0001-attempt-0001-provider-request",
    )
    .await?;
    let user_parts = provider_request["body"]["contents"]
        .as_array()
        .and_then(|contents| contents.iter().find(|content| content["role"] == "user"))
        .and_then(|content| content["parts"].as_array())
        .ok_or_else(|| anyhow!("missing Google user parts in provider request"))?;
    anyhow::ensure!(
        user_parts
            .iter()
            .any(|part| part["inlineData"]["mimeType"] == "image/png"),
        "Google direct vision request did not include PNG inline data: {}",
        serde_json::to_string_pretty(&provider_request)?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn google_live_runtime_capabilities_exclude_unsupported_media() -> Result<()> {
    let Some(harness) = start_google_live_daemon().await? else {
        return Ok(());
    };
    let client = Client::new();
    let runtime: Value = client
        .get(format!("{}/v1/runtime", harness.base_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let capabilities = &runtime["default_route"]["capabilities"];
    anyhow::ensure!(
        capabilities["multimodal_input"] == true
            && capabilities["image_generation"] == true
            && capabilities["image_edit"] == true
            && capabilities["native_web_search"] == false
            && capabilities["audio_generation"] == false
            && capabilities["transcription"] == false,
        "Google runtime capabilities did not expose the supported/unsupported media matrix: {}",
        serde_json::to_string_pretty(&runtime)?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn google_live_generate_audio_fails_closed() -> Result<()> {
    let Some(harness) = start_google_live_daemon().await? else {
        return Ok(());
    };
    let client = Client::new();
    let session_id = "google-live-generate-audio-unsupported";
    create_session(&client, &harness.base_url, session_id).await?;
    let run = submit_run_request(
        &client,
        &harness.base_url,
        session_id,
        SubmitInputRequest {
            source_plugin: None,
            source_kind: None,
            actor_id: None,
            provider: None,
            content: "Call generate_audio with input exactly `Google audio should be unavailable`, voice `alloy`, and format `mp3`.".to_string(),
            input_items: Vec::new(),
            attachments: Vec::new(),
            generation: Some(ModelGenerationConfig {
                tool_choice: ToolChoice::Specific {
                    name: "generate_audio".to_string(),
                },
                allow_parallel_tool_calls: false,
                ..ModelGenerationConfig::default()
            }),
            completion_requirements: None,
            metadata: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
            reply_address: None,
        },
    )
    .await?;
    let completed = wait_for_run(&client, &harness.base_url, &run.run_id).await?;
    anyhow::ensure!(
        completed.status == DaemonRunStatus::Failed
            && completed.error.as_deref().is_some_and(|error| error
                .contains("Google tool choice requested unavailable tool `generate_audio`")),
        "Google generate_audio unsupported run did not fail closed: {}",
        serde_json::to_string_pretty(&completed)?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn google_live_transcription_derivation_fails_closed() -> Result<()> {
    let Some(harness) = start_google_live_daemon().await? else {
        return Ok(());
    };
    let client = Client::new();
    let asset = import_asset(
        &client,
        &harness.base_url,
        "unsupported-google-stt.wav",
        "audio/wav",
        &sample_wav_bytes(),
    )
    .await?;
    let response = client
        .post(format!("{}/v1/derivations", harness.base_url))
        .json(&CreateDerivationRequest {
            profile: DerivationProfile::CanonicalText,
            subject: DerivationSubject::Asset {
                asset_id: asset.asset_id,
            },
            transcription: Some(DerivationTranscriptionOptions {
                prompt: Some("transcribe this short audio".to_string()),
                language: Some("en".to_string()),
                timestamp_granularities: Vec::new(),
                diarization: false,
            }),
        })
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    anyhow::ensure!(
        status == reqwest::StatusCode::BAD_REQUEST
            && body
                .contains("transcription options require a configured audio transcription backend"),
        "Google transcription derivation did not fail closed with expected 400: status={status}, body={body}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn google_live_attachment_inputs_cover_supported_file_types() -> Result<()> {
    let Some(harness) = start_google_live_daemon().await? else {
        return Ok(());
    };
    let client = Client::new();
    set_debug_level(
        &client,
        &harness.base_url,
        kheish_runtime::DebugCaptureLevel::Full,
    )
    .await?;
    let session_id = "google-live-attachments";
    create_session(&client, &harness.base_url, session_id).await?;
    let csv = import_asset(
        &client,
        &harness.base_url,
        "sample.csv",
        "text/csv",
        &sample_csv_bytes(),
    )
    .await?;
    let dxf = import_asset(
        &client,
        &harness.base_url,
        "sample.dxf",
        "application/dxf",
        &sample_dxf_bytes(),
    )
    .await?;
    anyhow::ensure!(
        dxf.preview_image_media_type.as_deref() == Some("image/png")
            && dxf.preview_image_uri.is_some(),
        "DXF asset is missing preview metadata: {}",
        serde_json::to_string_pretty(&dxf)?
    );
    let run = submit_run_request(
        &client,
        &harness.base_url,
        session_id,
        SubmitInputRequest {
            source_plugin: None,
            source_kind: None,
            actor_id: None,
            provider: None,
            content: String::new(),
            input_items: vec![
                SubmitInputItemRequest::Text {
                    text: "Read the attached CSV and DXF documents and reply exactly `3.2 / 3.20 | 6.5,3.4`.".to_string(),
                },
                SubmitInputItemRequest::AssetReference {
                    asset_id: csv.asset_id.clone(),
                },
                SubmitInputItemRequest::AssetReference {
                    asset_id: dxf.asset_id.clone(),
                },
            ],
            attachments: Vec::new(),
            generation: Some(ModelGenerationConfig {
                tool_choice: ToolChoice::None,
                allow_parallel_tool_calls: false,
                ..ModelGenerationConfig::default()
            }),
            completion_requirements: None,
            metadata: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
            reply_address: None,
        },
    )
    .await?;
    let completed = wait_for_run(&client, &harness.base_url, &run.run_id).await?;
    anyhow::ensure!(
        completed.status == DaemonRunStatus::Completed,
        "Google attachment run failed: {}",
        serde_json::to_string_pretty(&completed)?
    );
    let session = get_session(&client, &harness.base_url, session_id).await?;
    anyhow::ensure!(
        session
            .outputs
            .iter()
            .any(|output| output.content.contains("3.2 / 3.20 | 6.5,3.4")),
        "Google attachment output missing DXF/CSV marker: {}",
        serde_json::to_string_pretty(&session)?
    );
    let provider_request = get_run_debug_artifact(
        &client,
        &harness.base_url,
        &run.run_id,
        "turn-0001-attempt-0001-provider-request",
    )
    .await?;
    let user_parts = provider_request["body"]["contents"]
        .as_array()
        .and_then(|contents| contents.iter().find(|content| content["role"] == "user"))
        .and_then(|content| content["parts"].as_array())
        .ok_or_else(|| anyhow!("missing Google user parts in provider request"))?;
    anyhow::ensure!(
        user_parts.iter().any(|part| {
            part["inlineData"]["mimeType"] == "image/png"
                && (part["inlineData"]["data"]
                    .as_str()
                    .is_some_and(|data| !data.is_empty())
                    || part["inlineData"]["data"]["redacted"] == "<redacted base64>")
        }),
        "Google provider request did not include the DXF preview image: {}",
        serde_json::to_string_pretty(&provider_request)?
    );
    anyhow::ensure!(
        user_parts.iter().any(|part| {
            part["text"]
                .as_str()
                .is_some_and(|text| text.contains("Document attachment: sample.dxf"))
        }),
        "Google provider request did not include the DXF text payload: {}",
        serde_json::to_string_pretty(&provider_request)?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn google_live_can_generate_image_outputs() -> Result<()> {
    let Some(harness) = start_google_live_daemon().await? else {
        return Ok(());
    };
    run_generated_image_case(&harness.base_url, &harness.state_root).await
}

#[tokio::test(flavor = "multi_thread")]
async fn google_live_can_edit_png_image_outputs() -> Result<()> {
    let Some(harness) = start_google_live_daemon().await? else {
        return Ok(());
    };
    run_edited_image_case(
        &harness.base_url,
        &harness.state_root,
        "google-live-edited-image-png",
        "sample-plan.png",
        "image/png",
        &sample_png_bytes()?,
        "GOOGLE_EDITED_IMAGE_PNG_READY",
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn google_live_can_edit_jpeg_image_outputs() -> Result<()> {
    let Some(harness) = start_google_live_daemon().await? else {
        return Ok(());
    };
    run_edited_image_case(
        &harness.base_url,
        &harness.state_root,
        "google-live-edited-image-jpeg",
        "sample-plan.jpg",
        "image/jpeg",
        &sample_jpeg_bytes()?,
        "GOOGLE_EDITED_IMAGE_JPEG_READY",
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn google_live_can_edit_with_multiple_source_images() -> Result<()> {
    let Some(harness) = start_google_live_daemon().await? else {
        return Ok(());
    };
    run_multi_image_edit_case(&harness.base_url, &harness.state_root).await
}
