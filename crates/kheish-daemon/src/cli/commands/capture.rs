//! Capture-agent provisioning command handlers.

use std::io::Write;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

/// Handles `capture ...`.
pub(crate) async fn run_capture_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::CaptureCommand,
) -> Result<()> {
    match command {
        crate::CaptureCommand::Provision(args) => {
            let show_secrets = args.show_secrets;
            let (request, out_dir) = build_provision_request(client, args)?;
            let response = client
                .post_json::<_, kheish_daemon::CaptureAgentProvisionResponse>(
                    "/v1/capture-agent-provisions",
                    &request,
                )
                .await?;
            let written_configs = if let Some(out_dir) = out_dir.as_deref() {
                write_configs(&response, out_dir)?
            } else {
                Vec::new()
            };
            if out_dir.is_some() && !show_secrets {
                printer.print(&ProvisionSummaryView::from_response(
                    &response,
                    &written_configs,
                ))
            } else {
                printer.print(&response)
            }
        }
        crate::CaptureCommand::Agents { command } => match command {
            crate::CaptureAgentsCommand::List => {
                let agents = client
                    .get_json::<Vec<kheish_daemon::CaptureAgentView>>("/v1/capture-agents")
                    .await?;
                printer.print(&agents)
            }
            crate::CaptureAgentsCommand::Get { machine_id } => {
                let machine_id = crate::cli::url_encode_path_segment(&machine_id);
                let agent = client
                    .get_json::<kheish_daemon::CaptureAgentView>(&format!(
                        "/v1/capture-agents/{machine_id}"
                    ))
                    .await?;
                printer.print(&agent)
            }
        },
        crate::CaptureCommand::Alerts => {
            let alerts = client
                .get_json::<Vec<kheish_daemon::CaptureAgentAlertView>>("/v1/capture-alerts")
                .await?;
            printer.print(&alerts)
        }
        crate::CaptureCommand::Revoke(args) => {
            let machine_id = crate::cli::url_encode_path_segment(&args.machine_id);
            let view = client
                .post_json::<_, kheish_daemon::CaptureAgentView>(
                    &format!("/v1/capture-agents/{machine_id}/revoke"),
                    &kheish_daemon::RevokeCaptureAgentRequest {
                        reason: args.reason,
                    },
                )
                .await?;
            printer.print(&view)
        }
        crate::CaptureCommand::Heartbeat(args) => {
            let token = args
                .heartbeat_token
                .as_deref()
                .or(args.upload_token.as_deref())
                .ok_or_else(|| anyhow!("--heartbeat-token is required"))?;
            let machine_id = crate::cli::url_encode_path_segment(&args.machine_id);
            let view = client
                .post_json_with_bearer::<_, kheish_daemon::CaptureAgentHeartbeatResponse>(
                    &format!("/v1/capture-agents/{machine_id}/heartbeat"),
                    token,
                    &kheish_daemon::CaptureAgentHeartbeatRequest::default(),
                )
                .await?;
            printer.print(&view)
        }
    }
}

fn build_provision_request(
    client: &crate::cli::DaemonHttpClient,
    args: crate::CaptureProvisionArgs,
) -> Result<(
    kheish_daemon::CaptureAgentProvisionRequest,
    Option<std::path::PathBuf>,
)> {
    let out_dir = args.out_dir.clone();
    let mut machine_ids = args.agent_ids;
    if let Some(count) = args.count {
        let prefix = args
            .agent_prefix
            .clone()
            .ok_or_else(|| anyhow!("--count requires --agent-prefix"))?;
        for index in 0..count {
            machine_ids.push(format!("{prefix}-{index:03}"));
        }
    }
    if machine_ids.is_empty() {
        bail!("provide at least one --agent-id or --agent-prefix with --count");
    }

    let any_source_flag = args.enable_screen
        || args.enable_camera
        || args.enable_system_audio
        || args.enable_microphone;
    let sources = kheish_daemon::CaptureAgentProvisionSources {
        screen: if any_source_flag {
            args.enable_screen
        } else {
            true
        },
        camera: args.enable_camera,
        system_audio: args.enable_system_audio,
        microphone: args.enable_microphone,
    };
    if sources.camera && machine_ids.len() > 1 && args.camera_unique_id.is_some() {
        bail!(
            "--camera-unique-id can only be used with one agent; use API JSON for per-agent camera IDs"
        );
    }
    let agents = machine_ids
        .into_iter()
        .map(|machine_id| kheish_daemon::CaptureAgentProvisionTarget {
            machine_id,
            os_profile: None,
            camera_unique_id: args.camera_unique_id.clone(),
            camera_name: args.camera_name.clone(),
            microphone_name: args.microphone_name.clone(),
        })
        .collect();
    Ok((
        kheish_daemon::CaptureAgentProvisionRequest {
            batch_id: args.batch_id,
            daemon_base_url: args
                .daemon_url
                .unwrap_or_else(|| client.base_url().to_string()),
            os_profile: args.os_profile.into(),
            agents,
            sources,
            interval_ms: args.interval_ms,
            max_runs: args.max_runs,
            duration_ms: args.duration_ms,
            retention_seconds: args.retention_seconds,
            max_active_observations: args.max_active_observations,
            max_active_bytes: args.max_active_bytes,
            token_ttl_ms: args.token_ttl_ms,
            heartbeat_interval_ms: args.heartbeat_interval_ms,
            heartbeat_grace_ms: args.heartbeat_grace_ms,
        },
        out_dir,
    ))
}

fn write_configs(
    response: &kheish_daemon::CaptureAgentProvisionResponse,
    out_dir: &std::path::Path,
) -> Result<Vec<WrittenConfigView>> {
    std::fs::create_dir_all(out_dir).with_context(|| format!("create {}", out_dir.display()))?;
    let mut written = Vec::with_capacity(response.agents.len());
    for agent in &response.agents {
        let path = out_dir.join(format!(
            "{}.capture.toml",
            sanitize_file_component(&agent.machine_id)
        ));
        write_owner_only_file(&path, agent.config_toml.as_bytes())?;
        written.push(WrittenConfigView {
            machine_id: agent.machine_id.clone(),
            path,
        });
    }
    Ok(written)
}

fn sanitize_file_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn write_owner_only_file(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let temp_path = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("capture"),
        std::process::id()
    ));
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp_path)
            .with_context(|| format!("create {}", temp_path.display()))?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temp_path)
        .with_context(|| format!("create {}", temp_path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("write {}", temp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", temp_path.display()))?;
    drop(file);
    std::fs::rename(&temp_path, path)
        .with_context(|| format!("rename {} to {}", temp_path.display(), path.display()))?;
    if let Ok(parent_dir) = std::fs::File::open(parent) {
        let _ = parent_dir.sync_all();
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
struct WrittenConfigView {
    machine_id: String,
    path: std::path::PathBuf,
}

#[derive(Serialize)]
struct ProvisionSummaryView<'a> {
    batch_id: &'a str,
    agent_count: usize,
    config_files: &'a [WrittenConfigView],
    agents: Vec<ProvisionedAgentSummaryView<'a>>,
}

#[derive(Serialize)]
struct ProvisionedAgentSummaryView<'a> {
    machine_id: &'a str,
    sources: Vec<ProvisionedSourceSummaryView<'a>>,
}

#[derive(Serialize)]
struct ProvisionedSourceSummaryView<'a> {
    source_id: &'a str,
    kind: kheish_daemon::ObservationSourceKind,
    upload_token_set: bool,
}

impl<'a> ProvisionSummaryView<'a> {
    fn from_response(
        response: &'a kheish_daemon::CaptureAgentProvisionResponse,
        config_files: &'a [WrittenConfigView],
    ) -> Self {
        Self {
            batch_id: &response.batch_id,
            agent_count: response.agents.len(),
            config_files,
            agents: response
                .agents
                .iter()
                .map(|agent| ProvisionedAgentSummaryView {
                    machine_id: &agent.machine_id,
                    sources: agent
                        .sources
                        .iter()
                        .map(|source| ProvisionedSourceSummaryView {
                            source_id: &source.source_id,
                            kind: source.kind.clone(),
                            upload_token_set: !source.upload_token.trim().is_empty(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}
