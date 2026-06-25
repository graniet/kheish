//! Provider-aware native web search backends for daemon-managed tool dispatch.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use kheish_auth::ResolvedAuthMaterial;
use kheish_coding_tools::{
    ProviderWebSearchService, WebSearchBackendOutput, WebSearchHit, WebSearchRequest,
    WebSearchRoute,
};
use kheish_runtime::{
    AnthropicProviderConfig, OpenAiProviderConfig, RuntimeObserver, XAiProviderConfig,
    external_action_trace_with_grant_id, failed_external_action_outcome,
    failed_reqwest_external_action_outcome, safe_url_audit_target,
};
use reqwest::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue,
};
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::model_routing::{ConfiguredModelRoute, ModelRouteConfig};

const MAX_NATIVE_WEB_SEARCH_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_NATIVE_WEB_SEARCH_TITLE_CHARS: usize = 512;
const MAX_NATIVE_WEB_SEARCH_SNIPPET_CHARS: usize = 2048;

#[async_trait]
trait NativeWebSearchBackend: Send + Sync {
    fn provider(&self) -> &str;

    async fn search(
        &self,
        route: &WebSearchRoute,
        request: &WebSearchRequest,
    ) -> Result<Option<WebSearchBackendOutput>>;
}

/// One daemon-owned web search service that prefers provider-native backends.
pub(crate) struct DaemonWebSearchService {
    backends: BTreeMap<String, Arc<dyn NativeWebSearchBackend>>,
}

impl DaemonWebSearchService {
    /// Builds one provider-aware service from zero or more native backends.
    fn new(backends: BTreeMap<String, Arc<dyn NativeWebSearchBackend>>) -> Self {
        Self { backends }
    }
}

#[async_trait]
impl ProviderWebSearchService for DaemonWebSearchService {
    async fn search(
        &self,
        route: &WebSearchRoute,
        request: &WebSearchRequest,
    ) -> Result<Option<WebSearchBackendOutput>> {
        let Some(backend) = self.backends.get(&route.provider) else {
            return Ok(None);
        };
        backend.search(route, request).await
    }
}

pub(crate) fn build_web_search_service(
    routes: &[ConfiguredModelRoute],
    observer: Arc<dyn RuntimeObserver>,
) -> Result<Option<Arc<dyn ProviderWebSearchService>>> {
    let mut backends = BTreeMap::<String, Arc<dyn NativeWebSearchBackend>>::new();
    for route in routes {
        if !route.capabilities().native_web_search {
            continue;
        }
        match route.route_config() {
            ModelRouteConfig::OpenAi(config) => {
                let backend = OpenAiNativeWebSearchBackend::from_config(
                    route.route_id().to_string(),
                    config.clone(),
                    observer.clone(),
                )?;
                backends.insert(backend.provider().to_string(), Arc::new(backend));
            }
            ModelRouteConfig::Anthropic(config) => {
                let backend = AnthropicNativeWebSearchBackend::from_config(
                    route.route_id().to_string(),
                    config.clone(),
                    observer.clone(),
                )?;
                backends.insert(backend.provider().to_string(), Arc::new(backend));
            }
            ModelRouteConfig::Google(_) => {}
            ModelRouteConfig::OpenRouter(_) => {}
            ModelRouteConfig::XAi(config) => {
                let backend = XAiNativeWebSearchBackend::from_config(
                    route.route_id().to_string(),
                    config.clone(),
                    observer.clone(),
                )?;
                backends.insert(backend.provider().to_string(), Arc::new(backend));
            }
        }
    }
    if backends.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Arc::new(DaemonWebSearchService::new(backends))))
    }
}

fn native_web_search_target(provider: &str, endpoint: &str) -> String {
    format!("{provider}:web_search:{}", safe_url_audit_target(endpoint))
}

fn record_native_web_search_request(
    observer: &Arc<dyn RuntimeObserver>,
    provider: &str,
    endpoint: &str,
    body: &Value,
    grant_id: Option<String>,
) -> Result<String> {
    let request_digest = kheish_codec::digest_json_value(body)?;
    observer.record_external_action(external_action_trace_with_grant_id(
        "request",
        "provider_web_search",
        native_web_search_target(provider, endpoint),
        Some(request_digest.clone()),
        None,
        None,
        grant_id,
    ))?;
    Ok(request_digest)
}

fn record_native_web_search_response(
    observer: &Arc<dyn RuntimeObserver>,
    provider: &str,
    endpoint: &str,
    request_digest: &str,
    response_digest: Option<String>,
    outcome: impl Into<String>,
    grant_id: Option<String>,
) -> Result<()> {
    observer.record_external_action(external_action_trace_with_grant_id(
        "response",
        "provider_web_search",
        native_web_search_target(provider, endpoint),
        Some(request_digest.to_string()),
        response_digest,
        Some(outcome.into()),
        grant_id,
    ))
}

fn response_digest(bytes: &[u8]) -> String {
    kheish_codec::digest_text(&String::from_utf8_lossy(bytes))
}

async fn read_native_web_search_body_limited(mut response: reqwest::Response) -> Result<Vec<u8>> {
    ensure_native_web_search_content_length_allowed(&response)?;
    ensure_native_web_search_json_response(&response)?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_NATIVE_WEB_SEARCH_RESPONSE_BYTES {
            bail!(
                "native web search response exceeded {MAX_NATIVE_WEB_SEARCH_RESPONSE_BYTES} bytes"
            );
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn ensure_native_web_search_content_length_allowed(response: &reqwest::Response) -> Result<()> {
    let Some(value) = response.headers().get(CONTENT_LENGTH) else {
        return Ok(());
    };
    let length = value
        .to_str()
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    if length.is_some_and(|length| length > MAX_NATIVE_WEB_SEARCH_RESPONSE_BYTES as u64) {
        bail!("native web search response exceeded {MAX_NATIVE_WEB_SEARCH_RESPONSE_BYTES} bytes");
    }
    Ok(())
}

fn ensure_native_web_search_json_response(response: &reqwest::Response) -> Result<()> {
    let Some(value) = response.headers().get(CONTENT_TYPE) else {
        return Ok(());
    };
    let content_type = value.to_str().unwrap_or("").to_ascii_lowercase();
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    let is_json = media_type == "application/json" || media_type.ends_with("+json");
    if !is_json {
        bail!("native web search refuses non-JSON content type {media_type}");
    }
    Ok(())
}

async fn read_native_web_search_body_or_record(
    response: reqwest::Response,
    observer: &Arc<dyn RuntimeObserver>,
    provider: &str,
    endpoint: &str,
    request_digest: &str,
    grant_id: Option<String>,
    context: &str,
) -> Result<Vec<u8>> {
    match read_native_web_search_body_limited(response).await {
        Ok(bytes) => Ok(bytes),
        Err(error) => {
            let message = error.to_string();
            record_native_web_search_response(
                observer,
                provider,
                endpoint,
                request_digest,
                None,
                failed_external_action_outcome(format!("{context}: {message}")),
                grant_id,
            )?;
            Err(error).with_context(|| format!("failed to read {context}"))
        }
    }
}

struct OpenAiNativeWebSearchBackend {
    route_id: String,
    client: Client,
    config: OpenAiProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

impl OpenAiNativeWebSearchBackend {
    fn from_config(
        route_id: String,
        config: OpenAiProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self> {
        Ok(Self {
            route_id,
            client: Client::builder().build()?,
            config,
            observer,
        })
    }
}

#[async_trait]
impl NativeWebSearchBackend for OpenAiNativeWebSearchBackend {
    fn provider(&self) -> &str {
        &self.route_id
    }

    async fn search(
        &self,
        route: &WebSearchRoute,
        request: &WebSearchRequest,
    ) -> Result<Option<WebSearchBackendOutput>> {
        if route.provider != self.provider() {
            return Ok(None);
        }
        if !request.blocked_domains.is_empty() {
            return Ok(None);
        }

        let material = resolve_openai_auth_material(&self.config, false).await?;
        let endpoint = material
            .base_url_override
            .clone()
            .unwrap_or_else(|| self.config.base_url.clone());
        let grant_id = material.grant_id.clone();
        let mut tool = serde_json::Map::new();
        tool.insert("type".to_string(), json!("web_search"));
        if !request.allowed_domains.is_empty() {
            tool.insert(
                "filters".to_string(),
                json!({
                    "allowed_domains": request.allowed_domains,
                }),
            );
        }
        let body = json!({
            "model": route.model.clone().unwrap_or_else(|| self.config.model.clone()),
            "tools": [tool],
            "include": ["web_search_call.action.sources"],
            "tool_choice": "auto",
            "input": format!(
                "Use web search to find the most relevant current public sources for this query. Reply with a short summary sentence that cites the sources you used. Query: {}",
                request.query
            ),
        });
        ensure_openai_auth_material_active(&self.config, &material).await?;
        let request_digest = record_native_web_search_request(
            &self.observer,
            self.provider(),
            &endpoint,
            &body,
            grant_id.clone(),
        )?;
        let response = self
            .client
            .post(&endpoint)
            .headers(openai_headers_from_material(&material)?)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                let outcome = failed_reqwest_external_action_outcome(&error);
                record_native_web_search_response(
                    &self.observer,
                    self.provider(),
                    &endpoint,
                    &request_digest,
                    None,
                    outcome,
                    grant_id.clone(),
                )
                .err()
                .unwrap_or_else(|| error.into())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let bytes = read_native_web_search_body_or_record(
                response,
                &self.observer,
                self.provider(),
                &endpoint,
                &request_digest,
                grant_id.clone(),
                "OpenAI web search error",
            )
            .await?;
            let error = parse_openai_error_body(status, &bytes);
            record_native_web_search_response(
                &self.observer,
                self.provider(),
                &endpoint,
                &request_digest,
                Some(response_digest(&bytes)),
                if error.is_unsupported() {
                    "unsupported".to_string()
                } else {
                    failed_external_action_outcome(error.summary(self.provider()))
                },
                grant_id.clone(),
            )?;
            if error.is_unsupported() {
                return Ok(None);
            }
            return Err(anyhow!(error.summary(self.provider())));
        }

        let bytes = read_native_web_search_body_or_record(
            response,
            &self.observer,
            self.provider(),
            &endpoint,
            &request_digest,
            grant_id.clone(),
            "OpenAI web search response",
        )
        .await?;
        let payload: Value = match serde_json::from_slice(&bytes) {
            Ok(payload) => payload,
            Err(error) => {
                record_native_web_search_response(
                    &self.observer,
                    self.provider(),
                    &endpoint,
                    &request_digest,
                    Some(response_digest(&bytes)),
                    failed_external_action_outcome(format!(
                        "failed to decode OpenAI web search response: {error}"
                    )),
                    grant_id.clone(),
                )?;
                return Err(error).context("failed to decode OpenAI web search response");
            }
        };
        record_native_web_search_response(
            &self.observer,
            self.provider(),
            &endpoint,
            &request_digest,
            Some(response_digest(&bytes)),
            "200",
            grant_id,
        )?;
        Ok(normalize_openai_search_response(
            payload,
            route,
            request.limit,
            "openai_web_search",
        ))
    }
}

struct AnthropicNativeWebSearchBackend {
    route_id: String,
    client: Client,
    config: AnthropicProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

struct XAiNativeWebSearchBackend {
    route_id: String,
    client: Client,
    config: XAiProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

impl XAiNativeWebSearchBackend {
    fn from_config(
        route_id: String,
        config: XAiProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self> {
        Ok(Self {
            route_id,
            client: Client::builder().build()?,
            config,
            observer,
        })
    }
}

#[async_trait]
impl NativeWebSearchBackend for XAiNativeWebSearchBackend {
    fn provider(&self) -> &str {
        &self.route_id
    }

    async fn search(
        &self,
        route: &WebSearchRoute,
        request: &WebSearchRequest,
    ) -> Result<Option<WebSearchBackendOutput>> {
        if route.provider != self.provider() {
            return Ok(None);
        }

        let material = resolve_xai_auth_material(&self.config, false).await?;
        if request.allowed_domains.len() > 5 || request.blocked_domains.len() > 5 {
            return Ok(None);
        }
        let endpoint = material
            .base_url_override
            .clone()
            .unwrap_or_else(|| self.config.base_url.clone());
        let grant_id = material.grant_id.clone();
        let mut tool = serde_json::Map::new();
        tool.insert("type".to_string(), json!("web_search"));
        if request.enable_image_understanding {
            tool.insert("enable_image_understanding".to_string(), Value::Bool(true));
        }
        if !request.allowed_domains.is_empty() || !request.blocked_domains.is_empty() {
            if !request.allowed_domains.is_empty() && !request.blocked_domains.is_empty() {
                return Ok(None);
            }
            let mut filters = serde_json::Map::new();
            if !request.allowed_domains.is_empty() {
                filters.insert(
                    "allowed_domains".to_string(),
                    json!(request.allowed_domains),
                );
            }
            if !request.blocked_domains.is_empty() {
                filters.insert(
                    "excluded_domains".to_string(),
                    json!(request.blocked_domains),
                );
            }
            tool.insert("filters".to_string(), Value::Object(filters));
        }
        let body = json!({
            "model": route.model.clone().unwrap_or_else(|| self.config.model.clone()),
            "tools": [tool],
            "tool_choice": "auto",
            "input": format!(
                "Use web search to find the most relevant current public sources for this query. Reply with one short sentence and cite the sources you used. Query: {}",
                request.query
            ),
        });
        ensure_xai_auth_material_active(&self.config, &material).await?;
        let request_digest = record_native_web_search_request(
            &self.observer,
            self.provider(),
            &endpoint,
            &body,
            grant_id.clone(),
        )?;
        let response = self
            .client
            .post(&endpoint)
            .headers(xai_headers_from_material(&material)?)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                let outcome = failed_reqwest_external_action_outcome(&error);
                record_native_web_search_response(
                    &self.observer,
                    self.provider(),
                    &endpoint,
                    &request_digest,
                    None,
                    outcome,
                    grant_id.clone(),
                )
                .err()
                .unwrap_or_else(|| error.into())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let bytes = read_native_web_search_body_or_record(
                response,
                &self.observer,
                self.provider(),
                &endpoint,
                &request_digest,
                grant_id.clone(),
                "xAI web search error",
            )
            .await?;
            let error = parse_openai_error_body(status, &bytes);
            record_native_web_search_response(
                &self.observer,
                self.provider(),
                &endpoint,
                &request_digest,
                Some(response_digest(&bytes)),
                if error.is_unsupported() {
                    "unsupported".to_string()
                } else {
                    failed_external_action_outcome(error.summary(self.provider()))
                },
                grant_id.clone(),
            )?;
            if error.is_unsupported() {
                return Ok(None);
            }
            return Err(anyhow!(error.summary(self.provider())));
        }

        let bytes = read_native_web_search_body_or_record(
            response,
            &self.observer,
            self.provider(),
            &endpoint,
            &request_digest,
            grant_id.clone(),
            "xAI web search response",
        )
        .await?;
        let payload: Value = match serde_json::from_slice(&bytes) {
            Ok(payload) => payload,
            Err(error) => {
                record_native_web_search_response(
                    &self.observer,
                    self.provider(),
                    &endpoint,
                    &request_digest,
                    Some(response_digest(&bytes)),
                    failed_external_action_outcome(format!(
                        "failed to decode xAI web search response: {error}"
                    )),
                    grant_id.clone(),
                )?;
                return Err(error).context("failed to decode xAI web search response");
            }
        };
        record_native_web_search_response(
            &self.observer,
            self.provider(),
            &endpoint,
            &request_digest,
            Some(response_digest(&bytes)),
            "200",
            grant_id,
        )?;
        Ok(normalize_openai_search_response(
            payload,
            route,
            request.limit,
            "xai_web_search",
        ))
    }
}

impl AnthropicNativeWebSearchBackend {
    fn from_config(
        route_id: String,
        config: AnthropicProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self> {
        Ok(Self {
            route_id,
            client: Client::builder().build()?,
            config,
            observer,
        })
    }
}

#[async_trait]
impl NativeWebSearchBackend for AnthropicNativeWebSearchBackend {
    fn provider(&self) -> &str {
        &self.route_id
    }

    async fn search(
        &self,
        route: &WebSearchRoute,
        request: &WebSearchRequest,
    ) -> Result<Option<WebSearchBackendOutput>> {
        if route.provider != self.provider() {
            return Ok(None);
        }

        let mut tool = serde_json::Map::new();
        tool.insert("type".to_string(), json!("web_search_20250305"));
        tool.insert("name".to_string(), json!("web_search"));
        tool.insert("max_uses".to_string(), json!(1));
        if !request.allowed_domains.is_empty() {
            tool.insert(
                "allowed_domains".to_string(),
                json!(request.allowed_domains),
            );
        }
        if !request.blocked_domains.is_empty() {
            tool.insert(
                "blocked_domains".to_string(),
                json!(request.blocked_domains),
            );
        }
        let body = json!({
            "model": route.model.clone().unwrap_or_else(|| self.config.model.clone()),
            "max_tokens": 512,
            "messages": [{
                "role": "user",
                "content": format!(
                    "Use the web_search tool to find the most relevant current public sources for this query. After searching, reply with one short sentence and cite the sources used. Query: {}",
                    request.query
                ),
            }],
            "tools": [tool],
        });
        let mut force_refresh = false;
        let (response, endpoint, request_digest, grant_id) = loop {
            let material = resolve_anthropic_auth_material(&self.config, force_refresh).await?;
            let endpoint = material
                .base_url_override
                .clone()
                .unwrap_or_else(|| self.config.base_url.clone());
            let grant_id = material.grant_id.clone();
            ensure_anthropic_auth_material_active(&self.config, &material).await?;
            let request_digest = record_native_web_search_request(
                &self.observer,
                self.provider(),
                &endpoint,
                &body,
                grant_id.clone(),
            )?;
            let response = self
                .client
                .post(&endpoint)
                .headers(anthropic_headers_from_material(&self.config, &material)?)
                .json(&body)
                .send()
                .await
                .map_err(|error| {
                    let outcome = failed_reqwest_external_action_outcome(&error);
                    record_native_web_search_response(
                        &self.observer,
                        self.provider(),
                        &endpoint,
                        &request_digest,
                        None,
                        outcome,
                        grant_id.clone(),
                    )
                    .err()
                    .unwrap_or_else(|| error.into())
                })?;
            if response.status() == StatusCode::UNAUTHORIZED
                && self.config.request_auth_provider.is_some()
                && !force_refresh
            {
                record_native_web_search_response(
                    &self.observer,
                    self.provider(),
                    &endpoint,
                    &request_digest,
                    None,
                    failed_external_action_outcome("401-refresh"),
                    grant_id.clone(),
                )?;
                force_refresh = true;
                continue;
            }
            break (response, endpoint, request_digest, grant_id);
        };

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let bytes = read_native_web_search_body_or_record(
                response,
                &self.observer,
                self.provider(),
                &endpoint,
                &request_digest,
                grant_id.clone(),
                "Anthropic web search error",
            )
            .await?;
            let error = parse_anthropic_error_body(status, &bytes);
            record_native_web_search_response(
                &self.observer,
                self.provider(),
                &endpoint,
                &request_digest,
                Some(response_digest(&bytes)),
                if error.is_unsupported() {
                    "unsupported".to_string()
                } else {
                    failed_external_action_outcome(error.summary(self.provider()))
                },
                grant_id.clone(),
            )?;
            if error.is_unsupported() {
                return Ok(None);
            }
            return Err(anyhow!(error.summary(self.provider())));
        }

        let bytes = read_native_web_search_body_or_record(
            response,
            &self.observer,
            self.provider(),
            &endpoint,
            &request_digest,
            grant_id.clone(),
            "Anthropic web search response",
        )
        .await?;
        let payload: Value = match serde_json::from_slice(&bytes) {
            Ok(payload) => payload,
            Err(error) => {
                record_native_web_search_response(
                    &self.observer,
                    self.provider(),
                    &endpoint,
                    &request_digest,
                    Some(response_digest(&bytes)),
                    failed_external_action_outcome(format!(
                        "failed to decode Anthropic web search response: {error}"
                    )),
                    grant_id.clone(),
                )?;
                return Err(error).context("failed to decode Anthropic web search response");
            }
        };
        record_native_web_search_response(
            &self.observer,
            self.provider(),
            &endpoint,
            &request_digest,
            Some(response_digest(&bytes)),
            "200",
            grant_id,
        )?;
        Ok(normalize_anthropic_search_response(
            payload,
            route,
            request.limit,
        ))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct UpstreamSearchError {
    status: u16,
    error_type: Option<String>,
    code: Option<String>,
    message: Option<String>,
}

impl UpstreamSearchError {
    fn summary(&self, provider: &str) -> String {
        let mut parts = Vec::new();
        if let Some(error_type) = &self.error_type {
            parts.push(format!("type={error_type}"));
        }
        if let Some(code) = &self.code {
            parts.push(format!("code={code}"));
        }
        if parts.is_empty() {
            format!(
                "{provider} native web search failed with status {}",
                self.status
            )
        } else {
            format!(
                "{provider} native web search failed with status {}: {}",
                self.status,
                parts.join(", ")
            )
        }
    }

    fn is_unsupported(&self) -> bool {
        let status = self.status;
        let type_name = self
            .error_type
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let code = self
            .code
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let message = self
            .message
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        status == 404
            || type_name.contains("not_found")
            || code.contains("unsupported")
            || message.contains("web search is currently not supported")
            || message.contains("web search is not supported")
            || message.contains("tool is not enabled")
            || message.contains("tool use is disabled")
            || message.contains("unsupported")
    }
}

async fn resolve_openai_auth_material(
    config: &OpenAiProviderConfig,
    force_refresh: bool,
) -> Result<ResolvedAuthMaterial> {
    if let Some(provider) = &config.request_auth_provider {
        let result = if force_refresh {
            provider.refresh().await
        } else {
            provider.resolve().await
        };
        return result.map_err(Into::into);
    }
    let api_key = config
        .api_key
        .clone()
        .ok_or_else(|| anyhow!("missing OpenAI API key"))?;
    let mut headers = BTreeMap::new();
    headers.insert("Authorization".to_string(), format!("Bearer {api_key}"));
    if let Some(organization) = &config.organization {
        headers.insert("OpenAI-Organization".to_string(), organization.clone());
    }
    if let Some(project) = &config.project {
        headers.insert("OpenAI-Project".to_string(), project.clone());
    }
    Ok(ResolvedAuthMaterial {
        headers,
        base_url_override: None,
        grant_id: None,
        lease_id: None,
    })
}

async fn ensure_openai_auth_material_active(
    config: &OpenAiProviderConfig,
    material: &ResolvedAuthMaterial,
) -> Result<()> {
    if let Some(provider) = &config.request_auth_provider {
        provider.ensure_active(material).await?;
    }
    Ok(())
}

async fn resolve_xai_auth_material(
    config: &XAiProviderConfig,
    force_refresh: bool,
) -> Result<ResolvedAuthMaterial> {
    if let Some(provider) = &config.request_auth_provider {
        let result = if force_refresh {
            provider.refresh().await
        } else {
            provider.resolve().await
        };
        return result.map_err(Into::into);
    }
    let api_key = config
        .api_key
        .clone()
        .ok_or_else(|| anyhow!("missing xAI API key"))?;
    let mut headers = BTreeMap::new();
    headers.insert("Authorization".to_string(), format!("Bearer {api_key}"));
    Ok(ResolvedAuthMaterial {
        headers,
        base_url_override: None,
        grant_id: None,
        lease_id: None,
    })
}

async fn ensure_xai_auth_material_active(
    config: &XAiProviderConfig,
    material: &ResolvedAuthMaterial,
) -> Result<()> {
    if let Some(provider) = &config.request_auth_provider {
        provider.ensure_active(material).await?;
    }
    Ok(())
}

fn openai_headers_from_material(material: &ResolvedAuthMaterial) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    for (name, value) in &material.headers {
        let header_name = if name.eq_ignore_ascii_case("authorization") {
            AUTHORIZATION
        } else {
            HeaderName::from_bytes(name.as_bytes())?
        };
        headers.insert(header_name, HeaderValue::from_str(value)?);
    }
    Ok(headers)
}

fn xai_headers_from_material(material: &ResolvedAuthMaterial) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    for (name, value) in &material.headers {
        let header_name = if name.eq_ignore_ascii_case("authorization") {
            AUTHORIZATION
        } else {
            HeaderName::from_bytes(name.as_bytes())?
        };
        headers.insert(header_name, HeaderValue::from_str(value)?);
    }
    Ok(headers)
}

async fn resolve_anthropic_auth_material(
    config: &AnthropicProviderConfig,
    force_refresh: bool,
) -> Result<ResolvedAuthMaterial> {
    if let Some(provider) = &config.request_auth_provider {
        let result = if force_refresh {
            provider.refresh().await
        } else {
            provider.resolve().await
        };
        return result.map_err(Into::into);
    }
    let api_key = config
        .api_key
        .clone()
        .ok_or_else(|| anyhow!("missing Anthropic API key"))?;
    let mut headers = BTreeMap::new();
    headers.insert("x-api-key".to_string(), api_key);
    Ok(ResolvedAuthMaterial {
        headers,
        base_url_override: None,
        grant_id: None,
        lease_id: None,
    })
}

async fn ensure_anthropic_auth_material_active(
    config: &AnthropicProviderConfig,
    material: &ResolvedAuthMaterial,
) -> Result<()> {
    if let Some(provider) = &config.request_auth_provider {
        provider.ensure_active(material).await?;
    }
    Ok(())
}

fn anthropic_headers_from_material(
    config: &AnthropicProviderConfig,
    material: &ResolvedAuthMaterial,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    for (name, value) in &material.headers {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes())?,
            HeaderValue::from_str(value)?,
        );
    }
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_str(&config.anthropic_version)?,
    );
    if !config.beta_headers.is_empty() {
        headers.insert(
            HeaderName::from_static("anthropic-beta"),
            HeaderValue::from_str(&config.beta_headers.join(","))?,
        );
    }
    Ok(headers)
}

fn parse_openai_error_body(status: u16, body: &[u8]) -> UpstreamSearchError {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: Option<OpenAiErrorValue>,
    }
    #[derive(Deserialize)]
    struct OpenAiErrorValue {
        message: Option<String>,
        r#type: Option<String>,
        code: Option<String>,
    }

    let parsed = serde_json::from_slice::<ErrorBody>(body)
        .ok()
        .and_then(|value| value.error);
    UpstreamSearchError {
        status,
        error_type: parsed.as_ref().and_then(|value| value.r#type.clone()),
        code: parsed.as_ref().and_then(|value| value.code.clone()),
        message: parsed.and_then(|value| value.message),
    }
}

fn parse_anthropic_error_body(status: u16, body: &[u8]) -> UpstreamSearchError {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: Option<AnthropicErrorValue>,
    }
    #[derive(Deserialize)]
    struct AnthropicErrorValue {
        r#type: Option<String>,
        message: Option<String>,
    }

    let parsed = serde_json::from_slice::<ErrorBody>(body)
        .ok()
        .and_then(|value| value.error);
    UpstreamSearchError {
        status,
        error_type: parsed.as_ref().and_then(|value| value.r#type.clone()),
        code: None,
        message: parsed.and_then(|value| value.message),
    }
}

fn normalize_openai_search_response(
    response: Value,
    route: &WebSearchRoute,
    limit: usize,
    engine: &str,
) -> Option<WebSearchBackendOutput> {
    let mut cited = BTreeMap::<String, WebSearchHit>::new();
    let mut seen_urls = BTreeSet::new();
    let mut source_urls = Vec::new();
    let mut saw_native_search = false;

    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("web_search_call") => {
                    saw_native_search = true;
                    if let Some(sources) = item
                        .get("action")
                        .and_then(|action| action.get("sources"))
                        .and_then(Value::as_array)
                    {
                        for source in sources {
                            let Some(url) = source.get("url").and_then(Value::as_str) else {
                                continue;
                            };
                            if seen_urls.insert(url.to_string()) {
                                source_urls.push(url.to_string());
                            }
                        }
                    }
                }
                Some("message") => {
                    let Some(content) = item.get("content").and_then(Value::as_array) else {
                        continue;
                    };
                    for block in content {
                        if block.get("type").and_then(Value::as_str) != Some("output_text") {
                            continue;
                        }
                        let text = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let Some(annotations) = block.get("annotations").and_then(Value::as_array)
                        else {
                            continue;
                        };
                        for annotation in annotations {
                            if annotation.get("type").and_then(Value::as_str)
                                != Some("url_citation")
                            {
                                continue;
                            }
                            let Some(title) = annotation.get("title").and_then(Value::as_str)
                            else {
                                continue;
                            };
                            let Some(url) = annotation.get("url").and_then(Value::as_str) else {
                                continue;
                            };
                            let start_index = annotation
                                .get("start_index")
                                .and_then(Value::as_u64)
                                .map(|value| value as usize)
                                .unwrap_or_default();
                            let end_index = annotation
                                .get("end_index")
                                .and_then(Value::as_u64)
                                .map(|value| value as usize)
                                .unwrap_or_default();
                            let snippet = text_snippet_by_char_range(text, start_index, end_index);
                            saw_native_search = true;
                            cited.entry(url.to_string()).or_insert_with(|| {
                                WebSearchHit::new(title.to_string(), url.to_string(), snippet)
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    if let Some(citations) = response.get("citations").and_then(Value::as_array) {
        for citation in citations {
            let Some(url) = citation.get("url").and_then(Value::as_str) else {
                continue;
            };
            let title = citation
                .get("title")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .unwrap_or_else(|| fallback_title_for_url(url));
            let snippet = citation
                .get("cited_text")
                .or_else(|| citation.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            saw_native_search = true;
            cited
                .entry(url.to_string())
                .or_insert_with(|| WebSearchHit::new(title, url.to_string(), snippet));
        }
    }

    if !saw_native_search {
        return None;
    }

    let mut results = cited.into_values().collect::<Vec<_>>();
    for url in source_urls {
        if results.iter().any(|hit| hit.url == url) {
            continue;
        }
        results.push(WebSearchHit::new(fallback_title_for_url(&url), url, ""));
    }
    let results = sanitize_native_web_search_hits(results, limit);
    if results.is_empty() {
        return None;
    }

    Some(WebSearchBackendOutput {
        engine: engine.to_string(),
        implementation: "provider_native".to_string(),
        provider: Some(route.provider.clone()),
        model: route.model.clone(),
        results,
    })
}

#[derive(Debug, Deserialize)]
struct AnthropicSearchHit {
    url: String,
    title: String,
}

fn normalize_anthropic_search_response(
    response: Value,
    route: &WebSearchRoute,
    limit: usize,
) -> Option<WebSearchBackendOutput> {
    let mut results = BTreeMap::<String, WebSearchHit>::new();
    let mut source_urls = Vec::new();
    let mut saw_native_search = false;

    if let Some(content) = response.get("content").and_then(Value::as_array) {
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("web_search_tool_result") => {
                    saw_native_search = true;
                    if let Some(hits) = block.get("content").cloned().and_then(|value| {
                        serde_json::from_value::<Vec<AnthropicSearchHit>>(value).ok()
                    }) {
                        for hit in hits {
                            source_urls.push((hit.url, hit.title));
                        }
                    }
                }
                Some("text") => {
                    let Some(citations) = block.get("citations").and_then(Value::as_array) else {
                        continue;
                    };
                    for citation in citations {
                        let Some(url) = citation.get("url").and_then(Value::as_str) else {
                            continue;
                        };
                        let Some(title) = citation.get("title").and_then(Value::as_str) else {
                            continue;
                        };
                        let cited_text = citation
                            .get("cited_text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        saw_native_search = true;
                        results.entry(url.to_string()).or_insert_with(|| {
                            WebSearchHit::new(
                                title.to_string(),
                                url.to_string(),
                                cited_text.to_string(),
                            )
                        });
                    }
                }
                _ => {}
            }
        }
    }

    if !saw_native_search {
        return None;
    }

    for (url, title) in source_urls {
        results
            .entry(url.clone())
            .or_insert_with(|| WebSearchHit::new(title, url, ""));
    }

    let results = sanitize_native_web_search_hits(results.into_values().collect(), limit);
    if results.is_empty() {
        return None;
    }

    Some(WebSearchBackendOutput {
        engine: "anthropic_web_search_20250305".to_string(),
        implementation: "provider_native".to_string(),
        provider: Some(route.provider.clone()),
        model: route.model.clone(),
        results,
    })
}

fn sanitize_native_web_search_hits(results: Vec<WebSearchHit>, limit: usize) -> Vec<WebSearchHit> {
    let mut seen = BTreeSet::new();
    results
        .into_iter()
        .filter_map(sanitize_native_web_search_hit)
        .filter(|hit| seen.insert(hit.url.clone()))
        .take(limit)
        .collect()
}

fn sanitize_native_web_search_hit(mut hit: WebSearchHit) -> Option<WebSearchHit> {
    let parsed = Url::parse(hit.url.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    let host = parsed
        .host_str()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if is_localhost_host(&host) {
        return None;
    }
    if let Ok(ip) = host.parse::<IpAddr>()
        && is_private_or_special_ip(ip)
    {
        return None;
    }
    hit.url = parsed.to_string();
    hit.domain = host;
    hit.title = truncate_native_search_text(&hit.title, MAX_NATIVE_WEB_SEARCH_TITLE_CHARS);
    if hit.title.is_empty() {
        hit.title = fallback_title_for_url(&hit.url);
    }
    hit.snippet = truncate_native_search_text(&hit.snippet, MAX_NATIVE_WEB_SEARCH_SNIPPET_CHARS);
    Some(hit)
}

fn truncate_native_search_text(value: &str, max_chars: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    normalized.chars().take(max_chars).collect()
}

fn is_localhost_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost")
}

fn is_private_or_special_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_private_or_special_ipv4(ip),
        IpAddr::V6(ip) => is_private_or_special_ipv6(ip),
    }
}

fn is_private_or_special_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || ip.is_unspecified()
        || ip.octets()[0] == 0
}

fn is_private_or_special_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_private_or_special_ipv4(mapped);
    }
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || ip.is_multicast()
}

fn fallback_title_for_url(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(ToString::to_string))
        .unwrap_or_else(|| url.to_string())
}

fn text_snippet_by_char_range(text: &str, start: usize, end: usize) -> String {
    if start >= end {
        return String::new();
    }
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Result;
    use async_trait::async_trait;
    use kheish_auth::{RequestAuthProvider, ResolvedAuthMaterial};
    use kheish_runtime::{
        AnthropicProviderConfig, InMemoryObserver, NoopObserver, OpenAiProviderConfig,
        TraceEventKind,
    };
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{
        AnthropicNativeWebSearchBackend, NativeWebSearchBackend, OpenAiNativeWebSearchBackend,
        UpstreamSearchError, fallback_title_for_url, normalize_anthropic_search_response,
        normalize_openai_search_response, text_snippet_by_char_range,
    };
    use kheish_coding_tools::{WebSearchRequest, WebSearchRoute};

    #[test]
    fn upstream_error_unsupported_detection_matches_capability_failures() {
        let error = UpstreamSearchError {
            status: 400,
            error_type: Some("invalid_request_error".to_string()),
            code: Some("unsupported_value".to_string()),
            message: Some("web search is not supported for this model".to_string()),
        };
        assert!(error.is_unsupported());
    }

    #[test]
    fn fallback_title_uses_host_when_available() {
        assert_eq!(
            fallback_title_for_url("https://sqlite.org/wal.html"),
            "sqlite.org"
        );
    }

    #[test]
    fn snippets_slice_by_character_range() {
        assert_eq!(text_snippet_by_char_range("hello world", 0, 5), "hello");
    }

    #[test]
    fn openai_normalization_ignores_unknown_output_items() {
        let route = WebSearchRoute {
            session_id: "session-1".to_string(),
            provider: "openai".to_string(),
            model: Some("gpt-5.4".to_string()),
            run_id: Some("run-1".to_string()),
            agent_id: Some("agent-1".to_string()),
        };
        let output = normalize_openai_search_response(
            json!({
                "output": [
                    { "type": "reasoning", "summary": [] },
                    {
                        "type": "web_search_call",
                        "action": { "type": "search", "sources": [{ "url": "https://sqlite.org/wal.html" }] }
                    },
                    {
                        "type": "message",
                        "content": [{
                            "type": "output_text",
                            "text": "SQLite WAL docs are here.",
                            "annotations": [{
                                "type": "url_citation",
                                "title": "Write-Ahead Logging",
                                "url": "https://sqlite.org/wal.html",
                                "start_index": 0,
                                "end_index": 6
                            }]
                        }]
                    }
                ]
            }),
            &route,
            5,
            "openai_web_search",
        );
        let output = output.expect("expected one native OpenAI search result");
        assert_eq!(output.implementation, "provider_native");
        assert_eq!(output.results.len(), 1);
        assert_eq!(output.results[0].url, "https://sqlite.org/wal.html");
    }

    #[test]
    fn anthropic_normalization_ignores_unknown_content_blocks() {
        let route = WebSearchRoute {
            session_id: "session-1".to_string(),
            provider: "anthropic".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            run_id: Some("run-1".to_string()),
            agent_id: Some("agent-1".to_string()),
        };
        let output = normalize_anthropic_search_response(
            json!({
                "content": [
                    { "type": "thinking", "text": "..." },
                    {
                        "type": "web_search_tool_result",
                        "content": [{ "url": "https://sqlite.org/wal.html", "title": "Write-Ahead Logging" }]
                    },
                    {
                        "type": "text",
                        "citations": [{
                            "url": "https://sqlite.org/wal.html",
                            "title": "Write-Ahead Logging",
                            "cited_text": "WAL mode"
                        }]
                    }
                ]
            }),
            &route,
            5,
        );
        let output = output.expect("expected one native Anthropic search result");
        assert_eq!(output.implementation, "provider_native");
        assert_eq!(output.results.len(), 1);
        assert_eq!(output.results[0].url, "https://sqlite.org/wal.html");
    }

    #[tokio::test]
    async fn anthropic_web_search_refreshes_once_after_401() -> Result<()> {
        #[derive(Default)]
        struct RefreshingAuthProvider {
            resolves: AtomicUsize,
            refreshes: AtomicUsize,
        }

        #[async_trait]
        impl RequestAuthProvider for RefreshingAuthProvider {
            async fn resolve(&self) -> Result<ResolvedAuthMaterial> {
                self.resolves.fetch_add(1, Ordering::SeqCst);
                Ok(ResolvedAuthMaterial {
                    headers: [("x-api-key".to_string(), "stale-key".to_string())]
                        .into_iter()
                        .collect(),
                    base_url_override: None,
                    grant_id: None,
                    lease_id: None,
                })
            }

            async fn refresh(&self) -> Result<ResolvedAuthMaterial> {
                self.refreshes.fetch_add(1, Ordering::SeqCst);
                Ok(ResolvedAuthMaterial {
                    headers: [("x-api-key".to_string(), "fresh-key".to_string())]
                        .into_iter()
                        .collect(),
                    base_url_override: None,
                    grant_id: None,
                    lease_id: None,
                })
            }

            async fn ensure_active(&self, _material: &ResolvedAuthMaterial) -> Result<()> {
                Ok(())
            }
        }

        let auth_provider = Arc::new(RefreshingAuthProvider::default());
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            for (index, expected_key) in ["stale-key", "fresh-key"].into_iter().enumerate() {
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
                let request_text = String::from_utf8(request).expect("request should be utf-8");
                let api_key = request_text
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("x-api-key")
                                .then_some(value.trim())
                        })
                    })
                    .unwrap_or_default();
                assert_eq!(api_key, expected_key);

                let (status, body) = if index == 0 {
                    (
                        "401 Unauthorized",
                        json!({"error": {"message": "expired"}}).to_string(),
                    )
                } else {
                    (
                        "200 OK",
                        json!({
                            "content": [
                                {
                                    "type": "web_search_tool_result",
                                    "content": [
                                        {
                                            "url": "https://example.com/current",
                                            "title": "Current Example"
                                        }
                                    ]
                                },
                                {
                                    "type": "text",
                                    "citations": [
                                        {
                                            "url": "https://example.com/current",
                                            "title": "Current Example",
                                            "cited_text": "current source"
                                        }
                                    ]
                                }
                            ]
                        })
                        .to_string(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("response write should succeed");
            }
        });

        let mut config = AnthropicProviderConfig::new("claude-test", "unused-key");
        config.base_url = format!("http://{address}/v1/messages");
        config.api_key = None;
        config.request_auth_provider = Some(auth_provider.clone());
        let backend = AnthropicNativeWebSearchBackend::from_config(
            "anthropic".to_string(),
            config,
            Arc::new(NoopObserver),
        )?;
        let output = backend
            .search(
                &WebSearchRoute {
                    session_id: "session-1".to_string(),
                    provider: "anthropic".to_string(),
                    model: Some("claude-test".to_string()),
                    run_id: Some("run-1".to_string()),
                    agent_id: Some("agent-1".to_string()),
                },
                &WebSearchRequest {
                    query: "current example".to_string(),
                    allowed_domains: Vec::new(),
                    blocked_domains: Vec::new(),
                    limit: 5,
                    enable_image_understanding: false,
                },
            )
            .await?
            .expect("native Anthropic web search should recover after refresh");

        assert_eq!(output.results.len(), 1);
        assert_eq!(output.results[0].url, "https://example.com/current");
        assert_eq!(auth_provider.resolves.load(Ordering::SeqCst), 1);
        assert_eq!(auth_provider.refreshes.load(Ordering::SeqCst), 1);
        server.await.expect("server task should finish");
        Ok(())
    }

    #[tokio::test]
    async fn openai_web_search_revalidates_auth_before_upstream_request() -> Result<()> {
        struct RevokedAuthProvider;

        #[async_trait]
        impl RequestAuthProvider for RevokedAuthProvider {
            async fn resolve(&self) -> Result<ResolvedAuthMaterial> {
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

            async fn refresh(&self) -> Result<ResolvedAuthMaterial> {
                self.resolve().await
            }

            async fn ensure_active(&self, material: &ResolvedAuthMaterial) -> Result<()> {
                anyhow::bail!(
                    "credential lease {} is not active",
                    material.lease_id.as_deref().unwrap_or("<missing>")
                )
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let mut config = OpenAiProviderConfig::new("gpt-5.4", "unused-key");
        config.base_url = format!("http://{address}/v1/responses");
        config.api_key = None;
        config.request_auth_provider = Some(Arc::new(RevokedAuthProvider));
        let backend = OpenAiNativeWebSearchBackend::from_config(
            "openai".to_string(),
            config,
            Arc::new(NoopObserver),
        )?;

        let error = backend
            .search(
                &WebSearchRoute {
                    session_id: "session-1".to_string(),
                    provider: "openai".to_string(),
                    model: Some("gpt-5.4".to_string()),
                    run_id: Some("run-1".to_string()),
                    agent_id: Some("agent-1".to_string()),
                },
                &WebSearchRequest {
                    query: "current example".to_string(),
                    allowed_domains: Vec::new(),
                    blocked_domains: Vec::new(),
                    limit: 5,
                    enable_image_understanding: false,
                },
            )
            .await
            .expect_err("revoked credential lease should block native search");
        assert!(error.to_string().contains("not active"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "native search should not reach upstream after lease revalidation failed"
        );
        Ok(())
    }

    #[test]
    fn openai_normalization_returns_none_when_the_response_never_used_search() {
        let route = WebSearchRoute {
            session_id: "session-1".to_string(),
            provider: "openai".to_string(),
            model: Some("gpt-5.4".to_string()),
            run_id: Some("run-1".to_string()),
            agent_id: Some("agent-1".to_string()),
        };
        let output = normalize_openai_search_response(
            json!({
                "output": [
                    {
                        "type": "message",
                        "content": [{
                            "type": "output_text",
                            "text": "No search was used.",
                            "annotations": []
                        }]
                    }
                ]
            }),
            &route,
            5,
            "openai_web_search",
        );
        assert!(output.is_none());
    }

    #[test]
    fn native_normalization_filters_private_urls_and_bounds_metadata() {
        let route = WebSearchRoute {
            session_id: "session-1".to_string(),
            provider: "openai".to_string(),
            model: Some("gpt-5.4".to_string()),
            run_id: Some("run-1".to_string()),
            agent_id: Some("agent-1".to_string()),
        };
        let long_title = "A".repeat(2_000);
        let long_snippet = "B".repeat(4_000);
        let output = normalize_openai_search_response(
            json!({
                "citations": [
                    {
                        "url": "http://127.0.0.1/admin",
                        "title": "Loopback",
                        "cited_text": "private"
                    },
                    {
                        "url": "https://example.com/docs",
                        "title": long_title,
                        "cited_text": long_snippet
                    }
                ]
            }),
            &route,
            5,
            "openai_web_search",
        )
        .expect("expected safe native citation to survive");

        assert_eq!(output.results.len(), 1);
        assert_eq!(output.results[0].url, "https://example.com/docs");
        assert_eq!(output.results[0].domain, "example.com");
        assert_eq!(output.results[0].title.chars().count(), 512);
        assert_eq!(output.results[0].snippet.chars().count(), 2048);
    }

    #[test]
    fn anthropic_normalization_drops_private_and_invalid_citations() {
        let route = WebSearchRoute {
            session_id: "session-1".to_string(),
            provider: "anthropic".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            run_id: Some("run-1".to_string()),
            agent_id: Some("agent-1".to_string()),
        };
        let output = normalize_anthropic_search_response(
            json!({
                "content": [
                    {
                        "type": "web_search_tool_result",
                        "content": [
                            { "url": "http://localhost/private", "title": "Localhost" },
                            { "url": "javascript:alert(1)", "title": "Script" },
                            { "url": "https://example.com/current", "title": "Current Example" }
                        ]
                    },
                    {
                        "type": "text",
                        "citations": [
                            {
                                "url": "http://169.254.169.254/latest/meta-data",
                                "title": "Metadata",
                                "cited_text": "private"
                            },
                            {
                                "url": "https://example.com/current",
                                "title": "Current Example",
                                "cited_text": "current source"
                            }
                        ]
                    }
                ]
            }),
            &route,
            5,
        )
        .expect("expected safe Anthropic citation to survive");

        assert_eq!(output.results.len(), 1);
        assert_eq!(output.results[0].url, "https://example.com/current");
        assert_eq!(output.results[0].domain, "example.com");
    }

    #[tokio::test]
    async fn openai_native_web_search_rejects_oversized_response_body_and_audits() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
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
            let body = "x".repeat(super::MAX_NATIVE_WEB_SEARCH_RESPONSE_BYTES + 1);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });

        let mut config = OpenAiProviderConfig::new("gpt-test", "test-key");
        config.base_url = format!("http://{address}/v1/responses");
        let backend = OpenAiNativeWebSearchBackend::from_config(
            "openai".to_string(),
            config,
            observer.clone(),
        )?;
        let error = backend
            .search(
                &WebSearchRoute {
                    session_id: "session-1".to_string(),
                    provider: "openai".to_string(),
                    model: Some("gpt-test".to_string()),
                    run_id: Some("run-1".to_string()),
                    agent_id: Some("agent-1".to_string()),
                },
                &WebSearchRequest {
                    query: "current example".to_string(),
                    allowed_domains: Vec::new(),
                    blocked_domains: Vec::new(),
                    limit: 5,
                    enable_image_understanding: false,
                },
            )
            .await
            .expect_err("oversized native web search response should fail");
        assert!(format!("{error:#}").contains("exceeded"));
        server.await.expect("server task should finish");

        assert!(observer.traces().iter().any(|trace| {
            matches!(
                &trace.kind,
                TraceEventKind::ExternalAction {
                    phase,
                    kind,
                    outcome: Some(outcome),
                    ..
                } if phase == "response"
                    && kind == "provider_web_search"
                    && outcome == "failed:response_too_large"
            )
        }));
        Ok(())
    }

    #[tokio::test]
    async fn openai_native_web_search_rejects_non_json_response_and_audits() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
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
            let body = "<html>not json</html>";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });

        let mut config = OpenAiProviderConfig::new("gpt-test", "test-key");
        config.base_url = format!("http://{address}/v1/responses");
        let backend = OpenAiNativeWebSearchBackend::from_config(
            "openai".to_string(),
            config,
            observer.clone(),
        )?;
        let error = backend
            .search(
                &WebSearchRoute {
                    session_id: "session-1".to_string(),
                    provider: "openai".to_string(),
                    model: Some("gpt-test".to_string()),
                    run_id: Some("run-1".to_string()),
                    agent_id: Some("agent-1".to_string()),
                },
                &WebSearchRequest {
                    query: "current example".to_string(),
                    allowed_domains: Vec::new(),
                    blocked_domains: Vec::new(),
                    limit: 5,
                    enable_image_understanding: false,
                },
            )
            .await
            .expect_err("non-json native web search response should fail");
        assert!(format!("{error:#}").contains("non-JSON"));
        server.await.expect("server task should finish");

        assert!(observer.traces().iter().any(|trace| {
            matches!(
                &trace.kind,
                TraceEventKind::ExternalAction {
                    phase,
                    kind,
                    outcome: Some(outcome),
                    ..
                } if phase == "response"
                    && kind == "provider_web_search"
                    && outcome == "failed:unsupported_content_type"
            )
        }));
        Ok(())
    }

    #[test]
    fn xai_normalization_keeps_provider_native_metadata() {
        let route = WebSearchRoute {
            session_id: "session-1".to_string(),
            provider: "xai".to_string(),
            model: Some("grok-4.20-0309-reasoning".to_string()),
            run_id: Some("run-1".to_string()),
            agent_id: Some("agent-1".to_string()),
        };
        let output = normalize_openai_search_response(
            json!({
                "output": [
                    {
                        "type": "web_search_call",
                        "action": { "type": "search", "sources": [{ "url": "https://sqlite.org/wal.html" }] }
                    },
                    {
                        "type": "message",
                        "content": [{
                            "type": "output_text",
                            "text": "SQLite WAL docs are here.",
                            "annotations": [{
                                "type": "url_citation",
                                "title": "Write-Ahead Logging",
                                "url": "https://sqlite.org/wal.html",
                                "start_index": 0,
                                "end_index": 6
                            }]
                        }]
                    }
                ]
            }),
            &route,
            5,
            "xai_web_search",
        )
        .expect("expected one native xAI search result");
        assert_eq!(output.implementation, "provider_native");
        assert_eq!(output.provider.as_deref(), Some("xai"));
        assert_eq!(output.engine, "xai_web_search");
    }

    #[test]
    fn xai_normalization_reads_top_level_citations() {
        let route = WebSearchRoute {
            session_id: "session-1".to_string(),
            provider: "xai".to_string(),
            model: Some("grok-4.20-0309-reasoning".to_string()),
            run_id: Some("run-1".to_string()),
            agent_id: Some("agent-1".to_string()),
        };
        let output = normalize_openai_search_response(
            json!({
                "citations": [
                    {
                        "url": "https://docs.x.ai/docs",
                        "title": "xAI Docs",
                        "cited_text": "Responses API"
                    }
                ]
            }),
            &route,
            5,
            "xai_web_search",
        )
        .expect("expected native xAI citations to normalize");
        assert_eq!(output.engine, "xai_web_search");
        assert_eq!(output.results.len(), 1);
        assert_eq!(output.results[0].url, "https://docs.x.ai/docs");
        assert_eq!(output.results[0].snippet, "Responses API");
    }
}
