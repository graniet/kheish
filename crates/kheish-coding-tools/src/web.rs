use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolInputKind,
    ToolSchemaField,
};
use kheish_types::ContextUpdate;
use reqwest::Url;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION, USER_AGENT};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::lookup_host;

use crate::shared::{
    SharedConfig, optional_bool_field, optional_string_array_field, optional_usize_field,
    string_field, tool_schema, truncate_text,
};

const MAX_WEB_FETCH_REDIRECTS: usize = 5;
const MAX_WEB_FETCH_HEADER_VALUE_BYTES: usize = 4096;

/// One provider-neutral web search request accepted by the `web_search` tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchRequest {
    /// The search query to execute.
    pub query: String,
    /// Optional allow-list of domains to keep.
    pub allowed_domains: Vec<String>,
    /// Optional deny-list of domains to remove.
    pub blocked_domains: Vec<String>,
    /// The effective maximum number of hits to return.
    pub limit: usize,
    /// Enables provider-native image understanding during search when supported.
    pub enable_image_understanding: bool,
}

/// The effective provider/model route attached to one tool execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchRoute {
    /// The owning session identifier.
    pub session_id: String,
    /// The provider pinned to the current run.
    pub provider: String,
    /// The pinned model, when known.
    pub model: Option<String>,
    /// The daemon run identifier, when available.
    pub run_id: Option<String>,
    /// The agent identifier, when available.
    pub agent_id: Option<String>,
}

/// One normalized provider-backed search result set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchBackendOutput {
    /// The stable engine identifier reported to the model.
    pub engine: String,
    /// The concrete implementation class used for the search.
    pub implementation: String,
    /// The provider that executed the search when provider-native search was used.
    pub provider: Option<String>,
    /// The model that executed the search when known.
    pub model: Option<String>,
    /// The normalized search hits.
    pub results: Vec<WebSearchHit>,
}

/// Provider-aware extension point for native `web_search` backends.
#[async_trait]
pub trait ProviderWebSearchService: Send + Sync {
    /// Executes one provider-native web search for the current route.
    ///
    /// Returning `Ok(None)` indicates that the provider-native backend is not
    /// available for the current route or request and that the caller should
    /// fall back to the local daemon-owned implementation.
    async fn search(
        &self,
        route: &WebSearchRoute,
        request: &WebSearchRequest,
    ) -> Result<Option<WebSearchBackendOutput>>;
}

pub(crate) struct WebSearchTool {
    shared: Arc<SharedConfig>,
    provider_search: Option<Arc<dyn ProviderWebSearchService>>,
}

impl WebSearchTool {
    pub(crate) fn with_provider_search(
        shared: Arc<SharedConfig>,
        provider_search: Option<Arc<dyn ProviderWebSearchService>>,
    ) -> Self {
        Self {
            shared,
            provider_search,
        }
    }

    async fn execute_local_search(
        &self,
        request: &WebSearchRequest,
    ) -> Result<WebSearchBackendOutput> {
        let response = self
            .shared
            .http
            .get("https://html.duckduckgo.com/html/")
            .header(
                USER_AGENT,
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36",
            )
            .query(&[("q", request.query.as_str())])
            .send()
            .await
            .with_context(|| format!("failed to search the web for `{}`", request.query))?
            .error_for_status()
            .with_context(|| format!("web search request failed for `{}`", request.query))?;
        ensure_content_length_allowed(&response, self.shared.max_read_bytes)?;
        ensure_text_response(&response)?;
        let (body, _) = read_response_body_limited(response, self.shared.max_read_bytes).await?;
        let body = String::from_utf8_lossy(&body).to_string();
        let hits = filter_search_hits_by_domain(
            parse_duckduckgo_results(&body),
            &request.allowed_domains,
            &request.blocked_domains,
        );
        Ok(WebSearchBackendOutput {
            engine: "duckduckgo_html".to_string(),
            implementation: "local".to_string(),
            provider: None,
            model: None,
            results: hits.into_iter().take(request.limit).collect(),
        })
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "web_search".to_string(),
            description: "Searches the public web for current information and returns titled results with URLs and snippets. Use this before web_fetch when you need up-to-date sources, and cite relevant URLs when answering.".to_string(),
            schema: tool_schema(vec![
                ToolSchemaField {
                    name: "query".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: true,
                    description: Some("Search query for current public web results.".to_string()),
                },
                ToolSchemaField {
                    name: "allowed_domains".to_string(),
                    kind: ToolInputKind::Array,
                    item_kind: Some(ToolInputKind::String),
                    structured_schema: None,
                    required: false,
                    description: Some("Optional domains to keep in the returned results.".to_string()),
                },
                ToolSchemaField {
                    name: "blocked_domains".to_string(),
                    kind: ToolInputKind::Array,
                    item_kind: Some(ToolInputKind::String),
                    structured_schema: None,
                    required: false,
                    description: Some("Optional domains to exclude from the returned results.".to_string()),
                },
                ToolSchemaField {
                    name: "limit".to_string(),
                    kind: ToolInputKind::Number,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some("Optional maximum number of search hits to return.".to_string()),
                },
                ToolSchemaField {
                    name: "enable_image_understanding".to_string(),
                    kind: ToolInputKind::Boolean,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some(
                        "When supported by the active provider, allows web_search to analyze images found during browsing."
                            .to_string(),
                    ),
                },
            ]),
            timeout_ms: 60_000,
            sandbox: SandboxProfile::NetworkEnabled,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let request = parse_web_search_request(&input, self.shared.max_results)?;
        let mut execution = if let (Some(route), Some(provider_search)) = (
            web_search_route_from_metadata(&ctx.metadata),
            self.provider_search.as_ref(),
        ) {
            provider_search.search(&route, &request).await?
        } else {
            None
        }
        .unwrap_or(self.execute_local_search(&request).await?);
        execution.results = filter_search_hits_by_domain(
            execution.results,
            &request.allowed_domains,
            &request.blocked_domains,
        )
        .into_iter()
        .filter(|hit| is_public_search_result_url(&hit.url))
        .take(request.limit)
        .collect::<Vec<_>>();
        let updates = execution
            .results
            .iter()
            .map(|hit| ContextUpdate::WebResourceVisited {
                uri: hit.url.clone(),
            })
            .collect::<Vec<_>>();
        let sources_markdown = execution
            .results
            .iter()
            .map(|hit| {
                let title = if hit.title.trim().is_empty() {
                    hit.url.as_str()
                } else {
                    hit.title.as_str()
                };
                format!(
                    "- [{}]({})",
                    escape_markdown_link_text(title),
                    escape_markdown_link_url(&hit.url)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let citations = execution
            .results
            .iter()
            .map(|hit| {
                json!({
                    "title": hit.title,
                    "url": hit.url,
                    "domain": hit.domain,
                    "snippet": hit.snippet,
                })
            })
            .collect::<Vec<_>>();
        Ok(ToolExecutionOutput::with_updates(
            json!({
                "query": request.query,
                "engine": execution.engine,
                "implementation": execution.implementation,
                "provider": execution.provider,
                "model": execution.model,
                "results": execution.results,
                "returned_results": execution.results.len(),
                "citations": citations,
                "allowed_domains": request.allowed_domains,
                "blocked_domains": request.blocked_domains,
                "enable_image_understanding": request.enable_image_understanding,
                "sources_markdown": sources_markdown,
                "citation_requirement": "After using these results, include a `Sources:` section that lists relevant sources as markdown hyperlinks in the format `- [Title](URL)`. Do not output bare URLs.",
            }),
            updates,
        ))
    }
}

pub(crate) struct WebFetchTool {
    shared: Arc<SharedConfig>,
    dns_resolver: Arc<dyn WebFetchDnsResolver>,
}

impl WebFetchTool {
    pub(crate) fn new(shared: Arc<SharedConfig>) -> Self {
        Self {
            shared,
            dns_resolver: Arc::new(SystemWebFetchDnsResolver),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_dns_resolver(
        shared: Arc<SharedConfig>,
        dns_resolver: Arc<dyn WebFetchDnsResolver>,
    ) -> Self {
        Self {
            shared,
            dns_resolver,
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "web_fetch".to_string(),
            description: "Fetches one HTTP resource and returns the response body.".to_string(),
            schema: tool_schema(vec![ToolSchemaField {
                name: "url".to_string(),
                kind: ToolInputKind::String,
                item_kind: None,
                structured_schema: None,
                required: true,
                description: Some("HTTP or HTTPS URL.".to_string()),
            }]),
            timeout_ms: 15_000,
            sandbox: SandboxProfile::NetworkEnabled,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let url = string_field(&input, "url")?;
        let allow_private_network = ctx
            .metadata
            .get("allow_private_network")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let requested_url = Url::parse(&url).with_context(|| format!("invalid URL {url}"))?;
        let (response, final_url, redirects) = fetch_with_hardened_redirects(
            &self.shared,
            self.dns_resolver.as_ref(),
            requested_url,
            self.shared.max_read_bytes,
            allow_private_network,
        )
        .await?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    Value::String(truncate_header_value(value.to_str().unwrap_or(""))),
                )
            })
            .collect::<serde_json::Map<String, Value>>();
        ensure_text_response(&response)?;
        let (body_bytes, truncated) =
            read_response_body_limited(response, self.shared.max_read_bytes).await?;
        let body = String::from_utf8_lossy(&body_bytes).to_string();
        let (body, text_truncated) = truncate_text(&body, self.shared.max_read_bytes);
        Ok(ToolExecutionOutput::with_updates(
            json!({
                "url": url,
                "final_url": final_url.as_str(),
                "redirects": redirects,
                "status": status,
                "headers": headers,
                "body": body,
                "bytes_read": body_bytes.len(),
                "truncated": truncated || text_truncated,
            }),
            vec![ContextUpdate::WebResourceVisited {
                uri: final_url.to_string(),
            }],
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub domain: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PinnedFetchResolution {
    host: String,
    addresses: Vec<SocketAddr>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ValidatedFetchTarget {
    pinned_resolution: Option<PinnedFetchResolution>,
}

#[async_trait]
pub(crate) trait WebFetchDnsResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>>;
}

struct SystemWebFetchDnsResolver;

#[async_trait]
impl WebFetchDnsResolver for SystemWebFetchDnsResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        Ok(lookup_host((host, port))
            .await
            .with_context(|| format!("failed to resolve {host}"))?
            .collect::<Vec<_>>())
    }
}

impl WebSearchHit {
    /// Builds one normalized hit from provider-native result fields.
    pub fn new(
        title: impl Into<String>,
        url: impl Into<String>,
        snippet: impl Into<String>,
    ) -> Self {
        let url = url.into();
        let domain = Url::parse(&url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(ToString::to_string))
            .unwrap_or_default();
        Self {
            title: title.into(),
            url,
            snippet: snippet.into(),
            domain,
        }
    }
}

fn parse_web_search_request(input: &Value, max_results: usize) -> Result<WebSearchRequest> {
    let query = string_field(input, "query")?;
    let query = query.trim();
    if query.len() < 2 {
        bail!("web_search query must be at least 2 characters long");
    }
    let allowed_domains =
        normalize_domain_filters(optional_string_array_field(input, "allowed_domains"));
    let blocked_domains =
        normalize_domain_filters(optional_string_array_field(input, "blocked_domains"));
    let limit = optional_usize_field(input, "limit")
        .unwrap_or(8)
        .min(max_results)
        .max(1);
    let enable_image_understanding =
        optional_bool_field(input, "enable_image_understanding").unwrap_or(false);
    Ok(WebSearchRequest {
        query: query.to_string(),
        allowed_domains,
        blocked_domains,
        limit,
        enable_image_understanding,
    })
}

fn web_search_route_from_metadata(metadata: &Value) -> Option<WebSearchRoute> {
    Some(WebSearchRoute {
        session_id: metadata.get("session_id")?.as_str()?.to_string(),
        provider: metadata.get("provider")?.as_str()?.to_string(),
        model: metadata
            .get("model")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        run_id: metadata
            .get("run_id")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        agent_id: metadata
            .get("agent_id")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    })
}

fn normalize_domain_filters(values: Option<Vec<String>>) -> Vec<String> {
    values
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| normalize_domain_filter(&value))
        .collect()
}

fn normalize_domain_filter(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_matches('.');
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(url) = Url::parse(trimmed) {
        return url.host_str().map(|host| host.to_ascii_lowercase());
    }
    let without_scheme = trimmed
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let domain = without_scheme
        .split('/')
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches('.');
    if domain.is_empty() {
        None
    } else {
        Some(domain.to_ascii_lowercase())
    }
}

pub(crate) fn parse_duckduckgo_results(html: &str) -> Vec<WebSearchHit> {
    let document = Html::parse_document(html);
    let result_selector = Selector::parse("div.result").expect("valid result selector");
    let link_selector = Selector::parse("a.result__a").expect("valid link selector");
    let snippet_selector = Selector::parse(".result__snippet").expect("valid snippet selector");
    let mut seen_urls = BTreeSet::new();
    let mut hits = Vec::new();

    for result in document.select(&result_selector) {
        let Some(anchor) = result.select(&link_selector).next() else {
            continue;
        };
        let Some(raw_href) = anchor.value().attr("href") else {
            continue;
        };
        let Some(url) = extract_search_result_url(raw_href) else {
            continue;
        };
        if !seen_urls.insert(url.clone()) {
            continue;
        }
        let title = normalize_text(&anchor.text().collect::<Vec<_>>().join(" "));
        if title.is_empty() {
            continue;
        }
        let snippet = result
            .select(&snippet_selector)
            .next()
            .map(|value| normalize_text(&value.text().collect::<Vec<_>>().join(" ")))
            .unwrap_or_default();
        let domain = Url::parse(&url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(ToString::to_string))
            .unwrap_or_default();
        hits.push(WebSearchHit {
            title,
            url,
            snippet,
            domain,
        });
    }

    hits
}

fn extract_search_result_url(raw_href: &str) -> Option<String> {
    let href = if raw_href.starts_with("//") {
        format!("https:{raw_href}")
    } else if raw_href.starts_with('/') {
        format!("https://html.duckduckgo.com{raw_href}")
    } else {
        raw_href.to_string()
    };
    let parsed = Url::parse(&href).ok()?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return None;
    }
    if matches!(
        parsed.host_str(),
        Some("duckduckgo.com") | Some("html.duckduckgo.com")
    ) && parsed.path().starts_with("/l/")
    {
        if let Some((_, target)) = parsed.query_pairs().find(|(key, _)| key == "uddg") {
            let target = target.to_string();
            let parsed_target = Url::parse(&target).ok()?;
            if matches!(parsed_target.scheme(), "http" | "https")
                && is_public_search_result_url(&target)
            {
                return Some(target);
            }
        }
    }
    let url = parsed.to_string();
    is_public_search_result_url(&url).then_some(url)
}

pub(crate) fn filter_search_hits_by_domain(
    hits: Vec<WebSearchHit>,
    allowed_domains: &[String],
    blocked_domains: &[String],
) -> Vec<WebSearchHit> {
    hits.into_iter()
        .filter(|hit| {
            let domain = hit.domain.to_ascii_lowercase();
            if !allowed_domains.is_empty()
                && !allowed_domains
                    .iter()
                    .any(|allowed| domain_matches_filter(&domain, allowed))
            {
                return false;
            }
            if blocked_domains
                .iter()
                .any(|blocked| domain_matches_filter(&domain, blocked))
            {
                return false;
            }
            true
        })
        .collect()
}

fn domain_matches_filter(domain: &str, filter: &str) -> bool {
    domain == filter || domain.ends_with(&format!(".{filter}"))
}

fn normalize_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

async fn fetch_with_hardened_redirects(
    shared: &SharedConfig,
    dns_resolver: &dyn WebFetchDnsResolver,
    mut url: Url,
    max_bytes: usize,
    allow_private_network: bool,
) -> Result<(reqwest::Response, Url, usize)> {
    let mut redirects = 0usize;
    loop {
        let target = validate_fetch_url(&url, allow_private_network, dns_resolver).await?;
        let client = web_fetch_client_for_target(shared, &target)?;
        let response = client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("failed to fetch {url}"))?;
        if !response.status().is_redirection() {
            ensure_content_length_allowed(&response, max_bytes)?;
            return Ok((response, url, redirects));
        }
        if redirects >= MAX_WEB_FETCH_REDIRECTS {
            bail!("web_fetch exceeded {MAX_WEB_FETCH_REDIRECTS} redirects");
        }
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| anyhow!("web_fetch redirect from {url} was missing Location"))?;
        url = url
            .join(location)
            .with_context(|| format!("web_fetch redirect from {url} had invalid Location"))?;
        redirects += 1;
    }
}

async fn validate_fetch_url(
    url: &Url,
    allow_private_network: bool,
    dns_resolver: &dyn WebFetchDnsResolver,
) -> Result<ValidatedFetchTarget> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!("web_fetch only supports http and https URLs");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("web_fetch refuses URLs containing credentials");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("web_fetch URL is missing a host"))?;
    if !allow_private_network && is_localhost_name(host) {
        bail!("web_fetch refuses localhost/private network host {host}");
    }
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        if !allow_private_network && is_private_or_special_ip(ip) {
            bail!("web_fetch refuses localhost/private network host {host}");
        }
        return Ok(ValidatedFetchTarget::default());
    }
    let port = url.port_or_known_default().ok_or_else(|| {
        anyhow!(
            "web_fetch URL is missing a port for scheme {}",
            url.scheme()
        )
    })?;
    let resolved = dns_resolver.resolve(host, port).await?;
    if !allow_private_network {
        validate_resolved_fetch_addresses(host, &resolved)?;
    } else if resolved.is_empty() {
        bail!("web_fetch host {host} resolved to no addresses");
    }
    Ok(ValidatedFetchTarget {
        pinned_resolution: Some(PinnedFetchResolution {
            host: host.to_string(),
            addresses: resolved,
        }),
    })
}

pub(crate) fn web_fetch_client_for_target(
    shared: &SharedConfig,
    target: &ValidatedFetchTarget,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(shared.user_agent.clone())
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if let Some(pinned) = &target.pinned_resolution {
        builder = builder.resolve_to_addrs(&pinned.host, &pinned.addresses);
    }
    builder
        .build()
        .context("failed to build hardened web_fetch HTTP client")
}

pub(crate) fn validate_resolved_fetch_addresses(host: &str, resolved: &[SocketAddr]) -> Result<()> {
    if resolved.is_empty() {
        bail!("web_fetch host {host} resolved to no addresses");
    }
    for address in resolved {
        if is_private_or_special_ip(address.ip()) {
            bail!("web_fetch refuses localhost/private network host {host}");
        }
    }
    Ok(())
}

fn is_public_search_result_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if is_localhost_name(host) {
        return false;
    }
    host.parse::<IpAddr>()
        .map(|ip| !is_private_or_special_ip(ip))
        .unwrap_or(true)
}

fn is_localhost_name(host: &str) -> bool {
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

fn ensure_content_length_allowed(response: &reqwest::Response, max_bytes: usize) -> Result<()> {
    let Some(value) = response.headers().get(CONTENT_LENGTH) else {
        return Ok(());
    };
    let length = value
        .to_str()
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    if length.is_some_and(|length| length > max_bytes as u64) {
        bail!("web_fetch response is larger than {max_bytes} bytes");
    }
    Ok(())
}

fn ensure_text_response(response: &reqwest::Response) -> Result<()> {
    let Some(value) = response.headers().get(CONTENT_TYPE) else {
        return Ok(());
    };
    let content_type = value.to_str().unwrap_or("").to_ascii_lowercase();
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    let allowed = media_type.starts_with("text/")
        || matches!(
            media_type,
            "application/json"
                | "application/xml"
                | "application/xhtml+xml"
                | "application/rss+xml"
                | "application/atom+xml"
                | "application/ld+json"
        )
        || media_type.ends_with("+json")
        || media_type.ends_with("+xml");
    if !allowed {
        bail!("web_fetch refuses non-text content type {media_type}");
    }
    Ok(())
}

async fn read_response_body_limited(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<(Vec<u8>, bool)> {
    let mut body = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await? {
        let remaining = max_bytes.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
        if body.len() == max_bytes {
            truncated = true;
            break;
        }
    }
    Ok((body, truncated))
}

fn truncate_header_value(value: &str) -> String {
    truncate_text(value, MAX_WEB_FETCH_HEADER_VALUE_BYTES).0
}

fn escape_markdown_link_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('(', "\\(")
        .replace(')', "\\)")
        .replace('\n', " ")
        .replace('\r', " ")
}

fn escape_markdown_link_url(value: &str) -> String {
    value.replace(')', "%29").replace('(', "%28")
}
