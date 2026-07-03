use std::collections::BTreeMap;
use std::fmt::{Debug, Formatter};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use futures_util::StreamExt;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

use crate::model::{
    ModelEventSink, ModelFinishReason, ModelProvider, ModelRuntimeRequest, ModelStreamEvent,
    ProviderError, ResponseFormat, ToolChoice,
};
use crate::observability::{
    DebugArtifact, RuntimeObserver, external_action_trace_with_grant_id,
    failed_external_action_outcome, safe_url_audit_target, safe_url_debug_target,
};
use crate::{
    DebugArtifactFormat, DebugCaptureLevel, NoopObserver, headers_payload_for_level,
    provider_payload_for_level,
};
use kheish_auth::{RequestAuthProvider, ResolvedAuthMaterial};
use kheish_codec::digest_json_value;
use kheish_types::{InputContentPart, ToolCallRecord, model_max_output_tokens};

use super::attachments::{
    AttachmentRenderCache, image_edit_attachment_hint_text, load_attachment_preview_image,
    load_document_attachment_text, load_image_attachment,
};
use super::errors::sanitize_upstream_error_message;
use super::prompt::{NormalizedConversationItem, normalize_provider_prompt};
use super::schema::structured_schema_json;

const DEFAULT_GOOGLE_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_GOOGLE_MODEL: &str = "gemini-2.5-flash";
const DEFAULT_GOOGLE_IMAGE_MODEL: &str = "gemini-2.5-flash-image";
const MAX_GOOGLE_IMAGE_RESPONSE_BYTES: usize = 24 * 1024 * 1024;

/// Configuration for the Google Gemini provider adapter.
#[derive(Clone)]
pub struct GoogleProviderConfig {
    /// The model identifier.
    pub model: String,
    /// The API key used by the Gemini Developer API.
    pub api_key: Option<String>,
    /// Optional request-scoped auth provider used to resolve credentials dynamically.
    pub request_auth_provider: Option<Arc<dyn RequestAuthProvider>>,
    /// The Gemini API base URL.
    pub base_url: String,
    /// Default output token ceiling when the generation config does not override it.
    pub default_max_output_tokens: u32,
    /// Optional daemon-owned asset root used to resolve opaque attachment URIs.
    pub asset_root: Option<PathBuf>,
    /// Shared in-process cache for prepared attachment payloads.
    pub(crate) attachment_cache: AttachmentRenderCache,
}

impl GoogleProviderConfig {
    /// Creates one Google configuration with a static API key.
    pub fn new(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        let model = resolve_google_model(&model.into());
        Self {
            default_max_output_tokens: model_max_output_tokens(&model).default,
            model,
            api_key: Some(api_key.into()),
            request_auth_provider: None,
            base_url: DEFAULT_GOOGLE_BASE_URL.to_string(),
            asset_root: None,
            attachment_cache: AttachmentRenderCache::default(),
        }
    }

    /// Loads the API key from one environment variable.
    pub fn from_env(
        model: impl Into<String>,
        env_var: impl AsRef<str>,
    ) -> Result<Self, ProviderError> {
        let env_var = env_var.as_ref();
        let api_key = std::env::var(env_var).map_err(|_| ProviderError {
            message: format!("missing Google API key in environment variable {env_var}"),
            retryable: false,
            retry_after_ms: None,
        })?;
        Ok(Self::new(model, api_key))
    }

    /// Creates one Google configuration backed by a dynamic auth provider.
    pub fn with_request_auth_provider(
        model: impl Into<String>,
        request_auth_provider: Arc<dyn RequestAuthProvider>,
    ) -> Self {
        let model = resolve_google_model(&model.into());
        Self {
            default_max_output_tokens: model_max_output_tokens(&model).default,
            model,
            api_key: None,
            request_auth_provider: Some(request_auth_provider),
            base_url: DEFAULT_GOOGLE_BASE_URL.to_string(),
            asset_root: None,
            attachment_cache: AttachmentRenderCache::default(),
        }
    }
}

impl Debug for GoogleProviderConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleProviderConfig")
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field(
                "request_auth_provider",
                &self.request_auth_provider.as_ref().map(|_| "<configured>"),
            )
            .field("base_url", &self.base_url)
            .field("default_max_output_tokens", &self.default_max_output_tokens)
            .field("asset_root", &self.asset_root)
            .field("attachment_cache", &"<configured>")
            .finish()
    }
}

/// Backward-compatible alias used by daemon image backends.
pub type GoogleImageProviderConfig = GoogleProviderConfig;

/// Google Gemini provider implemented through `models/{model}:generateContent`.
pub struct GoogleProvider {
    client: Client,
    config: GoogleProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

impl GoogleProvider {
    /// Builds one Google provider using a dedicated HTTP client.
    pub fn new(config: GoogleProviderConfig) -> Result<Self, ProviderError> {
        Self::with_observer(config, Arc::new(NoopObserver))
    }

    /// Builds one Google provider with runtime observation hooks enabled.
    pub fn with_observer(
        config: GoogleProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        let client = Client::builder().build().map_err(|error| ProviderError {
            message: format!("failed to build Google HTTP client: {error}"),
            retryable: false,
            retry_after_ms: None,
        })?;
        Ok(Self {
            client,
            config,
            observer,
        })
    }

    fn debug_level(&self) -> DebugCaptureLevel {
        self.observer.debug_level()
    }

    fn record_provider_request(
        &self,
        request: &ModelRuntimeRequest,
        endpoint: &str,
        headers: &HeaderMap,
        body: &Value,
        grant_id: Option<String>,
    ) -> Result<(), ProviderError> {
        record_google_external_request(&self.observer, endpoint, body, grant_id)?;
        let level = self.debug_level();
        if !level.is_enabled() {
            return Ok(());
        }
        self.observer.record_debug_artifact(DebugArtifact::new(
            level,
            Some(request.turn),
            Some(request.attempt),
            &format!("{}provider-request", request.kind.artifact_prefix()),
            DebugArtifactFormat::Json,
            json!({
                "provider": "google",
                "method": "POST",
                "url": safe_url_debug_target(endpoint),
                "headers": headers_payload_for_level(level, headers),
                "body": provider_payload_for_level(level, body),
            }),
        ));
        Ok(())
    }

    fn record_provider_response(
        &self,
        request: &ModelRuntimeRequest,
        target: &str,
        status: u16,
        headers: &HeaderMap,
        body: &Value,
        grant_id: Option<String>,
    ) -> Result<(), ProviderError> {
        record_google_external_response(&self.observer, target, status, body, grant_id)?;
        let level = self.debug_level();
        if !level.is_enabled() {
            return Ok(());
        }
        self.observer.record_debug_artifact(DebugArtifact::new(
            level,
            Some(request.turn),
            Some(request.attempt),
            &format!("{}provider-response", request.kind.artifact_prefix()),
            DebugArtifactFormat::Json,
            json!({
                "provider": "google",
                "status": status,
                "headers": headers_payload_for_level(level, headers),
                "body": provider_payload_for_level(level, body),
            }),
        ));
        Ok(())
    }

    fn record_provider_failure(
        &self,
        target: impl Into<String>,
        message: &str,
        grant_id: Option<String>,
    ) -> Result<(), ProviderError> {
        record_google_external_failure(&self.observer, target, message, grant_id)
    }

    async fn auth_material(
        &self,
        force_refresh: bool,
    ) -> Result<ResolvedAuthMaterial, ProviderError> {
        if let Some(provider) = &self.config.request_auth_provider {
            let result = if force_refresh {
                provider.refresh().await
            } else {
                provider.resolve().await
            };
            return result.map_err(|error| ProviderError {
                message: format!("failed to resolve Google auth material: {error}"),
                retryable: false,
                retry_after_ms: None,
            });
        }
        let api_key = self.config.api_key.clone().ok_or_else(|| ProviderError {
            message: "missing Google API key".to_string(),
            retryable: false,
            retry_after_ms: None,
        })?;
        let mut headers = BTreeMap::new();
        headers.insert("x-goog-api-key".to_string(), api_key);
        Ok(ResolvedAuthMaterial {
            headers,
            base_url_override: None,
            grant_id: None,
            lease_id: None,
        })
    }

    async fn ensure_auth_material_active(
        &self,
        material: &ResolvedAuthMaterial,
    ) -> Result<(), ProviderError> {
        if let Some(provider) = &self.config.request_auth_provider {
            provider
                .ensure_active(material)
                .await
                .map_err(|error| ProviderError {
                    message: format!("Google auth material is no longer active: {error}"),
                    retryable: false,
                    retry_after_ms: None,
                })?;
        }
        Ok(())
    }

    fn headers_from_material(
        &self,
        material: &ResolvedAuthMaterial,
    ) -> Result<HeaderMap, ProviderError> {
        headers_from_material(material)
    }

    fn build_request_body(&self, request: &ModelRuntimeRequest) -> Result<Value, ProviderError> {
        let effective_model = effective_model_name(request, &self.config.model);
        let normalized = normalize_provider_prompt(&request.prompt);
        let mut contents = Vec::new();
        for item in normalized.conversation {
            match item {
                NormalizedConversationItem::UserMessage {
                    content,
                    content_parts,
                    attachments,
                    ..
                } => {
                    let parts = google_user_parts(
                        &content,
                        &content_parts,
                        &attachments,
                        self.config.asset_root.as_deref(),
                        &self.config.attachment_cache,
                        google_model_supports_image_input(effective_model),
                    )
                    .map_err(|error| ProviderError {
                        message: format!("failed to prepare Google user parts: {error}"),
                        retryable: false,
                        retry_after_ms: None,
                    })?;
                    if !parts.is_empty() {
                        contents.push(json!({
                            "role": "user",
                            "parts": parts,
                        }));
                    }
                }
                NormalizedConversationItem::AssistantMessage { content, .. } => {
                    if !content.trim().is_empty() {
                        contents.push(json!({
                            "role": "model",
                            "parts": [{ "text": content }],
                        }));
                    }
                }
                NormalizedConversationItem::AssistantToolCalls { calls, .. } => {
                    if !google_model_supports_function_calling(effective_model) {
                        return Err(provider_error(format!(
                            "Google model '{effective_model}' does not support function calling"
                        )));
                    }
                    let parts = calls
                        .into_iter()
                        .map(|call| {
                            json!({
                                "functionCall": {
                                    "id": call.id,
                                    "name": call.name,
                                    "args": call.input,
                                }
                            })
                        })
                        .collect::<Vec<_>>();
                    if !parts.is_empty() {
                        contents.push(json!({
                            "role": "model",
                            "parts": parts,
                        }));
                    }
                }
                NormalizedConversationItem::ToolResults { results } => {
                    if !google_model_supports_function_calling(effective_model) {
                        return Err(provider_error(format!(
                            "Google model '{effective_model}' does not support function calling"
                        )));
                    }
                    let parts = results
                        .into_iter()
                        .map(google_function_response_part)
                        .collect::<Vec<_>>();
                    if !parts.is_empty() {
                        contents.push(json!({
                            "role": "user",
                            "parts": parts,
                        }));
                    }
                }
            }
        }

        let has_tools = !request.available_tools.is_empty()
            && !matches!(request.generation.tool_choice, ToolChoice::None);
        if has_tools && !google_model_supports_function_calling(effective_model) {
            return Err(provider_error(format!(
                "Google model '{effective_model}' does not support function calling"
            )));
        }
        if request.available_tools.is_empty()
            && matches!(
                request.generation.tool_choice,
                ToolChoice::Required | ToolChoice::Specific { .. }
            )
        {
            return Err(provider_error(
                "Google tool choice requires at least one available tool",
            ));
        }
        if let ToolChoice::Specific { name } = &request.generation.tool_choice {
            if !request
                .available_tools
                .iter()
                .any(|tool| tool.name == *name)
            {
                return Err(provider_error(format!(
                    "Google tool choice requested unavailable tool `{name}`"
                )));
            }
        }

        let mut body = serde_json::Map::new();
        body.insert("contents".to_string(), Value::Array(contents));

        if !normalized.instructions.is_empty() {
            body.insert(
                "systemInstruction".to_string(),
                json!({
                    "parts": [{
                        "text": normalized.instructions.join("\n\n"),
                    }]
                }),
            );
        }

        if has_tools {
            let declarations = request
                .available_tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": google_function_parameters_schema(&tool.input_schema),
                    })
                })
                .collect::<Vec<_>>();
            body.insert(
                "tools".to_string(),
                json!([{
                    "functionDeclarations": declarations,
                }]),
            );
            if let Some(tool_config) = google_tool_config(
                &request.generation.tool_choice,
                &request.generation.response_format,
            ) {
                body.insert("toolConfig".to_string(), tool_config);
            }
        }

        let mut generation_config = serde_json::Map::new();
        generation_config.insert(
            "maxOutputTokens".to_string(),
            Value::Number(
                request
                    .generation
                    .max_output_tokens
                    .unwrap_or(model_max_output_tokens(effective_model).default)
                    .into(),
            ),
        );
        if let Some(temperature) = request.generation.temperature {
            if let Some(number) = serde_json::Number::from_f64(temperature as f64) {
                generation_config.insert("temperature".to_string(), Value::Number(number));
            }
        }
        if let ResponseFormat::StructuredJson { schema } = &request.generation.response_format {
            generation_config.insert(
                "responseMimeType".to_string(),
                Value::String("application/json".to_string()),
            );
            generation_config.insert(
                "responseJsonSchema".to_string(),
                structured_schema_json(schema),
            );
        }
        if !generation_config.is_empty() {
            body.insert(
                "generationConfig".to_string(),
                Value::Object(generation_config),
            );
        }

        Ok(Value::Object(body))
    }
}

fn google_external_action_target(endpoint: &str) -> String {
    format!("google:{}", safe_url_audit_target(endpoint))
}

fn record_google_external_request(
    observer: &Arc<dyn RuntimeObserver>,
    endpoint: &str,
    body: &Value,
    grant_id: Option<String>,
) -> Result<(), ProviderError> {
    observer
        .record_external_action(external_action_trace_with_grant_id(
            "request",
            "model_provider",
            google_external_action_target(endpoint),
            Some(digest_json_value(body).unwrap_or_else(|_| "unknown".to_string())),
            None,
            None,
            grant_id,
        ))
        .map_err(provider_audit_error)
}

fn record_google_external_response(
    observer: &Arc<dyn RuntimeObserver>,
    target: &str,
    status: u16,
    body: &Value,
    grant_id: Option<String>,
) -> Result<(), ProviderError> {
    observer
        .record_external_action(external_action_trace_with_grant_id(
            "response",
            "model_provider",
            target.to_string(),
            None,
            Some(digest_json_value(body).unwrap_or_else(|_| "unknown".to_string())),
            Some(status.to_string()),
            grant_id,
        ))
        .map_err(provider_audit_error)
}

fn record_google_external_failure(
    observer: &Arc<dyn RuntimeObserver>,
    target: impl Into<String>,
    message: &str,
    grant_id: Option<String>,
) -> Result<(), ProviderError> {
    observer
        .record_external_action(external_action_trace_with_grant_id(
            "response",
            "model_provider",
            target,
            None,
            None,
            Some(failed_external_action_outcome(message)),
            grant_id,
        ))
        .map_err(provider_audit_error)
}

fn record_google_media_provider_request(
    observer: &Arc<dyn RuntimeObserver>,
    artifact_name: &str,
    endpoint: &str,
    headers: &HeaderMap,
    body: &Value,
    grant_id: Option<String>,
) -> Result<(), ProviderError> {
    record_google_external_request(observer, endpoint, body, grant_id)?;
    let level = observer.debug_level();
    if level.is_enabled() {
        observer.record_debug_artifact(DebugArtifact::new(
            level,
            None,
            None,
            artifact_name,
            DebugArtifactFormat::Json,
            json!({
                "provider": "google",
                "method": "POST",
                "url": safe_url_debug_target(endpoint),
                "headers": headers_payload_for_level(level, headers),
                "body": provider_payload_for_level(level, body),
            }),
        ));
    }
    Ok(())
}

fn record_google_media_provider_response(
    observer: &Arc<dyn RuntimeObserver>,
    artifact_name: &str,
    target: &str,
    status: u16,
    headers: &HeaderMap,
    body: &Value,
    grant_id: Option<String>,
) -> Result<(), ProviderError> {
    record_google_external_response(observer, target, status, body, grant_id)?;
    let level = observer.debug_level();
    if level.is_enabled() {
        observer.record_debug_artifact(DebugArtifact::new(
            level,
            None,
            None,
            artifact_name,
            DebugArtifactFormat::Json,
            json!({
                "provider": "google",
                "status": status,
                "headers": headers_payload_for_level(level, headers),
                "body": provider_payload_for_level(level, body),
            }),
        ));
    }
    Ok(())
}

#[async_trait]
impl ModelProvider for GoogleProvider {
    async fn stream(
        &self,
        request: ModelRuntimeRequest,
        sink: ModelEventSink,
    ) -> std::result::Result<(), ProviderError> {
        let effective_model = effective_model_name(&request, &self.config.model).to_string();
        let body = self.build_request_body(&request)?;
        let mut force_refresh = false;
        let (response, response_target, response_grant_id) = loop {
            let auth_material = self.auth_material(force_refresh).await?;
            let grant_id = auth_material.grant_id.clone();
            let headers = self.headers_from_material(&auth_material)?;
            let endpoint = google_generate_content_endpoint(
                auth_material
                    .base_url_override
                    .as_deref()
                    .unwrap_or(self.config.base_url.as_str()),
                &effective_model,
            )?;
            self.ensure_auth_material_active(&auth_material).await?;
            self.record_provider_request(&request, &endpoint, &headers, &body, grant_id.clone())?;
            let response = match self
                .client
                .post(&endpoint)
                .headers(headers)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    let mapped = map_transport_error(error);
                    self.record_provider_failure(
                        google_external_action_target(&endpoint),
                        &mapped.message,
                        grant_id,
                    )?;
                    return Err(mapped);
                }
            };
            if response.status() == StatusCode::UNAUTHORIZED
                && self.config.request_auth_provider.is_some()
                && !force_refresh
            {
                self.record_provider_failure(
                    google_external_action_target(&endpoint),
                    "401-refresh",
                    grant_id,
                )?;
                force_refresh = true;
                continue;
            }
            break (response, google_external_action_target(&endpoint), grant_id);
        };

        let status = response.status();
        let response_headers = response.headers().clone();
        let payload = response.json::<Value>().await.map_err(|error| {
            let mapped = ProviderError {
                message: format!("failed to decode Google response: {error}"),
                retryable: false,
                retry_after_ms: None,
            };
            self.record_provider_failure(
                &response_target,
                &mapped.message,
                response_grant_id.clone(),
            )
            .err()
            .unwrap_or(mapped)
        })?;
        self.record_provider_response(
            &request,
            &response_target,
            status.as_u16(),
            &response_headers,
            &payload,
            response_grant_id.clone(),
        )?;
        if !status.is_success() {
            return Err(map_payload_http_error(status, &response_headers, &payload));
        }

        if let Some(message_id) = payload
            .get("responseId")
            .or_else(|| payload.get("response_id"))
            .and_then(Value::as_str)
        {
            sink.emit(ModelStreamEvent::MessageId {
                value: message_id.to_string(),
            })
            .map_err(map_sink_error)?;
        }

        let candidates = payload
            .get("candidates")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                google_response_block_reason(&payload)
                    .map(|reason| provider_error(google_blocked_message("response", &reason)))
                    .unwrap_or_else(|| provider_error("Google response did not include candidates"))
            })
            .map_err(|error| {
                self.record_provider_failure(
                    &response_target,
                    &error.message,
                    response_grant_id.clone(),
                )
                .err()
                .unwrap_or(error)
            })?;
        let candidate = candidates
            .first()
            .ok_or_else(|| {
                google_response_block_reason(&payload)
                    .map(|reason| provider_error(google_blocked_message("response", &reason)))
                    .unwrap_or_else(|| {
                        provider_error("Google response did not include any candidates")
                    })
            })
            .map_err(|error| {
                self.record_provider_failure(
                    &response_target,
                    &error.message,
                    response_grant_id.clone(),
                )
                .err()
                .unwrap_or(error)
            })?;
        let candidate_block_reason = google_candidate_block_reason(candidate)
            .or_else(|| google_response_block_reason(&payload));

        if let Some(usage) = google_usage_snapshot(&payload) {
            sink.emit(ModelStreamEvent::Usage { usage })
                .map_err(map_sink_error)?;
        }

        let mut text = String::new();
        if let Some(parts) = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        {
            let response_id = payload
                .get("responseId")
                .or_else(|| payload.get("response_id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let mut function_call_index = 0usize;
            for part in parts {
                if let Some(delta) = part.get("text").and_then(Value::as_str) {
                    text.push_str(delta);
                }
                if let Some(call) = google_function_call_from_part(
                    part,
                    function_call_index,
                    response_id.as_deref(),
                )
                .map_err(|error| {
                    self.record_provider_failure(
                        &response_target,
                        &error.message,
                        response_grant_id.clone(),
                    )
                    .err()
                    .unwrap_or(error)
                })? {
                    function_call_index += 1;
                    sink.emit(ModelStreamEvent::ToolCall { call })
                        .map_err(map_sink_error)?;
                }
            }
        }

        match &request.generation.response_format {
            ResponseFormat::Text => {
                if !text.is_empty() {
                    sink.emit(ModelStreamEvent::TextDelta { text })
                        .map_err(map_sink_error)?;
                }
            }
            ResponseFormat::StructuredJson { .. } => {
                if text.trim().is_empty() {
                    let error = candidate_block_reason
                        .as_deref()
                        .map(|reason| {
                            provider_error(google_blocked_message("structured response", reason))
                        })
                        .unwrap_or_else(|| {
                            provider_error(
                                "Google structured response did not include a JSON text payload",
                            )
                        });
                    self.record_provider_failure(
                        &response_target,
                        &error.message,
                        response_grant_id.clone(),
                    )?;
                    return Err(error);
                }
                let value = serde_json::from_str::<Value>(text.trim()).map_err(|error| {
                    let mapped = provider_error(format!(
                        "Google structured response was not valid JSON: {error}"
                    ));
                    self.record_provider_failure(
                        &response_target,
                        &mapped.message,
                        response_grant_id.clone(),
                    )
                    .err()
                    .unwrap_or(mapped)
                })?;
                sink.emit(ModelStreamEvent::StructuredOutput { value })
                    .map_err(map_sink_error)?;
            }
        }

        let finish_reason = candidate
            .get("finishReason")
            .or_else(|| candidate.get("finish_reason"))
            .and_then(Value::as_str)
            .map(google_finish_reason)
            .unwrap_or(ModelFinishReason::Completed);
        sink.emit(ModelStreamEvent::Stop {
            reason: finish_reason,
        })
        .map_err(map_sink_error)?;
        Ok(())
    }
}

/// One Google image generation request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoogleImageGenerationRequest {
    /// The text prompt used to generate one image.
    pub prompt: String,
    /// The number of generated images to request.
    pub count: u32,
    /// Optional size override expressed as WIDTHxHEIGHT.
    pub size: Option<String>,
}

/// One Google image input used for editing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoogleImageEditInput {
    /// Original file name used when logging or debugging requests.
    pub file_name: String,
    /// Media type sent to Gemini.
    pub media_type: String,
    /// Raw image bytes.
    pub bytes: Vec<u8>,
}

/// One Google image edit request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoogleImageEditRequest {
    /// The text instruction that describes the requested edits.
    pub prompt: String,
    /// Ordered source images supplied to Gemini.
    pub images: Vec<GoogleImageEditInput>,
    /// The number of edited images to request.
    pub count: u32,
    /// Optional size override expressed as WIDTHxHEIGHT.
    pub size: Option<String>,
}

/// One normalized Google-generated image payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoogleGeneratedImage {
    /// The MIME type returned by Gemini.
    pub media_type: String,
    /// The decoded image bytes.
    pub bytes: Vec<u8>,
}

/// One normalized Google image generation response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoogleImageGenerationResponse {
    /// The concrete provider model that produced the images.
    pub model: String,
    /// The decoded image payloads.
    pub images: Vec<GoogleGeneratedImage>,
    /// Optional descriptive text returned alongside image parts.
    pub text: Option<String>,
}

/// Google-backed image generator.
pub struct GoogleImageGenerator {
    client: Client,
    config: GoogleImageProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

impl GoogleImageGenerator {
    /// Creates one Google image generator.
    pub fn new(
        config: GoogleImageProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        let client = Client::builder().build().map_err(|error| ProviderError {
            message: format!("failed to build Google HTTP client: {error}"),
            retryable: false,
            retry_after_ms: None,
        })?;
        Ok(Self {
            client,
            config,
            observer,
        })
    }

    /// Creates one Google image generator without runtime observation hooks.
    pub fn without_observer(config: GoogleImageProviderConfig) -> Result<Self, ProviderError> {
        Self::new(config, Arc::new(NoopObserver))
    }

    /// Generates one or more images through Gemini.
    pub async fn generate(
        &self,
        request: GoogleImageGenerationRequest,
    ) -> Result<GoogleImageGenerationResponse, ProviderError> {
        if request.prompt.trim().is_empty() {
            return Err(provider_error(
                "google image generation prompt must not be empty",
            ));
        }
        if request.count != 1 {
            return Err(provider_error(
                "google image generation currently supports count=1 only",
            ));
        }
        let body = build_google_image_body(&request.prompt, &[], request.size.as_deref())?;
        execute_google_image_request(
            &self.client,
            &self.config,
            &self.observer,
            "google-image-generation",
            body,
        )
        .await
    }
}

/// Google-backed image editor.
pub struct GoogleImageEditor {
    client: Client,
    config: GoogleImageProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

impl GoogleImageEditor {
    /// Creates one Google image editor.
    pub fn new(
        config: GoogleImageProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        let client = Client::builder().build().map_err(|error| ProviderError {
            message: format!("failed to build Google HTTP client: {error}"),
            retryable: false,
            retry_after_ms: None,
        })?;
        Ok(Self {
            client,
            config,
            observer,
        })
    }

    /// Creates one Google image editor without runtime observation hooks.
    pub fn without_observer(config: GoogleImageProviderConfig) -> Result<Self, ProviderError> {
        Self::new(config, Arc::new(NoopObserver))
    }

    /// Edits one or more images through Gemini.
    pub async fn edit(
        &self,
        request: GoogleImageEditRequest,
    ) -> Result<GoogleImageGenerationResponse, ProviderError> {
        if request.prompt.trim().is_empty() {
            return Err(provider_error("google image edit prompt must not be empty"));
        }
        if request.images.is_empty() {
            return Err(provider_error(
                "google image edit request must include at least one source image",
            ));
        }
        if request.count != 1 {
            return Err(provider_error(
                "google image editing currently supports count=1 only",
            ));
        }
        let body =
            build_google_image_body(&request.prompt, &request.images, request.size.as_deref())?;
        execute_google_image_request(
            &self.client,
            &self.config,
            &self.observer,
            "google-image-edit",
            body,
        )
        .await
    }
}

/// Normalizes one Google text model name to a supported default when absent.
pub fn resolve_google_model(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        DEFAULT_GOOGLE_MODEL.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Normalizes one Google image model name to an image-capable default route.
pub fn resolve_google_image_model(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return DEFAULT_GOOGLE_IMAGE_MODEL.to_string();
    }
    let canonical = trimmed.to_ascii_lowercase();
    if canonical.starts_with("imagen")
        || (canonical.starts_with("gemini") && canonical.contains("image"))
    {
        trimmed.to_string()
    } else {
        DEFAULT_GOOGLE_IMAGE_MODEL.to_string()
    }
}

fn effective_model_name<'a>(request: &'a ModelRuntimeRequest, configured: &'a str) -> &'a str {
    request.generation.model.as_deref().unwrap_or(configured)
}

fn google_tool_config(tool_choice: &ToolChoice, response_format: &ResponseFormat) -> Option<Value> {
    let mode = match tool_choice {
        ToolChoice::Auto => {
            if matches!(response_format, ResponseFormat::StructuredJson { .. }) {
                "VALIDATED"
            } else {
                "AUTO"
            }
        }
        ToolChoice::Required => "ANY",
        ToolChoice::Specific { .. } => "ANY",
        ToolChoice::None => "NONE",
    };
    let mut function_calling = serde_json::Map::new();
    function_calling.insert("mode".to_string(), Value::String(mode.to_string()));
    if let ToolChoice::Specific { name } = tool_choice {
        function_calling.insert("allowedFunctionNames".to_string(), json!([name]));
    }
    Some(json!({
        "functionCallingConfig": Value::Object(function_calling),
    }))
}

fn google_function_parameters_schema(schema: &Value) -> Value {
    google_supported_schema_fragment(schema)
}

fn google_supported_schema_fragment(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut supported = serde_json::Map::new();
            for (key, value) in map {
                match key.as_str() {
                    // Gemini function declarations accept a subset of OpenAPI schema fields.
                    // Keep the supported structural and validation keys, and drop unsupported
                    // extensions like `additionalProperties` that the live API rejects.
                    "type" | "format" | "title" | "description" | "nullable" | "enum"
                    | "required" | "propertyOrdering" | "default" | "example" | "minimum"
                    | "maximum" | "minLength" | "maxLength" | "pattern" | "minItems"
                    | "maxItems" | "minProperties" | "maxProperties" => {
                        supported.insert(key.clone(), value.clone());
                    }
                    "properties" => {
                        let property_map = value
                            .as_object()
                            .map(|properties| {
                                properties
                                    .iter()
                                    .map(|(name, child)| {
                                        (name.clone(), google_supported_schema_fragment(child))
                                    })
                                    .collect::<serde_json::Map<String, Value>>()
                            })
                            .unwrap_or_default();
                        supported.insert(key.clone(), Value::Object(property_map));
                    }
                    "items" => {
                        supported.insert(key.clone(), google_supported_schema_fragment(value));
                    }
                    "anyOf" => {
                        let variants = value
                            .as_array()
                            .map(|items| {
                                items
                                    .iter()
                                    .map(google_supported_schema_fragment)
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        supported.insert(key.clone(), Value::Array(variants));
                    }
                    "additionalProperties" => {}
                    _ => {}
                }
            }
            Value::Object(supported)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(google_supported_schema_fragment)
                .collect::<Vec<_>>(),
        ),
        other => other.clone(),
    }
}

fn google_user_parts(
    fallback_content: &str,
    content_parts: &[InputContentPart],
    attachments: &[kheish_types::AttachmentRef],
    asset_root: Option<&std::path::Path>,
    cache: &AttachmentRenderCache,
    include_document_previews: bool,
) -> anyhow::Result<Vec<Value>> {
    fn push_image_attachment_parts(
        parts: &mut Vec<Value>,
        attachment: &kheish_types::AttachmentRef,
        image: super::attachments::PreparedImageAttachment,
    ) {
        if let Some(text) = image_edit_attachment_hint_text(attachment) {
            parts.push(json!({ "text": text }));
        }
        parts.push(json!({
            "inlineData": {
                "mimeType": image.media_type,
                "data": image.base64_data,
            }
        }));
    }

    let mut parts = Vec::new();
    if !content_parts.is_empty() {
        for part in content_parts {
            match part {
                InputContentPart::Text { text } if !text.trim().is_empty() => {
                    parts.push(json!({ "text": text }));
                }
                InputContentPart::Text { .. } => {}
                InputContentPart::Attachment { attachment } => {
                    if let Some(image) = load_image_attachment(attachment, asset_root, cache)? {
                        push_image_attachment_parts(&mut parts, attachment, image);
                        continue;
                    }
                    if include_document_previews {
                        if let Some(preview) =
                            load_attachment_preview_image(attachment, asset_root, cache)?
                        {
                            parts.push(json!({
                                "inlineData": {
                                    "mimeType": preview.media_type,
                                    "data": preview.base64_data,
                                }
                            }));
                        }
                    }
                    if let Some(text) =
                        load_document_attachment_text(attachment, asset_root, cache)?
                    {
                        parts.push(json!({ "text": text }));
                    }
                }
            }
        }
        return Ok(parts);
    }

    if !fallback_content.trim().is_empty() {
        parts.push(json!({ "text": fallback_content }));
    }
    for attachment in attachments {
        if let Some(image) = load_image_attachment(attachment, asset_root, cache)? {
            push_image_attachment_parts(&mut parts, attachment, image);
            continue;
        }
        if include_document_previews {
            if let Some(preview) = load_attachment_preview_image(attachment, asset_root, cache)? {
                parts.push(json!({
                    "inlineData": {
                        "mimeType": preview.media_type,
                        "data": preview.base64_data,
                    }
                }));
            }
        }
        if let Some(text) = load_document_attachment_text(attachment, asset_root, cache)? {
            parts.push(json!({ "text": text }));
        }
    }
    Ok(parts)
}

fn google_function_response_part(result: kheish_types::ToolResultRecord) -> Value {
    let mut response = match result.output {
        Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("result".to_string(), other);
            map
        }
    };
    if result.is_error {
        response.insert("error".to_string(), Value::Bool(true));
    }
    json!({
        "functionResponse": {
            "id": result.call_id,
            "name": result
                .tool_name
                .unwrap_or_else(|| "tool".to_string()),
            "response": Value::Object(response),
        }
    })
}

fn google_function_call_from_part(
    part: &Value,
    call_index: usize,
    response_id: Option<&str>,
) -> Result<Option<ToolCallRecord>, ProviderError> {
    let Some(function_call) = part
        .get("functionCall")
        .or_else(|| part.get("function_call"))
    else {
        return Ok(None);
    };
    let explicit_id = function_call
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let id = explicit_id.map(str::to_string).unwrap_or_else(|| {
        match response_id.map(str::trim).filter(|id| !id.is_empty()) {
            Some(response_id) => format!("google-call-{response_id}-{call_index}"),
            None => format!("google-call-{call_index}"),
        }
    });
    let name = function_call
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| provider_error("Google function call was missing a name"))?
        .to_string();
    let input = match function_call.get("args") {
        Some(Value::Object(map)) => Value::Object(map.clone()),
        Some(Value::String(text)) => serde_json::from_str::<Value>(text).map_err(|error| {
            provider_error(format!(
                "Google function call arguments were not valid JSON: {error}"
            ))
        })?,
        Some(Value::Null) | None => json!({}),
        Some(other) => other.clone(),
    };
    Ok(Some(ToolCallRecord {
        id,
        name,
        input,
        assistant_message_id: None,
        assistant_provider_response_id: response_id.map(str::to_string),
    }))
}

fn google_usage_snapshot(payload: &Value) -> Option<kheish_types::ModelUsage> {
    let usage = payload
        .get("usageMetadata")
        .or_else(|| payload.get("usage_metadata"))?;
    let input_tokens = usage
        .get("promptTokenCount")
        .or_else(|| usage.get("prompt_token_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("candidatesTokenCount")
        .or_else(|| usage.get("candidates_token_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(kheish_types::ModelUsage {
        input_tokens,
        output_tokens,
        cost_usd: 0.0,
    })
}

fn google_finish_reason(reason: &str) -> ModelFinishReason {
    match reason.trim().to_ascii_uppercase().as_str() {
        "STOP" => ModelFinishReason::Completed,
        "MAX_TOKENS" => ModelFinishReason::MaxTokens,
        reason if google_finish_reason_is_blocked(reason) => ModelFinishReason::Blocked,
        _ => ModelFinishReason::Unknown,
    }
}

fn google_response_block_reason(payload: &Value) -> Option<String> {
    payload
        .get("promptFeedback")
        .or_else(|| payload.get("prompt_feedback"))
        .and_then(google_prompt_feedback_block_reason)
        .or_else(|| {
            payload
                .get("candidates")
                .and_then(Value::as_array)
                .and_then(|candidates| candidates.iter().find_map(google_candidate_block_reason))
        })
}

fn google_prompt_feedback_block_reason(feedback: &Value) -> Option<String> {
    feedback
        .get("blockReason")
        .or_else(|| feedback.get("block_reason"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .filter(|reason| !reason.eq_ignore_ascii_case("BLOCK_REASON_UNSPECIFIED"))
        .map(|reason| reason.to_ascii_uppercase())
}

fn google_candidate_block_reason(candidate: &Value) -> Option<String> {
    candidate
        .get("finishReason")
        .or_else(|| candidate.get("finish_reason"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|reason| google_finish_reason_is_blocked(reason))
        .map(|reason| reason.to_ascii_uppercase())
}

fn google_finish_reason_is_blocked(reason: &str) -> bool {
    matches!(
        reason.trim().to_ascii_uppercase().as_str(),
        "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "RECITATION" | "IMAGE_SAFETY"
    )
}

fn google_blocked_message(context: &str, reason: &str) -> String {
    format!("Google {context} was blocked by safety filters: {reason}")
}

fn google_model_supports_function_calling(model: &str) -> bool {
    !model.trim().to_ascii_lowercase().contains("image")
}

fn google_model_supports_image_input(model: &str) -> bool {
    model.trim().to_ascii_lowercase().starts_with("gemini-")
}

fn google_generate_content_endpoint(base_url: &str, model: &str) -> Result<String, ProviderError> {
    if model.trim().is_empty() {
        return Err(provider_error("missing Google model"));
    }
    let trimmed = base_url.trim_end_matches('/');
    let versioned = if trimmed.ends_with("/v1beta")
        || trimmed.ends_with("/v1")
        || trimmed.ends_with("/v1alpha")
        || trimmed.contains("/v1beta/")
        || trimmed.contains("/v1/")
        || trimmed.contains("/v1alpha/")
    {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1beta")
    };
    if versioned.ends_with("/models") {
        Ok(format!("{versioned}/{model}:generateContent"))
    } else {
        Ok(format!("{versioned}/models/{model}:generateContent"))
    }
}

fn headers_from_material(material: &ResolvedAuthMaterial) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    for (name, value) in &material.headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            ProviderError {
                message: format!("invalid Google auth header name {name}: {error}"),
                retryable: false,
                retry_after_ms: None,
            }
        })?;
        let value = HeaderValue::from_str(value).map_err(|error| ProviderError {
            message: format!("invalid Google auth header value for {name}: {error}"),
            retryable: false,
            retry_after_ms: None,
        })?;
        headers.insert(name, value);
    }
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

fn build_google_image_body(
    prompt: &str,
    images: &[GoogleImageEditInput],
    size: Option<&str>,
) -> Result<Value, ProviderError> {
    let mut parts = Vec::with_capacity(images.len() + 1);
    for image in images {
        parts.push(json!({
            "inlineData": {
                "mimeType": image.media_type,
                "data": BASE64_STANDARD.encode(&image.bytes),
            }
        }));
    }
    parts.push(json!({ "text": prompt }));

    let mut generation_config = serde_json::Map::new();
    generation_config.insert("responseModalities".to_string(), json!(["TEXT", "IMAGE"]));
    if let Some(size) = size {
        let (aspect_ratio, image_size) = google_image_size_parts(size)?;
        generation_config.insert(
            "imageConfig".to_string(),
            json!({
                "aspectRatio": aspect_ratio,
                "imageSize": image_size,
            }),
        );
    }

    Ok(json!({
        "contents": [{
            "role": "user",
            "parts": parts,
        }],
        "generationConfig": Value::Object(generation_config),
    }))
}

async fn execute_google_image_request(
    client: &Client,
    config: &GoogleImageProviderConfig,
    observer: &Arc<dyn RuntimeObserver>,
    artifact_prefix: &str,
    body: Value,
) -> Result<GoogleImageGenerationResponse, ProviderError> {
    let mut force_refresh = false;
    let (response, response_target, response_grant_id) = loop {
        let auth_material = google_image_auth_material(config, force_refresh).await?;
        let grant_id = auth_material.grant_id.clone();
        let endpoint = google_generate_content_endpoint(
            auth_material
                .base_url_override
                .as_deref()
                .unwrap_or(config.base_url.as_str()),
            &resolve_google_image_model(&config.model),
        )?;
        let headers = headers_from_material(&auth_material)?;
        ensure_google_image_auth_material_active(config, &auth_material).await?;
        record_google_media_provider_request(
            observer,
            &format!("{artifact_prefix}-provider-request"),
            &endpoint,
            &headers,
            &body,
            grant_id.clone(),
        )?;
        let response = match client
            .post(&endpoint)
            .headers(headers)
            .json(&body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                let mapped = map_transport_error(error);
                record_google_external_failure(
                    observer,
                    google_external_action_target(&endpoint),
                    &mapped.message,
                    grant_id,
                )?;
                return Err(mapped);
            }
        };
        if response.status() == StatusCode::UNAUTHORIZED
            && config.request_auth_provider.is_some()
            && !force_refresh
        {
            record_google_external_failure(
                observer,
                google_external_action_target(&endpoint),
                "401-refresh",
                grant_id,
            )?;
            force_refresh = true;
            continue;
        }
        break (response, google_external_action_target(&endpoint), grant_id);
    };
    let status = response.status();
    let response_headers = response.headers().clone();
    let payload = match read_google_image_response_json(response).await {
        Ok(payload) => payload,
        Err(error) => {
            record_google_external_failure(
                observer,
                response_target,
                &error.message,
                response_grant_id,
            )?;
            return Err(error);
        }
    };
    if !status.is_success() {
        let error = map_payload_http_error(status, &response_headers, &payload);
        record_google_external_failure(
            observer,
            response_target,
            &error.message,
            response_grant_id,
        )?;
        return Err(error);
    }
    record_google_media_provider_response(
        observer,
        &format!("{artifact_prefix}-provider-response"),
        &response_target,
        status.as_u16(),
        &response_headers,
        &payload,
        response_grant_id,
    )?;
    decode_google_image_response(&payload, &resolve_google_image_model(&config.model))
}

async fn google_image_auth_material(
    config: &GoogleImageProviderConfig,
    force_refresh: bool,
) -> Result<ResolvedAuthMaterial, ProviderError> {
    if let Some(provider) = &config.request_auth_provider {
        let result = if force_refresh {
            provider.refresh().await
        } else {
            provider.resolve().await
        };
        return result.map_err(|error| ProviderError {
            message: format!("failed to resolve Google auth material: {error}"),
            retryable: false,
            retry_after_ms: None,
        });
    }
    let api_key = config
        .api_key
        .as_ref()
        .ok_or_else(|| provider_error("missing Google API key"))?;
    let mut headers = BTreeMap::new();
    headers.insert("x-goog-api-key".to_string(), api_key.clone());
    Ok(ResolvedAuthMaterial {
        headers,
        base_url_override: None,
        grant_id: None,
        lease_id: None,
    })
}

async fn ensure_google_image_auth_material_active(
    config: &GoogleImageProviderConfig,
    material: &ResolvedAuthMaterial,
) -> Result<(), ProviderError> {
    if let Some(provider) = &config.request_auth_provider {
        provider
            .ensure_active(material)
            .await
            .map_err(|error| ProviderError {
                message: format!("Google auth material is no longer active: {error}"),
                retryable: false,
                retry_after_ms: None,
            })?;
    }
    Ok(())
}

fn google_image_size_parts(size: &str) -> Result<(String, String), ProviderError> {
    let (width, height) = parse_image_size(size)?;
    let aspect_ratio = normalize_aspect_ratio(width, height)
        .ok_or_else(|| provider_error(format!("unsupported Google image aspect ratio {size}")))?;
    let image_size = match width.max(height) {
        0..=1024 => "1K",
        1025..=2048 => "2K",
        2049..=4096 => "4K",
        _ => {
            return Err(provider_error(format!(
                "unsupported Google image size {size}; maximum supported edge is 4096"
            )));
        }
    };
    Ok((aspect_ratio.to_string(), image_size.to_string()))
}

fn parse_image_size(size: &str) -> Result<(u32, u32), ProviderError> {
    let (width, height) = size.split_once('x').ok_or_else(|| {
        provider_error(format!("invalid image size {size}; expected WIDTHxHEIGHT"))
    })?;
    let width = width.parse::<u32>().map_err(|_| {
        provider_error(format!(
            "invalid image size {size}; width must be one positive integer"
        ))
    })?;
    let height = height.parse::<u32>().map_err(|_| {
        provider_error(format!(
            "invalid image size {size}; height must be one positive integer"
        ))
    })?;
    if width == 0 || height == 0 {
        return Err(provider_error(format!(
            "invalid image size {size}; both dimensions must be positive"
        )));
    }
    Ok((width, height))
}

fn normalize_aspect_ratio(width: u32, height: u32) -> Option<&'static str> {
    let gcd = gcd(width, height);
    match (width / gcd, height / gcd) {
        (1, 1) => Some("1:1"),
        (2, 3) => Some("2:3"),
        (3, 2) => Some("3:2"),
        (3, 4) => Some("3:4"),
        (4, 3) => Some("4:3"),
        (4, 5) => Some("4:5"),
        (5, 4) => Some("5:4"),
        (9, 16) => Some("9:16"),
        (16, 9) => Some("16:9"),
        (21, 9) => Some("21:9"),
        _ => None,
    }
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let remainder = a % b;
        a = b;
        b = remainder;
    }
    a
}

async fn read_google_image_response_json(
    response: reqwest::Response,
) -> Result<Value, ProviderError> {
    if let Some(content_length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        && content_length > MAX_GOOGLE_IMAGE_RESPONSE_BYTES
    {
        return Err(provider_error(format!(
            "Google image response exceeds the {MAX_GOOGLE_IMAGE_RESPONSE_BYTES} byte limit"
        )));
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(map_transport_error)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_GOOGLE_IMAGE_RESPONSE_BYTES {
            return Err(provider_error(format!(
                "Google image response exceeds the {MAX_GOOGLE_IMAGE_RESPONSE_BYTES} byte limit"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice::<Value>(&bytes)
        .map_err(|error| provider_error(format!("failed to decode Google image response: {error}")))
}

fn decode_google_image_response(
    payload: &Value,
    fallback_model: &str,
) -> Result<GoogleImageGenerationResponse, ProviderError> {
    let candidates = payload
        .get("candidates")
        .and_then(Value::as_array)
        .ok_or_else(|| provider_error("Google image response did not include candidates"))?;

    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    for candidate in candidates {
        let Some(parts) = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    text_parts.push(trimmed.to_string());
                }
            }
            if let Some(inline_data) = part.get("inlineData").or_else(|| part.get("inline_data")) {
                let mime_type = inline_data
                    .get("mimeType")
                    .or_else(|| inline_data.get("mime_type"))
                    .and_then(Value::as_str)
                    .unwrap_or("image/png");
                let encoded = inline_data
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| provider_error("Google image part was missing base64 data"))?;
                let bytes = BASE64_STANDARD
                    .decode(encoded)
                    .map_err(|error| ProviderError {
                        message: format!("failed to decode Google image payload: {error}"),
                        retryable: false,
                        retry_after_ms: None,
                    })?;
                images.push(GoogleGeneratedImage {
                    media_type: mime_type.to_string(),
                    bytes,
                });
            }
        }
    }

    if images.is_empty() {
        if let Some(reason) = google_response_block_reason(payload) {
            return Err(provider_error(google_blocked_message(
                "image response",
                &reason,
            )));
        }
        return Err(provider_error(
            "Google image response did not include any inline image parts",
        ));
    }

    Ok(GoogleImageGenerationResponse {
        model: payload
            .get("modelVersion")
            .or_else(|| payload.get("model_version"))
            .and_then(Value::as_str)
            .unwrap_or(fallback_model)
            .to_string(),
        images,
        text: (!text_parts.is_empty()).then(|| text_parts.join("\n")),
    })
}

fn map_transport_error(error: reqwest::Error) -> ProviderError {
    ProviderError {
        message: format!("Google request failed: {error}"),
        retryable: true,
        retry_after_ms: None,
    }
}

fn provider_audit_error(error: anyhow::Error) -> ProviderError {
    ProviderError {
        message: format!("external action audit failed: {error}"),
        retryable: false,
        retry_after_ms: None,
    }
}

fn map_payload_http_error(
    status: StatusCode,
    headers: &HeaderMap,
    payload: &Value,
) -> ProviderError {
    let retry_after_ms = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(google_retry_after_ms);
    let error = payload.get("error").unwrap_or(payload);
    let message = sanitize_upstream_error_message(
        "Google",
        "request error",
        Some(status),
        error
            .get("status")
            .and_then(Value::as_str)
            .or_else(|| error.get("type").and_then(Value::as_str)),
        error
            .get("code")
            .and_then(Value::as_i64)
            .map(|value| value.to_string())
            .as_deref()
            .or_else(|| error.get("code").and_then(Value::as_str)),
        error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| payload.get("message").and_then(Value::as_str)),
    );
    ProviderError {
        message,
        retryable: status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS,
        retry_after_ms,
    }
}

fn google_retry_after_ms(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if let Ok(seconds) = trimmed.parse::<u64>() {
        return Some(seconds.saturating_mul(1_000));
    }
    let target = httpdate::parse_http_date(trimmed).ok()?;
    let duration = target.duration_since(SystemTime::now()).ok()?;
    Some(
        u64::try_from(duration.as_millis())
            .unwrap_or(u64::MAX)
            .max(1),
    )
}

fn map_sink_error(error: anyhow::Error) -> ProviderError {
    ProviderError {
        message: format!("failed to emit Google stream event: {error}"),
        retryable: false,
        retry_after_ms: None,
    }
}

fn provider_error(message: impl Into<String>) -> ProviderError {
    ProviderError {
        message: message.into(),
        retryable: false,
        retry_after_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Result;
    use async_trait::async_trait;
    use base64::Engine as _;
    use kheish_auth::{RequestAuthProvider, ResolvedAuthMaterial};
    use reqwest::StatusCode;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{
        GoogleImageEditInput, GoogleImageEditRequest, GoogleImageEditor,
        GoogleImageGenerationRequest, GoogleImageGenerator, GoogleProvider, GoogleProviderConfig,
        build_google_image_body, decode_google_image_response, google_function_call_from_part,
        google_function_parameters_schema, google_generate_content_endpoint,
        google_response_block_reason, google_user_parts, map_payload_http_error,
        resolve_google_image_model, resolve_google_model,
    };
    use crate::providers::attachments::AttachmentRenderCache;
    use crate::providers::test_fixtures::{
        MINIMAL_PNG, create_fixture_dir, write_image_attachment,
    };
    use crate::{
        DebugArtifact, DebugCaptureLevel, InMemoryObserver, ModelEventSink, ModelGenerationConfig,
        ModelProvider, ModelRuntimeRequest, ModelStreamEvent, NoopObserver, ResponseFormat,
        RuntimeObserver, ToolChoice, TraceEvent, TraceEventKind,
    };
    use kheish_core::ModelRequestKind;
    use kheish_types::{
        AttachmentRef, InputContentPart, ProviderPrompt, StructuredFieldSchema,
        StructuredValueKind, ToolDefinition,
    };
    use tokio::sync::mpsc;

    use crate::providers::testsupport::spawn_mock_server;

    fn has_provider_external_action(
        traces: &[TraceEvent],
        phase: &str,
        target_prefix: &str,
    ) -> bool {
        traces.iter().any(|event| {
            matches!(
                &event.kind,
                TraceEventKind::ExternalAction {
                    phase: recorded_phase,
                    kind,
                    target,
                    ..
                } if recorded_phase == phase
                    && kind == "model_provider"
                    && target.starts_with(target_prefix)
            )
        })
    }

    async fn spawn_mock_server_with_declared_content_length(
        declared_content_length: usize,
    ) -> Result<String> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("server should accept");
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = socket
                    .read(&mut buffer)
                    .await
                    .expect("request read should succeed");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {declared_content_length}\r\nContent-Type: application/json\r\n\r\nx"
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("response write should succeed");
        });
        Ok(format!("http://{address}"))
    }

    struct FixedDebugObserver {
        level: DebugCaptureLevel,
        artifacts: Mutex<Vec<DebugArtifact>>,
    }

    impl FixedDebugObserver {
        fn shared(level: DebugCaptureLevel) -> Arc<Self> {
            Arc::new(Self {
                level,
                artifacts: Mutex::new(Vec::new()),
            })
        }

        fn debug_artifacts(&self) -> Vec<DebugArtifact> {
            self.artifacts.lock().clone()
        }
    }

    impl RuntimeObserver for FixedDebugObserver {
        fn debug_level(&self) -> DebugCaptureLevel {
            self.level
        }

        fn record(&self, _event: TraceEvent) {}

        fn record_debug_artifact(&self, artifact: DebugArtifact) {
            self.artifacts.lock().push(artifact);
        }

        fn increment_counter(&self, _name: &str, _delta: u64) {}
    }

    fn default_runtime_request() -> ModelRuntimeRequest {
        ModelRuntimeRequest {
            attempt: 1,
            kind: ModelRequestKind::MainLoop,
            session_id: "session-google".to_string(),
            thread_id: None,
            turn: 1,
            prompt: ProviderPrompt {
                instructions: vec!["Be precise.".to_string()],
                force_synthetic_user_prefix: false,
                items: vec![kheish_types::ProviderInputItem::Message {
                    id: "user-1".to_string(),
                    role: kheish_types::Role::User,
                    content: "Tell me about the plan.".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: None,
                    provider_context: None,
                }],
            },
            available_tools: Vec::new(),
            generation: ModelGenerationConfig {
                tool_choice: ToolChoice::Auto,
                allow_parallel_tool_calls: true,
                max_output_tokens: Some(1024),
                temperature: Some(0.2),
                response_format: ResponseFormat::Text,
                ..ModelGenerationConfig::default()
            },
        }
    }

    #[test]
    fn google_user_parts_include_document_preview_images() -> Result<()> {
        let temp = create_fixture_dir("google-doc-preview")?;
        let raw_path = temp.join("plan.dxf");
        let text_path = temp.join("plan.dxf.txt");
        let preview_path = temp.join("plan.dxf.preview.png");
        std::fs::write(&raw_path, b"0\nEOF\n")?;
        std::fs::write(&text_path, b"DXF summary")?;
        std::fs::write(&preview_path, MINIMAL_PNG)?;
        let attachment = AttachmentRef {
            id: "asset-dxf".to_string(),
            media_type: "application/dxf".to_string(),
            uri: raw_path.display().to_string(),
            file_name: Some("plan.dxf".to_string()),
            sha256: None,
            byte_length: None,
            text_uri: Some(text_path.display().to_string()),
            text_sha256: None,
            text_byte_length: None,
            preview_image_uri: Some(preview_path.display().to_string()),
            preview_image_media_type: Some("image/png".to_string()),
            preview_image_sha256: None,
            preview_image_byte_length: None,
        };

        let parts = google_user_parts(
            "",
            &[InputContentPart::Attachment { attachment }],
            &[],
            None,
            &AttachmentRenderCache::default(),
            true,
        )?;

        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["inlineData"]["mimeType"], "image/png");
        assert!(
            parts[0]["inlineData"]["data"]
                .as_str()
                .unwrap_or_default()
                .starts_with("iVBOR")
        );
        assert_eq!(
            parts[1]["text"],
            "Document attachment: plan.dxf (application/dxf)\nDXF summary"
        );
        Ok(())
    }

    #[test]
    fn google_user_parts_include_image_asset_hints() -> Result<()> {
        let temp = create_fixture_dir("google-images")?;
        let png = write_image_attachment(&temp, "sample-a.png", "image/png")?;
        let jpeg = write_image_attachment(&temp, "sample-b.jpg", "image/jpeg")?;

        let parts = google_user_parts(
            "Inspect these attachments.",
            &[],
            &[png, jpeg],
            None,
            &AttachmentRenderCache::default(),
            true,
        )?;

        assert!(
            parts
                .iter()
                .any(|part| part["text"] == "Inspect these attachments.")
        );
        let asset_hints = parts
            .iter()
            .filter(|part| {
                part["text"]
                    .as_str()
                    .map(|text| text.contains("daemon asset ID"))
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        assert_eq!(asset_hints.len(), 2);
        assert!(asset_hints.iter().any(|part| {
            part["text"]
                .as_str()
                .map(|text| text.contains("fixture-sample-a.png"))
                .unwrap_or(false)
        }));
        assert!(asset_hints.iter().any(|part| {
            part["text"]
                .as_str()
                .map(|text| text.contains("fixture-sample-b.jpg"))
                .unwrap_or(false)
        }));
        let image_parts = parts
            .iter()
            .filter(|part| part.get("inlineData").is_some())
            .collect::<Vec<_>>();
        assert_eq!(image_parts.len(), 2);
        Ok(())
    }

    #[test]
    fn google_model_resolvers_apply_defaults() {
        assert_eq!(resolve_google_model(""), "gemini-2.5-flash");
        assert_eq!(
            resolve_google_image_model("gemini-2.5-flash"),
            "gemini-2.5-flash-image"
        );
        assert_eq!(
            resolve_google_image_model("gemini-3-pro-image-preview"),
            "gemini-3-pro-image-preview"
        );
        assert_eq!(
            resolve_google_image_model("gpt-image-1.5"),
            "gemini-2.5-flash-image"
        );
    }

    #[test]
    fn google_function_calls_without_provider_ids_are_stable_and_unique() -> Result<()> {
        let part = json!({
            "functionCall": {
                "name": "emit_output",
                "args": {
                    "content": "done"
                }
            }
        });
        let first = google_function_call_from_part(&part, 0, Some("resp-google-1"))?
            .expect("first Google function call should parse");
        let second = google_function_call_from_part(&part, 1, Some("resp-google-1"))?
            .expect("second Google function call should parse");
        assert_eq!(first.id, "google-call-resp-google-1-0");
        assert_eq!(second.id, "google-call-resp-google-1-1");
        assert_ne!(first.id, second.id);
        assert_eq!(
            first.assistant_provider_response_id.as_deref(),
            Some("resp-google-1")
        );

        let explicit = google_function_call_from_part(
            &json!({
                "functionCall": {
                    "id": "provider-call-1",
                    "name": "emit_output",
                    "args": {}
                }
            }),
            0,
            Some("resp-google-2"),
        )?
        .expect("explicit Google function call id should parse");
        assert_eq!(explicit.id, "provider-call-1");
        Ok(())
    }

    #[test]
    fn google_endpoint_targets_generate_content() -> Result<()> {
        let endpoint = google_generate_content_endpoint(
            "https://generativelanguage.googleapis.com",
            "gemini-2.5-flash",
        )?;
        assert_eq!(
            endpoint,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
        );
        Ok(())
    }

    #[test]
    fn google_image_body_maps_size_to_ratio_and_image_size() -> Result<()> {
        let body = build_google_image_body("draw a plan", &[], Some("1024x1536"))?;
        assert_eq!(
            body["generationConfig"]["responseModalities"],
            json!(["TEXT", "IMAGE"])
        );
        assert_eq!(
            body["generationConfig"]["imageConfig"]["aspectRatio"],
            json!("2:3")
        );
        assert_eq!(
            body["generationConfig"]["imageConfig"]["imageSize"],
            json!("2K")
        );
        Ok(())
    }

    #[test]
    fn google_image_body_rejects_unsupported_aspect_ratio() {
        let error = build_google_image_body("draw a plan", &[], Some("1000x1100"))
            .expect_err("unsupported ratios should be rejected");
        assert!(
            error
                .message
                .contains("unsupported Google image aspect ratio")
        );
    }

    #[test]
    fn google_image_response_decoder_extracts_inline_images() -> Result<()> {
        let payload = json!({
            "modelVersion": "gemini-3-pro-image-preview",
            "candidates": [{
                "content": {
                    "parts": [
                        { "text": "Here is the image." },
                        {
                            "inlineData": {
                                "mimeType": "image/png",
                                "data": super::BASE64_STANDARD.encode(b"png-bytes")
                            }
                        }
                    ]
                }
            }]
        });
        let decoded = decode_google_image_response(&payload, "gemini-3-pro-image-preview")?;
        assert_eq!(decoded.model, "gemini-3-pro-image-preview");
        assert_eq!(decoded.images.len(), 1);
        assert_eq!(decoded.images[0].media_type, "image/png");
        assert_eq!(decoded.images[0].bytes, b"png-bytes");
        assert_eq!(decoded.text.as_deref(), Some("Here is the image."));
        Ok(())
    }

    #[test]
    fn google_image_response_decoder_rejects_missing_inline_images() {
        let error = decode_google_image_response(
            &json!({
                "candidates": [{
                    "content": {
                        "parts": [{
                            "text": "text only"
                        }]
                    }
                }]
            }),
            "gemini-3-pro-image-preview",
        )
        .expect_err("responses without inline image parts should be rejected");
        assert!(
            error
                .message
                .contains("did not include any inline image parts")
        );
    }

    #[test]
    fn google_response_block_reason_reads_prompt_feedback_and_candidate_finish_reason() {
        assert_eq!(
            google_response_block_reason(&json!({
                "promptFeedback": {
                    "blockReason": "PROHIBITED_CONTENT"
                }
            }))
            .as_deref(),
            Some("PROHIBITED_CONTENT")
        );
        assert_eq!(
            google_response_block_reason(&json!({
                "candidates": [{
                    "finishReason": "IMAGE_SAFETY"
                }]
            }))
            .as_deref(),
            Some("IMAGE_SAFETY")
        );
        assert_eq!(
            google_response_block_reason(&json!({
                "promptFeedback": {
                    "blockReason": "BLOCK_REASON_UNSPECIFIED"
                }
            })),
            None
        );
    }

    #[test]
    fn google_image_response_decoder_reports_safety_blocks() {
        let error = decode_google_image_response(
            &json!({
                "promptFeedback": {
                    "blockReason": "IMAGE_SAFETY"
                },
                "candidates": []
            }),
            "gemini-3-pro-image-preview",
        )
        .expect_err("blocked image responses should be explicit");
        assert!(
            error
                .message
                .contains("Google image response was blocked by safety filters: IMAGE_SAFETY"),
            "unexpected error: {}",
            error.message
        );
    }

    #[test]
    fn google_payload_http_error_marks_retryable_and_parses_retry_after() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3".parse().unwrap());
        let error = map_payload_http_error(
            StatusCode::TOO_MANY_REQUESTS,
            &headers,
            &json!({
                "error": {
                    "message": "quota exceeded"
                }
            }),
        );
        assert!(error.retryable);
        assert_eq!(error.retry_after_ms, Some(3_000));
        assert_eq!(error.message, "Google request error with status 429");

        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        headers.insert(
            reqwest::header::RETRY_AFTER,
            httpdate::fmt_http_date(future).parse().unwrap(),
        );
        let date_error = map_payload_http_error(
            StatusCode::SERVICE_UNAVAILABLE,
            &headers,
            &json!({
                "error": {
                    "message": "try later"
                }
            }),
        );
        assert!(date_error.retryable);
        assert!(
            date_error.retry_after_ms.is_some_and(|value| value > 0),
            "HTTP-date Retry-After should produce a positive delay: {date_error:?}"
        );
        assert_eq!(date_error.message, "Google request error with status 503");
    }

    #[test]
    fn google_payload_http_error_does_not_echo_secret_messages() {
        let headers = reqwest::header::HeaderMap::new();
        let leaked_secret = "google-secret-from-upstream";
        let error = map_payload_http_error(
            StatusCode::UNAUTHORIZED,
            &headers,
            &json!({
                "error": {
                    "message": format!("bad x-goog-api-key {leaked_secret}"),
                    "status": "UNAUTHENTICATED",
                    "code": 401
                }
            }),
        );

        assert!(!error.message.contains(leaked_secret));
        assert_eq!(
            error.message,
            "Google request error with status 401: type=UNAUTHENTICATED, code=401"
        );
    }

    #[tokio::test]
    async fn google_provider_posts_tools_and_structured_schema() -> Result<()> {
        let captured = Arc::new(Mutex::new(String::new()));
        let base_url = spawn_mock_server(
            200,
            &[("content-type", "application/json")],
            &json!({
                "responseId": "resp-google-1",
                "usageMetadata": {
                    "promptTokenCount": 10,
                    "candidatesTokenCount": 3
                },
                "candidates": [{
                    "finishReason": "STOP",
                    "content": {
                        "parts": [{
                            "text": "{\"ok\":true}"
                        }]
                    }
                }]
            })
            .to_string(),
            captured.clone(),
        )
        .await?;

        let mut config = GoogleProviderConfig::new("gemini-2.5-flash", "test-key");
        config.base_url = base_url;
        let provider = GoogleProvider::with_observer(config, Arc::new(NoopObserver))?;
        let mut request = default_runtime_request();
        request.available_tools = vec![ToolDefinition {
            name: "extract_rooms".to_string(),
            description: "Extract room boundaries.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": { "type": "string" }
                },
                "required": ["plan"],
                "additionalProperties": false
            }),
            allows_parallel: false,
        }];
        request.generation.response_format = ResponseFormat::StructuredJson {
            schema: StructuredFieldSchema {
                kind: StructuredValueKind::Object,
                fields: BTreeMap::from([(
                    "ok".to_string(),
                    StructuredFieldSchema::new(StructuredValueKind::Boolean),
                )]),
                optional_fields: BTreeMap::new(),
                items: None,
            },
        };
        let (sender, mut receiver) = mpsc::unbounded_channel();
        provider
            .stream(request, ModelEventSink::new(sender))
            .await?;
        let payload: Value = serde_json::from_str(&captured.lock())?;
        assert_eq!(
            payload["systemInstruction"]["parts"][0]["text"],
            json!("Be precise.")
        );
        assert_eq!(
            payload["tools"][0]["functionDeclarations"][0]["name"],
            json!("extract_rooms")
        );
        assert_eq!(
            payload["tools"][0]["functionDeclarations"][0]["parameters"],
            json!({
                "type": "object",
                "properties": {
                    "plan": { "type": "string" }
                },
                "required": ["plan"]
            })
        );
        assert_eq!(
            payload["toolConfig"]["functionCallingConfig"]["mode"],
            json!("VALIDATED")
        );
        assert_eq!(
            payload["generationConfig"]["responseMimeType"],
            json!("application/json")
        );

        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        assert!(events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::MessageId { value } if value == "resp-google-1"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::StructuredOutput { value } if value["ok"] == json!(true)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::Usage { usage } if usage.input_tokens == 10 && usage.output_tokens == 3
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::Stop { reason } if *reason == kheish_types::ModelFinishReason::Completed
        )));
        Ok(())
    }

    #[tokio::test]
    async fn google_provider_rejects_specific_unavailable_tool_locally() -> Result<()> {
        let mut request = default_runtime_request();
        request.available_tools = vec![ToolDefinition {
            name: "read_file".to_string(),
            description: "Read one file.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
            allows_parallel: false,
        }];
        request.generation.tool_choice = ToolChoice::Specific {
            name: "generate_audio".to_string(),
        };
        let provider = GoogleProvider::with_observer(
            GoogleProviderConfig::new("gemini-2.5-flash", "test-key"),
            Arc::new(NoopObserver),
        )?;
        let (sender, _receiver) = mpsc::unbounded_channel();
        let error = provider
            .stream(request, ModelEventSink::new(sender))
            .await
            .expect_err("specific unavailable tools should fail before provider dispatch");
        assert!(
            error
                .message
                .contains("Google tool choice requested unavailable tool `generate_audio`"),
            "unexpected error: {}",
            error.message
        );
        assert!(!error.retryable);
        Ok(())
    }

    #[tokio::test]
    async fn google_provider_reports_prompt_feedback_safety_block() -> Result<()> {
        let captured = Arc::new(Mutex::new(String::new()));
        let base_url = spawn_mock_server(
            200,
            &[("content-type", "application/json")],
            &json!({
                "promptFeedback": {
                    "blockReason": "SAFETY",
                    "safetyRatings": []
                }
            })
            .to_string(),
            captured,
        )
        .await?;

        let mut config = GoogleProviderConfig::new("gemini-2.5-flash", "test-key");
        config.base_url = base_url;
        let provider = GoogleProvider::with_observer(config, Arc::new(NoopObserver))?;
        let (sender, _receiver) = mpsc::unbounded_channel();
        let error = provider
            .stream(default_runtime_request(), ModelEventSink::new(sender))
            .await
            .expect_err("prompt feedback safety blocks should be explicit");
        assert!(
            error
                .message
                .contains("Google response was blocked by safety filters: SAFETY"),
            "unexpected error: {}",
            error.message
        );
        assert!(!error.retryable);
        Ok(())
    }

    #[tokio::test]
    async fn google_structured_response_reports_candidate_safety_block() -> Result<()> {
        let captured = Arc::new(Mutex::new(String::new()));
        let base_url = spawn_mock_server(
            200,
            &[("content-type", "application/json")],
            &json!({
                "candidates": [{
                    "finishReason": "SAFETY",
                    "content": {
                        "parts": []
                    }
                }]
            })
            .to_string(),
            captured,
        )
        .await?;

        let mut config = GoogleProviderConfig::new("gemini-2.5-flash", "test-key");
        config.base_url = base_url;
        let provider = GoogleProvider::with_observer(config, Arc::new(NoopObserver))?;
        let mut request = default_runtime_request();
        request.generation.response_format = ResponseFormat::StructuredJson {
            schema: StructuredFieldSchema {
                kind: StructuredValueKind::Object,
                fields: BTreeMap::from([(
                    "ok".to_string(),
                    StructuredFieldSchema::new(StructuredValueKind::Boolean),
                )]),
                optional_fields: BTreeMap::new(),
                items: None,
            },
        };
        let (sender, _receiver) = mpsc::unbounded_channel();
        let error = provider
            .stream(request, ModelEventSink::new(sender))
            .await
            .expect_err("candidate safety blocks should be explicit for structured responses");
        assert!(
            error
                .message
                .contains("Google structured response was blocked by safety filters: SAFETY"),
            "unexpected error: {}",
            error.message
        );
        assert!(!error.retryable);
        Ok(())
    }

    #[test]
    fn google_function_parameters_schema_drops_unsupported_keywords_recursively() {
        let schema = json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "object",
                    "properties": {
                        "rooms": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string" }
                                },
                                "required": ["name"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "additionalProperties": {
                        "type": "string"
                    }
                }
            },
            "required": ["plan"],
            "additionalProperties": false
        });

        assert_eq!(
            google_function_parameters_schema(&schema),
            json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "object",
                        "properties": {
                            "rooms": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "name": { "type": "string" }
                                    },
                                    "required": ["name"]
                                }
                            }
                        }
                    }
                },
                "required": ["plan"]
            })
        );
    }

    #[tokio::test]
    async fn google_provider_uses_effective_model_output_limits() -> Result<()> {
        let captured = Arc::new(Mutex::new(String::new()));
        let base_url = spawn_mock_server(
            200,
            &[("content-type", "application/json")],
            &json!({
                "responseId": "resp-google-limit",
                "candidates": [{
                    "finishReason": "STOP",
                    "content": {
                        "parts": [{
                            "text": "limit-ok"
                        }]
                    }
                }]
            })
            .to_string(),
            captured.clone(),
        )
        .await?;

        let mut config = GoogleProviderConfig::new("gemini-2.5-flash", "test-key");
        config.base_url = base_url;
        let provider = GoogleProvider::with_observer(config, Arc::new(NoopObserver))?;
        let mut request = default_runtime_request();
        request.generation.model = Some("gemini-3-pro-image-preview".to_string());
        request.generation.max_output_tokens = None;
        request.available_tools.clear();

        let (sender, _receiver) = mpsc::unbounded_channel();
        provider
            .stream(request, ModelEventSink::new(sender))
            .await?;

        let payload: Value = serde_json::from_str(&captured.lock())?;
        assert_eq!(
            payload["generationConfig"]["maxOutputTokens"],
            json!(kheish_types::model_max_output_tokens("gemini-3-pro-image-preview").default)
        );
        Ok(())
    }

    #[tokio::test]
    async fn google_provider_streams_function_calls() -> Result<()> {
        let base_url = spawn_mock_server(
            200,
            &[("content-type", "application/json")],
            &json!({
                "candidates": [{
                    "finishReason": "STOP",
                    "content": {
                        "parts": [{
                            "functionCall": {
                                "id": "call-1",
                                "name": "extract_rooms",
                                "args": { "plan": "2d" }
                            }
                        }]
                    }
                }]
            })
            .to_string(),
            Arc::new(Mutex::new(String::new())),
        )
        .await?;

        let mut config = GoogleProviderConfig::new("gemini-2.5-flash", "test-key");
        config.base_url = base_url;
        let provider = GoogleProvider::with_observer(config, Arc::new(NoopObserver))?;
        let mut request = default_runtime_request();
        request.available_tools = vec![ToolDefinition {
            name: "extract_rooms".to_string(),
            description: "Extract room boundaries.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": { "type": "string" }
                }
            }),
            allows_parallel: false,
        }];
        let (sender, mut receiver) = mpsc::unbounded_channel();
        provider
            .stream(request, ModelEventSink::new(sender))
            .await?;

        let mut saw_call = false;
        while let Ok(event) = receiver.try_recv() {
            if let ModelStreamEvent::ToolCall { call } = event {
                saw_call = call.id == "call-1"
                    && call.name == "extract_rooms"
                    && call.input["plan"] == json!("2d");
            }
        }
        assert!(saw_call);
        Ok(())
    }

    #[tokio::test]
    async fn google_provider_refreshes_once_after_401() -> Result<()> {
        #[derive(Default)]
        struct Counts {
            requests: AtomicUsize,
            resolves: AtomicUsize,
            refreshes: AtomicUsize,
        }

        #[derive(Default)]
        struct FakeAuthProvider {
            counts: Arc<Counts>,
        }

        #[async_trait]
        impl RequestAuthProvider for FakeAuthProvider {
            async fn resolve(&self) -> anyhow::Result<ResolvedAuthMaterial> {
                self.counts.resolves.fetch_add(1, Ordering::SeqCst);
                Ok(ResolvedAuthMaterial {
                    headers: [(
                        "Authorization".to_string(),
                        "Bearer stale-google-token".to_string(),
                    )]
                    .into_iter()
                    .collect(),
                    base_url_override: None,
                    grant_id: None,
                    lease_id: None,
                })
            }

            async fn refresh(&self) -> anyhow::Result<ResolvedAuthMaterial> {
                self.counts.refreshes.fetch_add(1, Ordering::SeqCst);
                Ok(ResolvedAuthMaterial {
                    headers: [(
                        "Authorization".to_string(),
                        "Bearer fresh-google-token".to_string(),
                    )]
                    .into_iter()
                    .collect(),
                    base_url_override: None,
                    grant_id: None,
                    lease_id: None,
                })
            }

            async fn ensure_active(&self, _material: &ResolvedAuthMaterial) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let counts = Arc::new(Counts::default());
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server_counts = counts.clone();
        let server = tokio::spawn(async move {
            for expected_auth in ["Bearer stale-google-token", "Bearer fresh-google-token"] {
                let (mut socket, _) = listener.accept().await.expect("server should accept");
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let read = socket
                        .read(&mut buffer)
                        .await
                        .expect("request read should succeed");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request_text = String::from_utf8(request).expect("request must be utf-8");
                let auth_header = request_text
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("authorization")
                                .then_some(value.trim())
                        })
                    })
                    .unwrap_or_default()
                    .to_string();
                let request_number = server_counts.requests.fetch_add(1, Ordering::SeqCst);
                assert_eq!(auth_header, expected_auth);
                let (status, body) = if request_number == 0 {
                    (
                        "401 Unauthorized",
                        json!({
                            "error": {
                                "message": "expired"
                            }
                        })
                        .to_string(),
                    )
                } else {
                    (
                        "200 OK",
                        json!({
                            "responseId": "google-refresh-1",
                            "candidates": [{
                                "finishReason": "STOP",
                                "content": {
                                    "parts": [{
                                        "text": "done"
                                    }]
                                }
                            }]
                        })
                        .to_string(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("response write should succeed");
            }
        });

        let provider = GoogleProvider::with_observer(
            GoogleProviderConfig {
                base_url: format!("http://{address}/v1beta"),
                api_key: None,
                request_auth_provider: Some(Arc::new(FakeAuthProvider {
                    counts: counts.clone(),
                })),
                ..GoogleProviderConfig::new("gemini-2.5-flash", "unused-key")
            },
            Arc::new(NoopObserver),
        )?;

        let (sender, mut receiver) = mpsc::unbounded_channel();
        provider
            .stream(default_runtime_request(), ModelEventSink::new(sender))
            .await?;

        let mut text = String::new();
        while let Ok(event) = receiver.try_recv() {
            if let ModelStreamEvent::TextDelta { text: delta } = event {
                text.push_str(&delta);
            }
        }
        assert_eq!(text, "done");
        assert_eq!(counts.requests.load(Ordering::SeqCst), 2);
        assert_eq!(counts.resolves.load(Ordering::SeqCst), 1);
        assert_eq!(counts.refreshes.load(Ordering::SeqCst), 1);
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn google_image_generator_posts_generate_content_requests() -> Result<()> {
        let captured = Arc::new(Mutex::new(String::new()));
        let base_url = spawn_mock_server(
            200,
            &[("content-type", "application/json")],
            &json!({
                "modelVersion": "gemini-3-pro-image-preview",
                "candidates": [{
                    "content": {
                        "parts": [{
                            "inlineData": {
                                "mimeType": "image/png",
                                "data": super::BASE64_STANDARD.encode(b"png")
                            }
                        }]
                    }
                }]
            })
            .to_string(),
            captured.clone(),
        )
        .await?;

        let mut config = GoogleProviderConfig::new("gemini-2.5-flash", "test-key");
        config.base_url = base_url;
        let generator = GoogleImageGenerator::new(config, Arc::new(NoopObserver))?;
        let response = generator
            .generate(GoogleImageGenerationRequest {
                prompt: "render".to_string(),
                count: 1,
                size: Some("1024x1024".to_string()),
            })
            .await?;
        let payload: Value = serde_json::from_str(&captured.lock())?;
        assert_eq!(
            payload["generationConfig"]["responseModalities"],
            json!(["TEXT", "IMAGE"])
        );
        assert_eq!(payload["contents"][0]["parts"][0]["text"], json!("render"));
        assert_eq!(response.images.len(), 1);
        assert_eq!(response.images[0].bytes, b"png");
        Ok(())
    }

    #[tokio::test]
    async fn google_image_generator_records_external_action_traces() -> Result<()> {
        let base_url = spawn_mock_server(
            200,
            &[("content-type", "application/json")],
            &json!({
                "modelVersion": "gemini-3-pro-image-preview",
                "candidates": [{
                    "content": {
                        "parts": [{
                            "inlineData": {
                                "mimeType": "image/png",
                                "data": super::BASE64_STANDARD.encode(b"png")
                            }
                        }]
                    }
                }]
            })
            .to_string(),
            Arc::new(Mutex::new(String::new())),
        )
        .await?;

        let mut config = GoogleProviderConfig::new("gemini-2.5-flash", "test-key");
        config.base_url = base_url;
        let observer = InMemoryObserver::shared();
        let generator = GoogleImageGenerator::new(config, observer.clone())?;
        let response = generator
            .generate(GoogleImageGenerationRequest {
                prompt: "render".to_string(),
                count: 1,
                size: Some("1024x1024".to_string()),
            })
            .await?;

        assert_eq!(response.images.len(), 1);
        let traces = observer.traces();
        assert!(has_provider_external_action(
            &traces,
            "request",
            "google:http://"
        ));
        assert!(has_provider_external_action(
            &traces,
            "response",
            "google:http://"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn google_image_generator_rejects_oversized_json_content_length() -> Result<()> {
        let base_url = spawn_mock_server_with_declared_content_length(
            super::MAX_GOOGLE_IMAGE_RESPONSE_BYTES + 1,
        )
        .await?;

        let mut config = GoogleProviderConfig::new("gemini-2.5-flash", "test-key");
        config.base_url = base_url;
        let generator = GoogleImageGenerator::new(config, Arc::new(NoopObserver))?;
        let error = generator
            .generate(GoogleImageGenerationRequest {
                prompt: "render".to_string(),
                count: 1,
                size: Some("1024x1024".to_string()),
            })
            .await
            .expect_err("oversized image response should fail before JSON parse");

        assert!(
            error.message.contains("Google image response exceeds"),
            "unexpected error: {error:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn google_image_debug_artifacts_redact_headers_and_inline_images() -> Result<()> {
        let response_image = b"provider-image-secret".repeat(16);
        let response_image_base64 = super::BASE64_STANDARD.encode(&response_image);
        let base_url = spawn_mock_server(
            200,
            &[("content-type", "application/json")],
            &json!({
                "modelVersion": "gemini-2.5-flash-image",
                "candidates": [{
                    "content": {
                        "parts": [{
                            "inlineData": {
                                "mimeType": "image/png",
                                "data": response_image_base64
                            }
                        }]
                    }
                }]
            })
            .to_string(),
            Arc::new(Mutex::new(String::new())),
        )
        .await?;

        let source_image = b"source-image-secret".repeat(16);
        let source_image_base64 = super::BASE64_STANDARD.encode(&source_image);
        let mut config = GoogleProviderConfig::new("gemini-2.5-flash-image", "google-secret-key");
        config.base_url = base_url;
        let observer = FixedDebugObserver::shared(DebugCaptureLevel::Full);
        let editor = GoogleImageEditor::new(config, observer.clone())?;
        let response = editor
            .edit(GoogleImageEditRequest {
                prompt: "neutral edit".to_string(),
                images: vec![GoogleImageEditInput {
                    file_name: "source.png".to_string(),
                    media_type: "image/png".to_string(),
                    bytes: source_image,
                }],
                count: 1,
                size: None,
            })
            .await?;
        assert_eq!(response.images.len(), 1);

        let artifacts = observer.debug_artifacts();
        let rendered = serde_json::to_string(&artifacts)?;
        assert!(!rendered.contains("google-secret-key"));
        assert!(!rendered.contains(&source_image_base64));
        assert!(!rendered.contains(&response_image_base64));

        let request = artifacts
            .iter()
            .find(|artifact| artifact.name == "google-image-edit-provider-request")
            .ok_or_else(|| anyhow::anyhow!("missing Google image provider request artifact"))?;
        assert_eq!(request.payload["headers"]["x-goog-api-key"], "<redacted>");
        assert_eq!(
            request.payload["body"]["contents"][0]["parts"][0]["inlineData"]["data"]["redacted"],
            "<redacted base64>"
        );

        let response = artifacts
            .iter()
            .find(|artifact| artifact.name == "google-image-edit-provider-response")
            .ok_or_else(|| anyhow::anyhow!("missing Google image provider response artifact"))?;
        assert_eq!(
            response.payload["body"]["candidates"][0]["content"]["parts"][0]["inlineData"]["data"]
                ["redacted"],
            "<redacted base64>"
        );
        Ok(())
    }

    #[tokio::test]
    async fn google_image_generator_refreshes_once_after_401() -> Result<()> {
        #[derive(Default)]
        struct Counts {
            requests: AtomicUsize,
            resolves: AtomicUsize,
            refreshes: AtomicUsize,
        }

        #[derive(Default)]
        struct FakeAuthProvider {
            counts: Arc<Counts>,
        }

        #[async_trait]
        impl RequestAuthProvider for FakeAuthProvider {
            async fn resolve(&self) -> anyhow::Result<ResolvedAuthMaterial> {
                self.counts.resolves.fetch_add(1, Ordering::SeqCst);
                Ok(ResolvedAuthMaterial {
                    headers: [(
                        "Authorization".to_string(),
                        "Bearer stale-google-image-token".to_string(),
                    )]
                    .into_iter()
                    .collect(),
                    base_url_override: None,
                    grant_id: None,
                    lease_id: None,
                })
            }

            async fn refresh(&self) -> anyhow::Result<ResolvedAuthMaterial> {
                self.counts.refreshes.fetch_add(1, Ordering::SeqCst);
                Ok(ResolvedAuthMaterial {
                    headers: [(
                        "Authorization".to_string(),
                        "Bearer fresh-google-image-token".to_string(),
                    )]
                    .into_iter()
                    .collect(),
                    base_url_override: None,
                    grant_id: None,
                    lease_id: None,
                })
            }

            async fn ensure_active(&self, _material: &ResolvedAuthMaterial) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let counts = Arc::new(Counts::default());
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server_counts = counts.clone();
        let server = tokio::spawn(async move {
            for expected_auth in [
                "Bearer stale-google-image-token",
                "Bearer fresh-google-image-token",
            ] {
                let (mut socket, _) = listener.accept().await.expect("server should accept");
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let read = socket
                        .read(&mut buffer)
                        .await
                        .expect("request read should succeed");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request_text = String::from_utf8(request).expect("request must be utf-8");
                let auth_header = request_text
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("authorization")
                                .then_some(value.trim())
                        })
                    })
                    .unwrap_or_default()
                    .to_string();
                let request_number = server_counts.requests.fetch_add(1, Ordering::SeqCst);
                assert_eq!(auth_header, expected_auth);
                let (status, body) = if request_number == 0 {
                    (
                        "401 Unauthorized",
                        json!({
                            "error": {
                                "message": "expired"
                            }
                        })
                        .to_string(),
                    )
                } else {
                    (
                        "200 OK",
                        json!({
                            "modelVersion": "gemini-3-pro-image-preview",
                            "candidates": [{
                                "content": {
                                    "parts": [{
                                        "inlineData": {
                                            "mimeType": "image/png",
                                            "data": super::BASE64_STANDARD.encode(b"png")
                                        }
                                    }]
                                }
                            }]
                        })
                        .to_string(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("response write should succeed");
            }
        });

        let generator = GoogleImageGenerator::new(
            GoogleProviderConfig {
                base_url: format!("http://{address}/v1beta"),
                api_key: None,
                request_auth_provider: Some(Arc::new(FakeAuthProvider {
                    counts: counts.clone(),
                })),
                ..GoogleProviderConfig::new("gemini-3-pro-image-preview", "unused-key")
            },
            Arc::new(NoopObserver),
        )?;

        let response = generator
            .generate(GoogleImageGenerationRequest {
                prompt: "render".to_string(),
                count: 1,
                size: Some("1024x1024".to_string()),
            })
            .await?;
        assert_eq!(response.images.len(), 1);
        assert_eq!(response.images[0].bytes, b"png");
        assert_eq!(counts.requests.load(Ordering::SeqCst), 2);
        assert_eq!(counts.resolves.load(Ordering::SeqCst), 1);
        assert_eq!(counts.refreshes.load(Ordering::SeqCst), 1);
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn google_image_editor_posts_inline_image_parts() -> Result<()> {
        let captured = Arc::new(Mutex::new(String::new()));
        let base_url = spawn_mock_server(
            200,
            &[("content-type", "application/json")],
            &json!({
                "modelVersion": "gemini-3-pro-image-preview",
                "candidates": [{
                    "content": {
                        "parts": [{
                            "inlineData": {
                                "mimeType": "image/png",
                                "data": super::BASE64_STANDARD.encode(b"edited")
                            }
                        }]
                    }
                }]
            })
            .to_string(),
            captured.clone(),
        )
        .await?;

        let mut config = GoogleProviderConfig::new("gemini-2.5-flash", "test-key");
        config.base_url = base_url;
        let editor = GoogleImageEditor::new(config, Arc::new(NoopObserver))?;
        let response = editor
            .edit(GoogleImageEditRequest {
                prompt: "edit".to_string(),
                images: vec![GoogleImageEditInput {
                    file_name: "plan.png".to_string(),
                    media_type: "image/png".to_string(),
                    bytes: b"source".to_vec(),
                }],
                count: 1,
                size: None,
            })
            .await?;
        let payload: Value = serde_json::from_str(&captured.lock())?;
        assert_eq!(
            payload["contents"][0]["parts"][0]["inlineData"]["mimeType"],
            json!("image/png")
        );
        assert_eq!(payload["contents"][0]["parts"][1]["text"], json!("edit"));
        assert_eq!(response.images.len(), 1);
        assert_eq!(response.images[0].bytes, b"edited");
        Ok(())
    }

    #[tokio::test]
    async fn google_image_editor_preserves_source_image_order() -> Result<()> {
        let captured = Arc::new(Mutex::new(String::new()));
        let base_url = spawn_mock_server(
            200,
            &[("content-type", "application/json")],
            &json!({
                "modelVersion": "gemini-3-pro-image-preview",
                "candidates": [{
                    "content": {
                        "parts": [{
                            "inlineData": {
                                "mimeType": "image/png",
                                "data": super::BASE64_STANDARD.encode(b"edited")
                            }
                        }]
                    }
                }]
            })
            .to_string(),
            captured.clone(),
        )
        .await?;

        let mut config = GoogleProviderConfig::new("gemini-2.5-flash", "test-key");
        config.base_url = base_url;
        let editor = GoogleImageEditor::new(config, Arc::new(NoopObserver))?;
        let response = editor
            .edit(GoogleImageEditRequest {
                prompt: "edit".to_string(),
                images: vec![
                    GoogleImageEditInput {
                        file_name: "primary.png".to_string(),
                        media_type: "image/png".to_string(),
                        bytes: b"primary".to_vec(),
                    },
                    GoogleImageEditInput {
                        file_name: "reference.jpeg".to_string(),
                        media_type: "image/jpeg".to_string(),
                        bytes: b"reference".to_vec(),
                    },
                ],
                count: 1,
                size: None,
            })
            .await?;
        let payload: Value = serde_json::from_str(&captured.lock())?;
        assert_eq!(
            payload["contents"][0]["parts"][0]["inlineData"]["mimeType"],
            json!("image/png")
        );
        assert_eq!(
            payload["contents"][0]["parts"][0]["inlineData"]["data"],
            json!(super::BASE64_STANDARD.encode(b"primary"))
        );
        assert_eq!(
            payload["contents"][0]["parts"][1]["inlineData"]["mimeType"],
            json!("image/jpeg")
        );
        assert_eq!(
            payload["contents"][0]["parts"][1]["inlineData"]["data"],
            json!(super::BASE64_STANDARD.encode(b"reference"))
        );
        assert_eq!(payload["contents"][0]["parts"][2]["text"], json!("edit"));
        assert_eq!(response.images.len(), 1);
        Ok(())
    }
}
