use std::collections::BTreeMap;
use std::fmt::{Debug, Formatter};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use futures_util::StreamExt;
use reqwest::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderValue, LOCATION, RETRY_AFTER,
};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
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
    DebugArtifactFormat, DebugCaptureLevel, NoopObserver, current_cancellation_token,
    headers_payload_for_level, interrupted_error, provider_payload_for_level,
};
use kheish_auth::{RequestAuthProvider, ResolvedAuthMaterial};
use kheish_codec::{digest_bytes, digest_json_value, digest_text};
use kheish_types::{InputContentPart, ToolCallRecord, model_max_output_tokens};

use super::attachments::{
    AttachmentRenderCache, PreparedImageAttachment, image_edit_attachment_hint_text,
    load_attachment_preview_image, load_document_attachment_text, load_image_attachment,
};
use super::errors::{safe_error_payload_for_level, sanitize_upstream_error_message};
use super::prompt::{
    NormalizedConversationItem, NormalizedProviderPrompt, normalize_provider_prompt,
};
use super::sse::{JsonSseEvent, parse_json_sse_frame, pop_sse_frame};
use super::transcription::{AudioTranscriptionRequest, AudioTranscriptionResponse};

const DEFAULT_OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const DEFAULT_OPENROUTER_MODEL: &str = "openai/gpt-5.4-mini";
const DEFAULT_OPENROUTER_IMAGE_MODEL: &str = "openai/gpt-5-image-mini";
const DEFAULT_OPENROUTER_TRANSCRIPTION_MODEL: &str = "openai/gpt-4o-mini-transcribe";
const DEFAULT_OPENROUTER_TTS_MODEL: &str = "openai/gpt-4o-mini-tts-2025-12-15";
const DEFAULT_OPENROUTER_TTS_VOICE: &str = "alloy";
const DEFAULT_OPENROUTER_TTS_FORMAT: &str = "pcm";
const OPENAI_STRICT_OPTIONAL_FIELD_LIMIT: usize = 12;
const OPENROUTER_AUDIO_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OPENROUTER_AUDIO_RESPONSE_BYTES: usize = 12 * 1024 * 1024;
const MAX_OPENROUTER_TRANSCRIPTION_REQUEST_BYTES: usize = 25 * 1024 * 1024;
const MAX_OPENROUTER_TRANSCRIPTION_RESPONSE_BYTES: usize = 1024 * 1024;
const OPENROUTER_DISCOVERY_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_OPENROUTER_IMAGE_RESPONSE_BYTES: usize = 24 * 1024 * 1024;
const MAX_OPENROUTER_IMAGE_DOWNLOAD_BYTES: usize = 16 * 1024 * 1024;
const MAX_OPENROUTER_IMAGE_REDIRECTS: usize = 5;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRouterModelCapabilities {
    pub tools: bool,
    pub structured_output: bool,
    pub text_input: bool,
    pub image_input: bool,
    pub audio_input: bool,
    pub text_output: bool,
    pub image_output: bool,
    pub audio_output: bool,
}

impl OpenRouterModelCapabilities {
    pub fn multimodal_input(&self) -> bool {
        self.image_input || self.audio_input
    }

    pub fn image_generation(&self) -> bool {
        self.text_input && self.image_output
    }

    pub fn image_edit(&self) -> bool {
        self.image_input && self.image_output
    }

    pub fn audio_generation(&self) -> bool {
        self.text_input && self.audio_output
    }

    pub fn transcription(&self) -> bool {
        self.audio_input && self.text_output
    }
}

#[derive(Clone)]
pub struct OpenRouterProviderConfig {
    pub model: String,
    pub api_key: Option<String>,
    pub request_auth_provider: Option<Arc<dyn RequestAuthProvider>>,
    pub base_url: String,
    pub default_max_output_tokens: u32,
    pub asset_root: Option<PathBuf>,
    pub(crate) attachment_cache: AttachmentRenderCache,
    pub model_capabilities: BTreeMap<String, OpenRouterModelCapabilities>,
}

impl OpenRouterProviderConfig {
    pub fn new(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        let model = resolve_openrouter_model(&model.into());
        Self {
            default_max_output_tokens: model_max_output_tokens(&model).default,
            model,
            api_key: Some(api_key.into()),
            request_auth_provider: None,
            base_url: DEFAULT_OPENROUTER_BASE_URL.to_string(),
            asset_root: None,
            attachment_cache: AttachmentRenderCache::default(),
            model_capabilities: BTreeMap::new(),
        }
    }

    pub fn from_env(
        model: impl Into<String>,
        env_var: impl AsRef<str>,
    ) -> Result<Self, ProviderError> {
        let env_var = env_var.as_ref();
        let api_key = std::env::var(env_var).map_err(|_| ProviderError {
            message: format!("missing OpenRouter API key in environment variable {env_var}"),
            retryable: false,
            retry_after_ms: None,
        })?;
        Ok(Self::new(model, api_key))
    }

    pub fn with_request_auth_provider(
        model: impl Into<String>,
        request_auth_provider: Arc<dyn RequestAuthProvider>,
    ) -> Self {
        let model = resolve_openrouter_model(&model.into());
        Self {
            default_max_output_tokens: model_max_output_tokens(&model).default,
            model,
            api_key: None,
            request_auth_provider: Some(request_auth_provider),
            base_url: DEFAULT_OPENROUTER_BASE_URL.to_string(),
            asset_root: None,
            attachment_cache: AttachmentRenderCache::default(),
            model_capabilities: BTreeMap::new(),
        }
    }

    pub fn with_model_capabilities(
        mut self,
        capabilities: BTreeMap<String, OpenRouterModelCapabilities>,
    ) -> Self {
        self.model_capabilities = capabilities;
        self
    }

    pub fn capability_for_model(&self, model: &str) -> Option<&OpenRouterModelCapabilities> {
        self.model_capabilities.get(model.trim())
    }
}

pub async fn fetch_openrouter_model_capabilities(
    config: &OpenRouterProviderConfig,
) -> Result<BTreeMap<String, OpenRouterModelCapabilities>, ProviderError> {
    let provider = OpenRouterProvider::new(config.clone())?;
    let endpoint = openrouter_models_endpoint(&provider.config.base_url)?;
    let material = provider.auth_material(false).await?;
    let headers = provider.headers_from_material(&material)?;
    provider.ensure_auth_material_active(&material).await?;
    let response = provider
        .client
        .get(&endpoint)
        .timeout(OPENROUTER_DISCOVERY_REQUEST_TIMEOUT)
        .headers(headers)
        .send()
        .await
        .map_err(map_transport_error)?;
    if !response.status().is_success() {
        return Err(map_http_error(response).await);
    }
    let payload = response
        .json::<Value>()
        .await
        .map_err(|error| ProviderError {
            message: format!("failed to decode OpenRouter model discovery response: {error}"),
            retryable: false,
            retry_after_ms: None,
        })?;
    Ok(parse_openrouter_model_capabilities(&payload))
}

pub fn parse_openrouter_model_capabilities(
    payload: &Value,
) -> BTreeMap<String, OpenRouterModelCapabilities> {
    let mut models = BTreeMap::new();
    let entries = payload
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for entry in entries {
        let Some(model_id) = entry
            .get("id")
            .or_else(|| entry.get("slug"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let architecture = entry.get("architecture").unwrap_or(&Value::Null);
        let input_modalities =
            openrouter_string_array(architecture.get("input_modalities").unwrap_or(&Value::Null));
        let output_modalities = openrouter_string_array(
            architecture
                .get("output_modalities")
                .unwrap_or(&Value::Null),
        );
        let supported_parameters =
            openrouter_string_array(entry.get("supported_parameters").unwrap_or(&Value::Null));
        models.insert(
            model_id.to_string(),
            OpenRouterModelCapabilities {
                tools: supported_parameters.contains("tools")
                    || supported_parameters.contains("tool_choice"),
                structured_output: supported_parameters.contains("response_format")
                    || supported_parameters.contains("structured_outputs"),
                text_input: input_modalities.contains("text"),
                image_input: input_modalities.contains("image"),
                audio_input: input_modalities.contains("audio"),
                text_output: output_modalities.contains("text")
                    || output_modalities.contains("transcription"),
                image_output: output_modalities.contains("image"),
                audio_output: output_modalities.contains("audio")
                    || output_modalities.contains("speech"),
            },
        );
    }
    models
}

fn openrouter_string_array(value: &Value) -> std::collections::BTreeSet<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect()
}

impl Debug for OpenRouterProviderConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenRouterProviderConfig")
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
            .field("model_capabilities_len", &self.model_capabilities.len())
            .finish()
    }
}

#[derive(Clone)]
pub struct OpenRouterProvider {
    client: Client,
    config: OpenRouterProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

impl OpenRouterProvider {
    pub fn new(config: OpenRouterProviderConfig) -> Result<Self, ProviderError> {
        Self::with_observer(config, Arc::new(NoopObserver))
    }

    pub fn with_observer(
        config: OpenRouterProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        let client = Client::builder().build().map_err(|error| ProviderError {
            message: format!("failed to build OpenRouter HTTP client: {error}"),
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

    fn external_action_target(&self, endpoint: &str) -> String {
        format!("openrouter:{}", safe_url_audit_target(endpoint))
    }

    fn record_external_request(
        &self,
        endpoint: &str,
        body: &Value,
        grant_id: Option<String>,
    ) -> Result<(), ProviderError> {
        self.observer
            .record_external_action(external_action_trace_with_grant_id(
                "request",
                "model_provider",
                self.external_action_target(endpoint),
                Some(digest_json_value(body).unwrap_or_else(|_| "unknown".to_string())),
                None,
                None,
                grant_id,
            ))
            .map_err(provider_audit_error)
    }

    fn record_external_response(
        &self,
        target: &str,
        status: u16,
        body: Option<&Value>,
        grant_id: Option<String>,
    ) -> Result<(), ProviderError> {
        self.observer
            .record_external_action(external_action_trace_with_grant_id(
                "response",
                "model_provider",
                target.to_string(),
                None,
                body.map(|value| {
                    digest_json_value(value).unwrap_or_else(|_| "unknown".to_string())
                }),
                Some(status.to_string()),
                grant_id,
            ))
            .map_err(provider_audit_error)
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
                message: format!("failed to resolve OpenRouter auth material: {error}"),
                retryable: false,
                retry_after_ms: None,
            });
        }
        let api_key = self.config.api_key.clone().ok_or_else(|| ProviderError {
            message: "missing OpenRouter API key".to_string(),
            retryable: false,
            retry_after_ms: None,
        })?;
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {api_key}"));
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
                    message: format!("OpenRouter auth material is no longer active: {error}"),
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
        let mut headers = HeaderMap::new();
        for (name, value) in &material.headers {
            let header_name = if name.eq_ignore_ascii_case("authorization") {
                AUTHORIZATION
            } else {
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                    ProviderError {
                        message: format!("invalid OpenRouter header name `{name}`: {error}"),
                        retryable: false,
                        retry_after_ms: None,
                    }
                })?
            };
            headers.insert(
                header_name,
                HeaderValue::from_str(value).map_err(|error| ProviderError {
                    message: format!("invalid OpenRouter header `{name}`: {error}"),
                    retryable: false,
                    retry_after_ms: None,
                })?,
            );
        }
        Ok(headers)
    }

    fn build_request_body(&self, request: &ModelRuntimeRequest) -> Result<Value, ProviderError> {
        let normalized = normalize_provider_prompt(&request.prompt);
        let effective_model = request
            .generation
            .model
            .clone()
            .unwrap_or_else(|| self.config.model.clone());
        self.validate_model_capabilities(&effective_model, request, &normalized)?;
        let mut messages = openrouter_messages(
            &normalized.conversation,
            self.config.asset_root.as_deref(),
            &self.config.attachment_cache,
        )?;
        let instructions = normalized.instructions.join("\n\n");
        if !instructions.trim().is_empty() {
            messages.insert(
                0,
                json!({
                    "role": "system",
                    "content": instructions,
                }),
            );
        }

        let mut body = serde_json::Map::new();
        body.insert("model".to_string(), Value::String(effective_model.clone()));
        body.insert("messages".to_string(), Value::Array(messages));
        body.insert("stream".to_string(), Value::Bool(true));
        body.insert(
            "max_tokens".to_string(),
            Value::Number(
                request
                    .generation
                    .max_output_tokens
                    .unwrap_or_else(|| model_max_output_tokens(&effective_model).default)
                    .into(),
            ),
        );
        body.insert(
            "parallel_tool_calls".to_string(),
            Value::Bool(request.generation.allow_parallel_tool_calls),
        );
        if let Some(temperature) = request.generation.temperature {
            body.insert(
                "temperature".to_string(),
                serde_json::Number::from_f64(temperature as f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
            );
        }
        match &request.generation.response_format {
            ResponseFormat::Text => {}
            ResponseFormat::StructuredJson { schema } => {
                let schema = openrouter_structured_response_schema(schema)?;
                body.insert(
                    "response_format".to_string(),
                    json!({
                        "type": "json_schema",
                        "json_schema": {
                            "name": "kheish_response",
                            "strict": true,
                            "schema": schema,
                        }
                    }),
                );
            }
        }

        let tools = if matches!(request.generation.tool_choice, ToolChoice::None) {
            Vec::new()
        } else {
            request
                .available_tools
                .iter()
                .map(|tool| {
                    let (parameters, strict) = openai_tool_parameters_schema(&tool.input_schema);
                    let mut function = json!({
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": parameters,
                    });
                    if strict {
                        function["strict"] = Value::Bool(true);
                    } else {
                        function["strict"] = Value::Bool(false);
                    }
                    json!({
                        "type": "function",
                        "function": function,
                    })
                })
                .collect::<Vec<_>>()
        };
        if !tools.is_empty() {
            body.insert("tools".to_string(), Value::Array(tools));
            if let Some(tool_choice) = openrouter_tool_choice(
                !request.available_tools.is_empty(),
                &request.generation.tool_choice,
            ) {
                body.insert("tool_choice".to_string(), tool_choice);
            }
        }

        Ok(Value::Object(body))
    }

    fn validate_model_capabilities(
        &self,
        model: &str,
        request: &ModelRuntimeRequest,
        normalized: &NormalizedProviderPrompt,
    ) -> Result<(), ProviderError> {
        let Some(capabilities) = self.config.capability_for_model(model) else {
            return Ok(());
        };
        if !request.available_tools.is_empty()
            && !matches!(request.generation.tool_choice, ToolChoice::None)
            && !capabilities.tools
        {
            return Err(openrouter_unsupported_model_capability(model, "tools"));
        }
        if matches!(
            request.generation.response_format,
            ResponseFormat::StructuredJson { .. }
        ) && !capabilities.structured_output
        {
            return Err(openrouter_unsupported_model_capability(
                model,
                "structured_output",
            ));
        }
        if openrouter_prompt_uses_image_input(normalized) && !capabilities.image_input {
            return Err(openrouter_unsupported_model_capability(model, "vision"));
        }
        Ok(())
    }

    fn record_provider_request(
        &self,
        request: &ModelRuntimeRequest,
        endpoint: &str,
        headers: &HeaderMap,
        body: &Value,
        grant_id: Option<String>,
    ) -> Result<(), ProviderError> {
        self.record_external_request(endpoint, body, grant_id)?;
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
                "provider": "openrouter",
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
        body: Option<&Value>,
        grant_id: Option<String>,
    ) -> Result<(), ProviderError> {
        self.record_external_response(target, status, body, grant_id)?;
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
                "provider": "openrouter",
                "status": status,
                "headers": headers_payload_for_level(level, headers),
                "body": body.map(|value| provider_payload_for_level(level, value)),
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
        self.observer
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

    fn record_media_provider_request(
        &self,
        artifact_name: &str,
        endpoint: &str,
        headers: &HeaderMap,
        body: &Value,
        grant_id: Option<String>,
    ) -> Result<(), ProviderError> {
        self.record_external_request(endpoint, body, grant_id)?;
        let level = self.debug_level();
        if level.is_enabled() {
            self.observer.record_debug_artifact(DebugArtifact::new(
                level,
                None,
                None,
                artifact_name,
                DebugArtifactFormat::Json,
                json!({
                    "provider": "openrouter",
                    "method": "POST",
                    "url": safe_url_debug_target(endpoint),
                    "headers": headers_payload_for_level(level, headers),
                    "body": provider_payload_for_level(level, body),
                }),
            ));
        }
        Ok(())
    }

    fn record_media_provider_response(
        &self,
        artifact_name: &str,
        target: &str,
        status: u16,
        headers: &HeaderMap,
        body: &Value,
        grant_id: Option<String>,
    ) -> Result<(), ProviderError> {
        self.record_external_response(target, status, Some(body), grant_id)?;
        let level = self.debug_level();
        if level.is_enabled() {
            self.observer.record_debug_artifact(DebugArtifact::new(
                level,
                None,
                None,
                artifact_name,
                DebugArtifactFormat::Json,
                json!({
                    "provider": "openrouter",
                    "status": status,
                    "headers": headers_payload_for_level(level, headers),
                    "body": provider_payload_for_level(level, body),
                }),
            ));
        }
        Ok(())
    }

    fn record_provider_event(&self, request: &ModelRuntimeRequest, event: &JsonSseEvent) {
        let level = self.debug_level();
        if !level.is_enabled() {
            return;
        }
        let payload = if event.payload.get("error").is_some() {
            safe_error_payload_for_level(level, &openrouter_error_event_payload(event))
        } else {
            provider_payload_for_level(level, &event.payload)
        };
        self.observer.record_debug_artifact(DebugArtifact::new(
            level,
            Some(request.turn),
            Some(request.attempt),
            &format!("{}provider-events", request.kind.artifact_prefix()),
            DebugArtifactFormat::JsonLines,
            json!({
                "provider": "openrouter",
                "event_type": event.event_type,
                "payload": payload,
            }),
        ));
    }
}

fn openrouter_error_event_payload(event: &JsonSseEvent) -> Value {
    let error = event.payload.get("error").unwrap_or(&event.payload);
    json!({
        "message": sanitize_upstream_error_message(
            "OpenRouter",
            "stream error",
            None,
            error.get("type").and_then(Value::as_str),
            error
                .get("code")
                .and_then(Value::as_i64)
                .map(|value| value.to_string())
                .as_deref()
                .or_else(|| error.get("code").and_then(Value::as_str)),
            error.get("message").and_then(Value::as_str),
        ),
        "error_type": error.get("type").and_then(Value::as_str),
        "error_code": error.get("code").cloned(),
        "has_error_object": event.payload.get("error").is_some_and(Value::is_object),
    })
}

#[async_trait]
impl ModelProvider for OpenRouterProvider {
    async fn stream(
        &self,
        request: ModelRuntimeRequest,
        sink: ModelEventSink,
    ) -> std::result::Result<(), ProviderError> {
        let mut force_refresh = false;
        let (response, response_target, response_grant_id) = loop {
            let auth_material = self.auth_material(force_refresh).await?;
            let grant_id = auth_material.grant_id.clone();
            let endpoint = auth_material
                .base_url_override
                .clone()
                .unwrap_or_else(|| self.config.base_url.clone());
            let body = self.build_request_body(&request)?;
            let headers = self.headers_from_material(&auth_material)?;
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
                        self.external_action_target(&endpoint),
                        &mapped.message,
                        grant_id.clone(),
                    )?;
                    return Err(mapped);
                }
            };
            if response.status() == StatusCode::UNAUTHORIZED
                && self.config.request_auth_provider.is_some()
                && !force_refresh
            {
                self.record_provider_failure(
                    self.external_action_target(&endpoint),
                    "401-refresh",
                    grant_id.clone(),
                )?;
                force_refresh = true;
                continue;
            }
            break (response, self.external_action_target(&endpoint), grant_id);
        };

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let response_headers = response.headers().clone();
            let error = map_http_error(response).await;
            let payload = json!({
                "message": error.message,
                "retryable": error.retryable,
                "retry_after_ms": error.retry_after_ms,
            });
            self.record_provider_response(
                &request,
                &response_target,
                status,
                &response_headers,
                Some(&payload),
                response_grant_id.clone(),
            )?;
            return Err(error);
        }

        self.record_provider_response(
            &request,
            &response_target,
            response.status().as_u16(),
            response.headers(),
            None,
            response_grant_id.clone(),
        )?;

        let mut emitted_message_id = false;
        let mut function_calls = BTreeMap::<u32, PartialFunctionCall>::new();
        let mut structured_text = String::new();
        let mut pending_finish_reason = None::<ModelFinishReason>;
        let mut pending_tool_stop = false;
        let mut usage_emitted = false;
        let mut buffer = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    let mapped = map_transport_error(error);
                    self.record_provider_failure(
                        &response_target,
                        &mapped.message,
                        response_grant_id.clone(),
                    )?;
                    return Err(mapped);
                }
            };
            if !chunk.is_empty() {
                sink.mark_activity().map_err(map_sink_error)?;
            }
            buffer.extend_from_slice(&chunk);
            while let Some(frame) = pop_sse_frame(&mut buffer) {
                if frame.iter().all(|byte| byte.is_ascii_whitespace()) {
                    continue;
                }
                let parsed = parse_json_sse_frame(&frame, "OpenRouter").map_err(|error| {
                    self.record_provider_failure(
                        &response_target,
                        &error.message,
                        response_grant_id.clone(),
                    )
                    .err()
                    .unwrap_or(error)
                })?;
                let Some(event) = parsed else {
                    continue;
                };
                self.record_provider_event(&request, &event);
                handle_openrouter_sse_event(
                    &event,
                    &request.generation.response_format,
                    &sink,
                    &mut emitted_message_id,
                    &mut function_calls,
                    &mut structured_text,
                    &mut pending_finish_reason,
                    &mut pending_tool_stop,
                    &mut usage_emitted,
                )
                .map_err(|error| {
                    self.record_provider_failure(
                        &response_target,
                        &error.message,
                        response_grant_id.clone(),
                    )
                    .err()
                    .unwrap_or(error)
                })?;
            }
        }

        if !buffer.iter().all(|byte| byte.is_ascii_whitespace()) {
            if let Some(event) = parse_json_sse_frame(&buffer, "OpenRouter").map_err(|error| {
                self.record_provider_failure(
                    &response_target,
                    &error.message,
                    response_grant_id.clone(),
                )
                .err()
                .unwrap_or(error)
            })? {
                self.record_provider_event(&request, &event);
                handle_openrouter_sse_event(
                    &event,
                    &request.generation.response_format,
                    &sink,
                    &mut emitted_message_id,
                    &mut function_calls,
                    &mut structured_text,
                    &mut pending_finish_reason,
                    &mut pending_tool_stop,
                    &mut usage_emitted,
                )
                .map_err(|error| {
                    self.record_provider_failure(
                        &response_target,
                        &error.message,
                        response_grant_id.clone(),
                    )
                    .err()
                    .unwrap_or(error)
                })?;
            }
        }

        if pending_tool_stop {
            emit_pending_tool_calls(&mut function_calls, &sink).map_err(|error| {
                self.record_provider_failure(
                    &response_target,
                    &error.message,
                    response_grant_id.clone(),
                )
                .err()
                .unwrap_or(error)
            })?;
        }
        if matches!(
            request.generation.response_format,
            ResponseFormat::StructuredJson { .. }
        ) && !structured_text.trim().is_empty()
        {
            let value = serde_json::from_str::<Value>(structured_text.trim()).map_err(|error| {
                let mapped = ProviderError {
                    message: format!("OpenRouter structured response was not valid JSON: {error}"),
                    retryable: true,
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
            sink.emit(ModelStreamEvent::StructuredOutput { value })
                .map_err(map_sink_error)?;
        }
        if let Some(reason) = pending_finish_reason {
            sink.emit(ModelStreamEvent::Stop { reason })
                .map_err(map_sink_error)?;
        } else if usage_emitted {
            sink.emit(ModelStreamEvent::Stop {
                reason: ModelFinishReason::Completed,
            })
            .map_err(map_sink_error)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
struct PartialFunctionCall {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
}

fn handle_openrouter_sse_event(
    event: &JsonSseEvent,
    response_format: &ResponseFormat,
    sink: &ModelEventSink,
    emitted_message_id: &mut bool,
    function_calls: &mut BTreeMap<u32, PartialFunctionCall>,
    structured_text: &mut String,
    pending_finish_reason: &mut Option<ModelFinishReason>,
    pending_tool_stop: &mut bool,
    usage_emitted: &mut bool,
) -> Result<(), ProviderError> {
    let payload = &event.payload;
    if !*emitted_message_id {
        if let Some(id) = payload.get("id").and_then(Value::as_str) {
            sink.emit(ModelStreamEvent::MessageId {
                value: id.to_string(),
            })
            .map_err(map_sink_error)?;
            *emitted_message_id = true;
        }
    }
    if let Some(usage) = payload.get("usage") {
        sink.emit(ModelStreamEvent::Usage {
            usage: parse_usage(usage),
        })
        .map_err(map_sink_error)?;
        *usage_emitted = true;
    }
    let choices = payload
        .get("choices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for choice in choices {
        if let Some(delta) = choice.get("delta") {
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                match response_format {
                    ResponseFormat::Text => sink
                        .emit(ModelStreamEvent::TextDelta {
                            text: content.to_string(),
                        })
                        .map_err(map_sink_error)?,
                    ResponseFormat::StructuredJson { .. } => {
                        structured_text.push_str(content);
                    }
                }
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for tool_call in tool_calls {
                    let index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                    let entry = function_calls.entry(index).or_default();
                    if let Some(id) = tool_call.get("id").and_then(Value::as_str) {
                        entry.call_id = Some(id.to_string());
                    }
                    if let Some(function) = tool_call.get("function") {
                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                            entry.name = Some(name.to_string());
                        }
                        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                            entry.arguments.push_str(arguments);
                        }
                    }
                }
            }
        }
        if let Some(finish_reason) = choice.get("finish_reason").and_then(Value::as_str) {
            let reason = openrouter_finish_reason(finish_reason);
            if matches!(reason, ModelFinishReason::ToolCalls) {
                *pending_tool_stop = true;
            }
            *pending_finish_reason = Some(reason);
        }
    }
    if let Some(error) = payload.get("error") {
        return Err(ProviderError {
            message: sanitize_upstream_error_message(
                "OpenRouter",
                "stream error",
                None,
                error.get("type").and_then(Value::as_str),
                error.get("code").and_then(Value::as_str),
                error.get("message").and_then(Value::as_str),
            ),
            retryable: false,
            retry_after_ms: None,
        });
    }
    Ok(())
}

fn emit_pending_tool_calls(
    function_calls: &mut BTreeMap<u32, PartialFunctionCall>,
    sink: &ModelEventSink,
) -> Result<(), ProviderError> {
    let pending = std::mem::take(function_calls);
    for (_, partial) in pending {
        let call_id = partial.call_id.ok_or_else(|| ProviderError {
            message: "missing OpenRouter tool call id".to_string(),
            retryable: true,
            retry_after_ms: None,
        })?;
        let name = partial.name.ok_or_else(|| ProviderError {
            message: "missing OpenRouter tool call name".to_string(),
            retryable: true,
            retry_after_ms: None,
        })?;
        let input = parse_function_arguments(&partial.arguments)?;
        sink.emit(ModelStreamEvent::ToolCall {
            call: ToolCallRecord {
                id: call_id,
                name,
                input,
                assistant_message_id: None,
                assistant_provider_response_id: None,
            },
        })
        .map_err(map_sink_error)?;
    }
    Ok(())
}

fn openrouter_messages(
    conversation: &[NormalizedConversationItem],
    asset_root: Option<&Path>,
    cache: &AttachmentRenderCache,
) -> Result<Vec<Value>, ProviderError> {
    let mut messages = Vec::new();
    for item in conversation {
        match item {
            NormalizedConversationItem::UserMessage {
                content,
                content_parts,
                attachments,
                ..
            } => {
                let content_value =
                    openrouter_user_content(content, content_parts, attachments, asset_root, cache)
                        .map_err(|error| ProviderError {
                            message: format!("failed to prepare OpenRouter user content: {error}"),
                            retryable: false,
                            retry_after_ms: None,
                        })?;
                if content_value.is_null() {
                    continue;
                }
                messages.push(json!({
                    "role": "user",
                    "content": content_value,
                }));
            }
            NormalizedConversationItem::AssistantMessage { content, .. } => {
                messages.push(json!({
                    "role": "assistant",
                    "content": content,
                }));
            }
            NormalizedConversationItem::AssistantToolCalls { calls, .. } => {
                let tool_calls = calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": serde_json::to_string(&call.input)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            }
                        })
                    })
                    .collect::<Vec<_>>();
                messages.push(json!({
                    "role": "assistant",
                    "content": Value::Null,
                    "tool_calls": tool_calls,
                }));
            }
            NormalizedConversationItem::ToolResults { results } => {
                for result in results {
                    let content = match &result.output {
                        Value::String(value) => value.clone(),
                        value => {
                            serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
                        }
                    };
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": result.call_id,
                        "content": content,
                    }));
                }
            }
        }
    }
    Ok(messages)
}

fn openrouter_user_content(
    fallback_content: &str,
    content_parts: &[InputContentPart],
    attachments: &[kheish_types::AttachmentRef],
    asset_root: Option<&Path>,
    cache: &AttachmentRenderCache,
) -> anyhow::Result<Value> {
    let mut parts = Vec::new();
    if !content_parts.is_empty() {
        for part in content_parts {
            match part {
                InputContentPart::Text { text } if !text.trim().is_empty() => parts.push(json!({
                    "type": "text",
                    "text": text,
                })),
                InputContentPart::Text { .. } => {}
                InputContentPart::Attachment { attachment } => {
                    append_openrouter_attachment_parts(&mut parts, attachment, asset_root, cache)?;
                }
            }
        }
    } else {
        if !fallback_content.trim().is_empty() {
            parts.push(json!({
                "type": "text",
                "text": fallback_content,
            }));
        }
        for attachment in attachments {
            append_openrouter_attachment_parts(&mut parts, attachment, asset_root, cache)?;
        }
    }

    match parts.len() {
        0 => Ok(Value::Null),
        1 if parts[0].get("type").and_then(Value::as_str) == Some("text") => Ok(parts[0]
            .get("text")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()))),
        _ => Ok(Value::Array(parts)),
    }
}

fn append_openrouter_attachment_parts(
    parts: &mut Vec<Value>,
    attachment: &kheish_types::AttachmentRef,
    asset_root: Option<&Path>,
    cache: &AttachmentRenderCache,
) -> anyhow::Result<()> {
    if let Some(image) = load_image_attachment(attachment, asset_root, cache)? {
        push_openrouter_image_attachment(parts, attachment, &image);
        return Ok(());
    }
    if let Some(preview) = load_attachment_preview_image(attachment, asset_root, cache)? {
        parts.push(json!({
            "type": "image_url",
            "image_url": {
                "url": preview.data_url(),
            }
        }));
    }
    if let Some(text) = load_document_attachment_text(attachment, asset_root, cache)? {
        parts.push(json!({
            "type": "text",
            "text": text,
        }));
    }
    Ok(())
}

fn push_openrouter_image_attachment(
    parts: &mut Vec<Value>,
    attachment: &kheish_types::AttachmentRef,
    image: &PreparedImageAttachment,
) {
    if let Some(text) = image_edit_attachment_hint_text(attachment) {
        parts.push(json!({
            "type": "text",
            "text": text,
        }));
    }
    parts.push(json!({
        "type": "image_url",
        "image_url": {
            "url": image.data_url(),
        }
    }));
}

fn openrouter_tool_choice(has_available_tools: bool, tool_choice: &ToolChoice) -> Option<Value> {
    if !has_available_tools {
        return None;
    }
    match tool_choice {
        ToolChoice::Auto => Some(Value::String("auto".to_string())),
        ToolChoice::Required => Some(Value::String("required".to_string())),
        ToolChoice::Specific { name } => Some(json!({
            "type": "function",
            "function": {
                "name": name,
            }
        })),
        ToolChoice::None => Some(Value::String("none".to_string())),
    }
}

fn openai_tool_parameters_schema(schema: &Value) -> (Value, bool) {
    if openai_optional_property_count(schema) > OPENAI_STRICT_OPTIONAL_FIELD_LIMIT {
        return (schema.clone(), false);
    }
    match openai_strict_tool_schema(schema) {
        Some(strict_schema) => (strict_schema, true),
        None => (schema.clone(), false),
    }
}

fn openrouter_structured_response_schema(
    schema: &crate::model::StructuredFieldSchema,
) -> Result<Value, ProviderError> {
    let schema = schema.to_json_schema();
    openai_strict_tool_schema(&schema).ok_or_else(|| ProviderError {
        message: "OpenRouter structured response schema is not strict-compatible".to_string(),
        retryable: false,
        retry_after_ms: None,
    })
}

fn openai_strict_tool_schema(schema: &Value) -> Option<Value> {
    let Value::Object(map) = schema else {
        return Some(schema.clone());
    };
    let mut strict_map = map.clone();
    match map.get("type") {
        Some(Value::String(kind)) if kind == "object" => {
            if map.get("additionalProperties") != Some(&Value::Bool(false)) {
                return None;
            }
            let properties = match map.get("properties") {
                Some(Value::Object(properties)) => properties,
                Some(_) => return None,
                None => {
                    strict_map.insert("required".to_string(), Value::Array(Vec::new()));
                    return Some(Value::Object(strict_map));
                }
            };
            let required = map
                .get("required")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<std::collections::BTreeSet<_>>()
                })
                .unwrap_or_default();
            let mut strict_properties = serde_json::Map::new();
            let mut strict_required = Vec::with_capacity(properties.len());
            for (name, value) in properties {
                let mut child = openai_strict_tool_schema(value)?;
                if !required.contains(name) {
                    child = openai_nullable_schema(child)?;
                }
                strict_properties.insert(name.clone(), child);
                strict_required.push(Value::String(name.clone()));
            }
            strict_map.insert("properties".to_string(), Value::Object(strict_properties));
            strict_map.insert("required".to_string(), Value::Array(strict_required));
            strict_map.insert("additionalProperties".to_string(), Value::Bool(false));
            Some(Value::Object(strict_map))
        }
        Some(Value::String(kind)) if kind == "array" => {
            let items = map.get("items")?;
            strict_map.insert("items".to_string(), openai_strict_tool_schema(items)?);
            Some(Value::Object(strict_map))
        }
        Some(Value::Array(kinds))
            if kinds
                .iter()
                .any(|kind| matches!(kind, Value::String(value) if value == "object")) =>
        {
            None
        }
        Some(Value::Array(kinds))
            if kinds
                .iter()
                .any(|kind| matches!(kind, Value::String(value) if value == "array")) =>
        {
            let items = map.get("items")?;
            strict_map.insert("items".to_string(), openai_strict_tool_schema(items)?);
            Some(Value::Object(strict_map))
        }
        _ => Some(Value::Object(strict_map)),
    }
}

fn openai_nullable_schema(schema: Value) -> Option<Value> {
    let Value::Object(mut map) = schema else {
        return None;
    };
    let nullable = match map.get("type") {
        Some(Value::String(kind)) => Value::Array(vec![
            Value::String(kind.clone()),
            Value::String("null".to_string()),
        ]),
        Some(Value::Array(kinds)) => {
            let mut values = kinds.clone();
            if !values
                .iter()
                .any(|kind| matches!(kind, Value::String(value) if value == "null"))
            {
                values.push(Value::String("null".to_string()));
            }
            Value::Array(values)
        }
        _ => return None,
    };
    map.insert("type".to_string(), nullable);
    Some(Value::Object(map))
}

fn openai_optional_property_count(schema: &Value) -> usize {
    let Value::Object(map) = schema else {
        return 0;
    };

    let mut count = 0;
    if let Some(Value::Object(properties)) = map.get("properties") {
        let required = map
            .get("required")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<std::collections::BTreeSet<_>>()
            })
            .unwrap_or_default();
        count += properties
            .keys()
            .filter(|name| !required.contains(name.as_str()))
            .count();
        count += properties
            .values()
            .map(openai_optional_property_count)
            .sum::<usize>();
    }
    if let Some(items) = map.get("items") {
        count += openai_optional_property_count(items);
    }
    count
}

fn parse_function_arguments(arguments: &str) -> Result<Value, ProviderError> {
    if arguments.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(arguments).map_err(|error| ProviderError {
        message: format!("OpenRouter tool call arguments were not valid JSON: {error}"),
        retryable: true,
        retry_after_ms: None,
    })
}

fn parse_usage(usage: &Value) -> kheish_types::ModelUsage {
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let cost_usd = usage
        .get("cost")
        .and_then(Value::as_f64)
        .or_else(|| {
            usage
                .get("total_cost")
                .and_then(Value::as_f64)
                .or_else(|| usage.get("estimated_cost").and_then(Value::as_f64))
        })
        .unwrap_or_default();
    kheish_types::ModelUsage {
        input_tokens,
        output_tokens,
        cost_usd,
    }
}

fn openrouter_finish_reason(reason: &str) -> ModelFinishReason {
    match reason {
        "stop" => ModelFinishReason::Completed,
        "length" => ModelFinishReason::MaxTokens,
        "tool_calls" => ModelFinishReason::ToolCalls,
        "content_filter" => ModelFinishReason::Blocked,
        _ => ModelFinishReason::Unknown,
    }
}

fn map_transport_error(error: reqwest::Error) -> ProviderError {
    ProviderError {
        message: format!("OpenRouter transport error: {error}"),
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

async fn map_http_error(response: reqwest::Response) -> ProviderError {
    let status = response.status();
    let retry_after_ms = parse_retry_after_ms(response.headers());
    let body_text = response.text().await.unwrap_or_default();
    openrouter_http_error_from_text(status, retry_after_ms, &body_text)
}

fn openrouter_http_error_from_text(
    status: StatusCode,
    retry_after_ms: Option<u64>,
    body_text: &str,
) -> ProviderError {
    let parsed = serde_json::from_str::<Value>(body_text).ok();
    let error = parsed
        .as_ref()
        .and_then(|value| value.get("error"))
        .unwrap_or_else(|| parsed.as_ref().unwrap_or(&Value::Null));
    ProviderError {
        message: sanitize_upstream_error_message(
            "OpenRouter",
            "request error",
            Some(status),
            error.get("type").and_then(Value::as_str),
            error
                .get("code")
                .and_then(Value::as_i64)
                .map(|value| value.to_string())
                .as_deref()
                .or_else(|| error.get("code").and_then(Value::as_str)),
            error.get("message").and_then(Value::as_str),
        ),
        retryable: status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error(),
        retry_after_ms,
    }
}

fn parse_retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| seconds.saturating_mul(1_000))
}

fn map_sink_error(error: anyhow::Error) -> ProviderError {
    ProviderError {
        message: format!("OpenRouter sink error: {error}"),
        retryable: false,
        retry_after_ms: None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRouterGeneratedImage {
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub revised_prompt: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRouterImageGenerationRequest {
    pub prompt: String,
    pub count: u32,
    pub size: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRouterImageGenerationResponse {
    pub model: String,
    pub images: Vec<OpenRouterGeneratedImage>,
    pub text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRouterImageEditInput {
    pub file_name: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRouterImageEditRequest {
    pub prompt: String,
    pub images: Vec<OpenRouterImageEditInput>,
    pub count: u32,
    pub size: Option<String>,
}

pub struct OpenRouterImageGenerator {
    provider: OpenRouterProvider,
}

impl OpenRouterImageGenerator {
    pub fn new(
        mut config: OpenRouterProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        config.model = resolve_openrouter_image_model(&config.model);
        Ok(Self {
            provider: OpenRouterProvider::with_observer(config, observer)?,
        })
    }

    pub async fn generate(
        &self,
        request: OpenRouterImageGenerationRequest,
    ) -> Result<OpenRouterImageGenerationResponse, ProviderError> {
        if request.prompt.trim().is_empty() {
            return Err(ProviderError {
                message: "OpenRouter image generation prompt must not be empty".to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
        if !(1..=10).contains(&request.count) {
            return Err(ProviderError {
                message: "OpenRouter image generation count must be between 1 and 10".to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
        if let Some(capabilities) = self
            .provider
            .config
            .capability_for_model(&self.provider.config.model)
            && !capabilities.image_generation()
        {
            return Err(openrouter_unsupported_model_capability(
                &self.provider.config.model,
                "image_output",
            ));
        }

        let body = build_openrouter_image_body(
            &self.provider.config.model,
            request.prompt.trim(),
            &[],
            request.size.as_deref(),
        )?;
        collect_openrouter_images(&self.provider, body, request.count).await
    }
}

pub struct OpenRouterImageEditor {
    provider: OpenRouterProvider,
}

impl OpenRouterImageEditor {
    pub fn new(
        mut config: OpenRouterProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        config.model = resolve_openrouter_image_model(&config.model);
        Ok(Self {
            provider: OpenRouterProvider::with_observer(config, observer)?,
        })
    }

    pub async fn edit(
        &self,
        request: OpenRouterImageEditRequest,
    ) -> Result<OpenRouterImageGenerationResponse, ProviderError> {
        if request.prompt.trim().is_empty() {
            return Err(ProviderError {
                message: "OpenRouter image edit prompt must not be empty".to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
        if request.images.is_empty() {
            return Err(ProviderError {
                message: "OpenRouter image edit requires at least one source image".to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
        if !(1..=10).contains(&request.count) {
            return Err(ProviderError {
                message: "OpenRouter image edit count must be between 1 and 10".to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
        if let Some(capabilities) = self
            .provider
            .config
            .capability_for_model(&self.provider.config.model)
            && !capabilities.image_edit()
        {
            return Err(openrouter_unsupported_model_capability(
                &self.provider.config.model,
                "image_edit",
            ));
        }

        let body = build_openrouter_image_body(
            &self.provider.config.model,
            request.prompt.trim(),
            &request.images,
            request.size.as_deref(),
        )?;
        collect_openrouter_images(&self.provider, body, request.count).await
    }
}

#[derive(Clone)]
pub struct OpenRouterAudioTranscriber {
    provider: OpenRouterProvider,
}

impl OpenRouterAudioTranscriber {
    /// Builds one OpenRouter-backed audio transcriber without runtime observation hooks.
    pub fn new(config: OpenRouterProviderConfig) -> Result<Self, ProviderError> {
        Self::with_observer(config, Arc::new(NoopObserver))
    }

    /// Builds one OpenRouter-backed audio transcriber with runtime observation hooks enabled.
    pub fn with_observer(
        mut config: OpenRouterProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        config.model = resolve_openrouter_transcription_model(&config.model);
        Ok(Self {
            provider: OpenRouterProvider::with_observer(config, observer)?,
        })
    }

    pub async fn transcribe(
        &self,
        request: &AudioTranscriptionRequest,
    ) -> Result<AudioTranscriptionResponse, ProviderError> {
        validate_openrouter_transcription_request(request)?;
        let cancellation = current_cancellation_token();
        if cancellation
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return Err(openrouter_interrupted_error());
        }
        if let Some(capabilities) = self
            .provider
            .config
            .capability_for_model(&self.provider.config.model)
            && !capabilities.transcription()
        {
            return Err(openrouter_unsupported_model_capability(
                &self.provider.config.model,
                "audio_input",
            ));
        }
        let format = openrouter_audio_input_format(&request.file_name, &request.media_type)?;
        let model = self.provider.config.model.clone();
        let mut body = json!({
            "model": model,
            "input_audio": {
                "data": BASE64_STANDARD.encode(&request.bytes),
                "format": format.clone(),
            },
        });
        if let Some(prompt) = request
            .prompt
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            && let Some(object) = body.as_object_mut()
        {
            object.insert("prompt".to_string(), Value::String(prompt.to_string()));
        }
        if let Some(language) = request
            .language
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            && let Some(object) = body.as_object_mut()
        {
            object.insert("language".to_string(), Value::String(language.to_string()));
        }
        let payload = post_openrouter_audio_transcription_json(
            &self.provider,
            body,
            json!({
                "model": model,
                "file_name_sha256": digest_text(&request.file_name),
                "file_extension": request
                    .file_name
                    .rsplit_once('.')
                    .map(|(_, extension)| extension.to_ascii_lowercase()),
                "media_type": request.media_type,
                "byte_len": request.bytes.len(),
                "sha256": digest_bytes(&request.bytes),
                "format": format,
                "prompt_chars": request
                    .prompt
                    .as_deref()
                    .map(|value| value.chars().count())
                    .unwrap_or(0),
                "language": request.language,
                "diarization": request.diarization,
            }),
            cancellation,
        )
        .await?;
        let text = payload
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .or_else(|| openrouter_choice_text(&payload).ok())
            .ok_or_else(|| ProviderError {
                message: "OpenRouter transcription response did not contain text".to_string(),
                retryable: false,
                retry_after_ms: None,
            })?;
        let response_model = payload
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&self.provider.config.model)
            .to_string();
        Ok(AudioTranscriptionResponse {
            provider: "openrouter".to_string(),
            model: response_model,
            text,
            timestamps: None,
        })
    }
}

fn validate_openrouter_transcription_request(
    request: &AudioTranscriptionRequest,
) -> Result<(), ProviderError> {
    if request.bytes.is_empty() {
        return Err(ProviderError {
            message: "OpenRouter audio transcription requires audio bytes".to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if request.bytes.len() > MAX_OPENROUTER_TRANSCRIPTION_REQUEST_BYTES {
        return Err(ProviderError {
            message: format!(
                "OpenRouter audio transcription request exceeds the {} byte limit",
                MAX_OPENROUTER_TRANSCRIPTION_REQUEST_BYTES
            ),
            retryable: false,
            retry_after_ms: None,
        });
    }
    openrouter_audio_input_format(&request.file_name, &request.media_type)?;
    if request.diarization {
        return Err(ProviderError {
            message: "OpenRouter audio transcription does not support speaker diarization"
                .to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if let Some(prompt) = request.prompt.as_deref()
        && prompt.chars().count() > 2_000
    {
        return Err(ProviderError {
            message: "OpenRouter audio transcription prompt exceeds the 2000 character limit"
                .to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if let Some(language) = request.language.as_deref().map(str::trim)
        && !language.is_empty()
        && (language.chars().count() > 32
            || !language.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            }))
    {
        return Err(ProviderError {
            message:
                "OpenRouter audio transcription language must be a short ASCII language identifier"
                    .to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if !request.timestamp_granularities.is_empty() {
        return Err(ProviderError {
            message: "OpenRouter audio transcription does not support timestamp granularities"
                .to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpenRouterSpeechRequest {
    pub input: String,
    pub instructions: Option<String>,
    pub voice: Option<String>,
    pub response_format: Option<String>,
    pub speed: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpenRouterSpeechResponse {
    pub provider: String,
    pub model: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub transcript: Option<String>,
}

#[derive(Clone)]
pub struct OpenRouterSpeechSynthesizer {
    provider: OpenRouterProvider,
}

impl OpenRouterSpeechSynthesizer {
    /// Builds one OpenRouter-backed speech synthesizer without runtime observation hooks.
    pub fn new(config: OpenRouterProviderConfig) -> Result<Self, ProviderError> {
        Self::with_observer(config, Arc::new(NoopObserver))
    }

    /// Builds one OpenRouter-backed speech synthesizer with runtime observation hooks enabled.
    pub fn with_observer(
        mut config: OpenRouterProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        config.model = resolve_openrouter_tts_model(&config.model);
        Ok(Self {
            provider: OpenRouterProvider::with_observer(config, observer)?,
        })
    }

    pub async fn synthesize(
        &self,
        request: &OpenRouterSpeechRequest,
    ) -> Result<OpenRouterSpeechResponse, ProviderError> {
        if request.input.trim().is_empty() {
            return Err(ProviderError {
                message: "OpenRouter speech input must not be empty".to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
        let voice = request
            .voice
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_OPENROUTER_TTS_VOICE)
            .to_string();
        validate_openrouter_tts_voice(&voice)?;
        let response_format = request
            .response_format
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_OPENROUTER_TTS_FORMAT)
            .to_ascii_lowercase();
        if !openrouter_tts_response_format_supported(&response_format) {
            return Err(ProviderError {
                message: format!(
                    "unsupported OpenRouter speech response format `{response_format}`"
                ),
                retryable: false,
                retry_after_ms: None,
            });
        }
        if let Some(capabilities) = self
            .provider
            .config
            .capability_for_model(&self.provider.config.model)
            && !capabilities.audio_generation()
        {
            return Err(openrouter_unsupported_model_capability(
                &self.provider.config.model,
                "speech_output",
            ));
        }
        if let Some(speed) = request.speed {
            if !speed.is_finite() || speed <= 0.0 {
                return Err(ProviderError {
                    message: "OpenRouter speech speed must be a finite positive number".to_string(),
                    retryable: false,
                    retry_after_ms: None,
                });
            }
        }

        let mut force_refresh = false;
        let (response, response_target, response_grant_id) = loop {
            let auth_material = self.provider.auth_material(force_refresh).await?;
            let grant_id = auth_material.grant_id.clone();
            let endpoint = openrouter_tts_endpoint(
                auth_material
                    .base_url_override
                    .as_deref()
                    .unwrap_or(self.provider.config.base_url.as_str()),
            )?;
            let headers = self.provider.headers_from_material(&auth_material)?;
            self.provider
                .ensure_auth_material_active(&auth_material)
                .await?;
            let mut body = json!({
                "input": request.input,
                "model": self.provider.config.model,
                "voice": voice,
                "response_format": response_format,
                "speed": request.speed.unwrap_or(1.0),
            });
            if let Some(instructions) = request
                .instructions
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                && let Some(object) = body.as_object_mut()
            {
                object.insert(
                    "provider".to_string(),
                    json!({
                        "options": {
                            "openai": {
                                "instructions": instructions,
                            }
                        }
                    }),
                );
            }
            let request_summary = json!({
                "model": self.provider.config.model,
                "voice": body["voice"],
                "response_format": body["response_format"],
                "speed": request.speed.unwrap_or(1.0),
                "input_chars": request.input.chars().count(),
                "instructions_chars": request
                    .instructions
                    .as_deref()
                    .map(|value| value.chars().count())
                    .unwrap_or(0),
            });
            self.provider.record_media_provider_request(
                "openrouter-audio-speech-provider-request",
                &endpoint,
                &headers,
                &request_summary,
                grant_id.clone(),
            )?;
            let response = match self
                .provider
                .client
                .post(&endpoint)
                .timeout(OPENROUTER_AUDIO_REQUEST_TIMEOUT)
                .headers(headers)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    let mapped = map_transport_error(error);
                    self.provider.record_provider_failure(
                        self.provider.external_action_target(&endpoint),
                        &mapped.message,
                        grant_id.clone(),
                    )?;
                    return Err(mapped);
                }
            };
            if response.status() == StatusCode::UNAUTHORIZED
                && self.provider.config.request_auth_provider.is_some()
                && !force_refresh
            {
                self.provider.record_provider_failure(
                    self.provider.external_action_target(&endpoint),
                    "401-refresh",
                    grant_id.clone(),
                )?;
                force_refresh = true;
                continue;
            }
            break (
                response,
                self.provider.external_action_target(&endpoint),
                grant_id,
            );
        };

        if !response.status().is_success() {
            let error = map_http_error(response).await;
            self.provider.record_provider_failure(
                &response_target,
                &error.message,
                response_grant_id.clone(),
            )?;
            return Err(error);
        }

        let status = response.status();
        let response_headers = response.headers().clone();
        let media_type = response_headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                speech_media_type(
                    request
                        .response_format
                        .as_deref()
                        .unwrap_or(DEFAULT_OPENROUTER_TTS_FORMAT),
                )
                .to_string()
            });
        let bytes = match read_openrouter_audio_response_bytes(response).await {
            Ok(bytes) => bytes,
            Err(error) => {
                self.provider.record_provider_failure(
                    &response_target,
                    &error.message,
                    response_grant_id.clone(),
                )?;
                return Err(error);
            }
        };
        let response_summary = json!({
            "model": self.provider.config.model.clone(),
            "media_type": media_type.clone(),
            "byte_len": bytes.len(),
            "sha256": digest_bytes(&bytes),
            "transcript": Value::Null,
        });
        self.provider.record_media_provider_response(
            "openrouter-audio-speech-provider-response",
            &response_target,
            status.as_u16(),
            &response_headers,
            &response_summary,
            response_grant_id,
        )?;
        Ok(OpenRouterSpeechResponse {
            provider: "openrouter".to_string(),
            model: self.provider.config.model.clone(),
            media_type,
            bytes,
            transcript: None,
        })
    }
}

async fn post_openrouter_json(
    provider: &OpenRouterProvider,
    body: Value,
) -> Result<Value, ProviderError> {
    let mut force_refresh = false;
    let (response, response_target, response_grant_id) = loop {
        let auth_material = provider.auth_material(force_refresh).await?;
        let grant_id = auth_material.grant_id.clone();
        let endpoint = openrouter_chat_completions_endpoint(
            auth_material
                .base_url_override
                .as_deref()
                .unwrap_or(provider.config.base_url.as_str()),
        )?;
        let headers = provider.headers_from_material(&auth_material)?;
        provider.ensure_auth_material_active(&auth_material).await?;
        provider.record_media_provider_request(
            "openrouter-image-provider-request",
            &endpoint,
            &headers,
            &openrouter_image_request_summary(&body),
            grant_id.clone(),
        )?;
        let response = match provider
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
                provider.record_provider_failure(
                    provider.external_action_target(&endpoint),
                    &mapped.message,
                    grant_id.clone(),
                )?;
                return Err(mapped);
            }
        };
        if response.status() == StatusCode::UNAUTHORIZED
            && provider.config.request_auth_provider.is_some()
            && !force_refresh
        {
            provider.record_provider_failure(
                provider.external_action_target(&endpoint),
                "401-refresh",
                grant_id.clone(),
            )?;
            force_refresh = true;
            continue;
        }
        break (
            response,
            provider.external_action_target(&endpoint),
            grant_id,
        );
    };

    if !response.status().is_success() {
        let error = map_http_error(response).await;
        provider.record_provider_failure(
            &response_target,
            &error.message,
            response_grant_id.clone(),
        )?;
        return Err(error);
    }

    let status = response.status();
    let response_headers = response.headers().clone();
    let payload = match read_openrouter_image_response_json(response).await {
        Ok(payload) => payload,
        Err(error) => {
            provider.record_provider_failure(
                &response_target,
                &error.message,
                response_grant_id.clone(),
            )?;
            return Err(error);
        }
    };
    provider.record_media_provider_response(
        "openrouter-image-provider-response",
        &response_target,
        status.as_u16(),
        &response_headers,
        &openrouter_image_response_summary(&payload),
        response_grant_id,
    )?;
    Ok(payload)
}

async fn post_openrouter_audio_transcription_json(
    provider: &OpenRouterProvider,
    body: Value,
    request_summary: Value,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<Value, ProviderError> {
    let mut force_refresh = false;
    let (response, response_target, response_grant_id) = loop {
        let auth_material = provider.auth_material(force_refresh).await?;
        let grant_id = auth_material.grant_id.clone();
        let endpoint = openrouter_transcriptions_endpoint(
            auth_material
                .base_url_override
                .as_deref()
                .unwrap_or(provider.config.base_url.as_str()),
        )?;
        let headers = provider.headers_from_material(&auth_material)?;
        provider.ensure_auth_material_active(&auth_material).await?;
        provider.record_media_provider_request(
            "openrouter-audio-transcription-provider-request",
            &endpoint,
            &headers,
            &request_summary,
            grant_id.clone(),
        )?;
        let send = provider
            .client
            .post(&endpoint)
            .timeout(OPENROUTER_AUDIO_REQUEST_TIMEOUT)
            .headers(headers)
            .json(&body)
            .send();
        let response =
            match maybe_cancel_openrouter_provider_future(send, cancellation.clone()).await {
                Ok(response) => response,
                Err(error) => {
                    let mapped = error;
                    provider.record_provider_failure(
                        provider.external_action_target(&endpoint),
                        &mapped.message,
                        grant_id.clone(),
                    )?;
                    return Err(mapped);
                }
            };
        if response.status() == StatusCode::UNAUTHORIZED
            && provider.config.request_auth_provider.is_some()
            && !force_refresh
        {
            provider.record_provider_failure(
                provider.external_action_target(&endpoint),
                "401-refresh",
                grant_id.clone(),
            )?;
            force_refresh = true;
            continue;
        }
        break (
            response,
            provider.external_action_target(&endpoint),
            grant_id,
        );
    };

    if !response.status().is_success() {
        let error = map_http_error_with_body_limit(
            response,
            MAX_OPENROUTER_TRANSCRIPTION_RESPONSE_BYTES,
            "OpenRouter transcription error body",
            cancellation,
        )
        .await;
        provider.record_provider_failure(
            &response_target,
            &error.message,
            response_grant_id.clone(),
        )?;
        return Err(error);
    }

    let status = response.status();
    let response_headers = response.headers().clone();
    let payload = match read_openrouter_transcription_response_json(response, cancellation).await {
        Ok(payload) => payload,
        Err(error) => {
            provider.record_provider_failure(
                &response_target,
                &error.message,
                response_grant_id.clone(),
            )?;
            return Err(error);
        }
    };
    let text_chars = payload
        .get("text")
        .and_then(Value::as_str)
        .map(|text| text.chars().count())
        .or_else(|| {
            openrouter_choice_text(&payload)
                .ok()
                .map(|text| text.chars().count())
        })
        .unwrap_or(0);
    let response_summary = json!({
        "model": payload
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&provider.config.model),
        "text_chars": text_chars,
    });
    provider.record_media_provider_response(
        "openrouter-audio-transcription-provider-response",
        &response_target,
        status.as_u16(),
        &response_headers,
        &response_summary,
        response_grant_id,
    )?;
    Ok(payload)
}

async fn read_openrouter_transcription_response_json(
    response: reqwest::Response,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<Value, ProviderError> {
    read_openrouter_bounded_json_response(
        response,
        MAX_OPENROUTER_TRANSCRIPTION_RESPONSE_BYTES,
        "OpenRouter transcription response",
        cancellation,
    )
    .await
}

async fn read_openrouter_image_response_json(
    response: reqwest::Response,
) -> Result<Value, ProviderError> {
    read_openrouter_bounded_json_response(
        response,
        MAX_OPENROUTER_IMAGE_RESPONSE_BYTES,
        "OpenRouter image response",
        None,
    )
    .await
}

async fn read_openrouter_bounded_json_response(
    response: reqwest::Response,
    max_bytes: usize,
    label: &str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<Value, ProviderError> {
    if let Some(content_length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        && content_length > max_bytes
    {
        return Err(ProviderError {
            message: format!("{label} exceeds the {max_bytes} byte limit"),
            retryable: false,
            retry_after_ms: None,
        });
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let chunk = if let Some(cancellation) = cancellation.as_ref() {
            tokio::select! {
                chunk = stream.next() => chunk,
                _ = cancellation.cancelled() => return Err(openrouter_interrupted_error()),
            }
        } else {
            stream.next().await
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(map_transport_error)?;
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ProviderError {
                message: format!("{label} exceeds the {max_bytes} byte limit"),
                retryable: false,
                retry_after_ms: None,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice::<Value>(&bytes).map_err(|error| ProviderError {
        message: format!("failed to decode OpenRouter JSON response: {error}"),
        retryable: false,
        retry_after_ms: None,
    })
}

async fn map_http_error_with_body_limit(
    response: reqwest::Response,
    max_bytes: usize,
    label: &str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> ProviderError {
    let status = response.status();
    let retry_after_ms = parse_retry_after_ms(response.headers());
    let body_text =
        match read_openrouter_bounded_response_text(response, max_bytes, label, cancellation).await
        {
            Ok(body_text) => body_text,
            Err(error) => {
                return ProviderError {
                    message: error.message,
                    retryable: status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error(),
                    retry_after_ms,
                };
            }
        };
    openrouter_http_error_from_text(status, retry_after_ms, &body_text)
}

async fn read_openrouter_bounded_response_text(
    response: reqwest::Response,
    max_bytes: usize,
    label: &str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<String, ProviderError> {
    if let Some(content_length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        && content_length > max_bytes
    {
        return Err(ProviderError {
            message: format!("{label} exceeds the {max_bytes} byte limit"),
            retryable: false,
            retry_after_ms: None,
        });
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let chunk = if let Some(cancellation) = cancellation.as_ref() {
            tokio::select! {
                chunk = stream.next() => chunk,
                _ = cancellation.cancelled() => return Err(openrouter_interrupted_error()),
            }
        } else {
            stream.next().await
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(map_transport_error)?;
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ProviderError {
                message: format!("{label} exceeds the {max_bytes} byte limit"),
                retryable: false,
                retry_after_ms: None,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|error| ProviderError {
        message: format!("failed to decode {label}: {error}"),
        retryable: false,
        retry_after_ms: None,
    })
}

fn openrouter_image_request_summary(body: &Value) -> Value {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut prompt_chars = 0usize;
    let mut image_inputs = 0usize;
    for message in messages {
        match message.get("content") {
            Some(Value::String(text)) => {
                prompt_chars += text.chars().count();
            }
            Some(Value::Array(parts)) => {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        prompt_chars += text.chars().count();
                    }
                    if part.get("image_url").is_some() {
                        image_inputs += 1;
                    }
                }
            }
            _ => {}
        }
    }
    json!({
        "model": body.get("model").and_then(Value::as_str),
        "modalities": body.get("modalities"),
        "stream": body.get("stream").and_then(Value::as_bool),
        "image_config": body.get("image_config"),
        "prompt_chars": prompt_chars,
        "image_inputs": image_inputs,
    })
}

fn openrouter_image_response_summary(payload: &Value) -> Value {
    let mut image_count = 0usize;
    let mut text_chars = 0usize;
    if let Some(choices) = payload.get("choices").and_then(Value::as_array) {
        for choice in choices {
            let Some(message) = choice.get("message") else {
                continue;
            };
            if let Some(text) = openrouter_message_text(message) {
                text_chars += text.chars().count();
            }
            image_count += message
                .get("images")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
        }
    }
    json!({
        "model": payload.get("model").and_then(Value::as_str),
        "choice_count": payload
            .get("choices")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0),
        "image_count": image_count,
        "text_chars": text_chars,
    })
}

fn build_openrouter_image_body(
    model: &str,
    prompt: &str,
    images: &[OpenRouterImageEditInput],
    size: Option<&str>,
) -> Result<Value, ProviderError> {
    let image_config = openrouter_image_config(size)?;
    let mut content = vec![json!({
        "type": "text",
        "text": prompt,
    })];
    for image in images {
        content.push(json!({
            "type": "image_url",
            "image_url": {
                "url": data_url_for_image(image)?,
            }
        }));
    }
    let mut body = json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": if images.is_empty() {
                Value::String(prompt.to_string())
            } else {
                Value::Array(content)
            },
        }],
        "modalities": ["image", "text"],
        "stream": false,
    });
    if let Some(image_config) = image_config {
        body["image_config"] = image_config;
    }
    Ok(body)
}

async fn collect_openrouter_images(
    provider: &OpenRouterProvider,
    body: Value,
    requested_count: u32,
) -> Result<OpenRouterImageGenerationResponse, ProviderError> {
    let mut images = Vec::with_capacity(requested_count as usize);
    let mut text = None::<String>;
    let mut model = None::<String>;
    while images.len() < requested_count as usize {
        let payload = post_openrouter_json(provider, body.clone()).await?;
        let response = decode_openrouter_image_response(provider, &payload).await?;
        if text.is_none() {
            text = response.text;
        }
        if model.is_none() {
            model = Some(response.model);
        }
        let remaining = requested_count as usize - images.len();
        images.extend(response.images.into_iter().take(remaining));
    }
    Ok(OpenRouterImageGenerationResponse {
        model: model.unwrap_or_else(|| provider.config.model.clone()),
        images,
        text,
    })
}

async fn decode_openrouter_image_response(
    provider: &OpenRouterProvider,
    payload: &Value,
) -> Result<OpenRouterImageGenerationResponse, ProviderError> {
    let choices = payload
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError {
            message: "OpenRouter image response did not include any choices".to_string(),
            retryable: false,
            retry_after_ms: None,
        })?;

    let mut text = None::<String>;
    let mut images = Vec::new();
    for choice in choices {
        let Some(message) = choice.get("message") else {
            continue;
        };
        if text.is_none() {
            text = openrouter_message_text(message);
        }
        if let Some(generated_images) = message.get("images").and_then(Value::as_array) {
            for item in generated_images {
                images.push(decode_openrouter_generated_image(item).await?);
            }
        }
    }
    if images.is_empty() {
        return Err(ProviderError {
            message: "OpenRouter image response did not include any images".to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    Ok(OpenRouterImageGenerationResponse {
        model: payload
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&provider.config.model)
            .to_string(),
        images,
        text,
    })
}

async fn decode_openrouter_generated_image(
    item: &Value,
) -> Result<OpenRouterGeneratedImage, ProviderError> {
    let url = item
        .get("image_url")
        .or_else(|| item.get("imageUrl"))
        .and_then(|value| value.get("url"))
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError {
            message: "OpenRouter image item did not include image_url.url".to_string(),
            retryable: false,
            retry_after_ms: None,
        })?;
    let (media_type, bytes) = decode_image_payload(url).await?;
    Ok(OpenRouterGeneratedImage {
        media_type,
        bytes,
        revised_prompt: item
            .get("revised_prompt")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

async fn decode_image_payload(value: &str) -> Result<(String, Vec<u8>), ProviderError> {
    if let Some(encoded) = value.strip_prefix("data:") {
        let (meta, payload) = encoded.split_once(',').ok_or_else(|| ProviderError {
            message: "invalid OpenRouter image data URL".to_string(),
            retryable: false,
            retry_after_ms: None,
        })?;
        let media_type = meta
            .split(';')
            .next()
            .filter(|value| !value.is_empty())
            .unwrap_or("image/png");
        let bytes = BASE64_STANDARD
            .decode(payload)
            .map_err(|error| ProviderError {
                message: format!("failed to decode OpenRouter image payload: {error}"),
                retryable: false,
                retry_after_ms: None,
            })?;
        return Ok((media_type.to_string(), bytes));
    }

    if value.starts_with("http://") || value.starts_with("https://") {
        let (header_media_type, bytes) = download_openrouter_image_url(value).await?;
        let media_type = sniff_generated_image_media_type(&bytes)
            .map(str::to_string)
            .or(header_media_type)
            .unwrap_or_else(|| "image/png".to_string());
        return Ok((media_type, bytes));
    }

    let bytes = BASE64_STANDARD
        .decode(value)
        .map_err(|error| ProviderError {
            message: format!("failed to decode OpenRouter image payload: {error}"),
            retryable: false,
            retry_after_ms: None,
        })?;
    Ok((
        sniff_generated_image_media_type(&bytes)
            .unwrap_or("image/png")
            .to_string(),
        bytes,
    ))
}

async fn download_openrouter_image_url(
    value: &str,
) -> Result<(Option<String>, Vec<u8>), ProviderError> {
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| ProviderError {
            message: format!("failed to build OpenRouter image download client: {error}"),
            retryable: false,
            retry_after_ms: None,
        })?;
    let mut url = Url::parse(value).map_err(|error| ProviderError {
        message: format!("invalid OpenRouter image URL: {error}"),
        retryable: false,
        retry_after_ms: None,
    })?;

    for _ in 0..=MAX_OPENROUTER_IMAGE_REDIRECTS {
        validate_public_openrouter_image_url(&url).await?;
        let response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|error| ProviderError {
                message: format!("OpenRouter image download failed: {error}"),
                retryable: true,
                retry_after_ms: None,
            })?;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| ProviderError {
                    message: "OpenRouter image redirect missing Location header".to_string(),
                    retryable: false,
                    retry_after_ms: None,
                })?;
            url = url.join(location).map_err(|error| ProviderError {
                message: format!("invalid OpenRouter image redirect URL: {error}"),
                retryable: false,
                retry_after_ms: None,
            })?;
            continue;
        }

        let retry_after_ms = parse_retry_after_ms(response.headers());
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError {
                message: sanitize_upstream_error_message(
                    "OpenRouter",
                    "image download error",
                    Some(status),
                    None,
                    None,
                    Some(body.trim()).filter(|value| !value.is_empty()),
                ),
                retryable: status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error(),
                retry_after_ms,
            });
        }
        if let Some(content_length) = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            && content_length > MAX_OPENROUTER_IMAGE_DOWNLOAD_BYTES
        {
            return Err(ProviderError {
                message: format!(
                    "OpenRouter image download exceeds the {} byte limit",
                    MAX_OPENROUTER_IMAGE_DOWNLOAD_BYTES
                ),
                retryable: false,
                retry_after_ms,
            });
        }
        let header_media_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if let Some(media_type) = header_media_type.as_deref()
            && !media_type.to_ascii_lowercase().starts_with("image/")
        {
            return Err(ProviderError {
                message: format!(
                    "OpenRouter image download returned non-image content type {media_type}"
                ),
                retryable: false,
                retry_after_ms,
            });
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| ProviderError {
                message: format!("failed to read OpenRouter image download body: {error}"),
                retryable: status.is_server_error(),
                retry_after_ms,
            })?;
            if bytes.len().saturating_add(chunk.len()) > MAX_OPENROUTER_IMAGE_DOWNLOAD_BYTES {
                return Err(ProviderError {
                    message: format!(
                        "OpenRouter image download exceeds the {} byte limit",
                        MAX_OPENROUTER_IMAGE_DOWNLOAD_BYTES
                    ),
                    retryable: false,
                    retry_after_ms,
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok((header_media_type, bytes));
    }

    Err(ProviderError {
        message: "OpenRouter image download exceeded redirect limit".to_string(),
        retryable: false,
        retry_after_ms: None,
    })
}

async fn validate_public_openrouter_image_url(url: &Url) -> Result<(), ProviderError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(openrouter_provider_error(format!(
            "unsupported OpenRouter image URL scheme {}",
            url.scheme()
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(openrouter_provider_error(
            "OpenRouter image URL must not contain credentials",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| openrouter_provider_error("OpenRouter image URL is missing a host"))?;
    if matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "localhost."
    ) {
        return Err(openrouter_provider_error(
            "OpenRouter image URL points to localhost",
        ));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_openrouter_image_ip(ip) {
            return Err(openrouter_provider_error(format!(
                "OpenRouter image URL resolves to blocked address {ip}"
            )));
        }
        return Ok(());
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let addresses = tokio::net::lookup_host(format!("{host}:{port}"))
        .await
        .map_err(|error| ProviderError {
            message: format!("failed to resolve OpenRouter image URL host {host}: {error}"),
            retryable: true,
            retry_after_ms: None,
        })?
        .map(|address| address.ip())
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(openrouter_provider_error(format!(
            "OpenRouter image URL host {host} resolved no addresses"
        )));
    }
    if let Some(ip) = addresses
        .into_iter()
        .find(|ip| is_blocked_openrouter_image_ip(*ip))
    {
        return Err(openrouter_provider_error(format!(
            "OpenRouter image URL resolves to blocked address {ip}"
        )));
    }
    Ok(())
}

fn is_blocked_openrouter_image_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_blocked_openrouter_image_ipv4(ip),
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_multicast()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(is_blocked_openrouter_image_ipv4)
        }
    }
}

fn is_blocked_openrouter_image_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || ip.is_unspecified()
        || ip.octets()[0] == 0
}

fn openrouter_provider_error(message: impl Into<String>) -> ProviderError {
    ProviderError {
        message: message.into(),
        retryable: false,
        retry_after_ms: None,
    }
}

fn openrouter_choice_text(payload: &Value) -> Result<String, ProviderError> {
    payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| {
            choices
                .iter()
                .filter_map(|choice| choice.get("message"))
                .find_map(openrouter_message_text)
        })
        .ok_or_else(|| ProviderError {
            message: "OpenRouter response did not include assistant text content".to_string(),
            retryable: false,
            retry_after_ms: None,
        })
}

fn openrouter_message_text(message: &Value) -> Option<String> {
    match message.get("content") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(parts)) => {
            let text = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| part.as_str().map(str::to_string))
                })
                .collect::<Vec<_>>()
                .join("");
            if text.is_empty() { None } else { Some(text) }
        }
        _ => None,
    }
}

fn openrouter_prompt_uses_image_input(normalized: &NormalizedProviderPrompt) -> bool {
    normalized.conversation.iter().any(|item| match item {
        NormalizedConversationItem::UserMessage {
            content_parts,
            attachments,
            ..
        } => {
            content_parts.iter().any(|part| match part {
                InputContentPart::Attachment { attachment } => {
                    openrouter_attachment_uses_image_input(attachment)
                }
                InputContentPart::Text { .. } => false,
            }) || attachments
                .iter()
                .any(openrouter_attachment_uses_image_input)
        }
        _ => false,
    })
}

fn openrouter_attachment_uses_image_input(attachment: &kheish_types::AttachmentRef) -> bool {
    attachment
        .media_type
        .to_ascii_lowercase()
        .starts_with("image/")
        || attachment.preview_image_uri.is_some()
}

fn openrouter_unsupported_model_capability(model: &str, capability: &str) -> ProviderError {
    ProviderError {
        message: format!(
            "OpenRouter model `{model}` does not support required capability `{capability}`"
        ),
        retryable: false,
        retry_after_ms: None,
    }
}

fn openrouter_image_config(size: Option<&str>) -> Result<Option<Value>, ProviderError> {
    let Some(size) = size.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let (aspect_ratio, image_size) = match size.to_ascii_lowercase().as_str() {
        "1024x1024" | "square" => ("1:1", "1K"),
        "1536x1024" => ("3:2", "2K"),
        "1024x1536" => ("2:3", "2K"),
        "1792x1024" => ("16:9", "2K"),
        "1024x1792" => ("9:16", "2K"),
        other => {
            return Err(ProviderError {
                message: format!("unsupported OpenRouter image size {other}"),
                retryable: false,
                retry_after_ms: None,
            });
        }
    };
    Ok(Some(json!({
        "aspect_ratio": aspect_ratio,
        "image_size": image_size,
    })))
}

fn data_url_for_image(image: &OpenRouterImageEditInput) -> Result<String, ProviderError> {
    if image.bytes.is_empty() {
        return Err(ProviderError {
            message: "OpenRouter image edit inputs must include image bytes".to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    let media_type = normalized_image_media_type(image)?;
    Ok(format!(
        "data:{media_type};base64,{}",
        BASE64_STANDARD.encode(&image.bytes)
    ))
}

fn normalized_image_media_type(image: &OpenRouterImageEditInput) -> Result<String, ProviderError> {
    let media_type = image.media_type.trim();
    if !media_type.is_empty() {
        if media_type.to_ascii_lowercase().starts_with("image/") {
            return Ok(media_type.to_string());
        }
        return Err(ProviderError {
            message: format!(
                "unsupported OpenRouter image media type {}",
                image.media_type
            ),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if let Some(media_type) = image_media_type_from_extension(&image.file_name) {
        return Ok(media_type.to_string());
    }
    if let Some(media_type) = sniff_generated_image_media_type(&image.bytes) {
        return Ok(media_type.to_string());
    }
    Err(ProviderError {
        message: format!(
            "could not infer OpenRouter image media type for {}",
            image.file_name
        ),
        retryable: false,
        retry_after_ms: None,
    })
}

fn image_media_type_from_extension(file_name: &str) -> Option<&'static str> {
    let extension = Path::new(file_name)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

fn sniff_generated_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    const JPEG_SIGNATURE: &[u8] = b"\xff\xd8\xff";
    const GIF87A_SIGNATURE: &[u8] = b"GIF87a";
    const GIF89A_SIGNATURE: &[u8] = b"GIF89a";
    const WEBP_RIFF_SIGNATURE: &[u8] = b"RIFF";
    const WEBP_MARKER: &[u8] = b"WEBP";

    if bytes.starts_with(PNG_SIGNATURE) {
        return Some("image/png");
    }
    if bytes.starts_with(JPEG_SIGNATURE) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(GIF87A_SIGNATURE) || bytes.starts_with(GIF89A_SIGNATURE) {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(WEBP_RIFF_SIGNATURE) && &bytes[8..12] == WEBP_MARKER {
        return Some("image/webp");
    }
    None
}

fn openrouter_audio_input_format(
    file_name: &str,
    media_type: &str,
) -> Result<String, ProviderError> {
    let normalized = media_type
        .split_once(';')
        .map(|(value, _)| value)
        .unwrap_or(media_type)
        .trim()
        .to_ascii_lowercase();
    let format = match normalized.as_str() {
        "audio/wav" | "audio/x-wav" => "wav".to_string(),
        "audio/mpeg" | "audio/mp3" | "audio/x-mp3" | "audio/mpga" => "mp3".to_string(),
        "audio/aiff" | "audio/x-aiff" => "aiff".to_string(),
        "audio/aac" => "aac".to_string(),
        "audio/ogg" | "audio/opus" => "ogg".to_string(),
        "audio/flac" | "audio/x-flac" => "flac".to_string(),
        "audio/m4a" | "audio/mp4" | "audio/x-m4a" => "m4a".to_string(),
        "audio/webm" => "webm".to_string(),
        "audio/l16" => "pcm16".to_string(),
        "audio/l24" => "pcm24".to_string(),
        _ => {
            let extension = Path::new(file_name)
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            match extension.as_str() {
                "wav" | "mp3" | "aiff" | "aac" | "ogg" | "flac" | "m4a" | "mp4" | "webm" => {
                    extension
                }
                "mpga" => "mp3".to_string(),
                "pcm16" => "pcm16".to_string(),
                "pcm24" => "pcm24".to_string(),
                _ => {
                    return Err(ProviderError {
                        message: format!(
                            "OpenRouter audio transcription does not support media type {media_type}"
                        ),
                        retryable: false,
                        retry_after_ms: None,
                    });
                }
            }
        }
    };
    Ok(format)
}

async fn maybe_cancel_openrouter_provider_future<F, T>(
    future: F,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<T, ProviderError>
where
    F: std::future::Future<Output = Result<T, reqwest::Error>>,
{
    if let Some(cancellation) = cancellation {
        tokio::select! {
            result = future => result.map_err(map_transport_error),
            _ = cancellation.cancelled() => Err(openrouter_interrupted_error()),
        }
    } else {
        future.await.map_err(map_transport_error)
    }
}

fn openrouter_interrupted_error() -> ProviderError {
    ProviderError {
        message: interrupted_error().to_string(),
        retryable: false,
        retry_after_ms: None,
    }
}

fn openrouter_chat_completions_endpoint(base_url: &str) -> Result<String, ProviderError> {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        return Ok(trimmed.to_string());
    }
    if let Some(prefix) = trimmed.strip_suffix("/responses") {
        return Ok(format!("{prefix}/chat/completions"));
    }
    if trimmed.ends_with("/v1") {
        return Ok(format!("{trimmed}/chat/completions"));
    }
    Err(ProviderError {
        message: format!("unsupported OpenRouter base URL for chat completions: {base_url}"),
        retryable: false,
        retry_after_ms: None,
    })
}

fn openrouter_tts_endpoint(base_url: &str) -> Result<String, ProviderError> {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/audio/speech") || trimmed.ends_with("/tts") {
        return Ok(trimmed.to_string());
    }
    if let Some(prefix) = trimmed.strip_suffix("/chat/completions") {
        return Ok(format!("{prefix}/audio/speech"));
    }
    if let Some(prefix) = trimmed.strip_suffix("/responses") {
        return Ok(format!("{prefix}/audio/speech"));
    }
    if trimmed.ends_with("/v1") {
        return Ok(format!("{trimmed}/audio/speech"));
    }
    Err(ProviderError {
        message: format!("unsupported OpenRouter base URL for speech synthesis: {base_url}"),
        retryable: false,
        retry_after_ms: None,
    })
}

fn openrouter_transcriptions_endpoint(base_url: &str) -> Result<String, ProviderError> {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/audio/transcriptions") {
        return Ok(trimmed.to_string());
    }
    if let Some(prefix) = trimmed.strip_suffix("/chat/completions") {
        return Ok(format!("{prefix}/audio/transcriptions"));
    }
    if let Some(prefix) = trimmed.strip_suffix("/responses") {
        return Ok(format!("{prefix}/audio/transcriptions"));
    }
    if let Some(prefix) = trimmed.strip_suffix("/audio/speech") {
        return Ok(format!("{prefix}/audio/transcriptions"));
    }
    if let Some(prefix) = trimmed.strip_suffix("/tts") {
        return Ok(format!("{prefix}/audio/transcriptions"));
    }
    if trimmed.ends_with("/v1") {
        return Ok(format!("{trimmed}/audio/transcriptions"));
    }
    Err(ProviderError {
        message: format!("unsupported OpenRouter base URL for audio transcription: {base_url}"),
        retryable: false,
        retry_after_ms: None,
    })
}

fn openrouter_models_endpoint(base_url: &str) -> Result<String, ProviderError> {
    let trimmed = base_url.trim_end_matches('/');
    let prefix = if let Some(prefix) = trimmed.strip_suffix("/chat/completions") {
        prefix
    } else if let Some(prefix) = trimmed.strip_suffix("/responses") {
        prefix
    } else if let Some(prefix) = trimmed.strip_suffix("/audio/speech") {
        prefix
    } else if let Some(prefix) = trimmed.strip_suffix("/audio/transcriptions") {
        prefix
    } else if let Some(prefix) = trimmed.strip_suffix("/tts") {
        prefix
    } else if trimmed.ends_with("/v1") {
        trimmed
    } else {
        return Err(ProviderError {
            message: format!("unsupported OpenRouter base URL for model discovery: {base_url}"),
            retryable: false,
            retry_after_ms: None,
        });
    };
    Ok(format!("{prefix}/models?output_modalities=all"))
}

fn speech_media_type(response_format: &str) -> &'static str {
    match response_format.trim().to_ascii_lowercase().as_str() {
        "pcm" => "audio/l16",
        "wav" => "audio/wav",
        "opus" => "audio/opus",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        _ => "audio/mpeg",
    }
}

fn openrouter_tts_response_format_supported(format: &str) -> bool {
    matches!(format.trim().to_ascii_lowercase().as_str(), "mp3" | "pcm")
}

fn validate_openrouter_tts_voice(voice: &str) -> Result<(), ProviderError> {
    let normalized = voice.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(ProviderError {
            message: "OpenRouter speech voice must not be empty".to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if openrouter_tts_voice_supported(&normalized) {
        return Ok(());
    }
    Err(ProviderError {
        message: format!("unsupported OpenRouter speech voice `{voice}`"),
        retryable: false,
        retry_after_ms: None,
    })
}

fn openrouter_tts_voice_supported(voice: &str) -> bool {
    matches!(
        voice,
        "alloy"
            | "ash"
            | "ballad"
            | "cedar"
            | "coral"
            | "echo"
            | "fable"
            | "marin"
            | "nova"
            | "onyx"
            | "sage"
            | "shimmer"
            | "verse"
    ) || (voice.starts_with("voice_")
        && voice.len() <= 96
        && voice
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-')))
}

async fn read_openrouter_audio_response_bytes(
    response: reqwest::Response,
) -> Result<Vec<u8>, ProviderError> {
    if let Some(content_length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        && content_length > MAX_OPENROUTER_AUDIO_RESPONSE_BYTES
    {
        return Err(ProviderError {
            message: format!(
                "OpenRouter speech response exceeds the {} byte limit",
                MAX_OPENROUTER_AUDIO_RESPONSE_BYTES
            ),
            retryable: false,
            retry_after_ms: None,
        });
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(map_transport_error)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_OPENROUTER_AUDIO_RESPONSE_BYTES {
            return Err(ProviderError {
                message: format!(
                    "OpenRouter speech response exceeds the {} byte limit",
                    MAX_OPENROUTER_AUDIO_RESPONSE_BYTES
                ),
                retryable: false,
                retry_after_ms: None,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub fn resolve_openrouter_model(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return DEFAULT_OPENROUTER_MODEL.to_string();
    }
    trimmed.to_string()
}

pub fn resolve_openrouter_image_model(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(DEFAULT_OPENROUTER_MODEL) {
        return DEFAULT_OPENROUTER_IMAGE_MODEL.to_string();
    }
    trimmed.to_string()
}

pub fn resolve_openrouter_transcription_model(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case(DEFAULT_OPENROUTER_MODEL)
        || trimmed.eq_ignore_ascii_case("gpt-4o-transcribe")
        || trimmed.eq_ignore_ascii_case("gpt-4o-mini-transcribe")
        || trimmed.eq_ignore_ascii_case("whisper-1")
    {
        return DEFAULT_OPENROUTER_TRANSCRIPTION_MODEL.to_string();
    }
    trimmed.to_string()
}

pub fn resolve_openrouter_tts_model(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case(DEFAULT_OPENROUTER_MODEL)
        || trimmed.eq_ignore_ascii_case("tts-1")
        || trimmed.eq_ignore_ascii_case("tts-1-hd")
        || trimmed.eq_ignore_ascii_case("openai/tts-1")
        || trimmed.eq_ignore_ascii_case("openai/tts-1-hd")
        || trimmed.eq_ignore_ascii_case("gpt-4o-mini-tts")
        || trimmed.eq_ignore_ascii_case("openai/gpt-4o-mini-tts")
        || trimmed.eq_ignore_ascii_case("elevenlabs/eleven-turbo-v2")
    {
        return DEFAULT_OPENROUTER_TTS_MODEL.to_string();
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use anyhow::Result;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;
    use crate::model::ModelGenerationConfig;
    use crate::{
        ExecutionScope, InMemoryObserver, NoopObserver, TraceEvent, TraceEventKind, scope_execution,
    };
    use kheish_types::{
        AttachmentRef, ProviderInputItem, ProviderPrompt, Role, StructuredFieldSchema,
        StructuredValueKind, ToolChoice, ToolDefinition,
    };

    #[derive(Clone, Debug)]
    struct MockResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    #[derive(Clone, Debug)]
    struct CapturedRequest {
        method: String,
        path: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

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

    #[test]
    fn resolves_openrouter_media_endpoints_from_common_base_urls() {
        assert_eq!(
            openrouter_chat_completions_endpoint("https://openrouter.ai/api/v1")
                .expect("chat endpoint"),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            openrouter_chat_completions_endpoint("https://openrouter.ai/api/v1/responses")
                .expect("chat endpoint"),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            openrouter_tts_endpoint("https://openrouter.ai/api/v1/responses")
                .expect("tts endpoint"),
            "https://openrouter.ai/api/v1/audio/speech"
        );
        assert_eq!(
            openrouter_transcriptions_endpoint("https://openrouter.ai/api/v1/responses")
                .expect("transcription endpoint"),
            "https://openrouter.ai/api/v1/audio/transcriptions"
        );
    }

    #[test]
    fn openrouter_error_event_payload_does_not_echo_secret_messages() {
        let leaked_secret = "openrouter-secret-from-upstream";
        let event = JsonSseEvent {
            event_type: "error".to_string(),
            payload: json!({
                "error": {
                    "message": format!("bad authorization token {leaked_secret}"),
                    "type": "invalid_request_error",
                    "code": "invalid_api_key"
                }
            }),
        };

        let payload = openrouter_error_event_payload(&event);
        let message = payload["message"].as_str().expect("sanitized message");
        assert!(!message.contains(leaked_secret));
        assert_eq!(
            message,
            "OpenRouter stream error: type=invalid_request_error, code=invalid_api_key"
        );
    }

    #[tokio::test]
    async fn openrouter_image_url_download_blocks_private_and_credentialed_urls() {
        for url in [
            "http://127.0.0.1/image.png",
            "http://localhost/image.png",
            "http://10.0.0.1/image.png",
            "http://169.254.169.254/latest/meta-data",
            "http://user:pass@example.com/image.png",
        ] {
            let error = decode_image_payload(url)
                .await
                .expect_err("unsafe OpenRouter image URL should be rejected");
            let message = error.to_string();
            assert!(
                message.contains("blocked")
                    || message.contains("localhost")
                    || message.contains("credentials")
                    || message.contains("unsupported"),
                "unexpected error for {url}: {message}"
            );
        }
    }

    #[test]
    fn openrouter_strict_schema_makes_nested_optional_tool_fields_nullable() {
        let provider =
            OpenRouterProvider::new(OpenRouterProviderConfig::new("openai/gpt-test", "test-key"))
                .expect("provider should build");
        let body = provider
            .build_request_body(&ModelRuntimeRequest {
                attempt: 1,
                kind: kheish_core::ModelRequestKind::MainLoop,
                session_id: "session-openrouter-nested-strict".to_string(),
                thread_id: None,
                turn: 1,
                prompt: ProviderPrompt::default(),
                available_tools: vec![ToolDefinition {
                    name: "ask_user_question".to_string(),
                    description: "Ask structured questions.".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "questions": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": {"type": "string"},
                                        "header": {"type": "string"},
                                        "question": {"type": "string"},
                                        "options": {
                                            "type": "array",
                                            "items": {
                                                "type": "object",
                                                "properties": {
                                                    "id": {"type": "string"},
                                                    "label": {"type": "string"},
                                                    "description": {"type": "string"},
                                                    "preview": {"type": "string"}
                                                },
                                                "required": ["label"],
                                                "additionalProperties": false
                                            }
                                        },
                                        "multi_select": {"type": "boolean"}
                                    },
                                    "required": ["options", "question"],
                                    "additionalProperties": false
                                }
                            }
                        },
                        "required": ["questions"],
                        "additionalProperties": false
                    }),
                    allows_parallel: false,
                }],
                generation: ModelGenerationConfig::default(),
            })
            .expect("request body should build");

        let function = &body["tools"][0]["function"];
        assert_eq!(function["strict"], json!(true));
        let question = &function["parameters"]["properties"]["questions"]["items"];
        assert_eq!(
            question["required"],
            json!(["header", "id", "multi_select", "options", "question"])
        );
        assert_eq!(
            question["properties"]["id"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            question["properties"]["multi_select"]["type"],
            json!(["boolean", "null"])
        );
        let option = &question["properties"]["options"]["items"];
        assert_eq!(
            option["required"],
            json!(["description", "id", "label", "preview"])
        );
        assert_eq!(
            option["properties"]["preview"]["type"],
            json!(["string", "null"])
        );
    }

    #[test]
    fn openrouter_model_discovery_parser_maps_modalities_and_parameters() {
        let capabilities = parse_openrouter_model_capabilities(&json!({
            "data": [
                {
                    "id": "vendor/text-tools",
                    "supported_parameters": ["tools", "response_format"],
                    "architecture": {
                        "input_modalities": ["text"],
                        "output_modalities": ["text"]
                    }
                },
                {
                    "id": "vendor/image-audio",
                    "supported_parameters": [],
                    "architecture": {
                        "input_modalities": ["text", "image", "audio"],
                        "output_modalities": ["text", "image", "audio"]
                    }
                },
                {
                    "id": "vendor/tts",
                    "supported_parameters": [],
                    "architecture": {
                        "input_modalities": ["text"],
                        "output_modalities": ["speech"]
                    }
                },
                {
                    "id": "vendor/stt",
                    "supported_parameters": [],
                    "architecture": {
                        "input_modalities": ["audio"],
                        "output_modalities": ["transcription"]
                    }
                }
            ]
        }));

        let text = capabilities.get("vendor/text-tools").expect("text model");
        assert!(text.tools);
        assert!(text.structured_output);
        assert!(!text.image_input);
        assert!(!text.image_output);

        let media = capabilities.get("vendor/image-audio").expect("media model");
        assert!(media.image_edit());
        assert!(media.audio_generation());
        assert!(media.transcription());
        assert!(!media.tools);

        let tts = capabilities.get("vendor/tts").expect("tts model");
        assert!(tts.audio_generation());
        assert!(!tts.transcription());

        let stt = capabilities.get("vendor/stt").expect("stt model");
        assert!(stt.transcription());
        assert!(!stt.audio_generation());
    }

    #[test]
    fn openrouter_structured_output_schema_makes_optional_fields_nullable() {
        let mut capabilities = BTreeMap::new();
        capabilities.insert(
            "vendor/json".to_string(),
            OpenRouterModelCapabilities {
                structured_output: true,
                text_input: true,
                text_output: true,
                ..OpenRouterModelCapabilities::default()
            },
        );
        let provider = OpenRouterProvider::new(
            OpenRouterProviderConfig::new("vendor/json", "test-key")
                .with_model_capabilities(capabilities),
        )
        .expect("provider should build");
        let body = provider
            .build_request_body(&ModelRuntimeRequest {
                attempt: 1,
                kind: kheish_core::ModelRequestKind::MainLoop,
                session_id: "session-openrouter-structured-nullable".to_string(),
                thread_id: None,
                turn: 1,
                prompt: ProviderPrompt::default(),
                available_tools: Vec::new(),
                generation: ModelGenerationConfig {
                    response_format: ResponseFormat::StructuredJson {
                        schema: StructuredFieldSchema {
                            kind: StructuredValueKind::Object,
                            fields: BTreeMap::from([(
                                "answer".to_string(),
                                StructuredFieldSchema::new(StructuredValueKind::String),
                            )]),
                            optional_fields: BTreeMap::from([(
                                "confidence".to_string(),
                                StructuredFieldSchema::new(StructuredValueKind::Number),
                            )]),
                            items: None,
                        },
                    },
                    ..ModelGenerationConfig::default()
                },
            })
            .expect("request body should build");

        let schema = &body["response_format"]["json_schema"]["schema"];
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["required"], json!(["answer", "confidence"]));
        assert_eq!(
            schema["properties"]["confidence"]["type"],
            json!(["number", "null"])
        );
        assert!(body.get("provider").is_none());
    }

    #[test]
    fn openrouter_rejects_unsupported_tools_before_provider_call() {
        let mut capabilities = BTreeMap::new();
        capabilities.insert(
            "vendor/no-tools".to_string(),
            OpenRouterModelCapabilities {
                text_input: true,
                text_output: true,
                ..OpenRouterModelCapabilities::default()
            },
        );
        let provider = OpenRouterProvider::new(
            OpenRouterProviderConfig::new("vendor/no-tools", "test-key")
                .with_model_capabilities(capabilities),
        )
        .expect("provider should build");
        let error = provider
            .build_request_body(&ModelRuntimeRequest {
                attempt: 1,
                kind: kheish_core::ModelRequestKind::MainLoop,
                session_id: "session-openrouter-no-tools".to_string(),
                thread_id: None,
                turn: 1,
                prompt: ProviderPrompt::default(),
                available_tools: vec![ToolDefinition {
                    name: "read_file".to_string(),
                    description: "Read a file".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {},
                        "required": [],
                        "additionalProperties": false
                    }),
                    allows_parallel: false,
                }],
                generation: ModelGenerationConfig::default(),
            })
            .expect_err("unsupported tools should fail before HTTP");
        assert!(error.message.contains("required capability `tools`"));
    }

    #[test]
    fn openrouter_allows_no_tool_choice_on_models_without_tool_capability() {
        let mut capabilities = BTreeMap::new();
        capabilities.insert(
            "vendor/no-tools".to_string(),
            OpenRouterModelCapabilities {
                text_input: true,
                text_output: true,
                ..OpenRouterModelCapabilities::default()
            },
        );
        let provider = OpenRouterProvider::new(
            OpenRouterProviderConfig::new("vendor/no-tools", "test-key")
                .with_model_capabilities(capabilities),
        )
        .expect("provider should build");
        let mut generation = ModelGenerationConfig::default();
        generation.tool_choice = ToolChoice::None;
        let body = provider
            .build_request_body(&ModelRuntimeRequest {
                attempt: 1,
                kind: kheish_core::ModelRequestKind::MainLoop,
                session_id: "session-openrouter-no-tools-none".to_string(),
                thread_id: None,
                turn: 1,
                prompt: ProviderPrompt::default(),
                available_tools: vec![ToolDefinition {
                    name: "read_file".to_string(),
                    description: "Read a file".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {},
                        "required": [],
                        "additionalProperties": false
                    }),
                    allows_parallel: false,
                }],
                generation,
            })
            .expect("disabled tools should not require provider tool support");
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn openrouter_rejects_unsupported_vision_before_provider_call() {
        let mut capabilities = BTreeMap::new();
        capabilities.insert(
            "vendor/text-only".to_string(),
            OpenRouterModelCapabilities {
                text_input: true,
                text_output: true,
                ..OpenRouterModelCapabilities::default()
            },
        );
        let provider = OpenRouterProvider::new(
            OpenRouterProviderConfig::new("vendor/text-only", "test-key")
                .with_model_capabilities(capabilities),
        )
        .expect("provider should build");
        let error = provider
            .build_request_body(&ModelRuntimeRequest {
                attempt: 1,
                kind: kheish_core::ModelRequestKind::MainLoop,
                session_id: "session-openrouter-no-vision".to_string(),
                thread_id: None,
                turn: 1,
                prompt: ProviderPrompt {
                    instructions: Vec::new(),
                    force_synthetic_user_prefix: false,
                    items: vec![ProviderInputItem::Message {
                        id: "msg-1".to_string(),
                        role: Role::User,
                        content: "Inspect this image".to_string(),
                        content_parts: Vec::new(),
                        attachments: vec![AttachmentRef {
                            id: "asset-1".to_string(),
                            media_type: "image/png".to_string(),
                            uri: "asset://asset-1/raw".to_string(),
                            file_name: Some("image.png".to_string()),
                            sha256: None,
                            byte_length: None,
                            text_uri: None,
                            text_sha256: None,
                            text_byte_length: None,
                            preview_image_uri: None,
                            preview_image_media_type: None,
                            preview_image_sha256: None,
                            preview_image_byte_length: None,
                        }],
                        provider_response_id: None,
                        provider_context: None,
                    }],
                },
                available_tools: Vec::new(),
                generation: ModelGenerationConfig::default(),
            })
            .expect_err("unsupported vision should fail before HTTP");
        assert!(error.message.contains("required capability `vision`"));
    }

    #[tokio::test]
    async fn openrouter_image_generator_repeats_requests_for_multi_image_count() -> Result<()> {
        let png_bytes = b"\x89PNG\r\n\x1a\nopenrouter".to_vec();
        let image_payload = format!(
            "data:image/png;base64,{}",
            BASE64_STANDARD.encode(&png_bytes)
        );
        let response_body = json!({
            "model": DEFAULT_OPENROUTER_IMAGE_MODEL,
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "Generated image",
                    "images": [{
                        "image_url": { "url": image_payload }
                    }]
                }
            }]
        });
        let (base_url, captured) = spawn_mock_server(vec![
            json_response(200, response_body.clone()),
            json_response(200, response_body),
        ])
        .await?;
        let mut config = OpenRouterProviderConfig::new("", "test-key");
        config.base_url = format!("{base_url}/v1");

        let generator = OpenRouterImageGenerator::new(config, Arc::new(NoopObserver))?;
        let response = generator
            .generate(OpenRouterImageGenerationRequest {
                prompt: "Render a skyline".to_string(),
                count: 2,
                size: Some("1024x1024".to_string()),
            })
            .await?;

        assert_eq!(response.model, DEFAULT_OPENROUTER_IMAGE_MODEL);
        assert_eq!(response.images.len(), 2);
        assert!(response.images.iter().all(|image| image.bytes == png_bytes));

        let captured = captured.lock();
        assert_eq!(captured.len(), 2);
        for request in captured.iter() {
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/v1/chat/completions");
            assert_eq!(
                request.headers.get("authorization"),
                Some(&"Bearer test-key".to_string())
            );
            let body: Value = serde_json::from_slice(&request.body)?;
            assert_eq!(body["model"], DEFAULT_OPENROUTER_IMAGE_MODEL);
            assert_eq!(body["modalities"], json!(["image", "text"]));
            assert_eq!(body["image_config"]["aspect_ratio"], "1:1");
            assert_eq!(body["image_config"]["image_size"], "1K");
        }
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_image_generator_accepts_large_image_json_response() -> Result<()> {
        let png_bytes = b"\x89PNG\r\n\x1a\nopenrouter-large-json".to_vec();
        let image_payload = format!(
            "data:image/png;base64,{}",
            BASE64_STANDARD.encode(&png_bytes)
        );
        let response_body = json!({
            "model": DEFAULT_OPENROUTER_IMAGE_MODEL,
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "Generated image",
                    "images": [{
                        "image_url": { "url": image_payload }
                    }]
                }
            }],
            "_padding": "x".repeat(MAX_OPENROUTER_TRANSCRIPTION_RESPONSE_BYTES + 1),
        });
        let serialized = serde_json::to_vec(&response_body)?;
        assert!(serialized.len() > MAX_OPENROUTER_TRANSCRIPTION_RESPONSE_BYTES);
        assert!(serialized.len() < MAX_OPENROUTER_IMAGE_RESPONSE_BYTES);
        let (base_url, _) = spawn_mock_server(vec![MockResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: serialized,
        }])
        .await?;
        let mut config = OpenRouterProviderConfig::new("", "test-key");
        config.base_url = format!("{base_url}/v1");

        let generator = OpenRouterImageGenerator::new(config, Arc::new(NoopObserver))?;
        let response = generator
            .generate(OpenRouterImageGenerationRequest {
                prompt: "Render a small image".to_string(),
                count: 1,
                size: None,
            })
            .await?;

        assert_eq!(response.images.len(), 1);
        assert_eq!(response.images[0].bytes, png_bytes);
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_image_generator_rejects_oversized_response_with_image_label() -> Result<()>
    {
        let (base_url, _) =
            spawn_mock_server_with_declared_content_length(MAX_OPENROUTER_IMAGE_RESPONSE_BYTES + 1)
                .await?;
        let mut config = OpenRouterProviderConfig::new("", "test-key");
        config.base_url = format!("{base_url}/v1");

        let generator = OpenRouterImageGenerator::new(config, Arc::new(NoopObserver))?;
        let error = generator
            .generate(OpenRouterImageGenerationRequest {
                prompt: "Render a small image".to_string(),
                count: 1,
                size: None,
            })
            .await
            .expect_err("oversized image response should fail");

        assert!(
            error.message.contains("OpenRouter image response exceeds"),
            "unexpected error: {}",
            error.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_image_editor_sends_image_content_parts() -> Result<()> {
        let png_bytes = b"\x89PNG\r\n\x1a\nedit".to_vec();
        let response_body = json!({
            "model": DEFAULT_OPENROUTER_IMAGE_MODEL,
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "images": [{
                        "image_url": {
                            "url": format!("data:image/png;base64,{}", BASE64_STANDARD.encode(&png_bytes))
                        }
                    }]
                }
            }]
        });
        let (base_url, captured) =
            spawn_mock_server(vec![json_response(200, response_body)]).await?;
        let mut config = OpenRouterProviderConfig::new("", "test-key");
        config.base_url = format!("{base_url}/v1/chat/completions");

        let editor = OpenRouterImageEditor::new(config, Arc::new(NoopObserver))?;
        let response = editor
            .edit(OpenRouterImageEditRequest {
                prompt: "Make it brighter".to_string(),
                images: vec![OpenRouterImageEditInput {
                    file_name: "source.png".to_string(),
                    media_type: String::new(),
                    bytes: png_bytes.clone(),
                }],
                count: 1,
                size: None,
            })
            .await?;

        assert_eq!(response.images.len(), 1);
        assert_eq!(response.images[0].bytes, png_bytes);

        let captured = captured.lock();
        let body: Value = serde_json::from_slice(&captured[0].body)?;
        assert_eq!(
            body["messages"][0]["content"][0]["text"],
            "Make it brighter"
        );
        let image_url = body["messages"][0]["content"][1]["image_url"]["url"]
            .as_str()
            .expect("image data url");
        assert!(image_url.starts_with("data:image/png;base64,"));
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_audio_transcriber_posts_input_audio_to_transcriptions_endpoint()
    -> Result<()> {
        let response_body = json!({
            "text": "Bonjour le monde"
        });
        let (base_url, captured) =
            spawn_mock_server(vec![json_response(200, response_body)]).await?;
        let mut config = OpenRouterProviderConfig::new("", "test-key");
        config.base_url = format!("{base_url}/v1/responses");

        let transcriber = OpenRouterAudioTranscriber::new(config)?;
        let response = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sample.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: b"RIFFdata".to_vec(),
                prompt: Some("Preserve punctuation".to_string()),
                language: Some("fr".to_string()),
                timestamp_granularities: Vec::new(),
                diarization: false,
            })
            .await?;

        assert_eq!(response.provider, "openrouter");
        assert_eq!(response.model, DEFAULT_OPENROUTER_TRANSCRIPTION_MODEL);
        assert_eq!(response.text, "Bonjour le monde");

        let captured = captured.lock();
        assert_eq!(captured[0].path, "/v1/audio/transcriptions");
        let body: Value = serde_json::from_slice(&captured[0].body)?;
        assert_eq!(body["model"], DEFAULT_OPENROUTER_TRANSCRIPTION_MODEL);
        assert_eq!(body["input_audio"]["format"], "wav");
        assert_eq!(body["prompt"], "Preserve punctuation");
        assert_eq!(body["language"], "fr");
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_audio_transcriber_rejects_oversized_audio_before_network() -> Result<()> {
        let config = OpenRouterProviderConfig::new("", "test-key");
        let transcriber = OpenRouterAudioTranscriber::new(config)?;

        let error = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "huge.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: vec![0u8; MAX_OPENROUTER_TRANSCRIPTION_REQUEST_BYTES + 1],
                prompt: None,
                language: None,
                timestamp_granularities: Vec::new(),
                diarization: false,
            })
            .await
            .expect_err("oversized request should fail before network");

        assert!(!error.retryable);
        assert!(
            error.message.contains("request exceeds"),
            "unexpected error: {}",
            error.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_audio_transcriber_rejects_unknown_format_before_network() -> Result<()> {
        let config = OpenRouterProviderConfig::new("", "test-key");
        let transcriber = OpenRouterAudioTranscriber::new(config)?;

        let error = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sample.bin".to_string(),
                media_type: "application/octet-stream".to_string(),
                bytes: b"audio".to_vec(),
                prompt: None,
                language: None,
                timestamp_granularities: Vec::new(),
                diarization: false,
            })
            .await
            .expect_err("unsupported request should fail before network");

        assert!(!error.retryable);
        assert!(
            error
                .message
                .contains("does not support media type application/octet-stream"),
            "unexpected error: {}",
            error.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_audio_transcriber_honors_cancelled_execution_scope() -> Result<()> {
        let config = OpenRouterProviderConfig::new("", "test-key");
        let transcriber = OpenRouterAudioTranscriber::new(config)?;
        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();

        let error = scope_execution(ExecutionScope::default(), cancellation, async {
            transcriber
                .transcribe(&AudioTranscriptionRequest {
                    file_name: "sample.wav".to_string(),
                    media_type: "audio/wav".to_string(),
                    bytes: b"RIFFdata".to_vec(),
                    prompt: None,
                    language: None,
                    timestamp_granularities: Vec::new(),
                    diarization: false,
                })
                .await
        })
        .await
        .expect_err("cancelled transcription should stop before network");

        assert!(!error.retryable);
        assert_eq!(error.message, interrupted_error().to_string());
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_audio_transcriber_treats_missing_text_as_contract_error() -> Result<()> {
        let (base_url, _) = spawn_mock_server(vec![json_response(200, json!({}))]).await?;
        let mut config = OpenRouterProviderConfig::new("", "test-key");
        config.base_url = format!("{base_url}/v1/responses");
        let transcriber = OpenRouterAudioTranscriber::new(config)?;

        let error = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sample.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: b"RIFFdata".to_vec(),
                prompt: None,
                language: None,
                timestamp_granularities: Vec::new(),
                diarization: false,
            })
            .await
            .expect_err("missing text should fail");

        assert!(!error.retryable);
        assert!(
            error.message.contains("did not contain text"),
            "unexpected error: {}",
            error.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_audio_transcriber_records_external_action_traces() -> Result<()> {
        let response_body = json!({
            "text": "Bonjour le monde"
        });
        let (base_url, _) = spawn_mock_server(vec![json_response(200, response_body)]).await?;
        let mut config = OpenRouterProviderConfig::new("", "test-key");
        config.base_url = format!("{base_url}/v1/responses");
        let observer = InMemoryObserver::shared();

        let transcriber = OpenRouterAudioTranscriber::with_observer(config, observer.clone())?;
        let response = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sample.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: b"RIFFdata".to_vec(),
                prompt: Some("Preserve punctuation".to_string()),
                language: Some("fr".to_string()),
                timestamp_granularities: Vec::new(),
                diarization: false,
            })
            .await?;

        assert_eq!(response.text, "Bonjour le monde");
        let traces = observer.traces();
        assert!(has_provider_external_action(
            &traces,
            "request",
            "openrouter:http://"
        ));
        assert!(has_provider_external_action(
            &traces,
            "response",
            "openrouter:http://"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_audio_transcriber_redacted_debug_does_not_emit_input_audio_data()
    -> Result<()> {
        let response_body = json!({
            "text": "short clip transcript"
        });
        let (base_url, _) = spawn_mock_server(vec![json_response(200, response_body)]).await?;
        let mut config = OpenRouterProviderConfig::new("", "test-key");
        config.base_url = format!("{base_url}/v1/responses");
        let observer = FixedDebugObserver::shared(DebugCaptureLevel::Redacted);
        let secret_audio = b"AUDIO_SECRET_SENTINEL_123".to_vec();
        let encoded_secret_audio = BASE64_STANDARD.encode(&secret_audio);

        let transcriber = OpenRouterAudioTranscriber::with_observer(config, observer.clone())?;
        let response = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sentinel.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: secret_audio,
                prompt: Some("debug prompt should be summarized".to_string()),
                language: None,
                timestamp_granularities: Vec::new(),
                diarization: false,
            })
            .await?;

        assert_eq!(response.text, "short clip transcript");
        let artifacts = observer.debug_artifacts();
        let request_artifact = artifacts
            .iter()
            .find(|artifact| artifact.name == "openrouter-audio-transcription-provider-request")
            .ok_or_else(|| anyhow::anyhow!("missing OpenRouter transcription request artifact"))?;
        let rendered = serde_json::to_string(&request_artifact.payload)?;
        assert!(
            !rendered.contains("AUDIO_SECRET_SENTINEL_123"),
            "debug artifact leaked raw audio sentinel: {rendered}"
        );
        assert!(
            !rendered.contains(&encoded_secret_audio),
            "debug artifact leaked base64 audio sentinel: {rendered}"
        );
        assert!(
            rendered.contains("sha256"),
            "debug artifact should retain metadata-only checksum context: {rendered}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_audio_transcriber_rejects_oversized_response_body() -> Result<()> {
        let (base_url, _) = spawn_mock_server(vec![MockResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: vec![b'x'; MAX_OPENROUTER_TRANSCRIPTION_RESPONSE_BYTES + 1],
        }])
        .await?;
        let mut config = OpenRouterProviderConfig::new("", "test-key");
        config.base_url = format!("{base_url}/v1/responses");

        let transcriber = OpenRouterAudioTranscriber::new(config)?;
        let error = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sample.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: b"RIFFdata".to_vec(),
                prompt: None,
                language: None,
                timestamp_granularities: Vec::new(),
                diarization: false,
            })
            .await
            .expect_err("oversized response should fail");

        assert!(
            error.message.contains(&format!(
                "OpenRouter transcription response exceeds the {} byte limit",
                MAX_OPENROUTER_TRANSCRIPTION_RESPONSE_BYTES
            )),
            "unexpected error: {}",
            error.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_audio_transcriber_caps_non_success_error_body() -> Result<()> {
        let (base_url, _) = spawn_mock_server(vec![MockResponse {
            status: 500,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: vec![b'x'; MAX_OPENROUTER_TRANSCRIPTION_RESPONSE_BYTES + 1],
        }])
        .await?;
        let mut config = OpenRouterProviderConfig::new("", "test-key");
        config.base_url = format!("{base_url}/v1/responses");

        let transcriber = OpenRouterAudioTranscriber::new(config)?;
        let error = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sample.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: b"RIFFdata".to_vec(),
                prompt: None,
                language: None,
                timestamp_granularities: Vec::new(),
                diarization: false,
            })
            .await
            .expect_err("oversized error response should fail with a bounded body error");

        assert!(error.retryable);
        assert!(
            error.message.contains(&format!(
                "OpenRouter transcription error body exceeds the {} byte limit",
                MAX_OPENROUTER_TRANSCRIPTION_RESPONSE_BYTES
            )),
            "unexpected error: {}",
            error.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_speech_synthesizer_posts_to_tts_endpoint() -> Result<()> {
        let (base_url, captured) = spawn_mock_server(vec![MockResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "audio/L16".to_string())],
            body: b"pcm-audio".to_vec(),
        }])
        .await?;
        let mut config = OpenRouterProviderConfig::new("", "test-key");
        config.base_url = format!("{base_url}/v1/responses");

        let synthesizer = OpenRouterSpeechSynthesizer::new(config)?;
        let response = synthesizer
            .synthesize(&OpenRouterSpeechRequest {
                input: "Hello world".to_string(),
                instructions: Some("Speak with a concise studio-news tone.".to_string()),
                voice: None,
                response_format: None,
                speed: Some(1.25),
            })
            .await?;

        assert_eq!(response.provider, "openrouter");
        assert_eq!(response.model, DEFAULT_OPENROUTER_TTS_MODEL);
        assert_eq!(response.media_type, "audio/L16");
        assert_eq!(response.bytes, b"pcm-audio".to_vec());
        assert_eq!(response.transcript, None);

        let captured = captured.lock();
        assert_eq!(captured[0].path, "/v1/audio/speech");
        let body: Value = serde_json::from_slice(&captured[0].body)?;
        assert_eq!(body["model"], DEFAULT_OPENROUTER_TTS_MODEL);
        assert_eq!(body["voice"], DEFAULT_OPENROUTER_TTS_VOICE);
        assert_eq!(body["response_format"], DEFAULT_OPENROUTER_TTS_FORMAT);
        assert_eq!(body["speed"], 1.25);
        assert_eq!(
            body["provider"]["options"]["openai"]["instructions"],
            "Speak with a concise studio-news tone."
        );
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_speech_synthesizer_rejects_invalid_voice_locally() -> Result<()> {
        let synthesizer =
            OpenRouterSpeechSynthesizer::new(OpenRouterProviderConfig::new("", "test-key"))?;

        let error = synthesizer
            .synthesize(&OpenRouterSpeechRequest {
                input: "Hello world".to_string(),
                instructions: None,
                voice: Some("not-a-real-voice".to_string()),
                response_format: None,
                speed: None,
            })
            .await
            .expect_err("invalid voice should fail locally");

        assert!(
            error
                .message
                .contains("unsupported OpenRouter speech voice"),
            "unexpected error: {}",
            error.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_speech_synthesizer_rejects_oversized_response_stream() -> Result<()> {
        let (base_url, _) =
            spawn_mock_server_with_declared_content_length(MAX_OPENROUTER_AUDIO_RESPONSE_BYTES + 1)
                .await?;
        let mut config = OpenRouterProviderConfig::new("", "test-key");
        config.base_url = format!("{base_url}/v1/responses");

        let synthesizer = OpenRouterSpeechSynthesizer::new(config)?;
        let error = synthesizer
            .synthesize(&OpenRouterSpeechRequest {
                input: "Hello world".to_string(),
                instructions: None,
                voice: None,
                response_format: None,
                speed: None,
            })
            .await
            .expect_err("oversized response should fail");

        assert!(
            error.message.contains("byte limit"),
            "unexpected error: {}",
            error.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn openrouter_speech_synthesizer_records_external_action_traces() -> Result<()> {
        let (base_url, _) = spawn_mock_server(vec![MockResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "audio/mpeg".to_string())],
            body: b"audio".to_vec(),
        }])
        .await?;
        let mut config = OpenRouterProviderConfig::new("", "test-key");
        config.base_url = format!("{base_url}/v1/responses");
        let observer = InMemoryObserver::shared();

        let synthesizer = OpenRouterSpeechSynthesizer::with_observer(config, observer.clone())?;
        let response = synthesizer
            .synthesize(&OpenRouterSpeechRequest {
                input: "Hello world".to_string(),
                instructions: None,
                voice: None,
                response_format: None,
                speed: Some(1.0),
            })
            .await?;

        assert_eq!(response.media_type, "audio/mpeg");
        let traces = observer.traces();
        assert!(has_provider_external_action(
            &traces,
            "request",
            "openrouter:http://"
        ));
        assert!(has_provider_external_action(
            &traces,
            "response",
            "openrouter:http://"
        ));
        Ok(())
    }

    #[test]
    fn resolve_openrouter_tts_model_upgrades_known_stale_aliases() {
        assert_eq!(
            resolve_openrouter_tts_model(""),
            DEFAULT_OPENROUTER_TTS_MODEL
        );
        assert_eq!(
            resolve_openrouter_tts_model("tts-1"),
            DEFAULT_OPENROUTER_TTS_MODEL
        );
        assert_eq!(
            resolve_openrouter_tts_model("openai/tts-1-hd"),
            DEFAULT_OPENROUTER_TTS_MODEL
        );
        assert_eq!(
            resolve_openrouter_tts_model("gpt-4o-mini-tts"),
            DEFAULT_OPENROUTER_TTS_MODEL
        );
        assert_eq!(
            resolve_openrouter_tts_model("openai/gpt-4o-mini-tts"),
            DEFAULT_OPENROUTER_TTS_MODEL
        );
        assert_eq!(
            resolve_openrouter_tts_model("elevenlabs/eleven-turbo-v2"),
            DEFAULT_OPENROUTER_TTS_MODEL
        );
    }

    async fn spawn_mock_server(
        responses: Vec<MockResponse>,
    ) -> Result<(String, Arc<Mutex<Vec<CapturedRequest>>>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_requests = captured.clone();

        tokio::spawn(async move {
            for response in responses {
                let (mut socket, _) = listener.accept().await.expect("server should accept");
                let request = read_request(&mut socket)
                    .await
                    .expect("request read should succeed");
                captured_requests.lock().push(request);
                write_response(&mut socket, &response)
                    .await
                    .expect("response write should succeed");
            }
        });

        Ok((format!("http://{address}"), captured))
    }

    async fn spawn_mock_server_with_declared_content_length(
        declared_content_length: usize,
    ) -> Result<(String, Arc<Mutex<Vec<CapturedRequest>>>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_requests = captured.clone();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("server should accept");
            let request = read_request(&mut socket)
                .await
                .expect("request read should succeed");
            captured_requests.lock().push(request);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {declared_content_length}\r\nContent-Type: audio/mpeg\r\n\r\nx"
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("response write should succeed");
        });

        Ok((format!("http://{address}"), captured))
    }

    async fn read_request(socket: &mut TcpStream) -> Result<CapturedRequest> {
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = socket.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }

        let head_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
            .expect("request headers should terminate");
        let head = String::from_utf8(request[..head_end - 4].to_vec())?;
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts.next().unwrap_or_default().to_string();
        let path = request_parts.next().unwrap_or_default().to_string();
        let mut headers = BTreeMap::new();
        let mut content_length = 0usize;
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                let name = name.trim().to_ascii_lowercase();
                let value = value.trim().to_string();
                if name == "content-length" {
                    content_length = value.parse::<usize>().unwrap_or_default();
                }
                headers.insert(name, value);
            }
        }

        let mut body = request[head_end..].to_vec();
        while body.len() < content_length {
            let read = socket.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buffer[..read]);
        }

        Ok(CapturedRequest {
            method,
            path,
            headers,
            body,
        })
    }

    async fn write_response(socket: &mut TcpStream, response: &MockResponse) -> Result<()> {
        let mut head = format!(
            "HTTP/1.1 {} OK\r\nContent-Length: {}\r\n",
            response.status,
            response.body.len()
        );
        for (name, value) in &response.headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        socket.write_all(head.as_bytes()).await?;
        socket.write_all(&response.body).await?;
        Ok(())
    }

    fn json_response(status: u16, body: Value) -> MockResponse {
        MockResponse {
            status,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: serde_json::to_vec(&body).expect("json body"),
        }
    }
}
