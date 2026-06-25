//! Derivation command handlers.

use anyhow::{Result, bail};

/// Handles `derivations ...`.
pub(crate) async fn run_derivations_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::DerivationsCommand,
) -> Result<()> {
    match command {
        crate::DerivationsCommand::List { query } => {
            let mut path = "/v1/derivations".to_string();
            if let Some(query) = query.filter(|value| !value.trim().is_empty()) {
                path = format!(
                    "/v1/derivations?query={}",
                    crate::cli::url_encode_component(&query)
                );
            }
            let derivations = client
                .get_json::<Vec<kheish_daemon::DerivationView>>(&path)
                .await?;
            printer.print(&derivations)
        }
        crate::DerivationsCommand::Get { derivation_id } => {
            let derivation = client
                .get_json::<kheish_daemon::DerivationView>(&format!(
                    "/v1/derivations/{derivation_id}"
                ))
                .await?;
            printer.print(&derivation)
        }
        crate::DerivationsCommand::Create(args) => {
            let transcription = args.transcription_options();
            let mut path = "/v1/derivations".to_string();
            let mut query = Vec::new();
            if args.force_refresh {
                query.push("force_refresh=true");
            }
            if args.retry_failed {
                query.push("retry_failed=true");
            }
            if !query.is_empty() {
                path.push('?');
                path.push_str(&query.join("&"));
            }
            let subject = match (
                args.asset_id.filter(|value| !value.trim().is_empty()),
                args.observation_id.filter(|value| !value.trim().is_empty()),
                args.session_id.filter(|value| !value.trim().is_empty()),
                args.offset,
            ) {
                (Some(asset_id), None, None, None) => {
                    kheish_daemon::DerivationSubject::Asset { asset_id }
                }
                (None, Some(observation_id), None, None) => {
                    kheish_daemon::DerivationSubject::Observation { observation_id }
                }
                (None, None, Some(session_id), Some(offset)) => {
                    kheish_daemon::DerivationSubject::SessionInput { session_id, offset }
                }
                (None, None, Some(_), None) => bail!("--offset is required with --session-id"),
                (None, None, None, Some(_)) => bail!("--session-id is required with --offset"),
                _ => bail!(
                    "provide either --asset-id, --observation-id, or the pair --session-id/--offset"
                ),
            };
            let derivation = client
                .post_json::<_, kheish_daemon::DerivationView>(
                    &path,
                    &kheish_daemon::CreateDerivationRequest {
                        profile: args.profile.into(),
                        subject,
                        transcription,
                    },
                )
                .await?;
            printer.print(&derivation)
        }
    }
}
