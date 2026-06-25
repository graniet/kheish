//! Daemon bootstrap helpers for the CLI binary.

mod support;

use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;
use tracing::{info, warn};

use crate::cli::write_route_inventory_metadata;
use crate::logging::{LoggingConfig, init_logging};
use crate::{
    DaemonConfig, McpDiscoveryArg, SchedulerPolicyConfig, SubagentPolicyConfig,
    default_codex_credentials_path, default_codex_mcp_config_path,
};

pub(crate) use support::*;

#[cfg(unix)]
struct StateRootLock {
    file: std::fs::File,
}

#[cfg(unix)]
impl StateRootLock {
    fn acquire(state_root: &Path) -> Result<Self> {
        use std::os::fd::AsRawFd;

        std::fs::create_dir_all(state_root)
            .with_context(|| format!("failed to create state root {}", state_root.display()))?;
        let path = state_root.join("daemon.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open daemon lock {}", path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            anyhow::bail!(
                "state root {} is already locked by another daemon: {error}",
                state_root.display()
            );
        }
        writeln!(&file, "pid={}\nmechanism=flock", std::process::id())
            .with_context(|| format!("failed to write daemon lock {}", path.display()))?;
        Ok(Self { file })
    }
}

#[cfg(unix)]
impl Drop for StateRootLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;

        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(not(unix))]
struct StateRootLock;

#[cfg(not(unix))]
impl StateRootLock {
    fn acquire(state_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_root)
            .with_context(|| format!("failed to create state root {}", state_root.display()))?;
        Ok(Self)
    }
}

/// Starts the daemon HTTP server from parsed CLI arguments.
pub(crate) async fn run_serve(args: crate::ServeArgs) -> Result<()> {
    init_logging(LoggingConfig {
        format: args.log_format.resolve(),
        level: args.log_level.into(),
    })?;
    let workspace_root = match args.workspace_root {
        Some(ref workspace_root) => workspace_root.clone(),
        None => std::env::current_dir().context("failed to resolve current directory")?,
    };
    let state_root = args.state_root.clone();
    let mut config = DaemonConfig::new(args.bind, state_root, workspace_root);
    config.mcp_config_path = match args.mcp_discovery {
        McpDiscoveryArg::Auto => args
            .mcp_config
            .clone()
            .or_else(default_codex_mcp_config_path),
        McpDiscoveryArg::Disabled => args.mcp_config.clone(),
    };
    config.mcp_credentials_path = match args.mcp_discovery {
        McpDiscoveryArg::Auto => args
            .mcp_credentials
            .clone()
            .or_else(default_codex_credentials_path),
        McpDiscoveryArg::Disabled => args.mcp_credentials.clone(),
    };
    config.mcp_catalog_profiles = args.mcp_profiles.clone();
    config.connectors_config_path = args.connectors_config.clone();
    config.skill_roots = args.skill_roots.clone();
    config.control_plane_auth = resolve_control_plane_auth_config(&args)?;
    config.control_plane_auth_token_files = control_plane_auth_token_files(&args);
    config.control_plane_cors = resolve_control_plane_cors_config(&args)?;
    config.subagent_policy = if let Some(path) = args.subagent_policy_file.as_ref() {
        let raw = std::fs::read(path)
            .with_context(|| format!("failed to read subagent policy {}", path.display()))?;
        serde_json::from_slice(&raw)
            .with_context(|| format!("failed to parse subagent policy {}", path.display()))?
    } else {
        let mut policy = SubagentPolicyConfig::default();
        policy.max_child_depth = args.max_child_depth;
        policy.max_live_children_per_parent = args.max_live_children_per_parent;
        policy.max_live_descendants_per_root = args.max_live_descendants_per_root;
        policy.max_spawns_per_run = args.max_spawns_per_run;
        policy.max_live_sidechains_global = args.max_live_sidechains_global;
        policy.max_live_sidechains_per_session = args.max_live_sidechains_per_session;
        policy.spawn_rate_window_ms = args.spawn_rate_window_ms;
        policy.max_spawns_per_session_window = args.max_spawns_per_session_window;
        policy.max_spawns_per_profile_window = args.max_spawns_per_profile_window;
        policy.max_spawns_per_project_window = args.max_spawns_per_project_window;
        policy.max_spawns_global_window = args.max_spawns_global_window;
        policy.max_spawn_input_tokens_per_request = args.max_spawn_input_tokens_per_request;
        policy.max_spawn_output_tokens_per_request = args.max_spawn_output_tokens_per_request;
        policy.max_spawn_cost_microusd_per_window = args.max_spawn_cost_microusd_per_window;
        policy.max_spawn_cpu_ms_per_window = args.max_spawn_cpu_ms_per_window;
        policy.estimated_spawn_cost_microusd = args.estimated_spawn_cost_microusd;
        policy.estimated_spawn_cpu_ms = args.estimated_spawn_cpu_ms;
        policy
    };
    config.scheduler_policy = SchedulerPolicyConfig {
        retry_base_delay_ms: args.scheduler_retry_base_delay_ms,
        retry_max_delay_ms: args.scheduler_retry_max_delay_ms,
        retry_jitter_ms: args.scheduler_retry_jitter_ms,
        retry_max_attempts: args.scheduler_retry_max_attempts,
    };
    config.event_history_capacity = args.event_history_capacity.max(1);
    config.model_budget_max_total_output_tokens = args.model_budget_max_total_output_tokens;
    config.model_budget_max_total_cost_usd = args.model_budget_max_total_cost_usd;
    info!(
        bind = %args.bind,
        routes_file = args.routes_file.as_ref().map(|path| path.display().to_string()).as_deref(),
        workspace_root = %config.workspace_root.display(),
        state_root = %config.state_root.display(),
        mcp_config = config.mcp_config_path.as_ref().map(|path| path.display().to_string()).as_deref(),
        mcp_profiles = ?config.mcp_catalog_profiles,
        connectors_config = config.connectors_config_path.as_ref().map(|path| path.display().to_string()).as_deref(),
        "starting kheish daemon",
    );
    let _state_root_lock = StateRootLock::acquire(&args.state_root)?;
    config.state_root_lock_held = true;
    let auth_manager =
        kheish_auth::AuthManager::new(crate::cli::global_auth_store_path(&args.state_root))?;
    let routes = resolve_route_inventory(&args, auth_manager.clone()).await?;
    let route_summary = routes
        .iter()
        .map(|route| {
            format!(
                "{}={}/{}",
                route.route_id(),
                route.provider_name(),
                route.model_name()
            )
        })
        .collect::<Vec<_>>();
    info!(routes = ?route_summary, "loaded daemon routes");
    if let Err(error) =
        write_route_inventory_metadata(&args.state_root, args.routes_file.as_deref(), &routes)
    {
        warn!(error = %error, "failed to persist route inventory metadata");
    }

    let extra_image_backends = resolve_additional_image_backends(&args, auth_manager.clone())?;
    let extra_transcription_backends =
        resolve_additional_transcription_backends(&args, auth_manager.clone())?;
    let (service, listener) = kheish_daemon::build_provider_daemon(
        config,
        routes,
        extra_image_backends,
        extra_transcription_backends,
        auth_manager,
    )
    .await?;
    service
        .serve_with_shutdown(listener, async {
            wait_for_shutdown_signal().await;
        })
        .await
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate =
            signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received ctrl_c, shutting down daemon");
            }
            _ = terminate.recv() => {
                info!("received sigterm, shutting down daemon");
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        info!("received ctrl_c, shutting down daemon");
    }
}
