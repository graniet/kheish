use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Formatter};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use futures_util::StreamExt;
use reqwest::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER,
};
use reqwest::multipart::{Form, Part};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::model::{
    ModelEventSink, ModelFinishReason, ModelProvider, ModelRuntimeRequest, ModelStreamEvent,
    ProviderError, ReasoningConfig, ResponseFormat, StructuredFieldSchema, ToolChoice,
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
use kheish_codec::{digest_bytes, digest_json_value};
use kheish_core::ModelRequestKind;
use kheish_types::{InputContentPart, model_max_output_tokens};

use super::attachments::{
    AttachmentRenderCache, contains_supported_image_attachment, image_edit_attachment_hint_text,
    load_attachment_preview_image, load_document_attachment_text, load_image_attachment,
};
use super::errors::{safe_error_payload_for_level, sanitize_upstream_error_message};
use super::prompt::{
    NormalizedConversationItem, NormalizedProviderPrompt, normalize_provider_prompt,
};
use super::schema::structured_schema_json;
use super::sse::{JsonSseEvent, parse_json_sse_frame, pop_sse_frame};

const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1/responses";
const DEFAULT_XAI_BASE_URL: &str = "https://api.x.ai/v1/responses";
const DEFAULT_OPENAI_TTS_MODEL: &str = "gpt-4o-mini-tts";
const DEFAULT_OPENAI_TTS_VOICE: &str = "alloy";
const DEFAULT_OPENAI_TTS_FORMAT: &str = "mp3";
const CODEX_RESPONSES_PATH_MARKER: &str = "/backend-api/codex/responses";
const XAI_MAX_IMAGE_INPUT_BYTES: usize = 20 * 1024 * 1024;
const OPENAI_STRICT_OPTIONAL_FIELD_LIMIT: usize = 12;
const OPENAI_AUDIO_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OPENAI_AUDIO_RESPONSE_BYTES: usize = 12 * 1024 * 1024;
const MAX_OPENAI_IMAGE_RESPONSE_BYTES: usize = 24 * 1024 * 1024;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResponsesProviderFlavor {
    OpenAi,
    XAi,
}

impl ResponsesProviderFlavor {
    fn provider_name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::XAi => "xai",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::XAi => "xAI",
        }
    }

    fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAi => DEFAULT_OPENAI_BASE_URL,
            Self::XAi => DEFAULT_XAI_BASE_URL,
        }
    }

    fn supports_instructions(self) -> bool {
        matches!(self, Self::OpenAi)
    }
}
/// Token pricing used to estimate request cost from usage snapshots.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenAiPricing {
    /// Price per one million input tokens.
    pub input_per_million_tokens_usd: f64,
    /// Price per one million output tokens.
    pub output_per_million_tokens_usd: f64,
}

/// Configuration for the OpenAI Responses provider adapter.
#[derive(Clone)]
pub struct OpenAiProviderConfig {
    /// The model identifier.
    pub model: String,
    /// The API key.
    pub api_key: Option<String>,
    /// Optional request-scoped auth provider used to resolve account-backed credentials.
    pub request_auth_provider: Option<Arc<dyn RequestAuthProvider>>,
    /// The Responses API endpoint.
    pub base_url: String,
    /// Optional organization header.
    pub organization: Option<String>,
    /// Optional project header.
    pub project: Option<String>,
    /// Default output token ceiling when the generation config does not override it.
    pub default_max_output_tokens: u32,
    /// Optional static pricing table for cost estimation.
    pub pricing: Option<OpenAiPricing>,
    /// Optional daemon-owned asset root used to resolve opaque attachment URIs.
    pub asset_root: Option<PathBuf>,
    /// Shared in-process cache for prepared attachment payloads.
    pub(crate) attachment_cache: AttachmentRenderCache,
    /// The OpenAI-compatible provider flavor used by this configuration.
    pub(crate) flavor: ResponsesProviderFlavor,
}

impl OpenAiProviderConfig {
    /// Creates a configuration with the standard OpenAI Responses endpoint.
    pub fn new(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        let model = model.into();
        Self {
            default_max_output_tokens: model_max_output_tokens(&model).default,
            pricing: default_openai_pricing(&model),
            model,
            api_key: Some(api_key.into()),
            request_auth_provider: None,
            base_url: DEFAULT_OPENAI_BASE_URL.to_string(),
            organization: None,
            project: None,
            asset_root: None,
            attachment_cache: AttachmentRenderCache::default(),
            flavor: ResponsesProviderFlavor::OpenAi,
        }
    }

    /// Loads the API key from an environment variable.
    pub fn from_env(
        model: impl Into<String>,
        env_var: impl AsRef<str>,
    ) -> Result<Self, ProviderError> {
        let env_var = env_var.as_ref();
        let api_key = std::env::var(env_var).map_err(|_| ProviderError {
            message: format!("missing OpenAI API key in environment variable {env_var}"),
            retryable: false,
            retry_after_ms: None,
        })?;
        Ok(Self::new(model, api_key))
    }

    /// Builds a configuration whose request authorization is resolved dynamically.
    pub fn with_request_auth_provider(
        model: impl Into<String>,
        request_auth_provider: Arc<dyn RequestAuthProvider>,
    ) -> Self {
        let model = model.into();
        Self {
            default_max_output_tokens: model_max_output_tokens(&model).default,
            pricing: default_openai_pricing(&model),
            model,
            api_key: None,
            request_auth_provider: Some(request_auth_provider),
            base_url: DEFAULT_OPENAI_BASE_URL.to_string(),
            organization: None,
            project: None,
            asset_root: None,
            attachment_cache: AttachmentRenderCache::default(),
            flavor: ResponsesProviderFlavor::OpenAi,
        }
    }

    pub(crate) fn with_flavor(mut self, flavor: ResponsesProviderFlavor) -> Self {
        self.flavor = flavor;
        self.base_url = flavor.default_base_url().to_string();
        self
    }
}

impl Debug for OpenAiProviderConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProviderConfig")
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field(
                "request_auth_provider",
                &self.request_auth_provider.as_ref().map(|_| "<configured>"),
            )
            .field("base_url", &self.base_url)
            .field("organization", &self.organization)
            .field("project", &self.project)
            .field("default_max_output_tokens", &self.default_max_output_tokens)
            .field("pricing", &self.pricing)
            .field("asset_root", &self.asset_root)
            .field("flavor", &self.flavor)
            .field("attachment_cache", &"<configured>")
            .finish()
    }
}

/// OpenAI Responses streaming provider.
pub struct OpenAiProvider {
    client: Client,
    config: OpenAiProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

impl OpenAiProvider {
    /// Builds a new OpenAI provider using a dedicated HTTP client.
    pub fn new(config: OpenAiProviderConfig) -> Result<Self, ProviderError> {
        Self::with_observer(config, Arc::new(NoopObserver))
    }

    /// Builds a new OpenAI provider with runtime observation hooks enabled.
    pub fn with_observer(
        config: OpenAiProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        let client = Client::builder().build().map_err(|error| ProviderError {
            message: format!("failed to build OpenAI HTTP client: {error}"),
            retryable: false,
            retry_after_ms: None,
        })?;
        Ok(Self {
            client,
            config,
            observer,
        })
    }

    fn flavor(&self) -> ResponsesProviderFlavor {
        self.config.flavor
    }

    fn provider_name(&self) -> &'static str {
        self.flavor().provider_name()
    }

    fn display_name(&self) -> &'static str {
        self.flavor().display_name()
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
                message: format!(
                    "failed to resolve {} auth material: {error}",
                    self.display_name()
                ),
                retryable: false,
                retry_after_ms: None,
            });
        }
        let api_key = self.config.api_key.clone().ok_or_else(|| ProviderError {
            message: format!("missing {} API key", self.display_name()),
            retryable: false,
            retry_after_ms: None,
        })?;
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {api_key}"));
        if matches!(self.flavor(), ResponsesProviderFlavor::OpenAi)
            && let Some(organization) = &self.config.organization
        {
            headers.insert("OpenAI-Organization".to_string(), organization.clone());
        }
        if matches!(self.flavor(), ResponsesProviderFlavor::OpenAi)
            && let Some(project) = &self.config.project
        {
            headers.insert("OpenAI-Project".to_string(), project.clone());
        }
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
                    message: format!("OpenAI auth material is no longer active: {error}"),
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
                        message: format!("invalid OpenAI header name `{name}`: {error}"),
                        retryable: false,
                        retry_after_ms: None,
                    }
                })?
            };
            headers.insert(
                header_name,
                HeaderValue::from_str(value).map_err(|error| ProviderError {
                    message: format!("invalid OpenAI header `{name}`: {error}"),
                    retryable: false,
                    retry_after_ms: None,
                })?,
            );
        }
        if let Some(organization) = &self.config.organization {
            headers.insert(
                reqwest::header::HeaderName::from_static("openai-organization"),
                HeaderValue::from_str(organization).map_err(|error| ProviderError {
                    message: format!("invalid OpenAI organization header: {error}"),
                    retryable: false,
                    retry_after_ms: None,
                })?,
            );
        }
        if let Some(project) = &self.config.project {
            headers.insert(
                reqwest::header::HeaderName::from_static("openai-project"),
                HeaderValue::from_str(project).map_err(|error| ProviderError {
                    message: format!("invalid OpenAI project header: {error}"),
                    retryable: false,
                    retry_after_ms: None,
                })?,
            );
        }
        Ok(headers)
    }

    fn is_codex_account_endpoint(endpoint: &str) -> bool {
        endpoint.contains(CODEX_RESPONSES_PATH_MARKER)
    }

    fn build_request_body(
        &self,
        request: &ModelRuntimeRequest,
        codex_compat: bool,
    ) -> Result<Value, ProviderError> {
        let normalized = normalize_provider_prompt(&request.prompt);
        let effective_model = request
            .generation
            .model
            .clone()
            .unwrap_or_else(|| self.config.model.clone());
        let (previous_response_id, input_items) = openai_conversation_delta(
            &normalized,
            request.kind,
            &effective_model,
            self.config.asset_root.as_deref(),
            &self.config.attachment_cache,
            self.flavor(),
            codex_compat,
        )?;
        let default_max_output_tokens = model_max_output_tokens(&effective_model).default;
        let instructions = normalized.instructions.join("\n\n");
        let tools = if matches!(request.generation.tool_choice, ToolChoice::None) {
            Vec::new()
        } else {
            request
                .available_tools
                .iter()
                .map(|tool| {
                    let (parameters, strict) = openai_tool_parameters_schema(&tool.input_schema);
                    let mut entry = json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": parameters,
                    });
                    if matches!(self.flavor(), ResponsesProviderFlavor::OpenAi) {
                        entry["strict"] = Value::Bool(strict);
                    }
                    entry
                })
                .collect()
        };
        if matches!(self.flavor(), ResponsesProviderFlavor::XAi)
            && !tools.is_empty()
            && matches!(
                request.generation.response_format,
                ResponseFormat::StructuredJson { .. }
            )
            && !xai_model_supports_structured_outputs_with_tools(&effective_model)
        {
            return Err(ProviderError {
                message: format!(
                    "xAI model '{effective_model}' does not support structured outputs with tools"
                ),
                retryable: false,
                retry_after_ms: None,
            });
        }

        let mut body = serde_json::Map::new();
        body.insert("model".to_string(), Value::String(effective_model));
        body.insert("stream".to_string(), Value::Bool(true));
        let mut input = input_items;
        if !instructions.is_empty() && !self.flavor().supports_instructions() {
            input.insert(0, responses_system_message(&instructions));
        }
        let store = !codex_compat
            && !(matches!(self.flavor(), ResponsesProviderFlavor::XAi)
                && responses_input_contains_image_parts(&input));
        body.insert("store".to_string(), Value::Bool(store));
        body.insert("input".to_string(), Value::Array(input));
        if let Some(previous_response_id) = previous_response_id {
            body.insert(
                "previous_response_id".to_string(),
                Value::String(previous_response_id),
            );
        }
        if self.flavor().supports_instructions() && (codex_compat || !instructions.is_empty()) {
            body.insert(
                "instructions".to_string(),
                Value::String(if instructions.is_empty() {
                    "You are a helpful assistant.".to_string()
                } else {
                    instructions
                }),
            );
        }
        if codex_compat {
            body.insert("tools".to_string(), Value::Array(tools));
            body.insert(
                "parallel_tool_calls".to_string(),
                Value::Bool(request.generation.allow_parallel_tool_calls),
            );
            body.insert("include".to_string(), Value::Array(Vec::new()));
            if let Some(tool_choice) = openai_tool_choice(
                self.flavor(),
                !request.available_tools.is_empty(),
                &request.generation.tool_choice,
            ) {
                body.insert("tool_choice".to_string(), tool_choice);
            }
        } else {
            body.insert(
                "max_output_tokens".to_string(),
                Value::Number(
                    request
                        .generation
                        .max_output_tokens
                        .unwrap_or(default_max_output_tokens)
                        .into(),
                ),
            );
            if !tools.is_empty() {
                body.insert("tools".to_string(), Value::Array(tools));
                body.insert(
                    "parallel_tool_calls".to_string(),
                    Value::Bool(request.generation.allow_parallel_tool_calls),
                );
                if let Some(tool_choice) = openai_tool_choice(
                    self.flavor(),
                    !request.available_tools.is_empty(),
                    &request.generation.tool_choice,
                ) {
                    body.insert("tool_choice".to_string(), tool_choice);
                }
            }
        }
        if let Some(reasoning) =
            openai_reasoning_payload(self.flavor(), request.generation.reasoning.as_ref())?
        {
            body.insert("reasoning".to_string(), reasoning);
        }
        if !codex_compat
            && openai_model_supports_temperature(self.flavor(), &self.config.model, request)
            && let Some(temperature) = request.generation.temperature
        {
            body.insert(
                "temperature".to_string(),
                serde_json::Number::from_f64(temperature as f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
            );
        }
        if !matches!(
            (self.flavor(), &request.generation.response_format),
            (ResponsesProviderFlavor::XAi, ResponseFormat::Text)
        ) {
            body.insert(
                "text".to_string(),
                match &request.generation.response_format {
                    ResponseFormat::Text => json!({
                        "format": {
                            "type": "text",
                        }
                    }),
                    ResponseFormat::StructuredJson { schema } => {
                        let schema = openai_structured_response_schema(schema)?;
                        json!({
                            "format": {
                                "type": "json_schema",
                                "name": "kheish_response",
                                "schema": schema,
                                "strict": true,
                            }
                        })
                    }
                },
            );
        }
        Ok(Value::Object(body))
    }

    fn debug_level(&self) -> DebugCaptureLevel {
        self.observer.debug_level()
    }

    fn external_action_target(&self, endpoint: &str) -> String {
        format!(
            "{}:{}",
            self.provider_name(),
            safe_url_audit_target(endpoint)
        )
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
                "provider": self.provider_name(),
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
                "provider": self.provider_name(),
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
                    "provider": self.provider_name(),
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
                    "provider": self.provider_name(),
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
        let payload = if is_openai_error_event(event.event_type.as_str()) {
            safe_error_payload_for_level(
                level,
                &openai_error_event_payload(event, self.display_name()),
            )
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
                "provider": self.provider_name(),
                "event_type": event.event_type,
                "payload": payload,
            }),
        ));
    }
}

fn openai_reasoning_payload(
    flavor: ResponsesProviderFlavor,
    reasoning: Option<&ReasoningConfig>,
) -> Result<Option<Value>, ProviderError> {
    let Some(reasoning) = reasoning else {
        return Ok(None);
    };
    if !matches!(flavor, ResponsesProviderFlavor::OpenAi) {
        return Err(ProviderError {
            message: format!(
                "{} routes do not support OpenAI reasoning options",
                flavor.display_name()
            ),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if reasoning.budget_tokens.is_some() {
        return Err(ProviderError {
            message: "OpenAI reasoning does not accept budget_tokens; use effort instead"
                .to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if reasoning.interleaved {
        return Err(ProviderError {
            message: "OpenAI reasoning does not support interleaved thinking".to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }

    let mut payload = serde_json::Map::new();
    if let Some(effort) = reasoning.effort {
        payload.insert(
            "effort".to_string(),
            Value::String(effort.as_str().to_string()),
        );
    }
    if let Some(summary) = reasoning
        .summary
        .and_then(|summary| summary.as_provider_str())
    {
        payload.insert("summary".to_string(), Value::String(summary.to_string()));
    }
    if payload.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Value::Object(payload)))
    }
}

fn is_openai_error_event(event_type: &str) -> bool {
    matches!(event_type, "response.failed" | "error")
}

fn provider_audit_error(error: anyhow::Error) -> ProviderError {
    ProviderError {
        message: format!("external action audit failed: {error}"),
        retryable: false,
        retry_after_ms: None,
    }
}

fn responses_input_contains_image_parts(items: &[Value]) -> bool {
    items.iter().any(|item| {
        item.get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| part.get("type").and_then(Value::as_str) == Some("input_image"))
            })
    })
}

fn openai_error_event_payload(event: &JsonSseEvent, provider_display_name: &str) -> Value {
    let (raw_message, error_type, error_code) = openai_error_event_fields(event);
    json!({
        "message": sanitize_upstream_error_message(
            provider_display_name,
            "stream error",
            None,
            error_type,
            error_code,
            raw_message,
        ),
        "error_type": error_type,
        "error_code": error_code,
        "has_error_object": event.payload.get("error").is_some_and(Value::is_object),
        "has_response_error": event
            .payload
            .get("response")
            .and_then(|value| value.get("error"))
            .is_some(),
    })
}

fn openai_error_event_fields(event: &JsonSseEvent) -> (Option<&str>, Option<&str>, Option<&str>) {
    let error = match event.event_type.as_str() {
        "response.failed" => event
            .payload
            .get("response")
            .and_then(|value| value.get("error"))
            .or_else(|| event.payload.get("error")),
        "error" => event
            .payload
            .get("error")
            .filter(|value| value.is_object())
            .or(Some(&event.payload)),
        _ => None,
    };

    let message = error
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .or_else(|| event.payload.get("message").and_then(Value::as_str));
    let error_type = error
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str)
        .or_else(|| event.payload.get("type").and_then(Value::as_str));
    let error_code = error
        .and_then(|value| value.get("code"))
        .and_then(Value::as_str)
        .or_else(|| event.payload.get("code").and_then(Value::as_str));

    (message, error_type, error_code)
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

fn openai_structured_response_schema(
    schema: &StructuredFieldSchema,
) -> Result<Value, ProviderError> {
    let schema = structured_schema_json(schema);
    openai_strict_tool_schema(&schema).ok_or_else(|| ProviderError {
        message: "OpenAI structured response schema is not strict-compatible".to_string(),
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
                        .collect::<BTreeSet<_>>()
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
                    .collect::<BTreeSet<_>>()
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

#[async_trait]
impl ModelProvider for OpenAiProvider {
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
            let body =
                self.build_request_body(&request, Self::is_codex_account_endpoint(&endpoint))?;
            let headers = self.headers_from_material(&auth_material)?;
            debug!(
                session_id = %request.session_id,
                thread_id = request.thread_id.as_deref(),
                turn = request.turn,
                provider = self.provider_name(),
                model = request
                    .generation
                    .model
                    .as_deref()
                    .unwrap_or(self.config.model.as_str()),
                endpoint = %endpoint,
                codex_account_endpoint = Self::is_codex_account_endpoint(&endpoint),
                tool_count = request.available_tools.len(),
                forced_refresh = force_refresh,
                "starting provider request"
            );
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
                    let mapped = map_transport_error(error, self.display_name());
                    self.record_provider_failure(
                        self.external_action_target(&endpoint),
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
                    self.external_action_target(&endpoint),
                    "401-refresh",
                    grant_id,
                )?;
                warn!(
                    session_id = %request.session_id,
                    turn = request.turn,
                    provider = self.provider_name(),
                    endpoint = %endpoint,
                    "provider returned 401, forcing auth refresh"
                );
                force_refresh = true;
                continue;
            }
            break (response, self.external_action_target(&endpoint), grant_id);
        };

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let response_headers = response.headers().clone();
            let error = map_http_error(response, self.display_name()).await;
            warn!(
                session_id = %request.session_id,
                thread_id = request.thread_id.as_deref(),
                turn = request.turn,
                provider = self.provider_name(),
                model = request
                    .generation
                    .model
                    .as_deref()
                    .unwrap_or(self.config.model.as_str()),
                status,
                retryable = error.retryable,
                retry_after_ms = error.retry_after_ms,
                error = %error.message,
                "provider request failed"
            );
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
        let mut function_calls = BTreeMap::new();
        let mut message_text = BTreeMap::new();
        let mut emitted_tool_calls = BTreeSet::new();
        // Buffer raw bytes until a full SSE frame arrives so split UTF-8 code points
        // across transport chunks do not fail decoding prematurely.
        let mut buffer = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    let mapped = map_transport_error(error, self.display_name());
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
                let parsed =
                    parse_json_sse_frame(&frame, self.display_name()).map_err(|error| {
                        self.record_provider_failure(
                            &response_target,
                            &error.message,
                            response_grant_id.clone(),
                        )
                        .err()
                        .unwrap_or(error)
                    })?;
                if let Some(event) = parsed {
                    self.record_provider_event(&request, &event);
                    handle_sse_event(
                        event,
                        &mut function_calls,
                        &mut message_text,
                        &mut emitted_tool_calls,
                        &sink,
                        self.config.pricing.as_ref(),
                        self.display_name(),
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
        }

        if !buffer.iter().all(|byte| byte.is_ascii_whitespace()) {
            let parsed = parse_json_sse_frame(&buffer, self.display_name()).map_err(|error| {
                self.record_provider_failure(
                    &response_target,
                    &error.message,
                    response_grant_id.clone(),
                )
                .err()
                .unwrap_or(error)
            })?;
            if let Some(event) = parsed {
                self.record_provider_event(&request, &event);
                handle_sse_event(
                    event,
                    &mut function_calls,
                    &mut message_text,
                    &mut emitted_tool_calls,
                    &sink,
                    self.config.pricing.as_ref(),
                    self.display_name(),
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

        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
struct PartialFunctionCall {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
}

fn openai_conversation_delta(
    prompt: &NormalizedProviderPrompt,
    request_kind: ModelRequestKind,
    effective_model: &str,
    asset_root: Option<&std::path::Path>,
    cache: &AttachmentRenderCache,
    flavor: ResponsesProviderFlavor,
    codex_compat: bool,
) -> Result<(Option<String>, Vec<Value>), ProviderError> {
    // Compaction requests are reconstructed from checkpoint/journal state and must stay
    // reproducible without provider-side continuation identifiers.
    if codex_compat
        || matches!(flavor, ResponsesProviderFlavor::XAi)
        || matches!(request_kind, ModelRequestKind::Compaction)
    {
        let full_items = openai_input_items(
            &prompt.conversation,
            effective_model,
            asset_root,
            cache,
            flavor,
        )?;
        return Ok((None, full_items));
    }

    let mut previous_response_id = None;
    let mut start_index = 0usize;
    for (index, item) in prompt.conversation.iter().enumerate() {
        match item {
            NormalizedConversationItem::AssistantMessage {
                provider_response_id: Some(response_id),
                ..
            } if openai_response_id_usable_for_resume(response_id) => {
                previous_response_id = Some(response_id.clone());
                start_index = index + 1;
            }
            NormalizedConversationItem::AssistantToolCalls {
                assistant_provider_response_id: Some(response_id),
                ..
            } if openai_response_id_usable_for_resume(response_id) => {
                previous_response_id = Some(response_id.clone());
                start_index = index + 1;
            }
            _ => {}
        }
    }
    let items = if previous_response_id.is_some() {
        let response_id = previous_response_id
            .as_deref()
            .expect("checked previous response id");
        if openai_resume_delta_tool_results_are_safe(&prompt.conversation, start_index, response_id)
        {
            openai_input_items(
                &prompt.conversation[start_index..],
                effective_model,
                asset_root,
                cache,
                flavor,
            )?
        } else {
            previous_response_id = None;
            openai_input_items(
                &prompt.conversation,
                effective_model,
                asset_root,
                cache,
                flavor,
            )?
        }
    } else {
        openai_input_items(
            &prompt.conversation,
            effective_model,
            asset_root,
            cache,
            flavor,
        )?
    };
    Ok((previous_response_id, items))
}

fn openai_resume_delta_tool_results_are_safe(
    conversation: &[NormalizedConversationItem],
    start_index: usize,
    previous_response_id: &str,
) -> bool {
    let mut call_response_ids = BTreeMap::new();
    for item in &conversation[..start_index] {
        let NormalizedConversationItem::AssistantToolCalls {
            assistant_provider_response_id,
            calls,
            ..
        } = item
        else {
            continue;
        };
        let Some(response_id) = assistant_provider_response_id.as_deref() else {
            continue;
        };
        for call in calls {
            call_response_ids.insert(call.id.as_str(), response_id);
        }
    }

    let mut saw_non_tool_result = false;
    for item in &conversation[start_index..] {
        match item {
            NormalizedConversationItem::ToolResults { results } => {
                if saw_non_tool_result {
                    return false;
                }
                for result in results {
                    if call_response_ids.get(result.call_id.as_str()).copied()
                        != Some(previous_response_id)
                    {
                        return false;
                    }
                }
            }
            _ => saw_non_tool_result = true,
        }
    }

    true
}

fn openai_response_id_usable_for_resume(response_id: &str) -> bool {
    response_id.starts_with("resp_")
}

fn openai_input_items(
    conversation: &[NormalizedConversationItem],
    effective_model: &str,
    asset_root: Option<&std::path::Path>,
    cache: &AttachmentRenderCache,
    flavor: ResponsesProviderFlavor,
) -> Result<Vec<Value>, ProviderError> {
    let mut items = Vec::new();
    for item in conversation {
        match item {
            NormalizedConversationItem::UserMessage {
                content,
                content_parts,
                attachments,
                ..
            } => {
                if contains_supported_image_attachment(content_parts, attachments)
                    && !provider_model_supports_image_input(flavor, effective_model)
                {
                    return Err(ProviderError {
                        message: format!(
                            "{} model '{effective_model}' does not support image attachments",
                            flavor.display_name(),
                        ),
                        retryable: false,
                        retry_after_ms: None,
                    });
                }
                let content_blocks = openai_user_content_blocks(
                    content,
                    content_parts,
                    attachments,
                    asset_root,
                    cache,
                    flavor,
                    provider_model_supports_image_input(flavor, effective_model),
                )
                .map_err(|error| ProviderError {
                    message: format!("failed to prepare user content blocks: {error}"),
                    retryable: false,
                    retry_after_ms: None,
                })?;
                if content_blocks.is_empty() {
                    continue;
                }
                items.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": content_blocks,
                }));
            }
            NormalizedConversationItem::AssistantMessage { content, .. } => items.push(json!({
                "type": "message",
                "role": "assistant",
                "content": [
                    {
                        "type": "output_text",
                        "text": content,
                    }
                ]
            })),
            NormalizedConversationItem::AssistantToolCalls { calls, .. } => {
                for call in calls {
                    items.push(json!({
                        "type": "function_call",
                        "id": format!("fc_{}", call.id),
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": serde_json::to_string(&call.input)
                            .unwrap_or_else(|_| "{}".to_string()),
                        "status": "completed",
                    }));
                }
            }
            NormalizedConversationItem::ToolResults { results } => {
                for result in results {
                    let output = match &result.output {
                        Value::String(value) => Value::String(value.clone()),
                        value => Value::String(
                            serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()),
                        ),
                    };
                    items.push(json!({
                        "type": "function_call_output",
                        "call_id": result.call_id,
                        "output": output,
                    }));
                }
            }
        }
    }
    Ok(items)
}

fn responses_system_message(instructions: &str) -> Value {
    json!({
        "type": "message",
        "role": "system",
        "content": [
            {
                "type": "input_text",
                "text": instructions,
            }
        ],
    })
}

fn openai_user_content_blocks(
    fallback_content: &str,
    content_parts: &[InputContentPart],
    attachments: &[kheish_types::AttachmentRef],
    asset_root: Option<&std::path::Path>,
    cache: &AttachmentRenderCache,
    flavor: ResponsesProviderFlavor,
    include_document_previews: bool,
) -> anyhow::Result<Vec<Value>> {
    fn push_image_attachment_blocks(
        blocks: &mut Vec<Value>,
        attachment: &kheish_types::AttachmentRef,
        image: super::attachments::PreparedImageAttachment,
    ) {
        if let Some(text) = image_edit_attachment_hint_text(attachment) {
            blocks.push(json!({
                "type": "input_text",
                "text": text,
            }));
        }
        blocks.push(json!({
            "type": "input_image",
            "image_url": image.data_url(),
        }));
    }

    let mut blocks = Vec::new();
    if !content_parts.is_empty() {
        for part in content_parts {
            match part {
                InputContentPart::Text { text } if !text.trim().is_empty() => blocks.push(json!({
                    "type": "input_text",
                    "text": text,
                })),
                InputContentPart::Text { .. } => {}
                InputContentPart::Attachment { attachment } => {
                    if let Some(image) = load_image_attachment(attachment, asset_root, cache)? {
                        if matches!(flavor, ResponsesProviderFlavor::XAi)
                            && image.size_bytes > XAI_MAX_IMAGE_INPUT_BYTES
                        {
                            anyhow::bail!(
                                "xAI image attachment '{}' exceeds the 20 MiB limit",
                                image.id
                            );
                        }
                        push_image_attachment_blocks(&mut blocks, attachment, image);
                        continue;
                    }
                    if include_document_previews {
                        if let Some(preview) =
                            load_attachment_preview_image(attachment, asset_root, cache)?
                        {
                            blocks.push(json!({
                                "type": "input_image",
                                "image_url": preview.data_url(),
                            }));
                        }
                    }
                    if let Some(text) =
                        load_document_attachment_text(attachment, asset_root, cache)?
                    {
                        blocks.push(json!({
                            "type": "input_text",
                            "text": text,
                        }));
                    }
                }
            }
        }
        return Ok(blocks);
    }

    if !fallback_content.trim().is_empty() {
        blocks.push(json!({
            "type": "input_text",
            "text": fallback_content,
        }));
    }
    for attachment in attachments {
        if let Some(image) = load_image_attachment(attachment, asset_root, cache)? {
            if matches!(flavor, ResponsesProviderFlavor::XAi)
                && image.size_bytes > XAI_MAX_IMAGE_INPUT_BYTES
            {
                anyhow::bail!(
                    "xAI image attachment '{}' exceeds the 20 MiB limit",
                    image.id
                );
            }
            push_image_attachment_blocks(&mut blocks, attachment, image);
            continue;
        }
        if include_document_previews {
            if let Some(preview) = load_attachment_preview_image(attachment, asset_root, cache)? {
                blocks.push(json!({
                    "type": "input_image",
                    "image_url": preview.data_url(),
                }));
            }
        }
        if let Some(text) = load_document_attachment_text(attachment, asset_root, cache)? {
            blocks.push(json!({
                "type": "input_text",
                "text": text,
            }));
        }
    }
    Ok(blocks)
}

fn provider_model_supports_image_input(flavor: ResponsesProviderFlavor, model: &str) -> bool {
    let canonical = model.trim().to_ascii_lowercase();
    match flavor {
        ResponsesProviderFlavor::OpenAi => {
            canonical.contains("gpt-5")
                || canonical.contains("gpt-4.1")
                || canonical.contains("gpt-4o")
                || canonical.contains("o1")
                || canonical.contains("o3")
                || canonical.contains("o4")
        }
        ResponsesProviderFlavor::XAi => {
            canonical.starts_with("grok") && !canonical.contains("imagine")
        }
    }
}

fn xai_model_supports_structured_outputs_with_tools(model: &str) -> bool {
    model.trim().to_ascii_lowercase().starts_with("grok-4")
}

fn openai_tool_choice(
    flavor: ResponsesProviderFlavor,
    has_available_tools: bool,
    tool_choice: &ToolChoice,
) -> Option<Value> {
    if matches!(flavor, ResponsesProviderFlavor::XAi) && !has_available_tools {
        return None;
    }
    match tool_choice {
        ToolChoice::Auto => Some(Value::String("auto".to_string())),
        ToolChoice::Required => Some(Value::String("required".to_string())),
        ToolChoice::Specific { name } => Some(json!({
            "type": "function",
            "name": name,
        })),
        ToolChoice::None => Some(Value::String("none".to_string())),
    }
}

fn openai_model_supports_temperature(
    flavor: ResponsesProviderFlavor,
    configured_model: &str,
    request: &ModelRuntimeRequest,
) -> bool {
    if !matches!(flavor, ResponsesProviderFlavor::OpenAi) {
        return true;
    }
    let effective_model = request
        .generation
        .model
        .as_deref()
        .unwrap_or(configured_model)
        .trim()
        .trim_matches('"')
        .to_ascii_lowercase();
    !effective_model.starts_with("gpt-5")
}

fn default_openai_pricing(model: &str) -> Option<OpenAiPricing> {
    let canonical = model.trim().trim_matches('"').to_ascii_lowercase();
    match canonical.as_str() {
        // Source: https://platform.openai.com/docs/models/gpt-5-mini and
        // https://platform.openai.com/docs/pricing
        "gpt-5-mini" => Some(OpenAiPricing {
            input_per_million_tokens_usd: 0.25,
            output_per_million_tokens_usd: 2.0,
        }),
        // Source: https://openai.com/api/pricing
        "gpt-5.4" => Some(OpenAiPricing {
            input_per_million_tokens_usd: 2.5,
            output_per_million_tokens_usd: 15.0,
        }),
        _ => None,
    }
}

fn handle_sse_event(
    event: JsonSseEvent,
    function_calls: &mut BTreeMap<String, PartialFunctionCall>,
    message_text: &mut BTreeMap<String, String>,
    emitted_tool_calls: &mut BTreeSet<String>,
    sink: &ModelEventSink,
    pricing: Option<&OpenAiPricing>,
    provider_display_name: &str,
) -> Result<(), ProviderError> {
    match event.event_type.as_str() {
        "response.created" => {
            if let Some(id) = event
                .payload
                .get("response")
                .and_then(|response| response.get("id"))
                .and_then(Value::as_str)
            {
                sink.emit(ModelStreamEvent::MessageId {
                    value: id.to_string(),
                })
                .map_err(map_sink_error)?;
            }
            Ok(())
        }
        "response.in_progress" => Ok(()),
        "response.output_item.added" => {
            let item = event.payload.get("item").ok_or_else(|| ProviderError {
                message: "missing Responses output item".to_string(),
                retryable: true,
                retry_after_ms: None,
            })?;
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                        message_text.entry(item_id.to_string()).or_default();
                    }
                    Ok(())
                }
                Some("function_call") => {
                    let item_id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| ProviderError {
                            message: "missing Responses function call item id".to_string(),
                            retryable: true,
                            retry_after_ms: None,
                        })?
                        .to_string();
                    let entry = function_calls.entry(item_id).or_default();
                    entry.call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| entry.call_id.clone());
                    entry.name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| entry.name.clone());
                    if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                        entry.arguments = arguments.to_string();
                    }
                    Ok(())
                }
                _ => Ok(()),
            }
        }
        "response.output_text.delta" => {
            if let Some(item_id) = event.payload.get("item_id").and_then(Value::as_str) {
                if let Some(delta) = event.payload.get("delta").and_then(Value::as_str) {
                    message_text
                        .entry(item_id.to_string())
                        .or_default()
                        .push_str(delta);
                }
            }
            if let Some(delta) = event.payload.get("delta").and_then(Value::as_str) {
                sink.emit(ModelStreamEvent::TextDelta {
                    text: delta.to_string(),
                })
                .map_err(map_sink_error)?;
            }
            Ok(())
        }
        "response.output_text.done" => {
            if let Some(item_id) = event.payload.get("item_id").and_then(Value::as_str) {
                if let Some(text) = event.payload.get("text").and_then(Value::as_str) {
                    emit_missing_message_text(item_id, text, message_text, sink)?;
                }
            }
            Ok(())
        }
        "response.content_part.added" => {
            if let Some(item_id) = event.payload.get("item_id").and_then(Value::as_str) {
                if let Some(part_text) = event
                    .payload
                    .get("part")
                    .and_then(openai_content_part_text)
                    .filter(|text| !text.is_empty())
                {
                    emit_missing_message_text(item_id, &part_text, message_text, sink)?;
                }
            }
            Ok(())
        }
        "response.content_part.done" => {
            if let Some(item_id) = event.payload.get("item_id").and_then(Value::as_str) {
                if let Some(part_text) = event
                    .payload
                    .get("part")
                    .and_then(openai_content_part_text)
                    .filter(|text| !text.is_empty())
                {
                    emit_missing_message_text(item_id, &part_text, message_text, sink)?;
                }
            }
            Ok(())
        }
        "response.refusal.delta" => {
            if let Some(item_id) = event.payload.get("item_id").and_then(Value::as_str) {
                if let Some(delta) = event.payload.get("delta").and_then(Value::as_str) {
                    message_text
                        .entry(item_id.to_string())
                        .or_default()
                        .push_str(delta);
                }
            }
            if let Some(delta) = event.payload.get("delta").and_then(Value::as_str) {
                sink.emit(ModelStreamEvent::TextDelta {
                    text: delta.to_string(),
                })
                .map_err(map_sink_error)?;
            }
            Ok(())
        }
        "response.refusal.done" => {
            if let Some(item_id) = event.payload.get("item_id").and_then(Value::as_str) {
                if let Some(refusal) = event.payload.get("refusal").and_then(Value::as_str) {
                    emit_missing_message_text(item_id, refusal, message_text, sink)?;
                }
            }
            Ok(())
        }
        "response.function_call_arguments.delta" => {
            let item_id = event
                .payload
                .get("item_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderError {
                    message: "missing Responses function call item_id".to_string(),
                    retryable: true,
                    retry_after_ms: None,
                })?;
            let delta = event
                .payload
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            function_calls
                .entry(item_id.to_string())
                .or_default()
                .arguments
                .push_str(delta);
            Ok(())
        }
        "response.function_call_arguments.done" => {
            let item_id = event
                .payload
                .get("item_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderError {
                    message: "missing Responses function call completion item_id".to_string(),
                    retryable: true,
                    retry_after_ms: None,
                })?;
            let mut partial = function_calls.remove(item_id).unwrap_or_default();
            if let Some(arguments) = event.payload.get("arguments").and_then(Value::as_str) {
                partial.arguments = arguments.to_string();
            }
            let call_id = partial.call_id.ok_or_else(|| ProviderError {
                message: "missing Responses function call call_id".to_string(),
                retryable: true,
                retry_after_ms: None,
            })?;
            if !emitted_tool_calls.insert(call_id.clone()) {
                return Ok(());
            }
            let name = partial.name.ok_or_else(|| ProviderError {
                message: "missing Responses function call name".to_string(),
                retryable: true,
                retry_after_ms: None,
            })?;
            let input = parse_function_arguments(&partial.arguments)?;
            sink.emit(ModelStreamEvent::ToolCall {
                call: kheish_types::ToolCallRecord {
                    id: call_id,
                    name,
                    input,
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                },
            })
            .map_err(map_sink_error)?;
            Ok(())
        }
        "response.output_item.done" => {
            let item = event.payload.get("item").ok_or_else(|| ProviderError {
                message: "missing Responses completed item".to_string(),
                retryable: true,
                retry_after_ms: None,
            })?;
            if item.get("type").and_then(Value::as_str) == Some("message") {
                if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                    let emitted = message_text.remove(item_id).unwrap_or_default();
                    let completed = openai_message_text(item);
                    if !completed.is_empty() {
                        let missing = if emitted.is_empty() {
                            Some(completed)
                        } else {
                            completed
                                .strip_prefix(&emitted)
                                .filter(|value| !value.is_empty())
                                .map(str::to_string)
                        };
                        if let Some(text) = missing {
                            sink.emit(ModelStreamEvent::TextDelta { text })
                                .map_err(map_sink_error)?;
                        }
                    }
                }
                return Ok(());
            }
            if item.get("type").and_then(Value::as_str) != Some("function_call") {
                return Ok(());
            }
            let item_id = item
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderError {
                    message: "missing Responses function call completion id".to_string(),
                    retryable: true,
                    retry_after_ms: None,
                })?;
            let mut partial = function_calls.remove(item_id).unwrap_or_default();
            partial.call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or(partial.call_id);
            partial.name = item
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or(partial.name);
            if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                partial.arguments = arguments.to_string();
            }
            let call_id = partial.call_id.ok_or_else(|| ProviderError {
                message: "missing Responses function call call_id".to_string(),
                retryable: true,
                retry_after_ms: None,
            })?;
            if !emitted_tool_calls.insert(call_id.clone()) {
                return Ok(());
            }
            let name = partial.name.ok_or_else(|| ProviderError {
                message: "missing Responses function call name".to_string(),
                retryable: true,
                retry_after_ms: None,
            })?;
            let input = parse_function_arguments(&partial.arguments)?;
            sink.emit(ModelStreamEvent::ToolCall {
                call: kheish_types::ToolCallRecord {
                    id: call_id,
                    name,
                    input,
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                },
            })
            .map_err(map_sink_error)?;
            Ok(())
        }
        "response.completed" | "response.done" => {
            if let Some(response) = event.payload.get("response") {
                if let Some(usage) = response.get("usage") {
                    sink.emit(ModelStreamEvent::Usage {
                        usage: parse_usage(usage, pricing),
                    })
                    .map_err(map_sink_error)?;
                }
            }
            let reason = if emitted_tool_calls.is_empty() {
                ModelFinishReason::Completed
            } else {
                ModelFinishReason::ToolCalls
            };
            sink.emit(ModelStreamEvent::Stop { reason })
                .map_err(map_sink_error)?;
            Ok(())
        }
        "response.failed" => {
            let (raw_message, error_type, error_code) = openai_error_event_fields(&event);
            Err(ProviderError {
                message: sanitize_upstream_error_message(
                    provider_display_name,
                    "stream error",
                    None,
                    error_type,
                    error_code,
                    raw_message,
                ),
                retryable: false,
                retry_after_ms: None,
            })
        }
        "response.incomplete" => {
            if let Some(response) = event.payload.get("response") {
                if let Some(usage) = response.get("usage") {
                    sink.emit(ModelStreamEvent::Usage {
                        usage: parse_usage(usage, pricing),
                    })
                    .map_err(map_sink_error)?;
                }
            }
            let reason = match event
                .payload
                .get("response")
                .and_then(|response| response.get("incomplete_details"))
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str)
            {
                Some("max_output_tokens") => ModelFinishReason::MaxTokens,
                Some("content_filter") => ModelFinishReason::Blocked,
                _ => ModelFinishReason::Unknown,
            };
            sink.emit(ModelStreamEvent::Stop { reason })
                .map_err(map_sink_error)?;
            Ok(())
        }
        "error" => {
            let (raw_message, error_type, error_code) = openai_error_event_fields(&event);
            Err(ProviderError {
                message: sanitize_upstream_error_message(
                    provider_display_name,
                    "stream error",
                    None,
                    error_type,
                    error_code,
                    raw_message,
                ),
                retryable: false,
                retry_after_ms: None,
            })
        }
        _ => Ok(()),
    }
}

fn emit_missing_message_text(
    item_id: &str,
    text: &str,
    message_text: &mut BTreeMap<String, String>,
    sink: &ModelEventSink,
) -> Result<(), ProviderError> {
    if text.is_empty() {
        return Ok(());
    }
    let emitted = message_text.entry(item_id.to_string()).or_default();
    if emitted == text {
        return Ok(());
    }
    let missing = if emitted.is_empty() {
        text.to_string()
    } else if let Some(suffix) = text.strip_prefix(emitted.as_str()) {
        if suffix.is_empty() {
            return Ok(());
        }
        suffix.to_string()
    } else {
        text.to_string()
    };
    emitted.push_str(&missing);
    sink.emit(ModelStreamEvent::TextDelta { text: missing })
        .map_err(map_sink_error)
}

fn openai_message_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(openai_content_part_text)
        .collect::<Vec<_>>()
        .join("")
}

fn openai_content_part_text(part: &Value) -> Option<String> {
    match part.get("type").and_then(Value::as_str) {
        Some("output_text") | Some("text") => {
            part.get("text").and_then(Value::as_str).map(str::to_string)
        }
        Some("refusal") => part
            .get("refusal")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

fn parse_function_arguments(arguments: &str) -> Result<Value, ProviderError> {
    if arguments.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(arguments).map_err(|error| ProviderError {
        message: format!("failed to parse provider function call arguments: {error}"),
        retryable: true,
        retry_after_ms: None,
    })
}

fn parse_usage(usage: &Value, pricing: Option<&OpenAiPricing>) -> kheish_types::ModelUsage {
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let cost_usd = pricing
        .map(|pricing| {
            (input_tokens as f64 / 1_000_000.0) * pricing.input_per_million_tokens_usd
                + (output_tokens as f64 / 1_000_000.0) * pricing.output_per_million_tokens_usd
        })
        .unwrap_or_default();
    kheish_types::ModelUsage {
        input_tokens,
        output_tokens,
        cost_usd,
    }
}

fn map_sink_error(error: anyhow::Error) -> ProviderError {
    ProviderError {
        message: format!("failed to emit provider stream event: {error}"),
        retryable: false,
        retry_after_ms: None,
    }
}

fn map_transport_error(error: reqwest::Error, provider_display_name: &str) -> ProviderError {
    ProviderError {
        message: format!("{provider_display_name} transport error: {error}"),
        retryable: true,
        retry_after_ms: None,
    }
}

async fn map_http_error(response: reqwest::Response, provider_display_name: &str) -> ProviderError {
    let status = response.status();
    let retry_after_ms = retry_after_header(response.headers().get(RETRY_AFTER));
    let body = response.text().await.unwrap_or_default();
    let payload = serde_json::from_str::<Value>(&body).ok();
    let error = payload.as_ref().and_then(|payload| payload.get("error"));
    let message = sanitize_upstream_error_message(
        provider_display_name,
        "request failed",
        Some(status),
        error
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str),
        error
            .and_then(|value| value.get("code"))
            .and_then(Value::as_str),
        error
            .and_then(|value| value.get("message"))
            .and_then(Value::as_str),
    );
    ProviderError {
        message,
        retryable: is_retryable_status(status),
        retry_after_ms,
    }
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::CONFLICT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    ) || status.is_server_error()
}

fn retry_after_header(header: Option<&HeaderValue>) -> Option<u64> {
    let value = header?.to_str().ok()?;
    let seconds = value.parse::<u64>().ok()?;
    Some(seconds.saturating_mul(1_000))
}

/// One image-generation request executed through the OpenAI Images API.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenAiImageGenerationRequest {
    /// The text prompt used to generate the image.
    pub prompt: String,
    /// The number of images to generate.
    pub count: u32,
    /// Optional size override.
    pub size: Option<String>,
}

/// One source image supplied to the OpenAI Images edit endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenAiImageEditInput {
    /// The file name reported to the provider.
    pub file_name: String,
    /// The normalized MIME type reported to the provider.
    pub media_type: String,
    /// The raw image bytes uploaded to the provider.
    pub bytes: Vec<u8>,
}

/// One image-edit request executed through the OpenAI Images API.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenAiImageEditRequest {
    /// The text instruction used to edit the image.
    pub prompt: String,
    /// The ordered source images uploaded to the provider.
    pub images: Vec<OpenAiImageEditInput>,
    /// The number of edited images to return.
    pub count: u32,
    /// Optional size override.
    pub size: Option<String>,
}

/// One normalized generated image payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenAiGeneratedImage {
    /// The output media type chosen for the generation request.
    pub media_type: String,
    /// The decoded binary image payload.
    pub bytes: Vec<u8>,
    /// Optional revised prompt returned by the provider.
    pub revised_prompt: Option<String>,
}

/// One normalized image-generation response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenAiImageGenerationResponse {
    /// The provider model that produced the images.
    pub model: String,
    /// The generated image payloads.
    pub images: Vec<OpenAiGeneratedImage>,
}

/// One OpenAI text-to-speech request executed through the Audio API.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenAiSpeechRequest {
    /// The text that should be synthesized into audio.
    pub input: String,
    /// Optional style or tone instructions for the generated speech.
    pub instructions: Option<String>,
    /// Optional OpenAI voice identifier.
    pub voice: Option<String>,
    /// Optional response format such as `mp3`, `wav`, or `pcm`.
    pub response_format: Option<String>,
    /// Optional speech speed multiplier.
    pub speed: Option<f32>,
}

/// One normalized OpenAI text-to-speech response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenAiSpeechResponse {
    /// The provider family that generated the audio.
    pub provider: String,
    /// The concrete provider model that generated the audio.
    pub model: String,
    /// The normalized MIME type returned by the provider.
    pub media_type: String,
    /// The generated audio bytes.
    pub bytes: Vec<u8>,
    /// Optional transcript associated with the generated audio.
    pub transcript: Option<String>,
}

/// OpenAI-backed speech synthesizer that shares auth behavior with the text provider.
pub struct OpenAiSpeechSynthesizer {
    provider: OpenAiProvider,
}

impl OpenAiSpeechSynthesizer {
    /// Creates a new speech synthesizer using one OpenAI provider configuration.
    pub fn new(
        mut config: OpenAiProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        config.model = resolve_openai_tts_model(&config.model);
        Ok(Self {
            provider: OpenAiProvider::with_observer(config, observer)?,
        })
    }

    /// Generates one audio asset and returns normalized binary payloads.
    pub async fn synthesize(
        &self,
        request: &OpenAiSpeechRequest,
    ) -> Result<OpenAiSpeechResponse, ProviderError> {
        if request.input.trim().is_empty() {
            return Err(ProviderError {
                message: "OpenAI speech input must not be empty".to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
        let voice = request
            .voice
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_OPENAI_TTS_VOICE)
            .to_string();
        validate_openai_tts_voice(&voice)?;
        let response_format = request
            .response_format
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_OPENAI_TTS_FORMAT)
            .to_ascii_lowercase();
        if !openai_tts_response_format_supported(&response_format) {
            return Err(ProviderError {
                message: format!("unsupported OpenAI speech response format `{response_format}`"),
                retryable: false,
                retry_after_ms: None,
            });
        }
        if let Some(speed) = request.speed
            && (!speed.is_finite() || !(0.25..=4.0).contains(&speed))
        {
            return Err(ProviderError {
                message: "OpenAI speech speed must be between 0.25 and 4.0".to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }

        let mut force_refresh = false;
        let (response, response_target, response_grant_id) = loop {
            let auth_material = self.provider.auth_material(force_refresh).await?;
            let grant_id = auth_material.grant_id.clone();
            let endpoint = openai_audio_speech_endpoint(
                auth_material
                    .base_url_override
                    .as_deref()
                    .unwrap_or(self.provider.config.base_url.as_str()),
            )?;
            let headers = self.provider.headers_from_material(&auth_material)?;
            let model = self.provider.config.model.clone();
            let mut body = json!({
                "model": model.clone(),
                "input": request.input.clone(),
                "voice": voice.clone(),
                "response_format": response_format.clone(),
            });
            if let Some(instructions) = request
                .instructions
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                && let Some(object) = body.as_object_mut()
            {
                object.insert(
                    "instructions".to_string(),
                    Value::String(instructions.to_string()),
                );
            }
            if let Some(speed) = request.speed
                && let Some(object) = body.as_object_mut()
            {
                object.insert(
                    "speed".to_string(),
                    serde_json::Number::from_f64(speed as f64)
                        .map(Value::Number)
                        .unwrap_or(Value::Null),
                );
            }
            let request_summary = json!({
                "model": model,
                "voice": voice.clone(),
                "response_format": response_format.clone(),
                "speed": request.speed,
                "input_chars": request.input.chars().count(),
                "instructions_chars": request
                    .instructions
                    .as_deref()
                    .map(|value| value.chars().count())
                    .unwrap_or(0),
            });
            self.provider
                .ensure_auth_material_active(&auth_material)
                .await?;
            self.provider.record_media_provider_request(
                "openai-audio-speech-provider-request",
                &endpoint,
                &headers,
                &request_summary,
                grant_id.clone(),
            )?;
            let response = match self
                .provider
                .client
                .post(&endpoint)
                .timeout(OPENAI_AUDIO_REQUEST_TIMEOUT)
                .headers(headers)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    let mapped = map_transport_error(error, self.provider.display_name());
                    self.provider.record_provider_failure(
                        self.provider.external_action_target(&endpoint),
                        &mapped.message,
                        grant_id,
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
                    grant_id,
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
            let error = map_http_error(response, self.provider.display_name()).await;
            self.provider.record_provider_failure(
                response_target,
                &error.message,
                response_grant_id,
            )?;
            return Err(error);
        }

        let status = response.status();
        let response_headers = response.headers().clone();
        let media_type = openai_speech_media_type_from_headers(
            &response_headers,
            request
                .response_format
                .as_deref()
                .unwrap_or(DEFAULT_OPENAI_TTS_FORMAT),
        );
        let bytes =
            match read_openai_audio_response_bytes(response, self.provider.display_name()).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.provider.record_provider_failure(
                        response_target,
                        &error.message,
                        response_grant_id,
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
            "openai-audio-speech-provider-response",
            &response_target,
            status.as_u16(),
            &response_headers,
            &response_summary,
            response_grant_id,
        )?;
        Ok(OpenAiSpeechResponse {
            provider: self.provider.provider_name().to_string(),
            model: self.provider.config.model.clone(),
            media_type,
            bytes,
            transcript: None,
        })
    }
}

/// OpenAI-backed image generator that shares auth behavior with the text provider.
pub struct OpenAiImageGenerator {
    provider: OpenAiProvider,
}

impl OpenAiImageGenerator {
    /// Creates a new image generator using one OpenAI provider configuration.
    pub fn new(
        config: OpenAiProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            provider: OpenAiProvider::with_observer(config, observer)?,
        })
    }

    /// Generates one or more images and returns normalized binary payloads.
    pub async fn generate(
        &self,
        request: OpenAiImageGenerationRequest,
    ) -> Result<OpenAiImageGenerationResponse, ProviderError> {
        if request.prompt.trim().is_empty() {
            return Err(ProviderError {
                message: "image generation prompt must not be empty".to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
        if !(1..=10).contains(&request.count) {
            return Err(ProviderError {
                message: "image generation count must be between 1 and 10".to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }

        let mut force_refresh = false;
        let (response, response_target, response_grant_id) = loop {
            let auth_material = self.provider.auth_material(force_refresh).await?;
            let grant_id = auth_material.grant_id.clone();
            let endpoint = image_generation_endpoint(
                auth_material
                    .base_url_override
                    .as_deref()
                    .unwrap_or(self.provider.config.base_url.as_str()),
                self.provider.flavor(),
            )?;
            let headers = self.provider.headers_from_material(&auth_material)?;
            let body = build_image_generation_body(
                &self.provider.config.model,
                &request,
                self.provider.flavor(),
            );
            self.provider
                .ensure_auth_material_active(&auth_material)
                .await?;
            self.provider.record_media_provider_request(
                &format!(
                    "{}-image-generation-provider-request",
                    self.provider.provider_name()
                ),
                &endpoint,
                &headers,
                &body,
                grant_id.clone(),
            )?;
            let response = match self
                .provider
                .client
                .post(&endpoint)
                .headers(headers)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    let mapped = map_transport_error(error, self.provider.display_name());
                    self.provider.record_provider_failure(
                        self.provider.external_action_target(&endpoint),
                        &mapped.message,
                        grant_id,
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
                    grant_id,
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
            let error = map_http_error(response, self.provider.display_name()).await;
            self.provider.record_provider_failure(
                response_target,
                &error.message,
                response_grant_id,
            )?;
            return Err(error);
        }

        let status = response.status();
        let headers = response.headers().clone();
        let payload = match read_openai_image_response_json(
            response,
            "OpenAI image generation response",
            self.provider.display_name(),
        )
        .await
        {
            Ok(payload) => payload,
            Err(error) => {
                self.provider.record_provider_failure(
                    response_target,
                    &error.message,
                    response_grant_id,
                )?;
                return Err(error);
            }
        };
        self.provider.record_media_provider_response(
            &format!(
                "{}-image-generation-provider-response",
                self.provider.provider_name()
            ),
            &response_target,
            status.as_u16(),
            &headers,
            &payload,
            response_grant_id,
        )?;
        let fallback_media_type = match self.provider.flavor() {
            ResponsesProviderFlavor::OpenAi => "image/png",
            ResponsesProviderFlavor::XAi => "image/jpeg",
        };
        let images = payload
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError {
                message: format!(
                    "{} image generation response did not include a data array",
                    self.provider.display_name()
                ),
                retryable: false,
                retry_after_ms: None,
            })?
            .iter()
            .map(|item| decode_generated_image(item, fallback_media_type))
            .collect::<Result<Vec<_>, _>>()?;
        if images.is_empty() {
            return Err(ProviderError {
                message: format!(
                    "{} image generation response did not include any images",
                    self.provider.display_name()
                ),
                retryable: false,
                retry_after_ms: None,
            });
        }
        Ok(OpenAiImageGenerationResponse {
            model: self.provider.config.model.clone(),
            images,
        })
    }
}

/// OpenAI-backed image editor that shares auth behavior with the text provider.
pub struct OpenAiImageEditor {
    provider: OpenAiProvider,
}

impl OpenAiImageEditor {
    /// Creates a new image editor using one OpenAI provider configuration.
    pub fn new(
        config: OpenAiProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            provider: OpenAiProvider::with_observer(config, observer)?,
        })
    }

    /// Edits one or more images and returns normalized binary payloads.
    pub async fn edit(
        &self,
        request: OpenAiImageEditRequest,
    ) -> Result<OpenAiImageGenerationResponse, ProviderError> {
        if request.prompt.trim().is_empty() {
            return Err(ProviderError {
                message: "image edit prompt must not be empty".to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
        if request.images.is_empty() {
            return Err(ProviderError {
                message: "image edit request must include at least one source image".to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
        if !(1..=10).contains(&request.count) {
            return Err(ProviderError {
                message: "image edit count must be between 1 and 10".to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }

        let mut force_refresh = false;
        let (response, response_target, response_grant_id) = loop {
            let auth_material = self.provider.auth_material(force_refresh).await?;
            let grant_id = auth_material.grant_id.clone();
            let endpoint = image_edit_endpoint(
                auth_material
                    .base_url_override
                    .as_deref()
                    .unwrap_or(self.provider.config.base_url.as_str()),
                self.provider.flavor(),
            )?;
            let headers = self.provider.headers_from_material(&auth_material)?;
            let xai_body = if matches!(self.provider.flavor(), ResponsesProviderFlavor::XAi) {
                Some(build_xai_image_edit_body(
                    &self.provider.config.model,
                    &request,
                ))
            } else {
                None
            };
            let form = if xai_body.is_none() {
                Some(build_image_edit_form(
                    &self.provider.config.model,
                    &request,
                    self.provider.flavor(),
                )?)
            } else {
                None
            };
            let request_digest = json!({
                "count": request.count,
                "prompt": request.prompt.clone(),
                "image_count": request.images.len(),
                "size": request.size.clone(),
                "model": self.provider.config.model.clone(),
            });
            self.provider
                .ensure_auth_material_active(&auth_material)
                .await?;
            self.provider.record_media_provider_request(
                &format!(
                    "{}-image-edit-provider-request",
                    self.provider.provider_name()
                ),
                &endpoint,
                &headers,
                xai_body.as_ref().unwrap_or(&request_digest),
                grant_id.clone(),
            )?;
            let mut request_builder = self.provider.client.post(&endpoint).headers(headers);
            request_builder = if let Some(body) = &xai_body {
                request_builder.json(body)
            } else {
                request_builder.multipart(form.expect("multipart form is present for OpenAI"))
            };
            let response = match request_builder.send().await {
                Ok(response) => response,
                Err(error) => {
                    let mapped = map_transport_error(error, self.provider.display_name());
                    self.provider.record_provider_failure(
                        self.provider.external_action_target(&endpoint),
                        &mapped.message,
                        grant_id,
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
                    grant_id,
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
            let error = map_http_error(response, self.provider.display_name()).await;
            self.provider.record_provider_failure(
                response_target,
                &error.message,
                response_grant_id,
            )?;
            return Err(error);
        }

        let status = response.status();
        let headers = response.headers().clone();
        let payload = match read_openai_image_response_json(
            response,
            "OpenAI image edit response",
            self.provider.display_name(),
        )
        .await
        {
            Ok(payload) => payload,
            Err(error) => {
                self.provider.record_provider_failure(
                    response_target,
                    &error.message,
                    response_grant_id,
                )?;
                return Err(error);
            }
        };
        self.provider.record_media_provider_response(
            &format!(
                "{}-image-edit-provider-response",
                self.provider.provider_name()
            ),
            &response_target,
            status.as_u16(),
            &headers,
            &payload,
            response_grant_id,
        )?;
        let images = payload
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError {
                message: format!(
                    "{} image edit response did not include a data array",
                    self.provider.display_name()
                ),
                retryable: false,
                retry_after_ms: None,
            })?
            .iter()
            .map(|item| {
                let fallback_media_type = match self.provider.flavor() {
                    ResponsesProviderFlavor::OpenAi => "image/png",
                    ResponsesProviderFlavor::XAi => "image/jpeg",
                };
                decode_generated_image(item, fallback_media_type)
            })
            .collect::<Result<Vec<_>, _>>()?;
        if images.is_empty() {
            return Err(ProviderError {
                message: format!(
                    "{} image edit response did not include any images",
                    self.provider.display_name()
                ),
                retryable: false,
                retry_after_ms: None,
            });
        }
        Ok(OpenAiImageGenerationResponse {
            model: self.provider.config.model.clone(),
            images,
        })
    }
}

fn image_generation_endpoint(
    base_url: &str,
    flavor: ResponsesProviderFlavor,
) -> Result<String, ProviderError> {
    if base_url.contains(CODEX_RESPONSES_PATH_MARKER) {
        return Err(ProviderError {
            message: format!(
                "{} Codex account endpoints do not support image generation",
                flavor.display_name()
            ),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if let Some(prefix) = base_url.strip_suffix("/responses") {
        return Ok(format!("{prefix}/images/generations"));
    }
    if base_url.ends_with("/images/generations") {
        return Ok(base_url.to_string());
    }
    if base_url.ends_with("/v1") {
        return Ok(format!("{base_url}/images/generations"));
    }
    Err(ProviderError {
        message: format!(
            "unsupported {} base URL for image generation: {base_url}",
            flavor.display_name()
        ),
        retryable: false,
        retry_after_ms: None,
    })
}

fn image_edit_endpoint(
    base_url: &str,
    flavor: ResponsesProviderFlavor,
) -> Result<String, ProviderError> {
    if base_url.contains(CODEX_RESPONSES_PATH_MARKER) {
        return Err(ProviderError {
            message: format!(
                "{} Codex account endpoints do not support image editing",
                flavor.display_name()
            ),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if let Some(prefix) = base_url.strip_suffix("/responses") {
        return Ok(format!("{prefix}/images/edits"));
    }
    if base_url.ends_with("/images/edits") {
        return Ok(base_url.to_string());
    }
    if base_url.ends_with("/v1") {
        return Ok(format!("{base_url}/images/edits"));
    }
    Err(ProviderError {
        message: format!(
            "unsupported {} base URL for image editing: {base_url}",
            flavor.display_name()
        ),
        retryable: false,
        retry_after_ms: None,
    })
}

fn openai_audio_speech_endpoint(base_url: &str) -> Result<String, ProviderError> {
    if base_url.contains(CODEX_RESPONSES_PATH_MARKER) {
        return Err(ProviderError {
            message: "OpenAI Codex account endpoints do not support speech synthesis".to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/audio/speech") {
        return Ok(trimmed.to_string());
    }
    if let Some(prefix) = trimmed.strip_suffix("/responses") {
        return Ok(format!("{prefix}/audio/speech"));
    }
    if trimmed.ends_with("/v1") {
        return Ok(format!("{trimmed}/audio/speech"));
    }
    Err(ProviderError {
        message: format!("unsupported OpenAI base URL for speech synthesis: {base_url}"),
        retryable: false,
        retry_after_ms: None,
    })
}

fn openai_tts_response_format_supported(format: &str) -> bool {
    matches!(
        format.trim().to_ascii_lowercase().as_str(),
        "mp3" | "opus" | "aac" | "flac" | "wav" | "pcm"
    )
}

fn validate_openai_tts_voice(voice: &str) -> Result<(), ProviderError> {
    let normalized = voice.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(ProviderError {
            message: "OpenAI speech voice must not be empty".to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if openai_tts_voice_supported(&normalized) {
        return Ok(());
    }
    Err(ProviderError {
        message: format!("unsupported OpenAI speech voice `{voice}`"),
        retryable: false,
        retry_after_ms: None,
    })
}

fn openai_tts_voice_supported(voice: &str) -> bool {
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

async fn read_openai_audio_response_bytes(
    response: reqwest::Response,
    provider_name: &str,
) -> Result<Vec<u8>, ProviderError> {
    if let Some(content_length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        && content_length > MAX_OPENAI_AUDIO_RESPONSE_BYTES
    {
        return Err(ProviderError {
            message: format!(
                "OpenAI speech response exceeds the {} byte limit",
                MAX_OPENAI_AUDIO_RESPONSE_BYTES
            ),
            retryable: false,
            retry_after_ms: None,
        });
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| map_transport_error(error, provider_name))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_OPENAI_AUDIO_RESPONSE_BYTES {
            return Err(ProviderError {
                message: format!(
                    "OpenAI speech response exceeds the {} byte limit",
                    MAX_OPENAI_AUDIO_RESPONSE_BYTES
                ),
                retryable: false,
                retry_after_ms: None,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn read_openai_image_response_json(
    response: reqwest::Response,
    label: &str,
    provider_name: &str,
) -> Result<Value, ProviderError> {
    if let Some(content_length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        && content_length > MAX_OPENAI_IMAGE_RESPONSE_BYTES
    {
        return Err(ProviderError {
            message: format!("{label} exceeds the {MAX_OPENAI_IMAGE_RESPONSE_BYTES} byte limit"),
            retryable: false,
            retry_after_ms: None,
        });
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| map_transport_error(error, provider_name))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_OPENAI_IMAGE_RESPONSE_BYTES {
            return Err(ProviderError {
                message: format!(
                    "{label} exceeds the {MAX_OPENAI_IMAGE_RESPONSE_BYTES} byte limit"
                ),
                retryable: false,
                retry_after_ms: None,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice::<Value>(&bytes).map_err(|error| ProviderError {
        message: format!("failed to decode {label}: {error}"),
        retryable: false,
        retry_after_ms: None,
    })
}

fn openai_speech_media_type_from_headers(headers: &HeaderMap, response_format: &str) -> String {
    let header_media_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or(value)
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|value| value.starts_with("audio/"));
    header_media_type.unwrap_or_else(|| openai_speech_media_type(response_format).to_string())
}

fn openai_speech_media_type(response_format: &str) -> &'static str {
    match response_format.trim().to_ascii_lowercase().as_str() {
        "opus" => "audio/opus",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "pcm" => "audio/l16",
        _ => "audio/mpeg",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ImageEditMultipartField {
    Text {
        name: String,
        value: String,
    },
    Binary {
        name: String,
        file_name: String,
        media_type: String,
        bytes: Vec<u8>,
    },
}

fn build_image_generation_body(
    model: &str,
    request: &OpenAiImageGenerationRequest,
    flavor: ResponsesProviderFlavor,
) -> Value {
    let mut body = json!({
        "model": model,
        "prompt": request.prompt,
        "n": request.count,
    });
    let Value::Object(map) = &mut body else {
        unreachable!("OpenAI image generation body must stay an object");
    };
    match flavor {
        ResponsesProviderFlavor::OpenAi => {
            if let Some(size) = request.size.as_ref().filter(|value| !value.is_empty()) {
                map.insert("size".to_string(), Value::String(size.clone()));
            }
        }
        ResponsesProviderFlavor::XAi => {
            map.insert(
                "response_format".to_string(),
                Value::String("b64_json".to_string()),
            );
            if let Some((aspect_ratio, resolution)) =
                xai_image_generation_dimensions(request.size.as_deref())
            {
                map.insert(
                    "aspect_ratio".to_string(),
                    Value::String(aspect_ratio.to_string()),
                );
                map.insert(
                    "resolution".to_string(),
                    Value::String(resolution.to_string()),
                );
            }
        }
    }
    body
}

fn build_image_edit_form(
    model: &str,
    request: &OpenAiImageEditRequest,
    flavor: ResponsesProviderFlavor,
) -> Result<Form, ProviderError> {
    let fields = image_edit_form_fields(model, request, flavor)?;
    let mut form = Form::new();
    for field in fields {
        match field {
            ImageEditMultipartField::Text { name, value } => {
                form = form.text(name, value);
            }
            ImageEditMultipartField::Binary {
                name,
                file_name,
                media_type,
                bytes,
            } => {
                let part = Part::bytes(bytes)
                    .file_name(file_name)
                    .mime_str(&media_type)
                    .map_err(|error| ProviderError {
                        message: format!("unsupported image edit media type {media_type}: {error}"),
                        retryable: false,
                        retry_after_ms: None,
                    })?;
                form = form.part(name, part);
            }
        }
    }
    Ok(form)
}

fn image_edit_form_fields(
    model: &str,
    request: &OpenAiImageEditRequest,
    flavor: ResponsesProviderFlavor,
) -> Result<Vec<ImageEditMultipartField>, ProviderError> {
    if !matches!(flavor, ResponsesProviderFlavor::OpenAi) {
        return Err(ProviderError {
            message: format!(
                "{} image editing is not supported by this backend",
                flavor.display_name()
            ),
            retryable: false,
            retry_after_ms: None,
        });
    }
    let mut fields = vec![
        ImageEditMultipartField::Text {
            name: "model".to_string(),
            value: model.to_string(),
        },
        ImageEditMultipartField::Text {
            name: "prompt".to_string(),
            value: request.prompt.clone(),
        },
        ImageEditMultipartField::Text {
            name: "n".to_string(),
            value: request.count.to_string(),
        },
    ];
    if let Some(size) = request.size.as_ref().filter(|value| !value.is_empty()) {
        fields.push(ImageEditMultipartField::Text {
            name: "size".to_string(),
            value: size.clone(),
        });
    }
    for image in &request.images {
        fields.push(ImageEditMultipartField::Binary {
            name: "image[]".to_string(),
            file_name: image.file_name.clone(),
            media_type: image.media_type.clone(),
            bytes: image.bytes.clone(),
        });
    }
    Ok(fields)
}

fn build_xai_image_edit_body(model: &str, request: &OpenAiImageEditRequest) -> Value {
    let mut body = json!({
        "model": model,
        "prompt": request.prompt,
        "n": request.count,
        "response_format": "b64_json",
    });
    let Value::Object(map) = &mut body else {
        unreachable!("xAI image edit body must stay an object");
    };
    if let Some((aspect_ratio, resolution)) =
        xai_image_generation_dimensions(request.size.as_deref())
    {
        map.insert(
            "aspect_ratio".to_string(),
            Value::String(aspect_ratio.to_string()),
        );
        map.insert(
            "resolution".to_string(),
            Value::String(resolution.to_string()),
        );
    }
    let images = request
        .images
        .iter()
        .map(|image| {
            json!({
                "url": format!(
                    "data:{};base64,{}",
                    image.media_type,
                    BASE64_STANDARD.encode(&image.bytes)
                ),
                "type": "image_url",
            })
        })
        .collect::<Vec<_>>();
    if images.len() == 1 {
        map.insert("image".to_string(), images.into_iter().next().unwrap());
    } else {
        map.insert("images".to_string(), Value::Array(images));
    }
    body
}

fn xai_image_generation_dimensions(size: Option<&str>) -> Option<(&'static str, &'static str)> {
    let normalized = size.unwrap_or("1024x1024").trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1024x1024" | "square" => Some(("1:1", "1k")),
        "1536x1024" => Some(("3:2", "2k")),
        "1024x1536" => Some(("2:3", "2k")),
        "1792x1024" => Some(("16:9", "2k")),
        "1024x1792" => Some(("9:16", "2k")),
        _ => None,
    }
}

/// Normalizes one OpenAI image model name to an image-capable route.
pub fn resolve_openai_image_model(model: &str) -> String {
    let normalized = model.to_ascii_lowercase();
    if normalized.starts_with("gpt-image-") || normalized.starts_with("dall-e-") {
        return model.to_string();
    }
    "gpt-image-1.5".to_string()
}

/// Normalizes one OpenAI text-to-speech model name to an audio-capable route.
pub fn resolve_openai_tts_model(model: &str) -> String {
    let trimmed = model.trim();
    let normalized = trimmed.to_ascii_lowercase();
    if !trimmed.is_empty() && (normalized.starts_with("tts-") || normalized.contains("-tts")) {
        return trimmed.to_string();
    }
    DEFAULT_OPENAI_TTS_MODEL.to_string()
}

fn decode_generated_image(
    item: &Value,
    fallback_media_type: &str,
) -> Result<OpenAiGeneratedImage, ProviderError> {
    let encoded = item
        .get("b64_json")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError {
            message: "OpenAI image generation item did not include b64_json".to_string(),
            retryable: false,
            retry_after_ms: None,
        })?;
    let bytes = BASE64_STANDARD
        .decode(encoded)
        .map_err(|error| ProviderError {
            message: format!("failed to decode generated image payload: {error}"),
            retryable: false,
            retry_after_ms: None,
        })?;
    Ok(OpenAiGeneratedImage {
        media_type: sniff_generated_image_media_type(&bytes)
            .unwrap_or(fallback_media_type)
            .to_string(),
        bytes,
        revised_prompt: item
            .get("revised_prompt")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn sniff_generated_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    const JPEG_SIGNATURE: &[u8] = b"\xff\xd8\xff";
    if bytes.starts_with(PNG_SIGNATURE) {
        return Some("image/png");
    }
    if bytes.starts_with(JPEG_SIGNATURE) {
        return Some("image/jpeg");
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use anyhow::Result;
    use async_trait::async_trait;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    use super::{
        AttachmentRenderCache, DEFAULT_OPENAI_TTS_MODEL, ImageEditMultipartField,
        MAX_OPENAI_AUDIO_RESPONSE_BYTES, OpenAiImageEditInput, OpenAiImageEditRequest,
        OpenAiImageEditor, OpenAiImageGenerationRequest, OpenAiImageGenerator, OpenAiPricing,
        OpenAiProvider, OpenAiProviderConfig, OpenAiSpeechRequest, OpenAiSpeechSynthesizer,
        ResponsesProviderFlavor, build_image_generation_body, build_xai_image_edit_body,
        image_edit_endpoint, image_edit_form_fields, openai_audio_speech_endpoint,
        openai_conversation_delta,
    };
    use crate::model::{
        ModelBudget, ModelEventSink, ModelGenerationConfig, ModelProvider, ModelRetryPolicy,
        ModelRuntime, ModelRuntimeRequest, ModelStreamEvent, ReasoningConfig, ReasoningEffort,
        ReasoningSummary, ResponseFormat, StructuredFieldSchema, StructuredValueKind, ToolChoice,
    };
    use crate::observability::{
        DebugArtifact, InMemoryObserver, RuntimeObserver, TraceEvent, TraceEventKind,
    };
    use crate::providers::prompt::{NormalizedConversationItem, NormalizedProviderPrompt};
    use crate::providers::test_fixtures::{
        create_fixture_dir, write_document_attachment, write_document_attachment_with_preview,
        write_image_attachment,
    };
    use crate::providers::testsupport::{spawn_chunked_mock_server, spawn_mock_server};
    use crate::{DebugCaptureLevel, NoopObserver};
    use kheish_auth::{RequestAuthProvider, ResolvedAuthMaterial};
    use kheish_core::{
        ModelDriver, ModelRequestKind, build_compaction_system_prompt, build_compaction_user_prompt,
    };
    use kheish_types::model_max_output_tokens;
    use kheish_types::{
        CAPPED_DEFAULT_MAX_OUTPUT_TOKENS, ConversationKey, InputContentPart, PromptProjection,
        ProviderInputItem, ProviderPrompt, Role, SummaryBlock, ToolCallRecord, ToolDefinition,
        ToolResultRecord,
    };

    async fn spawn_mock_server_with_declared_content_length(
        headers: &[(&str, &str)],
        declared_content_length: usize,
    ) -> Result<String> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let headers = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect::<String>();
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
                "HTTP/1.1 200 OK\r\nContent-Length: {declared_content_length}\r\n{headers}\r\nx"
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
            self.artifacts
                .lock()
                .expect("artifacts mutex poisoned")
                .clone()
        }
    }

    impl RuntimeObserver for FixedDebugObserver {
        fn debug_level(&self) -> DebugCaptureLevel {
            self.level
        }

        fn record(&self, _event: TraceEvent) {}

        fn record_debug_artifact(&self, artifact: DebugArtifact) {
            self.artifacts
                .lock()
                .expect("artifacts mutex poisoned")
                .push(artifact);
        }

        fn increment_counter(&self, _name: &str, _delta: u64) {}
    }

    struct FailingAuditObserver {
        traces: Mutex<Vec<TraceEvent>>,
    }

    impl FailingAuditObserver {
        fn shared() -> Arc<Self> {
            Arc::new(Self {
                traces: Mutex::new(Vec::new()),
            })
        }
    }

    impl RuntimeObserver for FailingAuditObserver {
        fn record(&self, event: TraceEvent) {
            self.traces
                .lock()
                .expect("traces mutex poisoned")
                .push(event);
        }

        fn external_action_audit_failure(&self) -> Option<String> {
            Some("injected audit failure".to_string())
        }

        fn increment_counter(&self, _name: &str, _delta: u64) {}
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

    fn default_model_request(session_id: &str) -> kheish_core::ModelRequest {
        kheish_core::ModelRequest {
            kind: ModelRequestKind::MainLoop,
            conversation: ConversationKey {
                session_id: session_id.to_string(),
                thread_id: None,
            },
            turn: 1,
            prompt: PromptProjection::default(),
            provider_prompt: ProviderPrompt::default(),
            available_tools: Vec::new(),
            generation: ModelGenerationConfig::default(),
        }
    }

    #[test]
    fn openai_provider_config_uses_shared_default_max_tokens() {
        let config = OpenAiProviderConfig::new("gpt-5-mini", "test-key");
        assert_eq!(
            config.default_max_output_tokens,
            CAPPED_DEFAULT_MAX_OUTPUT_TOKENS
        );
    }

    #[tokio::test]
    async fn openai_provider_encodes_prompt_tools_and_system_text() -> Result<()> {
        let captured = Arc::new(Mutex::new(String::new()));
        let response = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg-1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg-1\",\"output_index\":0,\"content_index\":0,\"delta\":\"done\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":4}}}\n\n"
        );
        let url = spawn_mock_server(
            200,
            &[("content-type", "text/event-stream")],
            response,
            captured.clone(),
        )
        .await?;
        let provider = OpenAiProvider::new(OpenAiProviderConfig {
            base_url: url,
            ..OpenAiProviderConfig::new("gpt-test", "test-key")
        })?;

        let (sender, mut receiver) = mpsc::unbounded_channel();
        provider
            .stream(
                ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-1".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt {
                        instructions: Vec::new(),
                        force_synthetic_user_prefix: false,
                        items: vec![
                            ProviderInputItem::Summary {
                                summary: SummaryBlock {
                                    title: "resume".to_string(),
                                    content: "Earlier context".to_string(),
                                },
                            },
                            ProviderInputItem::Message {
                                id: "user-1".to_string(),
                                role: kheish_types::Role::User,
                                content: "Please use the tool".to_string(),
                                content_parts: Vec::new(),
                                attachments: Vec::new(),
                                provider_response_id: None,
                                provider_context: None,
                            },
                            ProviderInputItem::Message {
                                id: "assistant-1".to_string(),
                                role: kheish_types::Role::Assistant,
                                content: "I will use a tool.".to_string(),
                                content_parts: Vec::new(),
                                attachments: Vec::new(),
                                provider_response_id: Some("resp_123".to_string()),
                                provider_context: None,
                            },
                            ProviderInputItem::ToolCall {
                                assistant_message_id: Some("assistant-1".to_string()),
                                call: ToolCallRecord {
                                    id: "call-1".to_string(),
                                    name: "echo".to_string(),
                                    input: json!({"text": "ping"}),
                                    assistant_message_id: None,
                                    assistant_provider_response_id: Some("resp_123".to_string()),
                                },
                            },
                            ProviderInputItem::ToolResult {
                                result: ToolResultRecord {
                                    call_id: "call-1".to_string(),
                                    output: json!({"echo": "ping"}),
                                    is_error: false,
                                    tool_name: Some("echo".to_string()),
                                    offset: None,
                                    timestamp_ms: None,
                                    context_updates: Vec::new(),
                                    hook_contexts: Vec::new(),
                                },
                            },
                            ProviderInputItem::Message {
                                id: "user-2".to_string(),
                                role: kheish_types::Role::User,
                                content: "Continue.".to_string(),
                                content_parts: Vec::new(),
                                attachments: Vec::new(),
                                provider_response_id: None,
                                provider_context: None,
                            },
                        ],
                    },
                    available_tools: vec![ToolDefinition {
                        name: "echo".to_string(),
                        description: "Echoes input".to_string(),
                        input_schema: json!({
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"],
                            "additionalProperties": false
                        }),
                        allows_parallel: true,
                    }],
                    generation: ModelGenerationConfig {
                        model: None,
                        fallback_model: None,
                        tool_choice: ToolChoice::Specific {
                            name: "echo".to_string(),
                        },
                        allow_parallel_tool_calls: false,
                        max_output_tokens: Some(256),
                        temperature: Some(0.2),
                        reasoning: None,
                        response_format: ResponseFormat::StructuredJson {
                            schema: StructuredFieldSchema {
                                kind: StructuredValueKind::Object,
                                fields: Default::default(),
                                optional_fields: Default::default(),
                                items: None,
                            },
                        },
                    },
                },
                ModelEventSink::new(sender),
            )
            .await?;

        let payload: Value =
            serde_json::from_str(&captured.lock().expect("payload mutex poisoned"))?;
        assert_eq!(payload["model"], "gpt-test");
        assert_eq!(payload["store"], true);
        assert_eq!(payload["max_output_tokens"], 256);
        assert_eq!(payload["tool_choice"]["type"], "function");
        assert_eq!(payload["tool_choice"]["name"], "echo");
        assert_eq!(payload["previous_response_id"], "resp_123");
        assert_eq!(payload["tools"][0]["type"], "function");
        assert_eq!(payload["text"]["format"]["type"], "json_schema");
        let input = payload["input"]
            .as_array()
            .expect("input should be serialized");
        let serialized_input = serde_json::to_string(input)?;
        assert!(!serialized_input.contains("Earlier context"));
        assert!(!input.iter().any(|item| item["type"] == "function_call"));
        assert!(
            input
                .iter()
                .any(|item| item["type"] == "function_call_output")
        );
        assert!(input.iter().any(|item| {
            item["type"] == "function_call_output" && item["output"] == json!("{\"echo\":\"ping\"}")
        }));
        assert!(
            input
                .iter()
                .any(|item| item["type"] == "message" && item["role"] == "user")
        );

        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        assert!(
            events.iter().any(
                |event| matches!(event, ModelStreamEvent::TextDelta { text } if text == "done")
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn openai_provider_does_not_send_when_external_audit_is_unavailable() -> Result<()> {
        let captured = Arc::new(Mutex::new(String::new()));
        let url = spawn_mock_server(
            200,
            &[("content-type", "text/event-stream")],
            "",
            captured.clone(),
        )
        .await?;
        let provider = OpenAiProvider::with_observer(
            OpenAiProviderConfig {
                base_url: url,
                ..OpenAiProviderConfig::new("gpt-test", "test-key")
            },
            FailingAuditObserver::shared(),
        )?;

        let (sender, _receiver) = mpsc::unbounded_channel();
        let error = provider
            .stream(
                ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-audit".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                ModelEventSink::new(sender),
            )
            .await
            .expect_err("provider must fail before sending without audit");

        assert!(error.message.contains("external action audit failed"));
        assert!(
            captured.lock().expect("captured mutex poisoned").is_empty(),
            "provider should not send an HTTP request after audit failure"
        );
        Ok(())
    }

    #[test]
    fn openai_compaction_request_body_omits_previous_response_id_and_keeps_full_prompt() {
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-test", "test-key"))
            .expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: ModelRequestKind::Compaction,
                    session_id: "session-compaction".to_string(),
                    thread_id: None,
                    turn: 4,
                    prompt: ProviderPrompt {
                        instructions: vec![build_compaction_system_prompt()],
                        force_synthetic_user_prefix: false,
                        items: vec![
                            ProviderInputItem::Summary {
                                summary: SummaryBlock {
                                    title: "resume".to_string(),
                                    content: "Earlier compacted context.".to_string(),
                                },
                            },
                            ProviderInputItem::Message {
                                id: "user-1".to_string(),
                                role: Role::User,
                                content: "First preserved user message.".to_string(),
                                content_parts: Vec::new(),
                                attachments: Vec::new(),
                                provider_response_id: None,
                                provider_context: None,
                            },
                            ProviderInputItem::Message {
                                id: "assistant-1".to_string(),
                                role: Role::Assistant,
                                content: "Previously resumed assistant context.".to_string(),
                                content_parts: Vec::new(),
                                attachments: Vec::new(),
                                provider_response_id: Some("resp_123".to_string()),
                                provider_context: None,
                            },
                            ProviderInputItem::Message {
                                id: "compaction-request-4".to_string(),
                                role: Role::User,
                                content: build_compaction_user_prompt(true),
                                content_parts: Vec::new(),
                                attachments: Vec::new(),
                                provider_response_id: None,
                                provider_context: None,
                            },
                        ],
                    },
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                false,
            )
            .expect("request body should build");

        assert!(body.get("previous_response_id").is_none());
        let input = body["input"]
            .as_array()
            .expect("input should be serialized");
        let serialized_input =
            serde_json::to_string(input).expect("input should be serializable to JSON");
        assert!(serialized_input.contains("Earlier compacted context."));
        assert!(serialized_input.contains("Previously resumed assistant context."));
        assert!(serialized_input.contains("recent portion of the conversation"));
    }

    #[test]
    fn openai_request_body_includes_image_attachments() -> Result<()> {
        let temp = create_fixture_dir("openai-images")?;
        let png = write_image_attachment(&temp, "sample-a.png", "image/png")?;
        let jpeg = write_image_attachment(&temp, "sample-b.jpg", "image/jpeg")?;
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-5.4", "test-key"))?;

        let body = provider.build_request_body(
            &ModelRuntimeRequest {
                attempt: 1,
                kind: kheish_core::ModelRequestKind::MainLoop,
                session_id: "session-images".to_string(),
                thread_id: None,
                turn: 1,
                prompt: ProviderPrompt {
                    instructions: Vec::new(),
                    force_synthetic_user_prefix: false,
                    items: vec![ProviderInputItem::Message {
                        id: "user-1".to_string(),
                        role: kheish_types::Role::User,
                        content: "Inspect these attachments.".to_string(),
                        content_parts: Vec::new(),
                        attachments: vec![png, jpeg],
                        provider_response_id: None,
                        provider_context: None,
                    }],
                },
                available_tools: Vec::new(),
                generation: ModelGenerationConfig::default(),
            },
            false,
        )?;

        let input = body["input"]
            .as_array()
            .expect("input should be serialized");
        let user_message = input
            .iter()
            .find(|item| item["role"] == "user")
            .expect("user message should be present");
        let content = user_message["content"]
            .as_array()
            .expect("content should be an array");
        assert!(content.iter().any(
            |part| part["type"] == "input_text" && part["text"] == "Inspect these attachments."
        ));
        let asset_hints = content
            .iter()
            .filter(|part| {
                part["type"] == "input_text"
                    && part["text"]
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
        let image_urls = content
            .iter()
            .filter(|part| part["type"] == "input_image")
            .filter_map(|part| part["image_url"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(image_urls.len(), 2);
        assert!(
            image_urls
                .iter()
                .any(|url| url.starts_with("data:image/png;base64,"))
        );
        assert!(
            image_urls
                .iter()
                .any(|url| url.starts_with("data:image/jpeg;base64,"))
        );
        fs::metadata(temp.join("sample-a.png"))?;
        Ok(())
    }

    #[test]
    fn openai_conversation_delta_ignores_non_openai_resume_ids() -> Result<()> {
        let prompt = NormalizedProviderPrompt {
            instructions: Vec::new(),
            conversation: vec![
                NormalizedConversationItem::UserMessage {
                    id: "user-1".to_string(),
                    content: "Earlier document".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                },
                NormalizedConversationItem::AssistantMessage {
                    id: "assistant-1".to_string(),
                    content: "Noted".to_string(),
                    provider_response_id: Some("msg_123".to_string()),
                    provider_context: None,
                },
                NormalizedConversationItem::UserMessage {
                    id: "user-2".to_string(),
                    content: "Follow up".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                },
            ],
        };

        let (previous_response_id, items) = openai_conversation_delta(
            &prompt,
            ModelRequestKind::MainLoop,
            "gpt-test",
            None,
            &AttachmentRenderCache::default(),
            ResponsesProviderFlavor::OpenAi,
            false,
        )?;
        assert!(previous_response_id.is_none());
        assert_eq!(items.len(), 3);
        Ok(())
    }

    #[test]
    fn openai_conversation_delta_falls_back_for_stale_tool_outputs() -> Result<()> {
        let prompt = NormalizedProviderPrompt {
            instructions: Vec::new(),
            conversation: vec![
                NormalizedConversationItem::UserMessage {
                    id: "user-1".to_string(),
                    content: "Start.".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                },
                NormalizedConversationItem::AssistantToolCalls {
                    assistant_message_id: Some("assistant-old".to_string()),
                    assistant_provider_response_id: Some("resp_old".to_string()),
                    calls: vec![ToolCallRecord {
                        id: "call-old".to_string(),
                        name: "bash".to_string(),
                        input: json!({"cmd": "echo old"}),
                        assistant_message_id: Some("assistant-old".to_string()),
                        assistant_provider_response_id: Some("resp_old".to_string()),
                    }],
                },
                NormalizedConversationItem::UserMessage {
                    id: "user-2".to_string(),
                    content: "Continue.".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                },
                NormalizedConversationItem::AssistantToolCalls {
                    assistant_message_id: Some("assistant-current".to_string()),
                    assistant_provider_response_id: Some("resp_current".to_string()),
                    calls: vec![ToolCallRecord {
                        id: "call-current".to_string(),
                        name: "bash".to_string(),
                        input: json!({"cmd": "echo current"}),
                        assistant_message_id: Some("assistant-current".to_string()),
                        assistant_provider_response_id: Some("resp_current".to_string()),
                    }],
                },
                NormalizedConversationItem::UserMessage {
                    id: "user-3".to_string(),
                    content: "One more continuation.".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                },
                NormalizedConversationItem::ToolResults {
                    results: vec![
                        ToolResultRecord {
                            call_id: "call-old".to_string(),
                            output: json!({"error": "missing old result"}),
                            is_error: true,
                            tool_name: Some("bash".to_string()),
                            offset: None,
                            timestamp_ms: None,
                            context_updates: Vec::new(),
                            hook_contexts: Vec::new(),
                        },
                        ToolResultRecord {
                            call_id: "call-current".to_string(),
                            output: json!({"error": "missing current result"}),
                            is_error: true,
                            tool_name: Some("bash".to_string()),
                            offset: None,
                            timestamp_ms: None,
                            context_updates: Vec::new(),
                            hook_contexts: Vec::new(),
                        },
                    ],
                },
            ],
        };

        let (previous_response_id, items) = openai_conversation_delta(
            &prompt,
            ModelRequestKind::MainLoop,
            "gpt-test",
            None,
            &AttachmentRenderCache::default(),
            ResponsesProviderFlavor::OpenAi,
            false,
        )?;

        assert!(previous_response_id.is_none());
        assert!(items.iter().any(|item| item["type"] == "function_call"));
        assert!(
            items
                .iter()
                .any(|item| item["type"] == "function_call_output")
        );
        Ok(())
    }

    #[test]
    fn openai_request_body_excludes_stale_missing_tool_result_from_resume_delta() -> Result<()> {
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-5.4", "test-key"))?;
        let body = provider.build_request_body(
            &ModelRuntimeRequest {
                attempt: 1,
                kind: ModelRequestKind::MainLoop,
                session_id: "session-stale-tool".to_string(),
                thread_id: None,
                turn: 1,
                prompt: ProviderPrompt {
                    instructions: Vec::new(),
                    force_synthetic_user_prefix: false,
                    items: vec![
                        ProviderInputItem::Message {
                            id: "assistant-old".to_string(),
                            role: Role::Assistant,
                            content: "I will call a tool.".to_string(),
                            content_parts: Vec::new(),
                            attachments: Vec::new(),
                            provider_response_id: Some("resp_old".to_string()),
                            provider_context: None,
                        },
                        ProviderInputItem::ToolCall {
                            assistant_message_id: Some("assistant-old".to_string()),
                            call: ToolCallRecord {
                                id: "call-old".to_string(),
                                name: "bash".to_string(),
                                input: json!({"cmd": "echo old"}),
                                assistant_message_id: Some("assistant-old".to_string()),
                                assistant_provider_response_id: Some("resp_old".to_string()),
                            },
                        },
                        ProviderInputItem::Message {
                            id: "user-middle".to_string(),
                            role: Role::User,
                            content: "Continue.".to_string(),
                            content_parts: Vec::new(),
                            attachments: Vec::new(),
                            provider_response_id: None,
                            provider_context: None,
                        },
                        ProviderInputItem::Message {
                            id: "assistant-current".to_string(),
                            role: Role::Assistant,
                            content: "Current response.".to_string(),
                            content_parts: Vec::new(),
                            attachments: Vec::new(),
                            provider_response_id: Some("resp_current".to_string()),
                            provider_context: None,
                        },
                        ProviderInputItem::Message {
                            id: "user-current".to_string(),
                            role: Role::User,
                            content: "Next step.".to_string(),
                            content_parts: Vec::new(),
                            attachments: Vec::new(),
                            provider_response_id: None,
                            provider_context: None,
                        },
                    ],
                },
                available_tools: Vec::new(),
                generation: ModelGenerationConfig::default(),
            },
            false,
        )?;

        assert_eq!(body["previous_response_id"], "resp_current");
        let input = body["input"]
            .as_array()
            .expect("input should be serialized");
        assert!(
            !input
                .iter()
                .any(|item| item["type"] == "function_call_output"),
            "stale missing tool output must not leak into a resumed delta: {input:#?}"
        );
        assert!(input.iter().any(|item| {
            item["type"] == "message"
                && item["role"] == "user"
                && item["content"][0]["text"] == "Next step."
        }));
        Ok(())
    }

    #[test]
    fn openai_request_body_preserves_ordered_input_parts() -> Result<()> {
        let temp = create_fixture_dir("openai-ordered-parts")?;
        let png = write_image_attachment(&temp, "ordered.png", "image/png")?;
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-5.4", "test-key"))?;

        let body = provider.build_request_body(
            &ModelRuntimeRequest {
                attempt: 1,
                kind: kheish_core::ModelRequestKind::MainLoop,
                session_id: "session-ordered-parts".to_string(),
                thread_id: None,
                turn: 1,
                prompt: ProviderPrompt {
                    instructions: Vec::new(),
                    force_synthetic_user_prefix: false,
                    items: vec![ProviderInputItem::Message {
                        id: "user-1".to_string(),
                        role: kheish_types::Role::User,
                        content: "fallback transcript".to_string(),
                        content_parts: vec![
                            kheish_types::InputContentPart::Text {
                                text: "Before".to_string(),
                            },
                            kheish_types::InputContentPart::Attachment {
                                attachment: png.clone(),
                            },
                            kheish_types::InputContentPart::Text {
                                text: "After".to_string(),
                            },
                        ],
                        attachments: vec![png],
                        provider_response_id: None,
                        provider_context: None,
                    }],
                },
                available_tools: Vec::new(),
                generation: ModelGenerationConfig::default(),
            },
            false,
        )?;

        let content = body["input"][0]["content"]
            .as_array()
            .expect("content should be an array");
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[0]["text"], "Before");
        assert_eq!(content[1]["type"], "input_text");
        assert!(
            content[1]["text"]
                .as_str()
                .is_some_and(|text| text.contains("fixture-ordered.png"))
        );
        assert_eq!(content[2]["type"], "input_image");
        assert_eq!(content[3]["type"], "input_text");
        assert_eq!(content[3]["text"], "After");
        Ok(())
    }

    #[test]
    fn openai_allows_document_only_inputs_on_non_vision_models() -> Result<()> {
        let temp = create_fixture_dir("openai-doc-only")?;
        let document =
            write_document_attachment(&temp, "brief.md", "text/markdown", "DOCUMENT_ONLY_OK")?;
        let provider =
            OpenAiProvider::new(OpenAiProviderConfig::new("text-only-model", "test-key"))?;

        let body = provider.build_request_body(
            &ModelRuntimeRequest {
                attempt: 1,
                kind: kheish_core::ModelRequestKind::MainLoop,
                session_id: "session-doc-only".to_string(),
                thread_id: None,
                turn: 1,
                prompt: ProviderPrompt {
                    instructions: Vec::new(),
                    force_synthetic_user_prefix: false,
                    items: vec![ProviderInputItem::Message {
                        id: "user-1".to_string(),
                        role: kheish_types::Role::User,
                        content: String::new(),
                        content_parts: vec![InputContentPart::Attachment {
                            attachment: document.clone(),
                        }],
                        attachments: vec![document],
                        provider_response_id: None,
                        provider_context: None,
                    }],
                },
                available_tools: Vec::new(),
                generation: ModelGenerationConfig::default(),
            },
            false,
        )?;

        let content = body["input"][0]["content"]
            .as_array()
            .expect("content should be an array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "input_text");
        assert!(
            content[0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("DOCUMENT_ONLY_OK")
        );
        Ok(())
    }

    #[test]
    fn openai_includes_document_preview_images_on_vision_models() -> Result<()> {
        let temp = create_fixture_dir("openai-doc-preview")?;
        let document = write_document_attachment_with_preview(
            &temp,
            "plan.dxf",
            "application/dxf",
            "DXF summary",
        )?;
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-5.4", "test-key"))?;

        let body = provider.build_request_body(
            &ModelRuntimeRequest {
                attempt: 1,
                kind: kheish_core::ModelRequestKind::MainLoop,
                session_id: "session-doc-preview".to_string(),
                thread_id: None,
                turn: 1,
                prompt: ProviderPrompt {
                    instructions: Vec::new(),
                    force_synthetic_user_prefix: false,
                    items: vec![ProviderInputItem::Message {
                        id: "user-1".to_string(),
                        role: kheish_types::Role::User,
                        content: String::new(),
                        content_parts: vec![InputContentPart::Attachment {
                            attachment: document.clone(),
                        }],
                        attachments: vec![document],
                        provider_response_id: None,
                        provider_context: None,
                    }],
                },
                available_tools: Vec::new(),
                generation: ModelGenerationConfig::default(),
            },
            false,
        )?;

        let content = body["input"][0]["content"]
            .as_array()
            .expect("content should be an array");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "input_image");
        assert!(
            content[0]["image_url"]
                .as_str()
                .unwrap_or_default()
                .starts_with("data:image/png;base64,")
        );
        assert_eq!(content[1]["type"], "input_text");
        assert!(
            content[1]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("DXF summary")
        );
        Ok(())
    }

    #[test]
    fn openai_request_body_prefers_generation_model_override() {
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-base", "test-key"))
            .expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-override".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig {
                        model: Some("gpt-fallback".to_string()),
                        fallback_model: None,
                        ..ModelGenerationConfig::default()
                    },
                },
                false,
            )
            .expect("request body should build");
        assert_eq!(body["model"], "gpt-fallback");
        assert_eq!(
            body["max_output_tokens"],
            model_max_output_tokens("gpt-fallback").default
        );
    }

    #[test]
    fn openai_sets_strict_false_for_non_strict_compatible_tool_schema() {
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-test", "test-key"))
            .expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-strict".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: vec![ToolDefinition {
                        name: "task_create".to_string(),
                        description: "Creates a task with free-form metadata".to_string(),
                        input_schema: json!({
                            "type": "object",
                            "properties": {
                                "metadata": {
                                    "type": ["object", "null"],
                                    "additionalProperties": true
                                }
                            },
                            "required": ["metadata"],
                            "additionalProperties": false
                        }),
                        allows_parallel: true,
                    }],
                    generation: ModelGenerationConfig::default(),
                },
                false,
            )
            .expect("request body should build");

        assert_eq!(body["tools"][0]["strict"], json!(false));
    }

    #[test]
    fn openai_sets_strict_false_for_large_optional_tool_schema() {
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-test", "test-key"))
            .expect("provider should build");
        let mut properties = serde_json::Map::new();
        properties.insert("repo".to_string(), json!({"type": "string"}));
        for index in 0..13 {
            properties.insert(format!("optional_{index}"), json!({"type": "string"}));
        }
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-large-optional".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: vec![ToolDefinition {
                        name: "security_repo_audit".to_string(),
                        description: "Runs a repository audit".to_string(),
                        input_schema: json!({
                            "type": "object",
                            "properties": properties,
                            "required": ["repo"],
                            "additionalProperties": false
                        }),
                        allows_parallel: false,
                    }],
                    generation: ModelGenerationConfig::default(),
                },
                false,
            )
            .expect("request body should build");

        assert_eq!(body["tools"][0]["strict"], json!(false));
        assert_eq!(body["tools"][0]["parameters"]["required"], json!(["repo"]));
    }

    #[test]
    fn openai_strict_schema_makes_nested_optional_tool_fields_nullable() {
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-test", "test-key"))
            .expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-nested-strict".to_string(),
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
                },
                false,
            )
            .expect("request body should build");

        assert_eq!(body["tools"][0]["strict"], json!(true));
        let parameters = &body["tools"][0]["parameters"];
        let question = &parameters["properties"]["questions"]["items"];
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
        assert_eq!(question["properties"]["options"]["type"], json!("array"));

        let option = &question["properties"]["options"]["items"];
        assert_eq!(
            option["required"],
            json!(["description", "id", "label", "preview"])
        );
        assert_eq!(option["properties"]["label"]["type"], json!("string"));
        assert_eq!(
            option["properties"]["preview"]["type"],
            json!(["string", "null"])
        );
    }

    #[test]
    fn openai_structured_output_schema_makes_optional_fields_nullable() {
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-test", "test-key"))
            .expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-structured-nullable".to_string(),
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
                },
                false,
            )
            .expect("request body should build");

        let schema = &body["text"]["format"]["schema"];
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["required"], json!(["answer", "confidence"]));
        assert_eq!(schema["properties"]["answer"]["type"], json!("string"));
        assert_eq!(
            schema["properties"]["confidence"]["type"],
            json!(["number", "null"])
        );
    }

    #[test]
    fn codex_account_request_body_uses_codex_compatible_shape() {
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-test", "test-key"))
            .expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-codex-shape".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                true,
            )
            .expect("request body should build");
        assert_eq!(body["store"], Value::Bool(false));
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["include"], Value::Array(Vec::new()));
        assert_eq!(body["tool_choice"], Value::String("auto".to_string()));
        assert_eq!(
            body["instructions"],
            Value::String("You are a helpful assistant.".to_string())
        );
    }

    #[test]
    fn codex_account_request_body_replays_tool_results_without_response_resume() {
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-test", "test-key"))
            .expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-codex-tool-result".to_string(),
                    thread_id: None,
                    turn: 2,
                    prompt: ProviderPrompt {
                        instructions: Vec::new(),
                        force_synthetic_user_prefix: false,
                        items: vec![
                            ProviderInputItem::Message {
                                id: "user-1".to_string(),
                                role: kheish_types::Role::User,
                                content: "Call the tool.".to_string(),
                                content_parts: Vec::new(),
                                attachments: Vec::new(),
                                provider_response_id: None,
                                provider_context: None,
                            },
                            ProviderInputItem::ToolCall {
                                assistant_message_id: Some("assistant-1".to_string()),
                                call: ToolCallRecord {
                                    id: "call-1".to_string(),
                                    name: "echo".to_string(),
                                    input: json!({"text": "ping"}),
                                    assistant_message_id: Some("assistant-1".to_string()),
                                    assistant_provider_response_id: Some("resp_123".to_string()),
                                },
                            },
                            ProviderInputItem::ToolResult {
                                result: ToolResultRecord {
                                    call_id: "call-1".to_string(),
                                    output: json!({"echo": "ping"}),
                                    is_error: false,
                                    tool_name: Some("echo".to_string()),
                                    offset: None,
                                    timestamp_ms: None,
                                    context_updates: Vec::new(),
                                    hook_contexts: Vec::new(),
                                },
                            },
                            ProviderInputItem::Message {
                                id: "user-2".to_string(),
                                role: kheish_types::Role::User,
                                content: "Finish.".to_string(),
                                content_parts: Vec::new(),
                                attachments: Vec::new(),
                                provider_response_id: None,
                                provider_context: None,
                            },
                        ],
                    },
                    available_tools: vec![ToolDefinition {
                        name: "echo".to_string(),
                        description: "Echoes input".to_string(),
                        input_schema: json!({
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"],
                            "additionalProperties": false
                        }),
                        allows_parallel: true,
                    }],
                    generation: ModelGenerationConfig::default(),
                },
                true,
            )
            .expect("request body should build");

        assert_eq!(body["store"], Value::Bool(false));
        assert!(body.get("previous_response_id").is_none());
        let input = body["input"].as_array().expect("input should be an array");
        assert!(input.iter().any(|item| item["type"] == "function_call"));
        assert!(
            input
                .iter()
                .any(|item| item["type"] == "function_call_output")
        );
    }

    #[test]
    fn xai_request_body_moves_system_prompt_into_input_messages() {
        let config = OpenAiProviderConfig::new("grok-test", "test-key")
            .with_flavor(ResponsesProviderFlavor::XAi);
        let provider = OpenAiProvider::new(config).expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-xai-shape".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt {
                        instructions: vec!["System directive".to_string()],
                        force_synthetic_user_prefix: false,
                        items: vec![ProviderInputItem::Message {
                            id: "msg-user".to_string(),
                            role: Role::User,
                            content: "Hello".to_string(),
                            content_parts: Vec::new(),
                            attachments: Vec::new(),
                            provider_response_id: None,
                            provider_context: None,
                        }],
                    },
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                false,
            )
            .expect("request body should build");

        assert!(body.get("instructions").is_none());
        let input = body["input"]
            .as_array()
            .expect("xAI input should be an array");
        assert_eq!(
            input.first().and_then(|item| item["role"].as_str()),
            Some("system")
        );
        assert_eq!(input[0]["content"][0]["type"].as_str(), Some("input_text"));
        assert_eq!(
            input[0]["content"][0]["text"].as_str(),
            Some("System directive")
        );
    }

    #[test]
    fn xai_request_body_omits_none_tool_choice_without_available_tools() {
        let config = OpenAiProviderConfig::new("grok-test", "test-key")
            .with_flavor(ResponsesProviderFlavor::XAi);
        let provider = OpenAiProvider::new(config).expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-xai-none".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig {
                        tool_choice: ToolChoice::None,
                        ..ModelGenerationConfig::default()
                    },
                },
                false,
            )
            .expect("request body should build");

        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn xai_request_body_omits_auto_tool_choice_without_available_tools() {
        let config = OpenAiProviderConfig::new("grok-test", "test-key")
            .with_flavor(ResponsesProviderFlavor::XAi);
        let provider = OpenAiProvider::new(config).expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-xai-auto".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                false,
            )
            .expect("request body should build");

        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn xai_request_body_omits_text_format_for_plain_text_responses() {
        let provider = OpenAiProvider::new(
            OpenAiProviderConfig::new("grok-test", "test-key")
                .with_flavor(ResponsesProviderFlavor::XAi),
        )
        .expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-xai-text-format".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                false,
            )
            .expect("request body should build");

        assert!(body.get("text").is_none());
    }

    #[test]
    fn xai_image_generation_body_uses_documented_request_shape() {
        let body = build_image_generation_body(
            "grok-imagine-image",
            &OpenAiImageGenerationRequest {
                prompt: "Draw a cat".to_string(),
                count: 1,
                size: Some("1792x1024".to_string()),
            },
            ResponsesProviderFlavor::XAi,
        );

        assert_eq!(
            body["response_format"],
            Value::String("b64_json".to_string())
        );
        assert_eq!(body["aspect_ratio"], Value::String("16:9".to_string()));
        assert_eq!(body["resolution"], Value::String("2k".to_string()));
    }

    #[test]
    fn xai_image_edit_body_uses_documented_json_shape() {
        let single = build_xai_image_edit_body(
            "grok-imagine-image",
            &OpenAiImageEditRequest {
                prompt: "Darken the facade".to_string(),
                images: vec![OpenAiImageEditInput {
                    file_name: "source.png".to_string(),
                    media_type: "image/png".to_string(),
                    bytes: b"SOURCE".to_vec(),
                }],
                count: 1,
                size: Some("1024x1024".to_string()),
            },
        );

        assert_eq!(single["model"], "grok-imagine-image");
        assert_eq!(single["response_format"], "b64_json");
        assert_eq!(single["aspect_ratio"], "1:1");
        assert_eq!(single["resolution"], "1k");
        assert_eq!(single["image"]["type"], "image_url");
        assert_eq!(
            single["image"]["url"],
            format!(
                "data:image/png;base64,{}",
                BASE64_STANDARD.encode(b"SOURCE")
            )
        );
        assert!(single.get("images").is_none());

        let multi = build_xai_image_edit_body(
            "grok-imagine-image",
            &OpenAiImageEditRequest {
                prompt: "Use the reference".to_string(),
                images: vec![
                    OpenAiImageEditInput {
                        file_name: "source.png".to_string(),
                        media_type: "image/png".to_string(),
                        bytes: b"SOURCE".to_vec(),
                    },
                    OpenAiImageEditInput {
                        file_name: "reference.jpg".to_string(),
                        media_type: "image/jpeg".to_string(),
                        bytes: b"REFERENCE".to_vec(),
                    },
                ],
                count: 2,
                size: Some("1792x1024".to_string()),
            },
        );

        assert!(multi.get("image").is_none());
        assert_eq!(multi["aspect_ratio"], "16:9");
        assert_eq!(multi["resolution"], "2k");
        let images = multi["images"].as_array().expect("images array");
        assert_eq!(images.len(), 2);
        assert_eq!(images[0]["type"], "image_url");
        assert_eq!(images[1]["type"], "image_url");
        assert!(
            images[1]["url"]
                .as_str()
                .expect("image URL")
                .starts_with("data:image/jpeg;base64,")
        );
    }

    #[test]
    fn openai_image_edit_endpoint_targets_the_images_edits_api() -> Result<()> {
        assert_eq!(
            image_edit_endpoint(
                "https://api.openai.com/v1/responses",
                ResponsesProviderFlavor::OpenAi,
            )?,
            "https://api.openai.com/v1/images/edits"
        );
        Ok(())
    }

    #[test]
    fn xai_image_edit_endpoint_targets_the_images_edits_api() -> Result<()> {
        assert_eq!(
            image_edit_endpoint(
                "https://api.x.ai/v1/responses",
                ResponsesProviderFlavor::XAi,
            )?,
            "https://api.x.ai/v1/images/edits"
        );
        Ok(())
    }

    #[test]
    fn openai_audio_speech_endpoint_targets_the_audio_speech_api() -> Result<()> {
        assert_eq!(
            openai_audio_speech_endpoint("https://api.openai.com/v1/responses")?,
            "https://api.openai.com/v1/audio/speech"
        );
        assert!(
            openai_audio_speech_endpoint("https://chatgpt.com/backend-api/codex/responses")
                .expect_err("Codex account endpoints must be explicit")
                .message
                .contains("do not support speech synthesis")
        );
        Ok(())
    }

    #[tokio::test]
    async fn openai_speech_synthesizer_posts_to_audio_speech_endpoint() -> Result<()> {
        let captured = Arc::new(Mutex::new(String::new()));
        let url = spawn_mock_server(
            200,
            &[("content-type", "audio/wav")],
            "wav-audio",
            captured.clone(),
        )
        .await?;
        let synthesizer = OpenAiSpeechSynthesizer::new(
            OpenAiProviderConfig {
                base_url: format!("{url}/v1/responses"),
                ..OpenAiProviderConfig::new("gpt-5.4", "test-key")
            },
            Arc::new(crate::NoopObserver),
        )?;

        let response = synthesizer
            .synthesize(&OpenAiSpeechRequest {
                input: "Hello world".to_string(),
                instructions: Some("Speak with a concise studio-news tone.".to_string()),
                voice: Some("coral".to_string()),
                response_format: Some("wav".to_string()),
                speed: Some(1.25),
            })
            .await?;

        assert_eq!(response.provider, "openai");
        assert_eq!(response.model, DEFAULT_OPENAI_TTS_MODEL);
        assert_eq!(response.media_type, "audio/wav");
        assert_eq!(response.bytes, b"wav-audio".to_vec());

        let body: Value = serde_json::from_str(&captured.lock().expect("body mutex poisoned"))?;
        assert_eq!(body["model"], DEFAULT_OPENAI_TTS_MODEL);
        assert_eq!(body["voice"], "coral");
        assert_eq!(body["response_format"], "wav");
        assert_eq!(body["speed"], 1.25);
        assert_eq!(body["input"], "Hello world");
        assert_eq!(
            body["instructions"],
            "Speak with a concise studio-news tone."
        );
        Ok(())
    }

    #[tokio::test]
    async fn openai_speech_synthesizer_rejects_invalid_voice_locally() -> Result<()> {
        let synthesizer = OpenAiSpeechSynthesizer::new(
            OpenAiProviderConfig::new("gpt-4o-mini-tts", "test-key"),
            Arc::new(crate::NoopObserver),
        )?;

        let error = synthesizer
            .synthesize(&OpenAiSpeechRequest {
                input: "Hello world".to_string(),
                instructions: None,
                voice: Some("not-a-real-voice".to_string()),
                response_format: None,
                speed: None,
            })
            .await
            .expect_err("invalid voice should fail locally");

        assert!(
            error.message.contains("unsupported OpenAI speech voice"),
            "unexpected error: {}",
            error.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn openai_speech_synthesizer_rejects_oversized_response_stream() -> Result<()> {
        let url = spawn_mock_server_with_declared_content_length(
            &[("content-type", "audio/mpeg")],
            MAX_OPENAI_AUDIO_RESPONSE_BYTES + 1,
        )
        .await?;
        let synthesizer = OpenAiSpeechSynthesizer::new(
            OpenAiProviderConfig {
                base_url: format!("{url}/v1"),
                ..OpenAiProviderConfig::new("gpt-4o-mini-tts", "test-key")
            },
            Arc::new(crate::NoopObserver),
        )?;

        let error = synthesizer
            .synthesize(&OpenAiSpeechRequest {
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
    async fn openai_speech_synthesizer_records_debug_without_audio_bytes() -> Result<()> {
        let url = spawn_mock_server(
            200,
            &[("content-type", "application/octet-stream")],
            "mp3-bytes",
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let observer = FixedDebugObserver::shared(DebugCaptureLevel::Full);
        let synthesizer = OpenAiSpeechSynthesizer::new(
            OpenAiProviderConfig {
                base_url: format!("{url}/v1"),
                ..OpenAiProviderConfig::new("gpt-4o-mini-tts", "test-key")
            },
            observer.clone(),
        )?;

        let response = synthesizer
            .synthesize(&OpenAiSpeechRequest {
                input: "Hidden words".to_string(),
                instructions: Some("Sensitive style hint".to_string()),
                voice: None,
                response_format: None,
                speed: None,
            })
            .await?;

        assert_eq!(response.media_type, "audio/mpeg");
        let artifacts = observer.debug_artifacts();
        assert!(
            artifacts
                .iter()
                .any(|artifact| artifact.name == "openai-audio-speech-provider-request")
        );
        let response_artifact = artifacts
            .iter()
            .find(|artifact| artifact.name == "openai-audio-speech-provider-response")
            .expect("speech response artifact should be recorded");
        assert_eq!(response_artifact.payload["body"]["byte_len"], 9);
        assert!(!serde_json::to_string(&response_artifact.payload)?.contains("mp3-bytes"));
        Ok(())
    }

    #[tokio::test]
    async fn openai_speech_synthesizer_refreshes_once_after_401() -> Result<()> {
        #[derive(Default)]
        struct Counts {
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
                        "Bearer stale-token".to_string(),
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
                        "Bearer fresh-token".to_string(),
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
        let server = tokio::spawn(async move {
            for (index, expected_auth) in ["Bearer stale-token", "Bearer fresh-token"]
                .into_iter()
                .enumerate()
            {
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
                    .unwrap_or_default();
                assert_eq!(auth_header, expected_auth);
                let (status, body, content_type) = if index == 0 {
                    (
                        "401 Unauthorized",
                        "{\"error\":{\"message\":\"expired\"}}",
                        "application/json",
                    )
                } else {
                    ("200 OK", "audio", "audio/mpeg")
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("response write should succeed");
            }
        });

        let synthesizer = OpenAiSpeechSynthesizer::new(
            OpenAiProviderConfig {
                base_url: format!("http://{address}/v1/responses"),
                api_key: None,
                request_auth_provider: Some(Arc::new(FakeAuthProvider {
                    counts: counts.clone(),
                })),
                ..OpenAiProviderConfig::new("gpt-4o-mini-tts", "unused-key")
            },
            Arc::new(crate::NoopObserver),
        )?;

        let response = synthesizer
            .synthesize(&OpenAiSpeechRequest {
                input: "Hello".to_string(),
                instructions: None,
                voice: None,
                response_format: None,
                speed: None,
            })
            .await?;

        assert_eq!(response.bytes, b"audio".to_vec());
        assert_eq!(counts.resolves.load(Ordering::SeqCst), 1);
        assert_eq!(counts.refreshes.load(Ordering::SeqCst), 1);
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn openai_request_auth_revalidates_lease_before_send() -> Result<()> {
        struct RevokedAuthProvider;

        #[async_trait]
        impl RequestAuthProvider for RevokedAuthProvider {
            async fn resolve(&self) -> anyhow::Result<ResolvedAuthMaterial> {
                Ok(ResolvedAuthMaterial {
                    headers: [(
                        "Authorization".to_string(),
                        "Bearer should-not-send".to_string(),
                    )]
                    .into_iter()
                    .collect(),
                    base_url_override: None,
                    grant_id: Some("grant-revoked".to_string()),
                    lease_id: Some("lease-revoked".to_string()),
                })
            }

            async fn refresh(&self) -> anyhow::Result<ResolvedAuthMaterial> {
                self.resolve().await
            }

            async fn ensure_active(&self, material: &ResolvedAuthMaterial) -> anyhow::Result<()> {
                anyhow::bail!(
                    "credential lease {} is not active",
                    material.lease_id.as_deref().unwrap_or("<missing>")
                )
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let synthesizer = OpenAiSpeechSynthesizer::new(
            OpenAiProviderConfig {
                base_url: format!("http://{address}/v1/responses"),
                api_key: None,
                request_auth_provider: Some(Arc::new(RevokedAuthProvider)),
                ..OpenAiProviderConfig::new("gpt-4o-mini-tts", "unused-key")
            },
            Arc::new(crate::NoopObserver),
        )?;

        let error = synthesizer
            .synthesize(&OpenAiSpeechRequest {
                input: "Hello".to_string(),
                instructions: None,
                voice: None,
                response_format: None,
                speed: None,
            })
            .await
            .expect_err("revoked credential lease should block request");

        assert!(error.message.contains("not active"));

        let generator = OpenAiImageGenerator::new(
            OpenAiProviderConfig {
                base_url: format!("http://{address}/v1/responses"),
                api_key: None,
                request_auth_provider: Some(Arc::new(RevokedAuthProvider)),
                ..OpenAiProviderConfig::new("gpt-image-1", "unused-key")
            },
            Arc::new(crate::NoopObserver),
        )?;
        let error = generator
            .generate(OpenAiImageGenerationRequest {
                prompt: "Generate a test image".to_string(),
                count: 1,
                size: None,
            })
            .await
            .expect_err("revoked credential lease should block image generation");
        assert!(error.message.contains("not active"));

        let editor = OpenAiImageEditor::new(
            OpenAiProviderConfig {
                base_url: format!("http://{address}/v1/responses"),
                api_key: None,
                request_auth_provider: Some(Arc::new(RevokedAuthProvider)),
                ..OpenAiProviderConfig::new("gpt-image-1", "unused-key")
            },
            Arc::new(crate::NoopObserver),
        )?;
        let error = editor
            .edit(OpenAiImageEditRequest {
                prompt: "Edit the test image".to_string(),
                images: vec![OpenAiImageEditInput {
                    file_name: "source.png".to_string(),
                    media_type: "image/png".to_string(),
                    bytes: b"png".to_vec(),
                }],
                count: 1,
                size: None,
            })
            .await
            .expect_err("revoked credential lease should block image edit");
        assert!(error.message.contains("not active"));

        assert!(
            tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "provider request should not reach upstream after lease revalidation failed"
        );
        Ok(())
    }

    #[test]
    fn openai_image_edit_fields_preserve_image_order() -> Result<()> {
        let fields = image_edit_form_fields(
            "gpt-image-1.5",
            &OpenAiImageEditRequest {
                prompt: "Combine these references".to_string(),
                images: vec![
                    OpenAiImageEditInput {
                        file_name: "target.png".to_string(),
                        media_type: "image/png".to_string(),
                        bytes: b"TARGET".to_vec(),
                    },
                    OpenAiImageEditInput {
                        file_name: "reference.jpg".to_string(),
                        media_type: "image/jpeg".to_string(),
                        bytes: b"REFERENCE".to_vec(),
                    },
                ],
                count: 2,
                size: Some("1024x1024".to_string()),
            },
            ResponsesProviderFlavor::OpenAi,
        )?;

        assert!(matches!(
            &fields[0],
            ImageEditMultipartField::Text { name, value }
                if name == "model" && value == "gpt-image-1.5"
        ));
        assert!(matches!(
            &fields[1],
            ImageEditMultipartField::Text { name, value }
                if name == "prompt" && value == "Combine these references"
        ));
        assert!(matches!(
            &fields[3],
            ImageEditMultipartField::Text { name, value }
                if name == "size" && value == "1024x1024"
        ));
        assert!(matches!(
            &fields[4],
            ImageEditMultipartField::Binary { name, file_name, media_type, .. }
                if name == "image[]" && file_name == "target.png" && media_type == "image/png"
        ));
        assert!(matches!(
            &fields[5],
            ImageEditMultipartField::Binary { name, file_name, media_type, .. }
                if name == "image[]" && file_name == "reference.jpg" && media_type == "image/jpeg"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn openai_image_generator_records_external_action_traces() -> Result<()> {
        let response_body = format!(
            "{{\"created\":1,\"data\":[{{\"b64_json\":\"{}\"}}]}}",
            BASE64_STANDARD.encode(b"generated-image")
        );
        let url = spawn_mock_server(
            200,
            &[("content-type", "application/json")],
            &response_body,
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let observer = InMemoryObserver::shared();
        let generator = OpenAiImageGenerator::new(
            OpenAiProviderConfig {
                base_url: format!("{url}/v1"),
                ..OpenAiProviderConfig::new("gpt-image-1.5", "test-key")
            },
            observer.clone(),
        )?;

        let response = generator
            .generate(OpenAiImageGenerationRequest {
                prompt: "Draw a lighthouse".to_string(),
                count: 1,
                size: Some("1024x1024".to_string()),
            })
            .await?;

        assert_eq!(response.images.len(), 1);
        let traces = observer.traces();
        assert!(has_provider_external_action(
            &traces,
            "request",
            "openai:http://"
        ));
        assert!(has_provider_external_action(
            &traces,
            "response",
            "openai:http://"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn openai_image_generator_rejects_oversized_json_content_length() -> Result<()> {
        let url = spawn_mock_server_with_declared_content_length(
            &[("content-type", "application/json")],
            super::MAX_OPENAI_IMAGE_RESPONSE_BYTES + 1,
        )
        .await?;
        let generator = OpenAiImageGenerator::new(
            OpenAiProviderConfig {
                base_url: format!("{url}/v1"),
                ..OpenAiProviderConfig::new("gpt-image-1.5", "test-key")
            },
            Arc::new(NoopObserver),
        )?;

        let error = generator
            .generate(OpenAiImageGenerationRequest {
                prompt: "Draw a lighthouse".to_string(),
                count: 1,
                size: Some("1024x1024".to_string()),
            })
            .await
            .expect_err("oversized image response should fail before JSON parse");

        assert!(
            error
                .message
                .contains("OpenAI image generation response exceeds"),
            "unexpected error: {error:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn openai_image_generator_rejects_oversized_chunked_json_response() -> Result<()> {
        let chunks = vec![vec![b' '; 1024 * 1024]; 25];
        let url = spawn_chunked_mock_server(
            &[("content-type", "application/json")],
            chunks,
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let generator = OpenAiImageGenerator::new(
            OpenAiProviderConfig {
                base_url: format!("{url}/v1"),
                ..OpenAiProviderConfig::new("gpt-image-1.5", "test-key")
            },
            Arc::new(NoopObserver),
        )?;

        let error = generator
            .generate(OpenAiImageGenerationRequest {
                prompt: "Draw a lighthouse".to_string(),
                count: 1,
                size: Some("1024x1024".to_string()),
            })
            .await
            .expect_err("oversized chunked image response should fail before JSON parse");

        assert!(
            error
                .message
                .contains("OpenAI image generation response exceeds"),
            "unexpected error: {error:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn openai_image_editor_decodes_successful_edit_responses() -> Result<()> {
        let response_body = format!(
            "{{\"created\":1,\"data\":[{{\"b64_json\":\"{}\"}}]}}",
            BASE64_STANDARD.encode(b"edited-image")
        );
        let url = spawn_mock_server(
            200,
            &[("content-type", "application/json")],
            &response_body,
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let editor = OpenAiImageEditor::new(
            OpenAiProviderConfig {
                base_url: format!("{url}/v1"),
                ..OpenAiProviderConfig::new("gpt-image-1.5", "test-key")
            },
            Arc::new(crate::NoopObserver),
        )?;

        let response = editor
            .edit(OpenAiImageEditRequest {
                prompt: "Combine these references".to_string(),
                images: vec![OpenAiImageEditInput {
                    file_name: "target.png".to_string(),
                    media_type: "image/png".to_string(),
                    bytes: b"TARGET".to_vec(),
                }],
                count: 1,
                size: None,
            })
            .await?;

        assert_eq!(response.images.len(), 1);
        assert_eq!(response.images[0].media_type, "image/png");
        Ok(())
    }

    #[tokio::test]
    async fn openai_image_editor_records_external_action_traces() -> Result<()> {
        let response_body = format!(
            "{{\"created\":1,\"data\":[{{\"b64_json\":\"{}\"}}]}}",
            BASE64_STANDARD.encode(b"edited-image")
        );
        let url = spawn_mock_server(
            200,
            &[("content-type", "application/json")],
            &response_body,
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let observer = InMemoryObserver::shared();
        let editor = OpenAiImageEditor::new(
            OpenAiProviderConfig {
                base_url: format!("{url}/v1"),
                ..OpenAiProviderConfig::new("gpt-image-1.5", "test-key")
            },
            observer.clone(),
        )?;

        let response = editor
            .edit(OpenAiImageEditRequest {
                prompt: "Combine these references".to_string(),
                images: vec![OpenAiImageEditInput {
                    file_name: "target.png".to_string(),
                    media_type: "image/png".to_string(),
                    bytes: b"TARGET".to_vec(),
                }],
                count: 1,
                size: None,
            })
            .await?;

        assert_eq!(response.images.len(), 1);
        let traces = observer.traces();
        assert!(has_provider_external_action(
            &traces,
            "request",
            "openai:http://"
        ));
        assert!(has_provider_external_action(
            &traces,
            "response",
            "openai:http://"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn xai_image_editor_posts_json_and_records_native_debug_artifact() -> Result<()> {
        let response_body = format!(
            "{{\"created\":1,\"data\":[{{\"b64_json\":\"{}\"}}]}}",
            BASE64_STANDARD.encode(b"edited-image")
        );
        let captured = Arc::new(Mutex::new(String::new()));
        let url = spawn_mock_server(
            200,
            &[("content-type", "application/json")],
            &response_body,
            captured.clone(),
        )
        .await?;
        let observer = FixedDebugObserver::shared(DebugCaptureLevel::Full);
        let editor = OpenAiImageEditor::new(
            OpenAiProviderConfig {
                base_url: format!("{url}/v1/responses"),
                ..OpenAiProviderConfig::new("grok-imagine-image", "test-key")
                    .with_flavor(ResponsesProviderFlavor::XAi)
            },
            observer.clone(),
        )?;

        let response = editor
            .edit(OpenAiImageEditRequest {
                prompt: "Combine these references".to_string(),
                images: vec![
                    OpenAiImageEditInput {
                        file_name: "target.png".to_string(),
                        media_type: "image/png".to_string(),
                        bytes: b"TARGET".to_vec(),
                    },
                    OpenAiImageEditInput {
                        file_name: "reference.jpg".to_string(),
                        media_type: "image/jpeg".to_string(),
                        bytes: b"REFERENCE".to_vec(),
                    },
                ],
                count: 1,
                size: Some("1792x1024".to_string()),
            })
            .await?;

        assert_eq!(response.images.len(), 1);
        assert_eq!(response.images[0].media_type, "image/jpeg");
        let body = serde_json::from_str::<Value>(
            &captured
                .lock()
                .expect("request capture mutex poisoned")
                .clone(),
        )?;
        assert_eq!(body["model"], "grok-imagine-image");
        assert_eq!(body["response_format"], "b64_json");
        assert!(body.get("image").is_none());
        assert_eq!(
            body["images"]
                .as_array()
                .expect("xAI multi-image edit body")
                .len(),
            2
        );

        let request_artifact = observer
            .debug_artifacts()
            .into_iter()
            .find(|artifact| artifact.name == "xai-image-edit-provider-request")
            .expect("xAI image edit request artifact should be recorded");
        assert_eq!(request_artifact.payload["provider"], "xai");
        assert_eq!(request_artifact.payload["method"], "POST");
        assert!(
            request_artifact.payload["url"]
                .as_str()
                .expect("debug URL")
                .ends_with("/v1/images/edits")
        );
        assert_eq!(
            request_artifact.payload["body"]["images"][0]["type"],
            "image_url"
        );
        Ok(())
    }

    #[test]
    fn xai_specific_tool_choice_uses_responses_function_shape() {
        let provider = OpenAiProvider::new(
            OpenAiProviderConfig::new("grok-test", "test-key")
                .with_flavor(ResponsesProviderFlavor::XAi),
        )
        .expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-xai-specific-tool".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: vec![ToolDefinition {
                        name: "echo".to_string(),
                        description: "Echoes input".to_string(),
                        input_schema: json!({
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"],
                            "additionalProperties": false
                        }),
                        allows_parallel: true,
                    }],
                    generation: ModelGenerationConfig {
                        tool_choice: ToolChoice::Specific {
                            name: "echo".to_string(),
                        },
                        ..ModelGenerationConfig::default()
                    },
                },
                false,
            )
            .expect("request body should build");

        assert_eq!(body["tool_choice"]["type"], "function");
        assert_eq!(body["tool_choice"]["name"], "echo");
        assert!(body["tools"][0].get("strict").is_none());
    }

    #[test]
    fn request_body_omits_tool_choice_when_no_tools_are_available() {
        let provider = OpenAiProvider::new(
            OpenAiProviderConfig::new("grok-test", "test-key")
                .with_flavor(ResponsesProviderFlavor::XAi),
        )
        .expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-xai-tool-choice".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig {
                        tool_choice: ToolChoice::None,
                        ..ModelGenerationConfig::default()
                    },
                },
                false,
            )
            .expect("request body should build");

        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn openai_request_body_omits_tool_choice_when_no_tools_are_available() {
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-test", "test-key"))
            .expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-openai-tool-choice".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                false,
            )
            .expect("request body should build");

        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn openai_request_body_omits_temperature_for_gpt5_models() {
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-5.4", "test-key"))
            .expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-openai-temperature".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig {
                        temperature: Some(0.0),
                        ..ModelGenerationConfig::default()
                    },
                },
                false,
            )
            .expect("request body should build");

        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn openai_request_body_includes_reasoning_config() {
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-5.4", "test-key"))
            .expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-openai-reasoning".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig {
                        reasoning: Some(ReasoningConfig {
                            effort: Some(ReasoningEffort::Xhigh),
                            summary: Some(ReasoningSummary::Auto),
                            budget_tokens: None,
                            interleaved: false,
                        }),
                        ..ModelGenerationConfig::default()
                    },
                },
                false,
            )
            .expect("request body should build");

        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert!(body["text"]["format"].is_object());
    }

    #[test]
    fn openai_request_body_rejects_provider_specific_reasoning_fields() {
        let provider = OpenAiProvider::new(OpenAiProviderConfig::new("gpt-5.4", "test-key"))
            .expect("provider should build");
        let error = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-openai-budget".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig {
                        reasoning: Some(ReasoningConfig {
                            effort: None,
                            summary: None,
                            budget_tokens: Some(4_096),
                            interleaved: false,
                        }),
                        ..ModelGenerationConfig::default()
                    },
                },
                false,
            )
            .expect_err("OpenAI should reject Anthropic thinking budget");

        assert!(error.message.contains("budget_tokens"));
    }

    #[test]
    fn openai_provider_config_sets_default_pricing_for_supported_models() {
        let mini = OpenAiProviderConfig::new("gpt-5-mini", "test-key");
        assert_eq!(
            mini.pricing,
            Some(OpenAiPricing {
                input_per_million_tokens_usd: 0.25,
                output_per_million_tokens_usd: 2.0,
            })
        );

        let frontier = OpenAiProviderConfig::new("gpt-5.4", "test-key");
        assert_eq!(
            frontier.pricing,
            Some(OpenAiPricing {
                input_per_million_tokens_usd: 2.5,
                output_per_million_tokens_usd: 15.0,
            })
        );

        let unknown = OpenAiProviderConfig::new("gpt-test", "test-key");
        assert!(unknown.pricing.is_none());
    }

    #[test]
    fn xai_request_body_disables_store_when_image_input_is_present() -> Result<()> {
        let temp = create_fixture_dir("xai-images")?;
        let png = write_image_attachment(&temp, "sample.png", "image/png")?;
        let provider = OpenAiProvider::new(
            OpenAiProviderConfig::new("grok-test", "test-key")
                .with_flavor(ResponsesProviderFlavor::XAi),
        )
        .expect("provider should build");
        let body = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-xai-image-store".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt {
                        instructions: Vec::new(),
                        force_synthetic_user_prefix: false,
                        items: vec![ProviderInputItem::Message {
                            id: "msg-user".to_string(),
                            role: Role::User,
                            content: "Describe the image.".to_string(),
                            content_parts: vec![InputContentPart::Attachment { attachment: png }],
                            attachments: Vec::new(),
                            provider_response_id: None,
                            provider_context: None,
                        }],
                    },
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                false,
            )
            .expect("request body should build");

        assert_eq!(body["store"], Value::Bool(false));
        Ok(())
    }

    #[test]
    fn xai_conversation_delta_never_uses_response_resume() -> Result<()> {
        let prompt = NormalizedProviderPrompt {
            instructions: Vec::new(),
            conversation: vec![
                NormalizedConversationItem::UserMessage {
                    id: "user-1".to_string(),
                    content: "Earlier text".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                },
                NormalizedConversationItem::AssistantMessage {
                    id: "assistant-1".to_string(),
                    content: "Earlier reply".to_string(),
                    provider_response_id: Some("resp_123".to_string()),
                    provider_context: None,
                },
                NormalizedConversationItem::UserMessage {
                    id: "user-2".to_string(),
                    content: "Follow up".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                },
            ],
        };

        let (previous_response_id, items) = openai_conversation_delta(
            &prompt,
            ModelRequestKind::MainLoop,
            "grok-4.20-0309-reasoning",
            None,
            &AttachmentRenderCache::default(),
            ResponsesProviderFlavor::XAi,
            false,
        )?;
        assert!(previous_response_id.is_none());
        assert_eq!(items.len(), 3);
        Ok(())
    }

    #[test]
    fn xai_conversation_delta_keeps_image_inputs_without_response_resume() -> Result<()> {
        let temp = create_fixture_dir("xai-image-resume")?;
        let png = write_image_attachment(&temp, "sample.png", "image/png")?;
        let prompt = NormalizedProviderPrompt {
            instructions: Vec::new(),
            conversation: vec![
                NormalizedConversationItem::UserMessage {
                    id: "user-1".to_string(),
                    content: "Earlier text".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                },
                NormalizedConversationItem::AssistantMessage {
                    id: "assistant-1".to_string(),
                    content: "Earlier reply".to_string(),
                    provider_response_id: Some("resp_123".to_string()),
                    provider_context: None,
                },
                NormalizedConversationItem::UserMessage {
                    id: "user-2".to_string(),
                    content: "Describe the image".to_string(),
                    content_parts: vec![InputContentPart::Attachment {
                        attachment: png.clone(),
                    }],
                    attachments: vec![png],
                },
            ],
        };

        let (previous_response_id, items) = openai_conversation_delta(
            &prompt,
            ModelRequestKind::MainLoop,
            "grok-4.20-0309-reasoning",
            None,
            &AttachmentRenderCache::default(),
            ResponsesProviderFlavor::XAi,
            false,
        )?;
        assert!(previous_response_id.is_none());
        assert_eq!(items.len(), 3);
        Ok(())
    }

    #[test]
    fn xai_rejects_structured_outputs_with_tools_for_non_grok_4_models() {
        let provider = OpenAiProvider::new(
            OpenAiProviderConfig::new("grok-3-mini", "test-key")
                .with_flavor(ResponsesProviderFlavor::XAi),
        )
        .expect("provider should build");
        let error = provider
            .build_request_body(
                &ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-xai-structured-tools".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: vec![ToolDefinition {
                        name: "echo".to_string(),
                        description: "Echoes input".to_string(),
                        input_schema: json!({
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"],
                            "additionalProperties": false
                        }),
                        allows_parallel: true,
                    }],
                    generation: ModelGenerationConfig {
                        response_format: ResponseFormat::StructuredJson {
                            schema: StructuredFieldSchema {
                                kind: StructuredValueKind::Object,
                                fields: Default::default(),
                                optional_fields: Default::default(),
                                items: None,
                            },
                        },
                        ..ModelGenerationConfig::default()
                    },
                },
                false,
            )
            .expect_err("non-grok-4 xAI model should reject structured outputs with tools");

        assert!(
            error
                .message
                .contains("does not support structured outputs with tools"),
            "unexpected error: {}",
            error.message
        );
    }

    #[tokio::test]
    async fn openai_provider_parses_streamed_tool_calls() -> Result<()> {
        let response = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_123\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"fc_item_1\",\"type\":\"function_call\",\"status\":\"in_progress\",\"call_id\":\"call-9\",\"name\":\"echo\",\"arguments\":\"\"}}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_item_1\",\"output_index\":0,\"delta\":\"{\\\"text\\\":\\\"ping\\\"}\"}\n\n",
            "event: response.function_call_arguments.done\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_item_1\",\"output_index\":0,\"arguments\":\"{\\\"text\\\":\\\"ping\\\"}\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":7}}}\n\n"
        );
        let url = spawn_mock_server(
            200,
            &[("content-type", "text/event-stream")],
            response,
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let provider = OpenAiProvider::new(OpenAiProviderConfig {
            base_url: url,
            ..OpenAiProviderConfig::new("gpt-test", "test-key")
        })?;

        let (sender, mut receiver) = mpsc::unbounded_channel();
        provider
            .stream(
                ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-2".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                ModelEventSink::new(sender),
            )
            .await?;

        let mut saw_message_id = false;
        let mut saw_tool_call = false;
        let mut saw_stop = false;
        while let Ok(event) = receiver.try_recv() {
            match event {
                ModelStreamEvent::MessageId { value } => {
                    saw_message_id = true;
                    assert_eq!(value, "resp_123");
                }
                ModelStreamEvent::ToolCall { call } => {
                    saw_tool_call = true;
                    assert_eq!(call.id, "call-9");
                    assert_eq!(call.input["text"], "ping");
                }
                ModelStreamEvent::Stop { reason } => {
                    saw_stop = true;
                    assert_eq!(reason, crate::model::ModelFinishReason::ToolCalls);
                }
                _ => {}
            }
        }
        assert!(saw_message_id);
        assert!(saw_tool_call);
        assert!(saw_stop);
        Ok(())
    }

    #[tokio::test]
    async fn openai_provider_keeps_response_id_as_message_id() -> Result<()> {
        let response = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_456\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_456\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_456\",\"output_index\":0,\"content_index\":0,\"delta\":\"done\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
        );
        let url = spawn_mock_server(
            200,
            &[("content-type", "text/event-stream")],
            response,
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let provider = OpenAiProvider::new(OpenAiProviderConfig {
            base_url: url,
            ..OpenAiProviderConfig::new("gpt-test", "test-key")
        })?;

        let (sender, mut receiver) = mpsc::unbounded_channel();
        provider
            .stream(
                ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-response-id".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                ModelEventSink::new(sender),
            )
            .await?;

        let mut message_ids = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            if let ModelStreamEvent::MessageId { value } = event {
                message_ids.push(value);
            }
        }
        assert_eq!(message_ids, vec!["resp_456".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn openai_provider_maps_max_tokens_incomplete_reason() -> Result<()> {
        let response = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg-2\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg-2\",\"output_index\":0,\"content_index\":0,\"delta\":\"partial\"}\n\n",
            "event: response.incomplete\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n"
        );
        let url = spawn_mock_server(
            200,
            &[("content-type", "text/event-stream")],
            response,
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let provider = OpenAiProvider::new(OpenAiProviderConfig {
            base_url: url,
            ..OpenAiProviderConfig::new("gpt-test", "test-key")
        })?;

        let (sender, mut receiver) = mpsc::unbounded_channel();
        provider
            .stream(
                ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-3".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                ModelEventSink::new(sender),
            )
            .await?;

        let mut saw_delta = false;
        let mut saw_stop = false;
        while let Ok(event) = receiver.try_recv() {
            match event {
                ModelStreamEvent::TextDelta { text } => {
                    saw_delta = true;
                    assert_eq!(text, "partial");
                }
                ModelStreamEvent::Stop { reason } => {
                    saw_stop = true;
                    assert_eq!(reason, crate::model::ModelFinishReason::MaxTokens);
                }
                _ => {}
            }
        }
        assert!(saw_delta);
        assert!(saw_stop);
        Ok(())
    }

    #[tokio::test]
    async fn openai_provider_uses_completed_message_content_when_deltas_are_missing() -> Result<()>
    {
        let response = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg-3\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"msg-3\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"{\\\"decision\\\":\\\"block\\\"}\"}]}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":4}}}\n\n"
        );
        let url = spawn_mock_server(
            200,
            &[("content-type", "text/event-stream")],
            response,
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let provider = OpenAiProvider::new(OpenAiProviderConfig {
            base_url: url,
            ..OpenAiProviderConfig::new("gpt-test", "test-key")
        })?;

        let (sender, mut receiver) = mpsc::unbounded_channel();
        provider
            .stream(
                ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-message-done".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                ModelEventSink::new(sender),
            )
            .await?;

        let mut text = String::new();
        while let Ok(event) = receiver.try_recv() {
            if let ModelStreamEvent::TextDelta { text: delta } = event {
                text.push_str(&delta);
            }
        }
        assert_eq!(text, "{\"decision\":\"block\"}");
        Ok(())
    }

    #[tokio::test]
    async fn openai_provider_uses_output_text_done_and_response_done() -> Result<()> {
        let response = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg-4\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "event: response.output_text.done\n",
            "data: {\"type\":\"response.output_text.done\",\"item_id\":\"msg-4\",\"output_index\":0,\"content_index\":0,\"text\":\"HOOK_JSON\"}\n\n",
            "event: response.done\n",
            "data: {\"type\":\"response.done\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n"
        );
        let url = spawn_mock_server(
            200,
            &[("content-type", "text/event-stream")],
            response,
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let provider = OpenAiProvider::new(OpenAiProviderConfig {
            base_url: url,
            ..OpenAiProviderConfig::new("gpt-test", "test-key")
        })?;

        let (sender, mut receiver) = mpsc::unbounded_channel();
        provider
            .stream(
                ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-output-text-done".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                ModelEventSink::new(sender),
            )
            .await?;

        let mut text = String::new();
        let mut saw_stop = false;
        while let Ok(event) = receiver.try_recv() {
            match event {
                ModelStreamEvent::TextDelta { text: delta } => text.push_str(&delta),
                ModelStreamEvent::Stop { reason } => {
                    saw_stop = true;
                    assert_eq!(reason, crate::model::ModelFinishReason::Completed);
                }
                _ => {}
            }
        }
        assert_eq!(text, "HOOK_JSON");
        assert!(saw_stop);
        Ok(())
    }

    #[tokio::test]
    async fn openai_runtime_stays_alive_on_in_progress_events() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept should succeed");
            let mut buffer = [0u8; 8192];
            let _ = socket.read(&mut buffer).await;

            let created = concat!(
                "event: response.created\n",
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-live\"}}\n\n"
            );
            let in_progress = concat!(
                "event: response.in_progress\n",
                "data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp-live\"}}\n\n"
            );
            let done = concat!(
                "event: response.output_item.added\n",
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg-live\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "event: response.output_text.delta\n",
                "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg-live\",\"output_index\":0,\"content_index\":0,\"delta\":\"done\"}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
            );
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:X}\r\n{}\r\n",
                        created.len(),
                        created
                    )
                    .as_bytes(),
                )
                .await
                .expect("initial response should write");
            tokio::time::sleep(Duration::from_millis(60)).await;
            socket
                .write_all(format!("{:X}\r\n{}\r\n", in_progress.len(), in_progress).as_bytes())
                .await
                .expect("in-progress chunk should write");
            tokio::time::sleep(Duration::from_millis(60)).await;
            socket
                .write_all(format!("{:X}\r\n{}\r\n0\r\n\r\n", done.len(), done).as_bytes())
                .await
                .expect("done chunk should write");
        });

        let provider = OpenAiProvider::new(OpenAiProviderConfig {
            base_url: format!("http://{addr}"),
            ..OpenAiProviderConfig::new("gpt-test", "test-key")
        })?;
        let runtime = ModelRuntime::new(
            provider,
            ModelRetryPolicy {
                max_attempts: 1,
                base_backoff_ms: 1,
                stream_timeout_ms: 1_000,
                inactivity_timeout_ms: 100,
            },
            ModelBudget::default(),
            Arc::new(NoopObserver),
        );

        let turn = runtime
            .next_turn(default_model_request("session-openai-activity"))
            .await?;

        assert_eq!(turn.assistant_message.content, "done");
        assert_eq!(
            turn.finish_reason,
            crate::model::ModelFinishReason::Completed
        );
        Ok(())
    }

    #[tokio::test]
    async fn openai_runtime_stays_alive_when_progress_frame_is_split_across_chunks() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept should succeed");
            let mut buffer = [0u8; 8192];
            let _ = socket.read(&mut buffer).await;

            let created = concat!(
                "event: response.created\n",
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-split\"}}\n\n"
            );
            let split_frame = concat!(
                "event: response.in_progress\n",
                "data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp-split\"}}\n\n"
            );
            let split_at = split_frame
                .find("\"response\"")
                .expect("fixture should contain response field");
            let split_first = &split_frame.as_bytes()[..split_at];
            let split_second = &split_frame.as_bytes()[split_at..];
            let done = concat!(
                "event: response.output_item.added\n",
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg-split\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "event: response.output_text.delta\n",
                "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg-split\",\"output_index\":0,\"content_index\":0,\"delta\":\"done\"}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
            );
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:X}\r\n{}\r\n",
                        created.len(),
                        created
                    )
                    .as_bytes(),
                )
                .await
                .expect("initial response should write");
            tokio::time::sleep(Duration::from_millis(70)).await;
            socket
                .write_all(format!("{:X}\r\n", split_first.len()).as_bytes())
                .await
                .expect("split chunk header should write");
            socket
                .write_all(split_first)
                .await
                .expect("split chunk body should write");
            socket
                .write_all(b"\r\n")
                .await
                .expect("split chunk delimiter should write");
            tokio::time::sleep(Duration::from_millis(70)).await;
            socket
                .write_all(format!("{:X}\r\n", split_second.len()).as_bytes())
                .await
                .expect("split second chunk header should write");
            socket
                .write_all(split_second)
                .await
                .expect("split second chunk body should write");
            socket
                .write_all(b"\r\n")
                .await
                .expect("split second chunk delimiter should write");
            tokio::time::sleep(Duration::from_millis(70)).await;
            socket
                .write_all(format!("{:X}\r\n{}\r\n0\r\n\r\n", done.len(), done).as_bytes())
                .await
                .expect("done chunk should write");
        });

        let provider = OpenAiProvider::new(OpenAiProviderConfig {
            base_url: format!("http://{addr}"),
            ..OpenAiProviderConfig::new("gpt-test", "test-key")
        })?;
        let runtime = ModelRuntime::new(
            provider,
            ModelRetryPolicy {
                max_attempts: 1,
                base_backoff_ms: 1,
                stream_timeout_ms: 1_000,
                inactivity_timeout_ms: 100,
            },
            ModelBudget::default(),
            Arc::new(NoopObserver),
        );

        let turn = runtime
            .next_turn(default_model_request("session-openai-split-activity"))
            .await?;

        assert_eq!(turn.assistant_message.content, "done");
        assert_eq!(
            turn.finish_reason,
            crate::model::ModelFinishReason::Completed
        );
        Ok(())
    }

    #[tokio::test]
    async fn openai_provider_handles_utf8_split_across_transport_chunks() -> Result<()> {
        let response = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg-utf8\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg-utf8\",\"output_index\":0,\"content_index\":0,\"delta\":\"café\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n"
        );
        let split_at = response.find("é").expect("fixture should contain é") + 1;
        let url = spawn_chunked_mock_server(
            &[("content-type", "text/event-stream")],
            vec![
                response.as_bytes()[..split_at].to_vec(),
                response.as_bytes()[split_at..].to_vec(),
            ],
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let provider = OpenAiProvider::new(OpenAiProviderConfig {
            base_url: url,
            ..OpenAiProviderConfig::new("gpt-test", "test-key")
        })?;

        let (sender, mut receiver) = mpsc::unbounded_channel();
        provider
            .stream(
                ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-utf8".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                ModelEventSink::new(sender),
            )
            .await?;

        let mut text = String::new();
        while let Ok(event) = receiver.try_recv() {
            if let ModelStreamEvent::TextDelta { text: delta } = event {
                text.push_str(&delta);
            }
        }
        assert_eq!(text, "café");
        Ok(())
    }

    #[tokio::test]
    async fn openai_provider_redacts_sensitive_http_error_messages() -> Result<()> {
        let observer = FixedDebugObserver::shared(DebugCaptureLevel::Full);
        let leaked_secret = format!("{}{}", "sk-", "test-secret-provider-key");
        let url = spawn_mock_server(
            401,
            &[("content-type", "application/json")],
            &format!(
                "{{\"error\":{{\"message\":\"Incorrect API key provided: {leaked_secret}\"}}}}"
            ),
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let provider = OpenAiProvider::with_observer(
            OpenAiProviderConfig {
                base_url: url,
                ..OpenAiProviderConfig::new("gpt-test", "test-key")
            },
            observer.clone(),
        )?;

        let (sender, _) = mpsc::unbounded_channel();
        let error = provider
            .stream(
                ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-http-redaction".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                ModelEventSink::new(sender),
            )
            .await
            .expect_err("HTTP auth failure should bubble up");

        assert!(!error.message.contains(&leaked_secret));
        assert_eq!(error.message, "OpenAI request failed with status 401");

        let provider_response = observer
            .debug_artifacts()
            .into_iter()
            .find(|artifact| artifact.name == "provider-response")
            .expect("provider-response artifact should be recorded");
        let artifact_body = provider_response.payload["body"]["message"]
            .as_str()
            .expect("debug artifact should include a message");
        assert!(!artifact_body.contains(&leaked_secret));
        assert_eq!(artifact_body, "OpenAI request failed with status 401");
        Ok(())
    }

    #[tokio::test]
    async fn openai_provider_redacts_sensitive_stream_error_events() -> Result<()> {
        let observer = FixedDebugObserver::shared(DebugCaptureLevel::Full);
        let leaked_secret = format!("{}{}", "sk-", "test-stream-secret");
        let response = format!(
            concat!(
                "event: response.failed\n",
                "data: {{\"type\":\"response.failed\",\"response\":{{\"error\":{{\"message\":\"bad token {secret}\",\"type\":\"invalid_request_error\",\"code\":\"invalid_api_key\"}}}}}}\n\n"
            ),
            secret = leaked_secret
        );
        let url = spawn_mock_server(
            200,
            &[("content-type", "text/event-stream")],
            &response,
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let provider = OpenAiProvider::with_observer(
            OpenAiProviderConfig {
                base_url: url,
                ..OpenAiProviderConfig::new("gpt-test", "test-key")
            },
            observer.clone(),
        )?;

        let (sender, _) = mpsc::unbounded_channel();
        let error = provider
            .stream(
                ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-sse-redaction".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                ModelEventSink::new(sender),
            )
            .await
            .expect_err("stream failure should bubble up");

        assert!(!error.message.contains(&leaked_secret));
        assert_eq!(
            error.message,
            "OpenAI stream error: type=invalid_request_error, code=invalid_api_key"
        );

        let provider_event = observer
            .debug_artifacts()
            .into_iter()
            .find(|artifact| artifact.name == "provider-events")
            .expect("provider-events artifact should be recorded");
        let payload = provider_event.payload["payload"]["message"]
            .as_str()
            .expect("debug payload should include the error message");
        assert!(!payload.contains(&leaked_secret));
        assert_eq!(
            payload,
            "OpenAI stream error: type=invalid_request_error, code=invalid_api_key"
        );
        Ok(())
    }

    #[tokio::test]
    async fn openai_provider_extracts_nested_stream_error_events() -> Result<()> {
        let observer = FixedDebugObserver::shared(DebugCaptureLevel::Full);
        let leaked_secret = format!("{}{}", "sk-", "test-nested-error-secret");
        let response = format!(
            concat!(
                "event: error\n",
                "data: {{\"type\":\"error\",\"error\":{{\"message\":\"quota exceeded {secret}\",\"type\":\"insufficient_quota\",\"code\":\"insufficient_quota\"}}}}\n\n"
            ),
            secret = leaked_secret
        );
        let url = spawn_mock_server(
            200,
            &[("content-type", "text/event-stream")],
            &response,
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let provider = OpenAiProvider::with_observer(
            OpenAiProviderConfig {
                base_url: url,
                ..OpenAiProviderConfig::new("gpt-test", "test-key")
            },
            observer.clone(),
        )?;

        let (sender, _) = mpsc::unbounded_channel();
        let error = provider
            .stream(
                ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-sse-nested-error".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                ModelEventSink::new(sender),
            )
            .await
            .expect_err("stream failure should bubble up");

        assert!(!error.message.contains(&leaked_secret));
        assert_eq!(
            error.message,
            "OpenAI stream error: type=insufficient_quota, code=insufficient_quota"
        );

        let provider_event = observer
            .debug_artifacts()
            .into_iter()
            .find(|artifact| artifact.name == "provider-events")
            .expect("provider-events artifact should be recorded");
        let payload = provider_event.payload["payload"]["message"]
            .as_str()
            .expect("debug payload should include the error message");
        assert!(!payload.contains(&leaked_secret));
        assert_eq!(
            payload,
            "OpenAI stream error: type=insufficient_quota, code=insufficient_quota"
        );
        Ok(())
    }

    #[tokio::test]
    async fn openai_provider_refreshes_once_after_401() -> Result<()> {
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
                        "Bearer stale-token".to_string(),
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
                        "Bearer fresh-token".to_string(),
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
            for expected_auth in ["Bearer stale-token", "Bearer fresh-token"] {
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
                let (status, body, content_type) = if request_number == 0 {
                    (
                        "401 Unauthorized",
                        "{\"error\":{\"message\":\"expired\"}}".to_string(),
                        "application/json",
                    )
                } else {
                    (
                        "200 OK",
                        concat!(
                            "event: response.output_item.added\n",
                            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg-refresh\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                            "event: response.output_text.delta\n",
                            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg-refresh\",\"output_index\":0,\"content_index\":0,\"delta\":\"done\"}\n\n",
                            "event: response.completed\n",
                            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
                        )
                        .to_string(),
                        "text/event-stream",
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("response write should succeed");
            }
        });

        let provider = OpenAiProvider::new(OpenAiProviderConfig {
            base_url: format!("http://{address}/v1/responses"),
            api_key: None,
            request_auth_provider: Some(Arc::new(FakeAuthProvider {
                counts: counts.clone(),
            })),
            ..OpenAiProviderConfig::new("gpt-test", "unused-key")
        })?;

        let (sender, mut receiver) = mpsc::unbounded_channel();
        provider
            .stream(
                ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-refresh".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                ModelEventSink::new(sender),
            )
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
}
