//! HTTP and SSE transport helpers for the CLI binary.

use std::time::Duration;
use std::{error::Error as StdError, fmt};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::cli::output::Printer;
use kheish_daemon::{
    AgentSummaryCountsView, AgentSummaryListPage, AgentSummaryView, DaemonEvent, ListPage,
    ListPageMeta, ProblemDetails,
};

const DAEMON_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DAEMON_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const DAEMON_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const DAEMON_SSE_FRAME_PROBE_TIMEOUT: Duration = Duration::from_secs(12);
const DAEMON_SSE_MAX_FRAME_BYTES: usize = 1_048_576;

/// Result of a cheap CLI-side daemon probe used by `doctor`.
#[derive(Clone, Debug)]
pub(crate) struct DaemonProbeResult {
    pub(crate) ok: bool,
    pub(crate) status: Option<reqwest::StatusCode>,
    pub(crate) content_type: Option<String>,
    pub(crate) message: String,
    pub(crate) action: Option<String>,
}

/// Typed daemon HTTP error used by the CLI to preserve stable exit codes.
#[derive(Debug)]
pub(crate) struct DaemonHttpError {
    status: reqwest::StatusCode,
    code: Option<String>,
    detail: String,
}

impl DaemonHttpError {
    pub(crate) fn from_problem(status: reqwest::StatusCode, problem: ProblemDetails) -> Self {
        Self {
            status,
            code: Some(problem.code),
            detail: problem.detail,
        }
    }

    pub(crate) fn from_body(status: reqwest::StatusCode, body: String) -> Self {
        Self {
            status,
            code: None,
            detail: body,
        }
    }

    pub(crate) fn stable_exit_code(&self) -> u8 {
        match self.status {
            reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::PAYLOAD_TOO_LARGE => 2,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => 4,
            reqwest::StatusCode::NOT_FOUND => 5,
            reqwest::StatusCode::CONFLICT => 6,
            reqwest::StatusCode::REQUEST_TIMEOUT
            | reqwest::StatusCode::TOO_MANY_REQUESTS
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT => 7,
            _ => 1,
        }
    }
}

impl fmt::Display for DaemonHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code.as_deref() {
            Some(code) => write!(
                formatter,
                "daemon returned {} ({}): {}",
                self.status, code, self.detail
            ),
            None => write!(
                formatter,
                "daemon returned {}: {}",
                self.status, self.detail
            ),
        }
    }
}

impl StdError for DaemonHttpError {}

/// Thin HTTP client for the daemon control plane.
pub(crate) struct DaemonHttpClient {
    base_url: String,
    client: Client,
    token: Option<String>,
}

impl DaemonHttpClient {
    /// Creates one HTTP client bound to the provided base URL and optional bearer token.
    pub(crate) fn new(base_url: String, token: Option<String>) -> Self {
        let client = Client::builder()
            .connect_timeout(DAEMON_CONNECT_TIMEOUT)
            .build()
            .expect("daemon CLI HTTP client configuration should be valid");
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client,
            token,
        }
    }

    /// Returns the normalized daemon base URL.
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Performs one GET request and decodes the JSON response body.
    pub(crate) async fn get_json<T>(&self, path: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let response = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .context("daemon request failed")?;
        error_for_status_with_body(response)
            .await?
            .json::<T>()
            .await
            .context("failed to decode daemon response")
    }

    /// Performs one GET request and accepts either a paginated envelope or a legacy array.
    pub(crate) async fn get_list_page_compat<T>(&self, path: &str) -> Result<ListPage<T>>
    where
        T: DeserializeOwned,
    {
        let response = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .context("daemon request failed")?;
        let decoded = error_for_status_with_body(response)
            .await?
            .json::<ListOrPage<T>>()
            .await
            .context("failed to decode daemon response")?;
        Ok(decoded.into_page())
    }

    /// Performs one GET request for agent summaries and accepts either the new
    /// counted page envelope or the legacy array returned by older daemons.
    pub(crate) async fn get_agent_summary_list_page_compat(
        &self,
        path: &str,
    ) -> Result<AgentSummaryListPage> {
        let response = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .context("daemon request failed")?;
        let decoded = error_for_status_with_body(response)
            .await?
            .json::<AgentSummaryListOrPage>()
            .await
            .context("failed to decode daemon response")?;
        Ok(decoded.into_page())
    }

    /// Performs one GET request with query parameters and decodes the JSON response body.
    pub(crate) async fn get_json_with_query<Q, T>(&self, path: &str, query: &Q) -> Result<T>
    where
        Q: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let response = self
            .request(reqwest::Method::GET, path)
            .query(query)
            .send()
            .await
            .context("daemon request failed")?;
        error_for_status_with_body(response)
            .await?
            .json::<T>()
            .await
            .context("failed to decode daemon response")
    }

    /// Performs one GET request and returns the response body as text.
    pub(crate) async fn get_text(&self, path: &str) -> Result<String> {
        let response = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .context("daemon request failed")?;
        error_for_status_with_body(response)
            .await?
            .text()
            .await
            .context("failed to decode daemon response body")
    }

    /// Performs one POST request with a JSON body and decodes the JSON response body.
    pub(crate) async fn post_json<B, T>(&self, path: &str, body: &B) -> Result<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let response = self
            .request(reqwest::Method::POST, path)
            .json(body)
            .send()
            .await
            .context("daemon request failed")?;
        error_for_status_with_body(response)
            .await?
            .json::<T>()
            .await
            .context("failed to decode daemon response")
    }

    /// Performs one JSON POST with an Idempotency-Key header and decodes the JSON response body.
    pub(crate) async fn post_json_with_idempotency_key<B, T>(
        &self,
        path: &str,
        idempotency_key: &str,
        body: &B,
    ) -> Result<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let response = self
            .request(reqwest::Method::POST, path)
            .header("Idempotency-Key", idempotency_key)
            .json(body)
            .send()
            .await
            .context("daemon request failed")?;
        error_for_status_with_body(response)
            .await?
            .json::<T>()
            .await
            .context("failed to decode daemon response")
    }

    /// Performs one POST request with an explicit bearer token and decodes the JSON response body.
    pub(crate) async fn post_json_with_bearer<B, T>(
        &self,
        path: &str,
        token: &str,
        body: &B,
    ) -> Result<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let response = self
            .client
            .request(reqwest::Method::POST, self.url(path))
            .bearer_auth(token)
            .timeout(DAEMON_REQUEST_TIMEOUT)
            .json(body)
            .send()
            .await
            .context("daemon request failed")?;
        error_for_status_with_body(response)
            .await?
            .json::<T>()
            .await
            .context("failed to decode daemon response")
    }

    /// Performs one POST request without a body and decodes the JSON response body.
    pub(crate) async fn post_empty_json<T>(&self, path: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let response = self
            .request(reqwest::Method::POST, path)
            .send()
            .await
            .context("daemon request failed")?;
        error_for_status_with_body(response)
            .await?
            .json::<T>()
            .await
            .context("failed to decode daemon response")
    }

    /// Performs one DELETE request and decodes the JSON response body.
    pub(crate) async fn delete_json<T>(&self, path: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let response = self
            .request(reqwest::Method::DELETE, path)
            .send()
            .await
            .context("daemon request failed")?;
        error_for_status_with_body(response)
            .await?
            .json::<T>()
            .await
            .context("failed to decode daemon response")
    }

    /// Performs one DELETE request with a JSON body and decodes the JSON response body.
    pub(crate) async fn delete_json_with_body<B, T>(&self, path: &str, body: &B) -> Result<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let response = self
            .request(reqwest::Method::DELETE, path)
            .json(body)
            .send()
            .await
            .context("daemon request failed")?;
        error_for_status_with_body(response)
            .await?
            .json::<T>()
            .await
            .context("failed to decode daemon response")
    }

    /// Performs one PUT request with a JSON body and decodes the JSON response body.
    pub(crate) async fn put_json<B, T>(&self, path: &str, body: &B) -> Result<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let response = self
            .request(reqwest::Method::PUT, path)
            .json(body)
            .send()
            .await
            .context("daemon request failed")?;
        error_for_status_with_body(response)
            .await?
            .json::<T>()
            .await
            .context("failed to decode daemon response")
    }

    /// Performs one request and returns the final HTTP status.
    pub(crate) async fn request_status(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::StatusCode> {
        let response = self
            .request(method, path)
            .send()
            .await
            .context("daemon request failed")?;
        Ok(response.status())
    }

    /// Probes one path and returns an actionable diagnostic result.
    pub(crate) async fn probe(&self, path: &str) -> DaemonProbeResult {
        let response = match self
            .stream_request(reqwest::Method::GET, path)
            .timeout(DAEMON_PROBE_TIMEOUT)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => return request_probe_error("daemon probe request failed", error),
        };
        let status = response.status();
        if status.is_success() {
            DaemonProbeResult {
                ok: true,
                status: Some(status),
                content_type: response_content_type(&response),
                message: "ready endpoint is reachable".to_string(),
                action: None,
            }
        } else {
            DaemonProbeResult {
                ok: false,
                status: Some(status),
                content_type: response_content_type(&response),
                message: format!("ready endpoint returned HTTP {status}"),
                action: Some(
                    "verify daemon readiness, auth token, and reverse-proxy routing".to_string(),
                ),
            }
        }
    }

    /// Probes one SSE endpoint and verifies that it delivers at least one parseable frame.
    pub(crate) async fn probe_sse(&self, path: &str) -> DaemonProbeResult {
        let response = match tokio::time::timeout(
            DAEMON_PROBE_TIMEOUT,
            self.stream_request(reqwest::Method::GET, path).send(),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => return request_probe_error("daemon SSE probe request failed", error),
            Err(_) => {
                return DaemonProbeResult {
                    ok: false,
                    status: None,
                    content_type: None,
                    message: "daemon SSE probe did not return response headers before timeout"
                        .to_string(),
                    action: Some("verify daemon responsiveness and proxy timeouts".to_string()),
                };
            }
        };
        let status = response.status();
        let content_type = response_content_type(&response);
        if !response.status().is_success() {
            return DaemonProbeResult {
                ok: false,
                status: Some(status),
                content_type,
                message: format!("event stream returned HTTP {status}"),
                action: Some(
                    "verify daemon auth, `/v1/events/stream`, and proxy routing".to_string(),
                ),
            };
        }
        let is_event_stream = content_type
            .as_deref()
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"));
        if !is_event_stream {
            return DaemonProbeResult {
                ok: false,
                status: Some(status),
                content_type,
                message: "event stream did not return `text/event-stream`".to_string(),
                action: Some(
                    "disable proxy response buffering and preserve SSE content type".to_string(),
                ),
            };
        }

        let mut stream = response.bytes_stream();
        let frame_result = tokio::time::timeout(DAEMON_SSE_FRAME_PROBE_TIMEOUT, async {
            let mut buffer = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.context("failed to read daemon SSE chunk")?;
                buffer.extend_from_slice(&chunk);
                while let Some(frame) = pop_sse_frame_bytes(&mut buffer)? {
                    if parse_sse_event(&frame)?.is_some() {
                        return Ok(());
                    }
                }
                ensure_sse_buffer_within_limit(&buffer)?;
            }
            bail!("event stream ended before a complete SSE event")
        })
        .await;
        match frame_result {
            Ok(Ok(())) => DaemonProbeResult {
                ok: true,
                status: Some(status),
                content_type,
                message: "event stream is reachable and delivered a parseable SSE frame"
                    .to_string(),
                action: None,
            },
            Ok(Err(error)) => DaemonProbeResult {
                ok: false,
                status: Some(status),
                content_type,
                message: format!("event stream probe failed: {error:#}"),
                action: Some(
                    "inspect `/v1/events/stream` and proxy SSE buffering/content rewriting"
                        .to_string(),
                ),
            },
            Err(_) => DaemonProbeResult {
                ok: false,
                status: Some(status),
                content_type,
                message: "event stream did not deliver an SSE frame before the probe timeout"
                    .to_string(),
                action: Some(
                    "verify typed heartbeats and disable proxy buffering for SSE".to_string(),
                ),
            },
        }
    }

    /// Probes a browser CORS preflight for one expected origin.
    pub(crate) async fn probe_cors_preflight(&self, path: &str, origin: &str) -> DaemonProbeResult {
        const REQUIRED_METHODS: &[&str] = &["GET", "POST"];
        const REQUIRED_HEADERS: &[&str] = &["authorization", "content-type"];

        let response = match self
            .client
            .request(reqwest::Method::OPTIONS, self.url(path))
            .header(reqwest::header::ORIGIN, origin)
            .header(reqwest::header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(
                reqwest::header::ACCESS_CONTROL_REQUEST_HEADERS,
                REQUIRED_HEADERS.join(","),
            )
            .timeout(DAEMON_PROBE_TIMEOUT)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => return request_probe_error("daemon CORS preflight failed", error),
        };
        let status = response.status();
        let content_type = response_content_type(&response);
        let allowed_origin = response
            .headers()
            .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let allowed_methods = response
            .headers()
            .get(reqwest::header::ACCESS_CONTROL_ALLOW_METHODS)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let allowed_headers = response
            .headers()
            .get(reqwest::header::ACCESS_CONTROL_ALLOW_HEADERS)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let methods_ok = REQUIRED_METHODS
            .iter()
            .all(|method| header_list_contains(&allowed_methods, method));
        let headers_ok = REQUIRED_HEADERS
            .iter()
            .all(|header| header_list_contains(&allowed_headers, header));
        if status.is_success()
            && allowed_origin.as_deref() == Some(origin)
            && methods_ok
            && headers_ok
        {
            DaemonProbeResult {
                ok: true,
                status: Some(status),
                content_type,
                message: format!(
                    "CORS preflight accepts origin `{origin}` with required methods and headers"
                ),
                action: None,
            }
        } else {
            DaemonProbeResult {
                ok: false,
                status: Some(status),
                content_type,
                message: format!(
                    "CORS preflight for `{origin}` returned HTTP {status} with allow-origin {:?}, allow-methods {:?}, allow-headers {:?}",
                    allowed_origin, allowed_methods, allowed_headers
                ),
                action: Some(
                    "add the exact origin with `--http-cors-allow-origin` and preserve CORS method/header responses through any proxy"
                        .to_string(),
                ),
            }
        }
    }

    /// Streams one SSE endpoint and prints decoded events incrementally.
    pub(crate) async fn stream_events(&self, path: &str, printer: &Printer) -> Result<()> {
        let response = self
            .stream_request(reqwest::Method::GET, path)
            .send()
            .await
            .context("daemon stream request failed")?;
        let response = error_for_status_with_body(response).await?;
        let mut buffer = Vec::new();
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("failed to read daemon SSE chunk")?;
            buffer.extend_from_slice(&chunk);
            while let Some(frame) = pop_sse_frame_bytes(&mut buffer)? {
                if let Some(event) = parse_sse_event(&frame)? {
                    printer.print(&event)?;
                }
            }
            ensure_sse_buffer_within_limit(&buffer)?;
        }

        if !buffer.iter().all(u8::is_ascii_whitespace) {
            let frame = String::from_utf8(buffer).context("daemon SSE was not valid UTF-8")?;
            if let Some(event) = parse_sse_event(&frame)? {
                printer.print(&event)?;
            }
        }
        Ok(())
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.authenticated_request(method, path)
            .timeout(DAEMON_REQUEST_TIMEOUT)
    }

    fn stream_request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.authenticated_request(method, path)
    }

    fn authenticated_request(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> reqwest::RequestBuilder {
        let builder = self.client.request(method, self.url(path));
        if let Some(token) = self.token.as_deref() {
            builder.bearer_auth(token)
        } else {
            builder
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AgentSummaryListOrPage {
    Page(AgentSummaryListPage),
    Legacy(Vec<AgentSummaryView>),
}

impl AgentSummaryListOrPage {
    fn into_page(self) -> AgentSummaryListPage {
        match self {
            Self::Page(page) => page,
            Self::Legacy(items) => {
                let total_count = items.len();
                let counts = AgentSummaryCountsView::from_summaries(total_count, &items);
                AgentSummaryListPage {
                    items,
                    pagination: ListPageMeta {
                        limit: total_count,
                        total_count,
                        has_more: false,
                        next_cursor: None,
                        order: "legacy_array".to_string(),
                    },
                    counts,
                }
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ListOrPage<T> {
    Page(ListPage<T>),
    Legacy(Vec<T>),
}

impl<T> ListOrPage<T> {
    fn into_page(self) -> ListPage<T> {
        match self {
            Self::Page(page) => page,
            Self::Legacy(items) => {
                let total_count = items.len();
                ListPage {
                    items,
                    pagination: ListPageMeta {
                        limit: total_count,
                        total_count,
                        has_more: false,
                        next_cursor: None,
                        order: "legacy_array".to_string(),
                    },
                }
            }
        }
    }
}

fn response_content_type(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn header_list_contains(header_value: &str, expected: &str) -> bool {
    header_value
        .split(',')
        .map(str::trim)
        .any(|value| value.eq_ignore_ascii_case(expected))
}

fn request_probe_error(context: &str, error: reqwest::Error) -> DaemonProbeResult {
    let action = if error.is_timeout() {
        "check daemon responsiveness and proxy timeouts"
    } else if error.is_connect() || error.is_request() {
        "verify --base-url, daemon process, and network reachability"
    } else {
        "inspect daemon connectivity and CLI transport configuration"
    };
    DaemonProbeResult {
        ok: false,
        status: error.status(),
        content_type: None,
        message: format!("{context}: {error}"),
        action: Some(action.to_string()),
    }
}

/// Pops the next complete SSE frame from the provided buffer.
#[cfg(test)]
pub(crate) fn pop_sse_frame(buffer: &mut String) -> Option<String> {
    let mut candidate = None;
    for pattern in ["\r\n\r\n", "\n\n"] {
        if let Some(index) = buffer.find(pattern) {
            match candidate {
                Some((best_index, _)) if best_index < index => {}
                _ => candidate = Some((index, pattern.len())),
            }
        }
    }
    let (index, delimiter_len) = candidate?;
    let frame = buffer[..index].to_string();
    buffer.replace_range(..index + delimiter_len, "");
    Some(frame)
}

fn pop_sse_frame_bytes(buffer: &mut Vec<u8>) -> Result<Option<String>> {
    let mut candidate = None;
    for pattern in [b"\r\n\r\n".as_slice(), b"\n\n".as_slice()] {
        if let Some(index) = buffer
            .windows(pattern.len())
            .position(|window| window == pattern)
        {
            match candidate {
                Some((best_index, _)) if best_index < index => {}
                _ => candidate = Some((index, pattern.len())),
            }
        }
    }
    let Some((index, delimiter_len)) = candidate else {
        return Ok(None);
    };
    if index > DAEMON_SSE_MAX_FRAME_BYTES {
        bail!(
            "daemon SSE frame exceeded {} bytes before a frame delimiter",
            DAEMON_SSE_MAX_FRAME_BYTES
        );
    }
    let frame = buffer[..index].to_vec();
    buffer.drain(..index + delimiter_len);
    String::from_utf8(frame)
        .map(Some)
        .context("daemon SSE frame was not valid UTF-8")
}

fn ensure_sse_buffer_within_limit(buffer: &[u8]) -> Result<()> {
    if buffer.len() > DAEMON_SSE_MAX_FRAME_BYTES {
        bail!(
            "daemon SSE frame exceeded {} bytes without a frame delimiter",
            DAEMON_SSE_MAX_FRAME_BYTES
        );
    }
    Ok(())
}

/// Parses one raw SSE frame into a typed daemon event view when possible.
pub(crate) fn parse_sse_event(frame: &str) -> Result<Option<crate::StreamEventView>> {
    let mut event_name = None;
    let mut event_id = None;
    let mut data_lines = Vec::new();
    for line in frame.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("id:") {
            let value = value.trim();
            if !value.is_empty() {
                if !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    bail!("failed to decode SSE event id {value:?}");
                }
                event_id = Some(value.to_string());
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event_name = Some(value.trim().to_string());
            continue;
        }
        if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.trim_start().to_string());
        }
    }

    let Some(event_name) = event_name else {
        return Ok(None);
    };
    let data = data_lines.join("\n");
    let event = serde_json::from_str::<DaemonEvent>(&data)
        .with_context(|| format!("failed to decode SSE event {event_name}"))?;
    Ok(Some(crate::StreamEventView {
        id: event_id,
        event: event_name,
        data: event,
    }))
}

/// Converts non-success daemon responses into rich CLI errors that include the body.
pub(crate) async fn error_for_status_with_body(
    response: reqwest::Response,
) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    if let Ok(problem) = serde_json::from_str::<ProblemDetails>(&body) {
        return Err(DaemonHttpError::from_problem(status, problem).into());
    }
    Err(DaemonHttpError::from_body(status, body).into())
}

#[cfg(test)]
mod tests {
    use super::{parse_sse_event, pop_sse_frame};
    use axum::Router;
    use axum::routing::get;
    use kheish_daemon::DaemonEvent;
    use tokio::net::TcpListener;

    #[test]
    fn pop_sse_frame_prefers_the_earliest_complete_frame() {
        let mut buffer = "event: a\ndata: {}\n\nevent: b\ndata: {}\n\n".to_string();
        let first = pop_sse_frame(&mut buffer).expect("first frame");
        assert!(first.contains("event: a"));
        assert!(buffer.contains("event: b"));
    }

    #[test]
    fn pop_sse_frame_bytes_waits_for_complete_utf8_frame() {
        let mut buffer = "event: message\ndata: {\"text\":\"caf".as_bytes().to_vec();
        buffer.extend_from_slice(&[0xc3]);
        assert!(super::pop_sse_frame_bytes(&mut buffer).unwrap().is_none());
        buffer.extend_from_slice(&[0xa9]);
        buffer.extend_from_slice("\"}\n\n".as_bytes());
        let frame = super::pop_sse_frame_bytes(&mut buffer)
            .unwrap()
            .expect("frame");
        assert!(frame.contains("café"));
        assert!(buffer.is_empty());
    }

    #[test]
    fn sse_buffer_limit_rejects_unbounded_frames() {
        let buffer = vec![b'a'; super::DAEMON_SSE_MAX_FRAME_BYTES + 1];
        let error = super::ensure_sse_buffer_within_limit(&buffer)
            .expect_err("oversized frame should be rejected");
        assert!(error.to_string().contains("SSE frame exceeded"));
    }

    #[test]
    fn pop_sse_frame_bytes_rejects_oversized_complete_frame() {
        let mut buffer = vec![b'a'; super::DAEMON_SSE_MAX_FRAME_BYTES + 1];
        buffer.extend_from_slice(b"\n\n");
        let error = super::pop_sse_frame_bytes(&mut buffer)
            .expect_err("oversized complete frame should be rejected");
        assert!(error.to_string().contains("SSE frame exceeded"));
    }

    #[tokio::test]
    async fn parse_sse_event_extracts_named_event() {
        let parsed = parse_sse_event(
            "event: session_state_changed\ndata: {\"type\":\"session_state_changed\",\"session_id\":\"s1\",\"agent_id\":\"a1\",\"status\":\"running\",\"pending_approvals\":0}\n\n",
        )
        .expect("sse parse")
        .expect("sse event");
        assert_eq!(parsed.event, "session_state_changed");
        assert_eq!(parsed.id, None);
        match parsed.data {
            DaemonEvent::SessionStateChanged {
                session_id,
                agent_id,
                pending_approvals,
                ..
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(agent_id, "a1");
                assert_eq!(pending_approvals, 0);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn parse_sse_event_extracts_id_and_heartbeat() {
        let parsed = parse_sse_event(
            "id: 18446744073709551615\nevent: heartbeat\ndata: {\"type\":\"heartbeat\"}\n\n",
        )
        .expect("sse parse")
        .expect("sse event");
        assert_eq!(parsed.id.as_deref(), Some("18446744073709551615"));
        assert_eq!(parsed.event, "heartbeat");
        assert!(matches!(parsed.data, DaemonEvent::Heartbeat));
    }

    #[tokio::test]
    async fn parse_sse_event_rejects_non_numeric_id() {
        let error = parse_sse_event(
            "id: not-a-number\nevent: heartbeat\ndata: {\"type\":\"heartbeat\"}\n\n",
        )
        .expect_err("non-numeric SSE id should fail");
        assert!(error.to_string().contains("failed to decode SSE event id"));
    }

    #[tokio::test]
    async fn probe_sse_accepts_case_insensitive_event_stream_content_type() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake SSE server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let router = Router::new().route(
            "/v1/events/stream",
            get(|| async {
                (
                    [(
                        reqwest::header::CONTENT_TYPE.as_str(),
                        "Text/Event-Stream; Charset=UTF-8",
                    )],
                    "event: heartbeat\ndata: {\"type\":\"heartbeat\"}\n\n",
                )
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("fake SSE server");
        });

        let client = super::DaemonHttpClient::new(base_url, None);
        let probe = client.probe_sse("/v1/events/stream").await;
        assert!(probe.ok, "unexpected probe failure: {}", probe.message);
    }

    #[tokio::test]
    async fn probe_sse_rejects_malformed_event_stream_frame() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake SSE server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let router = Router::new().route(
            "/v1/events/stream",
            get(|| async {
                (
                    [(reqwest::header::CONTENT_TYPE.as_str(), "text/event-stream")],
                    "event: heartbeat\ndata: not-json\n\n",
                )
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("fake SSE server");
        });

        let client = super::DaemonHttpClient::new(base_url, None);
        let probe = client.probe_sse("/v1/events/stream").await;
        assert!(!probe.ok);
        assert!(
            probe.message.contains("failed to decode SSE event"),
            "unexpected probe message: {}",
            probe.message
        );
    }
}
