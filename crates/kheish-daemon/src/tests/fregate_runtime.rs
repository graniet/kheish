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
