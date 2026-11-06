//! Daemon tests for the runtime contract an embedding control plane relies on:
//! session tool allow-lists, caller-set run limits, routes with brokered
//! provider slots, MCP execution context, HTTP hook transport, and purges.

use super::*;

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

/// Serves the Anthropic Messages SSE shape on every path and records each
/// request, so tests can assert which endpoint and credential a route used.
async fn spawn_anthropic_messages_mock(reply: &str) -> Result<(String, CapturedModelRequests)> {
    #[derive(Clone)]
    struct MockState {
        reply: String,
        captured: CapturedModelRequests,
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
        let text = serde_json::to_string(&state.reply).expect("reply text serializes");
        let sse = format!(
            concat!(
                "event: message_start\n",
                "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg-mock\",\"usage\":{{\"input_tokens\":5,\"output_tokens\":0}}}}}}\n\n",
                "event: content_block_start\n",
                "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":{text}}}}}\n\n",
                "event: message_delta\n",
                "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"input_tokens\":5,\"output_tokens\":3}}}}\n\n",
                "event: message_stop\n",
                "data: {{\"type\":\"message_stop\"}}\n\n"
            ),
            text = text
        );
        ([(CONTENT_TYPE, "text/event-stream")], sse)
    }

    let captured: CapturedModelRequests = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let router = Router::new().fallback(messages).with_state(MockState {
        reply: reply.to_string(),
        captured: captured.clone(),
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok((format!("http://{address}"), captured))
}

/// Boots a production-built daemon with one startup Anthropic route aimed at
/// `boot_endpoint`; runtime-added routes reload from the state-root overlay.
async fn anthropic_provider_daemon(
    state_root: &Path,
    workspace_root: &Path,
    boot_endpoint: &str,
) -> Result<(SocketAddr, oneshot::Sender<()>)> {
    ensure_auth_store_master_key();
    let auth_manager = AuthManager::new(state_root.join("auth/global-slots.json"))?;
    let mut boot = AnthropicProviderConfig::new("claude-boot", "boot-key");
    boot.base_url = boot_endpoint.to_string();
    let route = ConfiguredModelRoute::new("boot", ModelRouteConfig::Anthropic(boot));
    let config = DaemonConfig::new(
        "127.0.0.1:0".parse::<SocketAddr>()?,
        state_root,
        workspace_root,
    );
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
    let (mock_base, captured) = spawn_anthropic_messages_mock("hello from the relay").await?;
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
