use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kheish_auth::{AuthProvider, AuthSlotId, AuthSlotStatus};
use kheish_core::ModelDriver;
use kheish_runtime::{
    DebugCaptureLevel, LEARNED_CONTEXT_PROMPT_BUDGET_OMITTED_COUNTER, MetricsSnapshot,
    RUN_MEMORY_PROMPT_BUDGET_OMITTED_COUNTER, RUN_MEMORY_PROMPT_INJECTED_COUNTER,
};
use subtle::ConstantTimeEq;

use crate::runs::now_ms;
use crate::{
    AssetStartupRepairDiagnosticView, AssetStartupRepairStatusView, DaemonAgentStatusSummaryView,
    DaemonCapabilities, DaemonControlPlaneAuthTokenFileStatusView, DaemonControlPlaneCorsPolicy,
    DaemonControlPlaneStatusView, DaemonEventStatusView, DaemonHealthSeverity, DaemonHealthView,
    DaemonHealthWarningView, DaemonProviderReadinessView, DaemonProviderRouteReadinessView,
    DaemonReadinessState, DaemonRunStatusSummaryView, DaemonScheduleStatusSummaryView,
    DaemonSessionStatusSummaryView, DaemonStateRootLockStatusView, DaemonStatusProbeState,
    DaemonStatusView, DaemonStorageProbeView, DaemonStorageStatusView, DaemonTaskStatusSummaryView,
    DeliveryQueueStatusView, HookStatusView, ResolvedModelRoute, RouteDiagnosticSeverity,
    RunMemoryStatusView, RuntimeSettingsView,
};

use super::DaemonState;

const AUTH_EXPIRING_SOON_MS: u64 = 10 * 60 * 1_000;
const DELIVERY_WORKER_STALE_MS: u64 = 120_000;
const STATUS_WRITE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

static STATUS_WRITE_PROBE_COUNTER: AtomicU64 = AtomicU64::new(1);

impl<M> DaemonState<M>
where
    M: ModelDriver + Send + Sync + 'static,
{
    /// Builds one cheap server-side operator status snapshot.
    pub(crate) async fn status_snapshot(
        self: &std::sync::Arc<Self>,
        capabilities: DaemonCapabilities,
    ) -> DaemonStatusView {
        let snapshot_started = Instant::now();
        let now = now_ms();
        if let Err(error) = self.expire_due_user_questions(now).await {
            tracing::warn!(error = ?error, "failed to expire due user questions before status snapshot");
        }
        let ready = self.readiness();
        let runtime = self.runtime_settings().await;
        let runs = self.run_service.status_snapshot(now).await;
        let runtime_metrics = self.observer.metrics_snapshot();
        let hooks = self.hooks.status_snapshot(&runtime_metrics);
        let mut run_memory_metrics = self.run_memory.metrics();
        run_memory_metrics.injected_total = run_memory_metrics.injected_total.saturating_add(
            runtime_metrics
                .counters
                .get(RUN_MEMORY_PROMPT_INJECTED_COUNTER)
                .copied()
                .unwrap_or_default(),
        );
        run_memory_metrics.prompt_limit_omitted_total = run_memory_metrics
            .prompt_limit_omitted_total
            .saturating_add(
                runtime_metrics
                    .counters
                    .get(RUN_MEMORY_PROMPT_BUDGET_OMITTED_COUNTER)
                    .copied()
                    .unwrap_or_default(),
            );
        let mut run_memory = self
            .session_service
            .run_memory_status_snapshot(now, self.run_memory.policy(), run_memory_metrics)
            .await;
        run_memory.maintenance = self.run_memory.maintenance();
        let session_memory = crate::SessionMemoryStatusView {
            metrics: crate::SessionMemoryMetricsSnapshot {
                prompt_limit_omitted_total: runtime_metrics
                    .counters
                    .get(LEARNED_CONTEXT_PROMPT_BUDGET_OMITTED_COUNTER)
                    .copied()
                    .unwrap_or_default(),
            },
        };
        let delivery = match self.delivery_service.status_snapshot(now).await {
            Ok(delivery) => delivery,
            Err(error) => {
                let message = error.to_string();
                tracing::warn!(error = ?error, "failed to build delivery status snapshot");
                DeliveryQueueStatusView {
                    status_error_count: 1,
                    status_error: Some(message),
                    ..Default::default()
                }
            }
        };
        let mut schedules = self.schedule_service.status_snapshot(now).await;
        schedules.dispatch_worker_enabled = self.schedule_dispatch_worker_enabled;
        let agents = self.agent_status_snapshot();
        let session_status = self.session_service.operator_status_snapshot().await;
        let mut tasks = self.task_service.status_snapshot().await;
        tasks.total += session_status.task_summary.total;
        tasks.pending += session_status.task_summary.pending;
        tasks.in_progress += session_status.task_summary.in_progress;
        tasks.blocked += session_status.task_summary.blocked;
        tasks.completed += session_status.task_summary.completed;
        tasks.failed += session_status.task_summary.failed;
        tasks.cancelled += session_status.task_summary.cancelled;
        tasks.unindexed_session_count += session_status.task_summary.unindexed_session_count;
        let control_plane = self.control_plane_status_snapshot();
        let storage = self.storage_status_snapshot(now).await;
        let provider_readiness = self.provider_readiness_snapshot(now, &runtime).await;
        let events = self.events.status_snapshot();
        let debug_encryption_key_error = self
            .run_service
            .debug_store_encryption_key_error()
            .map(str::to_string);
        let debug_redaction_config_error = kheish_runtime::debug_redaction_config_error();
        let mut health = status_health_snapshot(
            now,
            ready,
            &runtime,
            &runs,
            &schedules,
            &delivery,
            &agents,
            &tasks,
            &control_plane,
            &storage,
            &run_memory,
            &provider_readiness,
            &hooks,
            &events,
            &runtime_metrics,
            debug_encryption_key_error.as_deref(),
            debug_redaction_config_error.as_deref(),
        );
        health.snapshot_duration_ms = snapshot_started.elapsed().as_millis() as u64;
        DaemonStatusView {
            snapshot_at_ms: now,
            process_id: std::process::id(),
            status: if ready {
                DaemonReadinessState::Ready
            } else {
                DaemonReadinessState::Draining
            },
            ready,
            capabilities,
            runtime,
            control_plane,
            storage,
            provider_readiness,
            health,
            hooks,
            events,
            sessions: DaemonSessionStatusSummaryView {
                total: session_status.session_count,
            },
            runs,
            run_memory,
            session_memory,
            schedules,
            delivery,
            agents,
            tasks,
        }
    }

    fn control_plane_status_snapshot(&self) -> DaemonControlPlaneStatusView {
        control_plane_status_snapshot_from_config(
            &self.control_plane_base_url,
            self.control_plane_bind,
            &self.control_plane_auth,
            &self.control_plane_auth_token_files,
            &self.control_plane_cors,
        )
    }

    async fn storage_status_snapshot(&self, now: u64) -> DaemonStorageStatusView {
        let state_root = self.state_root.clone();
        let workspace_root = self.workspace_root.clone();
        let state_root_handle = {
            let path = state_root.clone();
            tokio::task::spawn_blocking(move || write_probe_status("state_root", &path, now))
        };
        let workspace_root_handle = {
            let path = workspace_root.clone();
            tokio::task::spawn_blocking(move || write_probe_status("workspace_root", &path, now))
        };
        let (state_root_probe, workspace_root_probe) = tokio::join!(
            join_write_probe("state_root", &state_root, state_root_handle),
            join_write_probe("workspace_root", &workspace_root, workspace_root_handle),
        );
        let probes = vec![state_root_probe, workspace_root_probe];
        let write_error_count = probes.iter().filter(|probe| !probe.writable).count();
        DaemonStorageStatusView {
            checked_at_ms: now,
            ok: write_error_count == 0,
            write_error_count,
            probes,
            state_root_lock: Some(state_root_lock_status(
                &state_root,
                self.state_root_lock_held,
            )),
            asset_repair: asset_startup_repair_status_view(self.assets.startup_repair_report()),
        }
    }

    async fn provider_readiness_snapshot(
        &self,
        now: u64,
        runtime: &RuntimeSettingsView,
    ) -> DaemonProviderReadinessView {
        let active_route_id = runtime.route_id.as_deref();
        let mut routes = Vec::with_capacity(runtime.routes.len());
        for route in &runtime.routes {
            routes.push(
                self.provider_route_readiness(now, route, active_route_id)
                    .await,
            );
        }
        let ready_route_count = routes
            .iter()
            .filter(|route| route.state == DaemonStatusProbeState::Ok)
            .count();
        let warning_route_count = routes
            .iter()
            .filter(|route| route.state == DaemonStatusProbeState::Warning)
            .count();
        let error_route_count = routes
            .iter()
            .filter(|route| route.state == DaemonStatusProbeState::Error)
            .count();
        let active_route_ready = routes
            .iter()
            .find(|route| route.active)
            .is_some_and(|route| route.state != DaemonStatusProbeState::Error);
        DaemonProviderReadinessView {
            route_count: routes.len(),
            ready_route_count,
            warning_route_count,
            error_route_count,
            active_route_ready,
            routes,
        }
    }

    pub(crate) async fn provider_route_readiness_for_route_id(
        &self,
        route_id: &str,
    ) -> Option<DaemonProviderRouteReadinessView> {
        let runtime = self.runtime_settings().await;
        self.provider_route_readiness_for_route_id_in_runtime(route_id, &runtime)
            .await
    }

    pub(crate) async fn provider_route_readiness_for_route_id_unlocked(
        &self,
        route_id: &str,
    ) -> Option<DaemonProviderRouteReadinessView> {
        let runtime = self.runtime_settings_unlocked();
        self.provider_route_readiness_for_route_id_in_runtime(route_id, &runtime)
            .await
    }

    async fn provider_route_readiness_for_route_id_in_runtime(
        &self,
        route_id: &str,
        runtime: &RuntimeSettingsView,
    ) -> Option<DaemonProviderRouteReadinessView> {
        let active_route_id = runtime.route_id.as_deref();
        let route = runtime
            .routes
            .iter()
            .find(|route| route.route_id == route_id)?;
        Some(
            self.provider_route_readiness(now_ms(), route, active_route_id)
                .await,
        )
    }

    async fn provider_route_readiness(
        &self,
        now: u64,
        route: &ResolvedModelRoute,
        active_route_id: Option<&str>,
    ) -> DaemonProviderRouteReadinessView {
        let active = active_route_id == Some(route.route_id.as_str());
        let Some(auth_ref) = route.auth_ref.as_deref() else {
            return inline_credentials_readiness(route, active);
        };

        let slot_id = AuthSlotId::new(auth_ref.to_string());
        match self.auth_manager.status(&slot_id).await {
            Ok(Some(status)) => provider_readiness_from_auth_status(now, route, active, status),
            Ok(None) => DaemonProviderRouteReadinessView {
                route_id: route.route_id.clone(),
                provider: route.provider.clone(),
                model: route.model.clone(),
                capabilities: route.capabilities.clone(),
                active,
                auth_ref: Some(auth_ref.to_string()),
                state: DaemonStatusProbeState::Error,
                code: "route_auth_ref_missing".to_string(),
                message: format!(
                    "route `{}` references missing auth_ref `{auth_ref}`",
                    route.route_id
                ),
                action: Some(format!(
                    "create auth slot `{auth_ref}` or update the route file and restart"
                )),
                auth_mode: None,
                auth_summary: None,
                auth_updated_at_ms: None,
            },
            Err(error) => DaemonProviderRouteReadinessView {
                route_id: route.route_id.clone(),
                provider: route.provider.clone(),
                model: route.model.clone(),
                capabilities: route.capabilities.clone(),
                active,
                auth_ref: Some(auth_ref.to_string()),
                state: DaemonStatusProbeState::Error,
                code: "route_auth_status_error".to_string(),
                message: format!(
                    "route `{}` auth_ref `{auth_ref}` status failed: {error}",
                    route.route_id
                ),
                action: Some(format!(
                    "inspect auth slot `{auth_ref}` with `kheish-daemon secrets get {auth_ref}`"
                )),
                auth_mode: None,
                auth_summary: None,
                auth_updated_at_ms: None,
            },
        }
    }

    pub(crate) fn subagent_policy_status(&self) -> crate::SubagentPolicyStatusView {
        self.subagent_service
            .spawn_policy_status(&self.subagent_policy, now_ms())
    }

    fn agent_status_snapshot(&self) -> DaemonAgentStatusSummaryView {
        let supervisor = self.supervisor.status_snapshot();
        DaemonAgentStatusSummaryView {
            total: supervisor.total,
            live_runtime_count: self.orchestrator.runtime_count(),
            sidechain_count: supervisor.sidechain_count,
            closed_count: supervisor.closed_count,
            terminal_snapshot_count: supervisor.terminal_snapshot_count,
            mailbox_message_count: supervisor.mailbox_message_count,
            idle: supervisor.idle,
            running: supervisor.running,
            waiting_for_approval: supervisor.waiting_for_approval,
            waiting_for_user_input: supervisor.waiting_for_user_input,
            failed: supervisor.failed,
            completed: supervisor.completed,
            audit_sink_error_count: supervisor.audit_sink_error_count,
            last_audit_sink_error: supervisor.last_audit_sink_error,
            spawn_policy: self.subagent_policy_status(),
        }
    }
}

fn control_plane_status_snapshot_from_config(
    base_url: &str,
    bind: std::net::SocketAddr,
    auth: &crate::ControlPlaneAuthConfig,
    auth_token_files: &crate::ControlPlaneAuthTokenFiles,
    cors: &crate::ControlPlaneCorsConfig,
) -> DaemonControlPlaneStatusView {
    let admin_token_file =
        control_plane_token_file_status("admin", auth_token_files.admin_token_file.as_ref());
    let read_only_token_file = control_plane_token_file_status(
        "read_only",
        auth_token_files.read_only_token_file.as_ref(),
    );
    let auth_token_files_status = [admin_token_file.as_ref(), read_only_token_file.as_ref()]
        .into_iter()
        .flatten()
        .map(|probe| probe.view.clone())
        .collect::<Vec<_>>();
    let admin_digest = if admin_token_file.is_some() {
        admin_token_file.as_ref().and_then(|probe| probe.digest)
    } else {
        auth.admin_token.as_deref().map(crate::api::digest_token)
    };
    let read_only_digest = if read_only_token_file.is_some() {
        read_only_token_file.as_ref().and_then(|probe| probe.digest)
    } else {
        auth.read_only_token
            .as_deref()
            .map(crate::api::digest_token)
    };
    let auth_duplicate_token = admin_digest
        .as_ref()
        .zip(read_only_digest.as_ref())
        .is_some_and(|(admin, read_only)| admin.ct_eq(read_only).into());
    let auth_enabled = auth.is_enabled() || !auth_token_files.is_empty();
    let read_only_token_enabled =
        auth.read_only_token.is_some() || auth_token_files.read_only_token_file.is_some();
    let bind_is_loopback = bind.ip().is_loopback();
    let bind_is_unspecified = bind.ip().is_unspecified();
    DaemonControlPlaneStatusView {
        base_url: base_url.to_string(),
        bind_addr: Some(bind.to_string()),
        bind_is_loopback,
        bind_is_unspecified,
        auth_enabled,
        read_only_token_enabled,
        auth_effective_admin_token_available: admin_digest.is_some() && !auth_duplicate_token,
        auth_effective_read_only_token_available: read_only_digest.is_some()
            && !auth_duplicate_token,
        auth_token_file_count: auth_token_files_status.len(),
        auth_token_file_error_count: auth_token_files_status
            .iter()
            .filter(|status| !status.token_loaded)
            .count(),
        auth_duplicate_token,
        auth_token_files: auth_token_files_status,
        cors_policy: if cors.is_loopback_policy() {
            DaemonControlPlaneCorsPolicy::Loopback
        } else {
            DaemonControlPlaneCorsPolicy::Exact
        },
        cors_allowed_origin_count: cors.allowed_origins.len(),
        externally_exposed_without_auth: !auth_enabled && !bind_is_loopback,
    }
}

#[derive(Clone, Debug)]
struct ControlPlaneTokenFileStatusProbe {
    view: DaemonControlPlaneAuthTokenFileStatusView,
    digest: Option<[u8; 32]>,
}

fn control_plane_token_file_status(
    role: &str,
    path: Option<&PathBuf>,
) -> Option<ControlPlaneTokenFileStatusProbe> {
    let path = path?;
    let path_display = path.display().to_string();
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let token = raw.trim();
            if token.is_empty() {
                return Some(ControlPlaneTokenFileStatusProbe {
                    view: DaemonControlPlaneAuthTokenFileStatusView {
                        role: role.to_string(),
                        path: path_display,
                        readable: true,
                        token_loaded: false,
                        error: Some("control-plane auth token file is empty".to_string()),
                    },
                    digest: None,
                });
            }
            Some(ControlPlaneTokenFileStatusProbe {
                view: DaemonControlPlaneAuthTokenFileStatusView {
                    role: role.to_string(),
                    path: path_display,
                    readable: true,
                    token_loaded: true,
                    error: None,
                },
                digest: Some(crate::api::digest_token(token)),
            })
        }
        Err(error) => Some(ControlPlaneTokenFileStatusProbe {
            view: DaemonControlPlaneAuthTokenFileStatusView {
                role: role.to_string(),
                path: path_display,
                readable: false,
                token_loaded: false,
                error: Some(error.to_string()),
            },
            digest: None,
        }),
    }
}

fn asset_startup_repair_status_view(
    report: crate::assets::AssetStartupRepairReport,
) -> AssetStartupRepairStatusView {
    AssetStartupRepairStatusView {
        repaired_count: report.repaired_count,
        skipped_asset_count: report.skipped_asset_count,
        skipped_raw_missing_count: report.skipped_raw_missing_count,
        skipped_raw_integrity_mismatch_count: report.skipped_raw_integrity_mismatch_count,
        invalid_tombstone_count: report.invalid_tombstone_count,
        completed_tombstone_delete_count: report.completed_tombstone_delete_count,
        restored_derived_text_count: report.restored_derived_text_count,
        restored_derived_preview_count: report.restored_derived_preview_count,
        integrity_backfilled_count: report.integrity_backfilled_count,
        diagnostics: report
            .diagnostics
            .into_iter()
            .map(|diagnostic| AssetStartupRepairDiagnosticView {
                asset_id: diagnostic.asset_id,
                kind: diagnostic.kind,
                action: diagnostic.action,
                reason: diagnostic.reason,
                uri: diagnostic.uri,
            })
            .collect(),
    }
}

fn status_health_snapshot(
    generated_at_ms: u64,
    ready: bool,
    runtime: &RuntimeSettingsView,
    runs: &DaemonRunStatusSummaryView,
    schedules: &DaemonScheduleStatusSummaryView,
    delivery: &DeliveryQueueStatusView,
    agents: &DaemonAgentStatusSummaryView,
    tasks: &DaemonTaskStatusSummaryView,
    control_plane: &DaemonControlPlaneStatusView,
    storage: &DaemonStorageStatusView,
    run_memory: &RunMemoryStatusView,
    provider_readiness: &DaemonProviderReadinessView,
    hooks: &HookStatusView,
    events: &DaemonEventStatusView,
    runtime_metrics: &MetricsSnapshot,
    debug_encryption_key_error: Option<&str>,
    debug_redaction_config_error: Option<&str>,
) -> DaemonHealthView {
    let route_error_count = runtime
        .route_diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == RouteDiagnosticSeverity::Error)
        .count();
    let route_warning_count = runtime
        .route_diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == RouteDiagnosticSeverity::Warning)
        .count();

    let mut warnings = Vec::new();
    if !ready {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Error,
            "daemon_draining",
            "daemon is draining",
            None,
            Some("wait for startup/recovery to finish before submitting new work"),
        );
    }
    if control_plane.externally_exposed_without_auth {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Error,
            "control_plane_auth_disabled_off_loopback",
            "control-plane auth is disabled on a bind address that may be reachable off-loopback",
            control_plane
                .bind_addr
                .clone()
                .or_else(|| Some(control_plane.base_url.clone())),
            Some("enable bearer auth or bind the daemon to a loopback address"),
        );
    }
    if control_plane.auth_enabled && !control_plane.auth_effective_admin_token_available {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Error,
            "control_plane_admin_auth_unavailable",
            "control-plane admin auth is configured but no effective admin token is available",
            control_plane
                .auth_token_files
                .iter()
                .find(|status| status.role == "admin" && !status.token_loaded)
                .map(|status| status.path.clone()),
            Some("repair the admin token file or restart with a valid admin bearer token"),
        );
    }
    if control_plane.auth_token_file_error_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "control_plane_auth_token_file_unavailable",
            format!(
                "{} control-plane auth token file(s) could not load a token",
                control_plane.auth_token_file_error_count
            ),
            control_plane
                .auth_token_files
                .iter()
                .find(|status| !status.token_loaded)
                .map(|status| status.path.clone()),
            Some(
                "inspect `status.control_plane.auth_token_files` and repair token-file contents or permissions",
            ),
        );
    }
    if control_plane.auth_duplicate_token {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Error,
            "control_plane_auth_duplicate_token",
            "effective admin and read-only control-plane tokens are identical",
            None,
            Some("configure distinct admin and read-only bearer tokens"),
        );
    }
    if runtime.debug_level == DebugCaptureLevel::Full {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "debug_capture_full",
            "debug capture level is full",
            None,
            Some("lower debug level after collecting the needed run bundle"),
        );
    }
    if let Some(error) = debug_encryption_key_error {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Error,
            "debug_capture_key_invalid",
            "debug capture encryption key configuration is invalid",
            Some(error.to_string()),
            Some(
                "fix KHEISH_DEBUG_CAPTURE_KEY or KHEISH_DEBUG_CAPTURE_KEY_FILE, then restart the daemon",
            ),
        );
    }
    if let Some(error) = debug_redaction_config_error {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Error,
            "debug_redaction_config_invalid",
            "debug redaction extension configuration is invalid",
            Some(error.to_string()),
            Some("fix KHEISH_DEBUG_REDACT_TOKENS_FILE before enabling redacted/full debug capture"),
        );
    }
    let debug_persist_failures = runtime_metrics
        .counters
        .get("debug_artifact_persist_failures")
        .copied()
        .unwrap_or_default();
    if debug_persist_failures > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Error,
            "debug_artifact_persist_failures",
            format!("{debug_persist_failures} debug artifact persistence failure(s)"),
            None,
            Some(
                "inspect daemon logs, verify debug storage permissions and encryption key configuration",
            ),
        );
    }
    if route_error_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Error,
            "route_diagnostics_error",
            format!("{route_error_count} route diagnostic error(s)"),
            None,
            Some("run `kheish-daemon doctor routes --check-auth` and fix the reported route"),
        );
    }
    if route_warning_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "route_diagnostics_warning",
            format!("{route_warning_count} route diagnostic warning(s)"),
            None,
            Some("run `kheish-daemon doctor routes` and review route warnings"),
        );
    }
    if provider_readiness.error_route_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Error,
            "provider_readiness_error",
            format!(
                "{} provider route(s) have readiness errors",
                provider_readiness.error_route_count
            ),
            provider_readiness
                .routes
                .iter()
                .find(|route| route.state == DaemonStatusProbeState::Error)
                .map(|route| route.route_id.clone()),
            Some("inspect `provider_readiness.routes` and repair the route auth or model config"),
        );
    }
    if provider_readiness.warning_route_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "provider_readiness_warning",
            format!(
                "{} provider route(s) have readiness warnings",
                provider_readiness.warning_route_count
            ),
            provider_readiness
                .routes
                .iter()
                .find(|route| route.state == DaemonStatusProbeState::Warning)
                .map(|route| route.route_id.clone()),
            Some("inspect `provider_readiness.routes` and refresh expiring account credentials"),
        );
    }
    if events.history_capacity > 0 && events.retained_event_count >= events.history_capacity {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Info,
            "event_replay_buffer_saturated",
            format!(
                "event replay buffer is full at {}/{} retained events",
                events.retained_event_count, events.history_capacity
            ),
            events.newest_event_id.map(|id| id.to_string()),
            Some("use SSE cursors promptly; older cursors may receive a stream_gap event"),
        );
    }
    if events.replay_gap_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Info,
            "event_replay_cursor_gaps",
            format!(
                "{} event replay subscription(s) requested cursors older than retained history",
                events.replay_gap_count
            ),
            events
                .oldest_event_id
                .and_then(|oldest| oldest.checked_sub(1))
                .map(|id| id.to_string()),
            Some(
                "inspect slow clients and increase event replay capacity if cursor gaps are frequent",
            ),
        );
    }
    if events.stream_lagged_event_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "event_stream_lagged",
            format!(
                "{} event(s) were skipped by live SSE consumers",
                events.stream_lagged_event_count
            ),
            events.newest_event_id.map(|id| id.to_string()),
            Some("inspect slow SSE clients and reconnect from the latest replay cursor"),
        );
    }
    if !provider_readiness.active_route_ready
        && provider_readiness.error_route_count == 0
        && (provider_readiness.route_count > 0 || runtime.route_id.is_some())
    {
        let active_route_id = provider_readiness
            .routes
            .iter()
            .find(|route| route.active)
            .map(|route| route.route_id.clone())
            .or_else(|| runtime.route_id.clone());
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Error,
            "provider_active_route_not_ready",
            "active provider route is not ready",
            active_route_id,
            Some(
                "inspect `provider_readiness.routes`, set a healthy default route, or repair route auth",
            ),
        );
    }
    let inline_provider_route_count = provider_readiness
        .routes
        .iter()
        .filter(|route| {
            route.code == "inline_credentials_configured"
                && auth_provider_for_route(&route.provider).is_some()
        })
        .count();
    if inline_provider_route_count > 0 {
        let first_inline_route = provider_readiness.routes.iter().find(|route| {
            route.code == "inline_credentials_configured"
                && auth_provider_for_route(&route.provider).is_some()
        });
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Info,
            "provider_inline_credentials",
            format!("{inline_provider_route_count} provider route(s) use inline credentials"),
            first_inline_route.map(|route| route.route_id.clone()),
            first_inline_route
                .and_then(|route| route.action.as_deref())
                .or(Some(
                    "move provider credentials into auth_ref slots for rotation and audit",
                )),
        );
    }
    for probe in storage.probes.iter().filter(|probe| !probe.writable) {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Error,
            "storage_write_probe_failed",
            format!("{} is not writable: {}", probe.name, probe.message),
            Some(probe.path.clone()),
            probe.action.as_deref(),
        );
    }
    if storage.asset_repair.skipped_asset_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "asset_startup_repair_skipped_assets",
            format!(
                "{} asset metadata record(s) skipped during startup repair",
                storage.asset_repair.skipped_asset_count
            ),
            storage
                .asset_repair
                .diagnostics
                .iter()
                .find(|diagnostic| diagnostic.action == "skip_asset")
                .and_then(|diagnostic| diagnostic.asset_id.clone()),
            Some(
                "inspect `status.storage.asset_repair` and restore or delete the damaged asset payloads",
            ),
        );
    } else if storage.asset_repair.repaired_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Info,
            "asset_startup_repair_performed",
            format!(
                "{} asset repair action(s) completed during startup",
                storage.asset_repair.repaired_count
            ),
            storage
                .asset_repair
                .diagnostics
                .first()
                .and_then(|diagnostic| diagnostic.asset_id.clone()),
            Some("inspect `status.storage.asset_repair` for the bounded repair summary"),
        );
    }
    let run_memory_maintenance = &run_memory.maintenance;
    if run_memory_maintenance.scan_error_count > 0 || run_memory_maintenance.prune_error_count > 0 {
        let first_diagnostic = run_memory_maintenance.diagnostics.first();
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "run_memory_maintenance_failed",
            format!(
                "run-memory maintenance reported {} scan error(s) and {} prune error(s)",
                run_memory_maintenance.scan_error_count, run_memory_maintenance.prune_error_count
            ),
            first_diagnostic.and_then(|diagnostic| {
                diagnostic
                    .path
                    .clone()
                    .or_else(|| diagnostic.run_id.clone())
            }),
            Some(
                "inspect `status.run_memory.maintenance`, repair the `run-memories/` store, then restart or reset the run-memory policy",
            ),
        );
    }
    if run_memory.stale_indexed_record_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "run_memory_stale_indexed_records",
            format!(
                "{} stale run-memory index record(s)",
                run_memory.stale_indexed_record_count
            ),
            None,
            Some(
                "trigger a run-memory read, reset the run-memory policy, or restart to prune stale pointers",
            ),
        );
    }
    if run_memory.metrics.skipped_unreadable_total > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "run_memory_unreadable_records",
            format!(
                "{} unreadable run-memory record(s) skipped",
                run_memory.metrics.skipped_unreadable_total
            ),
            None,
            Some(
                "inspect daemon logs and the `run-memories/` store for corrupt or missing records",
            ),
        );
    }
    let repaired_count = run_memory_maintenance.repair_count();
    if repaired_count > 0
        && run_memory_maintenance.scan_error_count == 0
        && run_memory_maintenance.prune_error_count == 0
    {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Info,
            "run_memory_maintenance_repaired",
            format!("{repaired_count} run-memory maintenance repair action(s) completed"),
            run_memory_maintenance
                .diagnostics
                .first()
                .and_then(|diagnostic| {
                    diagnostic
                        .path
                        .clone()
                        .or_else(|| diagnostic.run_id.clone())
                }),
            Some("inspect `status.run_memory.maintenance` for the bounded repair summary"),
        );
    }
    if runs.failed > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "failed_runs",
            format!("{} failed run(s)", runs.failed),
            None,
            Some("inspect failed runs with `kheish-daemon runs list` and `runs get <run_id>`"),
        );
    }
    if let Some(lag_ms) = runs.oldest_queued_run_age_ms
        && runs.queued_run_lag_threshold_ms > 0
        && lag_ms > runs.queued_run_lag_threshold_ms
    {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "queued_run_lag",
            format!("oldest queued run has waited {lag_ms} ms behind active session work"),
            runs.oldest_queued_run_id.clone(),
            Some(
                "inspect the active run in that session, pending approvals/questions, and live tasks before adding more work",
            ),
        );
    }
    if runs.stale_non_terminal_run_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "stale_non_terminal_runs",
            format!(
                "{} non-terminal run(s) idle longer than {} ms",
                runs.stale_non_terminal_run_count, runs.stale_non_terminal_run_threshold_ms
            ),
            runs.stale_non_terminal_run_ids.first().cloned(),
            Some(
                "inspect the related run, pending approvals/questions, and live tasks; cancel or resume the run if it is stuck",
            ),
        );
    }
    if agents.failed > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "failed_agents",
            format!("{} failed agent(s)", agents.failed),
            None,
            Some(
                "inspect failed agents with `kheish-daemon agents list` and `agents get <agent_id>`",
            ),
        );
    }
    if agents.audit_sink_error_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "agent_supervisor_audit_sink_errors",
            format!(
                "{} supervisor audit sink append failure(s)",
                agents.audit_sink_error_count
            ),
            agents.last_audit_sink_error.clone(),
            Some("repair the daemon state root so supervisor lifecycle audit remains durable"),
        );
    }
    if schedules.backoff_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "schedule_retry_backoff",
            format!(
                "{} schedule(s) are deferred by retry backoff",
                schedules.backoff_count
            ),
            None,
            Some("inspect schedule retry history and the last scheduled run error"),
        );
    }
    if let Some(lag_ms) = schedules.oldest_due_schedule_lag_ms {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "scheduler_lag",
            format!("scheduler is {lag_ms} ms behind the oldest due schedule"),
            schedules.oldest_due_schedule_id.clone(),
            Some("inspect scheduler status and active runs blocking dispatch"),
        );
    }
    if delivery.unresolved_dead_lettered > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "delivery_dead_letters",
            format!(
                "{} unresolved dead-lettered delivery(s)",
                delivery.unresolved_dead_lettered
            ),
            None,
            Some(
                "inspect `status.delivery` and replay or resolve unresolved dead-lettered deliveries",
            ),
        );
    }
    if let Some(error) = delivery.status_error.as_deref() {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Error,
            "delivery_status_unavailable",
            "delivery queue status could not inspect persisted ledgers",
            Some(error.to_string()),
            Some("inspect and repair delivery ledger files under the daemon state root"),
        );
    }
    if delivery.blocked_target_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "delivery_targets_blocked",
            format!(
                "{} delivery target(s) have ready work blocked behind an earlier retry",
                delivery.blocked_target_count
            ),
            None,
            Some("inspect delivery targets with pending retries and replay or purge stuck items"),
        );
    }
    if delivery.open_circuit_target_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "delivery_target_circuit_open",
            format!(
                "{} output delivery target circuit(s) are open after retryable failures",
                delivery.open_circuit_target_count
            ),
            None,
            Some("inspect `status.delivery` and downstream connector health before replaying"),
        );
    }
    if delivery.pending + delivery.retrying > 0 && delivery.worker_heartbeat_at_ms.is_none() {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Error,
            "delivery_worker_not_running",
            "delivery queue has pending work but no worker heartbeat",
            None,
            Some("restart the daemon and inspect delivery queue state"),
        );
    } else if let Some(lag_ms) = delivery.worker_lag_ms
        && delivery.pending + delivery.retrying > 0
        && lag_ms > DELIVERY_WORKER_STALE_MS
    {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "delivery_worker_stale",
            format!("delivery worker heartbeat is {lag_ms} ms old"),
            None,
            Some("inspect daemon logs and restart the daemon if the delivery worker is stuck"),
        );
    }
    if tasks.failed > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "failed_tasks",
            format!("{} failed task(s)", tasks.failed),
            None,
            Some("inspect failed tasks with `kheish-daemon tasks list` and `tasks get`"),
        );
    }
    if tasks.blocked > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "blocked_tasks",
            format!("{} blocked task(s)", tasks.blocked),
            None,
            Some("inspect project task dependencies and linked run state"),
        );
    }
    if tasks.unreadable_session_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "unreadable_task_sessions",
            format!(
                "{} session(s) could not be read for task status",
                tasks.unreadable_session_count
            ),
            None,
            Some("run doctor and inspect session files for corruption"),
        );
    }
    if tasks.unindexed_session_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "unindexed_task_sessions",
            format!(
                "{} session(s) do not have indexed task summaries",
                tasks.unindexed_session_count
            ),
            None,
            Some("restart the daemon or run a session/task operation to repair task summaries"),
        );
    }
    if tasks.live_background_shell_task_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Info,
            "live_background_shell_tasks",
            format!(
                "{} live background shell task(s)",
                tasks.live_background_shell_task_count
            ),
            None,
            Some("inspect live shell tasks with `kheish-daemon tasks list`"),
        );
    }
    if hooks.unresolved_dead_lettered_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "hook_dead_letters",
            format!(
                "{} unresolved hook dead-letter record(s)",
                hooks.unresolved_dead_lettered_count
            ),
            hooks.last_unresolved_dead_letter_hook.clone(),
            Some(
                "inspect `status.hooks` or resolve records with `POST /v1/runtime/hooks/dead-letter/{id}/resolve`",
            ),
        );
    }
    if hooks.dead_letter_persist_failure_count > 0 {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "hook_dead_letter_persist_failures",
            format!(
                "{} hook dead-letter persistence failure(s)",
                hooks.dead_letter_persist_failure_count
            ),
            None,
            Some("repair the daemon state root so hook failures remain inspectable"),
        );
    }
    if hooks.dead_letter_read_error.is_some() {
        push_health_warning(
            &mut warnings,
            DaemonHealthSeverity::Warning,
            "hook_dead_letter_store_unreadable",
            "hook dead-letter store could not be read",
            hooks.dead_letter_store_path.clone(),
            Some("inspect and repair the hook dead-letter store under the daemon state root"),
        );
    }

    let ok = warnings
        .iter()
        .all(|warning| warning.severity == DaemonHealthSeverity::Info);
    DaemonHealthView {
        generated_at_ms,
        snapshot_duration_ms: 0,
        ok,
        route_error_count,
        route_warning_count,
        scheduler_lag_ms: schedules.oldest_due_schedule_lag_ms,
        warnings,
    }
}

fn push_health_warning(
    warnings: &mut Vec<DaemonHealthWarningView>,
    severity: DaemonHealthSeverity,
    code: impl Into<String>,
    message: impl Into<String>,
    related_id: Option<String>,
    action: Option<&str>,
) {
    warnings.push(DaemonHealthWarningView {
        severity,
        code: code.into(),
        message: message.into(),
        related_id,
        action: action.map(ToOwned::to_owned),
    });
}

async fn join_write_probe(
    name: &str,
    path: &Path,
    handle: tokio::task::JoinHandle<DaemonStorageProbeView>,
) -> DaemonStorageProbeView {
    join_write_probe_with_timeout(name, path, handle, STATUS_WRITE_PROBE_TIMEOUT).await
}

async fn join_write_probe_with_timeout(
    name: &str,
    path: &Path,
    handle: tokio::task::JoinHandle<DaemonStorageProbeView>,
    timeout: Duration,
) -> DaemonStorageProbeView {
    match tokio::time::timeout(timeout, handle).await {
        Ok(Ok(probe)) => probe,
        Ok(Err(error)) => DaemonStorageProbeView {
            name: name.to_string(),
            path: path.display().to_string(),
            state: DaemonStatusProbeState::Error,
            writable: false,
            latency_ms: 0,
            code: "write_probe_task_failed".to_string(),
            message: error.to_string(),
            action: Some(format!(
                "restart the daemon and inspect runtime logs for `{}` status probe failures",
                name
            )),
        },
        Err(_) => DaemonStorageProbeView {
            name: name.to_string(),
            path: path.display().to_string(),
            state: DaemonStatusProbeState::Error,
            writable: false,
            latency_ms: timeout.as_millis() as u64,
            code: "write_probe_timeout".to_string(),
            message: format!("write probe exceeded {} ms", timeout.as_millis()),
            action: Some(format!(
                "inspect `{}` for stalled storage or slow mounts before relying on daemon health",
                path.display()
            )),
        },
    }
}

fn write_probe_status(name: &str, path: &Path, now: u64) -> DaemonStorageProbeView {
    let probe_id = STATUS_WRITE_PROBE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let probe_path = path.join(format!(
        ".kheish-status-write-check-{}-{now}-{probe_id}",
        std::process::id()
    ));
    let started = Instant::now();
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe_path)?;
        file.write_all(b"kheish status write probe\n")?;
        file.sync_all()?;
        drop(file);
        std::fs::remove_file(&probe_path)?;
        Ok(())
    })();
    let latency_ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(()) => DaemonStorageProbeView {
            name: name.to_string(),
            path: path.display().to_string(),
            state: DaemonStatusProbeState::Ok,
            writable: true,
            latency_ms,
            code: "write_probe_ok".to_string(),
            message: "write probe succeeded".to_string(),
            action: None,
        },
        Err(error) => {
            let _ = std::fs::remove_file(&probe_path);
            DaemonStorageProbeView {
                name: name.to_string(),
                path: path.display().to_string(),
                state: DaemonStatusProbeState::Error,
                writable: false,
                latency_ms,
                code: "write_probe_failed".to_string(),
                message: error.to_string(),
                action: Some(format!(
                    "restore write access for `{}` or restart the daemon with a writable --{}",
                    path.display(),
                    name.replace('_', "-")
                )),
            }
        }
    }
}

fn state_root_lock_status(state_root: &Path, held: bool) -> DaemonStateRootLockStatusView {
    let path = state_root.join("daemon.lock");
    DaemonStateRootLockStatusView {
        held,
        path: path.display().to_string(),
        mechanism: state_root_lock_mechanism().to_string(),
    }
}

fn state_root_lock_mechanism() -> &'static str {
    #[cfg(unix)]
    {
        "flock"
    }
    #[cfg(not(unix))]
    {
        "none"
    }
}

fn provider_readiness_from_auth_status(
    now: u64,
    route: &ResolvedModelRoute,
    active: bool,
    status: AuthSlotStatus,
) -> DaemonProviderRouteReadinessView {
    let auth_ref = status.slot_id.0.clone();
    let expected_provider = auth_provider_for_route(&route.provider);
    let mut state = DaemonStatusProbeState::Ok;
    let mut code = "route_auth_ready".to_string();
    let mut message = format!(
        "route `{}` auth_ref `{auth_ref}` is available",
        route.route_id
    );
    let mut action = None;

    if expected_provider.is_some_and(|expected| expected != status.provider) {
        state = DaemonStatusProbeState::Error;
        code = "route_auth_provider_mismatch".to_string();
        message = format!(
            "route `{}` expects provider `{}` but auth_ref `{auth_ref}` is `{}`",
            route.route_id, route.provider, status.provider
        );
        action = Some(format!(
            "replace auth_ref `{auth_ref}` with a {} credential or update the route provider",
            route.provider
        ));
    } else {
        if let Some(expires_at_ms) = status
            .details
            .get("expires_at_ms")
            .and_then(serde_json::Value::as_u64)
        {
            if expires_at_ms <= now {
                state = DaemonStatusProbeState::Error;
                code = "route_auth_expired".to_string();
                message = format!(
                    "route `{}` auth_ref `{auth_ref}` is expired",
                    route.route_id
                );
                action = Some(format!(
                    "refresh auth slot `{auth_ref}` before submitting provider work"
                ));
            } else if expires_at_ms.saturating_sub(now) <= AUTH_EXPIRING_SOON_MS {
                state = DaemonStatusProbeState::Warning;
                code = "route_auth_expiring_soon".to_string();
                message = format!(
                    "route `{}` auth_ref `{auth_ref}` expires soon",
                    route.route_id
                );
                action = Some(format!(
                    "refresh auth slot `{auth_ref}` before the current token expires"
                ));
            }
        }
        if state == DaemonStatusProbeState::Ok
            && let Some(last_refresh_outcome) = status
                .details
                .get("last_refresh_outcome")
                .and_then(serde_json::Value::as_str)
            && last_refresh_outcome != "success"
        {
            state = DaemonStatusProbeState::Warning;
            code = "route_auth_refresh_warning".to_string();
            message = format!(
                "route `{}` auth_ref `{auth_ref}` last refresh outcome was `{last_refresh_outcome}`",
                route.route_id
            );
            action = Some(format!(
                "refresh auth slot `{auth_ref}` and inspect account status"
            ));
        }
    }

    DaemonProviderRouteReadinessView {
        route_id: route.route_id.clone(),
        provider: route.provider.clone(),
        model: route.model.clone(),
        capabilities: route.capabilities.clone(),
        active,
        auth_ref: Some(auth_ref),
        state,
        code,
        message,
        action,
        auth_mode: Some(status.mode),
        auth_summary: Some(status.summary),
        auth_updated_at_ms: Some(status.updated_at_ms),
    }
}

fn inline_credentials_readiness(
    route: &ResolvedModelRoute,
    active: bool,
) -> DaemonProviderRouteReadinessView {
    let provider_uses_auth = auth_provider_for_route(&route.provider).is_some();
    DaemonProviderRouteReadinessView {
        route_id: route.route_id.clone(),
        provider: route.provider.clone(),
        model: route.model.clone(),
        capabilities: route.capabilities.clone(),
        active,
        auth_ref: None,
        state: DaemonStatusProbeState::Ok,
        code: "inline_credentials_configured".to_string(),
        message: format!(
            "route `{}` uses inline credentials accepted at daemon startup",
            route.route_id
        ),
        action: provider_uses_auth.then(|| {
            "move provider credentials into an auth_ref slot for rotation and audit".to_string()
        }),
        auth_mode: None,
        auth_summary: None,
        auth_updated_at_ms: None,
    }
}

fn auth_provider_for_route(provider: &str) -> Option<AuthProvider> {
    match provider {
        "anthropic" => Some(AuthProvider::Anthropic),
        "google" => Some(AuthProvider::Google),
        "openai" => Some(AuthProvider::OpenAi),
        "openrouter" => Some(AuthProvider::OpenRouter),
        "xai" => Some(AuthProvider::XAi),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::RouteDiagnosticView;
    use anyhow::Result;
    use kheish_auth::{AuthMode, AuthProvider, AuthSlotId};
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn status_health_snapshot_reports_operator_warnings() {
        let runtime = RuntimeSettingsView {
            debug_level: DebugCaptureLevel::Full,
            route_diagnostics: vec![RouteDiagnosticView {
                severity: RouteDiagnosticSeverity::Error,
                code: "missing_auth".to_string(),
                route_id: Some("openai".to_string()),
                message: "route auth is missing".to_string(),
            }],
            ..RuntimeSettingsView::default()
        };
        let runs = DaemonRunStatusSummaryView {
            failed: 2,
            oldest_queued_run_id: Some("run-queued".to_string()),
            oldest_queued_run_age_ms: Some(250),
            queued_run_lag_threshold_ms: 100,
            stale_non_terminal_run_threshold_ms: 100,
            stale_non_terminal_run_count: 1,
            stale_non_terminal_run_ids: vec!["run-stale".to_string()],
            ..Default::default()
        };
        let schedules = DaemonScheduleStatusSummaryView {
            backoff_count: 1,
            oldest_due_schedule_id: Some("schedule-1".to_string()),
            oldest_due_schedule_lag_ms: Some(250),
            ..Default::default()
        };
        let delivery = DeliveryQueueStatusView {
            pending: 1,
            retrying: 1,
            dead_lettered: 2,
            unresolved_dead_lettered: 2,
            ready: 1,
            dispatchable: 0,
            blocked_target_count: 1,
            open_circuit_target_count: 1,
            worker_heartbeat_at_ms: Some(1),
            worker_lag_ms: Some(DELIVERY_WORKER_STALE_MS + 1),
            status_error: Some("ledger permission denied".to_string()),
            ..Default::default()
        };
        let agents = DaemonAgentStatusSummaryView {
            failed: 1,
            ..Default::default()
        };
        let tasks = DaemonTaskStatusSummaryView {
            live_background_shell_task_count: 1,
            failed: 1,
            blocked: 1,
            unreadable_session_count: 1,
            unindexed_session_count: 1,
            ..Default::default()
        };
        let control_plane = DaemonControlPlaneStatusView {
            base_url: "http://10.0.0.1:4000".to_string(),
            externally_exposed_without_auth: true,
            ..Default::default()
        };
        let storage = DaemonStorageStatusView {
            checked_at_ms: 123,
            ok: false,
            write_error_count: 1,
            probes: vec![DaemonStorageProbeView {
                name: "state_root".to_string(),
                path: "/missing-state".to_string(),
                state: DaemonStatusProbeState::Error,
                writable: false,
                latency_ms: 1,
                code: "write_probe_failed".to_string(),
                message: "permission denied".to_string(),
                action: Some("restore write access".to_string()),
            }],
            state_root_lock: None,
            asset_repair: AssetStartupRepairStatusView::default(),
        };
        let provider_readiness = DaemonProviderReadinessView {
            route_count: 1,
            error_route_count: 1,
            routes: vec![DaemonProviderRouteReadinessView {
                route_id: "openai".to_string(),
                provider: "openai".to_string(),
                model: "gpt-5.4".to_string(),
                capabilities: crate::RouteCapabilities {
                    matrix_version: crate::ROUTE_CAPABILITY_MATRIX_VERSION,
                    multimodal_input: true,
                    native_web_search: true,
                    image_generation: true,
                    image_edit: true,
                    audio_generation: false,
                    transcription: true,
                },
                active: true,
                auth_ref: Some("openai.missing".to_string()),
                state: DaemonStatusProbeState::Error,
                code: "route_auth_ref_missing".to_string(),
                message: "route auth ref missing".to_string(),
                action: Some("create auth slot".to_string()),
                auth_mode: None,
                auth_summary: None,
                auth_updated_at_ms: None,
            }],
            ..Default::default()
        };
        let hooks = HookStatusView {
            dead_lettered_count: 1,
            unresolved_dead_lettered_count: 1,
            last_dead_letter_hook: Some("audit-hook".to_string()),
            last_unresolved_dead_letter_hook: Some("audit-hook".to_string()),
            dead_letter_persist_failure_count: 1,
            ..Default::default()
        };
        let events = DaemonEventStatusView {
            history_capacity: 2,
            retained_event_count: 2,
            newest_event_id: Some(7),
            replay_gap_count: 1,
            stream_lagged_event_count: 3,
            ..Default::default()
        };

        let health = status_health_snapshot(
            123,
            false,
            &runtime,
            &runs,
            &schedules,
            &delivery,
            &agents,
            &tasks,
            &control_plane,
            &storage,
            &RunMemoryStatusView::default(),
            &provider_readiness,
            &hooks,
            &events,
            &MetricsSnapshot::default(),
            None,
            None,
        );
        assert!(!health.ok);
        assert_eq!(health.generated_at_ms, 123);
        assert_eq!(health.route_error_count, 1);
        assert_eq!(health.scheduler_lag_ms, Some(250));
        let codes = health
            .warnings
            .iter()
            .map(|warning| warning.code.as_str())
            .collect::<Vec<_>>();
        assert!(codes.contains(&"daemon_draining"));
        assert!(codes.contains(&"control_plane_auth_disabled_off_loopback"));
        assert!(codes.contains(&"debug_capture_full"));
        assert!(codes.contains(&"route_diagnostics_error"));
        assert!(codes.contains(&"provider_readiness_error"));
        assert!(codes.contains(&"storage_write_probe_failed"));
        assert!(codes.contains(&"failed_runs"));
        assert!(codes.contains(&"queued_run_lag"));
        assert!(codes.contains(&"stale_non_terminal_runs"));
        assert!(codes.contains(&"failed_agents"));
        assert!(codes.contains(&"schedule_retry_backoff"));
        assert!(codes.contains(&"scheduler_lag"));
        assert!(codes.contains(&"delivery_dead_letters"));
        assert!(codes.contains(&"delivery_targets_blocked"));
        assert!(codes.contains(&"delivery_target_circuit_open"));
        assert!(codes.contains(&"delivery_status_unavailable"));
        assert!(codes.contains(&"delivery_worker_stale"));
        assert!(codes.contains(&"event_replay_buffer_saturated"));
        assert!(codes.contains(&"event_replay_cursor_gaps"));
        assert!(codes.contains(&"event_stream_lagged"));
        assert!(codes.contains(&"failed_tasks"));
        assert!(codes.contains(&"blocked_tasks"));
        assert!(codes.contains(&"unreadable_task_sessions"));
        assert!(codes.contains(&"unindexed_task_sessions"));
        assert!(codes.contains(&"live_background_shell_tasks"));
        assert!(codes.contains(&"hook_dead_letters"));
        assert!(codes.contains(&"hook_dead_letter_persist_failures"));
        assert!(
            health
                .warnings
                .iter()
                .find(|warning| warning.code == "stale_non_terminal_runs")
                .and_then(|warning| warning.action.as_deref())
                .is_some_and(|action| action.contains("cancel or resume"))
        );
    }

    #[test]
    fn status_health_snapshot_treats_info_only_as_ok() {
        let health = status_health_snapshot(
            123,
            true,
            &RuntimeSettingsView::default(),
            &DaemonRunStatusSummaryView::default(),
            &DaemonScheduleStatusSummaryView::default(),
            &DeliveryQueueStatusView::default(),
            &DaemonAgentStatusSummaryView::default(),
            &DaemonTaskStatusSummaryView {
                live_background_shell_task_count: 1,
                ..Default::default()
            },
            &DaemonControlPlaneStatusView::default(),
            &DaemonStorageStatusView {
                ok: true,
                ..Default::default()
            },
            &RunMemoryStatusView::default(),
            &DaemonProviderReadinessView::default(),
            &HookStatusView::default(),
            &DaemonEventStatusView::default(),
            &MetricsSnapshot::default(),
            None,
            None,
        );
        assert!(health.ok);
        assert_eq!(health.warnings.len(), 1);
        assert_eq!(health.warnings[0].severity, DaemonHealthSeverity::Info);
    }

    #[test]
    fn status_health_surfaces_debug_artifact_persistence_failures() {
        let health = status_health_snapshot(
            123,
            true,
            &RuntimeSettingsView::default(),
            &DaemonRunStatusSummaryView::default(),
            &DaemonScheduleStatusSummaryView::default(),
            &DeliveryQueueStatusView::default(),
            &DaemonAgentStatusSummaryView::default(),
            &DaemonTaskStatusSummaryView::default(),
            &DaemonControlPlaneStatusView::default(),
            &DaemonStorageStatusView {
                ok: true,
                ..Default::default()
            },
            &RunMemoryStatusView::default(),
            &DaemonProviderReadinessView::default(),
            &HookStatusView::default(),
            &DaemonEventStatusView::default(),
            &MetricsSnapshot {
                counters: BTreeMap::from([("debug_artifact_persist_failures".to_string(), 2)]),
            },
            None,
            None,
        );

        assert!(!health.ok);
        assert!(health.warnings.iter().any(|warning| {
            warning.severity == DaemonHealthSeverity::Error
                && warning.code == "debug_artifact_persist_failures"
        }));
    }

    #[test]
    fn status_health_surfaces_invalid_debug_capture_key() {
        let health = status_health_snapshot(
            123,
            true,
            &RuntimeSettingsView::default(),
            &DaemonRunStatusSummaryView::default(),
            &DaemonScheduleStatusSummaryView::default(),
            &DeliveryQueueStatusView::default(),
            &DaemonAgentStatusSummaryView::default(),
            &DaemonTaskStatusSummaryView::default(),
            &DaemonControlPlaneStatusView::default(),
            &DaemonStorageStatusView {
                ok: true,
                ..Default::default()
            },
            &RunMemoryStatusView::default(),
            &DaemonProviderReadinessView::default(),
            &HookStatusView::default(),
            &DaemonEventStatusView::default(),
            &MetricsSnapshot::default(),
            Some("KHEISH_DEBUG_CAPTURE_KEY must be exactly 32 raw bytes"),
            None,
        );

        assert!(!health.ok);
        assert!(health.warnings.iter().any(|warning| {
            warning.severity == DaemonHealthSeverity::Error
                && warning.code == "debug_capture_key_invalid"
                && warning
                    .related_id
                    .as_deref()
                    .is_some_and(|value| value.contains("KHEISH_DEBUG_CAPTURE_KEY"))
        }));
    }

    #[test]
    fn status_health_surfaces_invalid_debug_redaction_config() {
        let health = status_health_snapshot(
            123,
            true,
            &RuntimeSettingsView::default(),
            &DaemonRunStatusSummaryView::default(),
            &DaemonScheduleStatusSummaryView::default(),
            &DeliveryQueueStatusView::default(),
            &DaemonAgentStatusSummaryView::default(),
            &DaemonTaskStatusSummaryView::default(),
            &DaemonControlPlaneStatusView::default(),
            &DaemonStorageStatusView {
                ok: true,
                ..Default::default()
            },
            &RunMemoryStatusView::default(),
            &DaemonProviderReadinessView::default(),
            &HookStatusView::default(),
            &DaemonEventStatusView::default(),
            &MetricsSnapshot::default(),
            None,
            Some("failed to read debug redaction token file /missing"),
        );

        assert!(!health.ok);
        assert!(health.warnings.iter().any(|warning| {
            warning.severity == DaemonHealthSeverity::Error
                && warning.code == "debug_redaction_config_invalid"
                && warning
                    .related_id
                    .as_deref()
                    .is_some_and(|value| value.contains("redaction token file"))
        }));
    }

    #[test]
    fn status_health_surfaces_active_route_readiness_gap() {
        let health = status_health_snapshot(
            123,
            true,
            &RuntimeSettingsView {
                route_id: Some("primary".to_string()),
                ..Default::default()
            },
            &DaemonRunStatusSummaryView::default(),
            &DaemonScheduleStatusSummaryView::default(),
            &DeliveryQueueStatusView::default(),
            &DaemonAgentStatusSummaryView::default(),
            &DaemonTaskStatusSummaryView::default(),
            &DaemonControlPlaneStatusView::default(),
            &DaemonStorageStatusView {
                ok: true,
                ..Default::default()
            },
            &RunMemoryStatusView::default(),
            &DaemonProviderReadinessView {
                route_count: 1,
                ready_route_count: 1,
                warning_route_count: 0,
                error_route_count: 0,
                active_route_ready: false,
                routes: vec![DaemonProviderRouteReadinessView {
                    route_id: "secondary".to_string(),
                    provider: "openai".to_string(),
                    model: "gpt-5.4".to_string(),
                    active: false,
                    state: DaemonStatusProbeState::Ok,
                    code: "inline_credentials_configured".to_string(),
                    message: "route auth is ready".to_string(),
                    ..Default::default()
                }],
            },
            &HookStatusView::default(),
            &DaemonEventStatusView::default(),
            &MetricsSnapshot::default(),
            None,
            None,
        );

        assert!(!health.ok);
        assert!(health.warnings.iter().any(|warning| {
            warning.severity == DaemonHealthSeverity::Error
                && warning.code == "provider_active_route_not_ready"
                && warning.related_id.as_deref() == Some("primary")
        }));
    }

    #[test]
    fn status_health_surfaces_asset_startup_repairs() {
        let mut storage = DaemonStorageStatusView {
            ok: true,
            asset_repair: AssetStartupRepairStatusView {
                repaired_count: 2,
                restored_derived_text_count: 1,
                restored_derived_preview_count: 1,
                diagnostics: vec![AssetStartupRepairDiagnosticView {
                    asset_id: Some("asset-1".to_string()),
                    kind: "derived_text".to_string(),
                    action: "restore".to_string(),
                    reason: "derived_text_missing".to_string(),
                    uri: Some("asset://text/asset-1.txt".to_string()),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let health = status_health_snapshot(
            123,
            true,
            &RuntimeSettingsView::default(),
            &DaemonRunStatusSummaryView::default(),
            &DaemonScheduleStatusSummaryView::default(),
            &DeliveryQueueStatusView::default(),
            &DaemonAgentStatusSummaryView::default(),
            &DaemonTaskStatusSummaryView::default(),
            &DaemonControlPlaneStatusView::default(),
            &storage,
            &RunMemoryStatusView::default(),
            &DaemonProviderReadinessView::default(),
            &HookStatusView::default(),
            &DaemonEventStatusView::default(),
            &MetricsSnapshot::default(),
            None,
            None,
        );
        assert!(health.ok);
        assert!(health.warnings.iter().any(|warning| {
            warning.severity == DaemonHealthSeverity::Info
                && warning.code == "asset_startup_repair_performed"
                && warning.related_id.as_deref() == Some("asset-1")
        }));

        storage.asset_repair.skipped_asset_count = 1;
        storage.asset_repair.skipped_raw_missing_count = 1;
        storage.asset_repair.diagnostics = vec![AssetStartupRepairDiagnosticView {
            asset_id: Some("asset-2".to_string()),
            kind: "raw".to_string(),
            action: "skip_asset".to_string(),
            reason: "raw_payload_missing".to_string(),
            uri: Some("asset://raw/asset-2.txt".to_string()),
        }];
        let health = status_health_snapshot(
            123,
            true,
            &RuntimeSettingsView::default(),
            &DaemonRunStatusSummaryView::default(),
            &DaemonScheduleStatusSummaryView::default(),
            &DeliveryQueueStatusView::default(),
            &DaemonAgentStatusSummaryView::default(),
            &DaemonTaskStatusSummaryView::default(),
            &DaemonControlPlaneStatusView::default(),
            &storage,
            &RunMemoryStatusView::default(),
            &DaemonProviderReadinessView::default(),
            &HookStatusView::default(),
            &DaemonEventStatusView::default(),
            &MetricsSnapshot::default(),
            None,
            None,
        );
        assert!(!health.ok);
        assert!(health.warnings.iter().any(|warning| {
            warning.severity == DaemonHealthSeverity::Warning
                && warning.code == "asset_startup_repair_skipped_assets"
                && warning.related_id.as_deref() == Some("asset-2")
        }));
    }

    #[test]
    fn status_health_surfaces_run_memory_maintenance() {
        let mut run_memory = RunMemoryStatusView {
            maintenance: crate::RunMemoryMaintenanceStatusView {
                checked_at_ms: 123,
                source: Some("startup".to_string()),
                index_rebuilt: true,
                pruned_orphan_file_count: 2,
                diagnostics: vec![crate::RunMemoryMaintenanceDiagnosticView {
                    action: "delete_run_memory_file".to_string(),
                    reason: "orphan_file".to_string(),
                    run_id: None,
                    path: Some("/state/run-memories/__safe/id-x.json".to_string()),
                    message: "orphan run-memory file deleted".to_string(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let health = status_health_snapshot(
            123,
            true,
            &RuntimeSettingsView::default(),
            &DaemonRunStatusSummaryView::default(),
            &DaemonScheduleStatusSummaryView::default(),
            &DeliveryQueueStatusView::default(),
            &DaemonAgentStatusSummaryView::default(),
            &DaemonTaskStatusSummaryView::default(),
            &DaemonControlPlaneStatusView::default(),
            &DaemonStorageStatusView {
                ok: true,
                ..Default::default()
            },
            &run_memory,
            &DaemonProviderReadinessView::default(),
            &HookStatusView::default(),
            &DaemonEventStatusView::default(),
            &MetricsSnapshot::default(),
            None,
            None,
        );
        assert!(health.ok);
        assert!(health.warnings.iter().any(|warning| {
            warning.severity == DaemonHealthSeverity::Info
                && warning.code == "run_memory_maintenance_repaired"
                && warning.related_id.as_deref() == Some("/state/run-memories/__safe/id-x.json")
        }));

        run_memory.maintenance.scan_error_count = 1;
        run_memory.maintenance.diagnostics = vec![crate::RunMemoryMaintenanceDiagnosticView {
            action: "scan_run_memory_store".to_string(),
            reason: "scan_failed".to_string(),
            run_id: None,
            path: Some("/state/run-memories/__safe".to_string()),
            message: "not a directory".to_string(),
        }];
        let health = status_health_snapshot(
            123,
            true,
            &RuntimeSettingsView::default(),
            &DaemonRunStatusSummaryView::default(),
            &DaemonScheduleStatusSummaryView::default(),
            &DeliveryQueueStatusView::default(),
            &DaemonAgentStatusSummaryView::default(),
            &DaemonTaskStatusSummaryView::default(),
            &DaemonControlPlaneStatusView::default(),
            &DaemonStorageStatusView {
                ok: true,
                ..Default::default()
            },
            &run_memory,
            &DaemonProviderReadinessView::default(),
            &HookStatusView::default(),
            &DaemonEventStatusView::default(),
            &MetricsSnapshot::default(),
            None,
            None,
        );
        assert!(!health.ok);
        assert!(health.warnings.iter().any(|warning| {
            warning.severity == DaemonHealthSeverity::Warning
                && warning.code == "run_memory_maintenance_failed"
                && warning.related_id.as_deref() == Some("/state/run-memories/__safe")
        }));
    }

    #[test]
    fn status_health_surfaces_unreadable_and_stale_run_memory() {
        let run_memory = RunMemoryStatusView {
            stale_indexed_record_count: 1,
            metrics: crate::RunMemoryMetricsSnapshot {
                skipped_unreadable_total: 2,
                ..Default::default()
            },
            ..Default::default()
        };
        let health = status_health_snapshot(
            123,
            true,
            &RuntimeSettingsView::default(),
            &DaemonRunStatusSummaryView::default(),
            &DaemonScheduleStatusSummaryView::default(),
            &DeliveryQueueStatusView::default(),
            &DaemonAgentStatusSummaryView::default(),
            &DaemonTaskStatusSummaryView::default(),
            &DaemonControlPlaneStatusView::default(),
            &DaemonStorageStatusView {
                ok: true,
                ..Default::default()
            },
            &run_memory,
            &DaemonProviderReadinessView::default(),
            &HookStatusView::default(),
            &DaemonEventStatusView::default(),
            &MetricsSnapshot::default(),
            None,
            None,
        );
        assert!(!health.ok);
        let codes = health
            .warnings
            .iter()
            .map(|warning| warning.code.as_str())
            .collect::<Vec<_>>();
        assert!(codes.contains(&"run_memory_stale_indexed_records"));
        assert!(codes.contains(&"run_memory_unreadable_records"));
    }

    #[test]
    fn control_plane_status_detects_non_loopback_bind_without_auth() {
        let status = control_plane_status_snapshot_from_config(
            "http://127.0.0.1:4000",
            "0.0.0.0:4000".parse().expect("bind address"),
            &crate::ControlPlaneAuthConfig::disabled(),
            &crate::ControlPlaneAuthTokenFiles::default(),
            &crate::ControlPlaneCorsConfig::loopback(),
        );

        assert_eq!(status.base_url, "http://127.0.0.1:4000");
        assert_eq!(status.bind_addr.as_deref(), Some("0.0.0.0:4000"));
        assert!(!status.bind_is_loopback);
        assert!(status.bind_is_unspecified);
        assert!(status.externally_exposed_without_auth);

        let health = status_health_snapshot(
            123,
            true,
            &RuntimeSettingsView::default(),
            &DaemonRunStatusSummaryView::default(),
            &DaemonScheduleStatusSummaryView::default(),
            &DeliveryQueueStatusView::default(),
            &DaemonAgentStatusSummaryView::default(),
            &DaemonTaskStatusSummaryView::default(),
            &status,
            &DaemonStorageStatusView {
                ok: true,
                ..Default::default()
            },
            &RunMemoryStatusView::default(),
            &DaemonProviderReadinessView::default(),
            &HookStatusView::default(),
            &DaemonEventStatusView::default(),
            &MetricsSnapshot::default(),
            None,
            None,
        );
        assert!(!health.ok);
        assert!(health.warnings.iter().any(|warning| {
            warning.code == "control_plane_auth_disabled_off_loopback"
                && warning.related_id.as_deref() == Some("0.0.0.0:4000")
        }));
    }

    #[test]
    fn control_plane_status_counts_file_backed_auth_sources() -> Result<()> {
        let temp = tempdir()?;
        let admin_path = temp.path().join("admin.token");
        let read_only_path = temp.path().join("readonly.token");
        std::fs::write(&admin_path, "admin-secret\n")?;
        std::fs::write(&read_only_path, "\n")?;
        let status = control_plane_status_snapshot_from_config(
            "http://127.0.0.1:4000",
            "0.0.0.0:4000".parse().expect("bind address"),
            &crate::ControlPlaneAuthConfig::disabled(),
            &crate::ControlPlaneAuthTokenFiles {
                admin_token_file: Some(admin_path.clone()),
                read_only_token_file: Some(read_only_path.clone()),
            },
            &crate::ControlPlaneCorsConfig::loopback(),
        );

        assert!(status.auth_enabled);
        assert!(status.read_only_token_enabled);
        assert!(status.auth_effective_admin_token_available);
        assert!(!status.auth_effective_read_only_token_available);
        assert_eq!(status.auth_token_file_count, 2);
        assert_eq!(status.auth_token_file_error_count, 1);
        assert!(!status.auth_duplicate_token);
        assert_eq!(status.auth_token_files.len(), 2);
        assert!(status.auth_token_files.iter().any(|file| {
            file.role == "read_only"
                && file.path == read_only_path.display().to_string()
                && file.readable
                && !file.token_loaded
                && file
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("empty"))
        }));
        assert!(!status.externally_exposed_without_auth);
        Ok(())
    }

    #[test]
    fn control_plane_status_detects_effective_duplicate_tokens() -> Result<()> {
        let temp = tempdir()?;
        let admin_path = temp.path().join("admin.token");
        let read_only_path = temp.path().join("readonly.token");
        std::fs::write(&admin_path, "same-secret\n")?;
        std::fs::write(&read_only_path, "same-secret\n")?;
        let status = control_plane_status_snapshot_from_config(
            "http://127.0.0.1:4000",
            "127.0.0.1:4000".parse().expect("bind address"),
            &crate::ControlPlaneAuthConfig::disabled(),
            &crate::ControlPlaneAuthTokenFiles {
                admin_token_file: Some(admin_path),
                read_only_token_file: Some(read_only_path),
            },
            &crate::ControlPlaneCorsConfig::loopback(),
        );

        assert!(status.auth_duplicate_token);
        assert!(!status.auth_effective_admin_token_available);
        assert!(!status.auth_effective_read_only_token_available);

        let health = status_health_snapshot(
            123,
            true,
            &RuntimeSettingsView::default(),
            &DaemonRunStatusSummaryView::default(),
            &DaemonScheduleStatusSummaryView::default(),
            &DeliveryQueueStatusView::default(),
            &DaemonAgentStatusSummaryView::default(),
            &DaemonTaskStatusSummaryView::default(),
            &status,
            &DaemonStorageStatusView {
                ok: true,
                ..Default::default()
            },
            &RunMemoryStatusView::default(),
            &DaemonProviderReadinessView::default(),
            &HookStatusView::default(),
            &DaemonEventStatusView::default(),
            &MetricsSnapshot::default(),
            None,
            None,
        );
        assert!(!health.ok);
        assert!(health.warnings.iter().any(|warning| {
            warning.severity == DaemonHealthSeverity::Error
                && warning.code == "control_plane_auth_duplicate_token"
        }));
        Ok(())
    }

    #[test]
    fn provider_readiness_reports_auth_expiry_and_mismatch() {
        let route = ResolvedModelRoute {
            route_id: "primary".to_string(),
            provider: "openai".to_string(),
            model: "gpt-5.4".to_string(),
            auth_ref: Some("openai.primary".to_string()),
            capabilities: crate::RouteCapabilities {
                matrix_version: crate::ROUTE_CAPABILITY_MATRIX_VERSION,
                multimodal_input: true,
                native_web_search: true,
                image_generation: true,
                image_edit: true,
                audio_generation: false,
                transcription: true,
            },
        };
        let expired = provider_readiness_from_auth_status(
            10_000,
            &route,
            true,
            AuthSlotStatus {
                slot_id: AuthSlotId::new("openai.primary"),
                provider: AuthProvider::OpenAi,
                mode: AuthMode::OAuthAccount,
                summary: "codex_account".to_string(),
                updated_at_ms: 1,
                details: BTreeMap::from([("expires_at_ms".to_string(), json!(9_999))]),
            },
        );
        assert_eq!(expired.state, DaemonStatusProbeState::Error);
        assert_eq!(expired.code, "route_auth_expired");
        assert!(expired.active);
        assert_eq!(expired.auth_mode, Some(AuthMode::OAuthAccount));
        assert!(
            expired
                .action
                .as_deref()
                .is_some_and(|action| { action.contains("refresh auth slot `openai.primary`") })
        );
        let expired_with_refresh_warning = provider_readiness_from_auth_status(
            10_000,
            &route,
            true,
            AuthSlotStatus {
                slot_id: AuthSlotId::new("openai.primary"),
                provider: AuthProvider::OpenAi,
                mode: AuthMode::OAuthAccount,
                summary: "codex_account".to_string(),
                updated_at_ms: 1,
                details: BTreeMap::from([
                    ("expires_at_ms".to_string(), json!(9_999)),
                    ("last_refresh_outcome".to_string(), json!("failed")),
                ]),
            },
        );
        assert_eq!(
            expired_with_refresh_warning.state,
            DaemonStatusProbeState::Error
        );
        assert_eq!(expired_with_refresh_warning.code, "route_auth_expired");

        let mismatch = provider_readiness_from_auth_status(
            10_000,
            &route,
            false,
            AuthSlotStatus {
                slot_id: AuthSlotId::new("anthropic.primary"),
                provider: AuthProvider::Anthropic,
                mode: AuthMode::ApiKey,
                summary: "api_key".to_string(),
                updated_at_ms: 1,
                details: BTreeMap::new(),
            },
        );
        assert_eq!(mismatch.state, DaemonStatusProbeState::Error);
        assert_eq!(mismatch.code, "route_auth_provider_mismatch");
        assert_eq!(mismatch.auth_ref.as_deref(), Some("anthropic.primary"));
    }

    #[test]
    fn inline_credentials_readiness_marks_real_providers_ok_with_rotation_action() {
        let mut route = ResolvedModelRoute {
            route_id: "primary".to_string(),
            provider: "openai".to_string(),
            model: "gpt-5.4".to_string(),
            auth_ref: None,
            capabilities: crate::RouteCapabilities::default(),
        };
        let openai = inline_credentials_readiness(&route, true);
        assert_eq!(openai.state, DaemonStatusProbeState::Ok);
        assert_eq!(openai.code, "inline_credentials_configured");
        assert!(
            openai
                .action
                .as_deref()
                .is_some_and(|action| action.contains("auth_ref slot"))
        );

        route.provider = "scripted".to_string();
        let scripted = inline_credentials_readiness(&route, true);
        assert_eq!(scripted.state, DaemonStatusProbeState::Ok);
        assert_eq!(scripted.action, None);
    }

    #[test]
    fn write_probe_status_reports_success_and_failure_without_leaving_files() -> Result<()> {
        let temp = tempdir()?;
        let ok = write_probe_status("state_root", temp.path(), 123);
        assert!(ok.writable);
        assert_eq!(ok.state, DaemonStatusProbeState::Ok);
        assert_eq!(ok.code, "write_probe_ok");
        let leftovers = std::fs::read_dir(temp.path())?
            .filter_map(std::result::Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".kheish-status-write-check-")
            })
            .count();
        assert_eq!(leftovers, 0);

        let missing = write_probe_status("workspace_root", &temp.path().join("missing"), 123);
        assert!(!missing.writable);
        assert_eq!(missing.state, DaemonStatusProbeState::Error);
        assert_eq!(missing.code, "write_probe_failed");
        assert!(
            missing
                .action
                .as_deref()
                .is_some_and(|action| { action.contains("--workspace-root") })
        );
        Ok(())
    }

    #[test]
    fn state_root_lock_status_uses_the_guard_state_not_marker_contents() -> Result<()> {
        let temp = tempdir()?;
        let lock_path = temp.path().join("daemon.lock");
        std::fs::write(
            &lock_path,
            format!("pid={}\nmechanism=flock\n", std::process::id()),
        )?;

        let not_acquired = state_root_lock_status(temp.path(), false);
        assert!(!not_acquired.held);
        assert_eq!(not_acquired.path, lock_path.display().to_string());

        std::fs::remove_file(&lock_path)?;
        let acquired = state_root_lock_status(temp.path(), true);
        assert!(acquired.held);
        assert_eq!(acquired.path, lock_path.display().to_string());
        Ok(())
    }

    #[tokio::test]
    async fn join_write_probe_reports_timeout_without_waiting_for_blocking_task() -> Result<()> {
        let temp = tempdir()?;
        let started = Instant::now();
        let handle = tokio::task::spawn_blocking(|| {
            std::thread::sleep(Duration::from_millis(100));
            DaemonStorageProbeView {
                name: "state_root".to_string(),
                state: DaemonStatusProbeState::Ok,
                writable: true,
                code: "write_probe_ok".to_string(),
                ..Default::default()
            }
        });
        let probe = join_write_probe_with_timeout(
            "state_root",
            temp.path(),
            handle,
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(probe.state, DaemonStatusProbeState::Error);
        assert_eq!(probe.code, "write_probe_timeout");
        assert!(!probe.writable);
        assert!(
            started.elapsed() < Duration::from_millis(80),
            "timeout join waited for the blocking task to finish"
        );
        Ok(())
    }

    #[tokio::test]
    async fn status_write_probe_timeouts_are_parallelizable() -> Result<()> {
        let temp = tempdir()?;
        let left_path = temp.path().join("left");
        let right_path = temp.path().join("right");
        let left = tokio::task::spawn_blocking(|| {
            std::thread::sleep(Duration::from_millis(100));
            DaemonStorageProbeView {
                name: "state_root".to_string(),
                state: DaemonStatusProbeState::Ok,
                writable: true,
                code: "write_probe_ok".to_string(),
                ..Default::default()
            }
        });
        let right = tokio::task::spawn_blocking(|| {
            std::thread::sleep(Duration::from_millis(100));
            DaemonStorageProbeView {
                name: "workspace_root".to_string(),
                state: DaemonStatusProbeState::Ok,
                writable: true,
                code: "write_probe_ok".to_string(),
                ..Default::default()
            }
        });

        let started = Instant::now();
        let (left_probe, right_probe) = tokio::join!(
            join_write_probe_with_timeout(
                "state_root",
                &left_path,
                left,
                Duration::from_millis(10),
            ),
            join_write_probe_with_timeout(
                "workspace_root",
                &right_path,
                right,
                Duration::from_millis(10),
            ),
        );
        let elapsed = started.elapsed();

        assert_eq!(left_probe.code, "write_probe_timeout");
        assert_eq!(right_probe.code, "write_probe_timeout");
        assert!(
            elapsed < Duration::from_millis(80),
            "parallel probe timeout took {elapsed:?}"
        );
        Ok(())
    }
}
