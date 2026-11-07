//! Daemon tests for the runtime contract an embedding control plane relies on:
//! session tool allow-lists, caller-set run limits, routes with brokered
//! provider slots, MCP execution context, HTTP hook transport, and purges.

use super::*;
use kheish_types::RunErrorCode;

/// Records the tool names advertised on every model request, then replays
/// the scripted turns.
struct ToolCapturingProvider {
    inner: ScriptedProvider,
    seen_tools: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait]
impl ModelProvider for ToolCapturingProvider {
    async fn stream(
        &self,
        request: ModelRuntimeRequest,
        sink: ModelEventSink,
    ) -> std::result::Result<(), ProviderError> {
        self.seen_tools.lock().push(
            request
                .available_tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect(),
        );
        self.inner.stream(request, sink).await
    }
}

/// Starts a daemon whose `echo` tool always asks for approval, with a model
/// that records the advertised tools.
async fn tool_capturing_daemon(
    state_root: &Path,
    script: Vec<Result<Vec<ModelStreamEvent>, ProviderError>>,
) -> Result<(
    SocketAddr,
    oneshot::Sender<()>,
    Arc<Mutex<Vec<Vec<String>>>>,
)> {
    let events = DaemonEventBus::new(256);
    let debug = DebugControl::new(DebugCaptureLevel::Off);
    let observer: Arc<dyn RuntimeObserver> = DaemonObserver::shared(
        events.clone(),
        debug.clone(),
        FileDebugStore::new(state_root),
    );
    let seen_tools = Arc::new(Mutex::new(Vec::new()));
    let model = Arc::new(ModelRuntime::new(
        ToolCapturingProvider {
            inner: ScriptedProvider(Mutex::new(immediate_script_queue(script))),
            seen_tools: seen_tools.clone(),
        },
        ModelRetryPolicy::default(),
        ModelBudget::default(),
        observer.clone(),
    ));
    let permissions = Arc::new(PermissionEngine::new(
        vec![],
        vec![],
        vec![PermissionRule {
            scope: PermissionScope::Session,
            tool_name_pattern: "echo".to_string(),
            behavior: PermissionBehavior::Ask,
            reason: Some("approval required".to_string()),
        }],
        observer.clone(),
    ));
    let model_control =
        Some(Arc::new(ScriptedModelControl::new("scripted-model")) as Arc<dyn DaemonModelControl>);
    let (address, shutdown) = start_test_daemon(
        state_root,
        model,
        permissions,
        model_control,
        events,
        debug,
        observer,
        |tools| tools.register(EchoTool),
    )
    .await?;
    Ok((address, shutdown, seen_tools))
}

fn echo_tool_call_turn(message_id: &str, call_id: &str) -> Vec<ModelStreamEvent> {
    vec![
        ModelStreamEvent::MessageId {
            value: message_id.to_string(),
        },
        ModelStreamEvent::ToolCall {
            call: kheish_types::ToolCallRecord {
                id: call_id.to_string(),
                name: "echo".to_string(),
                input: json!({"text": "hidden"}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            },
        },
        ModelStreamEvent::Stop {
            reason: kheish_types::ModelFinishReason::ToolCalls,
        },
    ]
}

#[tokio::test]
async fn daemon_session_tool_allow_list_hides_tools_and_never_asks_for_them() -> Result<()> {
    let temp = tempdir()?;
    let state_root = temp.path().join("daemon-state");
    let (address, shutdown, seen_tools) = tool_capturing_daemon(
        &state_root,
        vec![
            Ok(echo_tool_call_turn(
                "assistant-hidden-1",
                "call-hidden-echo",
            )),
            Ok(scripted_events(
                "assistant-hidden-2",
                "Done without echo.",
                kheish_types::ModelFinishReason::Completed,
            )),
        ],
    )
    .await?;
    let client = Client::new();
    let base = format!("http://{address}");
    create_test_session(&client, &base, "allow-list-session").await?;

    // An enable entry outside the allow-list can never be visible: refused.
    let refused = client
        .put(format!(
            "{base}/v1/sessions/allow-list-session/tool-overrides"
        ))
        .json(&json!({"allow": ["mcp__fregate__scan_target"], "enable": ["echo"]}))
        .send()
        .await?;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let problem = refused.json::<ProblemDetails>().await?;
    assert_eq!(problem.code, "invalid_tool_overrides");
    // A misspelled field is refused, never stored as empty overrides.
    let misspelled = client
        .put(format!(
            "{base}/v1/sessions/allow-list-session/tool-overrides"
        ))
        .json(&json!({"alow": ["mcp__fregate__scan_target"]}))
        .send()
        .await?;
    assert!(
        misspelled.status().is_client_error(),
        "unknown fields must be refused: {}",
        misspelled.status()
    );

    let view = client
        .put(format!(
            "{base}/v1/sessions/allow-list-session/tool-overrides"
        ))
        .json(&json!({"allow": [" mcp__fregate__scan_target ", "mcp__fregate__scan_target"]}))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    assert_eq!(
        view["tool_overrides"],
        json!({"allow": ["mcp__fregate__scan_target"]})
    );
    let stored = client
        .get(format!(
            "{base}/v1/sessions/allow-list-session/tool-overrides"
        ))
        .send()
        .await?
        .error_for_status()?
        .json::<kheish_types::SessionToolOverrides>()
        .await?;
    assert_eq!(
        stored.allow,
        Some(vec!["mcp__fregate__scan_target".to_string()])
    );

    let run = submit_test_input(&client, &base, "allow-list-session", "call echo").await?;
    let finished =
        wait_for_run_status(&client, &base, &run.run_id, &[DaemonRunStatus::Completed]).await?;
    // The hidden tool never became an approval request.
    assert!(finished.pending_approvals.is_empty());
    let events = client
        .get(format!("{base}/v1/runs/{}/events", run.run_id))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    assert!(
        !events.to_string().contains("waiting_for_approval"),
        "a hidden tool must not wait for approval: {events}"
    );
    // The model was never shown echo, and no permission was ever evaluated.
    let seen = seen_tools.lock().clone();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|tools| tools.is_empty()), "{seen:?}");
    let audits = client
        .get(format!(
            "{base}/v1/sessions/allow-list-session/permission-audits"
        ))
        .send()
        .await?
        .error_for_status()?
        .json::<SessionPermissionAuditListView>()
        .await?;
    assert!(
        audits.audits.iter().all(|audit| audit.tool_name != "echo"),
        "{:?}",
        audits.audits
    );

    // Clearing the overrides restores the profile surface.
    client
        .delete(format!(
            "{base}/v1/sessions/allow-list-session/tool-overrides"
        ))
        .send()
        .await?
        .error_for_status()?;
    let cleared = client
        .get(format!(
            "{base}/v1/sessions/allow-list-session/tool-overrides"
        ))
        .send()
        .await?
        .error_for_status()?
        .json::<kheish_types::SessionToolOverrides>()
        .await?;
    assert!(cleared.is_empty());

    let _ = shutdown.send(());
    wait_for_daemon_shutdown(&client, &base).await?;
    Ok(())
}

/// One request received by the Anthropic Messages mock.
#[derive(Clone, Debug)]
struct CapturedModelRequest {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

type CapturedModelRequests = Arc<Mutex<Vec<CapturedModelRequest>>>;

/// One scripted reply of the Anthropic Messages mock.
#[derive(Clone, Debug)]
enum MockReply {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// Answers with text after a delay, like a slow model turn.
    Stall {
        delay_ms: u64,
        text: String,
    },
}

/// A local Anthropic Messages endpoint: it serves scripted replies (then a
/// default text reply) on every path and records each request, so tests can
/// assert which endpoint, credential, and headers a route used.
struct AnthropicMock {
    base: String,
    captured: CapturedModelRequests,
    script: Arc<Mutex<VecDeque<MockReply>>>,
}

impl AnthropicMock {
    fn push(&self, reply: MockReply) {
        self.script.lock().push_back(reply);
    }

    fn last_request(&self) -> Option<CapturedModelRequest> {
        self.captured.lock().last().cloned()
    }
}

fn anthropic_sse_event(name: &str, data: Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn anthropic_sse(reply: &MockReply) -> String {
    let mut body = anthropic_sse_event(
        "message_start",
        json!({"type": "message_start", "message": {"id": "msg-mock", "usage": {"input_tokens": 5, "output_tokens": 0}}}),
    );
    let stop_reason = match reply {
        MockReply::Text(text) | MockReply::Stall { text, .. } => {
            body.push_str(&anthropic_sse_event(
                "content_block_start",
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": text}}),
            ));
            "end_turn"
        }
        MockReply::ToolUse { id, name, input } => {
            body.push_str(&anthropic_sse_event(
                "content_block_start",
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
            ));
            body.push_str(&anthropic_sse_event(
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}),
            ));
            "tool_use"
        }
    };
    body.push_str(&anthropic_sse_event(
        "content_block_stop",
        json!({"type": "content_block_stop", "index": 0}),
    ));
    body.push_str(&anthropic_sse_event(
        "message_delta",
        json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}, "usage": {"input_tokens": 5, "output_tokens": 3}}),
    ));
    body.push_str(&anthropic_sse_event(
        "message_stop",
        json!({"type": "message_stop"}),
    ));
    body
}

async fn spawn_anthropic_messages_mock(default_text: &str) -> Result<AnthropicMock> {
    #[derive(Clone)]
    struct MockState {
        default_text: String,
        captured: CapturedModelRequests,
        script: Arc<Mutex<VecDeque<MockReply>>>,
    }

    async fn messages(
        State(state): State<MockState>,
        uri: axum::http::Uri,
        headers: axum::http::HeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        state.captured.lock().push(CapturedModelRequest {
            path: uri.path().to_string(),
            headers: headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_ascii_lowercase(),
                        value.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect(),
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        });
        let reply = state
            .script
            .lock()
            .pop_front()
            .unwrap_or_else(|| MockReply::Text(state.default_text.clone()));
        if let MockReply::Stall { delay_ms, .. } = &reply {
            tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
        }
        ([(CONTENT_TYPE, "text/event-stream")], anthropic_sse(&reply))
    }

    let captured: CapturedModelRequests = Arc::new(Mutex::new(Vec::new()));
    let script = Arc::new(Mutex::new(VecDeque::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let router = Router::new().fallback(messages).with_state(MockState {
        default_text: default_text.to_string(),
        captured: captured.clone(),
        script: script.clone(),
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(AnthropicMock {
        base: format!("http://{address}"),
        captured,
        script,
    })
}

/// Boots a production-built daemon with one startup Anthropic route aimed at
/// `boot_endpoint`; runtime-added routes reload from the state-root overlay.
async fn anthropic_provider_daemon(
    state_root: &Path,
    workspace_root: &Path,
    boot_endpoint: &str,
) -> Result<(SocketAddr, oneshot::Sender<()>)> {
    anthropic_provider_daemon_with_config(state_root, workspace_root, boot_endpoint, |_| {}).await
}

/// Same as [`anthropic_provider_daemon`], with daemon options adjusted first.
async fn anthropic_provider_daemon_with_config(
    state_root: &Path,
    workspace_root: &Path,
    boot_endpoint: &str,
    configure: impl FnOnce(&mut DaemonConfig),
) -> Result<(SocketAddr, oneshot::Sender<()>)> {
    ensure_auth_store_master_key();
    let auth_manager = AuthManager::new(state_root.join("auth/global-slots.json"))?;
    let mut boot = AnthropicProviderConfig::new("claude-boot", "boot-key");
    boot.base_url = boot_endpoint.to_string();
    let route = ConfiguredModelRoute::new("boot", ModelRouteConfig::Anthropic(boot));
    let mut config = DaemonConfig::new(
        "127.0.0.1:0".parse::<SocketAddr>()?,
        state_root,
        workspace_root,
    );
    configure(&mut config);
    let (service, listener) =
        build_provider_daemon(config, vec![route], Vec::new(), Vec::new(), auth_manager).await?;
    let address = listener.local_addr()?;
    let (shutdown, shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        let _ = service
            .serve_with_shutdown(listener, async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });
    Ok((address, shutdown))
}

async fn put_anthropic_api_key_slot(
    client: &Client,
    base: &str,
    slot_id: &str,
    api_key: &str,
) -> Result<()> {
    client
        .post(format!("{base}/v1/runtime/secrets"))
        .json(&json!({
            "slot_id": slot_id,
            "provider": "anthropic",
            "mode": "api_key",
            "state": {"kind": "api_key", "api_key": api_key},
            "updated_at_ms": 1_794_004_700_000_u64,
        }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// Submits one detached run on `route_id` and waits for it to complete.
async fn complete_run_on_route(
    client: &Client,
    base: &str,
    session_id: &str,
    route_id: &str,
) -> Result<RunView> {
    let run = client
        .post(format!("{base}/v1/sessions/{session_id}/runs"))
        .json(&json!({"content": "Say hello", "provider": route_id}))
        .send()
        .await?
        .error_for_status()?
        .json::<RunView>()
        .await?;
    wait_for_run_status(client, base, &run.run_id, &[DaemonRunStatus::Completed]).await
}

async fn post_route(client: &Client, base: &str, body: Value) -> Result<reqwest::Response> {
    Ok(client
        .post(format!("{base}/v1/runtime/routes"))
        .json(&body)
        .send()
        .await?)
}

#[tokio::test]
async fn runtime_routes_accept_base_url_and_provider_auth_slots() -> Result<()> {
    let temp = tempdir()?;
    let state_root = temp.path().join("daemon-state");
    let workspace_root = temp.path().join("workspace");
    let mock = spawn_anthropic_messages_mock("hello from the relay").await?;
    let mock_base = mock.base.clone();
    let captured = mock.captured.clone();
    let (address, shutdown) = anthropic_provider_daemon(
        &state_root,
        &workspace_root,
        &format!("{mock_base}/boot/v1/messages"),
    )
    .await?;
    let client = Client::new();
    let base = format!("http://{address}");
    let relay_endpoint = format!("{mock_base}/relay/v1/messages");

    put_anthropic_api_key_slot(&client, &base, "fregate.callback", "relay-token-A").await?;
    client
        .post(format!("{base}/v1/runtime/secrets"))
        .json(&json!({
            "slot_id": "generic.only",
            "provider": "generic",
            "mode": "opaque_secret",
            "state": {"value": "not-an-anthropic-credential"},
            "updated_at_ms": 1_794_004_700_000_u64,
        }))
        .send()
        .await?
        .error_for_status()?;

    // Unknown fields are refused instead of silently dropped.
    let unknown = post_route(
        &client,
        &base,
        json!({"route_id": "fregate", "provider": "anthropic", "model": "claude-sonnet-4-5",
               "auth_ref": "fregate.callback", "base_uri": relay_endpoint}),
    )
    .await?;
    assert!(
        unknown.status().is_client_error(),
        "unknown route fields must be refused: {}",
        unknown.status()
    );
    // The slot must be typed for the route's provider.
    let mismatch = post_route(
        &client,
        &base,
        json!({"route_id": "fregate", "provider": "anthropic", "model": "claude-sonnet-4-5",
               "auth_ref": "generic.only"}),
    )
    .await?;
    assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        mismatch.json::<ProblemDetails>().await?.code,
        "route_auth_ref_provider_mismatch"
    );
    let missing = post_route(
        &client,
        &base,
        json!({"route_id": "fregate", "provider": "anthropic", "model": "claude-sonnet-4-5",
               "auth_ref": "no.such.slot"}),
    )
    .await?;
    assert_eq!(
        missing.json::<ProblemDetails>().await?.code,
        "route_auth_ref_missing"
    );
    let bad_url = post_route(
        &client,
        &base,
        json!({"route_id": "fregate", "provider": "anthropic", "model": "claude-sonnet-4-5",
               "auth_ref": "fregate.callback", "base_url": "ftp://relay.internal/v1/messages"}),
    )
    .await?;
    assert_eq!(
        bad_url.json::<ProblemDetails>().await?.code,
        "route_base_url_invalid"
    );
    let ambiguous = post_route(
        &client,
        &base,
        json!({"route_id": "fregate", "provider": "anthropic", "model": "claude-sonnet-4-5",
               "auth_ref": "fregate.callback", "api_key": "inline"}),
    )
    .await?;
    assert_eq!(
        ambiguous.json::<ProblemDetails>().await?.code,
        "route_api_key_ambiguous"
    );

    let added = post_route(
        &client,
        &base,
        json!({"route_id": "fregate", "provider": "anthropic", "model": "claude-sonnet-4-5",
               "auth_ref": "fregate.callback", "base_url": relay_endpoint}),
    )
    .await?
    .error_for_status()?
    .json::<Value>()
    .await?;
    let route = added["routes"]
        .as_array()
        .and_then(|routes| routes.iter().find(|route| route["route_id"] == "fregate"))
        .cloned()
        .expect("fregate route listed");
    assert_eq!(route["auth_ref"], "fregate.callback");

    // The overlay records the slot and the endpoint, never the credential.
    let overlay = std::fs::read_to_string(state_root.join("routes-overlay.json"))?;
    assert!(
        overlay.contains("\"auth_ref\": \"fregate.callback\""),
        "{overlay}"
    );
    assert!(overlay.contains(&relay_endpoint), "{overlay}");
    assert!(!overlay.contains("relay-token"), "{overlay}");

    create_test_session(&client, &base, "route-session").await?;
    complete_run_on_route(&client, &base, "route-session", "fregate").await?;
    {
        let requests = captured.lock();
        let last = requests.last().expect("relay received the model request");
        assert_eq!(last.path, "/relay/v1/messages");
        assert_eq!(
            last.headers.get("x-api-key").map(String::as_str),
            Some("relay-token-A")
        );
        assert_eq!(last.body["model"], "claude-sonnet-4-5");
    }

    // Rotating the slot takes effect on the next request, without a restart.
    put_anthropic_api_key_slot(&client, &base, "fregate.callback", "relay-token-B").await?;
    complete_run_on_route(&client, &base, "route-session", "fregate").await?;
    assert_eq!(
        captured
            .lock()
            .last()
            .and_then(|request| request.headers.get("x-api-key").cloned())
            .as_deref(),
        Some("relay-token-B")
    );

    // An inline key route with an endpoint override is ready too.
    post_route(
        &client,
        &base,
        json!({"route_id": "inline", "provider": "anthropic", "model": "claude-sonnet-4-5",
               "api_key": "inline-key", "base_url": format!("{mock_base}/inline/v1/messages")}),
    )
    .await?
    .error_for_status()?;
    complete_run_on_route(&client, &base, "route-session", "inline").await?;
    {
        let requests = captured.lock();
        let last = requests.last().expect("inline route reached the mock");
        assert_eq!(last.path, "/inline/v1/messages");
        assert_eq!(
            last.headers.get("x-api-key").map(String::as_str),
            Some("inline-key")
        );
    }

    // Both routes rebuild from the overlay after a restart.
    let _ = shutdown.send(());
    wait_for_daemon_shutdown(&client, &base).await?;
    let (address, shutdown) = anthropic_provider_daemon(
        &state_root,
        &workspace_root,
        &format!("{mock_base}/boot/v1/messages"),
    )
    .await?;
    let base = format!("http://{address}");
    complete_run_on_route(&client, &base, "route-session", "fregate").await?;
    {
        let requests = captured.lock();
        let last = requests
            .last()
            .expect("relay received the request after restart");
        assert_eq!(last.path, "/relay/v1/messages");
        assert_eq!(
            last.headers.get("x-api-key").map(String::as_str),
            Some("relay-token-B")
        );
    }
    let removed = client
        .delete(format!("{base}/v1/runtime/routes/fregate"))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    assert!(
        !runtime_route_ids(&removed).contains(&"fregate".to_string()),
        "{removed:#}"
    );
    // Removing the route leaves the caller-owned provider slot in place.
    let secrets = client
        .get(format!("{base}/v1/runtime/secrets"))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    assert!(
        secrets.to_string().contains("fregate.callback"),
        "{secrets}"
    );

    let _ = shutdown.send(());
    wait_for_daemon_shutdown(&client, &base).await?;
    Ok(())
}

/// Requests seen by the streamable-HTTP MCP stub.
#[derive(Clone, Default)]
struct McpStubLog {
    /// `params` of every `tools/call`.
    calls: Arc<Mutex<Vec<Value>>>,
    /// `Authorization` header of every `tools/call`.
    call_authorizations: Arc<Mutex<Vec<String>>>,
}

/// A minimal streamable-HTTP MCP server exposing one `scan_target` tool and
/// answering with plain JSON responses.
async fn spawn_mcp_stub() -> Result<(String, McpStubLog)> {
    async fn rpc(
        State(log): State<McpStubLog>,
        headers: axum::http::HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        let Ok(message) = serde_json::from_slice::<Value>(&body) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let Some(id) = message.get("id").cloned() else {
            // Notifications such as `notifications/initialized`.
            return StatusCode::ACCEPTED.into_response();
        };
        let method = message["method"].as_str().unwrap_or_default().to_string();
        let result = match method.as_str() {
            "initialize" => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fregate-stub", "version": "1.0.0"}
            }),
            "tools/list" => json!({"tools": [{
                "name": "scan_target",
                "description": "Scans one target.",
                "inputSchema": {"type": "object", "properties": {"target": {"type": "string"}}}
            }]}),
            "tools/call" => {
                log.calls.lock().push(message["params"].clone());
                log.call_authorizations.lock().push(
                    headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_string(),
                );
                json!({"content": [{"type": "text", "text": "SCANNED"}], "isError": false})
            }
            _ => json!({}),
        };
        let mut response =
            Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response();
        if method == "initialize" {
            response.headers_mut().insert(
                "mcp-session-id",
                axum::http::HeaderValue::from_static("fregate-stub-session"),
            );
        }
        response
    }

    let log = McpStubLog::default();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let router = Router::new()
        .route(
            "/mcp",
            post(rpc)
                .get(|| async { StatusCode::METHOD_NOT_ALLOWED })
                .delete(|| async { StatusCode::ACCEPTED }),
        )
        .with_state(log.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok((format!("http://{address}/mcp"), log))
}

async fn put_generic_secret(client: &Client, base: &str, slot_id: &str, value: &str) -> Result<()> {
    client
        .post(format!("{base}/v1/runtime/secrets"))
        .json(&json!({
            "slot_id": slot_id,
            "provider": "generic",
            "mode": "opaque_secret",
            "state": {"value": value},
            "updated_at_ms": 1_794_004_700_000_u64,
        }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

fn scan_tool_use(call_id: &str, input: Value) -> MockReply {
    MockReply::ToolUse {
        id: call_id.to_string(),
        name: "mcp__fregate__scan_target".to_string(),
        input,
    }
}

async fn submit_run_and_complete(
    client: &Client,
    base: &str,
    session_id: &str,
    content: &str,
) -> Result<RunView> {
    let run = submit_test_input(client, base, session_id, content).await?;
    wait_for_run_status(client, base, &run.run_id, &[DaemonRunStatus::Completed]).await
}

#[tokio::test]
async fn mcp_tool_calls_carry_the_execution_context_and_survive_secret_rotation() -> Result<()> {
    let temp = tempdir()?;
    let state_root = temp.path().join("daemon-state");
    let workspace_root = temp.path().join("workspace");
    let mock = spawn_anthropic_messages_mock("scan finished").await?;
    let (mcp_url, mcp_log) = spawn_mcp_stub().await?;
    let (address, shutdown) = anthropic_provider_daemon(
        &state_root,
        &workspace_root,
        &format!("{}/v1/messages", mock.base),
    )
    .await?;
    let client = Client::new();
    let base = format!("http://{address}");

    put_generic_secret(&client, &base, "mcp.fregate.token", "mcp-token-A").await?;
    let server_body = json!({
        "name": "fregate",
        "url": mcp_url,
        "bearer_token_secret_ref": "mcp.fregate.token",
    });
    client
        .post(format!("{base}/v1/runtime/mcp/servers"))
        .json(&server_body)
        .send()
        .await?
        .error_for_status()?;
    create_test_session(&client, &base, "mission-m1").await?;
    client
        .put(format!("{base}/v1/sessions/mission-m1/permission-mode"))
        .json(&json!({"mode": "bypassPermissions"}))
        .send()
        .await?
        .error_for_status()?;

    // The model first tries to forge the execution context through the
    // arguments: the call is refused before it reaches the server. Its retry
    // without `_meta` goes through, with the context the daemon asserts.
    mock.push(scan_tool_use(
        "toolu_forged",
        json!({
            "target": "example.com",
            "_meta": {"kheish/session_id": "mission-forged", "kheish/run_id": "run-forged"}
        }),
    ));
    mock.push(scan_tool_use(
        "toolu_scan_1",
        json!({"target": "example.com"}),
    ));
    let run = submit_run_and_complete(&client, &base, "mission-m1", "scan example.com").await?;
    {
        let calls = mcp_log.calls.lock();
        assert_eq!(
            calls.len(),
            1,
            "exactly the clean call reaches the MCP server; last model request: {:#}",
            mock.last_request()
                .map(|request| request.body)
                .unwrap_or_default()
        );
        let params = &calls[0];
        assert_eq!(params["name"], "scan_target");
        assert_eq!(params["_meta"]["kheish/session_id"], "mission-m1");
        assert_eq!(params["_meta"]["kheish/run_id"], json!(run.run_id));
        assert_eq!(params["_meta"]["kheish/tool_call_id"], "toolu_scan_1");
        assert_eq!(params["arguments"], json!({"target": "example.com"}));
    }
    let forged_result = mock
        .captured
        .lock()
        .iter()
        .map(|request| request.body.to_string())
        .find(|body| body.contains("toolu_forged") && body.contains("tool_result"))
        .unwrap_or_default();
    assert!(
        forged_result.contains("unexpected tool input field _meta"),
        "the forged call is refused to the model: {forged_result}"
    );
    assert_eq!(
        mcp_log
            .call_authorizations
            .lock()
            .last()
            .map(String::as_str),
        Some("Bearer mcp-token-A")
    );

    // Rotating the credential disconnects the server.
    put_generic_secret(&client, &base, "mcp.fregate.token", "mcp-token-B").await?;
    let runtime = client
        .get(format!("{base}/v1/runtime"))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    let server = runtime["mcp"]["servers"]
        .as_array()
        .and_then(|servers| servers.iter().find(|server| server["server"] == "fregate"))
        .cloned()
        .expect("fregate server listed");
    assert_eq!(server["connected"], false, "{server:#}");

    // Deleting and re-adding the server reconnects it with the new secret;
    // before the fix the re-add collided with the stale tool registration.
    client
        .delete(format!("{base}/v1/runtime/mcp/servers/fregate"))
        .send()
        .await?
        .error_for_status()?;
    let readded = client
        .post(format!("{base}/v1/runtime/mcp/servers"))
        .json(&server_body)
        .send()
        .await?;
    assert_eq!(
        readded.status(),
        StatusCode::OK,
        "{}",
        readded.text().await?
    );
    mock.push(scan_tool_use(
        "toolu_scan_2",
        json!({"target": "example.org"}),
    ));
    let second = submit_run_and_complete(&client, &base, "mission-m1", "scan again").await?;
    {
        let calls = mcp_log.calls.lock();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[1]["_meta"]["kheish/run_id"], json!(second.run_id));
        assert_eq!(calls[1]["_meta"]["kheish/tool_call_id"], "toolu_scan_2");
    }
    assert_eq!(
        mcp_log
            .call_authorizations
            .lock()
            .last()
            .map(String::as_str),
        Some("Bearer mcp-token-B")
    );

    let _ = shutdown.send(());
    wait_for_daemon_shutdown(&client, &base).await?;
    Ok(())
}

/// One request received by the HTTP hook stub.
#[derive(Clone, Debug)]
struct CapturedHookRequest {
    headers: BTreeMap<String, String>,
    body: Value,
}

/// A permission hook endpoint: it records every request and answers with the
/// configured status and decision.
#[derive(Clone)]
struct HookStub {
    url: String,
    requests: Arc<Mutex<Vec<CapturedHookRequest>>>,
    status: Arc<Mutex<u16>>,
}

async fn spawn_hook_stub() -> Result<HookStub> {
    #[derive(Clone)]
    struct StubState {
        requests: Arc<Mutex<Vec<CapturedHookRequest>>>,
        status: Arc<Mutex<u16>>,
    }

    async fn permission(
        State(state): State<StubState>,
        headers: axum::http::HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        state.requests.lock().push(CapturedHookRequest {
            headers: headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_ascii_lowercase(),
                        value.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect(),
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        });
        let status = *state.status.lock();
        if status != 200 {
            return StatusCode::from_u16(status)
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
        Json(json!({"contract_version": 1, "permission": "allow"})).into_response()
    }

    let requests = Arc::new(Mutex::new(Vec::new()));
    let status = Arc::new(Mutex::new(200_u16));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let router = Router::new()
        .route("/hooks/permission", post(permission))
        .with_state(StubState {
            requests: requests.clone(),
            status: status.clone(),
        });
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(HookStub {
        url: format!("http://{address}/hooks/permission"),
        requests,
        status,
    })
}

fn permission_hook_settings(url: &str, allow_private: bool) -> Value {
    json!({"settings": {"hooks": {"permission_request": [{
        "name": "fregate-approval",
        "failure_policy": {"mode": "closed"},
        "executor": {
            "type": "http",
            "url": url,
            "timeout_ms": 5000,
            "allow_private": allow_private,
            "header_secret_refs": {"x-fregate-callback": "fregate.callback"}
        }
    }]}}})
}

#[tokio::test]
async fn http_hooks_reach_private_targets_with_secret_headers_and_run_context() -> Result<()> {
    let temp = tempdir()?;
    let state_root = temp.path().join("daemon-state");
    let workspace_root = temp.path().join("workspace");
    let mock = spawn_anthropic_messages_mock("scan finished").await?;
    let (mcp_url, mcp_log) = spawn_mcp_stub().await?;
    let hook = spawn_hook_stub().await?;
    let (address, shutdown) = anthropic_provider_daemon(
        &state_root,
        &workspace_root,
        &format!("{}/v1/messages", mock.base),
    )
    .await?;
    let client = Client::new();
    let base = format!("http://{address}");
    put_generic_secret(&client, &base, "fregate.callback", "callback-secret-1").await?;
    client
        .post(format!("{base}/v1/runtime/mcp/servers"))
        .json(&json!({"name": "fregate", "url": mcp_url}))
        .send()
        .await?
        .error_for_status()?;

    // A loopback target is refused unless the hook opts in.
    let refused = client
        .post(format!("{base}/v1/runtime/hooks"))
        .json(&permission_hook_settings(&hook.url, false))
        .send()
        .await?;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<ProblemDetails>().await?.code,
        "invalid_hook_settings"
    );
    // A reserved header can never come from a secret.
    let mut reserved = permission_hook_settings(&hook.url, true);
    reserved["settings"]["hooks"]["permission_request"][0]["executor"]["header_secret_refs"] =
        json!({"Idempotency-Key": "fregate.callback"});
    let reserved = client
        .post(format!("{base}/v1/runtime/hooks"))
        .json(&reserved)
        .send()
        .await?;
    assert_eq!(reserved.status(), StatusCode::BAD_REQUEST);

    client
        .post(format!("{base}/v1/runtime/hooks"))
        .json(&permission_hook_settings(&hook.url, true))
        .send()
        .await?
        .error_for_status()?;
    // Settings hold the slot name, never its value.
    let stored = client
        .get(format!("{base}/v1/runtime/hooks"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    assert!(stored.contains("\"allow_private\":true"), "{stored}");
    assert!(stored.contains("fregate.callback"), "{stored}");
    assert!(!stored.contains("callback-secret"), "{stored}");

    // Default permission mode: every MCP call asks, so the hook decides.
    create_test_session(&client, &base, "mission-hook").await?;
    let metadata = json!({"mission_id": "m1", "correlation_id": "corr-1"});
    mock.push(scan_tool_use(
        "toolu_hook_1",
        json!({"target": "example.com"}),
    ));
    let run = client
        .post(format!("{base}/v1/sessions/mission-hook/runs"))
        .json(&json!({"content": "scan", "metadata": metadata}))
        .send()
        .await?
        .error_for_status()?
        .json::<RunView>()
        .await?;
    wait_for_run_status(&client, &base, &run.run_id, &[DaemonRunStatus::Completed]).await?;
    assert_eq!(mcp_log.calls.lock().len(), 1, "the hook allowed the call");
    {
        let requests = hook.requests.lock();
        let request = requests.last().expect("the hook was called");
        assert_eq!(
            request
                .headers
                .get("x-fregate-callback")
                .map(String::as_str),
            Some("callback-secret-1")
        );
        assert!(request.headers.contains_key("idempotency-key"));
        assert_eq!(request.body["event"], "permission_request");
        assert_eq!(request.body["session_id"], "mission-hook");
        assert_eq!(request.body["run_id"], json!(run.run_id));
        assert_eq!(request.body["run_metadata"], metadata);
        assert_eq!(request.body["subject"], "mcp__fregate__scan_target");
    }

    // The header is read at call time: a rotated secret is sent next time.
    put_generic_secret(&client, &base, "fregate.callback", "callback-secret-2").await?;
    mock.push(scan_tool_use(
        "toolu_hook_2",
        json!({"target": "example.org"}),
    ));
    let second = submit_test_input(&client, &base, "mission-hook", "scan again").await?;
    wait_for_run_status(
        &client,
        &base,
        &second.run_id,
        &[DaemonRunStatus::Completed],
    )
    .await?;
    assert_eq!(
        hook.requests
            .lock()
            .last()
            .and_then(|request| request.headers.get("x-fregate-callback").cloned())
            .as_deref(),
        Some("callback-secret-2")
    );
    assert_eq!(mcp_log.calls.lock().len(), 2);

    // Fail-closed is unchanged: a failing hook denies the call.
    *hook.status.lock() = 500;
    mock.push(scan_tool_use(
        "toolu_hook_3",
        json!({"target": "example.net"}),
    ));
    let denied = submit_test_input(&client, &base, "mission-hook", "scan once more").await?;
    wait_for_run_status(
        &client,
        &base,
        &denied.run_id,
        &[DaemonRunStatus::Completed],
    )
    .await?;
    assert_eq!(mcp_log.calls.lock().len(), 2, "a failing hook must deny");
    let dead_letters = client
        .get(format!("{base}/v1/runtime/hooks/dead-letter"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    assert!(dead_letters.contains("fregate-approval"), "{dead_letters}");
    assert!(!dead_letters.contains("callback-secret"), "{dead_letters}");

    let _ = shutdown.send(());
    wait_for_daemon_shutdown(&client, &base).await?;
    Ok(())
}

/// Reads a run's SSE stream from the start and returns every `run_updated`
/// view until the run is terminal.
async fn collect_run_updates(client: &Client, base: &str, run_id: &str) -> Result<Vec<RunView>> {
    let mut response = client
        .get(format!("{base}/v1/runs/{run_id}/stream?cursor=0"))
        .send()
        .await?
        .error_for_status()?;
    let mut buffer = String::new();
    let mut updates = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        while let Some(frame_end) = buffer.find("\n\n") {
            let frame = buffer[..frame_end].to_string();
            buffer = buffer[frame_end + 2..].to_string();
            let mut name = None;
            let mut data = String::new();
            for line in frame.lines() {
                if let Some(value) = line.strip_prefix("event: ") {
                    name = Some(value.trim().to_string());
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data.push_str(value);
                }
            }
            if name.as_deref() == Some("run_updated")
                && let DaemonEvent::RunUpdated { run } = serde_json::from_str::<DaemonEvent>(&data)?
            {
                let terminal = run.status.is_terminal();
                updates.push(run);
                if terminal {
                    return Ok(updates);
                }
            }
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "run {run_id} did not settle on its stream: {updates:#?}"
        );
        let chunk = tokio::time::timeout(Duration::from_secs(5), response.chunk())
            .await
            .context("timed out waiting for a run stream chunk")??
            .ok_or_else(|| anyhow!("run stream ended before the run settled"))?;
        buffer.push_str(std::str::from_utf8(&chunk)?);
    }
}

async fn submit_limited_run(
    client: &Client,
    base: &str,
    session_id: &str,
    body: Value,
) -> Result<reqwest::Response> {
    Ok(client
        .post(format!("{base}/v1/sessions/{session_id}/runs"))
        .json(&body)
        .send()
        .await?)
}

#[tokio::test]
async fn run_limits_cap_turns_and_wall_clock_with_distinct_error_codes() -> Result<()> {
    let temp = tempdir()?;
    let state_root = temp.path().join("daemon-state");
    let workspace_root = temp.path().join("workspace");
    let mock = spawn_anthropic_messages_mock("done").await?;
    let (mcp_url, mcp_log) = spawn_mcp_stub().await?;
    let (address, shutdown) = anthropic_provider_daemon(
        &state_root,
        &workspace_root,
        &format!("{}/v1/messages", mock.base),
    )
    .await?;
    let client = Client::new();
    let base = format!("http://{address}");
    client
        .post(format!("{base}/v1/runtime/mcp/servers"))
        .json(&json!({"name": "fregate", "url": mcp_url}))
        .send()
        .await?
        .error_for_status()?;
    create_test_session(&client, &base, "limits-turns").await?;
    client
        .put(format!("{base}/v1/sessions/limits-turns/permission-mode"))
        .json(&json!({"mode": "bypassPermissions"}))
        .send()
        .await?
        .error_for_status()?;

    // Invalid limits are refused.
    for body in [
        json!({"content": "x", "max_turns": 0}),
        json!({"content": "x", "max_duration_ms": 0}),
    ] {
        let refused = submit_limited_run(&client, &base, "limits-turns", body).await?;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            refused.json::<ProblemDetails>().await?.code,
            "run_limits_invalid"
        );
    }

    // max_turns: the model keeps calling tools; the run stops after 2 turns.
    for index in 0..3 {
        mock.push(scan_tool_use(
            &format!("toolu_turn_{index}"),
            json!({"target": format!("host-{index}.example")}),
        ));
    }
    let run = submit_limited_run(
        &client,
        &base,
        "limits-turns",
        json!({"content": "keep scanning", "max_turns": 2, "idempotency_key": "turns-1"}),
    )
    .await?
    .error_for_status()?
    .json::<RunView>()
    .await?;
    assert_eq!(
        run.limits.as_ref().and_then(|limits| limits.max_turns),
        Some(2)
    );
    let updates = collect_run_updates(&client, &base, &run.run_id).await?;
    let settled = updates.last().cloned().expect("terminal update");
    assert_eq!(settled.status, DaemonRunStatus::Failed);
    assert_eq!(settled.error_code, Some(RunErrorCode::MaxTurnsExceeded));
    assert_eq!(
        settled.error.as_deref(),
        Some("max_turns_exceeded: agent loop exceeded max_turns=2")
    );
    assert!(
        updates
            .iter()
            .all(|update| update.limits.as_ref().and_then(|limits| limits.max_turns) == Some(2)),
        "every run_updated carries the limits: {updates:#?}"
    );
    assert_eq!(mcp_log.calls.lock().len(), 2, "exactly two turns ran tools");
    let fetched = client
        .get(format!("{base}/v1/runs/{}", run.run_id))
        .send()
        .await?
        .error_for_status()?
        .json::<RunView>()
        .await?;
    assert_eq!(fetched.error_code, Some(RunErrorCode::MaxTurnsExceeded));
    assert_eq!(
        terminal_run_event(&client, &base, &run.run_id).await?,
        RunEvent::Failed {
            error: "max_turns_exceeded: agent loop exceeded max_turns=2".to_string(),
            code: Some(RunErrorCode::MaxTurnsExceeded),
            refusal: None,
        },
        "the persisted failed event carries the code"
    );
    assert_eq!(
        fetched.limits.as_ref().and_then(|limits| limits.max_turns),
        Some(2)
    );
    // The same idempotency key with different limits is a different request.
    let conflict = submit_limited_run(
        &client,
        &base,
        "limits-turns",
        json!({"content": "keep scanning", "max_turns": 3, "idempotency_key": "turns-1"}),
    )
    .await?;
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    // Drop the tool call the capped run never consumed.
    mock.script.lock().clear();

    // max_duration_ms while the run executes: a slow model turn is cut off.
    mock.push(MockReply::Stall {
        delay_ms: 5_000,
        text: "too late".to_string(),
    });
    let started = Instant::now();
    let slow = submit_limited_run(
        &client,
        &base,
        "limits-turns",
        json!({"content": "think slowly", "max_duration_ms": 400}),
    )
    .await?
    .error_for_status()?
    .json::<RunView>()
    .await?;
    let updates = collect_run_updates(&client, &base, &slow.run_id).await?;
    let settled = updates.last().cloned().expect("terminal update");
    assert_eq!(settled.status, DaemonRunStatus::Cancelled);
    assert_eq!(settled.error_code, Some(RunErrorCode::DeadlineExceeded));
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the deadline cut the run short"
    );
    assert_eq!(
        settled.error.as_deref(),
        Some("deadline_exceeded: run exceeded max_duration_ms=400")
    );
    assert_eq!(
        terminal_run_event(&client, &base, &slow.run_id).await?,
        RunEvent::Cancelled {
            code: Some(RunErrorCode::DeadlineExceeded),
        },
        "the persisted cancelled event carries the code"
    );
    let limits = settled.limits.clone().expect("limits are kept");
    assert_eq!(limits.max_duration_ms, Some(400));
    let deadline_at_ms = limits.deadline_at_ms.expect("deadline fixed on start");
    assert_eq!(
        deadline_at_ms,
        settled.started_at_ms.expect("run started") + 400
    );
    assert_eq!(limits.remaining_ms, None, "the clock ran to the end");

    let _ = shutdown.send(());
    wait_for_daemon_shutdown(&client, &base).await?;
    Ok(())
}

/// Returns the last persisted event of a settled run.
async fn terminal_run_event(client: &Client, base: &str, run_id: &str) -> Result<RunEvent> {
    let events = client
        .get(format!("{base}/v1/runs/{run_id}/events"))
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<RunEventEntry>>()
        .await?;
    Ok(events
        .last()
        .expect("a settled run has events")
        .event
        .clone())
}

/// The `max_duration_ms` the deadline tests give their runs. Every approval
/// they hold, and every restart they wait through, lasts longer.
const DEADLINE_TEST_BUDGET_MS: u64 = 2_000;

/// A provider daemon rig that boots again on the same state root: a model
/// mock, and an MCP stub whose `scan_target` calls ask for approval in the
/// sessions' `default` permission mode.
struct DeadlineRig {
    _temp: tempfile::TempDir,
    state_root: PathBuf,
    workspace_root: PathBuf,
    mock: AnthropicMock,
    mcp_url: String,
    mcp_log: McpStubLog,
    client: Client,
}

impl DeadlineRig {
    async fn new() -> Result<Self> {
        let temp = tempdir()?;
        let state_root = temp.path().join("daemon-state");
        let workspace_root = temp.path().join("workspace");
        let mock = spawn_anthropic_messages_mock("done").await?;
        let (mcp_url, mcp_log) = spawn_mcp_stub().await?;
        Ok(Self {
            _temp: temp,
            state_root,
            workspace_root,
            mock,
            mcp_url,
            mcp_log,
            client: Client::new(),
        })
    }

    /// Boots a daemon on the rig's state root and returns its base URL.
    async fn boot(&self) -> Result<(String, oneshot::Sender<()>)> {
        let (address, shutdown) = anthropic_provider_daemon(
            &self.state_root,
            &self.workspace_root,
            &format!("{}/v1/messages", self.mock.base),
        )
        .await?;
        Ok((format!("http://{address}"), shutdown))
    }

    /// Adds the MCP stub; the daemon reloads it from its overlay on restart.
    async fn add_scan_server(&self, base: &str) -> Result<()> {
        self.client
            .post(format!("{base}/v1/runtime/mcp/servers"))
            .json(&json!({"name": "fregate", "url": self.mcp_url}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn stop(&self, base: &str, shutdown: oneshot::Sender<()>) -> Result<()> {
        let _ = shutdown.send(());
        wait_for_daemon_shutdown(&self.client, base).await
    }

    /// Submits a run limited to [`DEADLINE_TEST_BUDGET_MS`].
    async fn submit(&self, base: &str, session_id: &str, content: &str) -> Result<RunView> {
        Ok(submit_limited_run(
            &self.client,
            base,
            session_id,
            json!({"content": content, "max_duration_ms": DEADLINE_TEST_BUDGET_MS}),
        )
        .await?
        .error_for_status()?
        .json::<RunView>()
        .await?)
    }

    async fn fetch(&self, base: &str, run_id: &str) -> Result<RunView> {
        Ok(self
            .client
            .get(format!("{base}/v1/runs/{run_id}"))
            .send()
            .await?
            .error_for_status()?
            .json::<RunView>()
            .await?)
    }

    /// Allows every approval the run waits for.
    async fn approve(&self, base: &str, run: &RunView) -> Result<()> {
        let resolutions = run
            .pending_approval_ids
            .iter()
            .map(|request_id| json!({"request_id": request_id, "behavior": "allow"}))
            .collect::<Vec<_>>();
        self.client
            .post(format!("{base}/v1/runs/{}/approvals", run.run_id))
            .json(&json!({"resolutions": resolutions}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Waits until the model mock has received `count` requests.
    async fn wait_for_model_requests(&self, count: usize) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while self.mock.captured.lock().len() < count {
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "the model mock did not receive {count} requests"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }
}

/// The deadline clock of a run: `(deadline_at_ms, remaining_ms)`.
fn deadline_clock(run: &RunView) -> (Option<u64>, Option<u64>) {
    run.limits.as_ref().map_or((None, None), |limits| {
        (limits.deadline_at_ms, limits.remaining_ms)
    })
}

#[tokio::test]
async fn run_deadline_clock_stops_while_the_run_waits_for_an_approval() -> Result<()> {
    let rig = DeadlineRig::new().await?;
    let (base, shutdown) = rig.boot().await?;
    rig.add_scan_server(&base).await?;
    create_test_session(&rig.client, &base, "deadline-held").await?;

    // The approval is held longer than the whole budget; the run still
    // completes once approved.
    rig.mock.push(scan_tool_use(
        "toolu_held",
        json!({"target": "example.com"}),
    ));
    let run = rig.submit(&base, "deadline-held", "scan").await?;
    let waiting = wait_for_pending_approval_run(&rig.client, &base, &run.run_id).await?;
    let (deadline_at_ms, remaining_ms) = deadline_clock(&waiting);
    assert_eq!(deadline_at_ms, None, "no deadline while the run waits");
    let remaining_ms = remaining_ms.expect("the budget left is kept");
    assert!(
        remaining_ms > 0 && remaining_ms <= DEADLINE_TEST_BUDGET_MS,
        "{remaining_ms}"
    );
    tokio::time::sleep(Duration::from_millis(DEADLINE_TEST_BUDGET_MS + 500)).await;
    let held = rig.fetch(&base, &run.run_id).await?;
    assert_eq!(held.status, DaemonRunStatus::WaitingForApproval);
    assert_eq!(
        held.limits, waiting.limits,
        "nothing counts down while waiting"
    );

    let approved_at_ms = now_ms();
    rig.approve(&base, &held).await?;
    let completed = wait_for_run_status(
        &rig.client,
        &base,
        &run.run_id,
        &[DaemonRunStatus::Completed],
    )
    .await?;
    assert_eq!(completed.error_code, None);
    assert_eq!(rig.mcp_log.calls.lock().len(), 1, "the approved tool ran");
    let started_at_ms = completed.started_at_ms.expect("run started");
    let finished_at_ms = completed.finished_at_ms.expect("run finished");
    assert!(
        finished_at_ms - started_at_ms > DEADLINE_TEST_BUDGET_MS,
        "the run outlived its budget in wall-clock time"
    );
    let (deadline_at_ms, remaining_ms_after) = deadline_clock(&completed);
    assert_eq!(
        remaining_ms_after, None,
        "the clock ran again after the approval"
    );
    assert!(deadline_at_ms.expect("the clock ran again") >= approved_at_ms + remaining_ms);

    // The run stream shows the clock: running to a deadline, stopped with the
    // budget left, then running again from that budget.
    let updates = collect_run_updates(&rig.client, &base, &run.run_id).await?;
    let paused_index = updates
        .iter()
        .position(|update| update.status == DaemonRunStatus::WaitingForApproval)
        .expect("a waiting update");
    let first_deadline = updates[..paused_index]
        .iter()
        .find_map(|update| deadline_clock(update).0)
        .expect("a running update carries the first deadline");
    assert_eq!(first_deadline, started_at_ms + DEADLINE_TEST_BUDGET_MS);
    let paused = &updates[paused_index];
    assert_eq!(
        deadline_clock(paused),
        (None, Some(first_deadline - paused.updated_at_ms)),
        "the wait stops the clock at the budget left"
    );
    let resumed = updates[paused_index..]
        .iter()
        .find(|update| update.status == DaemonRunStatus::Running)
        .expect("a resumed update");
    assert_eq!(
        deadline_clock(resumed),
        (Some(resumed.updated_at_ms + remaining_ms), None),
        "the resume restarts the clock with the budget left"
    );

    rig.stop(&base, shutdown).await
}

#[tokio::test]
async fn run_deadline_runs_out_while_running_after_a_resume() -> Result<()> {
    let rig = DeadlineRig::new().await?;
    let (base, shutdown) = rig.boot().await?;
    rig.add_scan_server(&base).await?;
    create_test_session(&rig.client, &base, "deadline-resumed").await?;

    // The turn after the approved tool call stalls past the budget left.
    rig.mock.push(scan_tool_use(
        "toolu_resumed",
        json!({"target": "example.com"}),
    ));
    rig.mock.push(MockReply::Stall {
        delay_ms: 30_000,
        text: "too late".to_string(),
    });
    let run = rig.submit(&base, "deadline-resumed", "scan").await?;
    let waiting = wait_for_pending_approval_run(&rig.client, &base, &run.run_id).await?;
    let remaining_ms = deadline_clock(&waiting).1.expect("the budget left is kept");
    tokio::time::sleep(Duration::from_millis(DEADLINE_TEST_BUDGET_MS + 500)).await;
    let held = rig.fetch(&base, &run.run_id).await?;
    assert_eq!(held.status, DaemonRunStatus::WaitingForApproval);

    let approved_at_ms = now_ms();
    rig.approve(&base, &held).await?;
    let cancelled = wait_for_run_status(
        &rig.client,
        &base,
        &run.run_id,
        &[DaemonRunStatus::Cancelled],
    )
    .await?;
    assert_eq!(cancelled.error_code, Some(RunErrorCode::DeadlineExceeded));
    assert_eq!(
        cancelled.error,
        Some(format!(
            "deadline_exceeded: run exceeded max_duration_ms={DEADLINE_TEST_BUDGET_MS}"
        ))
    );
    assert_eq!(
        terminal_run_event(&rig.client, &base, &run.run_id).await?,
        RunEvent::Cancelled {
            code: Some(RunErrorCode::DeadlineExceeded),
        }
    );
    assert_eq!(rig.mcp_log.calls.lock().len(), 1, "the approved tool ran");
    let (deadline_at_ms, remaining_after) = deadline_clock(&cancelled);
    assert_eq!(remaining_after, None, "the clock was running");
    let deadline_at_ms = deadline_at_ms.expect("the resume restarted the clock");
    assert!(
        deadline_at_ms >= approved_at_ms + remaining_ms,
        "the clock resumed with the budget left, not from the first start"
    );
    let finished_at_ms = cancelled.finished_at_ms.expect("run finished");
    assert!(finished_at_ms >= deadline_at_ms);
    assert!(
        finished_at_ms < deadline_at_ms + 3_000,
        "the deadline cut the stalled turn short"
    );

    rig.stop(&base, shutdown).await
}

#[tokio::test]
async fn run_deadline_stays_paused_across_a_restart() -> Result<()> {
    let rig = DeadlineRig::new().await?;
    let (base, shutdown) = rig.boot().await?;
    rig.add_scan_server(&base).await?;
    create_test_session(&rig.client, &base, "deadline-restart-paused").await?;
    rig.mock.push(scan_tool_use(
        "toolu_restart_paused",
        json!({"target": "example.com"}),
    ));
    let run = rig.submit(&base, "deadline-restart-paused", "scan").await?;
    let waiting = wait_for_pending_approval_run(&rig.client, &base, &run.run_id).await?;
    let remaining_ms = deadline_clock(&waiting).1.expect("the budget left is kept");

    // The daemon stays down longer than the whole budget.
    rig.stop(&base, shutdown).await?;
    tokio::time::sleep(Duration::from_millis(DEADLINE_TEST_BUDGET_MS + 500)).await;
    let (base, shutdown) = rig.boot().await?;
    let restored = rig.fetch(&base, &run.run_id).await?;
    assert_eq!(restored.status, DaemonRunStatus::WaitingForApproval);
    assert_eq!(
        restored.limits, waiting.limits,
        "the restart keeps the run paused with the same budget"
    );

    // No timer runs for the paused run after boot either.
    tokio::time::sleep(Duration::from_millis(DEADLINE_TEST_BUDGET_MS + 500)).await;
    let held = rig.fetch(&base, &run.run_id).await?;
    assert_eq!(held.status, DaemonRunStatus::WaitingForApproval);
    assert_eq!(held.limits, waiting.limits);

    let approved_at_ms = now_ms();
    rig.approve(&base, &held).await?;
    let completed = wait_for_run_status(
        &rig.client,
        &base,
        &run.run_id,
        &[DaemonRunStatus::Completed],
    )
    .await?;
    assert_eq!(completed.error_code, None);
    assert_eq!(rig.mcp_log.calls.lock().len(), 1, "the approved tool ran");
    let (deadline_at_ms, remaining_after) = deadline_clock(&completed);
    assert_eq!(remaining_after, None);
    assert!(deadline_at_ms.expect("the clock ran again") >= approved_at_ms + remaining_ms);

    rig.stop(&base, shutdown).await
}

#[tokio::test]
async fn run_deadline_rearms_a_running_run_after_a_restart() -> Result<()> {
    let rig = DeadlineRig::new().await?;
    let (base, shutdown) = rig.boot().await?;

    // A direct run executing when the daemon stops.
    create_test_session(&rig.client, &base, "deadline-direct").await?;
    rig.mock.push(MockReply::Stall {
        delay_ms: 30_000,
        text: "never".to_string(),
    });
    let direct = submit_limited_run(
        &rig.client,
        &base,
        "deadline-direct",
        json!({"content": "think", "max_duration_ms": 60_000}),
    )
    .await?
    .error_for_status()?
    .json::<RunView>()
    .await?;
    rig.wait_for_model_requests(1).await?;
    let direct_deadline_at_ms = deadline_clock(&rig.fetch(&base, &direct.run_id).await?)
        .0
        .expect("the clock runs");

    // A scheduled run executing when the daemon stops; the scheduler replays
    // it after the restart.
    let replay_session = create_test_session(&rig.client, &base, "deadline-replay").await?;
    rig.mock.push(MockReply::Stall {
        delay_ms: 30_000,
        text: "never".to_string(),
    });
    create_test_schedule(
        &rig.client,
        &base,
        CreateScheduleRequest {
            name: "deadline-replay".to_string(),
            target_session_id: "deadline-replay".to_string(),
            target_agent_id: None,
            owner_session_id: Some("deadline-replay".to_string()),
            owner_agent_id: Some(replay_session.agent_id.clone()),
            created_by_run_id: None,
            cadence: ScheduleCadence::Once {
                fire_at_ms: now_ms() + 100,
            },
            max_executions: None,
            overlap_policy: ScheduleOverlapPolicy::Skip,
            misfire_policy: ScheduleMisfirePolicy::CoalesceOnce,
            request: Some(serde_json::from_value(json!({"content": "replay me"}))?),
            observation_materialization: None,
            flow_start: None,
        },
    )
    .await?;
    let replay_id = wait_for_session_run_count(&rig.client, &base, "deadline-replay", 1).await?[0]
        .run_id
        .clone();
    wait_for_run_status(&rig.client, &base, &replay_id, &[DaemonRunStatus::Running]).await?;
    rig.wait_for_model_requests(2).await?;
    rig.stop(&base, shutdown).await?;

    // Give the replayed run a running clock with 2 s left of a 60 s budget,
    // as a limited run that survives a restart would have.
    let run_store = FileRunStore::new(&rig.state_root);
    let mut record = run_store
        .load_run(&replay_id)?
        .expect("the scheduled run is persisted");
    let replay_deadline_at_ms = now_ms() + 2_000;
    record.view.limits = Some(crate::RunLimits {
        max_duration_ms: Some(60_000),
        deadline_at_ms: Some(replay_deadline_at_ms),
        ..crate::RunLimits::default()
    });
    run_store.save_run(&record)?;
    rig.mock.push(MockReply::Stall {
        delay_ms: 30_000,
        text: "never".to_string(),
    });
    let (base, shutdown) = rig.boot().await?;

    // The direct run is interrupted by the restart, never resumed, so its
    // deadline no longer applies.
    let interrupted = wait_for_run_status(
        &rig.client,
        &base,
        &direct.run_id,
        &[DaemonRunStatus::Interrupted],
    )
    .await?;
    assert_eq!(interrupted.error_code, None);
    assert_eq!(
        deadline_clock(&interrupted),
        (Some(direct_deadline_at_ms), None)
    );

    // The replayed run runs again with the time left to its deadline, not a
    // fresh budget, and the deadline ends it.
    let cancelled = wait_for_run_status(
        &rig.client,
        &base,
        &replay_id,
        &[DaemonRunStatus::Cancelled],
    )
    .await?;
    assert_eq!(cancelled.error_code, Some(RunErrorCode::DeadlineExceeded));
    assert_eq!(
        deadline_clock(&cancelled),
        (Some(replay_deadline_at_ms), None)
    );
    let finished_at_ms = cancelled.finished_at_ms.expect("run finished");
    assert!(finished_at_ms >= replay_deadline_at_ms);
    assert!(
        finished_at_ms < replay_deadline_at_ms + 3_000,
        "re-armed with the time left, not the whole budget"
    );

    rig.stop(&base, shutdown).await
}

async fn publish_fact(
    client: &Client,
    base: &str,
    kind: kheish_types::LearningScopeKind,
    scope_id: &str,
    content: &str,
) -> Result<LearningView> {
    let candidate = client
        .post(format!("{base}/v1/learning-candidates"))
        .json(&CreateLearningCandidateRequest {
            scope: kheish_types::LearningScope {
                kind,
                id: scope_id.to_string(),
            },
            kind: kheish_types::LearningKind::Fact,
            sensitivity: kheish_types::LearningSensitivity::Scoped,
            content: content.to_string(),
            confidence: 95,
            source: kheish_types::LearningSourceRef::default(),
            evidence_refs: Vec::new(),
            expires_at_ms: None,
        })
        .send()
        .await?
        .error_for_status()?
        .json::<LearningCandidateView>()
        .await?;
    client
        .post(format!(
            "{base}/v1/learning-candidates/{}/publish",
            candidate.candidate_id
        ))
        .json(&PublishLearningCandidateRequest::default())
        .send()
        .await?
        .error_for_status()?
        .json::<LearningView>()
        .await
        .map_err(Into::into)
}

#[tokio::test]
async fn learning_read_scopes_keep_runs_to_session_and_project_learnings() -> Result<()> {
    use kheish_types::LearningScopeKind;

    let temp = tempdir()?;
    let state_root = temp.path().join("daemon-state");
    let workspace_root = temp.path().join("workspace");
    let mock = spawn_anthropic_messages_mock("noted").await?;
    let (address, shutdown) = anthropic_provider_daemon_with_config(
        &state_root,
        &workspace_root,
        &format!("{}/v1/messages", mock.base),
        |config| {
            config.learning_read_scopes = "session,project".parse().expect("valid scopes");
        },
    )
    .await?;
    let client = Client::new();
    let base = format!("http://{address}");
    create_test_persona(
        &client,
        &base,
        "fp-ws1-verify",
        "Verifier",
        "You verify exposures.",
    )
    .await?;
    create_test_session_with_persona(&client, &base, "mission-learn", Some("fp-ws1-verify"))
        .await?;
    client
        .post(format!("{base}/v1/projects"))
        .json(&json!({"project_id": "fe-1", "display_name": "Engagement 1"}))
        .send()
        .await?
        .error_for_status()?;
    client
        .post(format!("{base}/v1/projects/fe-1/members"))
        .json(
            &json!({"member_id": "mission-learn", "member_kind": "session",
                      "display_name": "Mission", "session_id": "mission-learn"}),
        )
        .send()
        .await?
        .error_for_status()?;
    let markers = [
        (
            LearningScopeKind::Session,
            "mission-learn",
            "SESSION-MARKER-7Q",
        ),
        (LearningScopeKind::Project, "fe-1", "PROJECT-MARKER-3K"),
        (
            LearningScopeKind::Persona,
            "fp-ws1-verify",
            "PERSONA-MARKER-9X",
        ),
        (
            LearningScopeKind::Workspace,
            kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID,
            "WORKSPACE-MARKER-2B",
        ),
    ];
    for (kind, scope_id, marker) in markers {
        publish_fact(
            &client,
            &base,
            kind,
            scope_id,
            &format!("Exposure triage note {marker}."),
        )
        .await?;
    }

    let context = client
        .get(format!("{base}/v1/sessions/mission-learn/memory-context"))
        .send()
        .await?
        .error_for_status()?
        .json::<SessionMemoryContextView>()
        .await?;
    assert_eq!(
        context
            .learning_scopes
            .iter()
            .map(|scope| scope.kind.clone())
            .collect::<Vec<_>>(),
        vec![LearningScopeKind::Session, LearningScopeKind::Project]
    );
    let projected = serde_json::to_string(&context.learned_context)?;
    assert!(projected.contains("SESSION-MARKER-7Q"), "{projected}");
    assert!(projected.contains("PROJECT-MARKER-3K"), "{projected}");
    assert!(!projected.contains("PERSONA-MARKER-9X"), "{projected}");
    assert!(!projected.contains("WORKSPACE-MARKER-2B"), "{projected}");

    // What reaches the model follows the same scopes.
    let run = submit_test_input(&client, &base, "mission-learn", "exposure triage").await?;
    wait_for_run_status(&client, &base, &run.run_id, &[DaemonRunStatus::Completed]).await?;
    let prompt = mock
        .last_request()
        .map(|request| request.body.to_string())
        .unwrap_or_default();
    assert!(prompt.contains("SESSION-MARKER-7Q"), "{prompt}");
    assert!(prompt.contains("PROJECT-MARKER-3K"), "{prompt}");
    assert!(!prompt.contains("PERSONA-MARKER-9X"), "{prompt}");
    assert!(!prompt.contains("WORKSPACE-MARKER-2B"), "{prompt}");

    let _ = shutdown.send(());
    wait_for_daemon_shutdown(&client, &base).await?;
    Ok(())
}

fn run_number(run_id: &str) -> u64 {
    run_id
        .strip_prefix("run-")
        .and_then(|raw| raw.parse().ok())
        .unwrap_or_default()
}

async fn delete_session(client: &Client, base: &str, query: &str) -> Result<reqwest::Response> {
    Ok(client
        .delete(format!("{base}/v1/sessions/{query}"))
        .send()
        .await?)
}

async fn end_session(client: &Client, base: &str, session_id: &str) -> Result<()> {
    client
        .post(format!("{base}/v1/sessions/{session_id}/end"))
        .json(&json!({"reason": "mission done"}))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

#[tokio::test]
async fn ended_sessions_purge_every_trace_and_survive_a_crash_midway() -> Result<()> {
    use kheish_types::LearningScopeKind;

    let temp = tempdir()?;
    let state_root = temp.path().join("daemon-state");
    let workspace_root = temp.path().join("workspace");
    let mock = spawn_anthropic_messages_mock("done").await?;
    let (mcp_url, _mcp_log) = spawn_mcp_stub().await?;
    let endpoint = format!("{}/v1/messages", mock.base);
    let (address, shutdown) =
        anthropic_provider_daemon(&state_root, &workspace_root, &endpoint).await?;
    let client = Client::new();
    let base = format!("http://{address}");
    client
        .post(format!("{base}/v1/runtime/mcp/servers"))
        .json(&json!({"name": "fregate", "url": mcp_url}))
        .send()
        .await?
        .error_for_status()?;

    // A neighbour session whose data must survive the purge.
    create_test_session(&client, &base, "mission-keep").await?;
    let kept_run = submit_run_and_complete(&client, &base, "mission-keep", "hello").await?;
    let kept_learning = publish_fact(
        &client,
        &base,
        LearningScopeKind::Session,
        "mission-keep",
        "Keep this note.",
    )
    .await?;

    create_test_session(&client, &base, "mission-purge").await?;
    let first = submit_run_and_complete(&client, &base, "mission-purge", "hello").await?;
    let purged_learning = publish_fact(
        &client,
        &base,
        LearningScopeKind::Session,
        "mission-purge",
        "Purge this note.",
    )
    .await?;

    // Only an ended session can be purged.
    let not_ended = delete_session(&client, &base, "mission-purge").await?;
    assert_eq!(not_ended.status(), StatusCode::CONFLICT);
    assert_eq!(
        not_ended.json::<ProblemDetails>().await?.code,
        "session_not_ended"
    );
    end_session(&client, &base, "mission-purge").await?;

    // A run still waiting for an approval blocks the purge.
    mock.push(scan_tool_use(
        "toolu_purge_wait",
        json!({"target": "example.com"}),
    ));
    let waiting = submit_test_input(&client, &base, "mission-purge", "scan").await?;
    wait_for_pending_approval_run(&client, &base, &waiting.run_id).await?;
    let busy = delete_session(&client, &base, "mission-purge").await?;
    assert_eq!(busy.status(), StatusCode::CONFLICT);
    assert_eq!(busy.json::<ProblemDetails>().await?.code, "session_busy");
    client
        .post(format!("{base}/v1/runs/{}/cancel", waiting.run_id))
        .send()
        .await?
        .error_for_status()?;

    let purged = delete_session(&client, &base, "mission-purge?reason=retention")
        .await?
        .error_for_status()?
        .json::<SessionPurgeView>()
        .await?;
    assert_eq!(purged.session_id, "mission-purge");
    let mut purged_runs = purged.run_ids.clone();
    purged_runs.sort();
    let mut expected_runs = vec![first.run_id.clone(), waiting.run_id.clone()];
    expected_runs.sort();
    assert_eq!(purged_runs, expected_runs);
    assert_eq!(
        purged.learning_ids,
        vec![purged_learning.learning_id.clone()]
    );

    // Every surface forgets the session.
    let missing = client
        .get(format!("{base}/v1/sessions/mission-purge"))
        .send()
        .await?;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let sessions = client
        .get(format!("{base}/v1/sessions"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    assert!(!sessions.contains("mission-purge"), "{sessions}");
    assert!(sessions.contains("mission-keep"), "{sessions}");
    for run_id in &purged.run_ids {
        let gone = client
            .get(format!("{base}/v1/runs/{run_id}"))
            .send()
            .await?;
        assert_eq!(gone.status(), StatusCode::NOT_FOUND, "{run_id}");
        assert!(!resolve_storage_path_for_read(&state_root.join("runs"), run_id, "json").exists());
        assert!(
            !resolve_storage_path_for_read(&state_root.join("run-events"), run_id, "jsonl")
                .exists()
        );
    }
    assert!(
        !resolve_storage_path_for_read(&state_root.join("sessions"), "mission-purge", "jsonl")
            .exists()
    );
    assert!(
        !resolve_storage_path_for_read(&state_root.join("outputs"), "mission-purge", "jsonl")
            .exists(),
        "the session's persisted outputs are deleted"
    );
    assert!(
        resolve_storage_path_for_read(&state_root.join("outputs"), "mission-keep", "jsonl")
            .exists()
    );
    assert!(
        !state_root
            .join("learnings")
            .join(format!("{}.json", purged_learning.learning_id))
            .exists()
    );
    // The neighbour is untouched.
    assert!(
        state_root
            .join("learnings")
            .join(format!("{}.json", kept_learning.learning_id))
            .exists()
    );
    client
        .get(format!("{base}/v1/runs/{}", kept_run.run_id))
        .send()
        .await?
        .error_for_status()?;
    // The purge is audited in the supervisor audit log.
    let audit = client
        .get(format!("{base}/v1/agents/audit"))
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<Value>>()
        .await?;
    assert!(
        audit.iter().any(|entry| entry["event"] == "session_purged"
            && entry["session_id"] == "mission-purge"
            && entry["reason"] == "retention"),
        "{audit:#?}"
    );
    assert_eq!(
        delete_session(&client, &base, "mission-purge")
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );

    // Crash safety: the daemon stops right after writing the plan of a
    // second purge; the next boot finishes it.
    end_session(&client, &base, "mission-keep").await?;
    let keep_view = client
        .get(format!("{base}/v1/sessions/mission-keep"))
        .send()
        .await?
        .error_for_status()?
        .json::<SessionView>()
        .await?;
    let _ = shutdown.send(());
    wait_for_daemon_shutdown(&client, &base).await?;
    let plan_path = kheish_session::prepare_storage_path_for_write(
        &state_root.join("session-purges"),
        "mission-keep",
        "json",
    )?;
    std::fs::write(
        &plan_path,
        serde_json::to_string_pretty(&json!({
            "session_id": "mission-keep",
            "agent_id": keep_view.agent_id,
            "reason": "retention",
            "requested_at_ms": 1,
        }))?,
    )?;

    let (address, shutdown) =
        anthropic_provider_daemon(&state_root, &workspace_root, &endpoint).await?;
    let base = format!("http://{address}");
    let resumed = client
        .get(format!("{base}/v1/sessions/mission-keep"))
        .send()
        .await?;
    assert_eq!(resumed.status(), StatusCode::NOT_FOUND);
    assert!(
        !plan_path.exists(),
        "the plan is removed once the purge finishes"
    );
    assert!(
        !state_root
            .join("learnings")
            .join(format!("{}.json", kept_learning.learning_id))
            .exists()
    );
    assert!(
        !resolve_storage_path_for_read(&state_root.join("runs"), &kept_run.run_id, "json").exists()
    );

    // Purged run ids are never issued again.
    create_test_session(&client, &base, "mission-next").await?;
    let next = submit_run_and_complete(&client, &base, "mission-next", "hello").await?;
    let highest_purged = purged
        .run_ids
        .iter()
        .map(|run_id| run_number(run_id))
        .max()
        .unwrap_or_default();
    assert!(
        run_number(&next.run_id) > highest_purged,
        "{} reuses a purged id (highest purged run-{highest_purged})",
        next.run_id
    );

    let _ = shutdown.send(());
    wait_for_daemon_shutdown(&client, &base).await?;
    Ok(())
}
