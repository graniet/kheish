use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures_util::{SinkExt, StreamExt, stream::BoxStream};
use kheish_auth::{AuthManager, AuthSlotId, ExecutionCredentialContext};
use kheish_codec::digest_serialize;
use kheish_runtime::{redact_json_value, redact_text};
use parking_lot::Mutex as SyncMutex;
use rmcp::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientJsonRpcMessage,
    Implementation, InitializeRequestParams, JsonRpcMessage, ListResourceTemplatesResult,
    ListResourcesResult, ListRootsResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ReadResourceRequestParams, ReadResourceResult, Root, RootsCapabilities, ServerJsonRpcMessage,
};
use rmcp::service::{RoleClient, RunningService, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpClientTransportConfig, StreamableHttpError,
    StreamableHttpPostResponse,
};
use rmcp::transport::{Transport, async_rw::JsonRpcMessageCodec};
use serde::Deserialize;
use sse_stream::{Sse, SseStream};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time;
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::{debug, warn};

use crate::config::{McpHttpAuth, McpServerConfig, McpServerTransport, resolve_cwd};

type ServiceHandle = Arc<RunningService<RoleClient, KheishMcpClientHandler>>;
const MAX_MCP_STDERR_LOG_CHARS: usize = 512;
const MAX_MCP_STDIO_JSONRPC_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_MCP_HTTP_JSONRPC_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_MCP_HTTP_ERROR_BODY_BYTES: usize = 64 * 1024;
const HEADER_SESSION_ID: &str = "Mcp-Session-Id";
const HEADER_LAST_EVENT_ID: &str = "Last-Event-Id";
const HEADER_MCP_PROTOCOL_VERSION: &str = "MCP-Protocol-Version";
const EVENT_STREAM_MIME_TYPE: &str = "text/event-stream";
const JSON_MIME_TYPE: &str = "application/json";

/// One initialized MCP server connection.
pub(crate) struct McpClient {
    config: McpServerConfig,
    auth_manager: Option<Arc<AuthManager>>,
    service: Mutex<Option<ServiceHandle>>,
    workspace_root: Mutex<Option<PathBuf>>,
    streamable_http_auth_fingerprint: Mutex<Option<String>>,
    static_secret_ref_digests: BTreeMap<String, String>,
    shutdown_started: Arc<AtomicBool>,
    #[cfg(unix)]
    child_pid: Mutex<Option<u32>>,
    startup_timeout: Duration,
    tool_timeout: Duration,
}

struct ResolvedHttpHeaders {
    headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
    fingerprint: String,
}

#[derive(Clone)]
struct BoundedReqwestMcpHttpClient {
    inner: reqwest::Client,
}

impl BoundedReqwestMcpHttpClient {
    fn new() -> Self {
        Self {
            inner: reqwest::Client::builder()
                .pool_max_idle_per_host(0)
                .build()
                .expect("failed to build bounded MCP HTTP client"),
        }
    }
}

#[derive(Debug)]
struct McpHttpBodyLimitError {
    context: &'static str,
    max_bytes: usize,
}

impl std::fmt::Display for McpHttpBodyLimitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "mcp_response_too_large: {} body limit exceeded (max_bytes={})",
            self.context, self.max_bytes
        )
    }
}

impl std::error::Error for McpHttpBodyLimitError {}

impl StreamableHttpClient for BoundedReqwestMcpHttpClient {
    type Error = reqwest::Error;

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let mut request = self
            .inner
            .get(uri.as_ref())
            .header(
                reqwest::header::ACCEPT,
                [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "),
            )
            .header(HEADER_SESSION_ID, session_id.as_ref());
        if let Some(last_event_id) = last_event_id {
            request = request.header(HEADER_LAST_EVENT_ID, last_event_id);
        }
        if let Some(auth_header) = auth_token {
            request = request.bearer_auth(auth_header);
        }
        request = apply_mcp_custom_headers(request, custom_headers)?;
        let response = request.send().await.map_err(StreamableHttpError::Client)?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        let status = response.status();
        if !status.is_success() {
            let body =
                bounded_response_text(response, MAX_MCP_HTTP_ERROR_BODY_BYTES, "error response")
                    .await?;
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                redacted_mcp_http_error(&format!("HTTP {status}"), &body),
            )));
        }
        ensure_mcp_content_type(&response, &[EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE])?;
        Ok(bounded_sse_stream(response))
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_token: Option<String>,
        custom_headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let mut request = self.inner.delete(uri.as_ref());
        if let Some(auth_header) = auth_token {
            request = request.bearer_auth(auth_header);
        }
        request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        request = apply_mcp_custom_headers(request, custom_headers)?;
        let response = request.send().await.map_err(StreamableHttpError::Client)?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Ok(());
        }
        let status = response.status();
        if !status.is_success() {
            let body =
                bounded_response_text(response, MAX_MCP_HTTP_ERROR_BODY_BYTES, "error response")
                    .await?;
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                redacted_mcp_http_error(&format!("HTTP {status}"), &body),
            )));
        }
        Ok(())
    }

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let mut request = self.inner.post(uri.as_ref()).header(
            reqwest::header::ACCEPT,
            [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "),
        );
        if let Some(auth_header) = auth_token {
            request = request.bearer_auth(auth_header);
        }
        request = apply_mcp_custom_headers(request, custom_headers)?;
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        let response = request
            .json(&message)
            .send()
            .await
            .map_err(StreamableHttpError::Client)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(header) = response.headers().get(reqwest::header::WWW_AUTHENTICATE) {
                let header = header
                    .to_str()
                    .map_err(|_| {
                        StreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(
                            "invalid www-authenticate header value",
                        ))
                    })?
                    .to_string();
                return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                    redacted_mcp_http_error("auth required", &header),
                )));
            }
        }
        if response.status() == reqwest::StatusCode::FORBIDDEN {
            if let Some(header) = response.headers().get(reqwest::header::WWW_AUTHENTICATE) {
                let header_str = header.to_str().map_err(|_| {
                    StreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(
                        "invalid www-authenticate header value",
                    ))
                })?;
                return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                    redacted_mcp_http_error("insufficient scope", header_str),
                )));
            }
        }
        let status = response.status();
        if matches!(
            status,
            reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::NO_CONTENT
        ) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == reqwest::StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .map(|ct| String::from_utf8_lossy(ct.as_bytes()).to_string());
        let response_session_id = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        if !status.is_success() {
            let body =
                bounded_response_text(response, MAX_MCP_HTTP_ERROR_BODY_BYTES, "error response")
                    .await?;
            if content_type_starts_with(content_type.as_deref(), JSON_MIME_TYPE) {
                if let Some(message) = parse_json_rpc_error(&body) {
                    return Ok(StreamableHttpPostResponse::Json(
                        message,
                        response_session_id,
                    ));
                }
                tracing::warn!("HTTP {status}: could not parse JSON body as a JSON-RPC error");
            }
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                redacted_mcp_http_error(&format!("HTTP {status}"), &body),
            )));
        }
        match content_type.as_deref() {
            Some(ct) if ct.as_bytes().starts_with(EVENT_STREAM_MIME_TYPE.as_bytes()) => Ok(
                StreamableHttpPostResponse::Sse(bounded_sse_stream(response), response_session_id),
            ),
            Some(ct) if ct.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()) => {
                match bounded_response_json(response, MAX_MCP_HTTP_JSONRPC_MESSAGE_BYTES).await {
                    Ok(message) => Ok(StreamableHttpPostResponse::Json(
                        message,
                        response_session_id,
                    )),
                    Err(StreamableHttpError::Deserialize(error)) => {
                        tracing::warn!(
                            "could not parse JSON response as ServerJsonRpcMessage, treating as accepted: {error}"
                        );
                        Ok(StreamableHttpPostResponse::Accepted)
                    }
                    Err(error) => Err(error),
                }
            }
            _ => Err(StreamableHttpError::UnexpectedContentType(content_type)),
        }
    }
}

fn apply_mcp_custom_headers(
    mut request: reqwest::RequestBuilder,
    custom_headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
) -> Result<reqwest::RequestBuilder, StreamableHttpError<reqwest::Error>> {
    for (name, value) in custom_headers {
        if is_reserved_mcp_header(&name) {
            return Err(StreamableHttpError::ReservedHeaderConflict(
                name.to_string(),
            ));
        }
        request = request.header(name, value);
    }
    Ok(request)
}

fn is_reserved_mcp_header(name: &reqwest::header::HeaderName) -> bool {
    let name = name.as_str();
    (name.eq_ignore_ascii_case(reqwest::header::ACCEPT.as_str())
        || name.eq_ignore_ascii_case(HEADER_SESSION_ID)
        || name.eq_ignore_ascii_case(HEADER_LAST_EVENT_ID))
        && !name.eq_ignore_ascii_case(HEADER_MCP_PROTOCOL_VERSION)
}

fn ensure_mcp_content_type(
    response: &reqwest::Response,
    allowed_prefixes: &[&str],
) -> Result<(), StreamableHttpError<reqwest::Error>> {
    let Some(content_type) = response.headers().get(reqwest::header::CONTENT_TYPE) else {
        return Err(StreamableHttpError::UnexpectedContentType(None));
    };
    if allowed_prefixes
        .iter()
        .any(|prefix| content_type.as_bytes().starts_with(prefix.as_bytes()))
    {
        return Ok(());
    }
    Err(StreamableHttpError::UnexpectedContentType(Some(
        String::from_utf8_lossy(content_type.as_bytes()).to_string(),
    )))
}

fn content_type_starts_with(content_type: Option<&str>, expected: &str) -> bool {
    content_type.is_some_and(|value| value.as_bytes().starts_with(expected.as_bytes()))
}

fn redacted_mcp_http_error(prefix: &str, detail: &str) -> String {
    let redacted = serde_json::from_str::<serde_json::Value>(detail)
        .map(|value| redact_json_value(&value).to_string())
        .unwrap_or_else(|_| redact_text(detail));
    format!("{prefix}: {redacted}")
}

fn capture_static_secret_ref_digests(
    config: &McpServerConfig,
    auth_manager: Option<&AuthManager>,
) -> Result<BTreeMap<String, String>> {
    let static_secret_refs = config
        .credential_secret_refs
        .iter()
        .filter(|secret_ref| !secret_ref.starts_with("mcp.oauth."))
        .collect::<Vec<_>>();
    if static_secret_refs.is_empty() {
        return Ok(BTreeMap::new());
    }
    let manager =
        auth_manager.ok_or_else(|| anyhow!("MCP credential auth manager is not configured"))?;
    let mut digests = BTreeMap::new();
    for secret_ref in static_secret_refs {
        let current = manager
            .secret_value(&AuthSlotId::new(secret_ref.as_str()))?
            .ok_or_else(|| anyhow!("MCP credential slot `{secret_ref}` is missing"))?;
        digests.insert(secret_ref.to_string(), digest_serialize(&current)?);
    }
    Ok(digests)
}

async fn bounded_response_json(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<ServerJsonRpcMessage, StreamableHttpError<reqwest::Error>> {
    let bytes = bounded_response_bytes(response, max_bytes, "JSON response").await?;
    Ok(serde_json::from_slice::<ServerJsonRpcMessage>(&bytes)?)
}

async fn bounded_response_text(
    response: reqwest::Response,
    max_bytes: usize,
    context: &'static str,
) -> Result<String, StreamableHttpError<reqwest::Error>> {
    let bytes = bounded_response_bytes(response, max_bytes, context).await?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

async fn bounded_response_bytes(
    response: reqwest::Response,
    max_bytes: usize,
    context: &'static str,
) -> Result<Vec<u8>, StreamableHttpError<reqwest::Error>> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(mcp_http_body_limit_error(context, max_bytes));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(StreamableHttpError::Client)?;
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(mcp_http_body_limit_error(context, max_bytes));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn mcp_http_body_limit_error(
    context: &'static str,
    max_bytes: usize,
) -> StreamableHttpError<reqwest::Error> {
    StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
        McpHttpBodyLimitError { context, max_bytes }.to_string(),
    ))
}

fn bounded_sse_stream(response: reqwest::Response) -> BoxStream<'static, Result<Sse, SseError>> {
    let state = Arc::new(SyncMutex::new(SseByteLimitState::default()));
    let byte_stream = response.bytes_stream().map(move |chunk| {
        let mut state = state.lock();
        match chunk {
            Ok(bytes) => {
                if let Err(error) = state.observe(&bytes) {
                    return Err(SseError::Body(Box::new(error)));
                }
                Ok(bytes)
            }
            Err(error) => Err(SseError::Body(Box::new(error))),
        }
    });
    SseStream::from_byte_stream(byte_stream)
        .map(|event| match event {
            Ok(sse) => validate_sse_event_size(sse),
            Err(error) => Err(error),
        })
        .boxed()
}

#[derive(Default)]
struct SseByteLimitState {
    current_event_bytes: usize,
    previous_was_lf: bool,
}

impl SseByteLimitState {
    fn observe(&mut self, bytes: &[u8]) -> Result<(), McpHttpBodyLimitError> {
        for byte in bytes {
            self.current_event_bytes = self.current_event_bytes.saturating_add(1);
            if self.current_event_bytes > MAX_MCP_HTTP_JSONRPC_MESSAGE_BYTES {
                return Err(McpHttpBodyLimitError {
                    context: "SSE event",
                    max_bytes: MAX_MCP_HTTP_JSONRPC_MESSAGE_BYTES,
                });
            }
            if *byte == b'\n' {
                if self.previous_was_lf {
                    self.current_event_bytes = 0;
                    self.previous_was_lf = false;
                } else {
                    self.previous_was_lf = true;
                }
            } else if *byte != b'\r' {
                self.previous_was_lf = false;
            }
        }
        Ok(())
    }
}

fn validate_sse_event_size(mut sse: Sse) -> Result<Sse, SseError> {
    if sse
        .data
        .as_ref()
        .is_some_and(|data| data.len() > MAX_MCP_HTTP_JSONRPC_MESSAGE_BYTES)
    {
        return Err(SseError::Body(Box::new(McpHttpBodyLimitError {
            context: "SSE data",
            max_bytes: MAX_MCP_HTTP_JSONRPC_MESSAGE_BYTES,
        })));
    }
    if sse
        .event
        .as_ref()
        .is_some_and(|event| event.len() > MAX_MCP_HTTP_JSONRPC_MESSAGE_BYTES)
    {
        sse.event = None;
    }
    Ok(sse)
}

fn parse_json_rpc_error(body: &str) -> Option<ServerJsonRpcMessage> {
    match serde_json::from_str::<ServerJsonRpcMessage>(body) {
        Ok(message @ JsonRpcMessage::Error(_)) => Some(message),
        _ => None,
    }
}

struct BoundedTokioChildProcess {
    child: Option<Child>,
    read: FramedRead<ChildStdout, JsonRpcMessageCodec<RxJsonRpcMessage<RoleClient>>>,
    write: Arc<
        Mutex<Option<FramedWrite<ChildStdin, JsonRpcMessageCodec<TxJsonRpcMessage<RoleClient>>>>>,
    >,
}

impl BoundedTokioChildProcess {
    fn new(child: Child, stdout: ChildStdout, stdin: ChildStdin, max_message_bytes: usize) -> Self {
        Self {
            child: Some(child),
            read: FramedRead::new(
                stdout,
                JsonRpcMessageCodec::<RxJsonRpcMessage<RoleClient>>::new_with_max_length(
                    max_message_bytes,
                ),
            ),
            write: Arc::new(Mutex::new(Some(FramedWrite::new(
                stdin,
                JsonRpcMessageCodec::<TxJsonRpcMessage<RoleClient>>::default(),
            )))),
        }
    }

    fn id(&self) -> Option<u32> {
        self.child.as_ref()?.id()
    }
}

impl Transport<RoleClient> for BoundedTokioChildProcess {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let write = self.write.clone();
        async move {
            let mut guard = write.lock().await;
            let Some(writer) = guard.as_mut() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "MCP stdio transport is closed",
                ));
            };
            writer.send(item).await.map_err(Into::into)
        }
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        let next = self.read.next();
        async {
            next.await.and_then(|result| {
                result
                    .inspect_err(|error| {
                        warn!(error = %error, "failed to read MCP stdio JSON-RPC message");
                    })
                    .ok()
            })
        }
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        let write = self.write.clone();
        let child = self.child.take();
        async move {
            drop(write.lock().await.take());
            if let Some(child) = child {
                wait_or_kill_child(child).await?;
            }
            Ok(())
        }
    }
}

fn spawn_bounded_stdio_process(
    mut command: Command,
    max_message_bytes: usize,
) -> std::io::Result<(BoundedTokioChildProcess, Option<ChildStderr>)> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("MCP stdio stdout was not piped"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("MCP stdio stdin was not piped"))?;
    let stderr = child.stderr.take();
    Ok((
        BoundedTokioChildProcess::new(child, stdout, stdin, max_message_bytes),
        stderr,
    ))
}

async fn wait_or_kill_child(mut child: Child) -> std::io::Result<()> {
    match time::timeout(Duration::from_secs(3), child.wait()).await {
        Ok(status) => status.map(|_| ()),
        Err(_) => {
            child.start_kill()?;
            let _ = time::timeout(Duration::from_secs(2), child.wait()).await;
            Ok(())
        }
    }
}

#[derive(Clone)]
struct KheishMcpClientHandler {
    workspace_root: String,
}

impl ClientHandler for KheishMcpClientHandler {
    async fn list_roots(
        &self,
        _context: rmcp::service::RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, rmcp::ErrorData> {
        Ok(ListRootsResult::new(vec![Root::new(format!(
            "file://{}",
            self.workspace_root
        ))]))
    }

    fn get_info(&self) -> rmcp::model::ClientInfo {
        let mut capabilities = ClientCapabilities::default();
        capabilities.roots = Some(RootsCapabilities {
            list_changed: Some(true),
        });
        InitializeRequestParams::new(
            capabilities,
            Implementation::new("kheish-mcp-client", env!("CARGO_PKG_VERSION"))
                .with_title("Kheish"),
        )
        .with_protocol_version(ProtocolVersion::V_2025_06_18)
    }
}

impl McpClient {
    /// Builds one uninitialized MCP client from server config.
    pub(crate) async fn connect(
        config: &McpServerConfig,
        workspace_root: &Path,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Result<Self> {
        let static_secret_ref_digests =
            capture_static_secret_ref_digests(config, auth_manager.as_deref())?;
        Ok(Self {
            config: config.clone(),
            auth_manager,
            service: Mutex::new(None),
            workspace_root: Mutex::new(Some(workspace_root.to_path_buf())),
            streamable_http_auth_fingerprint: Mutex::new(None),
            static_secret_ref_digests,
            shutdown_started: Arc::new(AtomicBool::new(false)),
            #[cfg(unix)]
            child_pid: Mutex::new(None),
            startup_timeout: Duration::from_millis(config.startup_timeout_ms),
            tool_timeout: Duration::from_millis(config.tool_timeout_ms),
        })
    }

    /// Initializes the connection and returns server info plus instructions.
    pub(crate) async fn initialize(
        &self,
        workspace_root: &Path,
    ) -> Result<rmcp::model::InitializeResult> {
        self.ensure_static_secret_refs_current()?;
        let mut guard = self.service.lock().await;
        if let Some(service) = guard.as_ref() {
            return service
                .peer()
                .peer_info()
                .cloned()
                .ok_or_else(|| anyhow!("initialized MCP server missing peer info"));
        }
        *self.workspace_root.lock().await = Some(workspace_root.to_path_buf());
        self.shutdown_started.store(false, Ordering::Relaxed);
        let service = match &self.config.transport {
            McpServerTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                let process = build_stdio_command(
                    command,
                    args,
                    env,
                    self.config.inherit_env,
                    workspace_root,
                    cwd.as_deref(),
                );
                let (transport, stderr) =
                    spawn_bounded_stdio_process(process, MAX_MCP_STDIO_JSONRPC_MESSAGE_BYTES)?;
                #[cfg(unix)]
                {
                    *self.child_pid.lock().await = transport.id();
                }
                if let Some(stderr) = stderr {
                    let shutdown_started = self.shutdown_started.clone();
                    let server_name = self.config.name.clone();
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            if !shutdown_started.load(Ordering::Relaxed) {
                                let (line, truncated) = normalize_mcp_log_line(&line);
                                if is_structured_mcp_warning(&line) {
                                    warn!(
                                        target: "kheish.mcp.stderr",
                                        mcp_server = %server_name,
                                        truncated,
                                        line = %line,
                                        "mcp server stderr"
                                    );
                                } else {
                                    debug!(
                                        target: "kheish.mcp.stderr",
                                        mcp_server = %server_name,
                                        truncated,
                                        line = %line,
                                        "mcp server stderr"
                                    );
                                }
                            }
                        }
                    });
                }
                let handler = KheishMcpClientHandler {
                    workspace_root: workspace_root.display().to_string(),
                };
                time::timeout(self.startup_timeout, rmcp::serve_client(handler, transport))
                    .await
                    .map_err(|_| anyhow!("timed out initializing MCP server"))??
            }
            McpServerTransport::StreamableHttp { url, headers, auth } => {
                let resolved = self.resolve_http_headers(headers, auth, false).await?;
                *self.streamable_http_auth_fingerprint.lock().await =
                    Some(resolved.fingerprint.clone());
                self.start_streamable_http_service(workspace_root, url, resolved.headers)
                    .await?
            }
        };
        let peer_info = service
            .peer()
            .peer_info()
            .cloned()
            .ok_or_else(|| anyhow!("initialized MCP server missing peer info"))?;
        *guard = Some(Arc::new(service));
        Ok(peer_info)
    }

    pub(crate) async fn shutdown(&self) {
        self.begin_shutdown();
        if let Some(service) = self.service.lock().await.take() {
            service.cancellation_token().cancel();
        }
        #[cfg(unix)]
        if let Some(child_pid) = self.child_pid.lock().await.take() {
            shutdown_process_group(child_pid).await;
        }
    }

    pub(crate) fn begin_shutdown(&self) {
        self.shutdown_started.store(true, Ordering::Relaxed);
    }

    pub(crate) async fn list_tools(&self) -> Result<ListToolsResult> {
        let service = self.service().await?;
        run_with_timeout(service.list_tools(None), self.tool_timeout, "tools/list").await
    }

    pub(crate) async fn list_resources(
        &self,
        params: Option<PaginatedRequestParams>,
    ) -> Result<ListResourcesResult> {
        let service = self.service().await?;
        run_with_timeout(
            service.list_resources(params),
            self.tool_timeout,
            "resources/list",
        )
        .await
    }

    pub(crate) async fn list_resource_templates(
        &self,
        params: Option<PaginatedRequestParams>,
    ) -> Result<ListResourceTemplatesResult> {
        let service = self.service().await?;
        run_with_timeout(
            service.list_resource_templates(params),
            self.tool_timeout,
            "resources/templates/list",
        )
        .await
    }

    pub(crate) async fn read_resource(
        &self,
        params: ReadResourceRequestParams,
    ) -> Result<ReadResourceResult> {
        let service = self.service().await?;
        run_with_timeout(
            service.read_resource(params),
            self.tool_timeout,
            "resources/read",
        )
        .await
    }

    pub(crate) async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult> {
        let service = self.service().await?;
        let arguments = match arguments {
            serde_json::Value::Object(map) => map,
            other => {
                return Err(anyhow!(
                    "MCP tool arguments must be a JSON object, got {other}"
                ));
            }
        };
        let params = CallToolRequestParams::new(name.to_string()).with_arguments(arguments);
        run_with_timeout(service.call_tool(params), self.tool_timeout, "tools/call").await
    }

    async fn service(&self) -> Result<ServiceHandle> {
        self.ensure_static_secret_refs_current()?;
        self.ensure_streamable_http_auth_current().await?;
        self.service
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("MCP client not initialized"))
    }

    async fn ensure_streamable_http_auth_current(&self) -> Result<()> {
        let McpServerTransport::StreamableHttp { url, headers, auth } = &self.config.transport
        else {
            return Ok(());
        };
        if !matches!(auth, McpHttpAuth::OAuth { .. }) {
            return Ok(());
        }

        let resolved = self.resolve_http_headers(headers, auth, false).await?;
        let current_fingerprint = self.streamable_http_auth_fingerprint.lock().await.clone();
        if current_fingerprint.as_deref() == Some(resolved.fingerprint.as_str()) {
            return Ok(());
        }

        let workspace_root = self
            .workspace_root
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("MCP client has no workspace root for HTTP reconnect"))?;
        let mut guard = self.service.lock().await;
        if let Some(service) = guard.take() {
            service.cancellation_token().cancel();
        }
        let service = self
            .start_streamable_http_service(&workspace_root, url, resolved.headers)
            .await?;
        *guard = Some(Arc::new(service));
        *self.streamable_http_auth_fingerprint.lock().await = Some(resolved.fingerprint);
        Ok(())
    }

    fn ensure_static_secret_refs_current(&self) -> Result<()> {
        let static_secret_refs = self
            .config
            .credential_secret_refs
            .iter()
            .filter(|secret_ref| !secret_ref.starts_with("mcp.oauth."))
            .collect::<Vec<_>>();
        if static_secret_refs.is_empty() {
            return Ok(());
        }
        let manager = self
            .auth_manager
            .as_ref()
            .ok_or_else(|| anyhow!("MCP credential auth manager is not configured"))?;
        let loaded_values = match &self.config.transport {
            McpServerTransport::Stdio { env, .. } => env.values().collect::<Vec<_>>(),
            McpServerTransport::StreamableHttp { headers, auth, .. } => {
                let mut values = headers.values().collect::<Vec<_>>();
                if let McpHttpAuth::BearerToken { token } = auth {
                    values.push(token);
                }
                values
            }
        };
        for secret_ref in static_secret_refs {
            let current = manager
                .secret_value(&AuthSlotId::new(secret_ref.as_str()))?
                .ok_or_else(|| anyhow!("MCP credential slot `{secret_ref}` is missing"))?;
            let current_digest = digest_serialize(&current)?;
            if let Some(expected_digest) = self.static_secret_ref_digests.get(secret_ref) {
                anyhow::ensure!(
                    expected_digest == &current_digest,
                    "MCP credential slot `{secret_ref}` has changed since server startup; reload MCP server credentials"
                );
            }
            anyhow::ensure!(
                loaded_values.iter().any(|loaded| *loaded == &current),
                "MCP credential slot `{secret_ref}` has changed since server startup; reload MCP server credentials"
            );
        }
        Ok(())
    }

    async fn start_streamable_http_service(
        &self,
        workspace_root: &Path,
        url: &str,
        custom_headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
    ) -> Result<RunningService<RoleClient, KheishMcpClientHandler>> {
        let handler = KheishMcpClientHandler {
            workspace_root: workspace_root.display().to_string(),
        };
        let http_config = StreamableHttpClientTransportConfig::with_uri(url.to_string())
            .custom_headers(custom_headers);
        let transport = StreamableHttpClientTransport::with_client(
            BoundedReqwestMcpHttpClient::new(),
            http_config,
        );
        time::timeout(self.startup_timeout, rmcp::serve_client(handler, transport))
            .await
            .map_err(|_| anyhow!("timed out initializing MCP server"))?
            .map_err(Into::into)
    }

    async fn resolve_http_headers(
        &self,
        headers: &BTreeMap<String, String>,
        auth: &McpHttpAuth,
        force_refresh: bool,
    ) -> Result<ResolvedHttpHeaders> {
        let mut resolved = headers.clone();
        match auth {
            McpHttpAuth::None => {}
            McpHttpAuth::BearerToken { token } => {
                resolved.insert("Authorization".to_string(), format!("Bearer {token}"));
            }
            McpHttpAuth::OAuth {
                slot_id,
                resource,
                scopes,
            } => {
                let manager = self
                    .auth_manager
                    .as_ref()
                    .ok_or_else(|| anyhow!("MCP OAuth auth manager is not configured"))?;
                let material = manager
                    .resolve_mcp_brokered(
                        &AuthSlotId::new(slot_id.clone()),
                        &self.config.name,
                        resource,
                        scopes,
                        &current_execution_context(),
                        force_refresh,
                    )
                    .await?;
                manager.ensure_resolved_material_active(&material)?;
                for (name, value) in material.headers {
                    resolved.insert(name, value);
                }
            }
        }
        let mut custom_headers = HashMap::new();
        let fingerprint = digest_serialize(&resolved)?;
        for (name, value) in resolved {
            custom_headers.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes())?,
                reqwest::header::HeaderValue::from_str(&value)?,
            );
        }
        Ok(ResolvedHttpHeaders {
            headers: custom_headers,
            fingerprint,
        })
    }
}

fn current_execution_context() -> ExecutionCredentialContext {
    kheish_runtime::current_execution_scope()
        .map(|scope| ExecutionCredentialContext {
            session_id: Some(scope.session_id).filter(|value| !value.is_empty()),
            agent_id: scope.agent_id,
            run_id: scope.run_id,
            principal_id: scope.principal_id,
            parent_principal_id: scope.parent_principal_id,
            delegation_id: scope.delegation_id,
            credential_scope: scope.credential_scope.normalized(),
        })
        .unwrap_or_default()
}

fn build_stdio_command(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    inherit_env: bool,
    workspace_root: &Path,
    cwd: Option<&Path>,
) -> Command {
    let mut process = Command::new(command);
    if !inherit_env {
        process.env_clear();
        for key in ["PATH", "TMPDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(key) {
                process.env(key, value);
            }
        }
    }
    process
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .args(args)
        .envs(env)
        .current_dir(resolve_cwd(workspace_root, cwd));
    #[cfg(unix)]
    {
        // Keep Ctrl+C on the daemon from being delivered to stdio MCP children.
        process.process_group(0);
    }
    process
}

fn is_structured_mcp_warning(line: &str) -> bool {
    let lowercase = line.to_ascii_lowercase();
    lowercase.starts_with("error:")
        || lowercase.starts_with("fatal:")
        || lowercase.starts_with("panic:")
        || lowercase.contains("traceback (most recent call last)")
        || lowercase.contains("unhandled exception")
        || lowercase.contains(" exception:")
}

fn normalize_mcp_log_line(line: &str) -> (String, bool) {
    let redacted = redact_text(line);
    let mut normalized = String::new();
    let mut chars = redacted.chars();
    for _ in 0..MAX_MCP_STDERR_LOG_CHARS {
        match chars.next() {
            Some(ch) => normalized.push(ch),
            None => return (normalized, false),
        }
    }
    if chars.next().is_some() {
        normalized.push('…');
        return (normalized, true);
    }
    (normalized, false)
}

#[cfg(unix)]
async fn shutdown_process_group(child_pid: u32) {
    for _ in 0..5 {
        if !process_group_exists(child_pid) {
            return;
        }
        time::sleep(Duration::from_millis(50)).await;
    }
    let _ = signal_process_group(child_pid, libc::SIGTERM);
    for _ in 0..10 {
        if !process_group_exists(child_pid) {
            return;
        }
        time::sleep(Duration::from_millis(50)).await;
    }
    let _ = signal_process_group(child_pid, libc::SIGKILL);
}

#[cfg(unix)]
fn process_group_exists(group_id: u32) -> bool {
    // kill(-pgid, 0) probes for any live member in the process group.
    let result = unsafe { libc::kill(-(group_id as i32), 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn signal_process_group(child_pid: u32, signal: i32) -> std::io::Result<()> {
    let result = unsafe { libc::kill(-(child_pid as i32), signal) };
    if result == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}

async fn run_with_timeout<F, T, E>(future: F, timeout: Duration, label: &str) -> Result<T>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    time::timeout(timeout, future)
        .await
        .map_err(|_| anyhow!("timed out during {label}"))?
        .map_err(|error| anyhow!("{label} failed: {error}"))
}

#[derive(Debug, Deserialize)]
struct CodexCredentialEntry {
    server_name: String,
    server_url: String,
    access_token: String,
}

/// Loads a bearer token from Codex-compatible MCP credentials when available.
pub(crate) fn load_codex_bearer_token(
    path: &Path,
    server_name: &str,
    server_url: &str,
) -> Result<String> {
    let content = std::fs::read_to_string(path)?;
    let entries: BTreeMap<String, CodexCredentialEntry> = serde_json::from_str(&content)?;
    entries
        .values()
        .find(|entry| entry.server_name == server_name && entry.server_url == server_url)
        .map(|entry| entry.access_token.clone())
        .ok_or_else(|| anyhow!("no Codex credential entry found for MCP server {server_name}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, OnceLock};

    use anyhow::anyhow;
    use kheish_auth::{
        AUTH_STORE_MASTER_KEY_ENV, AuthManager, McpOAuthAccountRecordInput,
        register_ephemeral_debug_redaction_token,
    };
    use kheish_types::CredentialScope;
    use parking_lot::Mutex as SyncMutex;

    use rmcp::model::{ClientJsonRpcMessage, ClientRequest, PingRequest, RequestId};
    use rmcp::transport::Transport;
    use rmcp::transport::streamable_http_client::StreamableHttpClient;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{
        BoundedReqwestMcpHttpClient, MAX_MCP_HTTP_JSONRPC_MESSAGE_BYTES, MAX_MCP_STDERR_LOG_CHARS,
        McpClient, SseByteLimitState, build_stdio_command, normalize_mcp_log_line,
        redacted_mcp_http_error, shutdown_process_group, spawn_bounded_stdio_process,
    };
    use crate::config::{McpHttpAuth, McpServerConfig, McpServerTransport};

    fn auth_env_guard() -> parking_lot::MutexGuard<'static, ()> {
        static LOCK: OnceLock<SyncMutex<()>> = OnceLock::new();
        let guard = LOCK.get_or_init(|| SyncMutex::new(())).lock();
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        guard
    }

    #[test]
    fn mcp_stderr_normalization_redacts_credentials() {
        let (line, truncated) = normalize_mcp_log_line(
            "Authorization: Bearer mcp-secret-token\naccess_token=sk-proj-secret",
        );
        assert!(!truncated);
        assert!(
            line.contains("Authorization: Bearer <redacted>"),
            "unexpected normalized line: {line}"
        );
        assert!(line.contains("access_token=<redacted>"));
        assert!(!line.contains("mcp-secret-token"));
        assert!(!line.contains("sk-proj-secret"));
    }

    #[test]
    fn mcp_stderr_normalization_redacts_opaque_tokens_before_truncation() {
        let opaque_secret = format!(
            "stderr-opaque-{}",
            "x".repeat(MAX_MCP_STDERR_LOG_CHARS + 64)
        );
        register_ephemeral_debug_redaction_token(opaque_secret.clone());

        let (line, truncated) =
            normalize_mcp_log_line(&format!("warning before {opaque_secret} warning after"));

        assert!(!truncated, "redaction should shrink the normalized line");
        assert!(
            line.contains("<redacted>"),
            "unexpected normalized line: {line}"
        );
        assert!(!line.contains("stderr-opaque-"));
        assert!(!line.contains(&opaque_secret[..MAX_MCP_STDERR_LOG_CHARS]));
    }

    #[test]
    fn mcp_http_error_redacts_challenge_and_body_credentials() {
        let challenge = redacted_mcp_http_error(
            "auth required",
            r#"Bearer error="invalid_token", error_description="Authorization: Bearer mcp-secret-token""#,
        );
        assert!(challenge.contains("Authorization: Bearer <redacted>"));
        assert!(!challenge.contains("mcp-secret-token"));

        let body = redacted_mcp_http_error(
            "HTTP 500",
            r#"{"error":"x-api-key: sk-proj-mcp-secret","access_token":"abc.def.ghi"}"#,
        );
        assert!(body.contains("x-api-key: <redacted>"));
        assert!(!body.contains("sk-proj-mcp-secret"));
        assert!(!body.contains("abc.def.ghi"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn static_mcp_secret_refs_fail_closed_after_slot_rotation_or_revoke() -> anyhow::Result<()>
    {
        let _guard = auth_env_guard();
        let temp = tempfile::tempdir()?;
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let slot_id = kheish_auth::AuthSlotId::new("mcp.custom.demo.token");
        auth_manager
            .store_generic_secret(slot_id.clone(), "mcp-static-token-1")
            .await?;
        let config = McpServerConfig {
            name: "static-http".to_string(),
            startup_timeout_ms: 1_000,
            tool_timeout_ms: 1_000,
            required: false,
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            inherit_env: false,
            credential_secret_refs: vec![slot_id.0.clone()],
            transport: McpServerTransport::StreamableHttp {
                url: "http://127.0.0.1:9/mcp".to_string(),
                headers: BTreeMap::new(),
                auth: McpHttpAuth::BearerToken {
                    token: "mcp-static-token-1".to_string(),
                },
            },
        };
        let client = McpClient::connect(&config, temp.path(), Some(auth_manager.clone())).await?;
        client.ensure_static_secret_refs_current()?;

        auth_manager
            .store_generic_secret(slot_id.clone(), "mcp-static-token-2")
            .await?;
        let initialize = client
            .initialize(temp.path())
            .await
            .expect_err("rotated static MCP secret should fail before starting HTTP transport");
        assert!(
            initialize
                .to_string()
                .contains("has changed since server startup")
        );
        let rotated = client
            .ensure_static_secret_refs_current()
            .expect_err("rotated static MCP secret should fail closed");
        assert!(
            rotated
                .to_string()
                .contains("has changed since server startup")
        );

        auth_manager.revoke_slot_leases(&slot_id)?;
        let revoked = client
            .ensure_static_secret_refs_current()
            .expect_err("revoked static MCP secret should fail closed");
        assert!(revoked.to_string().contains("has been revoked"));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn static_mcp_secret_refs_detect_swapped_values() -> anyhow::Result<()> {
        let _guard = auth_env_guard();
        let temp = tempfile::tempdir()?;
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let first_slot = kheish_auth::AuthSlotId::new("mcp.custom.demo.first");
        let second_slot = kheish_auth::AuthSlotId::new("mcp.custom.demo.second");
        auth_manager
            .store_generic_secret(first_slot.clone(), "mcp-token-a")
            .await?;
        auth_manager
            .store_generic_secret(second_slot.clone(), "mcp-token-b")
            .await?;
        let config = McpServerConfig {
            name: "static-http".to_string(),
            startup_timeout_ms: 1_000,
            tool_timeout_ms: 1_000,
            required: false,
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            inherit_env: false,
            credential_secret_refs: vec![first_slot.0.clone(), second_slot.0.clone()],
            transport: McpServerTransport::StreamableHttp {
                url: "http://127.0.0.1:9/mcp".to_string(),
                headers: BTreeMap::from([
                    ("x-first-token".to_string(), "mcp-token-a".to_string()),
                    ("x-second-token".to_string(), "mcp-token-b".to_string()),
                ]),
                auth: McpHttpAuth::None,
            },
        };
        let client = McpClient::connect(&config, temp.path(), Some(auth_manager.clone())).await?;
        client.ensure_static_secret_refs_current()?;

        auth_manager
            .store_generic_secret(first_slot.clone(), "mcp-token-b")
            .await?;
        auth_manager
            .store_generic_secret(second_slot.clone(), "mcp-token-a")
            .await?;
        let error = client
            .ensure_static_secret_refs_current()
            .expect_err("swapped secret values should fail by slot binding");
        assert!(
            error
                .to_string()
                .contains("has changed since server startup")
        );
        Ok(())
    }

    #[cfg(unix)]
    fn process_group_id(pid: u32) -> anyhow::Result<u32> {
        let output = std::process::Command::new("ps")
            .args(["-o", "pgid=", "-p", &pid.to_string()])
            .output()?;
        if !output.status.success() {
            anyhow::bail!("ps failed for pid {pid}");
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().parse()?)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_children_run_in_a_distinct_process_group() -> anyhow::Result<()> {
        let workspace_root = std::env::temp_dir();
        let mut command = build_stdio_command(
            "bash",
            &["-lc".to_string(), "sleep 30".to_string()],
            &BTreeMap::new(),
            true,
            &workspace_root,
            None,
        );
        let mut child = command.spawn()?;
        let child_pid = child.id().ok_or_else(|| anyhow!("child pid missing"))?;
        let parent_pgid = process_group_id(std::process::id())?;
        let child_pgid = process_group_id(child_pid)?;
        child.start_kill()?;
        let _ = child.wait().await;
        assert_ne!(
            child_pgid, parent_pgid,
            "stdio MCP child should not share the daemon process group"
        );
        Ok(())
    }

    #[test]
    fn stdio_command_uses_configured_relative_cwd_under_workspace_root() {
        let workspace_root = std::env::temp_dir().join("kheish-mcp-workspace");
        let command = build_stdio_command(
            "bash",
            &["-lc".to_string(), "true".to_string()],
            &BTreeMap::new(),
            true,
            &workspace_root,
            Some(std::path::Path::new("server-root")),
        );
        assert_eq!(
            command.as_std().get_current_dir(),
            Some(workspace_root.join("server-root").as_path())
        );
    }

    #[cfg(unix)]
    fn process_group_has_members(pgid: u32) -> anyhow::Result<bool> {
        let output = std::process::Command::new("ps")
            .args(["-o", "pid=", "-g", &pgid.to_string()])
            .output()?;
        if !output.status.success() {
            return Ok(false);
        }
        Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_shutdown_terminates_descendants() -> anyhow::Result<()> {
        let workspace_root = std::env::temp_dir();
        let mut command = build_stdio_command(
            "bash",
            &["-lc".to_string(), "sleep 30 & wait".to_string()],
            &BTreeMap::new(),
            true,
            &workspace_root,
            None,
        );
        let mut child = command.spawn()?;
        let child_pid = child.id().ok_or_else(|| anyhow!("child pid missing"))?;
        shutdown_process_group(child_pid).await;
        let _ = child.wait().await;
        assert!(
            !process_group_has_members(child_pid)?,
            "MCP process group should be fully terminated on shutdown"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_stdio_transport_rejects_oversized_jsonrpc_lines() -> anyhow::Result<()> {
        let workspace_root = std::env::temp_dir();
        let command = build_stdio_command(
            "bash",
            &[
                ("printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"'; ".to_owned()
                    + "printf '%*s' 512 '' | tr ' ' x; "
                    + "printf '\"}\\n'; sleep 30"),
            ],
            &BTreeMap::new(),
            true,
            &workspace_root,
            None,
        );
        let (mut transport, _stderr) = spawn_bounded_stdio_process(command, 128)?;

        let received =
            tokio::time::timeout(std::time::Duration::from_secs(2), transport.receive()).await?;
        assert!(
            received.is_none(),
            "oversized MCP stdio message should close the receive stream"
        );
        transport.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn bounded_http_transport_rejects_oversized_json_content_length() -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await?;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                MAX_MCP_HTTP_JSONRPC_MESSAGE_BYTES + 1
            );
            socket.write_all(response.as_bytes()).await?;
            Ok::<_, anyhow::Error>(())
        });

        let client = BoundedReqwestMcpHttpClient::new();
        let message = ClientJsonRpcMessage::request(
            ClientRequest::PingRequest(PingRequest::default()),
            RequestId::Number(1),
        );
        let result = client
            .post_message(
                Arc::from(format!("http://{address}/mcp")),
                message,
                None,
                None,
                Default::default(),
            )
            .await;
        server.await??;
        let error = result.expect_err("oversized JSON response should fail before body read");
        assert!(
            error.to_string().contains("body limit") || error.to_string().contains("too large"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn mcp_get_and_delete_errors_do_not_expose_credentials() -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            for body in [
                "Authorization: Bearer mcp-get-secret https://example.test/callback?access_token=get-secret",
                "https://example.test/callback?client_secret=delete-secret",
            ] {
                let (mut socket, _) = listener.accept().await?;
                let mut request = [0_u8; 1024];
                let _ = socket.read(&mut request).await?;
                let response = format!(
                    "HTTP/1.1 500 Internal Server Error\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await?;
            }
            Ok::<_, anyhow::Error>(())
        });

        let client = BoundedReqwestMcpHttpClient::new();
        let get_result = client
            .get_stream(
                Arc::from(format!("http://{address}/mcp?access_token=url-get-secret")),
                Arc::from("session-1"),
                None,
                None,
                Default::default(),
            )
            .await;
        let get = match get_result {
            Ok(_) => anyhow::bail!("GET error should be surfaced redacted"),
            Err(error) => error,
        };
        let rendered = get.to_string();
        assert!(rendered.contains("Bearer <redacted>"));
        assert!(rendered.contains("access_token=<redacted>"));
        assert!(!rendered.contains("mcp-get-secret"));
        assert!(!rendered.contains("get-secret"));
        assert!(!rendered.contains("url-get-secret"));

        let delete = client
            .delete_session(
                Arc::from(format!(
                    "http://{address}/mcp?client_secret=url-delete-secret"
                )),
                Arc::from("session-1"),
                None,
                Default::default(),
            )
            .await
            .expect_err("DELETE error should be surfaced redacted");
        let rendered = delete.to_string();
        assert!(rendered.contains("client_secret=<redacted>"));
        assert!(!rendered.contains("delete-secret"));
        assert!(!rendered.contains("url-delete-secret"));
        server.await??;
        Ok(())
    }

    #[test]
    fn bounded_sse_state_rejects_oversized_event_before_parser() {
        let mut state = SseByteLimitState::default();
        let huge = vec![b'x'; MAX_MCP_HTTP_JSONRPC_MESSAGE_BYTES + 1];
        let error = state
            .observe(&huge)
            .expect_err("oversized SSE event should fail before parser");
        assert!(error.to_string().contains("body limit"));
    }

    #[test]
    fn restricted_stdio_command_does_not_restore_home() -> anyhow::Result<()> {
        let workspace_root = std::env::temp_dir();
        let command = build_stdio_command(
            "env",
            &Vec::new(),
            &BTreeMap::new(),
            false,
            &workspace_root,
            None,
        );
        let envs = command
            .as_std()
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().to_string(),
                    value.map(|value| value.to_string_lossy().to_string()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(envs.get("HOME"), None);
        assert!(envs.contains_key("PATH"));
        Ok(())
    }

    #[tokio::test]
    async fn oauth_http_headers_are_brokered_for_each_resolution() -> anyhow::Result<()> {
        let _guard = auth_env_guard();
        let temp = tempfile::tempdir()?;
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let slot_id = kheish_auth::AuthSlotId::new("mcp.oauth.oauth-http");
        auth_manager
            .store_mcp_oauth_account(McpOAuthAccountRecordInput {
                slot_id: slot_id.clone(),
                server_name: "oauth-http".to_string(),
                resource: "https://example.com/mcp".to_string(),
                issuer: "https://issuer.example.com".to_string(),
                authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                token_endpoint: "https://issuer.example.com/token".to_string(),
                client_id: "client".to_string(),
                client_secret: None,
                access_token: "oauth-live-1".to_string(),
                refresh_token: None,
                expires_at_ms: None,
                scopes: vec!["read".to_string()],
            })
            .await?;

        let headers = BTreeMap::new();
        let auth = McpHttpAuth::OAuth {
            slot_id: slot_id.0.clone(),
            resource: "https://example.com/mcp".to_string(),
            scopes: vec!["read".to_string()],
        };
        let config = McpServerConfig {
            name: "oauth-http".to_string(),
            startup_timeout_ms: 10_000,
            tool_timeout_ms: 120_000,
            required: false,
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            inherit_env: true,
            credential_secret_refs: vec![slot_id.0.clone()],
            transport: McpServerTransport::StreamableHttp {
                url: "https://example.com/mcp".to_string(),
                headers: headers.clone(),
                auth: auth.clone(),
            },
        };
        let client = McpClient::connect(&config, temp.path(), Some(auth_manager.clone())).await?;
        let scope = kheish_runtime::ExecutionScope {
            session_id: "mcp-session".to_string(),
            credential_scope: CredentialScope {
                mcp_server_allow: vec!["oauth-http".to_string()],
                ..CredentialScope::default()
            },
            ..kheish_runtime::ExecutionScope::default()
        };

        let first = kheish_runtime::scope_execution(scope.clone(), Default::default(), async {
            client.resolve_http_headers(&headers, &auth, false).await
        })
        .await?;
        assert_eq!(
            first
                .headers
                .get(&reqwest::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer oauth-live-1")
        );

        auth_manager
            .store_mcp_oauth_account(McpOAuthAccountRecordInput {
                slot_id: slot_id.clone(),
                server_name: "oauth-http".to_string(),
                resource: "https://example.com/mcp".to_string(),
                issuer: "https://issuer.example.com".to_string(),
                authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                token_endpoint: "https://issuer.example.com/token".to_string(),
                client_id: "client".to_string(),
                client_secret: None,
                access_token: "oauth-live-2".to_string(),
                refresh_token: None,
                expires_at_ms: None,
                scopes: vec!["read".to_string()],
            })
            .await?;
        let rotated = kheish_runtime::scope_execution(scope.clone(), Default::default(), async {
            client.resolve_http_headers(&headers, &auth, false).await
        })
        .await?;
        assert_ne!(first.fingerprint, rotated.fingerprint);
        assert_eq!(
            rotated
                .headers
                .get(&reqwest::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer oauth-live-2")
        );

        auth_manager.revoke_subject_and_leases("session:mcp-session")?;
        let revoked_result = kheish_runtime::scope_execution(scope, Default::default(), async {
            client.resolve_http_headers(&headers, &auth, false).await
        })
        .await;
        let error = match revoked_result {
            Ok(_) => {
                anyhow::bail!("revoked execution subject should not receive MCP OAuth material")
            }
            Err(error) => error,
        };
        assert!(error.to_string().contains("has been revoked"));
        Ok(())
    }
}
