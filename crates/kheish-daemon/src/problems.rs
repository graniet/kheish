use std::fmt;

/// Stable control-plane problem metadata carried across service boundaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DaemonProblem {
    pub(crate) status: u16,
    pub(crate) domain: &'static str,
    pub(crate) code: &'static str,
    detail: String,
}

impl DaemonProblem {
    pub(crate) fn bad_request(
        domain: &'static str,
        code: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(400, domain, code, detail)
    }

    pub(crate) fn conflict(
        domain: &'static str,
        code: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(409, domain, code, detail)
    }

    pub(crate) fn not_found(
        domain: &'static str,
        code: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(404, domain, code, detail)
    }

    pub(crate) fn session_not_found(detail: impl Into<String>) -> Self {
        Self::not_found("sessions", "session_not_found", detail)
    }

    pub(crate) fn run_not_found(detail: impl Into<String>) -> Self {
        Self::not_found("runs", "run_not_found", detail)
    }

    pub(crate) fn run_debug_not_found(detail: impl Into<String>) -> Self {
        Self::not_found("runs", "run_debug_not_found", detail)
    }

    pub(crate) fn run_debug_artifact_not_found(detail: impl Into<String>) -> Self {
        Self::not_found("runs", "run_debug_artifact_not_found", detail)
    }

    pub(crate) fn run_debug_artifact_unreadable(detail: impl Into<String>) -> Self {
        Self::conflict("runs", "run_debug_artifact_unreadable", detail)
    }

    pub(crate) fn invalid_idempotency_key(detail: impl Into<String>) -> Self {
        Self::bad_request("idempotency", "invalid_idempotency_key", detail)
    }

    pub(crate) fn idempotency_conflict(detail: impl Into<String>) -> Self {
        Self::conflict("idempotency", "idempotency_conflict", detail)
    }

    pub(crate) fn run_state_conflict(detail: impl Into<String>) -> Self {
        Self::conflict("runs", "run_state_conflict", detail)
    }

    pub(crate) fn session_busy(detail: impl Into<String>) -> Self {
        Self::conflict("sessions", "session_busy", detail)
    }

    pub(crate) fn run_retention_invalid_request(detail: impl Into<String>) -> Self {
        Self::bad_request("runs", "run_retention_invalid_request", detail)
    }

    pub(crate) fn approval_state_conflict(detail: impl Into<String>) -> Self {
        Self::conflict("approvals", "approval_state_conflict", detail)
    }

    pub(crate) fn approval_batch_empty(detail: impl Into<String>) -> Self {
        Self::bad_request("approvals", "approval_batch_empty", detail)
    }

    pub(crate) fn approval_duplicate_resolution(detail: impl Into<String>) -> Self {
        Self::bad_request("approvals", "approval_duplicate_resolution", detail)
    }

    pub(crate) fn approval_request_not_pending(detail: impl Into<String>) -> Self {
        Self::bad_request("approvals", "approval_request_not_pending", detail)
    }

    pub(crate) fn question_state_conflict(detail: impl Into<String>) -> Self {
        Self::conflict("questions", "question_state_conflict", detail)
    }

    pub(crate) fn question_expired(detail: impl Into<String>) -> Self {
        Self::conflict("questions", "question_expired", detail)
    }

    pub(crate) fn question_request_mismatch(detail: impl Into<String>) -> Self {
        Self::bad_request("questions", "question_request_mismatch", detail)
    }

    pub(crate) fn question_resolution_conflict(detail: impl Into<String>) -> Self {
        Self::conflict("questions", "question_resolution_conflict", detail)
    }

    pub(crate) fn runtime_revision_conflict(detail: impl Into<String>) -> Self {
        Self::conflict("runtime", "runtime_revision_conflict", detail)
    }

    pub(crate) fn runtime_change_blocked(detail: impl Into<String>) -> Self {
        Self::conflict("runtime", "runtime_change_blocked", detail)
    }

    pub(crate) fn runtime_revision_not_found(detail: impl Into<String>) -> Self {
        Self::not_found("runtime", "runtime_revision_not_found", detail)
    }

    pub(crate) fn invalid_tool_runtime_limits(detail: impl Into<String>) -> Self {
        Self::bad_request("runtime", "invalid_tool_runtime_limits", detail)
    }

    pub(crate) fn invalid_learning_policy(detail: impl Into<String>) -> Self {
        Self::bad_request("runtime", "invalid_learning_policy", detail)
    }

    pub(crate) fn invalid_run_memory_policy(detail: impl Into<String>) -> Self {
        Self::bad_request("runtime", "invalid_run_memory_policy", detail)
    }

    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }

    fn new(
        status: u16,
        domain: &'static str,
        code: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            status,
            domain,
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for DaemonProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for DaemonProblem {}
