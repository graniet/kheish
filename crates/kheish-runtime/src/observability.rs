use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::debug::{DebugArtifactFormat, DebugCaptureLevel};
use crate::execution::current_execution_scope;

/// A single trace event emitted by the runtime.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceEvent {
    /// The event timestamp in milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// The optional session identifier associated with the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The optional agent identifier associated with the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// The optional daemon run identifier associated with the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The stable principal identifier associated with the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    /// The parent principal identifier when the event originated from delegated work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_principal_id: Option<String>,
    /// The credential grant that authorized the external action when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
    /// The originating tool call identifier when the event happened inside one tool invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The event payload.
    pub kind: TraceEventKind,
}

impl TraceEvent {
    /// Creates a timestamped trace event.
    pub fn new(kind: TraceEventKind) -> Self {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let scope = current_execution_scope();
        Self {
            timestamp_ms,
            session_id: scope.as_ref().map(|scope| scope.session_id.clone()),
            agent_id: scope.as_ref().and_then(|scope| scope.agent_id.clone()),
            run_id: scope.as_ref().and_then(|scope| scope.run_id.clone()),
            principal_id: scope.as_ref().and_then(|scope| scope.principal_id.clone()),
            parent_principal_id: scope
                .as_ref()
                .and_then(|scope| scope.parent_principal_id.clone()),
            grant_id: scope.as_ref().and_then(|scope| scope.grant_id.clone()),
            tool_call_id: scope.and_then(|scope| scope.tool_call_id),
            kind,
        }
    }
}

/// One debug artifact emitted by runtime or provider services.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DebugArtifact {
    /// The event timestamp in milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// The optional session identifier associated with the artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The optional agent identifier associated with the artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// The optional daemon run identifier associated with the artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The capture level used when the artifact was emitted.
    pub level: DebugCaptureLevel,
    /// The optional turn associated with the artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<usize>,
    /// The optional attempt associated with the artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<usize>,
    /// The stable artifact name.
    pub name: String,
    /// The serialization format of the stored artifact.
    pub format: DebugArtifactFormat,
    /// The provider-neutral payload.
    pub payload: serde_json::Value,
}

impl DebugArtifact {
    /// Creates a timestamped debug artifact scoped to the current execution.
    pub fn new(
        level: DebugCaptureLevel,
        turn: Option<usize>,
        attempt: Option<usize>,
        name: impl Into<String>,
        format: DebugArtifactFormat,
        payload: serde_json::Value,
    ) -> Self {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let scope = current_execution_scope();
        Self {
            timestamp_ms,
            session_id: scope.as_ref().map(|scope| scope.session_id.clone()),
            agent_id: scope.as_ref().and_then(|scope| scope.agent_id.clone()),
            run_id: scope.and_then(|scope| scope.run_id),
            level,
            turn,
            attempt,
            name: name.into(),
            format,
            payload,
        }
    }
}

/// The supported trace event kinds emitted by Kheish runtime services.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TraceEventKind {
    /// A model attempt started.
    ModelAttemptStarted { turn: usize, attempt: usize },
    /// A model attempt completed.
    ModelAttemptFinished { turn: usize, attempt: usize },
    /// A model retry was scheduled.
    ModelRetryScheduled {
        turn: usize,
        attempt: usize,
        reason: String,
    },
    /// A tool execution started.
    ToolStarted { tool_name: String, call_id: String },
    /// A tool execution finished.
    ToolFinished {
        tool_name: String,
        call_id: String,
        is_error: bool,
    },
    /// A permission rule was evaluated.
    PermissionEvaluated { tool_name: String, decision: String },
    /// A session record was persisted.
    SessionPersisted { record_type: String },
    /// An output envelope was dispatched.
    OutputDispatched { plugin: String },
    /// A sub-agent was spawned.
    AgentSpawned { agent_id: String },
    /// One external request or response crossed the daemon boundary.
    ExternalAction {
        phase: String,
        kind: String,
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_digest: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_digest: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<String>,
    },
}

/// Builds one trace event that represents one external action crossing the daemon boundary.
pub fn external_action_trace(
    phase: impl Into<String>,
    kind: impl Into<String>,
    target: impl Into<String>,
    request_digest: Option<String>,
    response_digest: Option<String>,
    outcome: Option<String>,
) -> TraceEvent {
    TraceEvent::new(TraceEventKind::ExternalAction {
        phase: phase.into(),
        kind: kind.into(),
        target: target.into(),
        request_digest,
        response_digest,
        outcome,
    })
}

/// Builds one external-action trace and attaches the credential grant that authorized it.
pub fn external_action_trace_with_grant_id(
    phase: impl Into<String>,
    kind: impl Into<String>,
    target: impl Into<String>,
    request_digest: Option<String>,
    response_digest: Option<String>,
    outcome: Option<String>,
    grant_id: Option<String>,
) -> TraceEvent {
    let mut event = external_action_trace(
        phase,
        kind,
        target,
        request_digest,
        response_digest,
        outcome,
    );
    if grant_id.is_some() {
        event.grant_id = grant_id;
    }
    event
}

/// Builds one bounded categorical failure outcome safe for append-only audit records.
pub fn failed_external_action_outcome(message: impl AsRef<str>) -> String {
    let normalized = message
        .as_ref()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty() {
        return "failed".to_string();
    }
    let lower = normalized.to_ascii_lowercase();
    let class = if lower.contains("private network")
        || lower.contains("localhost/private")
        || lower.contains("metadata")
    {
        "blocked_private_network"
    } else if lower.contains("containing credentials") || lower.contains("url credentials") {
        "blocked_credentials"
    } else if lower.contains("larger than") || lower.contains("exceeded") {
        "response_too_large"
    } else if lower.contains("non-text content type")
        || lower.contains("non-json content type")
        || lower.contains("unsupported content type")
    {
        "unsupported_content_type"
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "timeout"
    } else if lower.contains("run interrupted") || lower.contains("cancelled") {
        "interrupted"
    } else if lower.contains("exit") || lower.contains("status code") {
        "exit_status"
    } else if lower.contains("401") || lower.contains("unauthorized") {
        "unauthorized"
    } else if lower.contains("403") || lower.contains("forbidden") || lower.contains("permission") {
        "permission"
    } else if lower.contains("status") || lower.contains("http ") {
        "status"
    } else if lower.contains("connect") || lower.contains("dns") || lower.contains("resolve") {
        "connect"
    } else if lower.contains("decode")
        || lower.contains("parse")
        || lower.contains("json")
        || lower.contains("utf-8")
    {
        "decode"
    } else if lower.contains("invalid") || lower.contains("unsupported") {
        "invalid_request"
    } else {
        "internal"
    };
    format!("failed:{class}")
}

/// Builds one categorical external-action outcome from a reqwest error.
pub fn failed_reqwest_external_action_outcome(error: &reqwest::Error) -> String {
    let class = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_status() {
        "status"
    } else if error.is_decode() {
        "decode"
    } else if error.is_request() {
        "request"
    } else {
        "transport"
    };
    format!("failed:{class}")
}

/// Returns one host-only URL target or a short digest for malformed values.
pub fn safe_url_audit_target(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(parsed) => {
            let Some(host) = parsed.host_str() else {
                return "unknown-host".to_string();
            };
            let mut target = format!("{}://{}", parsed.scheme(), host);
            if let Some(port) = parsed.port() {
                target.push(':');
                target.push_str(&port.to_string());
            }
            target
        }
        Err(_) => {
            let digest = kheish_codec::digest_text(url);
            format!(
                "invalid_url_sha256:{}",
                digest.get(..16).unwrap_or(digest.as_str())
            )
        }
    }
}

/// Returns a debug-friendly URL without userinfo, query, or fragment secret material.
pub fn safe_url_debug_target(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut parsed) => {
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.set_query(None);
            parsed.set_fragment(None);
            parsed.to_string()
        }
        Err(_) => {
            let digest = kheish_codec::digest_text(url);
            format!(
                "invalid_url_sha256:{}",
                digest.get(..16).unwrap_or(digest.as_str())
            )
        }
    }
}

/// A point-in-time metrics snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    /// Monotonic counter values keyed by metric name.
    pub counters: BTreeMap<String, u64>,
}

/// A diff between two metrics snapshots.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceDiff {
    /// Counter deltas keyed by metric name.
    pub counter_deltas: BTreeMap<String, i64>,
}

/// Runtime counter incremented when recovered run memory is omitted by final prompt packing.
pub const RUN_MEMORY_PROMPT_BUDGET_OMITTED_COUNTER: &str = "run_memory.prompt_budget_omitted_total";

/// Runtime counter incremented for recovered run-memory entries rendered into the final prompt.
pub const RUN_MEMORY_PROMPT_INJECTED_COUNTER: &str = "run_memory.prompt_injected_total";

/// Runtime counter incremented when learned session memory is omitted by final prompt packing.
pub const LEARNED_CONTEXT_PROMPT_BUDGET_OMITTED_COUNTER: &str =
    "session_memory.prompt_budget_omitted_total";

/// A sink for runtime traces and metrics.
pub trait RuntimeObserver: Send + Sync {
    /// Returns the currently active debug capture level.
    fn debug_level(&self) -> DebugCaptureLevel {
        DebugCaptureLevel::Off
    }

    /// Records a trace event.
    fn record(&self, event: TraceEvent);

    /// Records an external-action trace and fails if durable audit is unavailable.
    fn record_external_action(&self, event: TraceEvent) -> Result<()> {
        self.record(event);
        if let Some(error) = self.external_action_audit_failure() {
            bail!("external action audit unavailable: {error}");
        }
        Ok(())
    }

    /// Returns the first durable external-action audit failure when the sink is unhealthy.
    fn external_action_audit_failure(&self) -> Option<String> {
        None
    }

    /// Records one structured debug artifact.
    fn record_debug_artifact(&self, _artifact: DebugArtifact) {}

    /// Increments a counter by the provided delta.
    fn increment_counter(&self, name: &str, delta: u64);

    /// Returns a point-in-time metrics snapshot when the observer stores counters.
    fn metrics_snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot::default()
    }
}

/// An in-memory observer useful in tests and local runtimes.
#[derive(Default)]
pub struct InMemoryObserver {
    traces: Mutex<Vec<TraceEvent>>,
    artifacts: Mutex<Vec<DebugArtifact>>,
    counters: Mutex<BTreeMap<String, u64>>,
}

impl InMemoryObserver {
    /// Creates a shared in-memory observer.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns a copy of all recorded trace events.
    pub fn traces(&self) -> Vec<TraceEvent> {
        self.traces.lock().expect("traces mutex poisoned").clone()
    }

    /// Returns a copy of all recorded debug artifacts.
    pub fn debug_artifacts(&self) -> Vec<DebugArtifact> {
        self.artifacts
            .lock()
            .expect("artifacts mutex poisoned")
            .clone()
    }

    /// Returns a metrics snapshot.
    pub fn metrics(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            counters: self
                .counters
                .lock()
                .expect("counters mutex poisoned")
                .clone(),
        }
    }

    /// Computes a diff against another metrics snapshot.
    pub fn diff(&self, previous: &MetricsSnapshot) -> TraceDiff {
        let current = self.metrics();
        let mut counter_deltas = BTreeMap::new();
        for (name, current_value) in current.counters {
            let previous_value = previous.counters.get(&name).copied().unwrap_or_default();
            counter_deltas.insert(name, current_value as i64 - previous_value as i64);
        }
        TraceDiff { counter_deltas }
    }
}

impl RuntimeObserver for InMemoryObserver {
    fn record(&self, event: TraceEvent) {
        self.traces
            .lock()
            .expect("traces mutex poisoned")
            .push(event);
    }

    fn record_debug_artifact(&self, artifact: DebugArtifact) {
        self.artifacts
            .lock()
            .expect("artifacts mutex poisoned")
            .push(artifact);
    }

    fn increment_counter(&self, name: &str, delta: u64) {
        let mut counters = self.counters.lock().expect("counters mutex poisoned");
        *counters.entry(name.to_string()).or_default() += delta;
    }

    fn metrics_snapshot(&self) -> MetricsSnapshot {
        self.metrics()
    }
}

/// A no-op observer used when no telemetry sink is required.
#[derive(Default)]
pub struct NoopObserver;

impl RuntimeObserver for NoopObserver {
    fn record(&self, _event: TraceEvent) {}

    fn record_debug_artifact(&self, _artifact: DebugArtifact) {}

    fn increment_counter(&self, _name: &str, _delta: u64) {}
}

#[cfg(test)]
mod tests {
    use super::{failed_external_action_outcome, safe_url_debug_target};

    #[test]
    fn failed_external_action_outcome_classifies_web_fetch_blocks() {
        assert_eq!(
            failed_external_action_outcome(
                r#"{"error":"web_fetch refuses localhost/private network host 127.0.0.1"}"#
            ),
            "failed:blocked_private_network"
        );
        assert_eq!(
            failed_external_action_outcome(
                r#"{"error":"web_fetch refuses URLs containing credentials"}"#
            ),
            "failed:blocked_credentials"
        );
        assert_eq!(
            failed_external_action_outcome("native web search response exceeded 2097152 bytes"),
            "failed:response_too_large"
        );
        assert_eq!(
            failed_external_action_outcome(
                "native web search refuses non-JSON content type text/html"
            ),
            "failed:unsupported_content_type"
        );
    }

    #[test]
    fn safe_url_debug_target_removes_secret_bearing_url_parts() {
        let rendered = safe_url_debug_target(
            "https://user:password@example.test:8443/v1/responses?api_key=secret#fragment",
        );
        assert_eq!(rendered, "https://example.test:8443/v1/responses");
        assert!(!rendered.contains("password"));
        assert!(!rendered.contains("api_key"));
        assert!(!rendered.contains("secret"));
    }
}
