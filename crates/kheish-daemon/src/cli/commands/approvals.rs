//! Approval command handlers.

use anyhow::{Result, anyhow};

/// Handles `approvals ...`.
pub(crate) async fn run_approvals_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::ApprovalsCommand,
) -> Result<()> {
    match command {
        crate::ApprovalsCommand::List { session_id } => {
            let approvals =
                crate::cli::collect_pending_approvals(client, session_id.as_deref()).await?;
            printer.print(&approvals)
        }
        crate::ApprovalsCommand::Show {
            request_id,
            session_id,
        } => {
            let approvals =
                crate::cli::collect_pending_approvals(client, session_id.as_deref()).await?;
            let approval = approvals
                .into_iter()
                .find(|approval| approval.request.id == request_id)
                .ok_or_else(|| anyhow!("unknown approval request {request_id}"))?;
            printer.print(&approval)
        }
        crate::ApprovalsCommand::Allow(args) => {
            let wait = args.wait;
            let poll_interval_ms = args.poll_interval_ms;
            let mut resolution = args
                .into_resolution(kheish_types::ApprovalResolutionBehavior::Allow)
                .await?;
            resolution.run_id = crate::cli::find_pending_approval_run_id(
                client,
                &resolution.session_id,
                &resolution.resolution.request_id,
            )
            .await?;
            let run = crate::cli::resolve_approval(client, resolution).await?;
            if wait {
                let run = crate::cli::wait_for_run_after_approval_resolution(
                    client,
                    &run.run_id,
                    poll_interval_ms,
                )
                .await?;
                printer.print(&run)
            } else {
                printer.print(&run)
            }
        }
        crate::ApprovalsCommand::AllowAll(args) => {
            let idempotency_key = args.idempotency_key.clone();
            let mut result = crate::cli::resolve_all_approvals(
                client,
                crate::cli::collect_pending_approvals(client, args.session_id.as_deref()).await?,
                kheish_types::ApprovalResolutionBehavior::Allow,
                args.justification,
                None,
                idempotency_key,
            )
            .await?;
            if args.wait {
                result.runs = crate::cli::wait_for_all_runs_after_approval_resolution(
                    client,
                    result.runs,
                    args.poll_interval_ms,
                )
                .await?;
            }
            printer.print(&result)
        }
        crate::ApprovalsCommand::Deny(args) => {
            let wait = args.wait;
            let poll_interval_ms = args.poll_interval_ms;
            let mut resolution = args.into_resolution().await?;
            resolution.run_id = crate::cli::find_pending_approval_run_id(
                client,
                &resolution.session_id,
                &resolution.resolution.request_id,
            )
            .await?;
            let run = crate::cli::resolve_approval(client, resolution).await?;
            if wait {
                let run = crate::cli::wait_for_run_after_approval_resolution(
                    client,
                    &run.run_id,
                    poll_interval_ms,
                )
                .await?;
                printer.print(&run)
            } else {
                printer.print(&run)
            }
        }
        crate::ApprovalsCommand::DenyAll(args) => {
            let idempotency_key = args.idempotency_key.clone();
            let mut result = crate::cli::resolve_all_approvals(
                client,
                crate::cli::collect_pending_approvals(client, args.session_id.as_deref()).await?,
                kheish_types::ApprovalResolutionBehavior::Deny,
                args.justification,
                args.reason,
                idempotency_key,
            )
            .await?;
            if args.wait {
                result.runs = crate::cli::wait_for_all_runs_after_approval_resolution(
                    client,
                    result.runs,
                    args.poll_interval_ms,
                )
                .await?;
            }
            printer.print(&result)
        }
    }
}
