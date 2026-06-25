//! Shared wait and approval-resolution helpers for CLI commands.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

const DEFAULT_WAIT_TRANSIENT_ERROR_GRACE_MS: u64 = 15_000;

/// One approval request together with its owning session and run metadata.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PendingApprovalView {
    pub(crate) session_id: String,
    pub(crate) agent_id: String,
    pub(crate) run_id: Option<String>,
    pub(crate) request: kheish_types::ApprovalRequest,
}

#[derive(Debug, Clone)]
struct ApprovalResolutionBatch {
    session_id: String,
    run_id: Option<String>,
    resolutions: Vec<kheish_types::ApprovalResolution>,
}

/// Summary returned after bulk approval resolution.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct BulkApprovalResult {
    pub(crate) resolved_requests: usize,
    pub(crate) affected_sessions: usize,
    pub(crate) runs: Vec<kheish_daemon::RunView>,
}

/// Lists pending approvals, optionally filtered to one session.
pub(crate) async fn collect_pending_approvals(
    client: &crate::cli::DaemonHttpClient,
    session_id: Option<&str>,
) -> Result<Vec<PendingApprovalView>> {
    let session_ids = match session_id {
        Some(session_id) => vec![session_id.to_string()],
        None => client
            .get_json::<Vec<kheish_daemon::SessionViewSummary>>("/v1/sessions")
            .await?
            .into_iter()
            .map(|session| session.session_id)
            .collect(),
    };

    let mut approvals = Vec::new();
    for session_id in session_ids {
        let encoded_session_id = crate::cli::url_encode_path_segment(&session_id);
        let query_session_id = crate::cli::url_encode_component(&session_id);
        let active_run_id = client
            .get_json::<Vec<kheish_daemon::RunView>>(&format!(
                "/v1/runs?session_id={query_session_id}"
            ))
            .await?
            .into_iter()
            .find(|run| run.status == kheish_daemon::DaemonRunStatus::WaitingForApproval)
            .map(|run| run.run_id);
        if active_run_id.is_none() {
            continue;
        }
        let session = client
            .get_json::<kheish_daemon::SessionView>(&format!("/v1/sessions/{encoded_session_id}"))
            .await?;
        approvals.extend(
            session
                .snapshot
                .pending_approvals
                .into_iter()
                .map(|request| PendingApprovalView {
                    session_id: session.session_id.clone(),
                    agent_id: session.agent_id.clone(),
                    run_id: active_run_id.clone(),
                    request,
                }),
        );
    }
    Ok(approvals)
}

/// Resolves the run id attached to one pending approval request.
pub(crate) async fn find_pending_approval_run_id(
    client: &crate::cli::DaemonHttpClient,
    session_id: &str,
    request_id: &str,
) -> Result<Option<String>> {
    Ok(collect_pending_approvals(client, Some(session_id))
        .await?
        .into_iter()
        .find(|approval| approval.request.id == request_id)
        .and_then(|approval| approval.run_id))
}

/// Lists pending structured questions, optionally filtered to one session.
pub(crate) async fn collect_pending_questions(
    client: &crate::cli::DaemonHttpClient,
    session_id: Option<&str>,
) -> Result<Vec<kheish_daemon::PendingQuestionView>> {
    let path = match session_id {
        Some(session_id) => format!(
            "/v1/sessions/{}/questions",
            crate::cli::url_encode_path_segment(session_id)
        ),
        None => "/v1/questions".to_string(),
    };
    client
        .get_json::<Vec<kheish_daemon::PendingQuestionView>>(&path)
        .await
}

/// Resolves the run id attached to one pending structured question.
pub(crate) async fn find_pending_question_run_id(
    client: &crate::cli::DaemonHttpClient,
    session_id: Option<&str>,
    request_id: &str,
) -> Result<Option<String>> {
    let matches = collect_pending_questions(client, session_id)
        .await?
        .into_iter()
        .filter(|question| question.request.id == request_id)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [question] => Ok(question.run_id.clone()),
        _ => anyhow::bail!(
            "pending question request {request_id} is ambiguous; pass --session-id or --run-id"
        ),
    }
}

/// Loads question answers from either inline JSON or a JSON file.
pub(crate) async fn load_question_answers(
    args: &crate::QuestionAnswerArgs,
) -> Result<Vec<kheish_types::UserQuestionAnswer>> {
    if args.declined {
        match (&args.answers_json, &args.answers_file) {
            (None, None) => return Ok(Vec::new()),
            _ => bail!("do not pass --answers-json or --answers-file together with --declined"),
        }
    }
    let raw = match (&args.answers_json, &args.answers_file) {
        (Some(raw), None) => raw.clone(),
        (None, Some(path)) => std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?,
        (Some(_), Some(_)) => bail!("use either --answers-json or --answers-file"),
        (None, None) => bail!("one of --answers-json or --answers-file is required"),
    };
    serde_json::from_str(&raw).context("failed to decode user-question answers JSON")
}

/// Resolves one approval request through the daemon API.
pub(crate) async fn resolve_approval(
    client: &crate::cli::DaemonHttpClient,
    resolution: crate::ApprovalResolutionRequest,
) -> Result<kheish_daemon::RunView> {
    let path = resolution
        .run_id
        .map(|run_id| {
            format!(
                "/v1/runs/{}/approvals",
                crate::cli::url_encode_path_segment(&run_id)
            )
        })
        .unwrap_or_else(|| {
            format!(
                "/v1/sessions/{}/approval-runs",
                crate::cli::url_encode_path_segment(&resolution.session_id)
            )
        });
    let request = kheish_daemon::ResolveApprovalsRequest {
        idempotency_key: None,
        resolutions: vec![resolution.resolution],
    };
    match resolution.idempotency_key.as_deref() {
        Some(key) => {
            client
                .post_json_with_idempotency_key::<_, kheish_daemon::RunView>(&path, key, &request)
                .await
        }
        None => {
            client
                .post_json::<_, kheish_daemon::RunView>(&path, &request)
                .await
        }
    }
}

/// Resolves all pending approvals in grouped batches.
pub(crate) async fn resolve_all_approvals(
    client: &crate::cli::DaemonHttpClient,
    approvals: Vec<PendingApprovalView>,
    behavior: kheish_types::ApprovalResolutionBehavior,
    justification: Option<String>,
    reason: Option<String>,
    idempotency_key: Option<String>,
) -> Result<BulkApprovalResult> {
    let resolved_requests = approvals.len();
    let batches = build_bulk_approval_batches(approvals, behavior, justification, reason);
    let affected_sessions = batches.len();
    let mut runs = Vec::with_capacity(affected_sessions);

    for batch in batches {
        let path = batch
            .run_id
            .as_ref()
            .map(|run_id| {
                format!(
                    "/v1/runs/{}/approvals",
                    crate::cli::url_encode_path_segment(run_id)
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "/v1/sessions/{}/approval-runs",
                    crate::cli::url_encode_path_segment(&batch.session_id)
                )
            });
        let request = kheish_daemon::ResolveApprovalsRequest {
            idempotency_key: None,
            resolutions: batch.resolutions,
        };
        let run = match idempotency_key.as_deref() {
            Some(key) => {
                client
                    .post_json_with_idempotency_key::<_, kheish_daemon::RunView>(
                        &path, key, &request,
                    )
                    .await?
            }
            None => {
                client
                    .post_json::<_, kheish_daemon::RunView>(&path, &request)
                    .await?
            }
        };
        runs.push(run);
    }

    Ok(BulkApprovalResult {
        resolved_requests,
        affected_sessions,
        runs,
    })
}

/// Polls one run until it reaches a terminal daemon status.
pub(crate) async fn wait_for_run(
    client: &crate::cli::DaemonHttpClient,
    run_id: &str,
    poll_interval_ms: u64,
) -> Result<kheish_daemon::RunView> {
    let interval = Duration::from_millis(poll_interval_ms.max(50));
    let mut transient_error_deadline = None;
    loop {
        let run_id = crate::cli::url_encode_path_segment(run_id);
        let run = match client
            .get_json::<kheish_daemon::RunView>(&format!("/v1/runs/{run_id}"))
            .await
        {
            Ok(run) => {
                transient_error_deadline = None;
                run
            }
            Err(error)
                if should_retry_transient_wait_error(&error, &mut transient_error_deadline) =>
            {
                tokio::time::sleep(interval).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        if matches!(
            run.status,
            kheish_daemon::DaemonRunStatus::Completed
                | kheish_daemon::DaemonRunStatus::Failed
                | kheish_daemon::DaemonRunStatus::Interrupted
                | kheish_daemon::DaemonRunStatus::Cancelled
        ) {
            return Ok(run);
        }
        tokio::time::sleep(interval).await;
    }
}

/// Polls one run until approval resolution reaches a terminal or newly-blocked state.
pub(crate) async fn wait_for_run_after_approval_resolution(
    client: &crate::cli::DaemonHttpClient,
    run_id: &str,
    poll_interval_ms: u64,
) -> Result<kheish_daemon::RunView> {
    let interval = Duration::from_millis(poll_interval_ms.max(50));
    let mut transient_error_deadline = None;
    loop {
        let run_id = crate::cli::url_encode_path_segment(run_id);
        let run = match client
            .get_json::<kheish_daemon::RunView>(&format!("/v1/runs/{run_id}"))
            .await
        {
            Ok(run) => {
                transient_error_deadline = None;
                run
            }
            Err(error)
                if should_retry_transient_wait_error(&error, &mut transient_error_deadline) =>
            {
                tokio::time::sleep(interval).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        if matches!(
            run.status,
            kheish_daemon::DaemonRunStatus::Completed
                | kheish_daemon::DaemonRunStatus::Failed
                | kheish_daemon::DaemonRunStatus::Interrupted
                | kheish_daemon::DaemonRunStatus::Cancelled
                | kheish_daemon::DaemonRunStatus::WaitingForApproval
                | kheish_daemon::DaemonRunStatus::WaitingForUserQuestion
        ) {
            return Ok(run);
        }
        tokio::time::sleep(interval).await;
    }
}

/// Polls approval-resumed runs until each is terminal or newly blocked.
pub(crate) async fn wait_for_all_runs_after_approval_resolution(
    client: &crate::cli::DaemonHttpClient,
    runs: Vec<kheish_daemon::RunView>,
    poll_interval_ms: u64,
) -> Result<Vec<kheish_daemon::RunView>> {
    let mut resolved = Vec::with_capacity(runs.len());
    for run in runs {
        resolved.push(
            wait_for_run_after_approval_resolution(client, &run.run_id, poll_interval_ms).await?,
        );
    }
    Ok(resolved)
}

fn should_retry_transient_wait_error(
    error: &anyhow::Error,
    deadline: &mut Option<Instant>,
) -> bool {
    if !is_transient_wait_error(error) {
        return false;
    }
    let now = Instant::now();
    let deadline = deadline.get_or_insert_with(|| now + transient_wait_error_grace());
    now < *deadline
}

fn transient_wait_error_grace() -> Duration {
    std::env::var("KHEISH_DAEMON_WAIT_TRANSIENT_ERROR_GRACE_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis(DEFAULT_WAIT_TRANSIENT_ERROR_GRACE_MS))
}

fn is_transient_wait_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|error| error.is_connect() || error.is_timeout() || error.is_request())
    })
}

fn build_bulk_approval_batches(
    approvals: Vec<PendingApprovalView>,
    behavior: kheish_types::ApprovalResolutionBehavior,
    justification: Option<String>,
    reason: Option<String>,
) -> Vec<ApprovalResolutionBatch> {
    let mut grouped = std::collections::BTreeMap::<
        (String, Option<String>),
        Vec<kheish_types::ApprovalResolution>,
    >::new();
    for approval in approvals {
        grouped
            .entry((approval.session_id, approval.run_id))
            .or_default()
            .push(kheish_types::ApprovalResolution {
                request_id: approval.request.id,
                behavior: behavior.clone(),
                updated_input: None,
                justification: justification.clone(),
                reason: reason.clone(),
            });
    }

    grouped
        .into_iter()
        .map(
            |((session_id, run_id), resolutions)| ApprovalResolutionBatch {
                session_id,
                run_id,
                resolutions,
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_bulk_approval_batches_groups_by_session_and_run() {
        let batches = build_bulk_approval_batches(
            vec![
                PendingApprovalView {
                    session_id: "session-a".to_string(),
                    agent_id: "agent-a".to_string(),
                    run_id: Some("run-a".to_string()),
                    request: kheish_types::ApprovalRequest {
                        id: "req-1".to_string(),
                        tool_call_id: "call-1".to_string(),
                        tool_name: "bash".to_string(),
                        input: serde_json::Value::Null,
                        scope: "default".to_string(),
                        reason: "review".to_string(),
                    },
                },
                PendingApprovalView {
                    session_id: "session-a".to_string(),
                    agent_id: "agent-a".to_string(),
                    run_id: Some("run-a".to_string()),
                    request: kheish_types::ApprovalRequest {
                        id: "req-2".to_string(),
                        tool_call_id: "call-2".to_string(),
                        tool_name: "bash".to_string(),
                        input: serde_json::Value::Null,
                        scope: "default".to_string(),
                        reason: "review".to_string(),
                    },
                },
            ],
            kheish_types::ApprovalResolutionBehavior::Allow,
            Some("ok".to_string()),
            None,
        );
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].resolutions.len(), 2);
    }

    #[tokio::test]
    async fn load_question_answers_rejects_decline_with_payload() {
        let args = crate::QuestionAnswerArgs {
            request_id: "req-1".to_string(),
            session_id: None,
            run_id: None,
            answers_json: Some("[]".to_string()),
            answers_file: None,
            interactive: false,
            declined: true,
            justification: None,
            idempotency_key: None,
            wait: false,
            poll_interval_ms: 200,
        };
        let error = load_question_answers(&args)
            .await
            .expect_err("expected error");
        assert!(
            error
                .to_string()
                .contains("do not pass --answers-json or --answers-file together with --declined")
        );
    }
}
