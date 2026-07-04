use std::collections::BTreeMap;
use std::error::Error as _;
use std::fmt::{Debug, Formatter};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use reqwest::{Client, StatusCode};
use serde::Serialize;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::model::{
    ModelEventSink, ModelFinishReason, ModelProvider, ModelRuntimeRequest, ModelStreamEvent,
    ProviderError, ReasoningConfig, ReasoningEffort, ResponseFormat, ToolChoice,
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
use kheish_types::{
    InputContentPart, ToolResultRecord, capped_default_max_output_tokens, model_max_output_tokens,
};

use super::attachments::{
    AttachmentRenderCache, contains_supported_image_attachment, image_edit_attachment_hint_text,
    load_attachment_preview_image, load_document_attachment_text, load_image_attachment,
};
use super::errors::{safe_error_payload_for_level, sanitize_upstream_error_message};
use super::prompt::{NormalizedConversationItem, normalize_provider_prompt};
use super::sse::{JsonSseEvent, parse_json_sse_frame, pop_sse_frame};

const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1/messages";
const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";
const MIN_ANTHROPIC_THINKING_BUDGET_TOKENS: u32 = 1024;
const ANTHROPIC_INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
/// Token pricing used to estimate request cost from usage snapshots.
#[derive(Clone, Debug, PartialEq)]
pub struct AnthropicPricing {
    pub input_per_million_tokens_usd: f64,
    pub output_per_million_tokens_usd: f64,
}

/// Configuration for the Anthropic provider adapter.
#[derive(Clone)]
pub struct AnthropicProviderConfig {
    pub model: String,
    pub api_key: Option<String>,
    pub request_auth_provider: Option<Arc<dyn RequestAuthProvider>>,
    pub base_url: String,
    pub anthropic_version: String,
    pub beta_headers: Vec<String>,
    pub default_max_output_tokens: u32,
    pub pricing: Option<AnthropicPricing>,
    pub asset_root: Option<PathBuf>,
    pub(crate) attachment_cache: AttachmentRenderCache,
}

impl AnthropicProviderConfig {
    /// Creates a configuration with the standard Anthropic endpoint and version.
    pub fn new(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        let model = model.into();
        Self {
            default_max_output_tokens: capped_default_max_output_tokens(&model),
            model,
            api_key: Some(api_key.into()),
            request_auth_provider: None,
            base_url: DEFAULT_ANTHROPIC_BASE_URL.to_string(),
            anthropic_version: DEFAULT_ANTHROPIC_VERSION.to_string(),
            beta_headers: Vec::new(),
            pricing: None,
            asset_root: None,
            attachment_cache: AttachmentRenderCache::default(),
        }
    }

    /// Loads the API key from an environment variable.
    pub fn from_env(
        model: impl Into<String>,
        env_var: impl AsRef<str>,
    ) -> Result<Self, ProviderError> {
        let env_var = env_var.as_ref();
        let api_key = std::env::var(env_var).map_err(|_| ProviderError {
            message: format!("missing Anthropic API key in environment variable {env_var}"),
            retryable: false,
            retry_after_ms: None,
        })?;
        Ok(Self::new(model, api_key))
    }

    pub fn with_request_auth_provider(
        model: impl Into<String>,
        request_auth_provider: Arc<dyn RequestAuthProvider>,
    ) -> Self {
        let model = model.into();
        Self {
            default_max_output_tokens: capped_default_max_output_tokens(&model),
            model,
            api_key: None,
            request_auth_provider: Some(request_auth_provider),
            base_url: DEFAULT_ANTHROPIC_BASE_URL.to_string(),
            anthropic_version: DEFAULT_ANTHROPIC_VERSION.to_string(),
            beta_headers: Vec::new(),
            pricing: None,
            asset_root: None,
            attachment_cache: AttachmentRenderCache::default(),
        }
    }
}

impl Debug for AnthropicProviderConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProviderConfig")
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field(
                "request_auth_provider",
                &self.request_auth_provider.as_ref().map(|_| "<configured>"),
            )
            .field("base_url", &self.base_url)
            .field("anthropic_version", &self.anthropic_version)
            .field("beta_headers", &self.beta_headers)
            .field("default_max_output_tokens", &self.default_max_output_tokens)
            .field("pricing", &self.pricing)
            .field("asset_root", &self.asset_root)
            .field("attachment_cache", &"<configured>")
            .finish()
    }
}

/// Anthropic streaming provider based on the Messages SSE API.
pub struct AnthropicProvider {
    client: Client,
    config: AnthropicProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

impl AnthropicProvider {
    /// Builds a new Anthropic provider using a dedicated HTTP client.
    pub fn new(config: AnthropicProviderConfig) -> Result<Self, ProviderError> {
        Self::with_observer(config, Arc::new(NoopObserver))
    }

    /// Builds a new Anthropic provider with runtime observation hooks enabled.
    pub fn with_observer(
        config: AnthropicProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        let client = Client::builder().build().map_err(|error| ProviderError {
            message: format!("failed to build Anthropic HTTP client: {error}"),
            retryable: false,
            retry_after_ms: None,
        })?;
        Ok(Self {
            client,
            config,
            observer,
        })
    }

    fn build_request_body(&self, request: &ModelRuntimeRequest) -> Result<Value, ProviderError> {
        let effective_model = request
            .generation
            .model
            .clone()
            .unwrap_or_else(|| self.config.model.clone());
        let (system, messages) = anthropic_prompt_from_items(
            &request.prompt,
            &request.generation,
            &effective_model,
            self.config.asset_root.as_deref(),
            &self.config.attachment_cache,
        )?;
        let default_max_output_tokens = capped_default_max_output_tokens(&effective_model);
        let tools = if matches!(request.generation.tool_choice, ToolChoice::None) {
            Vec::new()
        } else {
            request
                .available_tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.input_schema,
                    })
                })
                .collect()
        };
        let reasoning = request.generation.reasoning.as_ref();
        let adaptive = anthropic_model_uses_adaptive_thinking(&effective_model);
        // Current-generation models reject `budget_tokens` and sampling
        // parameters with HTTP 400; thinking is adaptive-only there and depth
        // is steered through `output_config.effort`.
        let thinking_budget = if adaptive {
            None
        } else {
            anthropic_reasoning_budget(reasoning)?
        };
        let max_tokens = anthropic_max_tokens_for_reasoning(
            &effective_model,
            request.generation.max_output_tokens,
            default_max_output_tokens,
            thinking_budget,
            reasoning
                .map(|reasoning| reasoning.interleaved)
                .unwrap_or(false),
        )?;

        let adaptive_thinking = if adaptive {
            anthropic_adaptive_thinking_value(reasoning)
        } else {
            None
        };
        let thinking_requested = thinking_budget.is_some() || adaptive_thinking.is_some();

        let mut body = serde_json::Map::new();
        body.insert("model".to_string(), Value::String(effective_model));
        body.insert("max_tokens".to_string(), Value::Number(max_tokens.into()));
        if let Some(budget_tokens) = thinking_budget {
            body.insert(
                "thinking".to_string(),
                json!({
                    "type": "enabled",
                    "budget_tokens": budget_tokens,
                }),
            );
        }
        if let Some(thinking) = adaptive_thinking {
            body.insert("thinking".to_string(), thinking);
            if let Some(effort) = reasoning.and_then(anthropic_effort_for_adaptive) {
                body.insert("output_config".to_string(), json!({ "effort": effort }));
            }
        }
        body.insert("stream".to_string(), Value::Bool(true));
        body.insert(
            "messages".to_string(),
            serde_json::to_value(messages).expect("anthropic messages serialize"),
        );
        if !system.is_empty() {
            body.insert("system".to_string(), Value::String(system.join("\n\n")));
        }
        if !tools.is_empty() {
            body.insert("tools".to_string(), Value::Array(tools));
        }
        if thinking_requested
            && matches!(
                request.generation.tool_choice,
                ToolChoice::Required | ToolChoice::Specific { .. }
            )
        {
            return Err(ProviderError {
                message: "Anthropic thinking may not be enabled when tool_choice forces tool use"
                    .to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
        if let Some(tool_choice) = anthropic_tool_choice(
            &request.generation.tool_choice,
            request.generation.allow_parallel_tool_calls,
        ) {
            body.insert(
                "tool_choice".to_string(),
                serde_json::to_value(tool_choice).expect("tool choice serializes"),
            );
        }
        if !adaptive && let Some(temperature) = request.generation.temperature {
            body.insert(
                "temperature".to_string(),
                serde_json::Number::from_f64(temperature as f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
            );
        }
        Ok(Value::Object(body))
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
                message: format!("failed to resolve Anthropic auth material: {error}"),
                retryable: false,
                retry_after_ms: None,
            });
        }
        let api_key = self.config.api_key.clone().ok_or_else(|| ProviderError {
            message: "missing Anthropic API key".to_string(),
            retryable: false,
            retry_after_ms: None,
        })?;
        let mut headers = BTreeMap::new();
        headers.insert("x-api-key".to_string(), api_key);
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
                    message: format!("Anthropic auth material is no longer active: {error}"),
                    retryable: false,
                    retry_after_ms: None,
                })?;
        }
        Ok(())
    }

    fn headers_from_material(
        &self,
        material: &ResolvedAuthMaterial,
        reasoning: Option<&ReasoningConfig>,
    ) -> Result<HeaderMap, ProviderError> {
        let mut headers = HeaderMap::new();
        for (name, value) in &material.headers {
            let header_name =
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                    ProviderError {
                        message: format!("invalid Anthropic header name `{name}`: {error}"),
                        retryable: false,
                        retry_after_ms: None,
                    }
                })?;
            headers.insert(
                header_name,
                HeaderValue::from_str(value).map_err(|error| ProviderError {
                    message: format!("invalid Anthropic header `{name}`: {error}"),
                    retryable: false,
                    retry_after_ms: None,
                })?,
            );
        }
        let mut beta_values = material
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
            .map(|(_, value)| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for beta in &self.config.beta_headers {
            if !beta_values.iter().any(|existing| existing == beta) {
                beta_values.push(beta.clone());
            }
        }
        if reasoning
            .map(|reasoning| reasoning.interleaved)
            .unwrap_or(false)
        {
            if !beta_values
                .iter()
                .any(|existing| existing == ANTHROPIC_INTERLEAVED_THINKING_BETA)
            {
                beta_values.push(ANTHROPIC_INTERLEAVED_THINKING_BETA.to_string());
            }
        }
        headers.insert(
            "anthropic-version",
            HeaderValue::from_str(&self.config.anthropic_version).map_err(|error| {
                ProviderError {
                    message: format!("invalid Anthropic version header: {error}"),
                    retryable: false,
                    retry_after_ms: None,
                }
            })?,
        );
        if !beta_values.is_empty() {
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_str(&beta_values.join(",")).map_err(|error| ProviderError {
                    message: format!("invalid Anthropic beta header: {error}"),
                    retryable: false,
                    retry_after_ms: None,
                })?,
            );
        }
        Ok(headers)
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
        self.observer
            .record_external_action(external_action_trace_with_grant_id(
                "request",
                "model_provider",
                anthropic_external_action_target(endpoint),
                Some(digest_json_value(body).unwrap_or_else(|_| "unknown".to_string())),
                None,
                None,
                grant_id,
            ))
            .map_err(provider_audit_error)?;
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
                "provider": "anthropic",
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
            .map_err(provider_audit_error)?;
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
                "provider": "anthropic",
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

    fn record_provider_event(&self, request: &ModelRuntimeRequest, event: &JsonSseEvent) {
        let level = self.debug_level();
        if !level.is_enabled() {
            return;
        }
        let payload = if event.event_type == "error" {
            safe_error_payload_for_level(level, &anthropic_error_event_payload(event))
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
                "provider": "anthropic",
                "event_type": event.event_type,
                "payload": payload,
            }),
        ));
    }
}

fn anthropic_error_event_payload(event: &JsonSseEvent) -> Value {
    let error = event.payload.get("error");
    let error_type = error
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str);
    json!({
        "message": sanitize_upstream_error_message(
            "Anthropic",
            "stream error",
            None,
            error_type,
            None,
            error
                .and_then(|value| value.get("message"))
                .and_then(Value::as_str),
        ),
        "error_type": error_type,
    })
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    async fn stream(
        &self,
        request: ModelRuntimeRequest,
        sink: ModelEventSink,
    ) -> std::result::Result<(), ProviderError> {
        let body = self.build_request_body(&request)?;
        let mut force_refresh = false;
        let (response, response_target, response_grant_id) = loop {
            let auth_material = self.auth_material(force_refresh).await?;
            let grant_id = auth_material.grant_id.clone();
            let headers =
                self.headers_from_material(&auth_material, request.generation.reasoning.as_ref())?;
            let endpoint = auth_material
                .base_url_override
                .clone()
                .unwrap_or_else(|| self.config.base_url.clone());
            debug!(
                session_id = %request.session_id,
                thread_id = request.thread_id.as_deref(),
                turn = request.turn,
                provider = "anthropic",
                model = request
                    .generation
                    .model
                    .as_deref()
                .unwrap_or(self.config.model.as_str()),
                endpoint = %endpoint,
                tool_count = request.available_tools.len(),
                forced_refresh = force_refresh,
                "starting Anthropic provider request"
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
                    let mapped = map_transport_error(error);
                    self.record_provider_failure(
                        anthropic_external_action_target(&endpoint),
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
                    anthropic_external_action_target(&endpoint),
                    "401-refresh",
                    grant_id,
                )?;
                warn!(
                    session_id = %request.session_id,
                    turn = request.turn,
                    provider = "anthropic",
                    endpoint = %endpoint,
                    "Anthropic provider returned 401, forcing auth refresh"
                );
                force_refresh = true;
                continue;
            }
            break (
                response,
                anthropic_external_action_target(&endpoint),
                grant_id,
            );
        };

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let response_headers = response.headers().clone();
            let error = map_http_error(response).await;
            warn!(
                session_id = %request.session_id,
                thread_id = request.thread_id.as_deref(),
                turn = request.turn,
                provider = "anthropic",
                model = request
                    .generation
                    .model
                    .as_deref()
                    .unwrap_or(self.config.model.as_str()),
                status,
                retryable = error.retryable,
                retry_after_ms = error.retry_after_ms,
                error = %error.message,
                "Anthropic provider request failed"
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
        let mut partial_blocks = BTreeMap::new();
        let mut preserved_context_blocks = Vec::new();
        // Buffer raw bytes until a full SSE frame arrives so split UTF-8 code points
        // across transport chunks do not fail decoding prematurely.
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
                let parsed = parse_json_sse_frame(&frame, "Anthropic").map_err(|error| {
                    self.record_provider_failure(
                        &response_target,
                        &error.message,
                        response_grant_id.clone(),
                    )
                    .err()
                    .unwrap_or(error)
                })?;
                if let Some(event) = parsed {
                    let reached_message_stop = event.event_type == "message_stop";
                    self.record_provider_event(&request, &event);
                    handle_sse_event(
                        event,
                        &mut partial_blocks,
                        &mut preserved_context_blocks,
                        &sink,
                        self.config.pricing.as_ref(),
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
                    if reached_message_stop {
                        return Ok(());
                    }
                }
            }
        }

        if !buffer.iter().all(|byte| byte.is_ascii_whitespace()) {
            let parsed = parse_json_sse_frame(&buffer, "Anthropic").map_err(|error| {
                self.record_provider_failure(
                    &response_target,
                    &error.message,
                    response_grant_id.clone(),
                )
                .err()
                .unwrap_or(error)
            })?;
            if let Some(event) = parsed {
                let reached_message_stop = event.event_type == "message_stop";
                self.record_provider_event(&request, &event);
                handle_sse_event(
                    event,
                    &mut partial_blocks,
                    &mut preserved_context_blocks,
                    &sink,
                    self.config.pricing.as_ref(),
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
                if reached_message_stop {
                    return Ok(());
                }
            }
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
struct AnthropicMessage {
    role: &'static str,
    content: Vec<AnthropicContentBlock>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    Image {
        source: AnthropicImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        is_error: bool,
        content: Vec<AnthropicTextBlock>,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicTextBlock {
    Text { text: String },
}

#[derive(Clone, Debug, Serialize)]
struct AnthropicImageSource {
    #[serde(rename = "type")]
    source_type: &'static str,
    media_type: String,
    data: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicToolChoice {
    Auto {
        disable_parallel_tool_use: bool,
    },
    Any {
        disable_parallel_tool_use: bool,
    },
    Tool {
        name: String,
        disable_parallel_tool_use: bool,
    },
}

#[derive(Clone, Debug)]
enum PartialContentBlock {
    Text,
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
    Ignored,
}

fn anthropic_prompt_from_items(
    prompt: &kheish_types::ProviderPrompt,
    generation: &kheish_types::ModelGenerationConfig,
    effective_model: &str,
    asset_root: Option<&std::path::Path>,
    cache: &AttachmentRenderCache,
) -> Result<(Vec<String>, Vec<AnthropicMessage>), ProviderError> {
    let normalized = normalize_provider_prompt(prompt);
    let mut system = normalized.instructions;
    let mut messages = Vec::new();

    for item in normalized.conversation {
        match item {
            NormalizedConversationItem::UserMessage {
                content,
                content_parts,
                attachments,
                ..
            } => {
                if contains_supported_image_attachment(&content_parts, &attachments)
                    && !anthropic_model_supports_image_input(effective_model)
                {
                    return Err(ProviderError {
                        message: format!(
                            "Anthropic model '{effective_model}' does not support image attachments"
                        ),
                        retryable: false,
                        retry_after_ms: None,
                    });
                }
                for block in anthropic_user_content_blocks(
                    &content,
                    &content_parts,
                    &attachments,
                    asset_root,
                    cache,
                    anthropic_model_supports_image_input(effective_model),
                )
                .map_err(|error| ProviderError {
                    message: format!("failed to prepare user content blocks: {error}"),
                    retryable: false,
                    retry_after_ms: None,
                })? {
                    push_message_block(&mut messages, MessageBuilderKind::UserText, block);
                }
            }
            NormalizedConversationItem::AssistantMessage {
                id,
                content,
                provider_context,
                ..
            } => {
                let kind = MessageBuilderKind::Assistant {
                    message_id: Some(id),
                };
                for block in anthropic_preserved_blocks_from_context(provider_context.as_ref()) {
                    push_message_block(&mut messages, kind.clone(), block);
                }
                if !content.is_empty() {
                    push_message_block(
                        &mut messages,
                        kind,
                        AnthropicContentBlock::Text { text: content },
                    );
                }
            }
            NormalizedConversationItem::AssistantToolCalls {
                assistant_message_id,
                assistant_provider_response_id: _,
                calls,
            } => {
                for call in calls {
                    push_message_block(
                        &mut messages,
                        MessageBuilderKind::Assistant {
                            message_id: assistant_message_id.clone(),
                        },
                        AnthropicContentBlock::ToolUse {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            input: call.input.clone(),
                        },
                    );
                }
            }
            NormalizedConversationItem::ToolResults { results } => {
                for result in results {
                    push_message_block(
                        &mut messages,
                        MessageBuilderKind::UserToolResults,
                        AnthropicContentBlock::ToolResult {
                            tool_use_id: result.call_id.clone(),
                            is_error: result.is_error,
                            content: vec![AnthropicTextBlock::Text {
                                text: render_tool_result_content(&result),
                            }],
                        },
                    );
                }
            }
        }
    }

    if let ResponseFormat::StructuredJson { schema } = &generation.response_format {
        system.push(format!(
            "Return only valid JSON that matches this schema. Do not wrap it in Markdown.\n{}",
            serde_json::to_string_pretty(&schema.to_json_schema())
                .expect("structured schema serializes")
        ));
    }

    let messages = messages
        .into_iter()
        .map(|builder| AnthropicMessage {
            role: builder.role,
            content: builder.content,
        })
        .collect();
    Ok((system, messages))
}

fn anthropic_external_action_target(endpoint: &str) -> String {
    format!("anthropic:{}", safe_url_audit_target(endpoint))
}

fn anthropic_preserved_blocks_from_context(
    provider_context: Option<&Value>,
) -> Vec<AnthropicContentBlock> {
    let Some(blocks) = provider_context
        .and_then(|value| value.get("anthropic"))
        .and_then(|value| value.get("content_blocks"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("thinking") => {
                let thinking = block.get("thinking").and_then(Value::as_str)?;
                let signature = block.get("signature").and_then(Value::as_str)?;
                Some(AnthropicContentBlock::Thinking {
                    thinking: thinking.to_string(),
                    signature: signature.to_string(),
                })
            }
            Some("redacted_thinking") => {
                let data = block.get("data").and_then(Value::as_str)?;
                Some(AnthropicContentBlock::RedactedThinking {
                    data: data.to_string(),
                })
            }
            _ => None,
        })
        .collect()
}

fn anthropic_provider_context_payload(blocks: &[Value]) -> Value {
    json!({
        "anthropic": {
            "content_blocks": blocks,
        }
    })
}

fn provider_audit_error(error: anyhow::Error) -> ProviderError {
    ProviderError {
        message: format!("external action audit failed: {error}"),
        retryable: false,
        retry_after_ms: None,
    }
}

/// Returns whether the model rejects `budget_tokens` and sampling parameters
/// in favor of adaptive thinking (`thinking: {type: "adaptive"}` +
/// `output_config.effort`). One line per model family; extend the list when a
/// new generation ships.
fn anthropic_model_uses_adaptive_thinking(model: &str) -> bool {
    let canonical = model.trim().trim_matches('"').to_ascii_lowercase();
    ["fable-5", "mythos-5", "opus-4-7", "opus-4-8", "sonnet-5"]
        .iter()
        .any(|family| canonical.contains(family))
}

/// Builds the `thinking` value for adaptive-only models. Returns `None` when
/// the request does not ask for reasoning — the parameter is then omitted
/// entirely, which is the only spelling every adaptive model accepts (an
/// explicit `disabled` is rejected on some of them).
fn anthropic_adaptive_thinking_value(reasoning: Option<&ReasoningConfig>) -> Option<Value> {
    let reasoning = reasoning?;
    let wants_thinking = reasoning
        .effort
        .map(|effort| effort != ReasoningEffort::None)
        .unwrap_or(false)
        || reasoning.budget_tokens.is_some()
        || reasoning.interleaved;
    if !wants_thinking {
        return None;
    }
    let mut thinking = serde_json::Map::new();
    thinking.insert("type".to_string(), Value::String("adaptive".to_string()));
    if reasoning
        .summary
        .and_then(|summary| summary.as_provider_str())
        .is_some()
    {
        // Adaptive thinking exposes readable reasoning through the display
        // knob; the closest match for any requested summary style.
        thinking.insert(
            "display".to_string(),
            Value::String("summarized".to_string()),
        );
    }
    Some(Value::Object(thinking))
}

/// Maps the requested reasoning depth onto the adaptive `output_config.effort`
/// scale. Legacy `budget_tokens` configs are folded onto the nearest tier so
/// existing routes keep a comparable thinking depth after a model upgrade.
fn anthropic_effort_for_adaptive(reasoning: &ReasoningConfig) -> Option<&'static str> {
    if let Some(effort) = reasoning.effort {
        return match effort {
            ReasoningEffort::None => None,
            ReasoningEffort::Minimal | ReasoningEffort::Low => Some("low"),
            ReasoningEffort::Medium => Some("medium"),
            ReasoningEffort::High => Some("high"),
            ReasoningEffort::Xhigh => Some("xhigh"),
        };
    }
    let budget = reasoning.budget_tokens?;
    Some(match budget {
        0..=2_048 => "low",
        2_049..=4_096 => "medium",
        4_097..=8_192 => "high",
        _ => "xhigh",
    })
}

fn anthropic_reasoning_budget(
    reasoning: Option<&ReasoningConfig>,
) -> Result<Option<u32>, ProviderError> {
    let Some(reasoning) = reasoning else {
        return Ok(None);
    };
    if reasoning
        .summary
        .and_then(|summary| summary.as_provider_str())
        .is_some()
    {
        return Err(ProviderError {
            message: "Anthropic extended thinking does not support reasoning summaries".to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    let budget = reasoning
        .budget_tokens
        .or_else(|| reasoning.effort.and_then(anthropic_budget_for_effort));
    if reasoning.interleaved && budget.is_none() {
        return Err(ProviderError {
            message: "Anthropic interleaved thinking requires reasoning.effort or budget_tokens"
                .to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if let Some(budget) = budget
        && budget < MIN_ANTHROPIC_THINKING_BUDGET_TOKENS
    {
        return Err(ProviderError {
            message: format!(
                "Anthropic thinking budget_tokens must be at least {MIN_ANTHROPIC_THINKING_BUDGET_TOKENS}"
            ),
            retryable: false,
            retry_after_ms: None,
        });
    }
    Ok(budget)
}

fn anthropic_budget_for_effort(effort: ReasoningEffort) -> Option<u32> {
    match effort {
        ReasoningEffort::None => None,
        ReasoningEffort::Minimal => Some(MIN_ANTHROPIC_THINKING_BUDGET_TOKENS),
        ReasoningEffort::Low => Some(2_048),
        ReasoningEffort::Medium => Some(4_096),
        ReasoningEffort::High => Some(8_192),
        ReasoningEffort::Xhigh => Some(32_768),
    }
}

fn anthropic_max_tokens_for_reasoning(
    model: &str,
    explicit_max_output_tokens: Option<u32>,
    default_max_output_tokens: u32,
    thinking_budget: Option<u32>,
    interleaved: bool,
) -> Result<u32, ProviderError> {
    let requested_max = explicit_max_output_tokens.unwrap_or(default_max_output_tokens);
    let Some(thinking_budget) = thinking_budget else {
        return Ok(requested_max);
    };
    if interleaved {
        return Ok(requested_max);
    }
    if thinking_budget < requested_max {
        return Ok(requested_max);
    }
    if explicit_max_output_tokens.is_some() {
        return Err(ProviderError {
            message: format!(
                "Anthropic thinking budget_tokens ({thinking_budget}) must be lower than max_output_tokens ({requested_max}) unless interleaved thinking is enabled"
            ),
            retryable: false,
            retry_after_ms: None,
        });
    }

    let native_limit = model_max_output_tokens(model).upper_limit;
    let expanded = thinking_budget.saturating_add(1_024);
    if expanded <= native_limit {
        return Ok(expanded);
    }
    Err(ProviderError {
        message: format!(
            "Anthropic thinking budget_tokens ({thinking_budget}) leaves no response budget under the model max_output_tokens limit ({native_limit})"
        ),
        retryable: false,
        retry_after_ms: None,
    })
}

fn anthropic_user_content_blocks(
    fallback_content: &str,
    content_parts: &[InputContentPart],
    attachments: &[kheish_types::AttachmentRef],
    asset_root: Option<&std::path::Path>,
    cache: &AttachmentRenderCache,
    include_document_previews: bool,
) -> anyhow::Result<Vec<AnthropicContentBlock>> {
    fn push_image_attachment_blocks(
        blocks: &mut Vec<AnthropicContentBlock>,
        attachment: &kheish_types::AttachmentRef,
        image: super::attachments::PreparedImageAttachment,
    ) {
        if let Some(text) = image_edit_attachment_hint_text(attachment) {
            blocks.push(AnthropicContentBlock::Text { text });
        }
        blocks.push(AnthropicContentBlock::Image {
            source: AnthropicImageSource {
                source_type: "base64",
                media_type: image.media_type,
                data: image.base64_data,
            },
        });
    }

    let mut blocks = Vec::new();
    if !content_parts.is_empty() {
        for part in content_parts {
            match part {
                InputContentPart::Text { text } if !text.trim().is_empty() => {
                    blocks.push(AnthropicContentBlock::Text { text: text.clone() });
                }
                InputContentPart::Text { .. } => {}
                InputContentPart::Attachment { attachment } => {
                    if let Some(image) = load_image_attachment(attachment, asset_root, cache)? {
                        push_image_attachment_blocks(&mut blocks, attachment, image);
                        continue;
                    }
                    if include_document_previews {
                        if let Some(preview) =
                            load_attachment_preview_image(attachment, asset_root, cache)?
                        {
                            blocks.push(AnthropicContentBlock::Image {
                                source: AnthropicImageSource {
                                    source_type: "base64",
                                    media_type: preview.media_type,
                                    data: preview.base64_data,
                                },
                            });
                        }
                    }
                    if let Some(text) =
                        load_document_attachment_text(attachment, asset_root, cache)?
                    {
                        blocks.push(AnthropicContentBlock::Text { text });
                    }
                }
            }
        }
        return Ok(blocks);
    }

    if !fallback_content.trim().is_empty() {
        blocks.push(AnthropicContentBlock::Text {
            text: fallback_content.to_string(),
        });
    }
    for attachment in attachments {
        if let Some(image) = load_image_attachment(attachment, asset_root, cache)? {
            push_image_attachment_blocks(&mut blocks, attachment, image);
            continue;
        }
        if include_document_previews {
            if let Some(preview) = load_attachment_preview_image(attachment, asset_root, cache)? {
                blocks.push(AnthropicContentBlock::Image {
                    source: AnthropicImageSource {
                        source_type: "base64",
                        media_type: preview.media_type,
                        data: preview.base64_data,
                    },
                });
            }
        }
        if let Some(text) = load_document_attachment_text(attachment, asset_root, cache)? {
            blocks.push(AnthropicContentBlock::Text { text });
        }
    }
    Ok(blocks)
}

fn anthropic_model_supports_image_input(model: &str) -> bool {
    let canonical = model.trim().to_ascii_lowercase();
    canonical.starts_with("claude-")
}

fn anthropic_tool_choice(
    tool_choice: &ToolChoice,
    allow_parallel_tool_calls: bool,
) -> Option<AnthropicToolChoice> {
    let disable_parallel_tool_use = !allow_parallel_tool_calls;
    match tool_choice {
        ToolChoice::Auto => Some(AnthropicToolChoice::Auto {
            disable_parallel_tool_use,
        }),
        ToolChoice::Required => Some(AnthropicToolChoice::Any {
            disable_parallel_tool_use,
        }),
        ToolChoice::Specific { name } => Some(AnthropicToolChoice::Tool {
            name: name.clone(),
            disable_parallel_tool_use,
        }),
        ToolChoice::None => None,
    }
}

#[derive(Clone, Debug)]
struct MessageBuilder {
    role: &'static str,
    kind: MessageBuilderKind,
    content: Vec<AnthropicContentBlock>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MessageBuilderKind {
    UserText,
    UserToolResults,
    Assistant { message_id: Option<String> },
}

fn push_message_block(
    messages: &mut Vec<MessageBuilder>,
    kind: MessageBuilderKind,
    block: AnthropicContentBlock,
) {
    if messages
        .last()
        .map(|message| message.kind == kind)
        .unwrap_or(false)
    {
        if let Some(message) = messages.last_mut() {
            message.content.push(block);
        }
        return;
    }

    messages.push(MessageBuilder {
        role: kind.role(),
        kind,
        content: vec![block],
    });
}

impl MessageBuilderKind {
    fn role(&self) -> &'static str {
        match self {
            Self::UserText | Self::UserToolResults => "user",
            Self::Assistant { .. } => "assistant",
        }
    }
}

fn render_tool_result_content(result: &ToolResultRecord) -> String {
    if let Some(text) = result.output.as_str() {
        return text.to_string();
    }
    serde_json::to_string(&result.output).unwrap_or_else(|_| "{}".to_string())
}

fn handle_sse_event(
    event: JsonSseEvent,
    partial_blocks: &mut BTreeMap<usize, PartialContentBlock>,
    preserved_context_blocks: &mut Vec<Value>,
    sink: &ModelEventSink,
    pricing: Option<&AnthropicPricing>,
) -> Result<(), ProviderError> {
    match event.event_type.as_str() {
        "ping" | "message_stop" => Ok(()),
        "message_start" => {
            if let Some(message) = event.payload.get("message") {
                if let Some(id) = message.get("id").and_then(Value::as_str) {
                    sink.emit(ModelStreamEvent::MessageId {
                        value: id.to_string(),
                    })
                    .map_err(map_sink_error)?;
                }
                if let Some(usage) = message.get("usage") {
                    sink.emit(ModelStreamEvent::Usage {
                        usage: parse_usage(usage, pricing),
                    })
                    .map_err(map_sink_error)?;
                }
            }
            Ok(())
        }
        "content_block_start" => {
            let index = event
                .payload
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| ProviderError {
                    message: "missing content block index".to_string(),
                    retryable: true,
                    retry_after_ms: None,
                })? as usize;
            let block = event
                .payload
                .get("content_block")
                .ok_or_else(|| ProviderError {
                    message: "missing content block payload".to_string(),
                    retryable: true,
                    retry_after_ms: None,
                })?;
            let block_type = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let partial = match block_type {
                "text" => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            sink.emit(ModelStreamEvent::TextDelta {
                                text: text.to_string(),
                            })
                            .map_err(map_sink_error)?;
                        }
                    }
                    PartialContentBlock::Text
                }
                "thinking" => PartialContentBlock::Thinking {
                    thinking: block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    signature: block
                        .get("signature")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                },
                "redacted_thinking" => PartialContentBlock::RedactedThinking {
                    data: block
                        .get("data")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                },
                "tool_use" => PartialContentBlock::ToolUse {
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("tool-use")
                        .to_string(),
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string(),
                    input_json: block
                        .get("input")
                        .filter(|value| !matches!(value, Value::Object(map) if map.is_empty()))
                        .map(|value| serde_json::to_string(value).unwrap_or_default())
                        .unwrap_or_default(),
                },
                _ => PartialContentBlock::Ignored,
            };
            partial_blocks.insert(index, partial);
            Ok(())
        }
        "content_block_delta" => {
            let index = event
                .payload
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| ProviderError {
                    message: "missing content block delta index".to_string(),
                    retryable: true,
                    retry_after_ms: None,
                })? as usize;
            let delta = event.payload.get("delta").ok_or_else(|| ProviderError {
                message: "missing content block delta payload".to_string(),
                retryable: true,
                retry_after_ms: None,
            })?;
            match partial_blocks.get_mut(&index) {
                Some(PartialContentBlock::Text) => {
                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
                        sink.emit(ModelStreamEvent::TextDelta {
                            text: text.to_string(),
                        })
                        .map_err(map_sink_error)?;
                    }
                }
                Some(PartialContentBlock::Thinking {
                    thinking,
                    signature,
                }) => match delta.get("type").and_then(Value::as_str) {
                    Some("thinking_delta") => {
                        if let Some(delta) = delta.get("thinking").and_then(Value::as_str) {
                            thinking.push_str(delta);
                        }
                    }
                    Some("signature_delta") => {
                        if let Some(delta) = delta.get("signature").and_then(Value::as_str) {
                            signature.push_str(delta);
                        }
                    }
                    _ => {}
                },
                Some(PartialContentBlock::ToolUse { input_json, .. }) => {
                    if let Some(partial_json) = delta.get("partial_json").and_then(Value::as_str) {
                        input_json.push_str(partial_json);
                    }
                }
                _ => {}
            }
            Ok(())
        }
        "content_block_stop" => {
            let index = event
                .payload
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| ProviderError {
                    message: "missing content block stop index".to_string(),
                    retryable: true,
                    retry_after_ms: None,
                })? as usize;
            match partial_blocks.remove(&index) {
                Some(PartialContentBlock::ToolUse {
                    id,
                    name,
                    input_json,
                }) => {
                    let input = if input_json.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&input_json).map_err(|error| ProviderError {
                            message: format!("invalid Anthropic tool JSON payload: {error}"),
                            retryable: true,
                            retry_after_ms: None,
                        })?
                    };
                    sink.emit(ModelStreamEvent::ToolCall {
                        call: kheish_types::ToolCallRecord {
                            id,
                            name,
                            input,
                            assistant_message_id: None,
                            assistant_provider_response_id: None,
                        },
                    })
                    .map_err(map_sink_error)?;
                    Ok(())
                }
                Some(PartialContentBlock::Thinking {
                    thinking,
                    signature,
                }) => {
                    if !thinking.is_empty() && !signature.is_empty() {
                        preserved_context_blocks.push(json!({
                            "type": "thinking",
                            "thinking": thinking,
                            "signature": signature,
                        }));
                        sink.emit(ModelStreamEvent::ProviderContext {
                            value: anthropic_provider_context_payload(preserved_context_blocks),
                        })
                        .map_err(map_sink_error)?;
                    }
                    Ok(())
                }
                Some(PartialContentBlock::RedactedThinking { data }) => {
                    if !data.is_empty() {
                        preserved_context_blocks.push(json!({
                            "type": "redacted_thinking",
                            "data": data,
                        }));
                        sink.emit(ModelStreamEvent::ProviderContext {
                            value: anthropic_provider_context_payload(preserved_context_blocks),
                        })
                        .map_err(map_sink_error)?;
                    }
                    Ok(())
                }
                _ => Ok(()),
            }
        }
        "message_delta" => {
            if let Some(delta) = event.payload.get("delta") {
                if let Some(stop_reason) = delta.get("stop_reason").and_then(Value::as_str) {
                    sink.emit(ModelStreamEvent::Stop {
                        reason: map_stop_reason(stop_reason),
                    })
                    .map_err(map_sink_error)?;
                }
            }
            if let Some(usage) = event.payload.get("usage") {
                sink.emit(ModelStreamEvent::Usage {
                    usage: parse_usage(usage, pricing),
                })
                .map_err(map_sink_error)?;
            }
            Ok(())
        }
        "error" => {
            let error_type = event
                .payload
                .pointer("/error/type")
                .and_then(Value::as_str)
                .unwrap_or("provider_error");
            let message = event
                .payload
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Err(ProviderError {
                message: sanitize_upstream_error_message(
                    "Anthropic",
                    "stream error",
                    None,
                    Some(error_type),
                    None,
                    Some(message),
                ),
                retryable: matches!(
                    error_type,
                    "rate_limit_error" | "overloaded_error" | "api_error"
                ),
                retry_after_ms: None,
            })
        }
        _ => Ok(()),
    }
}

fn parse_usage(usage: &Value, pricing: Option<&AnthropicPricing>) -> kheish_types::ModelUsage {
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

fn map_stop_reason(stop_reason: &str) -> ModelFinishReason {
    match stop_reason {
        "end_turn" => ModelFinishReason::Completed,
        "tool_use" => ModelFinishReason::ToolCalls,
        "max_tokens" | "model_context_window_exceeded" => ModelFinishReason::MaxTokens,
        "stop_sequence" => ModelFinishReason::StopSequence,
        "refusal" => ModelFinishReason::Blocked,
        "pause_turn" => ModelFinishReason::Cancelled,
        _ => ModelFinishReason::Unknown,
    }
}

fn map_sink_error(error: anyhow::Error) -> ProviderError {
    ProviderError {
        message: format!("failed to emit Anthropic stream event: {error}"),
        retryable: true,
        retry_after_ms: None,
    }
}

fn map_transport_error(error: reqwest::Error) -> ProviderError {
    let mut message = format!("Anthropic transport error: {error}");
    let mut source = error.source();
    while let Some(current) = source {
        message.push_str(&format!(": {current}"));
        source = current.source();
    }
    ProviderError {
        message,
        retryable: error.is_timeout() || error.is_connect() || error.is_body(),
        retry_after_ms: None,
    }
}

async fn map_http_error(response: reqwest::Response) -> ProviderError {
    let status = response.status();
    let retry_after_ms = retry_after_ms(response.headers());
    let body = response.text().await.unwrap_or_default();
    let payload = serde_json::from_str::<Value>(&body).ok();
    let message = sanitize_upstream_error_message(
        "Anthropic",
        "request failed",
        Some(status),
        payload
            .as_ref()
            .and_then(|value| value.pointer("/error/type"))
            .and_then(Value::as_str),
        None,
        payload
            .as_ref()
            .and_then(|value| value.pointer("/error/message"))
            .and_then(Value::as_str),
    );
    ProviderError {
        message,
        retryable: matches!(
            status,
            StatusCode::TOO_MANY_REQUESTS
                | StatusCode::REQUEST_TIMEOUT
                | StatusCode::INTERNAL_SERVER_ERROR
                | StatusCode::BAD_GATEWAY
                | StatusCode::SERVICE_UNAVAILABLE
                | StatusCode::GATEWAY_TIMEOUT
        ) || status.as_u16() == 529,
        retry_after_ms,
    }
}

fn retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    retry_after_ms_with_now(headers, SystemTime::now())
}

fn retry_after_ms_with_now(headers: &HeaderMap, now: SystemTime) -> Option<u64> {
    let retry_after = headers.get(RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = retry_after.parse::<u64>() {
        return Some(seconds.saturating_mul(1_000));
    }
    let deadline = httpdate::parse_http_date(retry_after).ok()?;
    let millis = deadline.duration_since(now).unwrap_or_default().as_millis();
    Some(millis.min(u128::from(u64::MAX)) as u64)
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;
    use std::time::{Duration as StdDuration, SystemTime};

    use anyhow::Result;
    use async_trait::async_trait;
    use kheish_auth::{RequestAuthProvider, ResolvedAuthMaterial};
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use serde_json::Value;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};

    use super::{ANTHROPIC_INTERLEAVED_THINKING_BETA, AnthropicProvider, AnthropicProviderConfig};
    use crate::model::{
        ModelBudget, ModelEventSink, ModelGenerationConfig, ModelProvider, ModelRetryPolicy,
        ModelRuntime, ModelRuntimeRequest, ModelStreamEvent, ReasoningConfig, ReasoningEffort,
        ReasoningSummary, ResponseFormat, StructuredFieldSchema, StructuredValueKind, ToolChoice,
    };
    use crate::observability::{DebugArtifact, RuntimeObserver, TraceEvent};
    use crate::providers::test_fixtures::{
        create_fixture_dir, write_document_attachment, write_document_attachment_with_preview,
        write_image_attachment,
    };
    use crate::providers::testsupport::{spawn_chunked_mock_server, spawn_mock_server};
    use crate::{DebugCaptureLevel, NoopObserver};
    use kheish_core::ModelDriver;
    use kheish_types::capped_default_max_output_tokens;
    use kheish_types::{
        CAPPED_DEFAULT_MAX_OUTPUT_TOKENS, ConversationKey, InputContentPart, PromptProjection,
        ProviderInputItem, ProviderPrompt, SummaryBlock, ToolCallRecord, ToolDefinition,
        ToolResultRecord,
    };

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

    fn default_model_request(session_id: &str) -> kheish_core::ModelRequest {
        kheish_core::ModelRequest {
            kind: kheish_core::ModelRequestKind::MainLoop,
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
    fn anthropic_provider_config_uses_claude_code_style_default_max_tokens() {
        let config = AnthropicProviderConfig::new("claude-opus-4-6", "test-key");
        assert_eq!(
            config.default_max_output_tokens,
            CAPPED_DEFAULT_MAX_OUTPUT_TOKENS
        );
    }

    #[test]
    fn anthropic_maps_model_context_window_exceeded_to_max_tokens() {
        assert_eq!(
            super::map_stop_reason("model_context_window_exceeded"),
            crate::model::ModelFinishReason::MaxTokens
        );
    }

    #[tokio::test]
    async fn anthropic_provider_encodes_prompt_tools_and_system_text() -> Result<()> {
        let captured = Arc::new(Mutex::new(String::new()));
        let response = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-1\",\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"done\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":12,\"output_tokens\":4}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let url = spawn_mock_server(
            200,
            &[("content-type", "text/event-stream")],
            response,
            captured.clone(),
        )
        .await?;
        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: url,
            ..AnthropicProviderConfig::new("claude-test", "test-key")
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
                                provider_response_id: Some("anthropic-msg-1".to_string()),
                                provider_context: None,
                            },
                            ProviderInputItem::ToolCall {
                                assistant_message_id: Some("assistant-1".to_string()),
                                call: ToolCallRecord {
                                    id: "call-1".to_string(),
                                    name: "echo".to_string(),
                                    input: json!({"text": "ping"}),
                                    assistant_message_id: None,
                                    assistant_provider_response_id: None,
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
                                fields: BTreeMap::new(),
                                optional_fields: BTreeMap::new(),
                                items: None,
                            },
                        },
                    },
                },
                ModelEventSink::new(sender),
            )
            .await?;

        let payload: Value = serde_json::from_str(&captured.lock())?;
        assert_eq!(payload["model"], "claude-test");
        assert_eq!(payload["max_tokens"], 256);
        assert_eq!(payload["tool_choice"]["type"], "tool");
        assert_eq!(payload["tool_choice"]["name"], "echo");
        assert_eq!(payload["tool_choice"]["disable_parallel_tool_use"], true);
        assert_eq!(payload["tools"][0]["name"], "echo");
        let system = payload["system"]
            .as_str()
            .expect("system should be serialized");
        assert!(system.contains("Return only valid JSON"));

        let messages = payload["messages"]
            .as_array()
            .expect("messages should be an array");
        let serialized_messages = serde_json::to_string(messages)?;
        assert!(serialized_messages.contains("Earlier context"));
        assert!(
            messages
                .iter()
                .any(|message| message["role"] == "assistant")
        );
        assert!(serialized_messages.contains("\"tool_use\""));
        assert!(serialized_messages.contains("\"tool_result\""));

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

    fn minimal_request(generation: ModelGenerationConfig) -> ModelRuntimeRequest {
        ModelRuntimeRequest {
            attempt: 1,
            kind: kheish_core::ModelRequestKind::MainLoop,
            session_id: "session-thinking".to_string(),
            thread_id: None,
            turn: 1,
            prompt: ProviderPrompt {
                instructions: Vec::new(),
                force_synthetic_user_prefix: false,
                items: vec![ProviderInputItem::Message {
                    id: "user-1".to_string(),
                    role: kheish_types::Role::User,
                    content: "Hello".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: None,
                    provider_context: None,
                }],
            },
            available_tools: Vec::new(),
            generation,
        }
    }

    #[test]
    fn anthropic_model_capability_table_matches_current_generations() {
        for model in [
            "claude-fable-5",
            "claude-mythos-5",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-sonnet-5",
            "anthropic.claude-opus-4-8",
        ] {
            assert!(
                super::anthropic_model_uses_adaptive_thinking(model),
                "{model} should use adaptive thinking"
            );
        }
        for model in [
            "claude-opus-4-6",
            "claude-opus-4-5",
            "claude-sonnet-4-6",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
        ] {
            assert!(
                !super::anthropic_model_uses_adaptive_thinking(model),
                "{model} should keep the legacy request shape"
            );
        }
    }

    #[test]
    fn adaptive_model_uses_adaptive_thinking_and_omits_sampling_params() -> Result<()> {
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-fable-5", "test-key"))?;
        let body = provider.build_request_body(&minimal_request(ModelGenerationConfig {
            reasoning: Some(ReasoningConfig {
                effort: Some(ReasoningEffort::High),
                ..ReasoningConfig::default()
            }),
            temperature: Some(0.7),
            ..ModelGenerationConfig::default()
        }))?;

        assert_eq!(body["thinking"], json!({ "type": "adaptive" }));
        assert_eq!(body["output_config"], json!({ "effort": "high" }));
        assert!(
            body.get("temperature").is_none(),
            "temperature must be omitted on current models"
        );
        Ok(())
    }

    #[test]
    fn adaptive_model_maps_legacy_budget_tokens_to_an_effort_tier() -> Result<()> {
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-4-8", "test-key"))?;
        let body = provider.build_request_body(&minimal_request(ModelGenerationConfig {
            reasoning: Some(ReasoningConfig {
                budget_tokens: Some(4_096),
                ..ReasoningConfig::default()
            }),
            ..ModelGenerationConfig::default()
        }))?;

        assert_eq!(body["thinking"], json!({ "type": "adaptive" }));
        assert_eq!(body["output_config"], json!({ "effort": "medium" }));
        assert!(body["thinking"].get("budget_tokens").is_none());
        Ok(())
    }

    #[test]
    fn adaptive_model_without_reasoning_omits_thinking_entirely() -> Result<()> {
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-fable-5", "test-key"))?;
        let body = provider.build_request_body(&minimal_request(ModelGenerationConfig {
            temperature: Some(0.2),
            ..ModelGenerationConfig::default()
        }))?;

        assert!(
            body.get("thinking").is_none(),
            "omitting thinking is the only spelling every adaptive model accepts"
        );
        assert!(body.get("output_config").is_none());
        assert!(body.get("temperature").is_none());
        Ok(())
    }

    #[test]
    fn adaptive_model_maps_summary_to_summarized_display() -> Result<()> {
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-sonnet-5", "test-key"))?;
        let body = provider.build_request_body(&minimal_request(ModelGenerationConfig {
            reasoning: Some(ReasoningConfig {
                effort: Some(ReasoningEffort::Medium),
                summary: Some(ReasoningSummary::Auto),
                ..ReasoningConfig::default()
            }),
            ..ModelGenerationConfig::default()
        }))?;

        assert_eq!(
            body["thinking"],
            json!({ "type": "adaptive", "display": "summarized" })
        );
        Ok(())
    }

    #[test]
    fn legacy_model_keeps_budget_tokens_and_temperature() -> Result<()> {
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-4-6", "test-key"))?;
        let body = provider.build_request_body(&minimal_request(ModelGenerationConfig {
            reasoning: Some(ReasoningConfig {
                effort: Some(ReasoningEffort::High),
                ..ReasoningConfig::default()
            }),
            temperature: Some(0.7),
            ..ModelGenerationConfig::default()
        }))?;

        assert_eq!(
            body["thinking"],
            json!({ "type": "enabled", "budget_tokens": 8_192 })
        );
        assert!(body.get("output_config").is_none());
        let temperature = body["temperature"]
            .as_f64()
            .expect("temperature should be present on legacy models");
        assert!((temperature - 0.7).abs() < 1e-6);
        Ok(())
    }

    #[test]
    fn anthropic_request_body_includes_image_attachments() -> Result<()> {
        let temp = create_fixture_dir("anthropic-images")?;
        let png = write_image_attachment(&temp, "sample-a.png", "image/png")?;
        let jpeg = write_image_attachment(&temp, "sample-b.jpg", "image/jpeg")?;
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-4-6", "test-key"))?;

        let body = provider.build_request_body(&ModelRuntimeRequest {
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
        })?;

        let messages = body["messages"]
            .as_array()
            .expect("messages should be serialized");
        let user_message = messages
            .iter()
            .find(|item| item["role"] == "user")
            .expect("user message should be present");
        let content = user_message["content"]
            .as_array()
            .expect("content should be an array");
        assert!(
            content
                .iter()
                .any(|part| part["type"] == "text" && part["text"] == "Inspect these attachments.")
        );
        let asset_hints = content
            .iter()
            .filter(|part| {
                part["type"] == "text"
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
        let image_blocks = content
            .iter()
            .filter(|part| part["type"] == "image")
            .collect::<Vec<_>>();
        assert_eq!(image_blocks.len(), 2);
        assert!(image_blocks.iter().any(|part| {
            part["source"]["media_type"] == "image/png"
                && part["source"]["data"]
                    .as_str()
                    .map(|value| !value.is_empty())
                    .unwrap_or(false)
        }));
        assert!(image_blocks.iter().any(|part| {
            part["source"]["media_type"] == "image/jpeg"
                && part["source"]["data"]
                    .as_str()
                    .map(|value| !value.is_empty())
                    .unwrap_or(false)
        }));
        fs::metadata(temp.join("sample-b.jpg"))?;
        Ok(())
    }

    #[test]
    fn anthropic_request_body_preserves_ordered_input_parts() -> Result<()> {
        let temp = create_fixture_dir("anthropic-ordered-parts")?;
        let png = write_image_attachment(&temp, "ordered.png", "image/png")?;
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-4-6", "test-key"))?;

        let body = provider.build_request_body(&ModelRuntimeRequest {
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
        })?;

        let content = body["messages"][0]["content"]
            .as_array()
            .expect("content should be an array");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Before");
        assert_eq!(content[1]["type"], "text");
        assert!(
            content[1]["text"]
                .as_str()
                .is_some_and(|text| text.contains("fixture-ordered.png"))
        );
        assert_eq!(content[2]["type"], "image");
        assert_eq!(content[3]["type"], "text");
        assert_eq!(content[3]["text"], "After");
        Ok(())
    }

    #[test]
    fn anthropic_allows_document_only_inputs_on_non_vision_models() -> Result<()> {
        let temp = create_fixture_dir("anthropic-doc-only")?;
        let document =
            write_document_attachment(&temp, "brief.md", "text/markdown", "DOCUMENT_ONLY_OK")?;
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("text-only-model", "test-key"))?;

        let body = provider.build_request_body(&ModelRuntimeRequest {
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
        })?;

        let content = body["messages"][0]["content"]
            .as_array()
            .expect("content should be an array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert!(
            content[0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("DOCUMENT_ONLY_OK")
        );
        Ok(())
    }

    #[test]
    fn anthropic_includes_document_preview_images_on_vision_models() -> Result<()> {
        let temp = create_fixture_dir("anthropic-doc-preview")?;
        let document = write_document_attachment_with_preview(
            &temp,
            "plan.dxf",
            "application/dxf",
            "DXF summary",
        )?;
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-4-6", "test-key"))?;

        let body = provider.build_request_body(&ModelRuntimeRequest {
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
        })?;

        let content = body["messages"][0]["content"]
            .as_array()
            .expect("content should be an array");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["type"], "text");
        assert!(
            content[1]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("DXF summary")
        );
        Ok(())
    }

    #[test]
    fn anthropic_request_body_prefers_generation_model_override() {
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-base", "test-key"))
                .expect("provider should build");
        let body = provider
            .build_request_body(&ModelRuntimeRequest {
                attempt: 1,
                kind: kheish_core::ModelRequestKind::MainLoop,
                session_id: "session-override".to_string(),
                thread_id: None,
                turn: 1,
                prompt: ProviderPrompt::default(),
                available_tools: Vec::new(),
                generation: ModelGenerationConfig {
                    model: Some("claude-fallback".to_string()),
                    fallback_model: None,
                    ..ModelGenerationConfig::default()
                },
            })
            .expect("request body should build");
        assert_eq!(body["model"], "claude-fallback");
        assert_eq!(
            body["max_tokens"],
            capped_default_max_output_tokens("claude-fallback")
        );
    }

    #[test]
    fn anthropic_request_body_maps_reasoning_budget_to_thinking() {
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-4-6", "test-key"))
                .expect("provider should build");
        let body = provider
            .build_request_body(&ModelRuntimeRequest {
                attempt: 1,
                kind: kheish_core::ModelRequestKind::MainLoop,
                session_id: "session-anthropic-reasoning".to_string(),
                thread_id: None,
                turn: 1,
                prompt: ProviderPrompt::default(),
                available_tools: Vec::new(),
                generation: ModelGenerationConfig {
                    reasoning: Some(ReasoningConfig {
                        effort: Some(ReasoningEffort::Xhigh),
                        summary: Some(ReasoningSummary::None),
                        budget_tokens: None,
                        interleaved: false,
                    }),
                    ..ModelGenerationConfig::default()
                },
            })
            .expect("request body should build");

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 32_768);
        assert!(body["max_tokens"].as_u64().unwrap_or_default() > 32_768);
    }

    #[test]
    fn anthropic_request_body_allows_thinking_with_tools_when_blocks_are_persisted() {
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-4-6", "test-key"))
                .expect("provider should build");
        let body = provider
            .build_request_body(&ModelRuntimeRequest {
                attempt: 1,
                kind: kheish_core::ModelRequestKind::MainLoop,
                session_id: "session-anthropic-reasoning-tools".to_string(),
                thread_id: None,
                turn: 1,
                prompt: ProviderPrompt::default(),
                available_tools: vec![ToolDefinition {
                    name: "echo".to_string(),
                    description: "Echo".to_string(),
                    input_schema: json!({"type": "object"}),
                    allows_parallel: true,
                }],
                generation: ModelGenerationConfig {
                    reasoning: Some(ReasoningConfig {
                        effort: Some(ReasoningEffort::High),
                        summary: None,
                        budget_tokens: None,
                        interleaved: false,
                    }),
                    ..ModelGenerationConfig::default()
                },
            })
            .expect("thinking with tools should build now that signed blocks are persisted");

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["tools"][0]["name"], "echo");
    }

    #[test]
    fn anthropic_request_body_rejects_forced_tool_choice_with_thinking() {
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-4-6", "test-key"))
                .expect("provider should build");
        let error = provider
            .build_request_body(&ModelRuntimeRequest {
                attempt: 1,
                kind: kheish_core::ModelRequestKind::MainLoop,
                session_id: "session-anthropic-forced-thinking-tools".to_string(),
                thread_id: None,
                turn: 1,
                prompt: ProviderPrompt::default(),
                available_tools: vec![ToolDefinition {
                    name: "echo".to_string(),
                    description: "Echo".to_string(),
                    input_schema: json!({"type": "object"}),
                    allows_parallel: true,
                }],
                generation: ModelGenerationConfig {
                    reasoning: Some(ReasoningConfig {
                        effort: Some(ReasoningEffort::High),
                        summary: None,
                        budget_tokens: None,
                        interleaved: false,
                    }),
                    tool_choice: ToolChoice::Specific {
                        name: "echo".to_string(),
                    },
                    ..ModelGenerationConfig::default()
                },
            })
            .expect_err("Anthropic rejects thinking when tool_choice forces a tool");

        assert!(error.message.contains("tool_choice forces tool use"));
        assert!(!error.retryable);
    }

    #[test]
    fn anthropic_request_body_replays_preserved_thinking_before_tool_results() {
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-4-6", "test-key"))
                .expect("provider should build");
        let body = provider
            .build_request_body(&ModelRuntimeRequest {
                attempt: 1,
                kind: kheish_core::ModelRequestKind::MainLoop,
                session_id: "session-anthropic-thinking-replay".to_string(),
                thread_id: None,
                turn: 2,
                prompt: ProviderPrompt {
                    instructions: Vec::new(),
                    force_synthetic_user_prefix: false,
                    items: vec![
                        ProviderInputItem::Message {
                            id: "user-1".to_string(),
                            role: kheish_types::Role::User,
                            content: "Use the tool.".to_string(),
                            content_parts: Vec::new(),
                            attachments: Vec::new(),
                            provider_response_id: None,
                            provider_context: None,
                        },
                        ProviderInputItem::Message {
                            id: "assistant-1".to_string(),
                            role: kheish_types::Role::Assistant,
                            content: "I will call the tool.".to_string(),
                            content_parts: Vec::new(),
                            attachments: Vec::new(),
                            provider_response_id: Some("msg-thinking".to_string()),
                            provider_context: Some(json!({
                                "anthropic": {
                                    "content_blocks": [
                                        {
                                            "type": "thinking",
                                            "thinking": "I need the echo tool.",
                                            "signature": "sig-thinking",
                                        },
                                        {
                                            "type": "redacted_thinking",
                                            "data": "encrypted-thinking",
                                        }
                                    ],
                                }
                            })),
                        },
                        ProviderInputItem::ToolCall {
                            assistant_message_id: Some("assistant-1".to_string()),
                            call: ToolCallRecord {
                                id: "call-1".to_string(),
                                name: "echo".to_string(),
                                input: json!({"text": "ping"}),
                                assistant_message_id: None,
                                assistant_provider_response_id: Some("msg-thinking".to_string()),
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
                    ],
                },
                available_tools: vec![ToolDefinition {
                    name: "echo".to_string(),
                    description: "Echo".to_string(),
                    input_schema: json!({"type": "object"}),
                    allows_parallel: true,
                }],
                generation: ModelGenerationConfig::default(),
            })
            .expect("request body should build");

        let assistant = body["messages"]
            .as_array()
            .expect("messages should be an array")
            .iter()
            .find(|message| message["role"] == "assistant")
            .expect("assistant message should be present");
        let content = assistant["content"]
            .as_array()
            .expect("assistant content should be an array");
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "I need the echo tool.");
        assert_eq!(content[0]["signature"], "sig-thinking");
        assert_eq!(content[1]["type"], "redacted_thinking");
        assert_eq!(content[1]["data"], "encrypted-thinking");
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[3]["type"], "tool_use");
    }

    #[test]
    fn anthropic_headers_add_interleaved_thinking_beta_when_requested() {
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-4-6", "test-key"))
                .expect("provider should build");
        let headers = provider
            .headers_from_material(
                &ResolvedAuthMaterial {
                    headers: [("x-api-key".to_string(), "token".to_string())]
                        .into_iter()
                        .collect(),
                    base_url_override: None,
                    grant_id: None,
                    lease_id: None,
                },
                Some(&ReasoningConfig {
                    effort: Some(ReasoningEffort::High),
                    summary: None,
                    budget_tokens: None,
                    interleaved: true,
                }),
            )
            .expect("headers should build");

        assert_eq!(
            headers
                .get("anthropic-beta")
                .and_then(|value| value.to_str().ok()),
            Some(ANTHROPIC_INTERLEAVED_THINKING_BETA)
        );
    }

    #[test]
    fn anthropic_headers_merge_auth_oauth_beta_with_runtime_betas() {
        let mut provider_config = AnthropicProviderConfig::new("claude-test", "unused-key");
        provider_config.beta_headers = vec!["files-api-2025-04-14".to_string()];
        let provider = AnthropicProvider::new(provider_config).expect("provider should build");
        let headers = provider
            .headers_from_material(
                &ResolvedAuthMaterial {
                    headers: [
                        ("Authorization".to_string(), "Bearer token".to_string()),
                        ("anthropic-beta".to_string(), "oauth-2025-04-20".to_string()),
                    ]
                    .into_iter()
                    .collect(),
                    base_url_override: None,
                    grant_id: None,
                    lease_id: None,
                },
                None,
            )
            .expect("headers should build");
        assert_eq!(
            headers
                .get("anthropic-beta")
                .and_then(|value| value.to_str().ok()),
            Some("oauth-2025-04-20,files-api-2025-04-14")
        );
    }

    #[tokio::test]
    async fn anthropic_provider_parses_streamed_tool_calls() -> Result<()> {
        let response = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-2\",\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"Calling tool\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-9\",\"name\":\"echo\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"text\\\":\\\"ping\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":3,\"output_tokens\":7}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let url = spawn_mock_server(
            200,
            &[("content-type", "text/event-stream")],
            response,
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: url,
            ..AnthropicProviderConfig::new("claude-test", "test-key")
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

        let mut saw_tool_call = false;
        let mut saw_stop = false;
        while let Ok(event) = receiver.try_recv() {
            match event {
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
        assert!(saw_tool_call);
        assert!(saw_stop);
        Ok(())
    }

    #[tokio::test]
    async fn anthropic_provider_preserves_signed_thinking_blocks_for_tool_replay() -> Result<()> {
        let response = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-thinking\",\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"I should call echo.\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-123\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"sealed\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-9\",\"name\":\"echo\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"text\\\":\\\"ping\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":3,\"output_tokens\":7}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let url = spawn_mock_server(
            200,
            &[("content-type", "text/event-stream")],
            response,
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: url,
            ..AnthropicProviderConfig::new("claude-test", "test-key")
        })?;

        let (sender, mut receiver) = mpsc::unbounded_channel();
        provider
            .stream(
                ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-thinking-tool".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                ModelEventSink::new(sender),
            )
            .await?;

        let mut provider_context = None;
        let mut saw_tool_call = false;
        while let Ok(event) = receiver.try_recv() {
            match event {
                ModelStreamEvent::ProviderContext { value } => provider_context = Some(value),
                ModelStreamEvent::ToolCall { call } => {
                    saw_tool_call = true;
                    assert_eq!(call.id, "call-9");
                    assert_eq!(call.input["text"], "ping");
                }
                _ => {}
            }
        }
        assert!(saw_tool_call);
        let provider_context = provider_context.expect("provider context should be emitted");
        let blocks = provider_context["anthropic"]["content_blocks"]
            .as_array()
            .expect("anthropic content blocks should be present");
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "I should call echo.");
        assert_eq!(blocks[0]["signature"], "sig-123");
        assert_eq!(blocks[1]["type"], "redacted_thinking");
        assert_eq!(blocks[1]["data"], "sealed");
        Ok(())
    }

    #[tokio::test]
    async fn anthropic_provider_handles_utf8_split_across_transport_chunks() -> Result<()> {
        let response = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-utf8\",\"usage\":{\"input_tokens\":2,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"café\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
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
        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: url,
            ..AnthropicProviderConfig::new("claude-test", "test-key")
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
    async fn anthropic_provider_maps_retryable_http_errors() -> Result<()> {
        let url = spawn_mock_server(
            429,
            &[("content-type", "application/json"), ("retry-after", "2")],
            "{\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}",
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: url,
            ..AnthropicProviderConfig::new("claude-test", "test-key")
        })?;
        let (sender, _) = mpsc::unbounded_channel();
        let error = provider
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
            .await
            .expect_err("rate limiting should bubble up");
        assert!(error.retryable);
        assert_eq!(error.retry_after_ms, Some(2_000));
        Ok(())
    }

    #[test]
    fn anthropic_retry_after_accepts_http_date() {
        let now = SystemTime::UNIX_EPOCH + StdDuration::from_secs(1_000);
        let deadline = now + StdDuration::from_secs(90);
        let mut headers = HeaderMap::new();
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_str(&httpdate::fmt_http_date(deadline))
                .expect("date should be a valid header value"),
        );

        assert_eq!(super::retry_after_ms_with_now(&headers, now), Some(90_000));
    }

    #[tokio::test]
    async fn anthropic_provider_maps_http_500_as_retryable() -> Result<()> {
        let url = spawn_mock_server(
            500,
            &[("content-type", "application/json")],
            "{\"error\":{\"type\":\"api_error\",\"message\":\"temporary failure\"}}",
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: url,
            ..AnthropicProviderConfig::new("claude-test", "test-key")
        })?;
        let (sender, _) = mpsc::unbounded_channel();
        let error = provider
            .stream(
                ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-http-500".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                ModelEventSink::new(sender),
            )
            .await
            .expect_err("500 should bubble up");

        assert!(error.retryable);
        assert_eq!(error.retry_after_ms, None);
        Ok(())
    }

    #[tokio::test]
    async fn anthropic_provider_redacts_sensitive_http_error_messages() -> Result<()> {
        let observer = FixedDebugObserver::shared(DebugCaptureLevel::Full);
        let leaked_secret = format!("{}{}", "sk-", "ant-api03-secret-value");
        let url = spawn_mock_server(
            401,
            &[("content-type", "application/json")],
            &format!(
                "{{\"error\":{{\"type\":\"authentication_error\",\"message\":\"bad key {leaked_secret}\"}}}}"
            ),
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let provider = AnthropicProvider::with_observer(
            AnthropicProviderConfig {
                base_url: url,
                ..AnthropicProviderConfig::new("claude-test", "test-key")
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
        assert_eq!(
            error.message,
            "Anthropic request failed with status 401: type=authentication_error"
        );

        let provider_response = observer
            .debug_artifacts()
            .into_iter()
            .find(|artifact| artifact.name == "provider-response")
            .expect("provider-response artifact should be recorded");
        let artifact_body = provider_response.payload["body"]["message"]
            .as_str()
            .expect("debug artifact should include a message");
        assert!(!artifact_body.contains(&leaked_secret));
        assert_eq!(
            artifact_body,
            "Anthropic request failed with status 401: type=authentication_error"
        );
        Ok(())
    }

    #[tokio::test]
    async fn anthropic_provider_redacts_sensitive_stream_error_events() -> Result<()> {
        let observer = FixedDebugObserver::shared(DebugCaptureLevel::Full);
        let leaked_secret = format!("{}{}", "sk-", "ant-api03-stream-secret");
        let response = format!(
            concat!(
                "event: error\n",
                "data: {{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":\"leaked {secret}\"}}}}\n\n"
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
        let provider = AnthropicProvider::with_observer(
            AnthropicProviderConfig {
                base_url: url,
                ..AnthropicProviderConfig::new("claude-test", "test-key")
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
        assert_eq!(error.message, "Anthropic stream error: type=api_error");

        let provider_event = observer
            .debug_artifacts()
            .into_iter()
            .find(|artifact| artifact.name == "provider-events")
            .expect("provider-events artifact should be recorded");
        let payload = provider_event.payload["payload"]["message"]
            .as_str()
            .expect("debug payload should include the error message");
        assert!(!payload.contains(&leaked_secret));
        assert_eq!(payload, "Anthropic stream error: type=api_error");
        Ok(())
    }

    #[tokio::test]
    async fn anthropic_provider_returns_after_message_stop_without_waiting_for_socket_close()
    -> Result<()> {
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

            let body = concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-keepalive\",\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"done\"}}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":12,\"output_tokens\":4}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:X}\r\n{}\r\n",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("response write should succeed");
            tokio::time::sleep(Duration::from_millis(250)).await;
        });

        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: format!("http://{address}"),
            ..AnthropicProviderConfig::new("claude-test", "test-key")
        })?;

        let (sender, mut receiver) = mpsc::unbounded_channel();
        timeout(
            Duration::from_millis(100),
            provider.stream(
                ModelRuntimeRequest {
                    attempt: 1,
                    kind: kheish_core::ModelRequestKind::MainLoop,
                    session_id: "session-4".to_string(),
                    thread_id: None,
                    turn: 1,
                    prompt: ProviderPrompt::default(),
                    available_tools: Vec::new(),
                    generation: ModelGenerationConfig::default(),
                },
                ModelEventSink::new(sender),
            ),
        )
        .await
        .expect("provider should return before the server closes the socket")?;

        let mut saw_stop = false;
        while let Ok(event) = receiver.try_recv() {
            if let ModelStreamEvent::Stop { reason } = event {
                saw_stop = true;
                assert_eq!(reason, crate::model::ModelFinishReason::Completed);
            }
        }
        assert!(saw_stop);
        Ok(())
    }

    #[tokio::test]
    async fn anthropic_runtime_stays_alive_on_ping_events() -> Result<()> {
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

            let started = concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-runtime\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n"
            );
            let ping = concat!("event: ping\n", "data: {\"type\":\"ping\"}\n\n");
            let done = concat!(
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"done\"}}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n"
            );

            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:X}\r\n{}\r\n",
                        started.len(),
                        started
                    )
                    .as_bytes(),
                )
                .await
                .expect("initial response should write");
            tokio::time::sleep(Duration::from_millis(60)).await;
            socket
                .write_all(format!("{:X}\r\n{}\r\n", ping.len(), ping).as_bytes())
                .await
                .expect("ping chunk should write");
            tokio::time::sleep(Duration::from_millis(60)).await;
            socket
                .write_all(format!("{:X}\r\n{}\r\n0\r\n\r\n", done.len(), done).as_bytes())
                .await
                .expect("done chunk should write");
        });

        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: format!("http://{address}"),
            ..AnthropicProviderConfig::new("claude-test", "test-key")
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
            .next_turn(default_model_request("session-anthropic-ping"))
            .await?;

        assert_eq!(turn.assistant_message.content, "done");
        assert_eq!(
            turn.finish_reason,
            crate::model::ModelFinishReason::Completed
        );
        Ok(())
    }

    #[tokio::test]
    async fn anthropic_provider_refreshes_once_after_401() -> Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering};

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
                            "event: message_start\n",
                            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-refresh\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                            "event: content_block_start\n",
                            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"done\"}}\n\n",
                            "event: message_delta\n",
                            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}\n\n",
                            "event: message_stop\n",
                            "data: {\"type\":\"message_stop\"}\n\n"
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

        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: format!("http://{address}/v1/messages"),
            api_key: None,
            request_auth_provider: Some(Arc::new(FakeAuthProvider {
                counts: counts.clone(),
            })),
            ..AnthropicProviderConfig::new("claude-test", "unused-key")
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
