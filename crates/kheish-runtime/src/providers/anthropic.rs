use std::collections::{BTreeMap, BTreeSet};
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
    InputContentPart, ModelRefusal, ModelUsageIteration, RefusalFallbackRouting, RefusalFallbacks,
    ToolCallRecord, ToolResultRecord, capped_default_max_output_tokens, model_max_output_tokens,
};

use super::anthropic_models::{
    AnthropicModelProfile, AnthropicPricing, ThinkingMode, anthropic_accepts_fallback_models,
    anthropic_adaptive_effort, anthropic_list_pricing, anthropic_model_profile,
    anthropic_supports_strict_tools,
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
/// The beta that lets a request set `thinking.block_binding` and adds
/// `input_transformations` to the response.
const ANTHROPIC_THINKING_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";
/// The beta that lets a request name `fallbacks`, in both its forms.
const ANTHROPIC_SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// The most fallback models a request may name.
const MAX_ANTHROPIC_FALLBACK_MODELS: usize = 3;
/// Configuration for the Anthropic provider adapter.
#[derive(Clone)]
pub struct AnthropicProviderConfig {
    pub model: String,
    pub api_key: Option<String>,
    pub request_auth_provider: Option<Arc<dyn RequestAuthProvider>>,
    pub base_url: String,
    pub anthropic_version: String,
    pub beta_headers: Vec<String>,
    /// The models the API retries a declined request on, for the route's
    /// requests; a run's `generation.refusal_fallbacks` overrides it.
    pub refusal_fallbacks: Option<RefusalFallbacks>,
    pub default_max_output_tokens: u32,
    /// Prices of the route's requests, overriding the published list price
    /// of the model a request names. A response served by another model (a
    /// refusal fallback) is priced at that model's list price.
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
            refusal_fallbacks: None,
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
            refusal_fallbacks: None,
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
            .field("refusal_fallbacks", &self.refusal_fallbacks)
            .field("default_max_output_tokens", &self.default_max_output_tokens)
            .field("pricing", &self.pricing)
            .field("asset_root", &self.asset_root)
            .field("attachment_cache", &"<configured>")
            .finish()
    }
}

/// One Messages API request: its body and the beta features the body relies on.
struct AnthropicRequest {
    body: Value,
    betas: Vec<&'static str>,
    /// The model the request names.
    model: String,
}

/// The prompt-cache breakpoint Kheish places (the default 5-minute cache).
fn anthropic_cache_breakpoint() -> Value {
    json!({ "type": "ephemeral" })
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

    #[cfg(test)]
    fn build_request_body(&self, request: &ModelRuntimeRequest) -> Result<Value, ProviderError> {
        Ok(self.build_request(request)?.body)
    }

    fn build_request(
        &self,
        request: &ModelRuntimeRequest,
    ) -> Result<AnthropicRequest, ProviderError> {
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
            anthropic_tools(
                &request.available_tools,
                anthropic_supports_strict_tools(&effective_model),
            )
        };
        let reasoning = request.generation.reasoning.as_ref();
        let profile = anthropic_model_profile(&effective_model);
        let mut betas = Vec::new();
        // Adaptive generations reject `budget_tokens` and sampling parameters
        // with HTTP 400; their depth is steered through `output_config.effort`.
        let thinking_budget = if profile.thinking == ThinkingMode::Budget {
            anthropic_reasoning_budget(reasoning)?
        } else {
            None
        };
        let interleaved = thinking_budget.is_some()
            && reasoning
                .map(|reasoning| reasoning.interleaved)
                .unwrap_or(false);
        if interleaved {
            // Adaptive thinking interleaves on its own, without the beta.
            betas.push(ANTHROPIC_INTERLEAVED_THINKING_BETA);
        }
        let max_tokens = anthropic_max_tokens_for_reasoning(
            &effective_model,
            request.generation.max_output_tokens,
            default_max_output_tokens,
            thinking_budget,
            interleaved,
        )?;
        let mut thinking = anthropic_thinking_request(profile.thinking, reasoning, thinking_budget);
        if profile.preserved_thinking {
            // Preserved thinking binds each thinking block to the conversation
            // before it. Kheish edits that prefix in places (compaction, snip,
            // microcompact, the repair of an orphaned tool call after a
            // restart), and once an edit is in a session every later request
            // replays it, so every request to these models asks the API to
            // drop a block that no longer verifies rather than fail the run.
            // The field is set explicitly because its default differs by
            // account age; the dropped blocks come back in
            // `input_transformations`.
            let mut value = thinking
                .thinking
                .take()
                .unwrap_or_else(|| json!({ "type": "adaptive" }));
            value["block_binding"] = json!({ "prefix_mismatch_behavior": "drop_block" });
            thinking.thinking = Some(value);
            betas.push(ANTHROPIC_THINKING_BINDING_BETA);
        }
        let thinking_on = match profile.thinking {
            ThinkingMode::Budget | ThinkingMode::AdaptiveOnRequest => thinking.thinking.is_some(),
            ThinkingMode::AdaptiveByDefault => true,
        };

        let mut body = serde_json::Map::new();
        body.insert("model".to_string(), Value::String(effective_model.clone()));
        body.insert("max_tokens".to_string(), Value::Number(max_tokens.into()));
        if let Some(value) = thinking.thinking {
            body.insert("thinking".to_string(), value);
        }
        if let Some(effort) = thinking.effort {
            body.insert("output_config".to_string(), json!({ "effort": effort }));
        }
        body.insert("stream".to_string(), Value::Bool(true));
        body.insert(
            "messages".to_string(),
            serde_json::to_value(messages).expect("anthropic messages serialize"),
        );
        // Prompt caching: one breakpoint on the system prompt (it caches the
        // tools rendered before it too), one on the last tool so the tool
        // prefix still hits when the system prompt changes, and automatic
        // caching of the conversation tail. The system prompt and the tools
        // render the same bytes on every turn, so each turn reads the prefix
        // the previous one wrote.
        if !system.is_empty() {
            body.insert(
                "system".to_string(),
                json!([{
                    "type": "text",
                    "text": system.join("\n\n"),
                    "cache_control": anthropic_cache_breakpoint(),
                }]),
            );
        }
        if !tools.is_empty() {
            body.insert("tools".to_string(), Value::Array(tools));
        }
        body.insert("cache_control".to_string(), anthropic_cache_breakpoint());
        if thinking_on
            && matches!(
                request.generation.tool_choice,
                ToolChoice::Required | ToolChoice::Specific { .. }
            )
        {
            let message = if profile.thinking == ThinkingMode::AdaptiveByDefault {
                format!(
                    "Anthropic model {effective_model} always thinks, and thinking may not be enabled when tool_choice forces tool use; use tool_choice auto"
                )
            } else {
                "Anthropic thinking may not be enabled when tool_choice forces tool use".to_string()
            };
            return Err(ProviderError {
                message,
                retryable: false,
                retry_after_ms: None,
            });
        }
        if let Some(fallbacks) = request
            .generation
            .refusal_fallbacks
            .as_ref()
            .or(self.config.refusal_fallbacks.as_ref())
        {
            if let Some(value) = anthropic_fallbacks_value(fallbacks, &effective_model, &profile)? {
                body.insert("fallbacks".to_string(), value);
                betas.push(ANTHROPIC_SERVER_SIDE_FALLBACK_BETA);
            } else {
                debug!(
                    session_id = %request.session_id,
                    model = %effective_model,
                    "Anthropic model takes no server-side refusal fallbacks in this form; sending none"
                );
            }
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
        if profile.thinking == ThinkingMode::Budget
            && let Some(temperature) = request.generation.temperature
        {
            body.insert(
                "temperature".to_string(),
                serde_json::Number::from_f64(temperature as f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
            );
        }
        Ok(AnthropicRequest {
            body: Value::Object(body),
            betas,
            model: effective_model,
        })
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
        request_betas: &[&'static str],
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
        for beta in request_betas {
            if !beta_values.iter().any(|existing| existing == beta) {
                beta_values.push((*beta).to_string());
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
        let AnthropicRequest {
            body,
            betas: request_betas,
            model: requested_model,
        } = self.build_request(&request)?;
        let mut force_refresh = false;
        let (response, response_target, response_grant_id) = loop {
            let auth_material = self.auth_material(force_refresh).await?;
            let grant_id = auth_material.grant_id.clone();
            let mut headers = self.headers_from_material(&auth_material, &request_betas)?;
            super::insert_execution_context_headers(&mut headers, &request.session_id)?;
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
        let mut state =
            AnthropicStreamState::new(&request, requested_model, self.config.pricing.clone());
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
                    handle_sse_event(event, &mut state, &sink).map_err(|error| {
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
                handle_sse_event(event, &mut state, &sink).map_err(|error| {
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

        if let Some(message) = state.unparsed_tool_input.take() {
            return Err(ProviderError {
                message,
                retryable: true,
                retry_after_ms: None,
            });
        }
        state.finish(&sink)?;
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
    /// The point where one model's output gave way to a refusal fallback's.
    /// It stays where it appeared: the API uses its position to validate the
    /// thinking blocks around it.
    Fallback {
        from: Value,
        to: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        trigger: Option<Value>,
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
    // Tool calls by the assistant message that made them, so an answer with a
    // recorded block order can place each tool use where the model wrote it.
    let mut calls_by_message = BTreeMap::<String, Vec<ToolCallRecord>>::new();
    for item in &normalized.conversation {
        if let NormalizedConversationItem::AssistantToolCalls {
            assistant_message_id: Some(message_id),
            calls,
            ..
        } = item
        {
            calls_by_message
                .entry(message_id.clone())
                .or_default()
                .extend(calls.iter().cloned());
        }
    }
    let mut replayed_calls = BTreeSet::<String>::new();

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
                    message_id: Some(id.clone()),
                };
                let (recorded, ordered) = anthropic_recorded_blocks(provider_context.as_ref());
                if ordered {
                    // Replay the answer as the API returned it: thinking blocks
                    // keep their place between the text and the tool uses, so
                    // the conversation before each of them is unchanged.
                    let calls = calls_by_message.get(&id).cloned().unwrap_or_default();
                    for block in recorded {
                        let block = match block {
                            RecordedBlock::Native(block) => block,
                            RecordedBlock::Text(text) if text.trim().is_empty() => continue,
                            RecordedBlock::Text(text) => AnthropicContentBlock::Text { text },
                            RecordedBlock::ToolUse(call_id) => {
                                let Some(call) = calls.iter().find(|call| call.id == call_id)
                                else {
                                    continue;
                                };
                                if !replayed_calls.insert(call.id.clone()) {
                                    continue;
                                }
                                anthropic_tool_use_block(call)
                            }
                        };
                        push_message_block(&mut messages, kind.clone(), block);
                    }
                    // Calls the record does not name follow in their own order.
                    for call in &calls {
                        if replayed_calls.insert(call.id.clone()) {
                            push_message_block(
                                &mut messages,
                                kind.clone(),
                                anthropic_tool_use_block(call),
                            );
                        }
                    }
                } else {
                    for block in recorded {
                        if let RecordedBlock::Native(block) = block {
                            push_message_block(&mut messages, kind.clone(), block);
                        }
                    }
                    if !content.is_empty() {
                        push_message_block(
                            &mut messages,
                            kind,
                            AnthropicContentBlock::Text { text: content },
                        );
                    }
                }
            }
            NormalizedConversationItem::AssistantToolCalls {
                assistant_message_id,
                assistant_provider_response_id: _,
                calls,
            } => {
                for call in calls {
                    if replayed_calls.contains(&call.id) {
                        continue;
                    }
                    push_message_block(
                        &mut messages,
                        MessageBuilderKind::Assistant {
                            message_id: assistant_message_id.clone(),
                        },
                        anthropic_tool_use_block(&call),
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

fn anthropic_tool_use_block(call: &ToolCallRecord) -> AnthropicContentBlock {
    AnthropicContentBlock::ToolUse {
        id: call.id.clone(),
        name: call.name.clone(),
        input: call.input.clone(),
    }
}

fn anthropic_provider_context_payload(blocks: &[Value], input_transformations: &[Value]) -> Value {
    let mut anthropic = serde_json::Map::new();
    anthropic.insert("content_blocks".to_string(), Value::Array(blocks.to_vec()));
    if !input_transformations.is_empty() {
        anthropic.insert(
            "input_transformations".to_string(),
            Value::Array(input_transformations.to_vec()),
        );
    }
    json!({ "anthropic": Value::Object(anthropic) })
}

/// One block of an assistant answer as recorded in its provider context.
enum RecordedBlock {
    /// A block replayed verbatim (thinking, redacted thinking).
    Native(AnthropicContentBlock),
    /// A text block, by value.
    Text(String),
    /// A tool use, by id: its input comes from the turn's tool call.
    ToolUse(String),
}

/// Reads the recorded blocks of an assistant answer. `ordered` is true when
/// the record carries the answer's full block order (text and tool uses
/// included); older records list only the thinking blocks.
fn anthropic_recorded_blocks(provider_context: Option<&Value>) -> (Vec<RecordedBlock>, bool) {
    let Some(blocks) = provider_context
        .and_then(|value| value.get("anthropic"))
        .and_then(|value| value.get("content_blocks"))
        .and_then(Value::as_array)
    else {
        return (Vec::new(), false);
    };
    let mut ordered = false;
    let recorded = blocks
        .iter()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("thinking") => {
                let thinking = block.get("thinking").and_then(Value::as_str)?;
                let signature = block.get("signature").and_then(Value::as_str)?;
                Some(RecordedBlock::Native(AnthropicContentBlock::Thinking {
                    thinking: thinking.to_string(),
                    signature: signature.to_string(),
                }))
            }
            Some("redacted_thinking") => {
                let data = block.get("data").and_then(Value::as_str)?;
                Some(RecordedBlock::Native(
                    AnthropicContentBlock::RedactedThinking {
                        data: data.to_string(),
                    },
                ))
            }
            Some("text") => {
                ordered = true;
                let text = block.get("text").and_then(Value::as_str)?;
                Some(RecordedBlock::Text(text.to_string()))
            }
            Some("tool_use") => {
                ordered = true;
                let id = block.get("id").and_then(Value::as_str)?;
                Some(RecordedBlock::ToolUse(id.to_string()))
            }
            Some("fallback") => {
                ordered = true;
                Some(RecordedBlock::Native(AnthropicContentBlock::Fallback {
                    from: block.get("from")?.clone(),
                    to: block.get("to")?.clone(),
                    trigger: block.get("trigger").cloned(),
                }))
            }
            _ => None,
        })
        .collect();
    (recorded, ordered)
}

fn provider_audit_error(error: anyhow::Error) -> ProviderError {
    ProviderError {
        message: format!("external action audit failed: {error}"),
        retryable: false,
        retry_after_ms: None,
    }
}

/// Encodes the request's `fallbacks`: `"default"` or the list of models,
/// when the requested model takes that form. Returns `None` for a model
/// without server-side fallback (it would reject the field), and for Claude
/// Sonnet 5.5 asked for a model list, which only takes `"default"`.
fn anthropic_fallbacks_value(
    fallbacks: &RefusalFallbacks,
    model: &str,
    profile: &AnthropicModelProfile,
) -> Result<Option<Value>, ProviderError> {
    if !profile.server_side_fallback {
        return Ok(None);
    }
    let invalid = |message: String| ProviderError {
        message,
        retryable: false,
        retry_after_ms: None,
    };
    match fallbacks {
        RefusalFallbacks::Routing(RefusalFallbackRouting::Default) => {
            Ok(Some(Value::String("default".to_string())))
        }
        RefusalFallbacks::Models(models) => {
            if !anthropic_accepts_fallback_models(model) {
                return Ok(None);
            }
            if models.is_empty() || models.len() > MAX_ANTHROPIC_FALLBACK_MODELS {
                return Err(invalid(format!(
                    "refusal_fallbacks names {} models; Anthropic takes 1 to {MAX_ANTHROPIC_FALLBACK_MODELS}",
                    models.len()
                )));
            }
            let mut seen = BTreeSet::new();
            let mut entries = Vec::with_capacity(models.len());
            for entry in models {
                let name = entry.model.trim();
                if name.is_empty() || name == model || !seen.insert(name.to_string()) {
                    return Err(invalid(format!(
                        "refusal_fallbacks must name distinct models other than the requested model `{model}`"
                    )));
                }
                let mut value = serde_json::Map::new();
                value.insert("model".to_string(), Value::String(name.to_string()));
                if let Some(max_tokens) = entry.max_tokens {
                    value.insert("max_tokens".to_string(), Value::Number(max_tokens.into()));
                }
                entries.push(Value::Object(value));
            }
            Ok(Some(Value::Array(entries)))
        }
    }
}

/// The `thinking` and `output_config.effort` values of one request.
struct AnthropicThinkingRequest {
    thinking: Option<Value>,
    effort: Option<&'static str>,
}

/// Builds the thinking fields for a model's thinking mode:
/// - `budget_tokens` on generations that still take a manual budget;
/// - on Claude Opus 4.7 and 4.8, adaptive thinking only when the run asks for
///   reasoning (omitting `thinking` turns it off there);
/// - on the 5.x generations, which think whether or not `thinking` is sent,
///   adaptive thinking with the requested effort whenever the run carries a
///   reasoning config, and nothing otherwise so the model's default applies.
fn anthropic_thinking_request(
    mode: ThinkingMode,
    reasoning: Option<&ReasoningConfig>,
    budget: Option<u32>,
) -> AnthropicThinkingRequest {
    let none = AnthropicThinkingRequest {
        thinking: None,
        effort: None,
    };
    match mode {
        ThinkingMode::Budget => AnthropicThinkingRequest {
            thinking: budget.map(|budget_tokens| {
                json!({
                    "type": "enabled",
                    "budget_tokens": budget_tokens,
                })
            }),
            effort: None,
        },
        ThinkingMode::AdaptiveOnRequest => {
            let Some(reasoning) = reasoning.filter(|reasoning| anthropic_wants_thinking(reasoning))
            else {
                return none;
            };
            AnthropicThinkingRequest {
                thinking: Some(anthropic_adaptive_thinking_value(reasoning)),
                effort: anthropic_adaptive_effort(reasoning, mode),
            }
        }
        ThinkingMode::AdaptiveByDefault => {
            let Some(reasoning) = reasoning.filter(|reasoning| !reasoning.is_empty()) else {
                return none;
            };
            AnthropicThinkingRequest {
                thinking: Some(anthropic_adaptive_thinking_value(reasoning)),
                effort: anthropic_adaptive_effort(reasoning, mode),
            }
        }
    }
}

fn anthropic_wants_thinking(reasoning: &ReasoningConfig) -> bool {
    reasoning
        .effort
        .map(|effort| effort != ReasoningEffort::None)
        .unwrap_or(false)
        || reasoning.budget_tokens.is_some()
        || reasoning.interleaved
}

/// The adaptive `thinking` value: `{"type": "adaptive"}`, with the readable
/// summary display when the run asks for a reasoning summary.
fn anthropic_adaptive_thinking_value(reasoning: &ReasoningConfig) -> Value {
    let mut thinking = serde_json::Map::new();
    thinking.insert("type".to_string(), Value::String("adaptive".to_string()));
    if reasoning
        .summary
        .and_then(|summary| summary.as_provider_str())
        .is_some()
    {
        thinking.insert(
            "display".to_string(),
            Value::String("summarized".to_string()),
        );
    }
    Value::Object(thinking)
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
        // The largest manual budget Kheish asks for; the adaptive ladder's
        // `max` has no budget equivalent.
        ReasoningEffort::Xhigh | ReasoningEffort::Max => Some(32_768),
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

/// What one streamed Messages API answer has delivered so far.
struct AnthropicStreamState {
    partial_blocks: BTreeMap<usize, PartialContentBlock>,
    /// The answer's content blocks in the order the API returned them, kept
    /// so the assistant turn replays as the API expects it back: thinking and
    /// redacted thinking verbatim (a thinking block keeps its signature even
    /// when its text is empty, as under the default `display: "omitted"`),
    /// text by value, tool uses by id.
    layout: Vec<Value>,
    /// Whether the answer carried a block whose bytes and position must be
    /// replayed (thinking, redacted thinking, a fallback marker).
    has_positioned_blocks: bool,
    /// Tool calls, held until the answer is complete: a refusal fallback
    /// withdraws the tool uses of the attempt it replaced.
    tool_calls: Vec<ToolCallRecord>,
    /// Why a tool use's input did not parse. An answer cut off at the output
    /// limit can end inside a tool use: that turn is discarded whole, so the
    /// cut input is only an error when the answer was not cut off.
    unparsed_tool_input: Option<String>,
    /// The `input_transformations` the API reported for the request: thinking
    /// blocks it dropped (or would have, on a request it does not enforce).
    input_transformations: Vec<Value>,
    /// Whether the turn's provider context was emitted.
    finished: bool,
    /// The `usage` object as reported so far: `message_start` carries the
    /// prompt counts, `message_delta` the output count (and, on newer API
    /// versions, the prompt counts again), so the reports are merged, never
    /// replaced.
    usage: serde_json::Map<String, Value>,
    /// The model the request named.
    requested_model: String,
    /// The model producing the message, as `message_start` names it.
    model: String,
    /// The route's price override for the requested model.
    pricing_override: Option<AnthropicPricing>,
    session_id: String,
    turn: usize,
}

impl AnthropicStreamState {
    fn new(
        request: &ModelRuntimeRequest,
        requested_model: String,
        pricing_override: Option<AnthropicPricing>,
    ) -> Self {
        Self {
            partial_blocks: BTreeMap::new(),
            layout: Vec::new(),
            has_positioned_blocks: false,
            tool_calls: Vec::new(),
            unparsed_tool_input: None,
            input_transformations: Vec::new(),
            finished: false,
            usage: serde_json::Map::new(),
            model: requested_model.clone(),
            requested_model,
            pricing_override,
            session_id: request.session_id.clone(),
            turn: request.turn,
        }
    }

    /// Returns the prices of `model` for a request with `prompt_tokens`.
    fn pricing_for(&self, model: &str, prompt_tokens: u64) -> Option<AnthropicPricing> {
        if model == self.requested_model
            && let Some(pricing) = &self.pricing_override
        {
            return Some(pricing.clone());
        }
        anthropic_list_pricing(model, prompt_tokens)
    }

    /// Merges one usage report and emits the turn's usage so far.
    fn merge_usage(&mut self, update: &Value, sink: &ModelEventSink) -> Result<(), ProviderError> {
        if let Some(update) = update.as_object() {
            for (key, value) in update {
                if !value.is_null() {
                    self.usage.insert(key.clone(), value.clone());
                }
            }
        }
        let usage = Value::Object(self.usage.clone());
        let mut parsed = parse_usage(&usage);
        let iterations = usage
            .get("iterations")
            .and_then(Value::as_array)
            .filter(|iterations| !iterations.is_empty());
        if let Some(iterations) = iterations {
            // A refusal fallback ran: the top-level counts are the answering
            // attempt's only, and each attempt bills at its own model's
            // prices; an attempt that names no model ran the requested one.
            parsed.iterations = iterations
                .iter()
                .map(|iteration| {
                    let counts = parse_usage(iteration);
                    let model = iteration
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let priced_model = model.as_deref().unwrap_or(&self.requested_model);
                    let cost_usd = self
                        .pricing_for(priced_model, counts.prompt_tokens())
                        .map(|pricing| pricing.cost_usd(&counts, cache_write_1h_tokens(iteration)))
                        .unwrap_or_default();
                    ModelUsageIteration {
                        kind: iteration
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("message")
                            .to_string(),
                        model,
                        input_tokens: counts.input_tokens,
                        output_tokens: counts.output_tokens,
                        cache_creation_input_tokens: counts.cache_creation_input_tokens,
                        cache_read_input_tokens: counts.cache_read_input_tokens,
                        cost_usd,
                    }
                })
                .collect();
            parsed.cost_usd = parsed
                .iterations
                .iter()
                .map(|iteration| iteration.cost_usd)
                .sum();
        } else if let Some(pricing) = self.pricing_for(&self.model, parsed.prompt_tokens()) {
            parsed.cost_usd = pricing.cost_usd(&parsed, cache_write_1h_tokens(&usage));
        }
        sink.emit(ModelStreamEvent::Usage { usage: parsed })
            .map_err(map_sink_error)
    }

    /// Records the `input_transformations` of a response and logs each
    /// thinking block the API dropped or would have dropped.
    fn record_input_transformations(&mut self, value: Option<&Value>) {
        let Some(entries) = value.and_then(Value::as_array) else {
            return;
        };
        for entry in entries {
            if self.input_transformations.contains(entry) {
                continue;
            }
            let field = |key: &str| {
                entry
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let (transformation, reason, path) = (field("type"), field("reason"), field("path"));
            warn!(
                session_id = %self.session_id,
                turn = self.turn,
                provider = "anthropic",
                transformation = %transformation,
                reason = %reason,
                path = %path,
                "Anthropic reported a replayed thinking block it could not use"
            );
            self.input_transformations.push(entry.clone());
        }
    }

    /// Emits what the turn keeps for its replay once its content is complete.
    /// Idempotent: the stop event and the end of the stream both call it.
    fn finish(&mut self, sink: &ModelEventSink) -> Result<(), ProviderError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        // After a refusal fallback, the attempts before the last marker are
        // not part of the answer beyond their text: their thinking and tool
        // uses are withdrawn (never run, never replayed), their text and the
        // markers stay where they appeared.
        let block_type = |block: &Value| {
            block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        if let Some(boundary) = self
            .layout
            .iter()
            .rposition(|block| block_type(block) == "fallback")
        {
            let mut withdrawn = BTreeSet::new();
            let layout = std::mem::take(&mut self.layout);
            for (index, block) in layout.into_iter().enumerate() {
                let kind = block_type(&block);
                if index >= boundary || kind == "text" || kind == "fallback" {
                    self.layout.push(block);
                } else if kind == "tool_use"
                    && let Some(id) = block.get("id").and_then(Value::as_str)
                {
                    withdrawn.insert(id.to_string());
                }
            }
            self.tool_calls.retain(|call| !withdrawn.contains(&call.id));
        }
        for call in std::mem::take(&mut self.tool_calls) {
            sink.emit(ModelStreamEvent::ToolCall { call })
                .map_err(map_sink_error)?;
        }
        if !self.has_positioned_blocks && self.input_transformations.is_empty() {
            return Ok(());
        }
        sink.emit(ModelStreamEvent::ProviderContext {
            value: anthropic_provider_context_payload(&self.layout, &self.input_transformations),
        })
        .map_err(map_sink_error)
    }
}

fn content_block_index(event: &JsonSseEvent, what: &str) -> Result<usize, ProviderError> {
    event
        .payload
        .get("index")
        .and_then(Value::as_u64)
        .map(|index| index as usize)
        .ok_or_else(|| ProviderError {
            message: format!("missing content block {what}index"),
            retryable: true,
            retry_after_ms: None,
        })
}

fn handle_sse_event(
    event: JsonSseEvent,
    state: &mut AnthropicStreamState,
    sink: &ModelEventSink,
) -> Result<(), ProviderError> {
    match event.event_type.as_str() {
        "ping" => Ok(()),
        "message_stop" => state.finish(sink),
        "message_start" => {
            if let Some(message) = event.payload.get("message") {
                if let Some(id) = message.get("id").and_then(Value::as_str) {
                    sink.emit(ModelStreamEvent::MessageId {
                        value: id.to_string(),
                    })
                    .map_err(map_sink_error)?;
                }
                if let Some(model) = message.get("model").and_then(Value::as_str) {
                    state.model = model.to_string();
                }
                state.record_input_transformations(message.get("input_transformations"));
                if let Some(usage) = message.get("usage") {
                    state.merge_usage(usage, sink)?;
                }
            }
            Ok(())
        }
        "content_block_start" => {
            let index = content_block_index(&event, "")?;
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
                    let text = block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if !text.is_empty() {
                        sink.emit(ModelStreamEvent::TextDelta { text: text.clone() })
                            .map_err(map_sink_error)?;
                    }
                    PartialContentBlock::Text { text }
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
                "fallback" => {
                    // A start and stop pair with no deltas, between the
                    // declined attempt's output and the next model's.
                    state.has_positioned_blocks = true;
                    if let Some(model) = block.pointer("/to/model").and_then(Value::as_str) {
                        state.model = model.to_string();
                    }
                    state.layout.push(block.clone());
                    PartialContentBlock::Ignored
                }
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
            state.partial_blocks.insert(index, partial);
            Ok(())
        }
        "content_block_delta" => {
            let index = content_block_index(&event, "delta ")?;
            let delta = event.payload.get("delta").ok_or_else(|| ProviderError {
                message: "missing content block delta payload".to_string(),
                retryable: true,
                retry_after_ms: None,
            })?;
            match state.partial_blocks.get_mut(&index) {
                Some(PartialContentBlock::Text { text: buffer }) => {
                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
                        buffer.push_str(text);
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
            let index = content_block_index(&event, "stop ")?;
            match state.partial_blocks.remove(&index) {
                Some(PartialContentBlock::Text { text }) => {
                    state.layout.push(json!({ "type": "text", "text": text }));
                }
                Some(PartialContentBlock::ToolUse {
                    id,
                    name,
                    input_json,
                }) => {
                    let input = if input_json.trim().is_empty() {
                        json!({})
                    } else {
                        match serde_json::from_str(&input_json) {
                            Ok(input) => input,
                            Err(error) => {
                                state.unparsed_tool_input =
                                    Some(format!("invalid Anthropic tool JSON payload: {error}"));
                                return Ok(());
                            }
                        }
                    };
                    state.layout.push(json!({ "type": "tool_use", "id": id }));
                    state.tool_calls.push(ToolCallRecord {
                        id,
                        name,
                        input,
                        assistant_message_id: None,
                        assistant_provider_response_id: None,
                    });
                }
                // The signature carries the reasoning: a block whose text is
                // empty (the default display) is kept all the same, since
                // leaving one out of the replay breaks the chain of blocks.
                Some(PartialContentBlock::Thinking {
                    thinking,
                    signature,
                }) => {
                    if !signature.is_empty() {
                        state.has_positioned_blocks = true;
                        state.layout.push(json!({
                            "type": "thinking",
                            "thinking": thinking,
                            "signature": signature,
                        }));
                    }
                }
                Some(PartialContentBlock::RedactedThinking { data }) => {
                    if !data.is_empty() {
                        state.has_positioned_blocks = true;
                        state.layout.push(json!({
                            "type": "redacted_thinking",
                            "data": data,
                        }));
                    }
                }
                Some(PartialContentBlock::Ignored) | None => {}
            }
            Ok(())
        }
        "message_delta" => {
            if let Some(usage) = event.payload.get("usage") {
                state.merge_usage(usage, sink)?;
            }
            state.record_input_transformations(
                event
                    .payload
                    .get("input_transformations")
                    .or_else(|| event.payload.pointer("/delta/input_transformations")),
            );
            if let Some(stop_reason) = event
                .payload
                .pointer("/delta/stop_reason")
                .and_then(Value::as_str)
            {
                if !matches!(stop_reason, "max_tokens" | "model_context_window_exceeded")
                    && let Some(message) = state.unparsed_tool_input.take()
                {
                    return Err(ProviderError {
                        message,
                        retryable: true,
                        retry_after_ms: None,
                    });
                }
                // The runtime settles the turn on the stop event: everything
                // the turn keeps goes out before it.
                state.finish(sink)?;
                if stop_reason == "refusal" {
                    let details = event
                        .payload
                        .pointer("/delta/stop_details")
                        .or_else(|| event.payload.get("stop_details"))
                        .filter(|details| details.is_object());
                    let field = |key: &str| {
                        details
                            .and_then(|details| details.get(key))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    };
                    sink.emit(ModelStreamEvent::Refusal {
                        refusal: ModelRefusal {
                            category: field("category"),
                            explanation: field("explanation"),
                            model: Some(state.model.clone()),
                        },
                    })
                    .map_err(map_sink_error)?;
                }
                sink.emit(ModelStreamEvent::Stop {
                    reason: map_stop_reason(stop_reason),
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

/// Reads the token counts of a Messages API `usage` object (unpriced).
fn parse_usage(usage: &Value) -> kheish_types::ModelUsage {
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or_default();
    kheish_types::ModelUsage {
        input_tokens: count("input_tokens"),
        output_tokens: count("output_tokens"),
        cache_creation_input_tokens: count("cache_creation_input_tokens"),
        cache_read_input_tokens: count("cache_read_input_tokens"),
        cost_usd: 0.0,
        iterations: Vec::new(),
    }
}

/// The part of a usage report's cache writes that went to the 1-hour cache.
fn cache_write_1h_tokens(usage: &Value) -> u64 {
    usage
        .pointer("/cache_creation/ephemeral_1h_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default()
}

/// Encodes the tools sorted by name, so the tool prefix renders the same
/// bytes on every turn whatever order the runtime lists them in, with a
/// prompt-cache breakpoint on the last one. Each `input_schema` goes out as
/// the tool defines it. A tool that asks for strict input gets
/// `strict: true` when the model takes it and its schema is in the subset
/// strict tool use enforces, within the per-request limits (taken in name
/// order, so the same tools always get the same answer).
fn anthropic_tools(tools: &[kheish_types::ToolDefinition], strict_tools: bool) -> Vec<Value> {
    let mut sorted = tools.iter().collect::<Vec<_>>();
    sorted.sort_by(|left, right| left.name.cmp(&right.name));
    let mut budget = AnthropicStrictBudget::default();
    let mut encoded = sorted
        .into_iter()
        .map(|tool| {
            let mut encoded = json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            });
            if strict_tools
                && tool.strict
                && anthropic_strict_schema_cost(&tool.input_schema)
                    .is_some_and(|cost| budget.admit(cost))
            {
                encoded["strict"] = Value::Bool(true);
            }
            encoded
        })
        .collect::<Vec<_>>();
    if let Some(Value::Object(last)) = encoded.last_mut() {
        last.insert("cache_control".to_string(), anthropic_cache_breakpoint());
    }
    encoded
}

/// The schema complexity limits of strict tool use, per request: the strict
/// tools, and the optional parameters and the parameters with a union type
/// (`anyOf` or a list of types) across all their schemas. A request past
/// them is refused as too complex to compile.
const ANTHROPIC_MAX_STRICT_TOOLS: usize = 20;
const ANTHROPIC_MAX_STRICT_OPTIONAL_PARAMETERS: usize = 24;
const ANTHROPIC_MAX_STRICT_UNION_PARAMETERS: usize = 16;

/// The string formats strict tool use enforces.
const ANTHROPIC_STRICT_STRING_FORMATS: &[&str] = &[
    "date-time",
    "time",
    "date",
    "duration",
    "email",
    "hostname",
    "uri",
    "ipv4",
    "ipv6",
    "uuid",
];

/// The JSON Schema keywords strict tool use enforces. Any other keyword
/// (`minLength`, `maxLength`, `minimum`, `maximum`, `pattern`, `default`,
/// `oneOf`, ...) leaves the tool non-strict, with its schema unchanged.
const ANTHROPIC_STRICT_KEYWORDS: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "anyOf",
    "allOf",
    "description",
    "title",
    "format",
    "minItems",
    "$ref",
    "$defs",
    "definitions",
];

const ANTHROPIC_STRICT_TYPES: &[&str] = &[
    "object", "array", "string", "integer", "number", "boolean", "null",
];

/// What one strict schema costs of the per-request limits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AnthropicStrictCost {
    optional_parameters: usize,
    union_parameters: usize,
}

#[derive(Debug, Default)]
struct AnthropicStrictBudget {
    tools: usize,
    optional_parameters: usize,
    union_parameters: usize,
}

impl AnthropicStrictBudget {
    /// Takes one more strict tool when it fits the limits.
    fn admit(&mut self, cost: AnthropicStrictCost) -> bool {
        let fits = self.tools < ANTHROPIC_MAX_STRICT_TOOLS
            && self.optional_parameters + cost.optional_parameters
                <= ANTHROPIC_MAX_STRICT_OPTIONAL_PARAMETERS
            && self.union_parameters + cost.union_parameters
                <= ANTHROPIC_MAX_STRICT_UNION_PARAMETERS;
        if fits {
            self.tools += 1;
            self.optional_parameters += cost.optional_parameters;
            self.union_parameters += cost.union_parameters;
        }
        fits
    }
}

/// Returns what a tool schema costs under strict tool use, or `None` when
/// strict tool use cannot enforce it: a keyword outside the enforced subset,
/// an object that does not set `additionalProperties: false`, a `minItems`
/// other than 0 or 1, an unsupported string format, a `$ref` that is not a
/// local definition, or a recursive definition.
fn anthropic_strict_schema_cost(schema: &Value) -> Option<AnthropicStrictCost> {
    let definitions = schema
        .get("$defs")
        .or_else(|| schema.get("definitions"))
        .and_then(Value::as_object);
    let mut cost = AnthropicStrictCost::default();
    anthropic_strict_node_cost(schema, definitions, &mut Vec::new(), &mut cost)?;
    Some(cost)
}

fn anthropic_strict_node_cost(
    node: &Value,
    definitions: Option<&serde_json::Map<String, Value>>,
    resolving: &mut Vec<String>,
    cost: &mut AnthropicStrictCost,
) -> Option<()> {
    let object = node.as_object()?;
    if object
        .keys()
        .any(|keyword| !ANTHROPIC_STRICT_KEYWORDS.contains(&keyword.as_str()))
    {
        return None;
    }
    // A node that constrains nothing (`{}`) is not something strict tool use
    // compiles: every node names a type, a value set, variants or a reference.
    if !["type", "enum", "const", "anyOf", "allOf", "$ref"]
        .iter()
        .any(|keyword| object.contains_key(*keyword))
    {
        return None;
    }
    if let Some(reference) = object.get("$ref") {
        let name = reference
            .as_str()?
            .strip_prefix("#/$defs/")
            .or_else(|| reference.as_str()?.strip_prefix("#/definitions/"))?;
        if resolving.iter().any(|seen| seen == name) {
            return None;
        }
        let target = definitions?.get(name)?;
        resolving.push(name.to_string());
        anthropic_strict_node_cost(target, definitions, resolving, cost)?;
        resolving.pop();
    }
    let types = match object.get("type") {
        None => Vec::new(),
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().map(Value::as_str).collect::<Option<_>>()?,
        Some(_) => return None,
    };
    if types
        .iter()
        .any(|kind| !ANTHROPIC_STRICT_TYPES.contains(kind))
    {
        return None;
    }
    if let Some(format) = object.get("format")
        && !ANTHROPIC_STRICT_STRING_FORMATS.contains(&format.as_str()?)
    {
        return None;
    }
    if let Some(min_items) = object.get("minItems")
        && !matches!(min_items.as_u64(), Some(0 | 1))
    {
        return None;
    }
    let is_object = types.contains(&"object") || object.contains_key("properties");
    if is_object {
        if object.get("additionalProperties") != Some(&Value::Bool(false)) {
            return None;
        }
        let required = match object.get("required") {
            None => Vec::new(),
            Some(required) => required
                .as_array()?
                .iter()
                .map(Value::as_str)
                .collect::<Option<Vec<_>>>()?,
        };
        if let Some(properties) = object.get("properties") {
            for (name, property) in properties.as_object()? {
                if !required.contains(&name.as_str()) {
                    cost.optional_parameters += 1;
                }
                let union = property.get("anyOf").is_some()
                    || property.get("type").is_some_and(Value::is_array);
                if union {
                    cost.union_parameters += 1;
                }
                anthropic_strict_node_cost(property, definitions, resolving, cost)?;
            }
        }
    } else if object.contains_key("additionalProperties") {
        return None;
    }
    if let Some(items) = object.get("items") {
        anthropic_strict_node_cost(items, definitions, resolving, cost)?;
    }
    for keyword in ["anyOf", "allOf"] {
        if let Some(variants) = object.get(keyword) {
            for variant in variants.as_array()? {
                anthropic_strict_node_cost(variant, definitions, resolving, cost)?;
            }
        }
    }
    for keyword in ["$defs", "definitions"] {
        if let Some(defs) = object.get(keyword) {
            for (name, definition) in defs.as_object()? {
                if resolving.iter().any(|seen| seen == name) {
                    continue;
                }
                resolving.push(name.clone());
                anthropic_strict_node_cost(
                    definition,
                    definitions,
                    resolving,
                    &mut AnthropicStrictCost::default(),
                )?;
                resolving.pop();
            }
        }
    }
    Some(())
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
    use crate::execution::{ExecutionScope, scope_execution};
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
    use crate::providers::testsupport::{
        MockHttpResponse, spawn_chunked_mock_server, spawn_mock_server, spawn_recording_mock_server,
    };
    use crate::{DebugCaptureLevel, NoopObserver};
    use kheish_core::ModelDriver;
    use kheish_types::capped_default_max_output_tokens;
    use kheish_types::{
        CAPPED_DEFAULT_MAX_OUTPUT_TOKENS, ConversationKey, InputContentPart, ModelRefusal,
        PromptProjection, ProviderInputItem, ProviderPrompt, RefusalFallbackModel,
        RefusalFallbacks, SummaryBlock, ToolCallRecord, ToolDefinition, ToolResultRecord,
    };
    use tokio_util::sync::CancellationToken;

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
                        strict: false,
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
                        refusal_fallbacks: None,
                        retry_max_tokens: None,
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
        let system = payload["system"][0]["text"]
            .as_str()
            .expect("system should be serialized as one text block");
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
    fn every_current_family_gets_its_documented_thinking_shape() -> Result<()> {
        let generation = ModelGenerationConfig {
            reasoning: Some(ReasoningConfig {
                effort: Some(ReasoningEffort::High),
                ..ReasoningConfig::default()
            }),
            temperature: Some(0.5),
            ..ModelGenerationConfig::default()
        };
        let adaptive = json!({ "type": "adaptive" });
        let budget = json!({ "type": "enabled", "budget_tokens": 8_192 });
        for (model, thinking, effort) in [
            ("claude-opus-5-5", &adaptive, Some("high")),
            ("anthropic.claude-opus-5-5", &adaptive, Some("high")),
            ("claude-opus-5", &adaptive, Some("high")),
            ("claude-fable-5-1", &adaptive, Some("high")),
            ("claude-fable-5", &adaptive, Some("high")),
            ("claude-mythos-5-1", &adaptive, Some("high")),
            ("claude-sonnet-5-5", &adaptive, Some("high")),
            (
                "us.anthropic.claude-sonnet-5-5-v1:0",
                &adaptive,
                Some("high"),
            ),
            ("claude-sonnet-5", &adaptive, Some("high")),
            ("claude-haiku-5-5", &adaptive, Some("high")),
            ("anthropic.claude-haiku-5-5", &adaptive, Some("high")),
            ("claude-opus-4-8", &adaptive, Some("high")),
            ("anthropic.claude-opus-4-8", &adaptive, Some("high")),
            ("claude-opus-4-7", &adaptive, Some("high")),
            ("claude-opus-4-6", &budget, None),
            ("claude-sonnet-4-6", &budget, None),
            ("claude-sonnet-4-5-20250929", &budget, None),
            ("claude-haiku-4-5-20251001", &budget, None),
        ] {
            let provider = AnthropicProvider::new(AnthropicProviderConfig::new(model, "key"))?;
            let body = provider.build_request_body(&minimal_request(generation.clone()))?;
            // The conversation binding control is checked on its own below.
            let mut sent = body["thinking"].clone();
            if let Some(sent) = sent.as_object_mut() {
                sent.remove("block_binding");
            }
            assert_eq!(&sent, thinking, "{model} thinking");
            assert_eq!(
                body.get("output_config")
                    .map(|value| value["effort"].clone()),
                effort.map(|effort| json!(effort)),
                "{model} effort"
            );
            // Sampling parameters are a 400 wherever thinking is adaptive-only.
            assert_eq!(
                body.get("temperature").is_some(),
                effort.is_none(),
                "{model} temperature"
            );
        }
        Ok(())
    }

    #[test]
    fn claude_opus_5_5_is_never_sent_budget_tokens() -> Result<()> {
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-5-5", "test-key"))?;
        for (reasoning, effort) in [
            (
                ReasoningConfig {
                    budget_tokens: Some(4_096),
                    ..ReasoningConfig::default()
                },
                "medium",
            ),
            (
                ReasoningConfig {
                    effort: Some(ReasoningEffort::Max),
                    interleaved: true,
                    ..ReasoningConfig::default()
                },
                "max",
            ),
            // Thinking cannot be turned off on Claude Opus 5.5: `none` asks for
            // the least of it.
            (
                ReasoningConfig {
                    effort: Some(ReasoningEffort::None),
                    ..ReasoningConfig::default()
                },
                "low",
            ),
        ] {
            let request = provider.build_request(&minimal_request(ModelGenerationConfig {
                reasoning: Some(reasoning),
                ..ModelGenerationConfig::default()
            }))?;
            assert_eq!(request.body["thinking"]["type"], "adaptive");
            assert_eq!(request.body["output_config"], json!({ "effort": effort }));
            assert!(!request.body.to_string().contains("budget_tokens"));
            assert!(!request.betas.contains(&ANTHROPIC_INTERLEAVED_THINKING_BETA));
        }
        Ok(())
    }

    #[test]
    fn opus_4_8_thinks_only_when_asked() -> Result<()> {
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-4-8", "test-key"))?;
        let body = provider.build_request_body(&minimal_request(ModelGenerationConfig {
            reasoning: Some(ReasoningConfig {
                effort: Some(ReasoningEffort::None),
                ..ReasoningConfig::default()
            }),
            ..ModelGenerationConfig::default()
        }))?;
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
        Ok(())
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
                    strict: false,
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
                    strict: false,
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
                    strict: false,
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
        let interleaved = ModelGenerationConfig {
            reasoning: Some(ReasoningConfig {
                effort: Some(ReasoningEffort::High),
                summary: None,
                budget_tokens: None,
                interleaved: true,
            }),
            ..ModelGenerationConfig::default()
        };
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-4-6", "test-key"))
                .expect("provider should build");
        let request = provider
            .build_request(&minimal_request(interleaved.clone()))
            .expect("request should build");
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
                &request.betas,
            )
            .expect("headers should build");

        assert_eq!(
            headers
                .get("anthropic-beta")
                .and_then(|value| value.to_str().ok()),
            Some(ANTHROPIC_INTERLEAVED_THINKING_BETA)
        );

        // Adaptive thinking interleaves without the beta.
        let adaptive =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-4-8", "test-key"))
                .expect("provider should build");
        let request = adaptive
            .build_request(&minimal_request(interleaved))
            .expect("request should build");
        assert!(request.betas.is_empty());
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
                &[],
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

    /// A complete Messages SSE stream that answers `text` and ends the turn.
    fn sse_text_stream(message_id: &str, text: &str) -> String {
        format!(
            concat!(
                "event: message_start\n",
                "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"{id}\",\"model\":\"claude-opus-5-5\",\"usage\":{{\"input_tokens\":5,\"output_tokens\":0}}}}}}\n\n",
                "event: content_block_start\n",
                "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n",
                "event: content_block_delta\n",
                "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{text}}}}}\n\n",
                "event: content_block_stop\n",
                "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                "event: message_delta\n",
                "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\",\"stop_details\":null}},\"usage\":{{\"output_tokens\":3}}}}\n\n",
                "event: message_stop\n",
                "data: {{\"type\":\"message_stop\"}}\n\n"
            ),
            id = message_id,
            text = serde_json::to_string(text).expect("text serializes"),
        )
    }

    fn runtime_request(session_id: &str, generation: ModelGenerationConfig) -> ModelRuntimeRequest {
        ModelRuntimeRequest {
            attempt: 1,
            kind: kheish_core::ModelRequestKind::MainLoop,
            session_id: session_id.to_string(),
            thread_id: None,
            turn: 1,
            prompt: ProviderPrompt {
                instructions: vec!["You verify exposures.".to_string()],
                force_synthetic_user_prefix: false,
                items: vec![ProviderInputItem::Message {
                    id: "u1".to_string(),
                    role: kheish_types::Role::User,
                    content: "Say hello".to_string(),
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

    #[tokio::test]
    async fn anthropic_requests_carry_the_execution_scope_session_and_run() -> Result<()> {
        let (url, recorded) = spawn_recording_mock_server(vec![
            MockHttpResponse::sse(&sse_text_stream("msg-scope", "hello")),
            MockHttpResponse::sse(&sse_text_stream("msg-cli", "hello")),
        ])
        .await?;
        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: url,
            ..AnthropicProviderConfig::new("claude-opus-5-5", "test-key")
        })?;
        let scope = ExecutionScope {
            session_id: "fm-mission-1".to_string(),
            run_id: Some("run-42".to_string()),
            ..ExecutionScope::default()
        };
        let (sender, _receiver) = mpsc::unbounded_channel();
        scope_execution(
            scope,
            CancellationToken::new(),
            provider.stream(
                runtime_request("fm-mission-1", ModelGenerationConfig::default()),
                ModelEventSink::new(sender),
            ),
        )
        .await?;
        let (sender, _receiver) = mpsc::unbounded_channel();
        provider
            .stream(
                runtime_request("cli-session", ModelGenerationConfig::default()),
                ModelEventSink::new(sender),
            )
            .await?;

        let recorded = recorded.lock().clone();
        assert_eq!(recorded.len(), 2);
        assert_eq!(
            recorded[0].header("x-kheish-session-id"),
            Some("fm-mission-1")
        );
        assert_eq!(recorded[0].header("x-kheish-run-id"), Some("run-42"));
        assert_eq!(recorded[0].header("x-api-key"), Some("test-key"));
        // The ids travel as headers only: the model never sees them.
        assert_eq!(recorded[0].json()["model"], "claude-opus-5-5");
        assert!(!recorded[0].body.contains("run-42"));
        assert!(!recorded[0].body.contains("fm-mission-1"));
        assert_eq!(
            recorded[1].header("x-kheish-session-id"),
            Some("cli-session")
        );
        assert_eq!(recorded[1].header("x-kheish-run-id"), None);
        Ok(())
    }

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("The {name} tool."),
            input_schema: json!({"type": "object", "properties": {}}),
            allows_parallel: true,
            strict: false,
        }
    }

    #[test]
    fn requests_carry_stable_cache_breakpoints_on_system_and_tools() -> Result<()> {
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-5-5", "test-key"))?;
        let mut first = runtime_request("session-cache", ModelGenerationConfig::default());
        first.available_tools = vec![tool("submit_verdict"), tool("get_exposure")];
        let mut second = first.clone();
        // The runtime may list tools in another order on a later turn.
        second.available_tools.reverse();
        second.prompt.items.extend([
            ProviderInputItem::Message {
                id: "a1".to_string(),
                role: kheish_types::Role::Assistant,
                content: "Hello.".to_string(),
                content_parts: Vec::new(),
                attachments: Vec::new(),
                provider_response_id: Some("msg-1".to_string()),
                provider_context: None,
            },
            ProviderInputItem::Message {
                id: "u2".to_string(),
                role: kheish_types::Role::User,
                content: "Verify the exposure.".to_string(),
                content_parts: Vec::new(),
                attachments: Vec::new(),
                provider_response_id: None,
                provider_context: None,
            },
        ]);
        let first = provider.build_request_body(&first)?;
        let second = provider.build_request_body(&second)?;

        let breakpoint = json!({ "type": "ephemeral" });
        assert_eq!(
            first["system"],
            json!([{
                "type": "text",
                "text": "You verify exposures.",
                "cache_control": breakpoint,
            }])
        );
        let tools = first["tools"].as_array().expect("tools");
        assert_eq!(tools[0]["name"], "get_exposure");
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools[1]["name"], "submit_verdict");
        assert_eq!(tools[1]["cache_control"], breakpoint);
        // Automatic caching of the conversation tail.
        assert_eq!(first["cache_control"], breakpoint);
        // The cached prefix renders the same bytes on the next turn.
        assert_eq!(first["system"], second["system"]);
        assert_eq!(first["tools"], second["tools"]);
        assert_eq!(first["messages"][0], second["messages"][0]);
        Ok(())
    }

    #[tokio::test]
    async fn usage_merges_cache_counts_across_events_and_prices_them() -> Result<()> {
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-cache\",\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":1000,\"cache_creation_input_tokens\":20000,\"cache_read_input_tokens\":500000,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"done\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2000}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (url, _recorded) =
            spawn_recording_mock_server(vec![MockHttpResponse::sse(stream)]).await?;
        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: url,
            ..AnthropicProviderConfig::new("claude-opus-5-5", "test-key")
        })?;
        let runtime = ModelRuntime::new(
            provider,
            ModelRetryPolicy {
                max_attempts: 1,
                ..ModelRetryPolicy::default()
            },
            ModelBudget {
                max_total_output_tokens: Some(1_000_000),
                max_total_cost_usd: Some(1_000.0),
            },
            Arc::new(NoopObserver),
        );
        let turn = runtime
            .next_turn(default_model_request("session-cache"))
            .await?;
        let usage = turn.usage.expect("usage");
        assert_eq!(usage.input_tokens, 1_000);
        assert_eq!(usage.cache_creation_input_tokens, 20_000);
        assert_eq!(usage.cache_read_input_tokens, 500_000);
        assert_eq!(usage.output_tokens, 2_000);
        assert_eq!(usage.prompt_tokens(), 521_000);
        // Claude Opus 5.5: $4 input, $5 cache write, $0.20 cache read, $20 output.
        let expected = 1_000.0 * 4.0 / 1e6
            + 20_000.0 * 5.0 / 1e6
            + 500_000.0 * 0.20 / 1e6
            + 2_000.0 * 20.0 / 1e6;
        assert!(
            (usage.cost_usd - expected).abs() < 1e-9,
            "{}",
            usage.cost_usd
        );
        let consumed = runtime.budget_snapshot().consumed;
        assert_eq!(consumed.cache_read_input_tokens, 500_000);
        Ok(())
    }

    /// A streamed answer that interleaves thinking, text and a tool use the
    /// way Claude Opus 5.5 does under the default `display: "omitted"`:
    /// thinking blocks with empty text and a signature.
    const INTERLEAVED_THINKING_STREAM: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-a1\",\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":40,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"EqQBCkYIBxgCKkBsig-one==\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Checking the host. \"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Scan the port first.\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"EqQBCkYIBxgCKkBsig-two==\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_scan\",\"name\":\"mcp__fregate__scan_target\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"target\\\":\\\"example.com\\\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":3}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_details\":null},\"usage\":{\"output_tokens\":90}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );

    /// The answer above as the API expects it back.
    fn interleaved_answer_content() -> Value {
        json!([
            {"type": "thinking", "thinking": "", "signature": "EqQBCkYIBxgCKkBsig-one=="},
            {"type": "text", "text": "Checking the host. "},
            {"type": "thinking", "thinking": "Scan the port first.", "signature": "EqQBCkYIBxgCKkBsig-two=="},
            {"type": "tool_use", "id": "toolu_scan", "name": "mcp__fregate__scan_target", "input": {"target": "example.com"}},
        ])
    }

    fn user_item(id: &str, content: &str) -> ProviderInputItem {
        ProviderInputItem::Message {
            id: id.to_string(),
            role: kheish_types::Role::User,
            content: content.to_string(),
            content_parts: Vec::new(),
            attachments: Vec::new(),
            provider_response_id: None,
            provider_context: None,
        }
    }

    /// Runs the interleaved answer through the runtime and returns the
    /// conversation items the engine would store for it.
    async fn interleaved_answer_items() -> Result<Vec<ProviderInputItem>> {
        let (url, _recorded) =
            spawn_recording_mock_server(vec![MockHttpResponse::sse(INTERLEAVED_THINKING_STREAM)])
                .await?;
        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: url,
            ..AnthropicProviderConfig::new("claude-opus-5-5", "test-key")
        })?;
        let runtime = ModelRuntime::new(
            provider,
            ModelRetryPolicy {
                max_attempts: 1,
                ..ModelRetryPolicy::default()
            },
            ModelBudget::default(),
            Arc::new(NoopObserver),
        );
        let turn = runtime
            .next_turn(default_model_request("fm-mission"))
            .await?;
        let message = turn.assistant_message;
        let mut items = vec![ProviderInputItem::Message {
            id: message.id.clone(),
            role: message.role.clone(),
            content: message.content.clone(),
            content_parts: Vec::new(),
            attachments: Vec::new(),
            provider_response_id: message.provider_response_id.clone(),
            provider_context: message.provider_context.clone(),
        }];
        for call in turn.tool_calls {
            items.push(ProviderInputItem::ToolCall {
                assistant_message_id: Some(message.id.clone()),
                call,
            });
        }
        Ok(items)
    }

    fn opus_5_5_request(items: Vec<ProviderInputItem>) -> ModelRuntimeRequest {
        let mut request = runtime_request("fm-mission", ModelGenerationConfig::default());
        request.prompt.items = items;
        request
    }

    #[tokio::test]
    async fn thinking_blocks_replay_byte_for_byte_in_their_original_order() -> Result<()> {
        let mut items = vec![user_item("u1", "Verify the exposure on example.com.")];
        items.extend(interleaved_answer_items().await?);
        items.push(ProviderInputItem::ToolResult {
            result: ToolResultRecord {
                call_id: "toolu_scan".to_string(),
                output: json!({"open_ports": [443]}),
                is_error: false,
                tool_name: Some("mcp__fregate__scan_target".to_string()),
                offset: None,
                timestamp_ms: None,
                context_updates: Vec::new(),
                hook_contexts: Vec::new(),
            },
        });
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-5-5", "test-key"))?;
        let body = provider.build_request_body(&opus_5_5_request(items))?;
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages[1]["role"], "assistant");
        // Both thinking blocks, the empty-text one included, keep their
        // signatures and their place between the text and the tool use.
        assert_eq!(messages[1]["content"], interleaved_answer_content());
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_scan");
        Ok(())
    }

    #[test]
    fn preserved_thinking_models_send_drop_block_under_the_binding_beta() -> Result<()> {
        for model in [
            "claude-opus-5-5",
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "anthropic.claude-opus-5-5",
        ] {
            let provider = AnthropicProvider::new(AnthropicProviderConfig::new(model, "key"))?;
            // With and without a reasoning config: the field is always set.
            for generation in [
                ModelGenerationConfig::default(),
                ModelGenerationConfig {
                    reasoning: Some(ReasoningConfig {
                        effort: Some(ReasoningEffort::High),
                        summary: Some(ReasoningSummary::Auto),
                        ..ReasoningConfig::default()
                    }),
                    ..ModelGenerationConfig::default()
                },
            ] {
                let request = provider.build_request(&minimal_request(generation))?;
                assert_eq!(request.body["thinking"]["type"], "adaptive", "{model}");
                assert_eq!(
                    request.body["thinking"]["block_binding"],
                    json!({ "prefix_mismatch_behavior": "drop_block" }),
                    "{model}"
                );
                assert!(
                    request
                        .betas
                        .contains(&"thinking-binding-controls-2026-08-01"),
                    "{model}"
                );
            }
        }
        for model in ["claude-opus-5", "claude-opus-4-8", "claude-sonnet-5"] {
            let provider = AnthropicProvider::new(AnthropicProviderConfig::new(model, "key"))?;
            let request = provider.build_request(&minimal_request(ModelGenerationConfig {
                reasoning: Some(ReasoningConfig {
                    effort: Some(ReasoningEffort::High),
                    ..ReasoningConfig::default()
                }),
                ..ModelGenerationConfig::default()
            }))?;
            assert!(
                request.body["thinking"].get("block_binding").is_none(),
                "{model}"
            );
            assert!(request.betas.is_empty(), "{model}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn orphaned_tool_call_recovery_keeps_the_prefix_and_sends_drop_block() -> Result<()> {
        // The request that produced the answer, before the daemon died with
        // the tool call in flight.
        let before = vec![user_item("u1", "Verify the exposure on example.com.")];
        // After the restart, the next run's prompt: the answer and its tool
        // call, no result, then the continuation input.
        let mut after = before.clone();
        after.extend(interleaved_answer_items().await?);
        after.push(user_item("u2", "The run was interrupted; continue."));

        let (url, recorded) = spawn_recording_mock_server(vec![
            MockHttpResponse::sse(&sse_text_stream("msg-before", "ok")),
            MockHttpResponse::sse(&sse_text_stream("msg-after", "ok")),
        ])
        .await?;
        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: url,
            ..AnthropicProviderConfig::new("claude-opus-5-5", "test-key")
        })?;
        for items in [before, after] {
            let (sender, _receiver) = mpsc::unbounded_channel();
            provider
                .stream(opus_5_5_request(items), ModelEventSink::new(sender))
                .await?;
        }
        let recorded = recorded.lock().clone();
        let before = recorded[0].json();
        let after = recorded[1].json();

        // The prefix the thinking blocks were minted under is unchanged...
        assert_eq!(before["system"], after["system"]);
        assert_eq!(before["tools"], after["tools"]);
        assert_eq!(before["messages"][0], after["messages"][0]);
        // ...the answer replays verbatim...
        assert_eq!(
            after["messages"][1]["content"],
            interleaved_answer_content()
        );
        // ...the missing result is appended after it as an error...
        let repaired = &after["messages"][2]["content"][0];
        assert_eq!(repaired["type"], "tool_result");
        assert_eq!(repaired["tool_use_id"], "toolu_scan");
        assert_eq!(repaired["is_error"], true);
        assert!(
            repaired["content"][0]["text"]
                .as_str()
                .expect("text")
                .contains("tool result missing")
        );
        assert_eq!(
            after["messages"][3]["content"][0]["text"],
            "The run was interrupted; continue."
        );
        // ...and the request opts into drop_block under the binding beta.
        assert_eq!(
            after["thinking"]["block_binding"]["prefix_mismatch_behavior"],
            "drop_block"
        );
        assert!(
            recorded[1]
                .header("anthropic-beta")
                .expect("beta header")
                .split(',')
                .any(|beta| beta == "thinking-binding-controls-2026-08-01")
        );
        Ok(())
    }

    #[tokio::test]
    async fn compacted_history_replays_post_boundary_thinking_with_drop_block() -> Result<()> {
        // After a compaction the window starts with the summary; the turns
        // kept after the boundary replay their thinking.
        let mut items = vec![ProviderInputItem::Summary {
            summary: SummaryBlock {
                title: "compaction".to_string(),
                content: "<summary>The agent scanned example.com.</summary>".to_string(),
            },
        }];
        items.extend(interleaved_answer_items().await?);
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-5-5", "test-key"))?;
        let request = provider.build_request(&opus_5_5_request(items))?;
        let messages = request.body["messages"].as_array().expect("messages");
        assert_eq!(messages[0]["role"], "user");
        assert!(
            messages[0]["content"][0]["text"]
                .as_str()
                .expect("summary text")
                .contains("The agent scanned example.com.")
        );
        assert_eq!(messages[1]["content"], interleaved_answer_content());
        assert_eq!(
            request.body["thinking"]["block_binding"]["prefix_mismatch_behavior"],
            "drop_block"
        );
        Ok(())
    }

    #[tokio::test]
    async fn input_transformations_are_kept_with_the_answer() -> Result<()> {
        let stream = sse_text_stream("msg-dropped", "ok").replace(
            "\"usage\":{\"input_tokens\":5",
            "\"input_transformations\":[{\"type\":\"thinking_dropped\",\"path\":\"messages.1.content.0\",\"reason\":\"prefix_binding_mismatch\"}],\"usage\":{\"input_tokens\":5",
        );
        let (url, _recorded) =
            spawn_recording_mock_server(vec![MockHttpResponse::sse(&stream)]).await?;
        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: url,
            ..AnthropicProviderConfig::new("claude-opus-5-5", "test-key")
        })?;
        let runtime = ModelRuntime::new(
            provider,
            ModelRetryPolicy {
                max_attempts: 1,
                ..ModelRetryPolicy::default()
            },
            ModelBudget::default(),
            Arc::new(NoopObserver),
        );
        let turn = runtime
            .next_turn(default_model_request("fm-mission"))
            .await?;
        let context = turn
            .assistant_message
            .provider_context
            .expect("the transformation is recorded with the answer");
        assert_eq!(
            context["anthropic"]["input_transformations"][0]["reason"],
            "prefix_binding_mismatch"
        );
        Ok(())
    }

    /// Claude Opus 5.5 starts an answer and a tool call, declines mid-stream,
    /// and Claude Opus 5 answers: the response shape the refusals and
    /// fallback page documents for a streamed mid-output fallback.
    const MID_STREAM_FALLBACK_STREAM: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-fb\",\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":900,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-declined\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Looking at the service. \"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_declined\",\"name\":\"mcp__fregate__scan_target\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"target\\\":\\\"a\\\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"fallback\",\"from\":{\"model\":\"claude-opus-5-5\"},\"to\":{\"model\":\"claude-opus-5\"},\"trigger\":{\"type\":\"refusal\",\"category\":\"cyber\"}}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":3}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":4,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":4,\"delta\":{\"type\":\"text_delta\",\"text\":\"The port is closed.\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":4}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":5,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_served\",\"name\":\"mcp__fregate__submit_verdict\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":5,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"verdict\\\":\\\"closed\\\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":5}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_details\":null},\"usage\":{\"input_tokens\":900,\"output_tokens\":120,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0,\"iterations\":[{\"type\":\"message\",\"model\":\"claude-opus-5-5\",\"input_tokens\":900,\"output_tokens\":40,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0},{\"type\":\"fallback_message\",\"model\":\"claude-opus-5\",\"input_tokens\":900,\"output_tokens\":120,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}]}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );

    fn runtime_for(provider: AnthropicProvider) -> ModelRuntime<AnthropicProvider> {
        ModelRuntime::new(
            provider,
            ModelRetryPolicy {
                max_attempts: 1,
                ..ModelRetryPolicy::default()
            },
            ModelBudget {
                max_total_output_tokens: Some(1_000_000),
                max_total_cost_usd: Some(1_000.0),
            },
            Arc::new(NoopObserver),
        )
    }

    #[test]
    fn requests_name_refusal_fallbacks_from_the_run_or_the_route() -> Result<()> {
        let route_default = AnthropicProviderConfig {
            refusal_fallbacks: Some(RefusalFallbacks::default_routing()),
            ..AnthropicProviderConfig::new("claude-opus-5-5", "key")
        };
        let provider = AnthropicProvider::new(route_default.clone())?;
        let request = provider.build_request(&minimal_request(ModelGenerationConfig::default()))?;
        assert_eq!(request.body["fallbacks"], "default");
        assert!(request.betas.contains(&"server-side-fallback-2026-07-01"));

        // The run's list overrides the route's routing.
        let request = provider.build_request(&minimal_request(ModelGenerationConfig {
            refusal_fallbacks: Some(RefusalFallbacks::Models(vec![
                RefusalFallbackModel {
                    model: "claude-opus-5".to_string(),
                    max_tokens: Some(8_000),
                },
                RefusalFallbackModel {
                    model: "claude-opus-4-8".to_string(),
                    max_tokens: None,
                },
            ])),
            ..ModelGenerationConfig::default()
        }))?;
        assert_eq!(
            request.body["fallbacks"],
            json!([
                {"model": "claude-opus-5", "max_tokens": 8_000},
                {"model": "claude-opus-4-8"}
            ])
        );

        // Claude Sonnet 5.5 takes only the default routing; Claude Haiku 5.5
        // and Claude Opus 4.8 no server-side fallback at all.
        let list = ModelGenerationConfig {
            refusal_fallbacks: Some(RefusalFallbacks::Models(vec![RefusalFallbackModel {
                model: "claude-sonnet-5".to_string(),
                max_tokens: None,
            }])),
            ..ModelGenerationConfig::default()
        };
        for model in ["claude-sonnet-5-5", "claude-haiku-5-5", "claude-opus-4-8"] {
            let provider = AnthropicProvider::new(AnthropicProviderConfig::new(model, "key"))?;
            let request = provider.build_request(&minimal_request(list.clone()))?;
            assert!(request.body.get("fallbacks").is_none(), "{model}");
            assert!(!request.betas.contains(&"server-side-fallback-2026-07-01"));
        }
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-sonnet-5-5", "key"))?;
        let request = provider.build_request(&minimal_request(ModelGenerationConfig {
            refusal_fallbacks: Some(RefusalFallbacks::default_routing()),
            ..ModelGenerationConfig::default()
        }))?;
        assert_eq!(request.body["fallbacks"], "default");

        // A list that names the requested model, repeats one, or is too long.
        for models in [
            vec!["claude-opus-5-5"],
            vec!["claude-opus-5", "claude-opus-5"],
            vec!["a-1", "a-2", "a-3", "a-4"],
        ] {
            let error = AnthropicProvider::new(route_default.clone())?
                .build_request(&minimal_request(ModelGenerationConfig {
                    refusal_fallbacks: Some(RefusalFallbacks::Models(
                        models
                            .iter()
                            .map(|model| RefusalFallbackModel {
                                model: model.to_string(),
                                max_tokens: None,
                            })
                            .collect(),
                    )),
                    ..ModelGenerationConfig::default()
                }))
                .err()
                .expect("an invalid list is refused before the request");
            assert!(!error.retryable);
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_mid_stream_fallback_withdraws_the_declined_attempt_and_prices_each() -> Result<()> {
        let (url, recorded) =
            spawn_recording_mock_server(vec![MockHttpResponse::sse(MID_STREAM_FALLBACK_STREAM)])
                .await?;
        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: url,
            refusal_fallbacks: Some(RefusalFallbacks::default_routing()),
            ..AnthropicProviderConfig::new("claude-opus-5-5", "test-key")
        })?;
        let turn = runtime_for(provider)
            .next_turn(default_model_request("fm-mission"))
            .await?;
        let request = recorded.lock()[0].clone();
        assert_eq!(request.json()["fallbacks"], "default");
        assert!(
            request
                .header("anthropic-beta")
                .expect("betas")
                .contains("server-side-fallback-2026-07-01")
        );

        // Only the serving model's tool call runs.
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "toolu_served");
        // The declined attempt's text stays in the answer.
        assert_eq!(
            turn.assistant_message.content,
            "Looking at the service. The port is closed."
        );
        // The replay keeps the text before the marker, the marker where it
        // appeared, and everything after it.
        let blocks = turn
            .assistant_message
            .provider_context
            .as_ref()
            .expect("context")["anthropic"]["content_blocks"]
            .clone();
        assert_eq!(
            blocks,
            json!([
                {"type": "text", "text": "Looking at the service. "},
                {"type": "fallback", "from": {"model": "claude-opus-5-5"}, "to": {"model": "claude-opus-5"}, "trigger": {"type": "refusal", "category": "cyber"}},
                {"type": "text", "text": "The port is closed."},
                {"type": "tool_use", "id": "toolu_served"},
            ])
        );
        // Each attempt is priced at its own model's rates.
        let usage = turn.usage.expect("usage");
        assert_eq!(usage.iterations.len(), 2);
        assert_eq!(usage.iterations[0].kind, "message");
        assert_eq!(
            usage.iterations[0].model.as_deref(),
            Some("claude-opus-5-5")
        );
        assert_eq!(usage.iterations[1].kind, "fallback_message");
        assert_eq!(usage.iterations[1].model.as_deref(), Some("claude-opus-5"));
        let opus_5_5 = 900.0 * 4.0 / 1e6 + 40.0 * 20.0 / 1e6;
        let opus_5 = 900.0 * 5.0 / 1e6 + 120.0 * 25.0 / 1e6;
        assert!((usage.iterations[0].cost_usd - opus_5_5).abs() < 1e-12);
        assert!((usage.iterations[1].cost_usd - opus_5).abs() < 1e-12);
        assert!((usage.cost_usd - (opus_5_5 + opus_5)).abs() < 1e-12);
        assert_eq!(usage.billed().output_tokens, 160);

        // The stored turn replays as the API expects it back.
        let message = turn.assistant_message;
        let mut items = vec![user_item("u1", "Verify the exposure.")];
        items.push(ProviderInputItem::Message {
            id: message.id.clone(),
            role: message.role.clone(),
            content: message.content.clone(),
            content_parts: Vec::new(),
            attachments: Vec::new(),
            provider_response_id: message.provider_response_id.clone(),
            provider_context: message.provider_context.clone(),
        });
        for call in turn.tool_calls {
            items.push(ProviderInputItem::ToolCall {
                assistant_message_id: Some(message.id.clone()),
                call,
            });
        }
        let replay =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-5-5", "test-key"))?
                .build_request_body(&opus_5_5_request(items))?;
        assert_eq!(
            replay["messages"][1]["content"],
            json!([
                {"type": "text", "text": "Looking at the service. "},
                {"type": "fallback", "from": {"model": "claude-opus-5-5"}, "to": {"model": "claude-opus-5"}, "trigger": {"type": "refusal", "category": "cyber"}},
                {"type": "text", "text": "The port is closed."},
                {"type": "tool_use", "id": "toolu_served", "name": "mcp__fregate__submit_verdict", "input": {"verdict": "closed"}},
            ])
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_final_refusal_reports_its_category() -> Result<()> {
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-refused\",\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":412,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"fallback\",\"from\":{\"model\":\"claude-opus-5-5\"},\"to\":{\"model\":\"claude-opus-5\"},\"trigger\":{\"type\":\"refusal\",\"category\":\"cyber\"}}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\",\"stop_details\":{\"type\":\"refusal\",\"category\":\"cyber\",\"explanation\":\"This request was declined because it could enable cyber harm.\",\"recommended_model\":null}},\"usage\":{\"output_tokens\":0}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (url, _recorded) =
            spawn_recording_mock_server(vec![MockHttpResponse::sse(stream)]).await?;
        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: url,
            ..AnthropicProviderConfig::new("claude-opus-5-5", "test-key")
        })?;
        let turn = runtime_for(provider)
            .next_turn(default_model_request("fm-mission"))
            .await?;
        assert_eq!(turn.finish_reason, crate::model::ModelFinishReason::Blocked);
        assert_eq!(
            turn.refusal,
            Some(ModelRefusal {
                category: Some("cyber".to_string()),
                explanation: Some(
                    "This request was declined because it could enable cyber harm.".to_string()
                ),
                model: Some("claude-opus-5".to_string()),
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_answer_cut_off_inside_a_tool_use_is_a_max_tokens_turn() -> Result<()> {
        let cut = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-cut\",\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_cut\",\"name\":\"write_file\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"content\\\":\\\"the report sta\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\",\"stop_details\":null},\"usage\":{\"output_tokens\":16000}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (url, _recorded) =
            spawn_recording_mock_server(vec![MockHttpResponse::sse(cut)]).await?;
        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: url,
            ..AnthropicProviderConfig::new("claude-opus-5-5", "test-key")
        })?;
        let turn = runtime_for(provider)
            .next_turn(default_model_request("fm-mission"))
            .await?;
        assert_eq!(
            turn.finish_reason,
            crate::model::ModelFinishReason::MaxTokens
        );
        assert!(turn.tool_calls.is_empty());

        // The same cut input on an answer that was not cut off is an error.
        let broken = cut.replace("\"max_tokens\"", "\"tool_use\"");
        let (url, _recorded) =
            spawn_recording_mock_server(vec![MockHttpResponse::sse(&broken)]).await?;
        let provider = AnthropicProvider::new(AnthropicProviderConfig {
            base_url: url,
            ..AnthropicProviderConfig::new("claude-opus-5-5", "test-key")
        })?;
        let error = runtime_for(provider)
            .next_turn(default_model_request("fm-mission"))
            .await
            .expect_err("a tool use that does not parse fails the attempt");
        assert!(
            error
                .to_string()
                .contains("invalid Anthropic tool JSON payload")
        );
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

    /// Fregate's verdict tool: enums, an integer, nested closed objects and
    /// arrays with their own descriptions, every object closed. Strict tool
    /// use enforces all of it.
    fn submit_verdict_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "verdict": {"type": "string", "enum": ["confirmed", "likely_false_positive", "needs_human"]},
                "confidence": {"type": "integer", "description": "0 to 100."},
                "next_step": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["remediate", "dismiss", "recheck", "investigate"]},
                        "detail": {"type": "string", "description": "Specifics, at most 500 characters."}
                    },
                    "required": ["action", "detail"],
                    "additionalProperties": false
                },
                "reasons": {
                    "type": "array",
                    "description": "1 to 12 reasons.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": {"type": "string", "enum": ["supports", "contradicts", "context"]},
                            "evidence": {"type": "array", "items": {"type": "string"}}
                        },
                        "required": ["evidence", "kind"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["confidence", "next_step", "reasons", "verdict"],
            "additionalProperties": false
        })
    }

    /// The same shape with length and range limits, which strict tool use
    /// does not enforce.
    fn limited_schema() -> Value {
        let mut schema = submit_verdict_schema();
        schema["properties"]["confidence"]["minimum"] = json!(0);
        schema["properties"]["confidence"]["maximum"] = json!(100);
        schema["properties"]["next_step"]["properties"]["detail"]["maxLength"] = json!(500);
        schema["properties"]["key"] = json!({"type": "string", "minLength": 3});
        schema
    }

    fn mcp_tool(name: &str, input_schema: Value) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: "An MCP tool.".to_string(),
            input_schema,
            allows_parallel: true,
            strict: true,
        }
    }

    fn body_with_tools(model: &str, tools: Vec<ToolDefinition>) -> Result<Value> {
        let provider = AnthropicProvider::new(AnthropicProviderConfig::new(model, "test-key"))?;
        let mut request = runtime_request("session-tools", ModelGenerationConfig::default());
        request.available_tools = tools;
        Ok(provider.build_request_body(&request)?)
    }

    #[test]
    fn mcp_tool_schemas_reach_the_request_body_exactly() -> Result<()> {
        let body = body_with_tools(
            "claude-opus-5-5",
            vec![
                mcp_tool("mcp__fregate__get_fact", limited_schema()),
                mcp_tool("mcp__fregate__submit_verdict", submit_verdict_schema()),
            ],
        )?;
        let tools = body["tools"].as_array().expect("tools");
        assert_eq!(tools[0]["name"], "mcp__fregate__get_fact");
        assert_eq!(
            serde_json::to_string(&tools[0]["input_schema"])?,
            serde_json::to_string(&limited_schema())?,
            "enums, integers, nested objects and length limits reach the body byte for byte"
        );
        assert!(
            tools[0].get("strict").is_none(),
            "length and range limits leave the tool non-strict, with its schema unchanged"
        );
        assert_eq!(
            serde_json::to_string(&tools[1]["input_schema"])?,
            serde_json::to_string(&submit_verdict_schema())?
        );
        assert_eq!(tools[1]["strict"], json!(true));
        assert_eq!(tools[1]["cache_control"], json!({"type": "ephemeral"}));
        Ok(())
    }

    #[test]
    fn strict_is_sent_only_when_asked_and_when_the_model_takes_it() -> Result<()> {
        let mut own = mcp_tool("read_file", submit_verdict_schema());
        own.strict = false;
        let body = body_with_tools("claude-opus-5-5", vec![own])?;
        assert!(body["tools"][0].get("strict").is_none());

        for model in [
            "claude-opus-4-6",
            "claude-sonnet-4-5",
            "claude-3-7-sonnet-latest",
        ] {
            let body = body_with_tools(
                model,
                vec![mcp_tool(
                    "mcp__fregate__submit_verdict",
                    submit_verdict_schema(),
                )],
            )?;
            assert!(body["tools"][0].get("strict").is_none(), "{model}");
        }
        for model in [
            "claude-sonnet-5",
            "claude-haiku-4-5",
            "claude-opus-4-8",
            "claude-fable-5-1",
        ] {
            let body = body_with_tools(
                model,
                vec![mcp_tool(
                    "mcp__fregate__submit_verdict",
                    submit_verdict_schema(),
                )],
            )?;
            assert_eq!(body["tools"][0]["strict"], json!(true), "{model}");
        }
        Ok(())
    }

    #[test]
    fn strict_tools_stay_within_the_request_limits() -> Result<()> {
        // Twenty-two closed tools without optional parameters: the first
        // twenty by name are strict.
        let tools = (0..22)
            .map(|index| mcp_tool(&format!("mcp__s__tool_{index:02}"), submit_verdict_schema()))
            .collect();
        let body = body_with_tools("claude-opus-5-5", tools)?;
        let strict = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool.get("strict").is_some())
            .collect::<Vec<_>>();
        assert_eq!(strict.iter().filter(|strict| **strict).count(), 20);
        assert!(strict[..20].iter().all(|strict| *strict));

        // Optional parameters share a budget of 24 across strict tools.
        let mut optional =
            json!({"type": "object", "properties": {}, "additionalProperties": false});
        for index in 0..10 {
            optional["properties"][format!("p{index}")] = json!({"type": "string"});
        }
        let tools = (0..3)
            .map(|index| mcp_tool(&format!("mcp__s__opt_{index}"), optional.clone()))
            .collect();
        let body = body_with_tools("claude-opus-5-5", tools)?;
        let strict = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool.get("strict").is_some())
            .collect::<Vec<_>>();
        assert_eq!(strict, vec![true, true, false]);
        Ok(())
    }

    #[test]
    fn strict_schemas_are_the_subset_strict_tool_use_enforces() {
        use super::anthropic_strict_schema_cost as cost;
        assert!(cost(&submit_verdict_schema()).is_some());
        assert!(cost(&limited_schema()).is_none());
        // Every object must be closed.
        assert!(
            cost(&json!({"type": "object", "properties": {"a": {"type": "string"}}})).is_none()
        );
        assert!(
            cost(&json!({
                "type": "object",
                "properties": {"a": {"type": "object", "properties": {}}},
                "additionalProperties": false
            }))
            .is_none()
        );
        // Union types and optional parameters are counted.
        let counted = cost(&json!({
            "type": "object",
            "properties": {
                "a": {"type": ["string", "null"]},
                "b": {"anyOf": [{"type": "string"}, {"type": "integer"}]},
                "c": {"type": "string", "format": "date-time"}
            },
            "required": ["c"],
            "additionalProperties": false
        }))
        .expect("strict");
        assert_eq!(counted.optional_parameters, 2);
        assert_eq!(counted.union_parameters, 2);
        // Unsupported formats, minItems above 1, untyped nodes, recursion.
        for schema in [
            json!({"type": "object", "properties": {"u": {"type": "string", "format": "uri-reference"}}, "additionalProperties": false}),
            json!({"type": "object", "properties": {"l": {"type": "array", "items": {"type": "string"}, "minItems": 2}}, "additionalProperties": false}),
            json!({"type": "object", "properties": {"x": {}}, "additionalProperties": false}),
            json!({
                "type": "object",
                "properties": {"tree": {"$ref": "#/$defs/Node"}},
                "additionalProperties": false,
                "$defs": {"Node": {"type": "object", "properties": {"child": {"$ref": "#/$defs/Node"}}, "additionalProperties": false}}
            }),
        ] {
            assert!(cost(&schema).is_none(), "{schema}");
        }
        // A non-recursive local definition is fine.
        assert!(
            cost(&json!({
                "type": "object",
                "properties": {"step": {"$ref": "#/$defs/Step"}},
                "required": ["step"],
                "additionalProperties": false,
                "$defs": {"Step": {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"], "additionalProperties": false}}
            }))
            .is_some()
        );
    }

    #[test]
    fn tool_results_carry_their_error_flag_and_plain_text() -> Result<()> {
        let document = r#"{"ref":"fact:service:203.0.113.10:3389/tcp","in_current_state":true}"#;
        let items = vec![
            user_item("u1", "Verify."),
            ProviderInputItem::Message {
                id: "a1".to_string(),
                role: kheish_types::Role::Assistant,
                content: String::new(),
                content_parts: Vec::new(),
                attachments: Vec::new(),
                provider_response_id: None,
                provider_context: None,
            },
            ProviderInputItem::ToolCall {
                assistant_message_id: Some("a1".to_string()),
                call: ToolCallRecord {
                    id: "toolu_1".to_string(),
                    name: "mcp__fregate__get_fact".to_string(),
                    input: json!({}),
                    assistant_message_id: Some("a1".to_string()),
                    assistant_provider_response_id: None,
                },
            },
            ProviderInputItem::ToolCall {
                assistant_message_id: Some("a1".to_string()),
                call: ToolCallRecord {
                    id: "toolu_2".to_string(),
                    name: "mcp__fregate__get_fact".to_string(),
                    input: json!({}),
                    assistant_message_id: Some("a1".to_string()),
                    assistant_provider_response_id: None,
                },
            },
            ProviderInputItem::ToolResult {
                result: ToolResultRecord {
                    call_id: "toolu_1".to_string(),
                    output: Value::String(document.to_string()),
                    is_error: false,
                    tool_name: Some("mcp__fregate__get_fact".to_string()),
                    offset: None,
                    timestamp_ms: None,
                    context_updates: Vec::new(),
                    hook_contexts: Vec::new(),
                },
            },
            ProviderInputItem::ToolResult {
                result: ToolResultRecord {
                    call_id: "toolu_2".to_string(),
                    output: Value::String("unknown fact".to_string()),
                    is_error: true,
                    tool_name: Some("mcp__fregate__get_fact".to_string()),
                    offset: None,
                    timestamp_ms: None,
                    context_updates: Vec::new(),
                    hook_contexts: Vec::new(),
                },
            },
        ];
        let provider =
            AnthropicProvider::new(AnthropicProviderConfig::new("claude-opus-5-5", "test-key"))?;
        let body = provider.build_request_body(&opus_5_5_request(items))?;
        let results = &body["messages"][2]["content"];
        assert_eq!(
            results[0],
            json!({
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "is_error": false,
                "content": [{"type": "text", "text": document}]
            })
        );
        assert_eq!(
            results[1],
            json!({
                "type": "tool_result",
                "tool_use_id": "toolu_2",
                "is_error": true,
                "content": [{"type": "text", "text": "unknown fact"}]
            })
        );
        Ok(())
    }
}
