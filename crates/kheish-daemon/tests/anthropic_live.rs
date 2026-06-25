use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use kheish_agent::ForkContext;
use kheish_daemon::{
    CreateSessionRequest, DaemonConfig, DaemonRunKind, DaemonRunStatus, ResolveApprovalsRequest,
    RunDebugView, RunView, RuntimeSettingsView, SessionEventLogView, SessionView,
    SetDebugLevelRequest, SetModelRequest, SetPermissionModeRequest, SetSystemPromptRequest,
    SubmitInputRequest, build_anthropic_daemon,
};
use kheish_runtime::{
    AnthropicProviderConfig, DebugCaptureLevel, ModelGenerationConfig, PermissionMode,
    SystemPromptSettings,
};
use kheish_types::{ApprovalResolution, ApprovalResolutionBehavior, SessionEvent};
use reqwest::Client;
use tempfile::TempDir;
use tokio::sync::oneshot;

const DEFAULT_MODEL: &str = "claude-opus-4-6";

struct LiveDaemonHarness {
    _temp: TempDir,
    base_url: String,
    workspace_root: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Drop for LiveDaemonHarness {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_live_reads_file_and_answers() -> Result<()> {
    let _guard = live_test_guard();
    let Some(harness) = start_live_daemon(&[("facts/alpha.txt", "KHEISH_DAEMON_READ_OK")]).await?
    else {
        return Ok(());
    };
    let client = Client::new();

    create_session(&client, &harness.base_url, "live-read").await?;
    let target = harness.workspace_root.join("facts/alpha.txt");
    let view = submit_input(
        &client,
        &harness.base_url,
        "live-read",
        format!(
            "Use the read_file tool exactly once to read {}. After the tool result is available, reply with the file content only.",
            target.display()
        ),
    )
    .await?;

    assert!(view.snapshot.pending_approvals.is_empty());
    assert_eq!(view.outputs.len(), 1);
    assert!(
        view.outputs[0].content.contains("KHEISH_DAEMON_READ_OK"),
        "unexpected daemon output: {}",
        view.outputs[0].content
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_live_write_file_requires_approval_and_resumes() -> Result<()> {
    let _guard = live_test_guard();
    let Some(harness) = start_live_daemon(&[]).await? else {
        return Ok(());
    };
    let client = Client::new();

    create_session(&client, &harness.base_url, "live-write").await?;
    let relative_path = "approval.txt";
    let file_path = harness.workspace_root.join(relative_path);
    let pending = submit_input(
        &client,
        &harness.base_url,
        "live-write",
        format!(
            "Use the write_file tool exactly once to write KHEISH_DAEMON_WRITE_OK into {relative_path}. After the tool succeeds, reply with exactly WRITE_DONE."
        ),
    )
    .await?;

    assert_eq!(pending.snapshot.pending_approvals.len(), 1);
    assert_eq!(
        pending.snapshot.pending_approvals[0].tool_name,
        "write_file"
    );
    assert_eq!(
        pending.snapshot.pending_approvals[0].input["path"].as_str(),
        Some(relative_path)
    );

    let resumed = client
        .post(format!(
            "{}/v1/sessions/live-write/approvals",
            harness.base_url
        ))
        .json(&ResolveApprovalsRequest {
            idempotency_key: None,
            resolutions: vec![ApprovalResolution {
                request_id: pending.snapshot.pending_approvals[0].id.clone(),
                behavior: ApprovalResolutionBehavior::Allow,
                updated_input: None,
                justification: Some("approved by live test".to_string()),
                reason: None,
            }],
        })
        .send()
        .await?
        .error_for_status()?
        .json::<SessionView>()
        .await?;

    assert!(resumed.snapshot.pending_approvals.is_empty());
    let events = client
        .get(format!(
            "{}/v1/sessions/live-write/events",
            harness.base_url
        ))
        .send()
        .await?;
    let events = error_for_status_with_body(events)
        .await?
        .json::<SessionEventLogView>()
        .await?;
    let output_path = events
        .session
        .journal
        .iter()
        .find_map(|entry| match &entry.event {
            SessionEvent::ToolCallFinished { result } if !result.is_error => result
                .output
                .get("path")
                .and_then(|value| value.as_str())
                .map(PathBuf::from),
            _ => None,
        });
    assert!(
        output_path.is_some(),
        "write_file did not complete successfully: {}",
        serde_json::to_string_pretty(&events)?
    );
    let output_path = output_path.expect("write_file output path");
    assert_eq!(output_path, file_path);
    assert_eq!(
        fs::read_to_string(&output_path)
            .with_context(|| format!("expected {}", output_path.display()))?
            .trim_end(),
        "KHEISH_DAEMON_WRITE_OK"
    );
    assert_eq!(resumed.outputs.len(), 1);
    assert!(
        resumed.outputs[0].content.contains("WRITE_DONE"),
        "unexpected daemon output: {}",
        resumed.outputs[0].content
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_live_supports_sidechains_and_mailboxes() -> Result<()> {
    let _guard = live_test_guard();
    let Some(harness) = start_live_daemon(&[]).await? else {
        return Ok(());
    };
    let client = Client::new();

    let root = create_session(&client, &harness.base_url, "live-root").await?;
    let sidechain = client
        .post(format!(
            "{}/v1/agents/{}/sidechains",
            harness.base_url, root.agent_id
        ))
        .json(&kheish_daemon::SpawnSidechainRequest {
            session_id: Some("live-sidechain".to_string()),
            thread_id: Some("subtask-1".to_string()),
            route_policy: None,
            provider: None,
            permission_mode: None,
            retention: None,
            nickname: None,
            spawn_request_id: None,
            spawned_by_run_id: None,
            fork_context: ForkContext {
                parent_assistant_message: String::new(),
                inherited_tool_call_ids: Vec::new(),
                team_name: None,
                isolation: None,
                system_prompt: String::new(),
                prompt_merge_mode: kheish_runtime::PromptMergeMode::Replace,
                provider: None,
                generation: None,
                tool_surface: kheish_types::ToolSurfaceFilter::default(),
                worktree_path: Some(harness.workspace_root.display().to_string()),
            },
            generation: None,
            tool_surface: None,
            capability_scope: None,
            credential_scope: None,
            subtask: None,
        })
        .send()
        .await?
        .error_for_status()?
        .json::<SessionView>()
        .await?;
    assert_eq!(sidechain.session_id, "live-sidechain");

    let answered = submit_input(
        &client,
        &harness.base_url,
        "live-sidechain",
        "Reply with exactly SIDECHAIN_OK.".to_string(),
    )
    .await?;
    assert_eq!(answered.outputs.len(), 1);
    assert!(
        answered.outputs[0].content.contains("SIDECHAIN_OK"),
        "unexpected sidechain output: {}",
        answered.outputs[0].content
    );

    let mailbox_status = client
        .post(format!("{}/v1/mailboxes", harness.base_url))
        .json(&kheish_daemon::PostMailboxRequest {
            message_id: None,
            from_agent_id: root.agent_id.clone(),
            to_agent_id: sidechain.agent_id.clone(),
            subject: "handoff".to_string(),
            ttl_ms: None,
            payload: serde_json::json!({
                "instruction": "Reply with exactly MAILBOX_OK.",
                "message": "inspect done"
            }),
        })
        .send()
        .await?
        .status();
    assert_eq!(mailbox_status, reqwest::StatusCode::ACCEPTED);

    let run = wait_for_new_run(
        &client,
        &harness.base_url,
        "live-sidechain",
        kheish_daemon::DaemonRunKind::MailboxDelivery,
    )
    .await?;
    let completed = wait_for_run(&client, &harness.base_url, &run.run_id).await?;
    assert_eq!(
        completed.kind,
        kheish_daemon::DaemonRunKind::MailboxDelivery
    );
    assert_eq!(completed.outputs.len(), 1);
    assert!(
        completed.outputs[0].content.contains("MAILBOX_OK"),
        "unexpected mailbox output: {}",
        completed.outputs[0].content
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_live_reconfigures_runtime_and_auto_allows_edits() -> Result<()> {
    let _guard = live_test_guard();
    let Some(harness) = start_live_daemon(&[]).await? else {
        return Ok(());
    };
    let client = Client::new();

    let runtime = client
        .get(format!("{}/v1/runtime", harness.base_url))
        .send()
        .await?
        .error_for_status()?
        .json::<RuntimeSettingsView>()
        .await?;
    assert_eq!(runtime.provider.as_deref(), Some("anthropic"));
    assert_eq!(runtime.model.as_deref(), Some(DEFAULT_MODEL));
    assert_eq!(runtime.permission_mode, PermissionMode::Default);

    let runtime = client
        .post(format!("{}/v1/runtime/model", harness.base_url))
        .json(&SetModelRequest {
            provider: None,
            model: DEFAULT_MODEL.to_string(),
            expected_revision: None,
        })
        .send()
        .await?
        .error_for_status()?
        .json::<RuntimeSettingsView>()
        .await?;
    assert_eq!(runtime.model.as_deref(), Some(DEFAULT_MODEL));

    let runtime = client
        .post(format!("{}/v1/runtime/permission-mode", harness.base_url))
        .json(&SetPermissionModeRequest {
            mode: PermissionMode::AcceptEdits,
            expected_revision: None,
        })
        .send()
        .await?
        .error_for_status()?
        .json::<RuntimeSettingsView>()
        .await?;
    assert_eq!(runtime.permission_mode, PermissionMode::AcceptEdits);

    create_session(&client, &harness.base_url, "live-reconfig").await?;
    let relative_path = "auto-allow.txt";
    let absolute_path = harness.workspace_root.join(relative_path);
    let view = submit_input(
        &client,
        &harness.base_url,
        "live-reconfig",
        format!(
            "Use the write_file tool exactly once to write KHEISH_DAEMON_RECONFIG_OK into {relative_path}. After the tool succeeds, reply with exactly AUTO_ALLOW_DONE."
        ),
    )
    .await?;

    assert!(
        view.snapshot.pending_approvals.is_empty(),
        "write_file should have been auto-approved in acceptEdits mode: {}",
        serde_json::to_string_pretty(&view)?
    );
    let written = fs::read_to_string(&absolute_path)?;
    assert_eq!(written.trim_end(), "KHEISH_DAEMON_RECONFIG_OK");
    assert_eq!(view.outputs.len(), 1);
    assert!(
        view.outputs[0].content.contains("AUTO_ALLOW_DONE"),
        "unexpected daemon output: {}",
        view.outputs[0].content
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_live_applies_override_system_prompt() -> Result<()> {
    let _guard = live_test_guard();
    let Some(harness) = start_live_daemon(&[]).await? else {
        return Ok(());
    };
    let client = Client::new();

    let runtime = client
        .post(format!("{}/v1/runtime/system-prompt", harness.base_url))
        .json(&SetSystemPromptRequest {
            settings: SystemPromptSettings {
                override_prompt: Some(
                    "Reply with exactly KHEISH_SYSTEM_PROMPT_OK and nothing else.".to_string(),
                ),
                ..SystemPromptSettings::default()
            },
            expected_revision: None,
        })
        .send()
        .await?
        .error_for_status()?
        .json::<RuntimeSettingsView>()
        .await?;
    assert_eq!(
        runtime.system_prompt.override_prompt.as_deref(),
        Some("Reply with exactly KHEISH_SYSTEM_PROMPT_OK and nothing else.")
    );

    create_session(&client, &harness.base_url, "live-system-prompt").await?;
    let view = submit_input(
        &client,
        &harness.base_url,
        "live-system-prompt",
        "Say hello.".to_string(),
    )
    .await?;

    assert!(view.snapshot.pending_approvals.is_empty());
    assert_eq!(view.outputs.len(), 1);
    assert!(
        !view.outputs[0].content.trim().is_empty(),
        "unexpected daemon output: {}",
        view.outputs[0].content
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_live_runs_background_machine_report_end_to_end() -> Result<()> {
    let _guard = live_test_guard();
    let Some(harness) = start_live_daemon(&[]).await? else {
        return Ok(());
    };
    let client = Client::new();

    client
        .post(format!("{}/v1/runtime/permission-mode", harness.base_url))
        .json(&SetPermissionModeRequest {
            mode: PermissionMode::BypassPermissions,
            expected_revision: None,
        })
        .send()
        .await?
        .error_for_status()?;

    create_session(&client, &harness.base_url, "live-background-report").await?;
    let relative_path = "reports/machine-report.txt";
    let absolute_path = harness.workspace_root.join(relative_path);
    let run = submit_run(
        &client,
        &harness.base_url,
        "live-background-report",
        format!(
            "{}{}{}{}",
            "Inspect the current machine using bash commands. ",
            "Collect the hostname, kernel, uptime, memory summary, current user, and at least one disk summary. ",
            format!(
                "Then use write_file exactly once to write a concise report to {relative_path}. "
            ),
            "After the file is written, reply with exactly MACHINE_REPORT_DONE."
        ),
    )
    .await?;
    assert!(
        matches!(
            run.status,
            DaemonRunStatus::Running | DaemonRunStatus::Completed
        ),
        "unexpected initial run state: {}",
        serde_json::to_string_pretty(&run)?
    );

    let completed = wait_for_run(&client, &harness.base_url, &run.run_id).await?;
    assert_eq!(completed.status, DaemonRunStatus::Completed);
    assert!(
        completed
            .outputs
            .iter()
            .any(|output| output.content.contains("MACHINE_REPORT_DONE")),
        "unexpected outputs: {}",
        serde_json::to_string_pretty(&completed)?
    );
    let report = fs::read_to_string(&absolute_path)
        .with_context(|| format!("expected {}", absolute_path.display()))?;
    assert!(!report.trim().is_empty());
    let lowercase = report.to_lowercase();
    assert!(
        lowercase.contains("hostname")
            || lowercase.contains("nom d'hôte")
            || lowercase.contains("nom d’hôte")
            || lowercase.contains("hôte")
    );
    assert!(lowercase.contains("kernel"));
    assert!(lowercase.contains("memory") || lowercase.contains("ram"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_live_completes_french_workspace_report_without_explicit_write_tool_instruction()
-> Result<()> {
    let _guard = live_test_guard();
    let Some(harness) = start_live_daemon(&[]).await? else {
        return Ok(());
    };
    let client = Client::new();

    client
        .post(format!("{}/v1/runtime/permission-mode", harness.base_url))
        .json(&SetPermissionModeRequest {
            mode: PermissionMode::BypassPermissions,
            expected_revision: None,
        })
        .send()
        .await?
        .error_for_status()?;

    create_session(&client, &harness.base_url, "live-background-report-fr").await?;
    let relative_path = "reports/machine-report-fr.txt";
    let absolute_path = harness.workspace_root.join(relative_path);
    let run = submit_run(
        &client,
        &harness.base_url,
        "live-background-report-fr",
        format!(
            "{}{}{}",
            "Analyse la machine sur laquelle tu tournes. ",
            "Collecte le hostname, le noyau, l'utilisateur courant, un résumé mémoire et un résumé disque. ",
            format!(
                "Fais ensuite un rapport complet dans le fichier du workspace {relative_path}, puis réponds brièvement quand c'est fait."
            ),
        ),
    )
    .await?;
    assert!(
        matches!(
            run.status,
            DaemonRunStatus::Running | DaemonRunStatus::Completed
        ),
        "unexpected initial run state: {}",
        serde_json::to_string_pretty(&run)?
    );

    let completed = wait_for_run(&client, &harness.base_url, &run.run_id).await?;
    assert_eq!(completed.status, DaemonRunStatus::Completed);
    let report = fs::read_to_string(&absolute_path)
        .with_context(|| format!("expected {}", absolute_path.display()))?;
    assert!(!report.trim().is_empty());
    let lowercase = report.to_lowercase();
    assert!(
        lowercase.contains("hostname")
            || lowercase.contains("nom d'hôte")
            || lowercase.contains("nom d’hôte")
            || lowercase.contains("nom d'hote")
    );
    assert!(lowercase.contains("kernel") || lowercase.contains("noyau"));
    assert!(
        lowercase.contains("memory")
            || lowercase.contains("memoire")
            || lowercase.contains("mémoire")
    );

    let events = client
        .get(format!(
            "{}/v1/sessions/live-background-report-fr/events",
            harness.base_url
        ))
        .send()
        .await?;
    let events = error_for_status_with_body(events)
        .await?
        .json::<SessionEventLogView>()
        .await?;
    assert!(
        events.session.journal.iter().any(|entry| matches!(
            &entry.event,
            SessionEvent::ToolCallFinished { result }
                if !result.is_error
                    && result
                        .output
                        .get("path")
                        .and_then(|value| value.as_str())
                        == Some(absolute_path.to_string_lossy().as_ref())
        )),
        "write_file/edit_file completion was not observed: {}",
        serde_json::to_string_pretty(&events)?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_live_debug_artifacts_capture_provider_requests() -> Result<()> {
    let _guard = live_test_guard();
    let Some(harness) = start_live_daemon(&[]).await? else {
        return Ok(());
    };
    let client = Client::new();

    client
        .post(format!("{}/v1/runtime/debug-level", harness.base_url))
        .json(&SetDebugLevelRequest {
            level: DebugCaptureLevel::Redacted,
            expected_revision: None,
        })
        .send()
        .await?
        .error_for_status()?;
    client
        .post(format!("{}/v1/runtime/system-prompt", harness.base_url))
        .json(&SetSystemPromptRequest {
            settings: SystemPromptSettings {
                custom_prompt: Some(
                    "Reply with exactly ANTHROPIC_DEBUG_PROMPT_OK and nothing else.".to_string(),
                ),
                ..SystemPromptSettings::default()
            },
            expected_revision: None,
        })
        .send()
        .await?
        .error_for_status()?;

    create_session(&client, &harness.base_url, "live-debug-artifacts").await?;
    let run = submit_run(
        &client,
        &harness.base_url,
        "live-debug-artifacts",
        "Say hello.".to_string(),
    )
    .await?;
    let completed = wait_for_run(&client, &harness.base_url, &run.run_id).await?;
    assert_eq!(completed.status, DaemonRunStatus::Completed);

    let debug = client
        .get(format!(
            "{}/v1/runs/{}/debug",
            harness.base_url, completed.run_id
        ))
        .send()
        .await?
        .error_for_status()?
        .json::<RunDebugView>()
        .await?;
    let artifact_ids = debug
        .artifacts
        .iter()
        .map(|artifact| artifact.artifact_id.as_str())
        .collect::<Vec<_>>();
    assert!(artifact_ids.contains(&"turn-0001-attempt-0001-model-request"));
    assert!(artifact_ids.contains(&"turn-0001-attempt-0001-provider-request"));
    assert!(artifact_ids.contains(&"turn-0001-attempt-0001-provider-response"));
    assert!(artifact_ids.contains(&"turn-0001-attempt-0001-provider-events"));

    let provider_request = client
        .get(format!(
            "{}/v1/runs/{}/debug/artifacts/turn-0001-attempt-0001-provider-request",
            harness.base_url, completed.run_id
        ))
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;
    assert_eq!(provider_request["provider"], "anthropic");
    assert_eq!(provider_request["headers"]["x-api-key"], "<redacted>");
    let system = provider_request["body"]["system"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(system.contains("ANTHROPIC_DEBUG_PROMPT_OK"));

    let provider_events = client
        .get(format!(
            "{}/v1/runs/{}/debug/artifacts/turn-0001-attempt-0001-provider-events",
            harness.base_url, completed.run_id
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    assert!(!provider_events.trim().is_empty());

    Ok(())
}

async fn start_live_daemon(files: &[(&str, &str)]) -> Result<Option<LiveDaemonHarness>> {
    let Some(provider) = live_provider_config()? else {
        return Ok(None);
    };
    let temp = tempfile::tempdir()?;
    let state_root = temp.path().join("state");
    let workspace_root = temp.path().join("workspace");
    fs::create_dir_all(&workspace_root)?;
    for (relative_path, content) in files {
        write_workspace_file(&workspace_root, relative_path, content)?;
    }

    let config = DaemonConfig::new(
        "127.0.0.1:0".parse::<SocketAddr>()?,
        &state_root,
        &workspace_root,
    );
    let (service, listener) = build_anthropic_daemon(config, provider).await?;
    let address = listener.local_addr()?;
    let (shutdown, shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        let _ = service
            .serve_with_shutdown(listener, async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    Ok(Some(LiveDaemonHarness {
        _temp: temp,
        base_url: format!("http://{address}"),
        workspace_root,
        shutdown: Some(shutdown),
    }))
}

async fn create_session(client: &Client, base_url: &str, session_id: &str) -> Result<SessionView> {
    let response = client
        .post(format!("{base_url}/v1/sessions"))
        .json(&CreateSessionRequest {
            session_id: Some(session_id.to_string()),
            thread_id: None,
            persona_id: None,
            capability_scope: None,
            credential_scope: None,
        })
        .send()
        .await?;
    error_for_status_with_body(response)
        .await?
        .json::<SessionView>()
        .await
        .map_err(Into::into)
}

async fn submit_input(
    client: &Client,
    base_url: &str,
    session_id: &str,
    content: String,
) -> Result<SessionView> {
    let response = client
        .post(format!("{base_url}/v1/sessions/{session_id}/input"))
        .json(&SubmitInputRequest {
            source_plugin: None,
            source_kind: None,
            actor_id: None,
            provider: None,
            content,
            input_items: Vec::new(),
            attachments: Vec::new(),
            generation: Some(ModelGenerationConfig {
                temperature: Some(0.0),
                ..ModelGenerationConfig::default()
            }),
            completion_requirements: None,
            metadata: None,
            reply_address: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
        })
        .send()
        .await?;
    error_for_status_with_body(response)
        .await?
        .json::<SessionView>()
        .await
        .map_err(Into::into)
}

async fn submit_run(
    client: &Client,
    base_url: &str,
    session_id: &str,
    content: String,
) -> Result<RunView> {
    let response = client
        .post(format!("{base_url}/v1/sessions/{session_id}/runs"))
        .json(&SubmitInputRequest {
            source_plugin: None,
            source_kind: None,
            actor_id: None,
            provider: None,
            content,
            input_items: Vec::new(),
            attachments: Vec::new(),
            generation: Some(ModelGenerationConfig {
                temperature: Some(0.0),
                ..ModelGenerationConfig::default()
            }),
            completion_requirements: None,
            metadata: None,
            reply_address: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
        })
        .send()
        .await?;
    error_for_status_with_body(response)
        .await?
        .json::<RunView>()
        .await
        .map_err(Into::into)
}

async fn wait_for_run(client: &Client, base_url: &str, run_id: &str) -> Result<RunView> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let response = client
            .get(format!("{base_url}/v1/runs/{run_id}"))
            .send()
            .await?;
        let run = error_for_status_with_body(response)
            .await?
            .json::<RunView>()
            .await?;
        if matches!(
            run.status,
            DaemonRunStatus::Completed
                | DaemonRunStatus::Failed
                | DaemonRunStatus::Interrupted
                | DaemonRunStatus::Cancelled
        ) {
            return Ok(run);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for run {run_id} to complete"
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

async fn wait_for_new_run(
    client: &Client,
    base_url: &str,
    session_id: &str,
    kind: DaemonRunKind,
) -> Result<RunView> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let response = client
            .get(format!("{base_url}/v1/runs?session_id={session_id}"))
            .send()
            .await?;
        let runs = error_for_status_with_body(response)
            .await?
            .json::<Vec<RunView>>()
            .await?;
        if let Some(run) = runs.into_iter().find(|run| run.kind == kind) {
            return Ok(run);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for a {:?} run in session {session_id}",
            kind
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

fn write_workspace_file(workspace_root: &Path, relative_path: &str, content: &str) -> Result<()> {
    let path = workspace_root.join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content).context("failed to write workspace fixture")
}

async fn error_for_status_with_body(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    anyhow::bail!("daemon returned {status}: {body}");
}

fn live_provider_config() -> Result<Option<AnthropicProviderConfig>> {
    let Some(api_key) = first_env(&["KHEISH_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"]) else {
        eprintln!("Skipping daemon Anthropic live tests: no API key environment variable was set.");
        return Ok(None);
    };
    let model = first_env(&["KHEISH_ANTHROPIC_MODEL", "ANTHROPIC_MODEL"])
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    Ok(Some(AnthropicProviderConfig::new(model, api_key)))
}

fn first_env(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| std::env::var(name).ok())
}

fn live_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
