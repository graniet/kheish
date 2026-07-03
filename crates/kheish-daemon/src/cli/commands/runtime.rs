//! Runtime and control-plane command handlers.

use std::collections::BTreeSet;
use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use futures_util::future::join_all;
use kheish_auth::{AuthProvider, AuthSlotId};
use kheish_runtime::{ModelGenerationConfig, ToolChoice};
use serde_json::Value;

const ROUTE_CANARY_TOKEN: &str = "KHEISH_ROUTE_CANARY_OK";
const ROUTE_CANARY_SOURCE_PLUGIN: &str = "doctor";
const ROUTE_CANARY_SOURCE_KIND: &str = "route_canary";
const ROUTE_CANARY_ACTOR_ID: &str = "doctor";
const ROUTE_CANARY_POLL_INTERVAL_MS: u64 = 250;

/// Typed status decode failure so Doctor keeps exit-code 8 without string matching.
#[derive(Debug)]
pub(crate) struct DaemonStatusDecodeError {
    message: String,
}

impl DaemonStatusDecodeError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DaemonStatusDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DaemonStatusDecodeError {}

/// Fetches the daemon status snapshot, with a compatibility path for older daemons.
pub(crate) async fn fetch_daemon_status(
    client: &crate::cli::DaemonHttpClient,
) -> Result<kheish_daemon::DaemonStatusView> {
    let raw_status = client.get_json::<Value>("/v1/status").await?;
    match serde_json::from_value::<kheish_daemon::DaemonStatusView>(raw_status.clone()) {
        Ok(status) => Ok(status),
        Err(decode_error) => match fetch_legacy_daemon_status(client, raw_status).await {
            Ok(status) => Ok(status),
            Err(legacy_error) => Err(DaemonStatusDecodeError::new(format!(
                "failed to decode daemon status response: {decode_error}; legacy fallback failed: {legacy_error:#}"
            ))
            .into()),
        },
    }
}

async fn fetch_legacy_daemon_status(
    client: &crate::cli::DaemonHttpClient,
    raw_status: Value,
) -> Result<kheish_daemon::DaemonStatusView> {
    if !is_legacy_status_shape(&raw_status) {
        bail!("daemon status response is not the legacy status shape");
    }

    let capabilities = match legacy_status_capabilities(&raw_status)? {
        Some(capabilities) => capabilities,
        None => {
            client
                .get_json::<kheish_daemon::DaemonCapabilities>("/v1/capabilities")
                .await?
        }
    };
    let (runtime, sessions) = tokio::try_join!(
        client.get_json::<kheish_daemon::RuntimeSettingsView>("/v1/runtime"),
        client.get_json::<Vec<kheish_daemon::SessionViewSummary>>("/v1/sessions"),
    )?;
    legacy_daemon_status_view(raw_status, capabilities, runtime, sessions)
}

fn is_legacy_status_shape(raw_status: &Value) -> bool {
    let Some(object) = raw_status.as_object() else {
        return false;
    };
    if !object
        .keys()
        .all(|key| matches!(key.as_str(), "status" | "ready" | "capabilities"))
    {
        return false;
    }
    if !raw_status
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| matches!(status, "ready" | "draining"))
    {
        return false;
    }
    raw_status
        .get("ready")
        .map(Value::is_boolean)
        .unwrap_or(true)
}

fn legacy_status_capabilities(
    raw_status: &Value,
) -> Result<Option<kheish_daemon::DaemonCapabilities>> {
    raw_status
        .get("capabilities")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .context("legacy status response did not contain valid capabilities")
}

fn legacy_daemon_status_view(
    raw_status: Value,
    capabilities: kheish_daemon::DaemonCapabilities,
    runtime: kheish_daemon::RuntimeSettingsView,
    sessions: Vec<kheish_daemon::SessionViewSummary>,
) -> Result<kheish_daemon::DaemonStatusView> {
    let ready = raw_status
        .get("ready")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| raw_status.get("status").and_then(Value::as_str) == Some("ready"));
    let pending_approval_count = sessions
        .iter()
        .map(|session| session.pending_approvals)
        .sum();
    let pending_question_count = sessions
        .iter()
        .map(|session| session.pending_questions)
        .sum();
    Ok(kheish_daemon::DaemonStatusView {
        snapshot_at_ms: 0,
        process_id: 0,
        status: if ready {
            kheish_daemon::DaemonReadinessState::Ready
        } else {
            kheish_daemon::DaemonReadinessState::Draining
        },
        ready,
        capabilities,
        runtime,
        control_plane: kheish_daemon::DaemonControlPlaneStatusView::default(),
        storage: kheish_daemon::DaemonStorageStatusView::default(),
        provider_readiness: kheish_daemon::DaemonProviderReadinessView::default(),
        health: kheish_daemon::DaemonHealthView {
            ok: false,
            warnings: vec![kheish_daemon::DaemonHealthWarningView {
                severity: kheish_daemon::DaemonHealthSeverity::Warning,
                code: "legacy_status_snapshot".to_string(),
                message: "status was reconstructed from a legacy daemon response".to_string(),
                related_id: None,
                action: None,
            }],
            ..Default::default()
        },
        hooks: kheish_daemon::HookStatusView::default(),
        events: kheish_daemon::DaemonEventStatusView::default(),
        sessions: kheish_daemon::DaemonSessionStatusSummaryView {
            total: sessions.len(),
        },
        runs: kheish_daemon::DaemonRunStatusSummaryView {
            pending_approval_count,
            pending_question_count,
            ..Default::default()
        },
        run_memory: kheish_daemon::RunMemoryStatusView::default(),
        session_memory: kheish_daemon::SessionMemoryStatusView::default(),
        schedules: kheish_daemon::DaemonScheduleStatusSummaryView::default(),
        delivery: kheish_daemon::DeliveryQueueStatusView::default(),
        agents: kheish_daemon::DaemonAgentStatusSummaryView::default(),
        tasks: kheish_daemon::DaemonTaskStatusSummaryView::default(),
    })
}

/// Handles `doctor`.
pub(crate) async fn run_doctor(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    cors_origin: Option<String>,
) -> Result<()> {
    let (status_result, readyz_result, events_stream_result) = tokio::join!(
        fetch_daemon_status(client),
        client.probe("/readyz"),
        client.probe_sse("/v1/events/stream"),
    );
    let readyz_reachable = readyz_result.ok;
    let events_stream_reachable = events_stream_result.ok;
    let cors_probe = match cors_origin.as_deref() {
        Some(origin) => Some(client.probe_cors_preflight("/v1/status", origin).await),
        None => None,
    };
    let status = match status_result {
        Ok(status) => status,
        Err(error) => {
            let message = format!("failed to fetch daemon status: {error:#}");
            let mut checks = vec![
                crate::DoctorCheckView {
                    name: "daemon_status".to_string(),
                    code: "status_unreachable".to_string(),
                    ok: false,
                    severity: "error".to_string(),
                    message: message.clone(),
                    action: Some(
                        "verify --base-url and provide a valid KHEISH_DAEMON_TOKEN when HTTP auth is enabled"
                            .to_string(),
                    ),
                    related_id: Some(client.base_url().to_string()),
                },
                crate::DoctorCheckView {
                    name: "readyz".to_string(),
                    code: if readyz_reachable {
                        "readyz_reachable"
                    } else {
                        "readyz_unreachable"
                    }
                    .to_string(),
                    ok: readyz_reachable,
                    severity: if readyz_reachable { "info" } else { "error" }.to_string(),
                    message: readyz_result.message.clone(),
                    action: readyz_result.action.clone(),
                    related_id: readyz_result.status.map(|status| status.to_string()),
                },
                crate::DoctorCheckView {
                    name: "events_sse".to_string(),
                    code: if events_stream_reachable {
                        "events_reachable"
                    } else {
                        "events_unreachable"
                    }
                    .to_string(),
                    ok: events_stream_reachable,
                    severity: if events_stream_reachable {
                        "info"
                    } else {
                        "error"
                    }
                    .to_string(),
                    message: events_stream_result.message.clone(),
                    action: events_stream_result.action.clone(),
                    related_id: events_stream_result.status.map(|status| status.to_string()),
                },
            ];
            if let Some(cors_probe) = cors_probe.as_ref() {
                checks.push(crate::DoctorCheckView {
                    name: "control_plane_cors".to_string(),
                    code: "control_plane_cors".to_string(),
                    ok: cors_probe.ok,
                    severity: if cors_probe.ok { "info" } else { "error" }.to_string(),
                    message: cors_probe.message.clone(),
                    action: cors_probe.action.clone(),
                    related_id: cors_probe
                        .status
                        .map(|status| status.to_string())
                        .or_else(|| Some(client.base_url().to_string())),
                });
            }
            printer.print(&crate::DoctorView {
                ok: false,
                status: None,
                status_error: Some(message.clone()),
                session_count: 0,
                pending_approvals: 0,
                pending_questions: 0,
                readyz_reachable,
                events_stream_reachable,
                checks,
                warnings: Vec::new(),
                errors: vec![message],
            })?;
            return Err(error);
        }
    };
    let hook_target_probe = doctor_http_hook_target_diagnostics(&status.runtime.hooks).await;
    let report = doctor_report(
        &status,
        &readyz_result,
        &events_stream_result,
        cors_probe.as_ref(),
        Some(&hook_target_probe),
    );
    let session_count = status.sessions.total;
    let pending_approvals = status.runs.pending_approval_count;
    let pending_questions = status.runs.pending_question_count;
    printer.print(&crate::DoctorView {
        ok: report.ok,
        status: Some(redacted_doctor_status(&status)),
        status_error: None,
        session_count,
        pending_approvals,
        pending_questions,
        readyz_reachable,
        events_stream_reachable,
        checks: report.checks,
        warnings: report.warnings,
        errors: report.errors.clone(),
    })?;
    if !report.ok {
        bail!("doctor found {} error(s)", report.errors.len());
    }
    Ok(())
}

fn redacted_doctor_status(
    status: &kheish_daemon::DaemonStatusView,
) -> kheish_daemon::DaemonStatusView {
    let mut status = status.clone();
    status.runtime.hooks = redacted_doctor_hook_settings(&status.runtime.hooks);
    status
}

fn redacted_doctor_hook_settings(
    settings: &kheish_types::HookSettings,
) -> kheish_types::HookSettings {
    kheish_types::HookSettings {
        hooks: settings
            .hooks
            .iter()
            .map(|(event, hooks)| {
                (
                    event.clone(),
                    hooks
                        .iter()
                        .map(redacted_doctor_hook_definition)
                        .collect::<Vec<_>>(),
                )
            })
            .collect(),
    }
}

fn redacted_doctor_hook_definition(
    hook: &kheish_types::HookDefinition,
) -> kheish_types::HookDefinition {
    let mut hook = hook.clone();
    hook.executor = redacted_doctor_hook_executor(&hook.executor);
    hook
}

fn redacted_doctor_hook_executor(
    executor: &kheish_types::HookExecutorConfig,
) -> kheish_types::HookExecutorConfig {
    match executor {
        kheish_types::HookExecutorConfig::Command {
            shell, timeout_ms, ..
        } => kheish_types::HookExecutorConfig::Command {
            command: "<redacted>".to_string(),
            shell: shell.clone(),
            timeout_ms: *timeout_ms,
        },
        kheish_types::HookExecutorConfig::Http { url, timeout_ms } => {
            kheish_types::HookExecutorConfig::Http {
                url: redacted_doctor_http_hook_url(url),
                timeout_ms: *timeout_ms,
            }
        }
        kheish_types::HookExecutorConfig::Prompt {
            model, timeout_ms, ..
        } => kheish_types::HookExecutorConfig::Prompt {
            template: "<redacted>".to_string(),
            system_prompt: None,
            model: model.clone(),
            timeout_ms: *timeout_ms,
        },
        kheish_types::HookExecutorConfig::Agent {
            model,
            tool_surface,
            timeout_ms,
            max_turns,
            ..
        } => kheish_types::HookExecutorConfig::Agent {
            template: "<redacted>".to_string(),
            system_prompt: None,
            model: model.clone(),
            tool_surface: tool_surface.clone(),
            timeout_ms: *timeout_ms,
            max_turns: *max_turns,
        },
        kheish_types::HookExecutorConfig::Callback { name, timeout_ms } => {
            kheish_types::HookExecutorConfig::Callback {
                name: name.clone(),
                timeout_ms: *timeout_ms,
            }
        }
    }
}

fn redacted_doctor_http_hook_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(parsed) => {
            let scheme = parsed.scheme();
            let host = parsed.host_str().unwrap_or("<missing-host>");
            match parsed.port() {
                Some(port) => format!("{scheme}://{host}:{port}/<redacted>"),
                None => format!("{scheme}://{host}/<redacted>"),
            }
        }
        Err(_) => "<redacted-invalid-url>".to_string(),
    }
}

/// Handles `doctor routes`.
pub(crate) async fn run_doctor_routes(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    args: crate::DoctorRoutesArgs,
) -> Result<()> {
    let mut view = match args.routes_file.as_deref() {
        Some(path) => doctor_routes_file_view(path, args.default_route.as_deref()),
        None => {
            let status = fetch_daemon_status(client).await?;
            doctor_routes_runtime_view(&status)
        }
    };
    if let Some(route_id) = args.route.as_deref() {
        filter_doctor_routes_view(&mut view, route_id);
    }
    let canary_requires_runtime = args.canary && view.source != "runtime";
    let references_require_runtime = args.check_references && view.source != "runtime";
    let auth_check_error = if args.check_auth && !canary_requires_runtime {
        apply_route_auth_checks(client, &mut view).await.err()
    } else {
        None
    };
    let reference_check_error = if args.check_references && !references_require_runtime {
        apply_route_reference_checks(client, &mut view, args.route.as_deref())
            .await
            .err()
    } else if references_require_runtime {
        view.reference_checked = true;
        view.diagnostics.push(route_diagnostic(
            kheish_daemon::RouteDiagnosticSeverity::Error,
            "route_reference_check_requires_runtime",
            None,
            "route reference checks require the running daemon inventory; remove --routes-file",
        ));
        None
    } else {
        None
    };
    if args.canary {
        apply_route_canary_checks(client, &mut view, args.canary_timeout_ms).await;
    }
    refresh_doctor_routes_ok(&mut view);
    printer.print(&view)?;
    if let Some(error) = auth_check_error {
        return Err(error.context("doctor routes auth check failed"));
    }
    if let Some(error) = reference_check_error {
        return Err(error.context("doctor routes reference check failed"));
    }
    if !view.ok {
        bail!("doctor routes found {} error(s)", view.errors.len());
    }
    Ok(())
}

struct DoctorReport {
    ok: bool,
    checks: Vec<crate::DoctorCheckView>,
    warnings: Vec<String>,
    errors: Vec<String>,
}

fn doctor_report(
    status: &kheish_daemon::DaemonStatusView,
    readyz_probe: &crate::cli::http::DaemonProbeResult,
    events_stream_probe: &crate::cli::http::DaemonProbeResult,
    cors_probe: Option<&crate::cli::http::DaemonProbeResult>,
    hook_target_probe: Option<&HookDoctorDiagnostics>,
) -> DoctorReport {
    let checks = doctor_checks(
        status,
        readyz_probe,
        events_stream_probe,
        cors_probe,
        hook_target_probe,
    );
    let warnings = checks
        .iter()
        .filter(|check| check.severity == "warning")
        .map(|check| check.message.clone())
        .collect::<Vec<_>>();
    let errors = checks
        .iter()
        .filter(|check| check.severity == "error")
        .map(|check| check.message.clone())
        .collect::<Vec<_>>();
    DoctorReport {
        ok: errors.is_empty(),
        checks,
        warnings,
        errors,
    }
}

fn doctor_checks(
    status: &kheish_daemon::DaemonStatusView,
    readyz_probe: &crate::cli::http::DaemonProbeResult,
    events_stream_probe: &crate::cli::http::DaemonProbeResult,
    cors_probe: Option<&crate::cli::http::DaemonProbeResult>,
    hook_target_probe: Option<&HookDoctorDiagnostics>,
) -> Vec<crate::DoctorCheckView> {
    let mut checks = Vec::new();
    push_doctor_check_with_details(
        &mut checks,
        "daemon_status",
        status.ready,
        if status.ready { "info" } else { "error" },
        if status.ready {
            "daemon status is ready".to_string()
        } else {
            "daemon is draining".to_string()
        },
        (!status.ready)
            .then(|| "wait for startup/recovery to finish before submitting new work".to_string()),
        None,
    );
    push_doctor_check_with_details(
        &mut checks,
        "readyz",
        readyz_probe.ok,
        if readyz_probe.ok { "info" } else { "error" },
        readyz_probe.message.clone(),
        readyz_probe.action.clone(),
        readyz_probe.status.map(|status| status.to_string()),
    );
    push_doctor_check_with_details(
        &mut checks,
        "events_sse",
        events_stream_probe.ok,
        if events_stream_probe.ok {
            "info"
        } else {
            "error"
        },
        events_stream_probe.message.clone(),
        events_stream_probe.action.clone(),
        events_stream_probe
            .content_type
            .clone()
            .or_else(|| events_stream_probe.status.map(|status| status.to_string())),
    );
    let (
        events_status_ok,
        events_status_severity,
        events_status_message,
        events_status_action,
        events_status_related_id,
    ) = doctor_events_status_check(&status.events);
    push_doctor_check_with_details(
        &mut checks,
        "events_status",
        events_status_ok,
        events_status_severity,
        events_status_message,
        events_status_action,
        events_status_related_id,
    );
    let (auth_ok, auth_severity, auth_message, auth_action, auth_related_id) =
        doctor_control_plane_auth_check(&status.control_plane);
    push_doctor_check_with_details(
        &mut checks,
        "control_plane_auth",
        auth_ok,
        auth_severity,
        auth_message,
        auth_action,
        auth_related_id,
    );
    let cors_ok = cors_probe.map(|probe| probe.ok).unwrap_or(true);
    push_doctor_check_with_details(
        &mut checks,
        "control_plane_cors",
        cors_ok,
        if cors_ok { "info" } else { "error" },
        cors_probe.map_or_else(
            || {
                format!(
                    "control-plane CORS policy is {:?} with {} exact origin(s)",
                    status.control_plane.cors_policy,
                    status.control_plane.cors_allowed_origin_count
                )
            },
            |probe| probe.message.clone(),
        ),
        cors_probe
            .and_then(|probe| probe.action.clone())
            .or_else(|| {
                Some(match status.control_plane.cors_policy {
                    kheish_daemon::DaemonControlPlaneCorsPolicy::Loopback => {
                        "use `--cors-origin` to verify browser clients outside loopback".to_string()
                    }
                    kheish_daemon::DaemonControlPlaneCorsPolicy::Exact => {
                        "verify required browser origins with `--cors-origin`".to_string()
                    }
                })
            }),
        status.control_plane.bind_addr.clone(),
    );
    let route_errors = status
        .runtime
        .route_diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == kheish_daemon::RouteDiagnosticSeverity::Error)
        .count();
    let route_warnings = status
        .runtime
        .route_diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == kheish_daemon::RouteDiagnosticSeverity::Warning)
        .count();
    let route_inventory_ok = route_errors == 0
        && !status.runtime.routes.is_empty()
        && (status.runtime.default_route.is_some() || status.runtime.route_id.is_some());
    push_doctor_check_with_details(
        &mut checks,
        "routes",
        route_inventory_ok,
        if route_errors > 0 {
            "error"
        } else if status.runtime.routes.is_empty()
            || (status.runtime.default_route.is_none() && status.runtime.route_id.is_none())
        {
            "error"
        } else if route_warnings > 0 {
            "warning"
        } else {
            "info"
        },
        if route_errors > 0 || route_warnings > 0 {
            format!("{route_errors} route diagnostic error(s), {route_warnings} warning(s)")
        } else if status.runtime.routes.is_empty() {
            "runtime has no configured routes".to_string()
        } else if status.runtime.default_route.is_none() && status.runtime.route_id.is_none() {
            "runtime has no default route".to_string()
        } else {
            format!(
                "{} runtime route(s) configured",
                status.runtime.routes.len()
            )
        },
        if route_errors > 0 {
            Some("run `kheish-daemon doctor routes --check-auth` and fix route errors".to_string())
        } else if route_warnings > 0 {
            Some("run `kheish-daemon doctor routes` and review route warnings".to_string())
        } else if status.runtime.routes.is_empty() {
            Some("configure at least one provider route before accepting work".to_string())
        } else if status.runtime.default_route.is_none() && status.runtime.route_id.is_none() {
            Some("set a default route in the routes file or with runtime configuration".to_string())
        } else {
            None
        },
        None,
    );
    push_doctor_check_with_details(
        &mut checks,
        "provider_readiness",
        status.provider_readiness.error_route_count == 0,
        if status.provider_readiness.error_route_count > 0 {
            "error"
        } else if status.provider_readiness.warning_route_count > 0 {
            "warning"
        } else {
            "info"
        },
        if status.provider_readiness.route_count == 0 && status.runtime.routes.is_empty() {
            "no provider routes are configured".to_string()
        } else if status.provider_readiness.route_count == 0 {
            "provider readiness details are not available in this status payload".to_string()
        } else if status.provider_readiness.error_route_count > 0
            || status.provider_readiness.warning_route_count > 0
        {
            format!(
                "{} ready, {} warning, {} error provider route(s)",
                status.provider_readiness.ready_route_count,
                status.provider_readiness.warning_route_count,
                status.provider_readiness.error_route_count
            )
        } else {
            format!(
                "{} provider route(s) ready",
                status.provider_readiness.ready_route_count
            )
        },
        provider_readiness_action(status),
        provider_readiness_related_id(status),
    );
    push_doctor_check_with_details(
        &mut checks,
        "storage",
        status.storage.probes.is_empty() || status.storage.ok,
        if status.storage.probes.is_empty() {
            "info"
        } else if status.storage.ok {
            "info"
        } else {
            "error"
        },
        if status.storage.probes.is_empty() {
            "storage write-health probes are not available in this status payload".to_string()
        } else if status.storage.ok {
            format!("{} storage root(s) writable", status.storage.probes.len())
        } else {
            format!(
                "{} storage write probe(s) failed",
                status.storage.write_error_count
            )
        },
        status
            .storage
            .probes
            .iter()
            .find(|probe| !probe.writable)
            .and_then(|probe| probe.action.clone()),
        status
            .storage
            .probes
            .iter()
            .find(|probe| !probe.writable)
            .map(|probe| probe.path.clone()),
    );
    push_doctor_check_with_details(
        &mut checks,
        "state_root_lock",
        status
            .storage
            .state_root_lock
            .as_ref()
            .map(|lock| lock.held)
            .unwrap_or(true),
        if status
            .storage
            .state_root_lock
            .as_ref()
            .map(|lock| lock.held)
            .unwrap_or(true)
        {
            "info"
        } else {
            "error"
        },
        status.storage.state_root_lock.as_ref().map_or_else(
            || "state-root lock status is not available in this status payload".to_string(),
            |lock| {
                format!(
                    "state-root lock `{}` is {} via {}",
                    lock.path,
                    if lock.held { "held" } else { "not held" },
                    lock.mechanism
                )
            },
        ),
        status.storage.state_root_lock.as_ref().and_then(|lock| {
            (!lock.held).then(|| {
                "stop duplicate daemons and restart with one process per state root".to_string()
            })
        }),
        status
            .storage
            .state_root_lock
            .as_ref()
            .map(|lock| lock.path.clone()),
    );
    let run_memory_maintenance = &status.run_memory.maintenance;
    if run_memory_maintenance.repair_count() > 0
        || run_memory_maintenance.scan_error_count > 0
        || run_memory_maintenance.prune_error_count > 0
    {
        let first_diagnostic = run_memory_maintenance.diagnostics.first();
        let failed =
            run_memory_maintenance.scan_error_count + run_memory_maintenance.prune_error_count > 0;
        push_doctor_check_with_details(
            &mut checks,
            "run_memory_maintenance",
            !failed,
            if failed { "warning" } else { "info" },
            if failed {
                format!(
                    "run-memory maintenance reported {} scan error(s) and {} prune error(s)",
                    run_memory_maintenance.scan_error_count,
                    run_memory_maintenance.prune_error_count
                )
            } else {
                format!(
                    "{} run-memory maintenance repair action(s) completed",
                    run_memory_maintenance.repair_count()
                )
            },
            Some("inspect `status.run_memory.maintenance` for details".to_string()),
            first_diagnostic.and_then(|diagnostic| {
                diagnostic
                    .path
                    .clone()
                    .or_else(|| diagnostic.run_id.clone())
            }),
        );
    }
    let hooks = doctor_hook_diagnostics(&status.runtime.hooks);
    push_doctor_check_with_details(
        &mut checks,
        "hooks",
        hooks.error_count == 0,
        if hooks.error_count > 0 {
            "error"
        } else if hooks.warning_count > 0 {
            "warning"
        } else {
            "info"
        },
        hooks.message,
        hooks.action,
        hooks.related_id,
    );
    if let Some(hook_targets) = hook_target_probe.filter(|probe| probe.checked_count > 0) {
        push_doctor_check_with_details(
            &mut checks,
            "hooks_http_targets",
            hook_targets.error_count == 0,
            if hook_targets.error_count > 0 {
                "error"
            } else if hook_targets.warning_count > 0 {
                "warning"
            } else {
                "info"
            },
            hook_targets.message.clone(),
            hook_targets.action.clone(),
            hook_targets.related_id.clone(),
        );
    }
    for warning in &status.health.warnings {
        if warning.severity == kheish_daemon::DaemonHealthSeverity::Info {
            continue;
        }
        push_doctor_check_with_details(
            &mut checks,
            &warning.code,
            warning.severity != kheish_daemon::DaemonHealthSeverity::Error,
            match warning.severity {
                kheish_daemon::DaemonHealthSeverity::Info => "info",
                kheish_daemon::DaemonHealthSeverity::Warning => "warning",
                kheish_daemon::DaemonHealthSeverity::Error => "error",
            },
            warning.message.clone(),
            warning.action.clone(),
            warning.related_id.clone(),
        );
    }
    checks
}

fn doctor_events_status_check(
    events: &kheish_daemon::DaemonEventStatusView,
) -> (bool, &'static str, String, Option<String>, Option<String>) {
    if events.history_capacity == 0 && events.next_event_id == 0 && events.retained_event_count == 0
    {
        return (
            true,
            "info",
            "event replay status is not available in this status payload".to_string(),
            Some("upgrade/restart the daemon to expose event replay counters".to_string()),
            None,
        );
    }
    if events.stream_lagged_event_count > 0 {
        return (
            true,
            "warning",
            format!(
                "{} SSE event(s) were skipped by lagging live stream consumers",
                events.stream_lagged_event_count
            ),
            Some(
                "inspect slow SSE clients and proxy buffering; reconnect with replay cursors"
                    .to_string(),
            ),
            events.newest_event_id.map(|id| id.to_string()),
        );
    }
    if events.replay_gap_count > 0 {
        return (
            true,
            "warning",
            format!(
                "{} SSE replay cursor gap(s) were observed",
                events.replay_gap_count
            ),
            Some(
                "increase event history capacity or reconnect clients before cursors expire"
                    .to_string(),
            ),
            events
                .oldest_event_id
                .map(|id| id.saturating_sub(1).to_string()),
        );
    }
    if events.history_capacity > 0
        && events.replay_buffer_utilization_percent >= 90
        && events.evicted_event_count > 0
    {
        return (
            true,
            "warning",
            format!(
                "event replay buffer is {}% full with {}/{} retained event(s)",
                events.replay_buffer_utilization_percent,
                events.retained_event_count,
                events.history_capacity
            ),
            Some("increase event history capacity or reduce client reconnect windows".to_string()),
            events.newest_event_id.map(|id| id.to_string()),
        );
    }
    (
        true,
        "info",
        format!(
            "event replay retained {}/{} event(s) for {} active subscriber(s)",
            events.retained_event_count, events.history_capacity, events.subscriber_count
        ),
        None,
        events.newest_event_id.map(|id| id.to_string()),
    )
}

fn doctor_control_plane_auth_check(
    control_plane: &kheish_daemon::DaemonControlPlaneStatusView,
) -> (bool, &'static str, String, Option<String>, Option<String>) {
    let bind = control_plane
        .bind_addr
        .clone()
        .or_else(|| Some(control_plane.base_url.clone()));
    if control_plane.externally_exposed_without_auth {
        return (
            false,
            "error",
            "control-plane auth is disabled on a bind address that may be reachable off-loopback"
                .to_string(),
            Some("enable bearer auth or bind the daemon to a loopback address".to_string()),
            bind,
        );
    }
    if control_plane.auth_duplicate_token {
        return (
            false,
            "error",
            "effective admin and read-only control-plane tokens are identical".to_string(),
            Some("configure distinct admin and read-only bearer tokens".to_string()),
            bind,
        );
    }
    if control_plane.auth_token_file_error_count > 0 {
        return (
            true,
            "warning",
            format!(
                "{} control-plane auth token file(s) could not load a token",
                control_plane.auth_token_file_error_count
            ),
            Some("inspect `status.control_plane.auth_token_files` and repair token-file contents or permissions".to_string()),
            control_plane
                .auth_token_files
                .iter()
                .find(|status| !status.token_loaded)
                .map(|status| status.path.clone())
                .or(bind),
        );
    }
    if control_plane.auth_enabled {
        let source = if control_plane.auth_token_file_count > 0 {
            format!(
                " with {} token-file source(s)",
                control_plane.auth_token_file_count
            )
        } else {
            String::new()
        };
        return (
            true,
            "info",
            format!("control-plane auth is enabled{source}"),
            None,
            bind,
        );
    }
    (
        true,
        "info",
        "control-plane auth is disabled on a loopback-only bind".to_string(),
        None,
        bind,
    )
}

fn provider_readiness_action(status: &kheish_daemon::DaemonStatusView) -> Option<String> {
    status
        .provider_readiness
        .routes
        .iter()
        .find(|route| route.state == kheish_daemon::DaemonStatusProbeState::Error)
        .and_then(|route| route.action.clone())
        .or_else(|| {
            status
                .provider_readiness
                .routes
                .iter()
                .find(|route| route.state == kheish_daemon::DaemonStatusProbeState::Warning)
                .and_then(|route| route.action.clone())
        })
        .or_else(|| {
            (status.provider_readiness.route_count == 0 && !status.runtime.routes.is_empty())
                .then(|| "upgrade/restart the daemon to expose provider readiness".to_string())
        })
}

fn provider_readiness_related_id(status: &kheish_daemon::DaemonStatusView) -> Option<String> {
    status
        .provider_readiness
        .routes
        .iter()
        .find(|route| route.state == kheish_daemon::DaemonStatusProbeState::Error)
        .map(|route| route.route_id.clone())
        .or_else(|| {
            status
                .provider_readiness
                .routes
                .iter()
                .find(|route| route.state == kheish_daemon::DaemonStatusProbeState::Warning)
                .map(|route| route.route_id.clone())
        })
}

struct HookDoctorDiagnostics {
    checked_count: usize,
    error_count: usize,
    warning_count: usize,
    message: String,
    action: Option<String>,
    related_id: Option<String>,
}

fn doctor_hook_diagnostics(settings: &kheish_types::HookSettings) -> HookDoctorDiagnostics {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut total = 0usize;
    let shared_validation_error =
        kheish_daemon::validate_hook_settings(settings)
            .err()
            .map(|error| {
                (
                    "settings".to_string(),
                    format!("shared hook validation failed: {error:#}"),
                )
            });
    for (event, hooks) in &settings.hooks {
        for hook in hooks {
            total += 1;
            let hook_id = format!("{event:?}:{}", hook.name);
            if hook.name.trim().is_empty() {
                errors.push((hook_id.clone(), "hook name must not be empty".to_string()));
            }
            match &hook.executor {
                kheish_types::HookExecutorConfig::Command {
                    command,
                    shell,
                    timeout_ms,
                } => {
                    if command.trim().is_empty() {
                        errors.push((hook_id.clone(), "command hook command is empty".to_string()));
                    }
                    if let Some(error) = doctor_validate_command_hook_shell(shell.as_deref()) {
                        errors.push((hook_id.clone(), error));
                    }
                    if let Some(error) = doctor_validate_hook_timeout("command", *timeout_ms) {
                        errors.push((hook_id.clone(), error));
                    }
                }
                kheish_types::HookExecutorConfig::Http { url, timeout_ms } => {
                    if let Some(error) = doctor_validate_hook_timeout("HTTP", *timeout_ms) {
                        errors.push((hook_id.clone(), error));
                    }
                    if let Some(error) = doctor_validate_http_hook_target(url) {
                        errors.push((hook_id.clone(), error));
                    }
                }
                kheish_types::HookExecutorConfig::Prompt {
                    template,
                    timeout_ms,
                    ..
                } => {
                    if template.trim().is_empty() {
                        errors.push((hook_id.clone(), "prompt hook template is empty".to_string()));
                    }
                    if let Some(error) = doctor_validate_hook_timeout("prompt", *timeout_ms) {
                        errors.push((hook_id.clone(), error));
                    }
                }
                kheish_types::HookExecutorConfig::Agent {
                    template,
                    timeout_ms,
                    max_turns,
                    ..
                } => {
                    if template.trim().is_empty() {
                        errors.push((hook_id.clone(), "agent hook template is empty".to_string()));
                    }
                    if let Some(error) = doctor_validate_hook_timeout("agent", *timeout_ms) {
                        errors.push((hook_id.clone(), error));
                    }
                    if max_turns == &Some(0) {
                        errors.push((hook_id.clone(), "agent hook max_turns is zero".to_string()));
                    }
                }
                kheish_types::HookExecutorConfig::Callback { name, timeout_ms } => {
                    if name.trim().is_empty() {
                        errors.push((hook_id.clone(), "callback hook name is empty".to_string()));
                    } else {
                        warnings.push((
                            hook_id.clone(),
                            "callback hook availability is only verifiable at dispatch time"
                                .to_string(),
                        ));
                    }
                    if let Some(error) = doctor_validate_hook_timeout("callback", *timeout_ms) {
                        errors.push((hook_id.clone(), error));
                    }
                }
            }
        }
    }
    if errors.is_empty()
        && let Some(error) = shared_validation_error
    {
        errors.push(error);
    }
    if let Some((related_id, message)) = errors.first() {
        HookDoctorDiagnostics {
            checked_count: total,
            error_count: errors.len(),
            warning_count: warnings.len(),
            message: format!(
                "{} hook configuration error(s), {} warning(s): {message}",
                errors.len(),
                warnings.len()
            ),
            action: Some(
                "fix the hook definition and reload with `kheish-daemon runtime hooks set`"
                    .to_string(),
            ),
            related_id: Some(related_id.clone()),
        }
    } else if let Some((related_id, message)) = warnings.first() {
        HookDoctorDiagnostics {
            checked_count: total,
            error_count: 0,
            warning_count: warnings.len(),
            message: format!(
                "{total} hook(s) configured with {} static warning(s): {message}",
                warnings.len()
            ),
            action: Some("verify callback registration in the embedding process".to_string()),
            related_id: Some(related_id.clone()),
        }
    } else {
        HookDoctorDiagnostics {
            checked_count: total,
            error_count: 0,
            warning_count: 0,
            message: format!("{total} hook(s) configured and statically valid"),
            action: Some(
                "inspect configured hooks with `kheish-daemon runtime hooks get`".to_string(),
            ),
            related_id: None,
        }
    }
}

async fn doctor_http_hook_target_diagnostics(
    settings: &kheish_types::HookSettings,
) -> HookDoctorDiagnostics {
    let mut targets = Vec::new();
    for (event, hooks) in &settings.hooks {
        for hook in hooks {
            let kheish_types::HookExecutorConfig::Http { url, .. } = &hook.executor else {
                continue;
            };
            if doctor_validate_http_hook_target(url).is_some() {
                continue;
            }
            targets.push((format!("{event:?}:{}", hook.name), url.clone()));
        }
    }
    let checked_count = targets.len();
    let results = join_all(
        targets
            .iter()
            .map(|(_, url)| doctor_resolve_http_hook_target(url)),
    )
    .await;
    let results = targets
        .into_iter()
        .zip(results)
        .map(|((hook_id, _), result)| (hook_id, result.map_err(|error| format!("{error:#}"))))
        .collect::<Vec<_>>();
    doctor_http_hook_target_diagnostics_from_results(checked_count, results)
}

fn doctor_http_hook_target_diagnostics_from_results(
    checked_count: usize,
    results: Vec<(String, std::result::Result<Vec<SocketAddr>, String>)>,
) -> HookDoctorDiagnostics {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for (hook_id, result) in results {
        match result {
            Ok(addresses) => {
                if let Some(blocked) = addresses
                    .iter()
                    .find(|address| kheish_daemon::hook_http_target_blocks_ip(address.ip()))
                {
                    errors.push((
                        hook_id,
                        format!(
                            "HTTP hook hostname resolves to private or local address `{blocked}`"
                        ),
                    ));
                }
            }
            Err(error) => {
                warnings.push((
                    hook_id,
                    format!("HTTP hook hostname DNS resolution failed: {error}"),
                ));
            }
        }
    }
    if let Some((related_id, message)) = errors.first() {
        HookDoctorDiagnostics {
            checked_count,
            error_count: errors.len(),
            warning_count: warnings.len(),
            message: format!(
                "{} HTTP hook target error(s), {} warning(s): {message}",
                errors.len(),
                warnings.len()
            ),
            action: Some(
                "fix HTTP hook hostnames so DNS does not resolve to private or local addresses"
                    .to_string(),
            ),
            related_id: Some(related_id.clone()),
        }
    } else if let Some((related_id, message)) = warnings.first() {
        HookDoctorDiagnostics {
            checked_count,
            error_count: 0,
            warning_count: warnings.len(),
            message: format!(
                "{checked_count} HTTP hook target(s) checked with {} warning(s): {message}",
                warnings.len()
            ),
            action: Some("verify DNS for configured HTTP hook hostnames".to_string()),
            related_id: Some(related_id.clone()),
        }
    } else {
        HookDoctorDiagnostics {
            checked_count,
            error_count: 0,
            warning_count: 0,
            message: format!("{checked_count} HTTP hook target(s) resolve to public addresses"),
            action: None,
            related_id: None,
        }
    }
}

async fn doctor_resolve_http_hook_target(url: &str) -> Result<Vec<SocketAddr>> {
    let parsed = reqwest::Url::parse(url)?;
    let Some(host) = parsed.host_str() else {
        bail!("HTTP hook URL requires a concrete host");
    };
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("HTTP hook URL requires an explicit or default port"))?;
    if let Ok(address) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(address, port)]);
    }
    let host = host.to_string();
    let resolved = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .context("HTTP hook DNS lookup timed out")??
    .collect::<Vec<_>>();
    if resolved.is_empty() {
        bail!("HTTP hook hostname resolved no addresses");
    }
    Ok(resolved)
}

fn doctor_validate_hook_timeout(kind: &str, timeout_ms: Option<u64>) -> Option<String> {
    let timeout_ms = timeout_ms?;
    if timeout_ms == 0 {
        return Some(format!("{kind} hook timeout_ms is zero"));
    }
    if timeout_ms > kheish_daemon::MAX_CONFIGURED_HOOK_TIMEOUT_MS {
        return Some(format!(
            "{kind} hook timeout_ms exceeds {}",
            kheish_daemon::MAX_CONFIGURED_HOOK_TIMEOUT_MS
        ));
    }
    None
}

fn doctor_validate_command_hook_shell(shell: Option<&str>) -> Option<String> {
    let Some(shell) = shell else {
        return None;
    };
    let shell = shell.trim();
    if shell.is_empty() {
        return Some("command hook shell is empty".to_string());
    }
    let shell_path = Path::new(shell);
    if shell_path.is_absolute() || shell.contains(std::path::MAIN_SEPARATOR) {
        return doctor_validate_shell_path(shell_path, shell);
    }
    None
}

fn doctor_validate_shell_path(path: &Path, display: &str) -> Option<String> {
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            Some(format!("command hook shell `{display}` is not a file"))
        }
        Ok(metadata) if !doctor_shell_metadata_is_executable(&metadata) => {
            Some(format!("command hook shell `{display}` is not executable"))
        }
        Ok(_) => None,
        Err(error) => Some(format!(
            "command hook shell `{display}` is not usable: {error}"
        )),
    }
}

#[cfg(unix)]
fn doctor_shell_metadata_is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn doctor_shell_metadata_is_executable(_metadata: &fs::Metadata) -> bool {
    true
}

fn doctor_validate_http_hook_target(url: &str) -> Option<String> {
    let parsed = match reqwest::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(error) => return Some(format!("HTTP hook URL is invalid: {error}")),
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return Some("HTTP hook URL must use http or https".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Some("HTTP hook URL must not include userinfo".to_string());
    }
    let Some(host) = parsed.host_str() else {
        return Some("HTTP hook URL requires a concrete host".to_string());
    };
    let lower_host = host.trim_end_matches('.').to_ascii_lowercase();
    if lower_host == "localhost" || lower_host.ends_with(".localhost") {
        return Some("HTTP hook URL must not target localhost".to_string());
    }
    if let Ok(address) = host.parse::<IpAddr>()
        && kheish_daemon::hook_http_target_blocks_ip(address)
    {
        return Some("HTTP hook URL must not target private or local addresses".to_string());
    }
    None
}

fn push_doctor_check_with_details(
    checks: &mut Vec<crate::DoctorCheckView>,
    name: impl Into<String>,
    ok: bool,
    severity: impl Into<String>,
    message: impl Into<String>,
    action: Option<String>,
    related_id: Option<String>,
) {
    let name = name.into();
    let message = message.into();
    if checks
        .iter()
        .any(|check| check.name == name || check.message == message)
    {
        return;
    }
    checks.push(crate::DoctorCheckView {
        code: name.clone(),
        name,
        ok,
        severity: severity.into(),
        message,
        action,
        related_id,
    });
}

#[cfg(test)]
fn doctor_warnings(
    status: &kheish_daemon::DaemonStatusView,
    readyz_reachable: bool,
    events_stream_reachable: bool,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut warning_codes = BTreeSet::new();
    let mut push_warning = |code: &str, message: String| {
        if warning_codes.insert(code.to_string()) {
            warnings.push(message);
        }
    };
    if !status.ready {
        push_warning("daemon_draining", "daemon is draining".to_string());
    }
    if !readyz_reachable {
        push_warning(
            "readyz_unreachable",
            "ready endpoint is not reachable".to_string(),
        );
    }
    if !events_stream_reachable {
        push_warning(
            "events_unreachable",
            "event stream is not reachable".to_string(),
        );
    }
    if status.runtime.debug_level == kheish_runtime::DebugCaptureLevel::Full {
        push_warning(
            "debug_capture_full",
            "debug capture level is full".to_string(),
        );
    }
    if status.runs.failed > 0 {
        push_warning(
            "failed_runs",
            format!("{} failed run(s)", status.runs.failed),
        );
    }
    if status.agents.failed > 0 {
        push_warning(
            "failed_agents",
            format!("{} failed agent(s)", status.agents.failed),
        );
    }
    if status.schedules.backoff_count > 0 {
        push_warning(
            "schedule_retry_backoff",
            format!(
                "{} schedule(s) are deferred by retry backoff",
                status.schedules.backoff_count
            ),
        );
    }
    if status.tasks.failed > 0 {
        push_warning(
            "failed_tasks",
            format!("{} failed task(s)", status.tasks.failed),
        );
    }
    if status.tasks.blocked > 0 {
        push_warning(
            "blocked_tasks",
            format!("{} blocked task(s)", status.tasks.blocked),
        );
    }
    if status.tasks.unreadable_session_count > 0 {
        push_warning(
            "unreadable_task_sessions",
            format!(
                "{} session(s) could not be read for task status",
                status.tasks.unreadable_session_count
            ),
        );
    }
    if status.tasks.unindexed_session_count > 0 {
        push_warning(
            "unindexed_task_sessions",
            format!(
                "{} session(s) do not have indexed task summaries",
                status.tasks.unindexed_session_count
            ),
        );
    }
    for warning in status
        .health
        .warnings
        .iter()
        .filter(|warning| warning.severity != kheish_daemon::DaemonHealthSeverity::Info)
    {
        push_warning(&warning.code, warning.message.clone());
    }
    warnings
}

fn doctor_routes_runtime_view(status: &kheish_daemon::DaemonStatusView) -> crate::DoctorRoutesView {
    let routes = status
        .runtime
        .routes
        .iter()
        .map(runtime_route_view)
        .collect::<Vec<_>>();
    let route_ids = routes
        .iter()
        .map(|route| route.route_id.as_str())
        .collect::<BTreeSet<_>>();
    let default_route = status
        .runtime
        .default_route
        .as_ref()
        .map(|route| route.route_id.clone())
        .or_else(|| status.runtime.route_id.clone())
        .or_else(|| {
            (routes.len() == 1).then(|| {
                routes
                    .first()
                    .expect("route exists when routes.len() == 1")
                    .route_id
                    .clone()
            })
        });
    let mut diagnostics = status.runtime.route_diagnostics.clone();
    append_route_inventory_source_diagnostics(status, &mut diagnostics);

    if routes.is_empty() {
        diagnostics.push(route_diagnostic(
            kheish_daemon::RouteDiagnosticSeverity::Error,
            "route_inventory_empty",
            None,
            "runtime has no configured routes",
        ));
    }
    match default_route.as_deref() {
        Some(route_id) if !route_ids.contains(route_id) => {
            diagnostics.push(route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Error,
                "default_route_missing",
                Some(route_id.to_string()),
                format!("default route `{route_id}` is not present in runtime routes"),
            ));
        }
        None => diagnostics.push(route_diagnostic(
            kheish_daemon::RouteDiagnosticSeverity::Error,
            "default_route_unset",
            None,
            "runtime has no default route",
        )),
        _ => {}
    }

    let (warnings, errors) = diagnostic_messages(&diagnostics);
    crate::DoctorRoutesView {
        ok: !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == kheish_daemon::RouteDiagnosticSeverity::Error),
        source: "runtime".to_string(),
        default_route,
        route_count: routes.len(),
        auth_checked: false,
        reference_checked: false,
        canary_checked: false,
        routes,
        canaries: Vec::new(),
        diagnostics,
        warnings,
        errors,
    }
}

fn runtime_route_view(route: &kheish_daemon::ResolvedModelRoute) -> crate::DoctorRouteView {
    crate::DoctorRouteView {
        route_id: route.route_id.clone(),
        provider: route.provider.clone(),
        model: route.model.clone(),
        auth_ref: route.auth_ref.clone(),
        auth_kind: if route.auth_ref.is_some() {
            "auth_ref"
        } else {
            "provider_default"
        }
        .to_string(),
        model_support: None,
        capabilities: Some(route.capabilities.clone()),
        account_auth_slot: None,
        account_auth_provider: None,
        account_auth_file: None,
    }
}

fn append_route_inventory_source_diagnostics(
    status: &kheish_daemon::DaemonStatusView,
    diagnostics: &mut Vec<kheish_daemon::RouteDiagnosticView>,
) {
    let Some(state_root) = status.runtime.state_root.as_deref() else {
        return;
    };
    let metadata = match crate::cli::read_route_inventory_metadata(Path::new(state_root)) {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return,
        Err(error) => {
            diagnostics.push(route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Warning,
                "route_inventory_metadata_unreadable",
                None,
                format!("route inventory metadata could not be read: {error}"),
            ));
            return;
        }
    };
    if metadata.source_kind != "routes_file" {
        return;
    }
    match crate::cli::current_routes_file_sha256(&metadata) {
        Ok(Some(current_sha256)) => {
            if metadata.routes_file_sha256.as_deref() != Some(current_sha256.as_str()) {
                let path = metadata.routes_file_path.as_deref().unwrap_or("<unknown>");
                let loaded = metadata
                    .routes_file_sha256
                    .as_deref()
                    .map(short_digest)
                    .unwrap_or("<missing>");
                let current = short_digest(&current_sha256);
                diagnostics.push(route_diagnostic(
                    kheish_daemon::RouteDiagnosticSeverity::Warning,
                    "route_file_drift",
                    None,
                    format!(
                        "routes file `{path}` changed after daemon load: loaded sha256 {loaded}, current sha256 {current}; restart or run a controlled route reload before relying on new routes"
                    ),
                ));
            }
        }
        Ok(None) => {}
        Err(error) => {
            let path = metadata.routes_file_path.as_deref().unwrap_or("<unknown>");
            diagnostics.push(route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Warning,
                "route_file_unreadable",
                None,
                format!(
                    "routes file `{path}` loaded by the daemon could not be re-read for drift checking: {error}"
                ),
            ));
        }
    }
}

fn short_digest(value: &str) -> &str {
    value.get(..16).unwrap_or(value)
}

fn doctor_routes_file_view(
    path: &Path,
    default_route_override: Option<&str>,
) -> crate::DoctorRoutesView {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            let diagnostics = vec![route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Error,
                "route_file_read_failed",
                None,
                format!("failed to read {}: {error}", path.display()),
            )];
            let (warnings, errors) = diagnostic_messages(&diagnostics);
            return crate::DoctorRoutesView {
                ok: false,
                source: "file".to_string(),
                default_route: None,
                route_count: 0,
                auth_checked: false,
                reference_checked: false,
                canary_checked: false,
                routes: Vec::new(),
                canaries: Vec::new(),
                diagnostics,
                warnings,
                errors,
            };
        }
    };
    let config = match crate::route_file::RoutesFileConfig::from_toml_str(&raw) {
        Ok(config) => config,
        Err(error) => {
            let diagnostics = vec![route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Error,
                "route_file_invalid",
                None,
                error.to_string(),
            )];
            let (warnings, errors) = diagnostic_messages(&diagnostics);
            return crate::DoctorRoutesView {
                ok: false,
                source: "file".to_string(),
                default_route: None,
                route_count: 0,
                auth_checked: false,
                reference_checked: false,
                canary_checked: false,
                routes: Vec::new(),
                canaries: Vec::new(),
                diagnostics,
                warnings,
                errors,
            };
        }
    };

    let mut diagnostics = Vec::new();
    let file_default_route = match config.effective_default_route() {
        Ok(route_id) => Some(route_id),
        Err(error) => {
            diagnostics.push(route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Error,
                "default_route_unresolved",
                None,
                error.to_string(),
            ));
            None
        }
    };
    let default_route = match (file_default_route, default_route_override) {
        (Some(_), Some(route_id)) => {
            if !config.routes.contains_key(route_id) {
                diagnostics.push(route_diagnostic(
                    kheish_daemon::RouteDiagnosticSeverity::Error,
                    "default_route_missing",
                    Some(route_id.to_string()),
                    format!("default route override `{route_id}` is not defined under [routes]"),
                ));
            }
            Some(route_id.to_string())
        }
        (Some(route_id), None) => Some(route_id),
        (None, _) => None,
    };
    let routes = config
        .routes
        .iter()
        .map(|(route_id, entry)| file_route_view(route_id, entry))
        .collect::<Vec<_>>();
    for (route_id, entry) in &config.routes {
        diagnostics.extend(file_route_diagnostics(route_id, entry));
    }
    let (warnings, errors) = diagnostic_messages(&diagnostics);

    crate::DoctorRoutesView {
        ok: !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == kheish_daemon::RouteDiagnosticSeverity::Error),
        source: "file".to_string(),
        default_route,
        route_count: routes.len(),
        auth_checked: false,
        reference_checked: false,
        canary_checked: false,
        routes,
        canaries: Vec::new(),
        diagnostics,
        warnings,
        errors,
    }
}

fn file_route_view(
    route_id: &str,
    entry: &crate::route_file::RouteFileEntry,
) -> crate::DoctorRouteView {
    let capabilities = entry.resolved_capabilities(entry.driver.supported_capabilities());
    let account_auth = file_route_account_auth(route_id, entry);
    crate::DoctorRouteView {
        route_id: route_id.to_string(),
        provider: entry.driver.as_str().to_string(),
        model: entry.default_model.clone(),
        auth_ref: entry.auth_ref.clone(),
        auth_kind: route_file_auth_kind(entry).to_string(),
        model_support: Some(file_route_model_support(entry)),
        capabilities: Some(capabilities),
        account_auth_slot: account_auth
            .as_ref()
            .map(|account_auth| account_auth.slot_id.clone()),
        account_auth_provider: account_auth
            .as_ref()
            .map(|account_auth| account_auth.provider),
        account_auth_file: account_auth.and_then(|account_auth| account_auth.auth_file),
    }
}

struct FileRouteAccountAuth {
    slot_id: String,
    provider: AuthProvider,
    auth_file: Option<PathBuf>,
}

fn file_route_account_auth(
    route_id: &str,
    entry: &crate::route_file::RouteFileEntry,
) -> Option<FileRouteAccountAuth> {
    if entry.auth_ref.is_some() {
        return None;
    }
    match (entry.driver, entry.openai_auth_source) {
        (
            crate::route_file::RouteFileDriver::Openai,
            Some(crate::route_file::RouteFileOpenAiAuthSource::Codex),
        ) => Some(FileRouteAccountAuth {
            slot_id: format!("route.{route_id}"),
            provider: AuthProvider::OpenAi,
            auth_file: entry
                .openai_auth_file
                .clone()
                .or_else(crate::cli::default_codex_auth_path),
        }),
        _ => match (entry.driver, entry.anthropic_auth_source) {
            (
                crate::route_file::RouteFileDriver::Anthropic,
                Some(crate::route_file::RouteFileAnthropicAuthSource::ClaudeCode),
            ) => Some(FileRouteAccountAuth {
                slot_id: format!("route.{route_id}"),
                provider: AuthProvider::Anthropic,
                auth_file: entry
                    .anthropic_credentials_file
                    .clone()
                    .or_else(kheish_auth::default_claude_code_credentials_path),
            }),
            _ => None,
        },
    }
}

fn file_route_model_support(
    entry: &crate::route_file::RouteFileEntry,
) -> kheish_daemon::ModelSupportPolicy {
    if entry.driver == crate::route_file::RouteFileDriver::Openrouter {
        kheish_daemon::ModelSupportPolicy::Any
    } else {
        entry.model_support
    }
}

fn route_file_auth_kind(entry: &crate::route_file::RouteFileEntry) -> &'static str {
    if entry.auth_ref.is_some() {
        "auth_ref"
    } else if entry.api_key.is_some() {
        "inline_api_key"
    } else if entry.api_key_env.is_some() {
        "api_key_env"
    } else if entry.openai_auth_source.is_some() || entry.openai_auth_file.is_some() {
        "openai_account"
    } else if entry.anthropic_auth_source.is_some() || entry.anthropic_credentials_file.is_some() {
        "anthropic_account"
    } else {
        "provider_default"
    }
}

fn file_route_diagnostics(
    route_id: &str,
    entry: &crate::route_file::RouteFileEntry,
) -> Vec<kheish_daemon::RouteDiagnosticView> {
    let mut diagnostics = Vec::new();
    if entry.api_key.is_some() {
        diagnostics.push(route_diagnostic(
            kheish_daemon::RouteDiagnosticSeverity::Warning,
            "inline_api_key",
            Some(route_id.to_string()),
            format!("route `{route_id}` uses inline api_key; prefer daemon-managed auth_ref"),
        ));
    }
    if let Some(env_name) = entry.api_key_env.as_deref() {
        if let Some(diagnostic) =
            api_key_env_diagnostic(route_id, env_name, std::env::var(env_name))
        {
            diagnostics.push(diagnostic);
        }
    }
    if let Some(diagnostic) = file_route_missing_credential_diagnostic(route_id, entry) {
        diagnostics.push(diagnostic);
    }
    if let Some(path) = entry.openai_auth_file.as_ref()
        && !path.exists()
    {
        diagnostics.push(route_diagnostic(
            kheish_daemon::RouteDiagnosticSeverity::Error,
            "openai_auth_file_missing",
            Some(route_id.to_string()),
            format!(
                "route `{route_id}` references missing openai_auth_file `{}`",
                path.display()
            ),
        ));
    }
    if let Some(path) = entry.anthropic_credentials_file.as_ref()
        && !path.exists()
    {
        diagnostics.push(route_diagnostic(
            kheish_daemon::RouteDiagnosticSeverity::Error,
            "anthropic_credentials_file_missing",
            Some(route_id.to_string()),
            format!(
                "route `{route_id}` references missing anthropic_credentials_file `{}`",
                path.display()
            ),
        ));
    }
    if let Some(account_auth) = file_route_account_auth(route_id, entry)
        && let Some(path) = account_auth.auth_file.as_ref()
        && path.exists()
        && let Some(error) = account_auth_file_validation_error(account_auth.provider, path)
    {
        diagnostics.push(route_diagnostic(
            kheish_daemon::RouteDiagnosticSeverity::Error,
            account_auth_file_invalid_code(account_auth.provider),
            Some(route_id.to_string()),
            format!(
                "route `{route_id}` references invalid account auth file `{}`: {error}",
                path.display()
            ),
        ));
    }
    if entry.driver == crate::route_file::RouteFileDriver::Openai
        && matches!(
            entry.openai_auth_source,
            Some(crate::route_file::RouteFileOpenAiAuthSource::Codex)
        )
        && explicit_openai_media_capability_enabled(entry)
    {
        diagnostics.push(route_diagnostic(
            kheish_daemon::RouteDiagnosticSeverity::Error,
            "openai_account_auth_media_unsupported",
            Some(route_id.to_string()),
            format!(
                "route `{route_id}` uses OpenAI Codex account auth, which supports Responses text/tool calls only; disable image/audio/transcription capability overrides or use OpenAI API-key auth for media"
            ),
        ));
    }
    let supported = entry.driver.supported_capabilities();
    if entry.native_web_search == Some(true) && !supported.native_web_search {
        diagnostics.push(unsupported_capability_diagnostic(
            route_id,
            entry.driver.as_str(),
            "native_web_search",
        ));
    }
    if entry.image_generation == Some(true) && !supported.image_generation {
        diagnostics.push(unsupported_capability_diagnostic(
            route_id,
            entry.driver.as_str(),
            "image_generation",
        ));
    }
    if entry.image_edit == Some(true) && !supported.image_edit {
        diagnostics.push(unsupported_capability_diagnostic(
            route_id,
            entry.driver.as_str(),
            "image_edit",
        ));
    }
    if entry.audio_generation == Some(true) && !supported.audio_generation {
        diagnostics.push(unsupported_capability_diagnostic(
            route_id,
            entry.driver.as_str(),
            "audio_generation",
        ));
    }
    if entry.transcription == Some(true) && !supported.transcription {
        diagnostics.push(unsupported_capability_diagnostic(
            route_id,
            entry.driver.as_str(),
            "transcription",
        ));
    }
    diagnostics
}

fn file_route_missing_credential_diagnostic(
    route_id: &str,
    entry: &crate::route_file::RouteFileEntry,
) -> Option<kheish_daemon::RouteDiagnosticView> {
    if entry.auth_ref.is_some()
        || entry.api_key.is_some()
        || entry.api_key_env.is_some()
        || crate::cli::serve::route_uses_driver_defaults(route_id, entry.driver)
        || crate::cli::serve::resolve_route_api_key(route_id, entry).is_some()
    {
        return None;
    }
    let action = match entry.driver {
        crate::route_file::RouteFileDriver::Openai => {
            if matches!(
                entry.openai_auth_source,
                Some(crate::route_file::RouteFileOpenAiAuthSource::Codex)
            ) {
                return None;
            }
            "set api_key/api_key_env/auth_ref or choose openai_auth_source = \"codex\""
        }
        crate::route_file::RouteFileDriver::Anthropic => {
            if matches!(
                entry.anthropic_auth_source,
                Some(crate::route_file::RouteFileAnthropicAuthSource::ClaudeCode)
            ) {
                return None;
            }
            "set api_key/api_key_env/auth_ref or choose anthropic_auth_source = \"claude_code\""
        }
        crate::route_file::RouteFileDriver::Google => "set api_key/api_key_env/auth_ref",
        crate::route_file::RouteFileDriver::Openrouter => "set api_key/api_key_env/auth_ref",
        crate::route_file::RouteFileDriver::Xai => "set api_key/api_key_env/auth_ref",
    };
    Some(route_diagnostic(
        kheish_daemon::RouteDiagnosticSeverity::Error,
        "route_credentials_missing",
        Some(route_id.to_string()),
        format!(
            "route `{route_id}` is a custom {} route without credentials; {action}",
            entry.driver.as_str()
        ),
    ))
}

fn api_key_env_diagnostic(
    route_id: &str,
    env_name: &str,
    value: Result<String, std::env::VarError>,
) -> Option<kheish_daemon::RouteDiagnosticView> {
    match value {
        Ok(value) if value.trim().is_empty() => Some(route_diagnostic(
            kheish_daemon::RouteDiagnosticSeverity::Error,
            "api_key_env_empty",
            Some(route_id.to_string()),
            format!("route `{route_id}` references api_key_env `{env_name}` but it is empty"),
        )),
        Ok(_) => None,
        Err(std::env::VarError::NotPresent) => Some(route_diagnostic(
            kheish_daemon::RouteDiagnosticSeverity::Error,
            "api_key_env_missing",
            Some(route_id.to_string()),
            format!("route `{route_id}` references api_key_env `{env_name}` but it is not set"),
        )),
        Err(std::env::VarError::NotUnicode(_)) => Some(route_diagnostic(
            kheish_daemon::RouteDiagnosticSeverity::Error,
            "api_key_env_invalid",
            Some(route_id.to_string()),
            format!(
                "route `{route_id}` references api_key_env `{env_name}` but it is not valid UTF-8"
            ),
        )),
    }
}

fn account_auth_file_validation_error(provider: AuthProvider, path: &Path) -> Option<String> {
    let slot_id = AuthSlotId::new("doctor.account-auth-file-check");
    let result = match provider {
        AuthProvider::OpenAi => {
            kheish_auth::OpenAiAuthBackend::import_codex_record(slot_id, path, None, None)
                .map(|_| ())
        }
        AuthProvider::Anthropic => {
            kheish_auth::AnthropicAuthBackend::import_claude_code_record(slot_id, path).map(|_| ())
        }
        _ => return None,
    };
    result
        .err()
        .map(|error| sanitized_account_auth_file_error(provider, &error))
}

fn sanitized_account_auth_file_error(provider: AuthProvider, error: &anyhow::Error) -> String {
    let message = error.to_string();
    match provider {
        AuthProvider::OpenAi if message.contains("failed to read Codex auth file") => {
            "Codex auth file could not be read".to_string()
        }
        AuthProvider::OpenAi if message.contains("failed to parse Codex auth file") => {
            "Codex auth file is not valid JSON or does not match the expected schema".to_string()
        }
        AuthProvider::OpenAi if message.contains("does not contain account tokens") => {
            "Codex auth file does not contain account tokens".to_string()
        }
        AuthProvider::OpenAi if message.contains("auth mode") => {
            "Codex auth file uses an unsupported account auth mode".to_string()
        }
        AuthProvider::Anthropic
            if message.contains("failed to read Claude Code credentials file") =>
        {
            "Claude Code credentials file could not be read".to_string()
        }
        AuthProvider::Anthropic
            if message.contains("failed to parse Claude Code credentials file") =>
        {
            "Claude Code credentials file is not valid JSON or does not match the expected schema"
                .to_string()
        }
        AuthProvider::Anthropic if message.contains("do not contain claudeAiOauth") => {
            "Claude Code credentials file does not contain claudeAiOauth".to_string()
        }
        AuthProvider::Anthropic if message.contains("missing the required") => {
            "Claude Code credentials file is missing required OAuth scopes".to_string()
        }
        _ => "account auth file could not be imported".to_string(),
    }
}

fn account_auth_file_invalid_code(provider: AuthProvider) -> &'static str {
    match provider {
        AuthProvider::OpenAi => "openai_auth_file_invalid",
        AuthProvider::Anthropic => "anthropic_credentials_file_invalid",
        _ => "account_auth_file_invalid",
    }
}

fn explicit_openai_media_capability_enabled(entry: &crate::route_file::RouteFileEntry) -> bool {
    entry.image_generation == Some(true)
        || entry.image_edit == Some(true)
        || entry.audio_generation == Some(true)
        || entry.transcription == Some(true)
}

fn unsupported_capability_diagnostic(
    route_id: &str,
    driver: &str,
    capability: &str,
) -> kheish_daemon::RouteDiagnosticView {
    route_diagnostic(
        kheish_daemon::RouteDiagnosticSeverity::Error,
        "unsupported_capability_override",
        Some(route_id.to_string()),
        format!(
            "route `{route_id}` enables {capability} but driver `{driver}` does not support it"
        ),
    )
}

fn filter_doctor_routes_view(view: &mut crate::DoctorRoutesView, route_id: &str) {
    let original_count = view.routes.len();
    view.routes.retain(|route| route.route_id == route_id);
    view.diagnostics.retain(|diagnostic| {
        diagnostic
            .route_id
            .as_deref()
            .map_or(true, |diagnostic_route_id| diagnostic_route_id == route_id)
    });
    view.route_count = view.routes.len();
    if view.routes.is_empty() && original_count > 0 {
        view.diagnostics.push(route_diagnostic(
            kheish_daemon::RouteDiagnosticSeverity::Error,
            "route_not_found",
            Some(route_id.to_string()),
            format!(
                "route `{route_id}` is not present in {} route inventory",
                view.source
            ),
        ));
    }
}

async fn apply_route_reference_checks(
    client: &crate::cli::DaemonHttpClient,
    view: &mut crate::DoctorRoutesView,
    route_filter: Option<&str>,
) -> Result<()> {
    view.reference_checked = true;
    if view.source != "runtime" {
        view.diagnostics.push(route_diagnostic(
            kheish_daemon::RouteDiagnosticSeverity::Error,
            "route_reference_check_requires_runtime",
            None,
            "route reference checks require the running daemon inventory; remove --routes-file",
        ));
        return Ok(());
    }

    let configured_route_ids = view
        .routes
        .iter()
        .map(|route| route.route_id.as_str())
        .collect::<BTreeSet<_>>();
    let missing_reference = |route_id: &str| {
        let route_id = route_id.trim();
        !route_id.is_empty()
            && !configured_route_ids.contains(route_id)
            && route_filter.map_or(true, |filter| filter == route_id)
    };

    let (sessions, schedules, runs) = match tokio::try_join!(
        client.get_json::<Vec<kheish_daemon::SessionViewSummary>>("/v1/sessions"),
        client.get_json::<Vec<kheish_daemon::ScheduleView>>("/v1/schedules"),
        client.get_json::<Vec<kheish_daemon::RunView>>("/v1/runs"),
    ) {
        Ok(values) => values,
        Err(error) => {
            view.diagnostics.push(route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Error,
                "route_reference_check_failed",
                None,
                format!("failed to inspect persisted route references: {error}"),
            ));
            return Err(error);
        }
    };

    for run in runs {
        if run.status.is_terminal() {
            continue;
        }
        let Some(route_id) = run.request.provider.as_deref() else {
            continue;
        };
        if missing_reference(route_id) {
            view.diagnostics.push(route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Error,
                "stale_run_route",
                Some(route_id.to_string()),
                format!(
                    "run `{}` ({:?}) is pinned to route `{route_id}` which is not present in runtime route inventory; cancel, resume after restoring the route, or update route configuration",
                    run.run_id, run.status
                ),
            ));
        }
    }

    for session in sessions {
        let Some(route_id) = session.route_policy.provider.as_deref() else {
            continue;
        };
        if missing_reference(route_id) {
            view.diagnostics.push(route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Error,
                "stale_session_route_policy",
                Some(route_id.to_string()),
                format!(
                    "session `{}` route policy references route `{route_id}` which is not present in runtime route inventory; update or clear the session route policy",
                    session.session_id
                ),
            ));
        }
    }

    for schedule in schedules {
        if matches!(
            schedule.status,
            kheish_daemon::ScheduleStatus::Completed | kheish_daemon::ScheduleStatus::Canceled
        ) {
            continue;
        }
        let Some(route_id) = schedule.request.provider.as_deref() else {
            continue;
        };
        if missing_reference(route_id) {
            view.diagnostics.push(route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Error,
                "stale_schedule_route",
                Some(route_id.to_string()),
                format!(
                    "schedule `{}` targets route `{route_id}` which is not present in runtime route inventory; update, pause, or delete the schedule",
                    schedule.schedule_id
                ),
            ));
        }
    }
    Ok(())
}

async fn apply_route_auth_checks(
    client: &crate::cli::DaemonHttpClient,
    view: &mut crate::DoctorRoutesView,
) -> Result<()> {
    view.auth_checked = true;
    if view.source == "file" {
        view.diagnostics.push(route_diagnostic(
            kheish_daemon::RouteDiagnosticSeverity::Info,
            "auth_check_uses_running_daemon",
            None,
            "auth_ref values from the routes file are checked against the running daemon secrets",
        ));
    }
    let auth_refs = view
        .routes
        .iter()
        .filter_map(|route| {
            route
                .auth_ref
                .as_ref()
                .map(|auth_ref| (auth_ref.clone(), route.route_id.clone()))
        })
        .collect::<Vec<_>>();
    let account_auth_refs = view
        .routes
        .iter()
        .filter_map(|route| {
            let slot_id = route.account_auth_slot.clone()?;
            let provider = route.account_auth_provider?;
            Some((
                route.route_id.clone(),
                slot_id,
                provider,
                route.account_auth_file.clone(),
            ))
        })
        .collect::<Vec<_>>();
    if auth_refs.is_empty() && account_auth_refs.is_empty() {
        return Ok(());
    }

    let statuses = match client
        .get_json::<Vec<kheish_auth::AuthSlotStatus>>("/v1/runtime/secrets")
        .await
    {
        Ok(statuses) => statuses,
        Err(error) => {
            view.diagnostics.push(route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Error,
                "auth_check_failed",
                None,
                format!("failed to check daemon auth slots: {error}"),
            ));
            return Err(error);
        }
    };
    apply_auth_ref_status_diagnostics(view, &auth_refs, &statuses);
    apply_account_auth_status_diagnostics(view, &account_auth_refs, &statuses);
    Ok(())
}

async fn apply_route_canary_checks(
    client: &crate::cli::DaemonHttpClient,
    view: &mut crate::DoctorRoutesView,
    timeout_ms: u64,
) {
    view.canary_checked = true;
    if view.source != "runtime" {
        view.diagnostics.push(route_diagnostic(
            kheish_daemon::RouteDiagnosticSeverity::Error,
            "route_canary_requires_runtime",
            None,
            "route canaries require the running daemon inventory; remove --routes-file",
        ));
        return;
    }

    let routes = view.routes.clone();
    for route in routes {
        let canary = run_route_canary(client, &route, timeout_ms).await;
        if canary.status != "passed" {
            let code = if canary.status == "timeout" {
                "route_canary_timeout"
            } else {
                "route_canary_failed"
            };
            view.diagnostics.push(route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Error,
                code,
                Some(canary.route_id.clone()),
                format!(
                    "route `{}` canary {}: {}",
                    canary.route_id, canary.status, canary.message
                ),
            ));
        }
        view.canaries.push(canary);
    }
}

async fn run_route_canary(
    client: &crate::cli::DaemonHttpClient,
    route: &crate::DoctorRouteView,
    timeout_ms: u64,
) -> crate::DoctorRouteCanaryView {
    let started = Instant::now();
    let planned_session_id = route_canary_session_id(&route.route_id, now_ms());
    let session = match client
        .post_json::<_, kheish_daemon::SessionView>(
            "/v1/sessions",
            &kheish_daemon::CreateSessionRequest {
                session_id: Some(planned_session_id.clone()),
                thread_id: None,
                persona_id: None,
                capability_scope: None,
                credential_scope: None,
            },
        )
        .await
    {
        Ok(session) => session,
        Err(error) => {
            return route_canary_view(
                route,
                planned_session_id,
                None,
                "failed",
                started,
                format!("failed to create canary session: {error}"),
            );
        }
    };
    let session_id = session.session_id;
    let run = match submit_route_canary_run(client, &session_id, route).await {
        Ok(run) => run,
        Err(error) => {
            end_route_canary_session(client, &session_id).await;
            return route_canary_view(
                route,
                session_id,
                None,
                "failed",
                started,
                format!("failed to submit canary run: {error}"),
            );
        }
    };
    let run_id = run.run_id.clone();
    let timeout_duration = Duration::from_millis(timeout_ms.max(1));
    let final_run = match tokio::time::timeout(
        timeout_duration,
        crate::cli::wait_for_run(client, &run_id, ROUTE_CANARY_POLL_INTERVAL_MS),
    )
    .await
    {
        Ok(Ok(final_run)) => final_run,
        Ok(Err(error)) => {
            end_route_canary_session(client, &session_id).await;
            return route_canary_view(
                route,
                session_id,
                Some(run_id),
                "failed",
                started,
                format!("failed while waiting for canary run: {error}"),
            );
        }
        Err(_) => {
            cancel_route_canary_run(client, &run_id).await;
            end_route_canary_session(client, &session_id).await;
            return route_canary_view(
                route,
                session_id,
                Some(run_id),
                "timeout",
                started,
                format!("run did not settle within {timeout_ms} ms"),
            );
        }
    };

    end_route_canary_session(client, &session_id).await;
    if final_run.status == kheish_daemon::DaemonRunStatus::Completed
        && final_run
            .outputs
            .iter()
            .any(|output| output.content.contains(ROUTE_CANARY_TOKEN))
    {
        route_canary_view(
            route,
            session_id,
            Some(run_id),
            "passed",
            started,
            format!("run completed and emitted {ROUTE_CANARY_TOKEN}"),
        )
    } else {
        let status = format!("{:?}", final_run.status);
        let mut message = format!("run ended with status {status}");
        if let Some(error) = final_run.error.as_deref() {
            message.push_str(": ");
            message.push_str(error);
        } else {
            message.push_str("; output preview: ");
            message.push_str(&run_output_preview(&final_run));
        }
        route_canary_view(route, session_id, Some(run_id), "failed", started, message)
    }
}

async fn submit_route_canary_run(
    client: &crate::cli::DaemonHttpClient,
    session_id: &str,
    route: &crate::DoctorRouteView,
) -> Result<kheish_daemon::RunView> {
    let encoded_session_id = crate::cli::url_encode_path_segment(session_id);
    let path = format!("/v1/sessions/{encoded_session_id}/runs");
    client
        .post_json::<_, kheish_daemon::RunView>(
            &path,
            &kheish_daemon::SubmitRunRequest {
                idempotency_key: Some(format!(
                    "doctor-route-canary-{}-{}",
                    route.route_id,
                    now_ms()
                )),
                request: kheish_daemon::SubmitInputRequest {
                    provider: Some(route.route_id.clone()),
                    source_plugin: Some(ROUTE_CANARY_SOURCE_PLUGIN.to_string()),
                    source_kind: Some(ROUTE_CANARY_SOURCE_KIND.to_string()),
                    actor_id: Some(ROUTE_CANARY_ACTOR_ID.to_string()),
                    content: format!("Reply exactly {ROUTE_CANARY_TOKEN} and nothing else."),
                    input_items: Vec::new(),
                    attachments: Vec::new(),
                    generation: Some(ModelGenerationConfig {
                        model: Some(route.model.clone()),
                        tool_choice: ToolChoice::None,
                        max_output_tokens: Some(32),
                        ..ModelGenerationConfig::default()
                    }),
                    completion_requirements: None,
                    metadata: Some(serde_json::json!({
                        "doctor_route_canary": true,
                        "route_id": route.route_id.clone(),
                    })),
                    binding_keys: Vec::new(),
                    reply_targets: Vec::new(),
                    reply_plugin: None,
                    reply_address: None,
                },
            },
        )
        .await
}

async fn cancel_route_canary_run(client: &crate::cli::DaemonHttpClient, run_id: &str) {
    let encoded_run_id = crate::cli::url_encode_path_segment(run_id);
    let _ = client
        .post_empty_json::<kheish_daemon::RunView>(&format!("/v1/runs/{encoded_run_id}/cancel"))
        .await;
}

async fn end_route_canary_session(client: &crate::cli::DaemonHttpClient, session_id: &str) {
    let encoded_session_id = crate::cli::url_encode_path_segment(session_id);
    let _ = client
        .post_json::<_, kheish_daemon::SessionView>(
            &format!("/v1/sessions/{encoded_session_id}/end"),
            &kheish_daemon::EndSessionRequest {
                reason: Some("doctor route canary complete".to_string()),
            },
        )
        .await;
}

fn route_canary_view(
    route: &crate::DoctorRouteView,
    session_id: String,
    run_id: Option<String>,
    status: &str,
    started: Instant,
    message: String,
) -> crate::DoctorRouteCanaryView {
    crate::DoctorRouteCanaryView {
        route_id: route.route_id.clone(),
        provider: route.provider.clone(),
        model: route.model.clone(),
        session_id,
        run_id,
        status: status.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        message,
    }
}

fn route_canary_session_id(route_id: &str, timestamp_ms: u64) -> String {
    let mut safe_route_id = route_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    if safe_route_id.is_empty() {
        safe_route_id = "route".to_string();
    }
    format!("doctor-route-canary-{safe_route_id}-{timestamp_ms}")
}

fn run_output_preview(run: &kheish_daemon::RunView) -> String {
    let output = run
        .outputs
        .iter()
        .map(|output| output.content.trim())
        .filter(|content| !content.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if output.is_empty() {
        return "no daemon output".to_string();
    }
    const MAX_PREVIEW_CHARS: usize = 240;
    let mut preview = output.chars().take(MAX_PREVIEW_CHARS).collect::<String>();
    if output.chars().count() > MAX_PREVIEW_CHARS {
        preview.push_str("...");
    }
    preview
}

fn apply_account_auth_status_diagnostics(
    view: &mut crate::DoctorRoutesView,
    account_auth_refs: &[(String, String, AuthProvider, Option<PathBuf>)],
    statuses: &[kheish_auth::AuthSlotStatus],
) {
    for (route_id, slot_id, expected_provider, auth_file) in account_auth_refs {
        let status = statuses
            .iter()
            .find(|status| status.slot_id.to_string() == *slot_id);
        match status {
            Some(status) => {
                if status.provider != *expected_provider {
                    view.diagnostics.push(route_diagnostic(
                        kheish_daemon::RouteDiagnosticSeverity::Error,
                        "account_auth_provider_mismatch",
                        Some(route_id.clone()),
                        format!(
                            "account auth slot `{slot_id}` targets provider `{}` but route `{route_id}` requires `{expected_provider}`",
                            status.provider
                        ),
                    ));
                }
                if status.mode != kheish_auth::AuthMode::OAuthAccount {
                    view.diagnostics.push(route_diagnostic(
                        kheish_daemon::RouteDiagnosticSeverity::Error,
                        "account_auth_slot_mode_mismatch",
                        Some(route_id.clone()),
                        format!(
                            "account auth slot `{slot_id}` uses mode `{:?}` but route `{route_id}` requires OAuth account auth",
                            status.mode
                        ),
                    ));
                }
            }
            None if auth_file.as_ref().is_some_and(|path| path.exists()) => {
                let auth_file = auth_file
                    .as_ref()
                    .expect("auth_file exists because guard checked it");
                if let Some(error) =
                    account_auth_file_validation_error(*expected_provider, auth_file)
                {
                    view.diagnostics.push(route_diagnostic(
                        kheish_daemon::RouteDiagnosticSeverity::Error,
                        account_auth_file_invalid_code(*expected_provider),
                        Some(route_id.clone()),
                        format!(
                            "account auth slot `{slot_id}` is not present, and source credentials file `{}` cannot be imported on serve startup: {error}",
                            auth_file.display()
                        ),
                    ));
                } else {
                    view.diagnostics.push(route_diagnostic(
                        kheish_daemon::RouteDiagnosticSeverity::Info,
                        "account_auth_file_available",
                        Some(route_id.clone()),
                        format!(
                            "account auth slot `{slot_id}` is not present, but a source credentials file is available for import on serve startup"
                        ),
                    ));
                }
            }
            None => {
                let file_hint = auth_file
                    .as_ref()
                    .map(|path| format!(" or provide credentials at `{}`", path.display()))
                    .unwrap_or_default();
                view.diagnostics.push(route_diagnostic(
                    kheish_daemon::RouteDiagnosticSeverity::Error,
                    "account_auth_missing",
                    Some(route_id.clone()),
                    format!(
                        "route `{route_id}` requires account auth but slot `{slot_id}` is not present in daemon secrets{file_hint}"
                    ),
                ));
            }
        }
    }
}

fn apply_auth_ref_status_diagnostics(
    view: &mut crate::DoctorRoutesView,
    auth_refs: &[(String, String)],
    statuses: &[kheish_auth::AuthSlotStatus],
) {
    for (auth_ref, route_id) in auth_refs {
        let Some(status) = statuses
            .iter()
            .find(|status| status.slot_id.to_string() == *auth_ref)
        else {
            view.diagnostics.push(route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Error,
                "auth_ref_missing",
                Some(route_id.clone()),
                format!("auth_ref `{auth_ref}` is not present in daemon secrets"),
            ));
            continue;
        };
        let Some(route) = view.routes.iter().find(|route| route.route_id == *route_id) else {
            continue;
        };
        let Some(expected_provider) = auth_provider_for_route_provider(&route.provider) else {
            view.diagnostics.push(route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Warning,
                "auth_ref_provider_unchecked",
                Some(route_id.clone()),
                format!(
                    "auth_ref `{auth_ref}` provider cannot be validated for route provider `{}`",
                    route.provider
                ),
            ));
            continue;
        };
        if status.provider != expected_provider {
            view.diagnostics.push(route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Error,
                "auth_ref_provider_mismatch",
                Some(route_id.clone()),
                format!(
                    "auth_ref `{auth_ref}` targets provider `{}` but route `{route_id}` requires `{expected_provider}`",
                    status.provider
                ),
            ));
            continue;
        }
        if let Some(expires_at_ms) = status
            .details
            .get("expires_at_ms")
            .and_then(serde_json::Value::as_u64)
        {
            let now = now_ms();
            if expires_at_ms <= now {
                view.diagnostics.push(route_diagnostic(
                    kheish_daemon::RouteDiagnosticSeverity::Error,
                    "auth_ref_expired",
                    Some(route_id.clone()),
                    format!("auth_ref `{auth_ref}` for route `{route_id}` is expired"),
                ));
            } else if expires_at_ms.saturating_sub(now) <= 10 * 60 * 1_000 {
                view.diagnostics.push(route_diagnostic(
                    kheish_daemon::RouteDiagnosticSeverity::Warning,
                    "auth_ref_expiring_soon",
                    Some(route_id.clone()),
                    format!("auth_ref `{auth_ref}` for route `{route_id}` expires soon"),
                ));
            }
        }
        if let Some(last_refresh_outcome) = status
            .details
            .get("last_refresh_outcome")
            .and_then(serde_json::Value::as_str)
            && last_refresh_outcome != "success"
        {
            view.diagnostics.push(route_diagnostic(
                kheish_daemon::RouteDiagnosticSeverity::Warning,
                "auth_ref_refresh_warning",
                Some(route_id.clone()),
                format!(
                    "auth_ref `{auth_ref}` for route `{route_id}` last refresh outcome was `{last_refresh_outcome}`"
                ),
            ));
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_millis() as u64
}

fn auth_provider_for_route_provider(provider: &str) -> Option<AuthProvider> {
    match provider {
        "anthropic" => Some(AuthProvider::Anthropic),
        "google" => Some(AuthProvider::Google),
        "openai" => Some(AuthProvider::OpenAi),
        "openrouter" => Some(AuthProvider::OpenRouter),
        "xai" => Some(AuthProvider::XAi),
        _ => None,
    }
}

fn refresh_doctor_routes_ok(view: &mut crate::DoctorRoutesView) {
    let (warnings, errors) = diagnostic_messages(&view.diagnostics);
    view.warnings = warnings;
    view.errors = errors;
    view.ok = !view
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == kheish_daemon::RouteDiagnosticSeverity::Error);
}

fn route_diagnostic(
    severity: kheish_daemon::RouteDiagnosticSeverity,
    code: &str,
    route_id: Option<String>,
    message: impl Into<String>,
) -> kheish_daemon::RouteDiagnosticView {
    kheish_daemon::RouteDiagnosticView {
        severity,
        code: code.to_string(),
        route_id,
        message: message.into(),
    }
}

fn diagnostic_messages(
    diagnostics: &[kheish_daemon::RouteDiagnosticView],
) -> (Vec<String>, Vec<String>) {
    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == kheish_daemon::RouteDiagnosticSeverity::Warning)
        .map(|diagnostic| diagnostic.message.clone())
        .collect::<Vec<_>>();
    let errors = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == kheish_daemon::RouteDiagnosticSeverity::Error)
        .map(|diagnostic| diagnostic.message.clone())
        .collect::<Vec<_>>();
    (warnings, errors)
}

#[cfg(test)]
mod tests {
    use super::{
        api_key_env_diagnostic, apply_account_auth_status_diagnostics,
        apply_auth_ref_status_diagnostics, doctor_hook_diagnostics,
        doctor_http_hook_target_diagnostics, doctor_http_hook_target_diagnostics_from_results,
        doctor_report, doctor_routes_file_view, doctor_routes_runtime_view, doctor_warnings,
        is_legacy_status_shape, legacy_daemon_status_view, refresh_doctor_routes_ok,
    };
    use anyhow::Result;
    use kheish_auth::{AuthMode, AuthProvider, AuthSlotId, AuthSlotStatus};

    fn ok_probe(message: &str) -> crate::cli::http::DaemonProbeResult {
        crate::cli::http::DaemonProbeResult {
            ok: true,
            status: Some(reqwest::StatusCode::OK),
            content_type: None,
            message: message.to_string(),
            action: None,
        }
    }

    fn status_fixture() -> kheish_daemon::DaemonStatusView {
        kheish_daemon::DaemonStatusView {
            snapshot_at_ms: 0,
            process_id: 0,
            status: kheish_daemon::DaemonReadinessState::Ready,
            ready: true,
            capabilities: kheish_daemon::DaemonCapabilities {
                control_plane_version: "test".to_string(),
                api_revision: 3,
                route_capability_matrix_version: kheish_daemon::ROUTE_CAPABILITY_MATRIX_VERSION,
                approvals: true,
                sidechains: true,
                mailboxes: true,
                session_events: true,
                restart_restore: true,
                live_events: true,
                session_run_idempotency: true,
                playbooks: true,
                flows: true,
                problem_details: true,
                openapi: true,
                cursor_pagination: true,
                paginated_lists: true,
                domain_errors: true,
                sse_replay: true,
                typed_sse_heartbeat: true,
                agent_supervisor_audit: true,
                spawn_policies: true,
            },
            runtime: kheish_daemon::RuntimeSettingsView {
                workspace_root: None,
                state_root: None,
                default_route: None,
                route_id: None,
                provider: None,
                model: None,
                routes: Vec::new(),
                route_diagnostics: Vec::new(),
                permission_mode: kheish_runtime::PermissionMode::Default,
                system_prompt: kheish_runtime::SystemPromptSettings::default(),
                hooks: kheish_types::HookSettings::default(),
                debug_level: kheish_runtime::DebugCaptureLevel::Off,
                debug_capture: kheish_daemon::DebugCapturePolicyView::default(),
                mcp: kheish_mcp::McpRuntimeSnapshot::default(),
                skills: kheish_daemon::RuntimeSkillsView::default(),
                learning_policy: kheish_daemon::LearningAutomationPolicyConfig::default(),
                run_memory_policy: kheish_daemon::RunMemoryPolicyConfig::default(),
                tool_runtime_limits: kheish_runtime::ToolRuntimeLimits::default(),
                subagent_policy: kheish_daemon::SubagentPolicyConfig::default(),
                scheduler_policy: kheish_daemon::SchedulerPolicyConfig::default(),
                config: kheish_daemon::RuntimeConfigMetadataView::default(),
            },
            control_plane: kheish_daemon::DaemonControlPlaneStatusView::default(),
            storage: kheish_daemon::DaemonStorageStatusView::default(),
            provider_readiness: kheish_daemon::DaemonProviderReadinessView::default(),
            health: kheish_daemon::DaemonHealthView {
                ok: true,
                ..Default::default()
            },
            hooks: kheish_daemon::HookStatusView::default(),
            events: kheish_daemon::DaemonEventStatusView::default(),
            sessions: kheish_daemon::DaemonSessionStatusSummaryView { total: 0 },
            runs: kheish_daemon::DaemonRunStatusSummaryView::default(),
            run_memory: kheish_daemon::RunMemoryStatusView::default(),
            session_memory: kheish_daemon::SessionMemoryStatusView::default(),
            schedules: kheish_daemon::DaemonScheduleStatusSummaryView::default(),
            delivery: kheish_daemon::DeliveryQueueStatusView::default(),
            agents: kheish_daemon::DaemonAgentStatusSummaryView::default(),
            tasks: kheish_daemon::DaemonTaskStatusSummaryView::default(),
        }
    }

    #[test]
    fn doctor_warnings_reports_operational_risks() {
        let mut status = status_fixture();
        status.ready = false;
        status.status = kheish_daemon::DaemonReadinessState::Draining;
        status.runtime.debug_level = kheish_runtime::DebugCaptureLevel::Full;
        status.runs.failed = 2;
        status.agents.failed = 1;
        status.schedules.backoff_count = 3;
        status.tasks.failed = 4;
        status.tasks.blocked = 5;
        status.tasks.unreadable_session_count = 6;
        status.tasks.unindexed_session_count = 7;
        status
            .health
            .warnings
            .push(kheish_daemon::DaemonHealthWarningView {
                severity: kheish_daemon::DaemonHealthSeverity::Error,
                code: "route_diagnostics_error".to_string(),
                message: "1 route diagnostic error(s)".to_string(),
                related_id: None,
                action: None,
            });

        let warnings = doctor_warnings(&status, false, false);
        assert!(warnings.contains(&"daemon is draining".to_string()));
        assert!(warnings.contains(&"ready endpoint is not reachable".to_string()));
        assert!(warnings.contains(&"event stream is not reachable".to_string()));
        assert!(warnings.contains(&"debug capture level is full".to_string()));
        assert!(warnings.contains(&"2 failed run(s)".to_string()));
        assert!(warnings.contains(&"1 failed agent(s)".to_string()));
        assert!(warnings.contains(&"3 schedule(s) are deferred by retry backoff".to_string()));
        assert!(warnings.contains(&"4 failed task(s)".to_string()));
        assert!(warnings.contains(&"5 blocked task(s)".to_string()));
        assert!(warnings.contains(&"6 session(s) could not be read for task status".to_string()));
        assert!(warnings.contains(&"7 session(s) do not have indexed task summaries".to_string()));
        assert!(warnings.contains(&"1 route diagnostic error(s)".to_string()));
    }

    #[test]
    fn doctor_warnings_stays_empty_for_clean_status() {
        assert!(doctor_warnings(&status_fixture(), true, true).is_empty());
    }

    #[test]
    fn doctor_report_exposes_structured_checks_and_errors() {
        let mut status = status_fixture();
        status.runtime.default_route = Some(kheish_daemon::ResolvedModelRoute {
            route_id: "openai".to_string(),
            provider: "openai".to_string(),
            model: "gpt-5.4".to_string(),
            auth_ref: None,
            capabilities: kheish_daemon::RouteCapabilities {
                matrix_version: kheish_daemon::ROUTE_CAPABILITY_MATRIX_VERSION,
                multimodal_input: true,
                native_web_search: true,
                image_generation: true,
                image_edit: true,
                audio_generation: true,
                transcription: true,
            },
        });
        status.runtime.routes = vec![status.runtime.default_route.clone().expect("route")];
        status.control_plane.base_url = "http://127.0.0.1:4000".to_string();
        status.control_plane.bind_addr = Some("127.0.0.1:4000".to_string());
        status.control_plane.bind_is_loopback = true;

        let readyz = ok_probe("ready endpoint is reachable");
        let sse = ok_probe("event stream is reachable");
        let clean = doctor_report(&status, &readyz, &sse, None, None);
        assert!(clean.ok, "clean doctor should pass: {:?}", clean.errors);
        assert!(clean.errors.is_empty());
        assert!(
            clean
                .checks
                .iter()
                .any(|check| check.name == "routes" && check.ok)
        );

        status.control_plane.bind_addr = Some("0.0.0.0:4000".to_string());
        status.control_plane.bind_is_loopback = false;
        status.control_plane.bind_is_unspecified = true;
        status.control_plane.externally_exposed_without_auth = true;
        let broken = doctor_report(&status, &readyz, &sse, None, None);
        assert!(!broken.ok);
        assert!(
            broken
                .errors
                .iter()
                .any(|error| { error.contains("control-plane auth is disabled") })
        );
        assert!(
            broken
                .checks
                .iter()
                .any(|check| check.name == "control_plane_auth"
                    && check.severity == "error"
                    && check.action.is_some()
                    && check.related_id.as_deref() == Some("0.0.0.0:4000"))
        );
    }

    #[test]
    fn doctor_report_surfaces_readiness_storage_and_health_actions() {
        let mut status = status_fixture();
        status.runtime.routes = vec![kheish_daemon::ResolvedModelRoute {
            route_id: "openai".to_string(),
            provider: "openai".to_string(),
            model: "gpt-5.4".to_string(),
            auth_ref: Some("openai.primary".to_string()),
            capabilities: kheish_daemon::RouteCapabilities {
                matrix_version: kheish_daemon::ROUTE_CAPABILITY_MATRIX_VERSION,
                multimodal_input: true,
                native_web_search: true,
                image_generation: true,
                image_edit: true,
                audio_generation: true,
                transcription: true,
            },
        }];
        status.runtime.route_id = Some("openai".to_string());
        status.provider_readiness = kheish_daemon::DaemonProviderReadinessView {
            route_count: 1,
            ready_route_count: 0,
            warning_route_count: 0,
            error_route_count: 1,
            active_route_ready: false,
            routes: vec![kheish_daemon::DaemonProviderRouteReadinessView {
                route_id: "openai".to_string(),
                provider: "openai".to_string(),
                model: "gpt-5.4".to_string(),
                capabilities: kheish_daemon::RouteCapabilities {
                    matrix_version: kheish_daemon::ROUTE_CAPABILITY_MATRIX_VERSION,
                    multimodal_input: true,
                    native_web_search: true,
                    image_generation: true,
                    image_edit: true,
                    audio_generation: true,
                    transcription: true,
                },
                active: true,
                auth_ref: Some("openai.primary".to_string()),
                state: kheish_daemon::DaemonStatusProbeState::Error,
                code: "route_auth_ref_missing".to_string(),
                message: "route `openai` references missing auth_ref `openai.primary`".to_string(),
                action: Some("create auth slot `openai.primary`".to_string()),
                auth_mode: None,
                auth_summary: None,
                auth_updated_at_ms: None,
            }],
        };
        status.storage = kheish_daemon::DaemonStorageStatusView {
            checked_at_ms: 1,
            ok: false,
            write_error_count: 1,
            probes: vec![kheish_daemon::DaemonStorageProbeView {
                name: "state_root".to_string(),
                path: "/tmp/state".to_string(),
                state: kheish_daemon::DaemonStatusProbeState::Error,
                writable: false,
                latency_ms: 2,
                code: "write_probe_failed".to_string(),
                message: "permission denied".to_string(),
                action: Some("restore write access".to_string()),
            }],
            state_root_lock: None,
            asset_repair: kheish_daemon::AssetStartupRepairStatusView::default(),
            session_storage: None,
        };
        status
            .health
            .warnings
            .push(kheish_daemon::DaemonHealthWarningView {
                severity: kheish_daemon::DaemonHealthSeverity::Error,
                code: "provider_readiness_error".to_string(),
                message: "1 provider route(s) have readiness errors".to_string(),
                related_id: Some("openai".to_string()),
                action: Some("repair the route auth".to_string()),
            });
        status.runtime.hooks = kheish_types::HookSettings {
            hooks: std::collections::BTreeMap::from([(
                kheish_types::HookEventName::ConfigChange,
                vec![kheish_types::HookDefinition {
                    name: "bad-hook".to_string(),
                    matcher: None,
                    failure_policy: Default::default(),
                    executor: kheish_types::HookExecutorConfig::Command {
                        command: "   ".to_string(),
                        shell: None,
                        timeout_ms: Some(0),
                    },
                }],
            )]),
        };

        let readyz = ok_probe("ready endpoint is reachable");
        let sse = ok_probe("event stream is reachable");
        let report = doctor_report(&status, &readyz, &sse, None, None);
        assert!(!report.ok);
        let provider = report
            .checks
            .iter()
            .find(|check| check.name == "provider_readiness")
            .expect("provider readiness check");
        assert_eq!(provider.severity, "error");
        assert_eq!(provider.related_id.as_deref(), Some("openai"));
        assert_eq!(
            provider.action.as_deref(),
            Some("create auth slot `openai.primary`")
        );
        let storage = report
            .checks
            .iter()
            .find(|check| check.name == "storage")
            .expect("storage check");
        assert_eq!(storage.severity, "error");
        assert_eq!(storage.related_id.as_deref(), Some("/tmp/state"));
        assert_eq!(storage.action.as_deref(), Some("restore write access"));
        let health = report
            .checks
            .iter()
            .find(|check| check.name == "provider_readiness_error")
            .expect("health warning check");
        assert_eq!(health.action.as_deref(), Some("repair the route auth"));
        assert_eq!(health.related_id.as_deref(), Some("openai"));
        let hooks = report
            .checks
            .iter()
            .find(|check| check.name == "hooks")
            .expect("hooks check");
        assert_eq!(hooks.severity, "error");
        assert!(hooks.action.is_some());
    }

    #[test]
    fn doctor_report_surfaces_event_and_control_plane_token_file_diagnostics() {
        let mut status = status_fixture();
        status.runtime.default_route = Some(kheish_daemon::ResolvedModelRoute {
            route_id: "openai".to_string(),
            provider: "openai".to_string(),
            model: "gpt-5.4".to_string(),
            auth_ref: None,
            capabilities: kheish_daemon::RouteCapabilities {
                matrix_version: kheish_daemon::ROUTE_CAPABILITY_MATRIX_VERSION,
                multimodal_input: true,
                native_web_search: true,
                image_generation: true,
                image_edit: true,
                audio_generation: true,
                transcription: true,
            },
        });
        status.runtime.routes = vec![status.runtime.default_route.clone().expect("route")];
        status.control_plane = kheish_daemon::DaemonControlPlaneStatusView {
            base_url: "http://127.0.0.1:4000".to_string(),
            bind_addr: Some("127.0.0.1:4000".to_string()),
            bind_is_loopback: true,
            auth_enabled: true,
            read_only_token_enabled: true,
            auth_effective_admin_token_available: false,
            auth_effective_read_only_token_available: true,
            auth_token_file_count: 2,
            auth_token_file_error_count: 1,
            auth_token_files: vec![kheish_daemon::DaemonControlPlaneAuthTokenFileStatusView {
                role: "admin".to_string(),
                path: "/state/admin.token".to_string(),
                readable: false,
                token_loaded: false,
                error: Some("permission denied".to_string()),
            }],
            ..Default::default()
        };
        status.events = kheish_daemon::DaemonEventStatusView {
            history_capacity: 10,
            retained_event_count: 10,
            subscriber_count: 1,
            oldest_event_id: Some(41),
            newest_event_id: Some(50),
            next_event_id: 51,
            tail_event_id_cursor: Some("50".to_string()),
            replay_buffer_utilization_percent: 100,
            evicted_event_count: 5,
            replay_gap_count: 2,
            stream_lagged_event_count: 3,
            scope_eviction_floor_id: 30,
            evicted_session_scope_count: 1,
            evicted_run_scope_count: 1,
        };
        status
            .health
            .warnings
            .push(kheish_daemon::DaemonHealthWarningView {
            severity: kheish_daemon::DaemonHealthSeverity::Error,
            code: "control_plane_admin_auth_unavailable".to_string(),
            message:
                "control-plane admin auth is configured but no effective admin token is available"
                    .to_string(),
            related_id: Some("/state/admin.token".to_string()),
            action: Some("repair the admin token file".to_string()),
        });

        let readyz = ok_probe("ready endpoint is reachable");
        let sse = ok_probe("event stream is reachable");
        let report = doctor_report(&status, &readyz, &sse, None, None);
        assert!(!report.ok);
        let auth = report
            .checks
            .iter()
            .find(|check| check.name == "control_plane_auth")
            .expect("control-plane auth check");
        assert_eq!(auth.severity, "warning");
        assert_eq!(auth.related_id.as_deref(), Some("/state/admin.token"));
        assert!(
            auth.action
                .as_deref()
                .is_some_and(|action| action.contains("auth_token_files"))
        );
        let events = report
            .checks
            .iter()
            .find(|check| check.name == "events_status")
            .expect("event status check");
        assert_eq!(events.severity, "warning");
        assert_eq!(events.related_id.as_deref(), Some("50"));
        assert!(
            events
                .message
                .contains("skipped by lagging live stream consumers")
        );
        let health = report
            .checks
            .iter()
            .find(|check| check.name == "control_plane_admin_auth_unavailable")
            .expect("control-plane health check");
        assert_eq!(health.severity, "error");
        assert_eq!(health.related_id.as_deref(), Some("/state/admin.token"));
    }

    #[test]
    fn doctor_report_surfaces_run_memory_maintenance_info() {
        let mut status = status_fixture();
        status.runtime.default_route = Some(kheish_daemon::ResolvedModelRoute {
            route_id: "openai".to_string(),
            provider: "openai".to_string(),
            model: "gpt-5.4".to_string(),
            auth_ref: None,
            capabilities: kheish_daemon::RouteCapabilities {
                matrix_version: kheish_daemon::ROUTE_CAPABILITY_MATRIX_VERSION,
                multimodal_input: true,
                native_web_search: true,
                image_generation: true,
                image_edit: true,
                audio_generation: true,
                transcription: true,
            },
        });
        status.runtime.routes = vec![status.runtime.default_route.clone().expect("route")];
        status.run_memory.maintenance = kheish_daemon::RunMemoryMaintenanceStatusView {
            checked_at_ms: 123,
            source: Some("runtime_policy".to_string()),
            pruned_orphan_file_count: 1,
            diagnostics: vec![kheish_daemon::RunMemoryMaintenanceDiagnosticView {
                action: "delete_run_memory_file".to_string(),
                reason: "orphan_file".to_string(),
                run_id: None,
                path: Some("/state/run-memories/__safe/id-x.json".to_string()),
                message: "orphan run-memory file deleted".to_string(),
            }],
            ..Default::default()
        };
        let readyz = ok_probe("ready endpoint is reachable");
        let sse = ok_probe("event stream is reachable");

        let report = doctor_report(&status, &readyz, &sse, None, None);
        assert!(report.ok);
        assert!(report.checks.iter().any(|check| {
            check.name == "run_memory_maintenance"
                && check.severity == "info"
                && check.related_id.as_deref() == Some("/state/run-memories/__safe/id-x.json")
        }));
    }

    #[test]
    fn doctor_hook_diagnostics_rejects_local_http_targets() {
        let hooks = doctor_hook_diagnostics(&kheish_types::HookSettings {
            hooks: std::collections::BTreeMap::from([(
                kheish_types::HookEventName::Notification,
                vec![
                    kheish_types::HookDefinition {
                        name: "localhost".to_string(),
                        matcher: None,
                        failure_policy: Default::default(),
                        executor: kheish_types::HookExecutorConfig::Http {
                            url: "http://localhost:9999/hook".to_string(),
                            timeout_ms: Some(1_000),
                        },
                    },
                    kheish_types::HookDefinition {
                        name: "private-ip".to_string(),
                        matcher: None,
                        failure_policy: Default::default(),
                        executor: kheish_types::HookExecutorConfig::Http {
                            url: "http://10.0.0.1/hook".to_string(),
                            timeout_ms: Some(1_000),
                        },
                    },
                ],
            )]),
        });
        assert_eq!(hooks.error_count, 2);
        assert_eq!(hooks.warning_count, 0);
        assert!(hooks.message.contains("hook configuration error"));
        assert!(hooks.action.is_some());
    }

    #[test]
    fn doctor_hook_diagnostics_uses_shared_runtime_validation() {
        let hooks = doctor_hook_diagnostics(&kheish_types::HookSettings {
            hooks: std::collections::BTreeMap::from([(
                kheish_types::HookEventName::Notification,
                vec![kheish_types::HookDefinition {
                    name: "too-many-retries".to_string(),
                    matcher: None,
                    failure_policy: kheish_types::HookFailurePolicy {
                        max_retries: 4,
                        ..Default::default()
                    },
                    executor: kheish_types::HookExecutorConfig::Command {
                        command: "true".to_string(),
                        shell: None,
                        timeout_ms: Some(1_000),
                    },
                }],
            )]),
        });
        assert_eq!(hooks.error_count, 1);
        assert!(
            hooks.message.contains("shared hook validation failed")
                && hooks.message.contains("max_retries")
        );
        assert_eq!(hooks.related_id.as_deref(), Some("settings"));
    }

    #[tokio::test]
    async fn doctor_http_hook_target_diagnostics_checks_public_ip_literal_without_dns() {
        let hooks = kheish_types::HookSettings {
            hooks: std::collections::BTreeMap::from([(
                kheish_types::HookEventName::Notification,
                vec![kheish_types::HookDefinition {
                    name: "public-v4".to_string(),
                    matcher: None,
                    failure_policy: Default::default(),
                    executor: kheish_types::HookExecutorConfig::Http {
                        url: "https://93.184.216.34/hook".to_string(),
                        timeout_ms: Some(1_000),
                    },
                }],
            )]),
        };
        let diagnostics = doctor_http_hook_target_diagnostics(&hooks).await;
        assert_eq!(diagnostics.checked_count, 1);
        assert_eq!(diagnostics.error_count, 0);
        assert_eq!(diagnostics.warning_count, 0);
        assert!(diagnostics.message.contains("resolve to public addresses"));
        assert_eq!(diagnostics.related_id, None);
    }

    #[test]
    fn doctor_http_hook_ip_filter_matches_ipv6_private_ranges() {
        assert!(kheish_daemon::hook_http_target_blocks_ip(
            "::ffff:127.0.0.1".parse().unwrap()
        ));
        assert!(kheish_daemon::hook_http_target_blocks_ip(
            "::7f00:1".parse().unwrap()
        ));
        assert!(kheish_daemon::hook_http_target_blocks_ip(
            "64:ff9b::192.0.2.1".parse().unwrap()
        ));
        assert!(kheish_daemon::hook_http_target_blocks_ip(
            "2001:db8::1".parse().unwrap()
        ));
    }

    #[test]
    fn doctor_http_hook_target_diagnostics_reports_private_dns_results() {
        let diagnostics = doctor_http_hook_target_diagnostics_from_results(
            1,
            vec![(
                "Notification:rebinding".to_string(),
                Ok(vec!["127.0.0.1:443".parse().unwrap()]),
            )],
        );
        assert_eq!(diagnostics.checked_count, 1);
        assert_eq!(diagnostics.error_count, 1);
        assert_eq!(diagnostics.warning_count, 0);
        assert_eq!(
            diagnostics.related_id.as_deref(),
            Some("Notification:rebinding")
        );
        assert!(
            diagnostics
                .message
                .contains("resolves to private or local address")
        );
        assert!(diagnostics.action.is_some());
    }

    #[test]
    fn doctor_hook_diagnostics_rejects_missing_command_shell() {
        let hooks = doctor_hook_diagnostics(&kheish_types::HookSettings {
            hooks: std::collections::BTreeMap::from([(
                kheish_types::HookEventName::Setup,
                vec![kheish_types::HookDefinition {
                    name: "missing-shell".to_string(),
                    matcher: None,
                    failure_policy: Default::default(),
                    executor: kheish_types::HookExecutorConfig::Command {
                        command: "true".to_string(),
                        shell: Some("/definitely/not/kheish-shell".to_string()),
                        timeout_ms: Some(1_000),
                    },
                }],
            )]),
        });
        assert_eq!(hooks.error_count, 1);
        assert!(hooks.message.contains("command hook shell"));
        assert!(hooks.action.is_some());
    }

    #[test]
    fn doctor_routes_auth_check_rejects_provider_mismatch() {
        let mut view = crate::DoctorRoutesView {
            ok: true,
            source: "runtime".to_string(),
            default_route: Some("google".to_string()),
            route_count: 1,
            auth_checked: true,
            reference_checked: false,
            canary_checked: false,
            routes: vec![crate::DoctorRouteView {
                route_id: "google".to_string(),
                provider: "google".to_string(),
                model: "gemini-2.5-flash".to_string(),
                auth_ref: Some("openai.primary".to_string()),
                auth_kind: "auth_ref".to_string(),
                model_support: None,
                capabilities: None,
                account_auth_slot: None,
                account_auth_provider: None,
                account_auth_file: None,
            }],
            canaries: Vec::new(),
            diagnostics: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
        };
        let statuses = vec![AuthSlotStatus {
            slot_id: AuthSlotId::new("openai.primary"),
            provider: AuthProvider::OpenAi,
            mode: AuthMode::ApiKey,
            summary: "api_key".to_string(),
            updated_at_ms: 1,
            details: Default::default(),
        }];

        apply_auth_ref_status_diagnostics(
            &mut view,
            &[("openai.primary".to_string(), "google".to_string())],
            &statuses,
        );
        refresh_doctor_routes_ok(&mut view);

        assert!(!view.ok);
        assert!(view.errors.iter().any(|error| {
            error.contains("targets provider `openai`") && error.contains("requires `google`")
        }));
        assert!(
            view.diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.code == "auth_ref_provider_mismatch" })
        );
    }

    #[test]
    fn doctor_routes_auth_check_reports_expiry_and_refresh_warnings() {
        let mut view = crate::DoctorRoutesView {
            ok: true,
            source: "runtime".to_string(),
            default_route: Some("openai".to_string()),
            route_count: 1,
            auth_checked: true,
            reference_checked: false,
            canary_checked: false,
            routes: vec![crate::DoctorRouteView {
                route_id: "openai".to_string(),
                provider: "openai".to_string(),
                model: "gpt-5.4".to_string(),
                auth_ref: Some("openai.primary".to_string()),
                auth_kind: "auth_ref".to_string(),
                model_support: None,
                capabilities: None,
                account_auth_slot: None,
                account_auth_provider: None,
                account_auth_file: None,
            }],
            canaries: Vec::new(),
            diagnostics: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
        };
        let statuses = vec![AuthSlotStatus {
            slot_id: AuthSlotId::new("openai.primary"),
            provider: AuthProvider::OpenAi,
            mode: AuthMode::ApiKey,
            summary: "api_key".to_string(),
            updated_at_ms: 1,
            details: std::collections::BTreeMap::from([
                ("expires_at_ms".to_string(), serde_json::json!(1)),
                (
                    "last_refresh_outcome".to_string(),
                    serde_json::json!("failed"),
                ),
            ]),
        }];

        apply_auth_ref_status_diagnostics(
            &mut view,
            &[("openai.primary".to_string(), "openai".to_string())],
            &statuses,
        );
        refresh_doctor_routes_ok(&mut view);

        assert!(!view.ok);
        assert!(view.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "auth_ref_expired"
                && diagnostic.route_id.as_deref() == Some("openai")
        }));
        assert!(view.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "auth_ref_refresh_warning"
                && diagnostic.route_id.as_deref() == Some("openai")
        }));
    }

    #[test]
    fn doctor_routes_file_reports_actionable_warnings() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        std::fs::write(
            &path,
            "version = 1\n\
             default_route = \"openrouter\"\n\
             \n\
             [routes.openrouter]\n\
             driver = \"openai\"\n\
             default_model = \"openai/gpt-5.4-mini\"\n\
             model_support = \"any\"\n\
             api_key = \"test-key\"\n",
        )?;

        let view = doctor_routes_file_view(&path, None);
        assert!(view.ok, "route file should be valid: {:?}", view.errors);
        assert_eq!(view.source, "file");
        assert_eq!(view.default_route.as_deref(), Some("openrouter"));
        assert_eq!(view.route_count, 1);
        assert_eq!(view.routes[0].auth_kind, "inline_api_key");
        assert!(
            view.warnings
                .iter()
                .any(|warning| warning.contains("inline api_key")),
            "warnings should flag inline keys: {:?}",
            view.warnings
        );
        assert_eq!(
            view.diagnostics[0].code, "inline_api_key",
            "inline key warning should be structured"
        );
        Ok(())
    }

    #[test]
    fn doctor_routes_file_reports_invalid_toml_without_panicking() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        std::fs::write(
            &path,
            "version = 1\n\
             default_route = \"missing\"\n\
             \n\
             [routes.openai]\n\
             driver = \"openai\"\n\
             default_model = \"gpt-5.4\"\n",
        )?;

        let view = doctor_routes_file_view(&path, None);
        assert!(!view.ok);
        assert_eq!(view.route_count, 0);
        assert!(
            view.errors
                .iter()
                .any(|error| error.contains("default_route `missing`")),
            "errors should explain the validation failure: {:?}",
            view.errors
        );
        Ok(())
    }

    #[test]
    fn doctor_routes_file_reports_unknown_fields() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        std::fs::write(
            &path,
            "version = 1\n\
             default_route = \"openai\"\n\
             \n\
             [routes.openai]\n\
             driver = \"openai\"\n\
             default_model = \"gpt-5.4\"\n\
             api_key_enb = \"OPENAI_API_KEY\"\n",
        )?;

        let view = doctor_routes_file_view(&path, None);
        assert!(!view.ok);
        assert_eq!(view.route_count, 0);
        assert!(
            view.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "route_file_invalid"
                    && diagnostic
                        .message
                        .contains("invalid syntax or route schema")),
            "diagnostics should explain the strict lint failure: {:?}",
            view.diagnostics
        );
        Ok(())
    }

    #[test]
    fn doctor_routes_file_reports_invalid_base_url_without_leaking_userinfo() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        std::fs::write(
            &path,
            "version = 1\n\
             default_route = \"openai\"\n\
             \n\
             [routes.openai]\n\
             driver = \"openai\"\n\
             default_model = \"gpt-5.4\"\n\
             base_url = \"https://user:secret@example.test/v1/responses\"\n",
        )?;

        let view = doctor_routes_file_view(&path, None);
        assert!(!view.ok);
        assert!(
            view.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "route_file_invalid"
                    && diagnostic
                        .message
                        .contains("base_url must not include userinfo")
            }),
            "diagnostics should explain the unsafe base_url: {:?}",
            view.diagnostics
        );
        let rendered = serde_json::to_string(&view)?;
        assert!(
            !rendered.contains("secret"),
            "doctor routes output should not leak base_url userinfo secrets: {rendered}"
        );
        Ok(())
    }

    #[test]
    fn doctor_routes_file_redacts_malformed_toml_secret_context() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        std::fs::write(
            &path,
            "version = 1\n\
             \n\
             [routes.openai]\n\
             driver = \"openai\"\n\
             default_model = \"gpt-5.4\"\n\
             api_key = \"sk-doctor-secret\n",
        )?;

        let view = doctor_routes_file_view(&path, None);
        assert!(!view.ok);
        let rendered = serde_json::to_string(&view)?;
        assert!(
            rendered.contains("route_file_invalid"),
            "doctor routes should report an invalid route file: {rendered}"
        );
        assert!(
            !rendered.contains("sk-doctor-secret") && !rendered.contains("api_key ="),
            "doctor routes must not leak TOML source context: {rendered}"
        );
        Ok(())
    }

    #[test]
    fn doctor_routes_file_applies_valid_default_route_override() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        std::fs::write(
            &path,
            "version = 1\n\
             default_route = \"openrouter\"\n\
             \n\
             [routes.openrouter]\n\
             driver = \"openai\"\n\
             default_model = \"openai/gpt-5.4-mini\"\n\
             model_support = \"any\"\n\
             api_key = \"test-key\"\n\
             \n\
             [routes.openai]\n\
             driver = \"openai\"\n\
             default_model = \"gpt-5.4\"\n\
             api_key = \"test-key\"\n",
        )?;

        let view = doctor_routes_file_view(&path, Some("openai"));
        assert!(
            view.ok,
            "route file override should be valid: {:?}",
            view.errors
        );
        assert_eq!(view.default_route.as_deref(), Some("openai"));
        assert_eq!(view.route_count, 2);
        Ok(())
    }

    #[test]
    fn doctor_routes_file_rejects_invalid_default_route_override() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        std::fs::write(
            &path,
            "version = 1\n\
             default_route = \"openrouter\"\n\
             \n\
             [routes.openrouter]\n\
             driver = \"openai\"\n\
             default_model = \"openai/gpt-5.4-mini\"\n\
             model_support = \"any\"\n\
             api_key = \"test-key\"\n",
        )?;

        let view = doctor_routes_file_view(&path, Some("missing"));
        assert!(!view.ok);
        assert_eq!(view.default_route.as_deref(), Some("missing"));
        assert!(
            view.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "default_route_missing"
                    && diagnostic.route_id.as_deref() == Some("missing")
            }),
            "diagnostics should flag the invalid override: {:?}",
            view.diagnostics
        );
        Ok(())
    }

    #[test]
    fn doctor_routes_file_override_does_not_rescue_missing_file_default() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        std::fs::write(
            &path,
            "version = 1\n\
             \n\
             [routes.openrouter]\n\
             driver = \"openai\"\n\
             default_model = \"openai/gpt-5.4-mini\"\n\
             model_support = \"any\"\n\
             api_key = \"test-key\"\n\
             \n\
             [routes.openai]\n\
             driver = \"openai\"\n\
             default_model = \"gpt-5.4\"\n\
             api_key = \"test-key\"\n",
        )?;

        let view = doctor_routes_file_view(&path, Some("openai"));
        assert!(!view.ok);
        assert_eq!(view.default_route, None);
        assert!(
            view.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "default_route_unresolved"),
            "diagnostics should preserve the file-level missing default error: {:?}",
            view.diagnostics
        );
        Ok(())
    }

    #[test]
    fn doctor_routes_file_reports_missing_credentials() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        let missing_env = format!("KHEISH_TEST_MISSING_API_KEY_{}", std::process::id());
        let missing_openai = dir.path().join("missing-openai-auth.json");
        std::fs::write(
            &path,
            format!(
                "version = 1\n\
                 default_route = \"openrouter\"\n\
                 \n\
                 [routes.openrouter]\n\
                 driver = \"openrouter\"\n\
                 default_model = \"openai/gpt-5.4-mini\"\n\
                 api_key_env = \"{missing_env}\"\n\
                 \n\
                 [routes.openai]\n\
                 driver = \"openai\"\n\
                 default_model = \"gpt-5.4\"\n\
                 openai_auth_source = \"codex\"\n\
                 openai_auth_file = \"{}\"\n",
                missing_openai.display()
            ),
        )?;

        let view = doctor_routes_file_view(&path, None);
        assert!(!view.ok);
        assert!(view.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "api_key_env_missing"
                && diagnostic.route_id.as_deref() == Some("openrouter")
        }));
        assert!(view.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "openai_auth_file_missing"
                && diagnostic.route_id.as_deref() == Some("openai")
        }));
        Ok(())
    }

    #[test]
    fn doctor_routes_file_reports_custom_routes_without_credentials() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        std::fs::write(
            &path,
            "version = 1\n\
             default_route = \"custom-openai\"\n\
             \n\
             [routes.custom-openai]\n\
             driver = \"openai\"\n\
             default_model = \"gpt-5.4\"\n\
             \n\
             [routes.custom-openrouter]\n\
             driver = \"openrouter\"\n\
             default_model = \"openai/gpt-5.4-mini\"\n\
             \n\
             [routes.custom-google]\n\
             driver = \"google\"\n\
             default_model = \"gemini-3-pro\"\n\
             \n\
             [routes.custom-xai]\n\
             driver = \"xai\"\n\
             default_model = \"grok-4\"\n\
             \n\
             [routes.custom-anthropic]\n\
             driver = \"anthropic\"\n\
             default_model = \"claude-opus-4-6\"\n",
        )?;

        let view = doctor_routes_file_view(&path, None);
        assert!(!view.ok);
        for route_id in [
            "custom-openai",
            "custom-openrouter",
            "custom-google",
            "custom-xai",
            "custom-anthropic",
        ] {
            assert!(
                view.diagnostics.iter().any(|diagnostic| {
                    diagnostic.code == "route_credentials_missing"
                        && diagnostic.route_id.as_deref() == Some(route_id)
                }),
                "missing custom credential diagnostic for {route_id}: {:?}",
                view.diagnostics
            );
        }
        Ok(())
    }

    #[test]
    fn doctor_routes_file_reports_empty_api_key_env() -> Result<()> {
        let diagnostic =
            api_key_env_diagnostic("openrouter", "KHEISH_EMPTY_KEY", Ok("  ".to_string()))
                .expect("empty env value should produce a diagnostic");
        assert_eq!(diagnostic.code, "api_key_env_empty");
        assert_eq!(diagnostic.route_id.as_deref(), Some("openrouter"));
        assert!(diagnostic.message.contains("KHEISH_EMPTY_KEY"));
        Ok(())
    }

    #[test]
    fn doctor_routes_file_reports_missing_codex_account_auth_without_slot_or_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        let missing_openai = dir.path().join("missing-openai-auth.json");
        std::fs::write(
            &path,
            format!(
                "version = 1\n\
                 \n\
                 [routes.openai]\n\
                 driver = \"openai\"\n\
                 default_model = \"gpt-5.4\"\n\
                 openai_auth_source = \"codex\"\n\
                 openai_auth_file = \"{}\"\n",
                missing_openai.display()
            ),
        )?;

        let mut view = doctor_routes_file_view(&path, None);
        let account_auth_refs = view
            .routes
            .iter()
            .filter_map(|route| {
                Some((
                    route.route_id.clone(),
                    route.account_auth_slot.clone()?,
                    route.account_auth_provider?,
                    route.account_auth_file.clone(),
                ))
            })
            .collect::<Vec<_>>();
        apply_account_auth_status_diagnostics(&mut view, &account_auth_refs, &[]);
        refresh_doctor_routes_ok(&mut view);

        assert!(!view.ok);
        assert!(view.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "account_auth_missing"
                && diagnostic.route_id.as_deref() == Some("openai")
                && diagnostic.message.contains("slot `route.openai`")
        }));
        Ok(())
    }

    #[test]
    fn doctor_routes_file_reports_malformed_codex_account_auth_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        let codex_auth = dir.path().join("auth.json");
        std::fs::write(&codex_auth, "not-json")?;
        std::fs::write(
            &path,
            format!(
                "version = 1\n\
                 \n\
                 [routes.openai]\n\
                 driver = \"openai\"\n\
                 default_model = \"gpt-5.4\"\n\
                 openai_auth_source = \"codex\"\n\
                 openai_auth_file = \"{}\"\n",
                codex_auth.display()
            ),
        )?;

        let view = doctor_routes_file_view(&path, None);
        assert!(!view.ok);
        assert!(
            view.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "openai_auth_file_invalid"
                    && diagnostic.route_id.as_deref() == Some("openai")
                    && diagnostic
                        .message
                        .contains("Codex auth file is not valid JSON")
                    && !diagnostic.message.contains("not-json")
            }),
            "diagnostics should reject malformed Codex auth without leaking content: {:?}",
            view.diagnostics
        );
        Ok(())
    }

    #[test]
    fn doctor_routes_file_redacts_unsupported_codex_auth_mode() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        let codex_auth = dir.path().join("auth.json");
        let auth_mode = format!("{}{}", "sk-", "doctor-secret-auth-mode");
        std::fs::write(
            &codex_auth,
            format!(
                r#"{{"auth_mode":"{auth_mode}","tokens":{{"refresh_token":"refresh-token"}}}}"#
            ),
        )?;
        std::fs::write(
            &path,
            format!(
                "version = 1\n\
                 \n\
                 [routes.openai]\n\
                 driver = \"openai\"\n\
                 default_model = \"gpt-5.4\"\n\
                 openai_auth_source = \"codex\"\n\
                 openai_auth_file = \"{}\"\n",
                codex_auth.display()
            ),
        )?;

        let view = doctor_routes_file_view(&path, None);
        assert!(!view.ok);
        let rendered = serde_json::to_string(&view)?;
        assert!(
            rendered.contains("unsupported account auth mode"),
            "diagnostics should explain the sanitized auth mode failure: {rendered}"
        );
        assert!(
            !rendered.contains(&auth_mode) && !rendered.contains("refresh-token"),
            "doctor routes must not leak credential file contents: {rendered}"
        );
        Ok(())
    }

    #[test]
    fn doctor_routes_file_reports_malformed_claude_code_credentials_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        let credentials = dir.path().join("credentials.json");
        std::fs::write(&credentials, "{")?;
        std::fs::write(
            &path,
            format!(
                "version = 1\n\
                 \n\
                 [routes.anthropic]\n\
                 driver = \"anthropic\"\n\
                 default_model = \"claude-opus-4-6\"\n\
                 anthropic_auth_source = \"claude_code\"\n\
                 anthropic_credentials_file = \"{}\"\n",
                credentials.display()
            ),
        )?;

        let view = doctor_routes_file_view(&path, None);
        assert!(!view.ok);
        assert!(
            view.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "anthropic_credentials_file_invalid"
                    && diagnostic.route_id.as_deref() == Some("anthropic")
                    && diagnostic
                        .message
                        .contains("Claude Code credentials file is not valid JSON")
            }),
            "diagnostics should reject malformed Claude Code credentials: {:?}",
            view.diagnostics
        );
        Ok(())
    }

    #[test]
    fn doctor_routes_file_rejects_codex_media_capability_override() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        std::fs::write(
            &path,
            "version = 1\n\
             \n\
             [routes.openai]\n\
             driver = \"openai\"\n\
             default_model = \"gpt-5.4\"\n\
             openai_auth_source = \"codex\"\n\
             audio_generation = true\n",
        )?;

        let view = doctor_routes_file_view(&path, None);
        assert!(!view.ok);
        assert!(
            view.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "openai_account_auth_media_unsupported"
                    && diagnostic.route_id.as_deref() == Some("openai")
            }),
            "doctor routes should mirror serve rejection for Codex media overrides: {:?}",
            view.diagnostics
        );
        Ok(())
    }

    #[test]
    fn doctor_routes_file_rejects_unsupported_capability_overrides() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("routes.toml");
        std::fs::write(
            &path,
            "version = 1\n\
             \n\
             [routes.anthropic]\n\
             driver = \"anthropic\"\n\
             default_model = \"claude-opus-4-6\"\n\
             api_key = \"test-key\"\n\
             image_generation = true\n",
        )?;

        let view = doctor_routes_file_view(&path, None);
        assert!(!view.ok);
        assert!(
            view.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "unsupported_capability_override"
                    && diagnostic.route_id.as_deref() == Some("anthropic")
            }),
            "diagnostics should flag unsupported image_generation: {:?}",
            view.diagnostics
        );
        Ok(())
    }

    #[test]
    fn doctor_routes_runtime_reports_missing_inventory() {
        let view = doctor_routes_runtime_view(&status_fixture());
        assert!(!view.ok);
        assert_eq!(view.source, "runtime");
        assert_eq!(view.route_count, 0);
        assert!(
            view.errors
                .iter()
                .any(|error| error.contains("no configured routes")),
            "errors should explain missing routes: {:?}",
            view.errors
        );
    }

    #[test]
    fn doctor_routes_runtime_reports_loaded_route_file_drift() -> Result<()> {
        let temp = tempfile::TempDir::new()?;
        let state_root = temp.path().join("state");
        let routes_path = temp.path().join("routes.toml");
        std::fs::create_dir_all(&state_root)?;
        std::fs::write(
            &routes_path,
            r#"version = 1
default_route = "openai"

[routes.openai]
driver = "openai"
default_model = "gpt-5.4"
api_key = "test-key"
"#,
        )?;
        let loaded_routes = [kheish_daemon::ConfiguredModelRoute::new(
            "openai",
            kheish_daemon::ModelRouteConfig::OpenAi(kheish_runtime::OpenAiProviderConfig::new(
                "gpt-5.4", "test-key",
            )),
        )];
        crate::cli::write_route_inventory_metadata(
            &state_root,
            Some(routes_path.as_path()),
            &loaded_routes,
        )?;
        std::fs::write(
            &routes_path,
            r#"version = 1
default_route = "openai"

[routes.openai]
driver = "openai"
default_model = "gpt-5.4-mini"
api_key = "test-key"
"#,
        )?;

        let mut status = status_fixture();
        status.runtime.state_root = Some(state_root.display().to_string());
        status.runtime.default_route = Some(kheish_daemon::ResolvedModelRoute {
            route_id: "openai".to_string(),
            provider: "openai".to_string(),
            model: "gpt-5.4".to_string(),
            auth_ref: None,
            capabilities: kheish_daemon::RouteCapabilities {
                matrix_version: kheish_daemon::ROUTE_CAPABILITY_MATRIX_VERSION,
                multimodal_input: true,
                native_web_search: true,
                image_generation: true,
                image_edit: true,
                audio_generation: true,
                transcription: true,
            },
        });
        status.runtime.routes = vec![status.runtime.default_route.clone().expect("route")];

        let view = doctor_routes_runtime_view(&status);

        assert!(view.ok, "route-file drift is a warning: {:?}", view.errors);
        assert!(view.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "route_file_drift"
                && diagnostic.message.contains("loaded sha256")
                && diagnostic.message.contains("current sha256")
        }));
        Ok(())
    }

    #[test]
    fn legacy_daemon_status_view_reconstructs_minimal_snapshot() {
        let fixture = status_fixture();
        let status = legacy_daemon_status_view(
            serde_json::json!({
                "status": "ready",
                "ready": true
            }),
            fixture.capabilities,
            fixture.runtime,
            Vec::new(),
        )
        .expect("legacy status should convert");

        assert_eq!(status.status, kheish_daemon::DaemonReadinessState::Ready);
        assert!(status.ready);
        assert_eq!(status.capabilities.control_plane_version, "test");
        assert_eq!(status.sessions.total, 0);
        assert_eq!(status.runs.pending_approval_count, 0);
        assert_eq!(status.tasks.live_background_shell_task_count, 0);
    }

    #[test]
    fn legacy_status_shape_rejects_malformed_new_status_payloads() {
        assert!(is_legacy_status_shape(&serde_json::json!({
            "status": "ready",
            "ready": true,
            "capabilities": {
                "control_plane_version": "0.1.0",
                "approvals": true,
                "sidechains": true,
                "mailboxes": true,
                "session_events": true,
                "restart_restore": true,
                "live_events": true
            }
        })));
        assert!(!is_legacy_status_shape(&serde_json::json!({
            "status": "ready",
            "ready": true,
            "capabilities": {},
            "runtime": {}
        })));
        assert!(!is_legacy_status_shape(&serde_json::json!({
            "status": "ready",
            "ready": "yes"
        })));
        assert!(!is_legacy_status_shape(&serde_json::json!({
            "status": "broken",
            "ready": true
        })));
    }
}

/// Handles `runtime ...`.
pub(crate) async fn run_runtime_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::RuntimeCommand,
) -> Result<()> {
    match command {
        crate::RuntimeCommand::Get => {
            let runtime = client
                .get_json::<kheish_daemon::RuntimeSettingsView>("/v1/runtime")
                .await?;
            printer.print(&runtime)
        }
        crate::RuntimeCommand::Permissions { command } => match command {
            crate::RuntimePermissionsCommand::Check(args) => {
                let input = serde_json::from_str::<serde_json::Value>(&args.input_json)
                    .map_err(|error| anyhow::anyhow!("invalid --input-json: {error}"))?;
                let explanation = client
                    .post_json::<_, kheish_runtime::PermissionExplanation>(
                        "/v1/runtime/permissions/check",
                        &kheish_daemon::CheckPermissionRequest {
                            tool_name: args.tool_name,
                            tool_call_id: args.tool_call_id,
                            session_id: args.session_id,
                            mode_override: args.mode.map(Into::into),
                            input,
                        },
                    )
                    .await?;
                printer.print(&explanation)
            }
            crate::RuntimePermissionsCommand::Matrix(args) => {
                let matrix = client
                    .post_json::<_, kheish_daemon::PermissionMatrixView>(
                        "/v1/runtime/permissions/matrix",
                        &kheish_daemon::CheckPermissionMatrixRequest {
                            session_id: args.session_id,
                        },
                    )
                    .await?;
                printer.print(&matrix)
            }
        },
        crate::RuntimeCommand::LearningPolicy { command } => match command {
            crate::RuntimeLearningPolicyCommand::Get => {
                let settings = client
                    .get_json::<kheish_daemon::LearningAutomationPolicyConfig>(
                        "/v1/runtime/learning-policy",
                    )
                    .await?;
                printer.print(&settings)
            }
            crate::RuntimeLearningPolicyCommand::Set(args) => {
                let settings = if args.reset {
                    kheish_daemon::LearningAutomationPolicyConfig::default()
                } else {
                    crate::cli::read_json_input::<kheish_daemon::LearningAutomationPolicyConfig>(
                        None,
                        args.file.as_deref(),
                        args.stdin,
                    )
                    .await?
                };
                let mut payload = serde_json::to_value(&settings)?;
                let payload_object = payload.as_object_mut().ok_or_else(|| {
                    anyhow::anyhow!("learning policy must serialize as an object")
                })?;
                if let Some(expected_revision) = args.expected_revision {
                    payload_object.insert(
                        "expected_revision".to_string(),
                        Value::from(expected_revision),
                    );
                }
                let runtime = client
                    .post_json::<_, kheish_daemon::RuntimeSettingsView>(
                        "/v1/runtime/learning-policy",
                        &payload,
                    )
                    .await?;
                printer.print(&runtime)
            }
        },
        crate::RuntimeCommand::RunMemoryPolicy { command } => match command {
            crate::RuntimeRunMemoryPolicyCommand::Get => {
                let settings = client
                    .get_json::<kheish_daemon::RunMemoryPolicyConfig>(
                        "/v1/runtime/run-memory-policy",
                    )
                    .await?;
                printer.print(&settings)
            }
            crate::RuntimeRunMemoryPolicyCommand::Set(args) => {
                let settings = if args.reset {
                    kheish_daemon::RunMemoryPolicyConfig::default()
                } else {
                    crate::cli::read_json_input::<kheish_daemon::RunMemoryPolicyConfig>(
                        None,
                        args.file.as_deref(),
                        args.stdin,
                    )
                    .await?
                };
                settings.validate()?;
                let runtime = client
                    .post_json::<_, kheish_daemon::RuntimeSettingsView>(
                        "/v1/runtime/run-memory-policy",
                        &kheish_daemon::SetRunMemoryPolicyRequest {
                            policy: settings,
                            expected_revision: args.expected_revision,
                        },
                    )
                    .await?;
                printer.print(&runtime)
            }
        },
        crate::RuntimeCommand::ToolLimits { command } => match command {
            crate::RuntimeToolLimitsCommand::Get => {
                let settings = client
                    .get_json::<kheish_runtime::ToolRuntimeLimits>("/v1/runtime/tool-limits")
                    .await?;
                printer.print(&settings)
            }
            crate::RuntimeToolLimitsCommand::Set(args) => {
                let limits = if args.reset {
                    kheish_runtime::ToolRuntimeLimits::default()
                } else {
                    crate::cli::read_json_input::<kheish_runtime::ToolRuntimeLimits>(
                        None,
                        args.file.as_deref(),
                        args.stdin,
                    )
                    .await?
                };
                limits.validate()?;
                let runtime = client
                    .post_json::<_, kheish_daemon::RuntimeSettingsView>(
                        "/v1/runtime/tool-limits",
                        &kheish_daemon::SetToolRuntimeLimitsRequest {
                            limits,
                            expected_revision: args.expected_revision,
                        },
                    )
                    .await?;
                printer.print(&runtime)
            }
        },
        crate::RuntimeCommand::SubagentPolicy { command } => match command {
            crate::RuntimeSubagentPolicyCommand::Quotas => {
                let settings = client
                    .get_json::<kheish_daemon::SubagentPolicyStatusView>(
                        "/v1/runtime/subagent-policy/quotas",
                    )
                    .await?;
                printer.print(&settings)
            }
        },
        crate::RuntimeCommand::Hooks { command } => match command {
            crate::RuntimeHooksCommand::Get => {
                let settings = client
                    .get_json::<kheish_types::HookSettings>("/v1/runtime/hooks")
                    .await?;
                printer.print(&settings)
            }
            crate::RuntimeHooksCommand::DeadLetter => {
                let records = client
                    .get_json::<Vec<kheish_daemon::HookDeadLetterView>>(
                        "/v1/runtime/hooks/dead-letter",
                    )
                    .await?;
                printer.print(&records)
            }
            crate::RuntimeHooksCommand::ResolveDeadLetter {
                dead_letter_id,
                reason,
            } => {
                let dead_letter_id = crate::cli::url_encode_path_segment(&dead_letter_id);
                let record = client
                    .post_json::<_, kheish_daemon::HookDeadLetterView>(
                        &format!("/v1/runtime/hooks/dead-letter/{dead_letter_id}/resolve"),
                        &kheish_daemon::ResolveHookDeadLetterRequest {
                            reason: Some(reason),
                        },
                    )
                    .await?;
                printer.print(&record)
            }
            crate::RuntimeHooksCommand::Set(args) => {
                let settings = if args.reset {
                    kheish_types::HookSettings::default()
                } else {
                    crate::cli::read_json_input::<kheish_types::HookSettings>(
                        None,
                        args.file.as_deref(),
                        args.stdin,
                    )
                    .await?
                };
                let payload = if settings.hooks.is_empty() {
                    serde_json::to_value(&kheish_daemon::SetHooksRequest {
                        settings,
                        expected_revision: args.expected_revision,
                        skip_hooks: args.skip_hooks,
                    })?
                } else {
                    let mut payload = serde_json::to_value(&settings)?;
                    let payload_object = payload.as_object_mut().ok_or_else(|| {
                        anyhow::anyhow!("hook settings must serialize as an object")
                    })?;
                    if let Some(expected_revision) = args.expected_revision {
                        payload_object.insert(
                            "expected_revision".to_string(),
                            Value::from(expected_revision),
                        );
                    }
                    if args.skip_hooks {
                        payload_object.insert("skip_hooks".to_string(), Value::Bool(true));
                    }
                    payload
                };
                let runtime = client
                    .post_json::<_, kheish_daemon::RuntimeSettingsView>(
                        "/v1/runtime/hooks",
                        &payload,
                    )
                    .await?;
                printer.print(&runtime)
            }
        },
        crate::RuntimeCommand::Auth { command } => match command {
            crate::RuntimeAuthCommand::Accounts { command } => match command {
                crate::RuntimeAuthAccountsCommand::List => {
                    let statuses = client
                        .get_json::<Vec<kheish_auth::AuthSlotStatus>>("/v1/runtime/auth/accounts")
                        .await?;
                    printer.print(&statuses)
                }
                crate::RuntimeAuthAccountsCommand::Get { slot_id } => {
                    let status = client
                        .get_json::<kheish_auth::AuthSlotStatus>(&format!(
                            "/v1/runtime/auth/accounts/{slot_id}"
                        ))
                        .await?;
                    printer.print(&status)
                }
                crate::RuntimeAuthAccountsCommand::Refresh { slot_id } => {
                    let status = client
                        .post_empty_json::<kheish_auth::AuthSlotStatus>(&format!(
                            "/v1/runtime/auth/accounts/{slot_id}/refresh"
                        ))
                        .await?;
                    printer.print(&status)
                }
                crate::RuntimeAuthAccountsCommand::Revoke { slot_id } => {
                    let status = client
                        .post_empty_json::<serde_json::Value>(&format!(
                            "/v1/runtime/auth/accounts/{slot_id}/revoke"
                        ))
                        .await?;
                    printer.print(&status)
                }
            },
            crate::RuntimeAuthCommand::Subject { subject_id } => {
                let status = client
                    .get_json::<kheish_auth::AuthSubjectStatus>(&format!(
                        "/v1/runtime/auth/subjects/{subject_id}"
                    ))
                    .await?;
                printer.print(&status)
            }
            crate::RuntimeAuthCommand::RevokeSubject { subject_id } => {
                let status = client
                    .post_empty_json::<kheish_auth::AuthSubjectStatus>(&format!(
                        "/v1/runtime/auth/subjects/{subject_id}/revoke"
                    ))
                    .await?;
                printer.print(&status)
            }
            crate::RuntimeAuthCommand::Lease { lease_id } => {
                let status = client
                    .get_json::<kheish_auth::CredentialLeaseStatus>(&format!(
                        "/v1/runtime/auth/leases/{lease_id}"
                    ))
                    .await?;
                printer.print(&status)
            }
            crate::RuntimeAuthCommand::RevokeLease { lease_id } => {
                let status = client
                    .post_empty_json::<kheish_auth::CredentialLeaseStatus>(&format!(
                        "/v1/runtime/auth/leases/{lease_id}/revoke"
                    ))
                    .await?;
                printer.print(&status)
            }
            crate::RuntimeAuthCommand::RevokeSlot { slot_id } => {
                let status = client
                    .post_empty_json::<serde_json::Value>(&format!(
                        "/v1/runtime/auth/slots/{slot_id}/revoke"
                    ))
                    .await?;
                printer.print(&status)
            }
        },
        crate::RuntimeCommand::Revisions => {
            let revisions = client
                .get_json::<kheish_daemon::RuntimeConfigRevisionListResponse>(
                    "/v1/runtime/revisions",
                )
                .await?;
            printer.print(&revisions)
        }
        crate::RuntimeCommand::Rollback {
            target_revision,
            expected_revision,
            skip_hooks,
        } => {
            let runtime = client
                .post_json::<_, kheish_daemon::RuntimeSettingsView>(
                    "/v1/runtime/rollback",
                    &kheish_daemon::RuntimeRollbackRequest {
                        target_revision,
                        expected_revision,
                        skip_hooks,
                    },
                )
                .await?;
            printer.print(&runtime)
        }
        crate::RuntimeCommand::SetModel {
            model,
            expected_revision,
        } => {
            let route_ids = crate::cli::fetch_known_route_ids(client).await?;
            let selector = crate::cli::parse_model_selector(&model, &route_ids)?;
            let runtime = client
                .post_json::<_, kheish_daemon::RuntimeSettingsView>(
                    "/v1/runtime/model",
                    &kheish_daemon::SetModelRequest {
                        provider: selector.provider,
                        model: selector.model,
                        expected_revision,
                    },
                )
                .await?;
            printer.print(&runtime)
        }
        crate::RuntimeCommand::SetPermissionMode {
            mode,
            expected_revision,
        } => {
            let runtime = client
                .post_json::<_, kheish_daemon::RuntimeSettingsView>(
                    "/v1/runtime/permission-mode",
                    &kheish_daemon::SetPermissionModeRequest {
                        mode: mode.into(),
                        expected_revision,
                    },
                )
                .await?;
            printer.print(&runtime)
        }
        crate::RuntimeCommand::SetDebugLevel {
            level,
            expected_revision,
        } => {
            let runtime = client
                .post_json::<_, kheish_daemon::RuntimeSettingsView>(
                    "/v1/runtime/debug-level",
                    &kheish_daemon::SetDebugLevelRequest {
                        level: level.into(),
                        expected_revision,
                    },
                )
                .await?;
            printer.print(&runtime)
        }
        crate::RuntimeCommand::SetSystemPrompt(args) => {
            let (mut settings, base_revision) = if args.reset {
                (kheish_runtime::SystemPromptSettings::default(), None)
            } else {
                let runtime = client
                    .get_json::<kheish_daemon::RuntimeSettingsView>("/v1/runtime")
                    .await?;
                (runtime.system_prompt, Some(runtime.config.revision))
            };

            if args.clear_text {
                settings.override_prompt = None;
                settings.custom_prompt = None;
                settings.append_prompt = None;
            }
            if args.clear_language {
                settings.language = None;
            }
            if args.clear_output_style {
                settings.output_style = None;
            }
            if let Some(language) = args.language {
                settings.language = Some(language);
            }
            if let Some(output_style) = args.output_style {
                settings.output_style = Some(output_style);
            }

            let has_prompt_input =
                args.content.is_some() || args.content_file.is_some() || args.stdin;
            if let Some(mode) = args.mode {
                settings.override_prompt = None;
                settings.custom_prompt = None;
                settings.append_prompt = None;
                match mode {
                    crate::SystemPromptModeArg::Default => {}
                    crate::SystemPromptModeArg::Custom
                    | crate::SystemPromptModeArg::Append
                    | crate::SystemPromptModeArg::Override => {
                        let content = crate::cli::read_text_input(
                            args.content,
                            args.content_file.as_deref(),
                            args.stdin,
                        )
                        .await?;
                        match mode {
                            crate::SystemPromptModeArg::Custom => {
                                settings.custom_prompt = Some(content);
                            }
                            crate::SystemPromptModeArg::Append => {
                                settings.append_prompt = Some(content);
                            }
                            crate::SystemPromptModeArg::Override => {
                                settings.override_prompt = Some(content);
                            }
                            crate::SystemPromptModeArg::Default => {}
                        }
                    }
                }
            } else if has_prompt_input {
                bail!("set-system-prompt content requires --mode");
            }

            let runtime = client
                .post_json::<_, kheish_daemon::RuntimeSettingsView>(
                    "/v1/runtime/system-prompt",
                    &kheish_daemon::SetSystemPromptRequest {
                        settings,
                        expected_revision: args.expected_revision.or(base_revision),
                    },
                )
                .await?;
            printer.print(&runtime)
        }
    }
}

/// Handles `events ...`.
pub(crate) async fn run_events_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::EventsCommand,
) -> Result<()> {
    match command {
        crate::EventsCommand::Stream {
            session_id,
            run_id,
            cursor,
        } => {
            let mut params = Vec::new();
            if let Some(cursor) = cursor {
                params.push(format!("cursor={cursor}"));
            }
            let suffix = if params.is_empty() {
                String::new()
            } else {
                format!("?{}", params.join("&"))
            };
            match (session_id, run_id) {
                (Some(session_id), None) => {
                    let session_id = crate::cli::url_encode_path_segment(&session_id);
                    client
                        .stream_events(
                            &format!("/v1/sessions/{session_id}/stream{suffix}"),
                            printer,
                        )
                        .await
                }
                (None, Some(run_id)) => {
                    let run_id = crate::cli::url_encode_path_segment(&run_id);
                    client
                        .stream_events(&format!("/v1/runs/{run_id}/stream{suffix}"), printer)
                        .await
                }
                (session_id, run_id) => {
                    if let Some(session_id) = session_id.filter(|value| !value.trim().is_empty()) {
                        params.push(format!(
                            "session_id={}",
                            crate::cli::url_encode_component(&session_id)
                        ));
                    }
                    if let Some(run_id) = run_id.filter(|value| !value.trim().is_empty()) {
                        params.push(format!(
                            "run_id={}",
                            crate::cli::url_encode_component(&run_id)
                        ));
                    }
                    let mut path = "/v1/events/stream".to_string();
                    if !params.is_empty() {
                        path.push('?');
                        path.push_str(&params.join("&"));
                    }
                    client.stream_events(&path, printer).await
                }
            }
        }
    }
}
