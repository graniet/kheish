//! Mailbox command handlers.

use anyhow::Result;
use serde_json::Value;

/// Handles `mailboxes ...`.
pub(crate) async fn run_mailboxes_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::MailboxesCommand,
) -> Result<()> {
    match command {
        crate::MailboxesCommand::Post(args) => {
            let payload = crate::cli::read_optional_json_input(
                args.payload_json.as_deref(),
                args.payload_file.as_deref(),
            )
            .await?
            .unwrap_or(Value::Null);
            let response = client
                .post_json::<_, kheish_daemon::PostMailboxResponse>(
                    "/v1/mailboxes",
                    &kheish_daemon::PostMailboxRequest {
                        message_id: args.message_id,
                        from_agent_id: args.from_agent_id,
                        to_agent_id: args.to_agent_id,
                        subject: args.subject,
                        ttl_ms: args.ttl_ms,
                        payload,
                    },
                )
                .await?;
            printer.print(&response)
        }
    }
}
