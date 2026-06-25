use std::collections::BTreeSet;
use std::future::Future;

use anyhow::anyhow;
use kheish_types::CredentialScope;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

const RUN_INTERRUPTED_MESSAGE: &str = "run interrupted";

tokio::task_local! {
    static EXECUTION_SCOPE: ExecutionScope;
}

tokio::task_local! {
    static EXECUTION_CANCELLATION: CancellationToken;
}

/// The contextual identifiers attached to one runtime execution scope.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionScope {
    /// The session being executed.
    pub session_id: String,
    /// The optional agent identifier associated with the execution.
    pub agent_id: Option<String>,
    /// The optional daemon run identifier associated with the execution.
    pub run_id: Option<String>,
    /// The stable principal identifier associated with the execution when known.
    pub principal_id: Option<String>,
    /// The parent principal identifier when the execution was delegated explicitly.
    pub parent_principal_id: Option<String>,
    /// The stable delegation identifier associated with this execution when known.
    pub delegation_id: Option<String>,
    /// The broker grant identifier associated with the active auth decision when known.
    pub grant_id: Option<String>,
    /// The current tool call identifier when execution is inside one tool invocation.
    pub tool_call_id: Option<String>,
    /// The effective provider pinned to this execution when known.
    pub provider: Option<String>,
    /// The effective primary model pinned to this execution when known.
    pub model: Option<String>,
    /// The effective credential scope enforced for auth-backed resources.
    #[serde(default, skip_serializing_if = "CredentialScope::is_empty")]
    pub credential_scope: CredentialScope,
    /// The optional workspace root visible to this execution.
    pub workspace_root: Option<String>,
    /// The skill names visible to this execution when scoped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visible_skills: Vec<String>,
    /// The MCP server names visible to this execution when scoped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visible_mcp_servers: Vec<String>,
    /// The qualified MCP tool names visible to this execution when scoped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visible_mcp_tools: Vec<String>,
}

/// Runs a future with execution context and a cancellation token attached.
pub async fn scope_execution<F, T>(
    scope: ExecutionScope,
    cancellation: CancellationToken,
    future: F,
) -> T
where
    F: Future<Output = T>,
{
    EXECUTION_SCOPE
        .scope(scope, EXECUTION_CANCELLATION.scope(cancellation, future))
        .await
}

/// Returns the current execution scope when running inside a scoped runtime task.
pub fn current_execution_scope() -> Option<ExecutionScope> {
    EXECUTION_SCOPE.try_with(Clone::clone).ok()
}

/// Returns the current cancellation token when running inside a scoped runtime task.
pub fn current_cancellation_token() -> Option<CancellationToken> {
    EXECUTION_CANCELLATION.try_with(Clone::clone).ok()
}

/// Builds the canonical interruption error used across runtime services.
pub fn interrupted_error() -> anyhow::Error {
    anyhow!(RUN_INTERRUPTED_MESSAGE)
}

/// Returns true when the provided error represents an explicit interruption.
pub fn is_interrupted_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|source| source.to_string() == RUN_INTERRUPTED_MESSAGE)
        || error.to_string() == RUN_INTERRUPTED_MESSAGE
}

/// Reads one optional string allowlist propagated through tool-context metadata.
///
/// The runtime encodes scoped visibility lists as JSON string arrays on the
/// execution scope. Tools can use this helper to enforce the same visibility
/// contract at execution time.
pub fn tool_context_string_allowlist(metadata: &Value, field: &str) -> Option<BTreeSet<String>> {
    metadata.get(field).and_then(Value::as_array).map(|values| {
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect()
    })
}
