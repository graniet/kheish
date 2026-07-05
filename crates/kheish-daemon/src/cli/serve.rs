//! Daemon bootstrap helpers for the CLI binary.

mod support;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::cli::write_route_inventory_metadata;
use crate::logging::{LoggingConfig, init_logging};
use crate::{
    DaemonConfig, McpDiscoveryArg, SchedulerPolicyConfig, SubagentPolicyConfig,
    default_codex_credentials_path, default_codex_mcp_config_path,
};

pub(crate) use support::*;

use crate::cli::state_lock::StateRootLock;

/// Per-boot toggles that separate `serve` from the onboarding `up` flow.
#[derive(Clone, Copy, Default)]
struct ServeOptions {
    allow_empty_routes: bool,
    print_console_url: bool,
    open_browser: bool,
}

/// Starts the daemon HTTP server from parsed CLI arguments.
pub(crate) async fn run_serve(args: crate::ServeArgs) -> Result<()> {
    serve_daemon(args, ServeOptions::default()).await
}

/// Starts the daemon in onboarding mode.
///
/// Generates a master key if none is configured, tolerates an empty route
/// inventory, prints the console URL, and (unless `--no-open`) opens a browser.
pub(crate) async fn run_up(args: crate::UpArgs) -> Result<()> {
    ensure_up_master_key(&args.serve.state_root)?;
    serve_daemon(
        args.serve,
        ServeOptions {
            allow_empty_routes: true,
            print_console_url: true,
            open_browser: !args.no_open,
        },
    )
    .await
}

async fn serve_daemon(args: crate::ServeArgs, options: ServeOptions) -> Result<()> {
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
    config.allow_empty_routes = options.allow_empty_routes || args.allow_empty_routes;
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
    config.scheduler_enabled = !args.disable_scheduler;
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
    let routes = match resolve_route_inventory(&args, auth_manager.clone()).await {
        Ok(routes) => routes,
        Err(error) if config.allow_empty_routes => {
            warn!(
                error = %format!("{error:#}"),
                "no model routes resolved; starting in onboarding mode (add routes via POST /v1/runtime/routes)"
            );
            Vec::new()
        }
        Err(error) => return Err(error),
    };
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
    if options.print_console_url || options.open_browser {
        let bound = listener
            .local_addr()
            .context("failed to inspect bound control-plane address")?;
        let console_url = console_url_for(bound);
        if options.print_console_url {
            info!(url = %console_url, "kheish console ready");
            println!("Kheish console ready at {console_url}");
        }
        if options.open_browser
            && let Err(error) = crate::cli::open_browser_url(&console_url)
        {
            warn!(error = %error, "failed to open the console in a browser");
        }
    }
    service
        .serve_with_shutdown(listener, async {
            wait_for_shutdown_signal().await;
        })
        .await
}

/// Builds the loopback console URL for a bound control-plane address.
///
/// A wildcard bind (`0.0.0.0`/`::`) is reachable on loopback, so point the
/// operator at localhost rather than an unroutable `0.0.0.0` URL.
pub(crate) fn console_url_for(addr: std::net::SocketAddr) -> String {
    let host = if addr.ip().is_unspecified() {
        if addr.is_ipv6() {
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        } else {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        }
    } else {
        addr.ip()
    };
    format!("http://{}/", std::net::SocketAddr::new(host, addr.port()))
}

/// Ensures an auth-store master key is available for the onboarding `up` flow.
///
/// Honors an operator-provided key (env or file). Otherwise it generates one,
/// persists it at `<state_root>/auth-store-master.key` with `0600` permissions,
/// and points `KHEISH_AUTH_STORE_MASTER_KEY_FILE` at it. Idempotent: a re-launch
/// reuses the existing file rather than minting a new key.
pub(crate) fn ensure_up_master_key(state_root: &std::path::Path) -> Result<()> {
    use kheish_auth::{AUTH_STORE_MASTER_KEY_FILE_ENV, load_auth_store_master_key_from_env};
    if load_auth_store_master_key_from_env()?.is_some() {
        return Ok(());
    }
    std::fs::create_dir_all(state_root).with_context(|| {
        format!(
            "failed to create daemon state root {}",
            state_root.display()
        )
    })?;
    let key_path = state_root.join("auth-store-master.key");
    if !key_path.exists() {
        let key = kheish_auth::generate_auth_store_master_key_base64();
        write_private_file(&key_path, key.as_bytes())?;
    }
    // SAFETY: `up` runs this at process start, before any worker thread reads the
    // auth-store master key; the daemon consumes this env var moments later in
    // the same task.
    unsafe {
        std::env::set_var(AUTH_STORE_MASTER_KEY_FILE_ENV, &key_path);
    }
    Ok(())
}

/// Writes `contents` to `path`, creating it with `0600` permissions on Unix.
fn write_private_file(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
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
