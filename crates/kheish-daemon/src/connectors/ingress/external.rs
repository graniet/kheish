use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use kheish_auth::{AuthManager, AuthSlotId, CredentialLease};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tokio::sync::{oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{info, warn};

use kheish_core::ModelDriver;

use crate::DaemonState;
use crate::connectors::config::ResolvedExternalConnector;
use crate::connectors::{
    ExternalConnectorHealth, ExternalConnectorHealthStatus, ExternalConnectorManifest,
    ExternalConnectorMode, is_supported_external_connector_protocol_version,
};

const EXTERNAL_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const EXTERNAL_STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(250);
const EXTERNAL_RESTART_INITIAL_DELAY: Duration = Duration::from_secs(1);
const EXTERNAL_RESTART_MAX_DELAY: Duration = Duration::from_secs(60);
const EXTERNAL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const EXTERNAL_DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(100);
const EXTERNAL_CONNECTOR_NAME_ENV: &str = "KHEISH_EXTERNAL_CONNECTOR_NAME";
const EXTERNAL_CONNECTOR_BASE_URL_ENV: &str = "KHEISH_EXTERNAL_CONNECTOR_BASE_URL";
const EXTERNAL_CONNECTOR_DAEMON_BASE_URL_ENV: &str = "KHEISH_EXTERNAL_CONNECTOR_DAEMON_BASE_URL";
const EXTERNAL_CONNECTOR_SHARED_TOKEN_ENV: &str = "KHEISH_EXTERNAL_CONNECTOR_SHARED_TOKEN";
const EXTERNAL_CONNECTOR_CREDENTIAL_TOKEN_ENV: &str = "KHEISH_EXTERNAL_CONNECTOR_CREDENTIAL_TOKEN";
const EXTERNAL_CONNECTOR_CREDENTIAL_KEYS_JSON_ENV: &str =
    "KHEISH_EXTERNAL_CONNECTOR_CREDENTIAL_KEYS_JSON";
const PRESERVED_CHILD_PROCESS_ENV_KEYS: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "TMPDIR",
    "TMP",
    "TEMP",
    "PYTHONHOME",
    "PYTHONPATH",
    "VIRTUAL_ENV",
    "SYSTEMROOT",
    "WINDIR",
];

struct RunningExternalProcess {
    connector: ResolvedExternalConnector,
    shutdown: Option<oneshot::Sender<()>>,
}

pub(crate) fn spawn_external_process_supervisor<M>(
    state: Arc<DaemonState<M>>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let task = tokio::spawn(async move {
        let daemon_base_url = state.control_plane_base_url().to_string();
        let mut revision_rx = state.connectors().subscribe();
        let mut running: BTreeMap<String, RunningExternalProcess> = BTreeMap::new();
        let mut join_set = JoinSet::new();
        reconcile_external_processes(&state, &daemon_base_url, &mut running, &mut join_set);

        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        for (_, mut process) in std::mem::take(&mut running) {
                            if let Some(shutdown) = process.shutdown.take() {
                                let _ = shutdown.send(());
                            }
                        }
                        while join_set.join_next().await.is_some() {}
                    }
                    return;
                }
                changed = revision_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    reconcile_external_processes(&state, &daemon_base_url, &mut running, &mut join_set);
                }
                Some(result) = join_set.join_next() => {
                    match result {
                        Ok((name, Ok(()))) => {
                            running.remove(&name);
                            reconcile_external_processes(&state, &daemon_base_url, &mut running, &mut join_set);
                        }
                        Ok((name, Err(error))) => {
                            running.remove(&name);
                            warn!(connector = %name, error = ?error, "external child-process supervisor exited");
                            reconcile_external_processes(&state, &daemon_base_url, &mut running, &mut join_set);
                        }
                        Err(error) => {
                            warn!(error = ?error, "external child-process supervisor panicked");
                            reconcile_external_processes(&state, &daemon_base_url, &mut running, &mut join_set);
                        }
                    }
                }
            }
        }
    });
    task
}

fn reconcile_external_processes<M>(
    state: &Arc<DaemonState<M>>,
    daemon_base_url: &str,
    running: &mut BTreeMap<String, RunningExternalProcess>,
    join_set: &mut JoinSet<(String, anyhow::Result<()>)>,
) where
    M: ModelDriver + Send + Sync + 'static,
{
    let desired = state
        .connectors()
        .external_connectors()
        .into_iter()
        .filter(|connector| connector.mode == ExternalConnectorMode::ChildProcess)
        .map(|connector| (connector.name.clone(), connector))
        .collect::<BTreeMap<_, _>>();

    let stale = running
        .iter()
        .filter(|(name, current)| {
            desired
                .get(*name)
                .map(|connector| connector != &current.connector)
                .unwrap_or(true)
        })
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    for name in stale {
        if let Some(mut process) = running.remove(&name) {
            if let Some(shutdown) = process.shutdown.take() {
                let _ = shutdown.send(());
            }
        }
    }

    for (name, connector) in desired {
        if running.contains_key(&name) {
            continue;
        }
        let task_name = name.clone();
        let task_connector = connector.clone();
        let task_daemon_base_url = daemon_base_url.to_string();
        let auth_manager = state.auth_manager().clone();
        let runtime = state.external_connector_runtime().clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        join_set.spawn(async move {
            let result = run_external_process_loop(
                task_connector,
                &task_daemon_base_url,
                auth_manager,
                runtime,
                shutdown_rx,
            )
            .await;
            (task_name, result)
        });
        running.insert(
            name,
            RunningExternalProcess {
                connector,
                shutdown: Some(shutdown_tx),
            },
        );
    }
}

async fn run_external_process_loop(
    connector: ResolvedExternalConnector,
    daemon_base_url: &str,
    auth_manager: Arc<AuthManager>,
    runtime: Arc<crate::connectors::ExternalConnectorRuntimeService>,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let child = connector
        .child_process
        .clone()
        .ok_or_else(|| anyhow!("missing child_process settings"))?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("external ingress HTTP client should build");
    let mut restart_attempt = 0u32;
    let mut active_credential_lease: Option<CredentialLease> = None;
    loop {
        let mut command = Command::new(&child.command);
        command.args(&child.args);
        command.kill_on_drop(true);
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.as_std_mut().process_group(0);
        }
        command.env_clear();
        for key in PRESERVED_CHILD_PROCESS_ENV_KEYS {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        for (name, value) in &child.env {
            command.env(name, value);
        }
        command.env(EXTERNAL_CONNECTOR_NAME_ENV, &connector.name);
        command.env(EXTERNAL_CONNECTOR_BASE_URL_ENV, &connector.base_url);
        command.env(EXTERNAL_CONNECTOR_DAEMON_BASE_URL_ENV, daemon_base_url);
        if let Some(shared_token) = connector.shared_token.as_deref() {
            command.env(EXTERNAL_CONNECTOR_SHARED_TOKEN_ENV, shared_token);
        }
        if !child.credential_slots.is_empty() {
            if let Some(active_lease) = active_credential_lease.take() {
                auth_manager.revoke_lease(&active_lease.id, active_lease.expires_at_ms)?;
            }
            let keys = child.credential_slots.keys().cloned().collect::<Vec<_>>();
            let secret_refs = child
                .credential_slots
                .values()
                .map(AuthSlotId::new)
                .collect::<Vec<_>>();
            let (credential_token, credential_lease) = auth_manager.issue_connector_lease(
                &connector.name,
                &keys,
                &secret_refs,
                None,
                None,
            )?;
            runtime
                .set_child_process_credential_token(&connector, Some(credential_token.clone()))
                .await;
            command.env(EXTERNAL_CONNECTOR_CREDENTIAL_TOKEN_ENV, credential_token);
            command.env(
                EXTERNAL_CONNECTOR_CREDENTIAL_KEYS_JSON_ENV,
                serde_json::to_string(&keys).expect("credential env keys should serialize"),
            );
            active_credential_lease = Some(credential_lease);
        } else {
            runtime
                .set_child_process_credential_token(&connector, None)
                .await;
            if let Some(active_lease) = active_credential_lease.take() {
                auth_manager.revoke_lease(&active_lease.id, active_lease.expires_at_ms)?;
            }
        }
        if let Some(working_dir) = child.working_dir.as_deref() {
            command.current_dir(working_dir);
        }
        let mut process = match command.spawn() {
            Ok(process) => process,
            Err(error) => {
                runtime
                    .set_child_process_credential_token(&connector, None)
                    .await;
                if let Some(active_lease) = active_credential_lease.take() {
                    auth_manager.revoke_lease(&active_lease.id, active_lease.expires_at_ms)?;
                }
                runtime.note_child_restart(&connector).await;
                warn!(
                    connector = %connector.name,
                    error = ?error,
                    "external child-process connector failed to spawn; retrying with backoff"
                );
                let delay = restart_backoff_delay(&connector.name, restart_attempt);
                restart_attempt = restart_attempt.saturating_add(1);
                tokio::select! {
                    _ = &mut shutdown_rx => return Ok(()),
                    _ = tokio::time::sleep(delay) => {}
                }
                continue;
            }
        };
        let ready = tokio::select! {
            _ = &mut shutdown_rx => {
                shutdown_external_process(&client, &connector, &mut process, runtime.as_ref()).await?;
                if let Some(active_lease) = active_credential_lease.take() {
                    auth_manager.revoke_lease(&active_lease.id, active_lease.expires_at_ms)?;
                }
                return Ok(());
            }
            ready = wait_for_external_readiness(&client, &connector) => ready,
        };
        match ready {
            Ok((manifest, health)) => {
                runtime.note_manifest(&connector, manifest).await;
                runtime.note_health(&connector, health).await;
                restart_attempt = 0;
                info!(connector = %connector.name, base_url = %connector.base_url, "external child-process connector is ready");
            }
            Err(error) => {
                runtime.note_manifest_fetch_failure(&connector).await;
                warn!(connector = %connector.name, error = ?error, "external child-process connector failed readiness checks");
                shutdown_external_process(&client, &connector, &mut process, runtime.as_ref())
                    .await?;
                runtime
                    .set_child_process_credential_token(&connector, None)
                    .await;
                if let Some(active_lease) = active_credential_lease.take() {
                    auth_manager.revoke_lease(&active_lease.id, active_lease.expires_at_ms)?;
                }
                runtime.note_child_restart(&connector).await;
                tokio::time::sleep(restart_backoff_delay(&connector.name, restart_attempt)).await;
                restart_attempt = restart_attempt.saturating_add(1);
                continue;
            }
        }

        let status = tokio::select! {
            _ = &mut shutdown_rx => {
                shutdown_external_process(&client, &connector, &mut process, runtime.as_ref()).await?;
                runtime
                    .set_child_process_credential_token(&connector, None)
                    .await;
                if let Some(active_lease) = active_credential_lease.take() {
                    auth_manager.revoke_lease(&active_lease.id, active_lease.expires_at_ms)?;
                }
                return Ok(());
            }
            status = process.wait() => status.map_err(|error| {
                anyhow!(
                    "failed to wait for external connector {}: {error}",
                    connector.name
                )
            })?,
        };
        warn!(
            connector = %connector.name,
            status = %status,
            "external child-process connector exited; restarting"
        );
        runtime
            .set_child_process_credential_token(&connector, None)
            .await;
        if let Some(active_lease) = active_credential_lease.take() {
            auth_manager.revoke_lease(&active_lease.id, active_lease.expires_at_ms)?;
        }
        runtime.note_child_restart(&connector).await;
        tokio::time::sleep(restart_backoff_delay(&connector.name, restart_attempt)).await;
        restart_attempt = restart_attempt.saturating_add(1);
    }
}

async fn wait_for_external_readiness(
    client: &reqwest::Client,
    connector: &ResolvedExternalConnector,
) -> Result<(ExternalConnectorManifest, ExternalConnectorHealth)> {
    let started = tokio::time::Instant::now();
    loop {
        if started.elapsed() > EXTERNAL_STARTUP_TIMEOUT {
            anyhow::bail!(
                "external connector {} did not become ready within {:?}",
                connector.name,
                EXTERNAL_STARTUP_TIMEOUT
            );
        }
        match fetch_manifest(client, connector).await {
            Ok(manifest) => {
                if !is_supported_external_connector_protocol_version(manifest.protocol_version) {
                    anyhow::bail!(
                        "external connector {} reported unsupported protocol_version {}",
                        connector.name,
                        manifest.protocol_version
                    );
                }
                if let Ok(health) = fetch_health(client, connector).await
                    && health_matches_manifest(&manifest, &health)
                    && matches!(
                        health.status,
                        ExternalConnectorHealthStatus::Ready
                            | ExternalConnectorHealthStatus::Degraded
                    )
                {
                    return Ok((manifest, health));
                }
            }
            Err(error) => {
                warn!(connector = %connector.name, error = ?error, "waiting for external connector readiness");
            }
        }
        tokio::time::sleep(EXTERNAL_STARTUP_POLL_INTERVAL).await;
    }
}

fn health_matches_manifest(
    manifest: &ExternalConnectorManifest,
    health: &ExternalConnectorHealth,
) -> bool {
    is_supported_external_connector_protocol_version(health.protocol_version)
        && health.instance_id == manifest.instance_id
}

async fn fetch_manifest(
    client: &reqwest::Client,
    connector: &ResolvedExternalConnector,
) -> Result<ExternalConnectorManifest> {
    let mut request = client.get(format!("{}/manifest", connector.base_url));
    if let Some(shared_token) = connector.shared_token.as_deref() {
        request = request.bearer_auth(shared_token);
    }
    request
        .send()
        .await?
        .error_for_status()?
        .json::<ExternalConnectorManifest>()
        .await
        .map_err(Into::into)
}

async fn fetch_health(
    client: &reqwest::Client,
    connector: &ResolvedExternalConnector,
) -> Result<ExternalConnectorHealth> {
    let mut request = client.get(format!("{}/health", connector.base_url));
    if let Some(shared_token) = connector.shared_token.as_deref() {
        request = request.bearer_auth(shared_token);
    }
    request
        .send()
        .await?
        .error_for_status()?
        .json::<ExternalConnectorHealth>()
        .await
        .map_err(Into::into)
}

async fn shutdown_external_process(
    client: &reqwest::Client,
    connector: &ResolvedExternalConnector,
    process: &mut tokio::process::Child,
    runtime: &crate::connectors::ExternalConnectorRuntimeService,
) -> Result<()> {
    #[cfg(unix)]
    if let Some(pid) = process.id() {
        signal_external_process(connector, pid, libc::SIGTERM, "SIGTERM");
    }
    #[cfg(not(unix))]
    {
        let _ = process.start_kill();
    }

    let deadline = tokio::time::Instant::now() + EXTERNAL_SHUTDOWN_TIMEOUT;
    loop {
        if let Some(_status) = process.try_wait()? {
            return Ok(());
        }
        if let Ok(health) = fetch_health(client, connector).await {
            runtime.note_health(connector, health.clone()).await;
            if matches!(health.status, ExternalConnectorHealthStatus::Draining) {
                info!(connector = %connector.name, "external child-process connector entered draining state");
            }
        }
        if tokio::time::Instant::now() >= deadline {
            warn!(connector = %connector.name, "external child-process shutdown timed out; killing");
            #[cfg(unix)]
            if let Some(pid) = process.id() {
                signal_external_process(connector, pid, libc::SIGKILL, "SIGKILL");
            }
            let _ = process.kill().await;
            let _ = process.wait().await;
            return Ok(());
        }
        tokio::time::sleep(EXTERNAL_DRAIN_POLL_INTERVAL).await;
    }
}

#[cfg(unix)]
fn signal_external_process(
    connector: &ResolvedExternalConnector,
    pid: u32,
    signal: libc::c_int,
    signal_name: &str,
) {
    let child_pid = pid as i32;
    let group_result = unsafe { libc::kill(-child_pid, signal) };
    if group_result == 0 {
        return;
    }
    let group_error = std::io::Error::last_os_error();
    let child_result = unsafe { libc::kill(child_pid, signal) };
    if child_result != 0 {
        let child_error = std::io::Error::last_os_error();
        if child_error.kind() != std::io::ErrorKind::InvalidInput {
            warn!(
                connector = %connector.name,
                pid,
                signal = signal_name,
                group_error = ?group_error,
                child_error = ?child_error,
                "failed to signal external child-process group or child"
            );
        }
    } else {
        warn!(
            connector = %connector.name,
            pid,
            signal = signal_name,
            group_error = ?group_error,
            "external child-process group signal failed; signaled child pid only"
        );
    }
}

fn restart_backoff_delay(name: &str, attempt: u32) -> Duration {
    let capped_attempt = attempt.min(6);
    let base = EXTERNAL_RESTART_INITIAL_DELAY
        .as_millis()
        .saturating_mul(1u128 << capped_attempt)
        .min(EXTERNAL_RESTART_MAX_DELAY.as_millis());
    let digest = Sha256::digest(format!("{name}:{attempt}"));
    let jitter_ms = (u16::from_be_bytes([digest[0], digest[1]]) as u128) % 400;
    Duration::from_millis((base + jitter_ms) as u64)
}

#[cfg(test)]
mod tests {
    use crate::connectors::{
        ExternalConnectorCapabilities, ExternalConnectorHealth, ExternalConnectorHealthStatus,
        ExternalConnectorManifest,
    };

    use super::health_matches_manifest;

    #[test]
    fn external_health_must_match_manifest_identity_and_protocol() {
        let manifest = ExternalConnectorManifest {
            protocol_version: 1,
            instance_id: "instance-1".to_string(),
            capabilities: ExternalConnectorCapabilities::default(),
            experimental: false,
        };
        let health = ExternalConnectorHealth {
            protocol_version: 1,
            instance_id: "instance-1".to_string(),
            status: ExternalConnectorHealthStatus::Ready,
            detail: None,
        };
        assert!(health_matches_manifest(&manifest, &health));

        assert!(!health_matches_manifest(
            &manifest,
            &ExternalConnectorHealth {
                instance_id: "other".to_string(),
                ..health.clone()
            }
        ));
        assert!(!health_matches_manifest(
            &manifest,
            &ExternalConnectorHealth {
                protocol_version: 2,
                ..health
            }
        ));
    }
}
