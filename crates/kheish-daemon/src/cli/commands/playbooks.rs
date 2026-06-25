//! Playbook and Flow command handlers.

use anyhow::{Context, Result};
use serde_json::Value;

/// Handles `playbooks ...`.
pub(crate) async fn run_playbooks_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::PlaybooksCommand,
) -> Result<()> {
    match command {
        crate::PlaybooksCommand::List { query, status } => {
            let playbooks = client
                .get_json_with_query::<_, Vec<kheish_daemon::PlaybookView>>(
                    "/v1/playbooks",
                    &kheish_daemon::PlaybookListQuery {
                        query,
                        status: status.map(Into::into),
                    },
                )
                .await?;
            printer.print(&playbooks)
        }
        crate::PlaybooksCommand::Get { playbook_id } => {
            let playbook_id = crate::cli::url_encode_path_segment(&playbook_id);
            let playbook = client
                .get_json::<kheish_daemon::PlaybookView>(&format!("/v1/playbooks/{playbook_id}"))
                .await?;
            printer.print(&playbook)
        }
        crate::PlaybooksCommand::Validate(args) => {
            let manifest = read_required_manifest(args).await?;
            let result = client
                .post_json::<_, kheish_daemon::PlaybookValidationResult>(
                    "/v1/playbooks/validate",
                    &kheish_daemon::ValidatePlaybookRequest { manifest },
                )
                .await?;
            printer.print(&result)
        }
        crate::PlaybooksCommand::Create(args) => {
            let manifest = read_required_manifest(args).await?;
            let playbook = client
                .post_json::<_, kheish_daemon::PlaybookView>(
                    "/v1/playbooks",
                    &kheish_daemon::CreatePlaybookRequest { manifest },
                )
                .await?;
            printer.print(&playbook)
        }
        crate::PlaybooksCommand::Publish(args) => {
            let evidence_refs = read_evidence_refs(
                args.evidence_refs_json.as_deref(),
                args.evidence_refs_file.as_deref(),
            )
            .await?;
            let playbook_id = crate::cli::url_encode_path_segment(&args.playbook_id);
            let playbook = client
                .post_json::<_, kheish_daemon::PlaybookView>(
                    &format!("/v1/playbooks/{playbook_id}/publish"),
                    &kheish_daemon::PublishPlaybookRequest {
                        version: args.version,
                        digest: args.digest,
                        status: args.status.map(Into::into),
                        evidence_refs,
                    },
                )
                .await?;
            printer.print(&playbook)
        }
        crate::PlaybooksCommand::Revoke(args) => {
            let evidence_refs = read_evidence_refs(
                args.evidence_refs_json.as_deref(),
                args.evidence_refs_file.as_deref(),
            )
            .await?;
            let playbook_id = crate::cli::url_encode_path_segment(&args.playbook_id);
            let playbook = client
                .post_json::<_, kheish_daemon::PlaybookView>(
                    &format!("/v1/playbooks/{playbook_id}/revoke"),
                    &kheish_daemon::RevokePlaybookRequest {
                        version: args.version,
                        digest: args.digest,
                        reason: args.reason,
                        evidence_refs,
                    },
                )
                .await?;
            printer.print(&playbook)
        }
    }
}

/// Handles `flows ...`.
pub(crate) async fn run_flows_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::FlowsCommand,
) -> Result<()> {
    match command {
        crate::FlowsCommand::List {
            playbook_id,
            session_id,
            status,
        } => {
            let flows = client
                .get_json_with_query::<_, Vec<kheish_daemon::FlowView>>(
                    "/v1/flows",
                    &kheish_daemon::FlowListQuery {
                        playbook_id,
                        session_id,
                        status: status.map(Into::into),
                    },
                )
                .await?;
            printer.print(&flows)
        }
        crate::FlowsCommand::Start(args) => {
            let request = crate::cli::read_optional_typed_json_input::<
                kheish_daemon::SubmitInputRequest,
            >(args.request_json.as_deref(), args.request_file.as_deref())
            .await?
            .context("flow start requires --request-json or --request-file")?;
            let metadata = crate::cli::read_optional_json_input(
                args.metadata_json.as_deref(),
                args.metadata_file.as_deref(),
            )
            .await?
            .unwrap_or(Value::Null);
            let evidence_refs = read_evidence_refs(
                args.evidence_refs_json.as_deref(),
                args.evidence_refs_file.as_deref(),
            )
            .await?;
            let flow = client
                .post_json::<_, kheish_daemon::FlowView>(
                    "/v1/flows",
                    &kheish_daemon::StartFlowRequest {
                        flow_id: args.flow_id,
                        idempotency_key: args.idempotency_key,
                        playbook_ref: kheish_daemon::PlaybookVersionRef {
                            playbook_id: args.playbook_id,
                            version: args.version,
                            digest: args.digest,
                        },
                        session_id: args.session_id,
                        request,
                        metadata,
                        evidence_refs,
                    },
                )
                .await?;
            printer.print(&flow)
        }
        crate::FlowsCommand::Get { flow_id } => {
            let flow_id = crate::cli::url_encode_path_segment(&flow_id);
            let flow = client
                .get_json::<kheish_daemon::FlowView>(&format!("/v1/flows/{flow_id}"))
                .await?;
            printer.print(&flow)
        }
        crate::FlowsCommand::Cancel { flow_id } => {
            let flow_id = crate::cli::url_encode_path_segment(&flow_id);
            let flow = client
                .post_empty_json::<kheish_daemon::FlowView>(&format!("/v1/flows/{flow_id}/cancel"))
                .await?;
            printer.print(&flow)
        }
        crate::FlowsCommand::Evidence(args) => {
            let evidence_refs = read_evidence_refs(
                args.evidence_refs_json.as_deref(),
                args.evidence_refs_file.as_deref(),
            )
            .await?;
            let flow_id = crate::cli::url_encode_path_segment(&args.flow_id);
            let flow = client
                .post_json::<_, kheish_daemon::FlowView>(
                    &format!("/v1/flows/{flow_id}/evidence"),
                    &kheish_daemon::AppendFlowEvidenceRequest { evidence_refs },
                )
                .await?;
            printer.print(&flow)
        }
        crate::FlowsCommand::VerifyProductView(args) => {
            let flow_id = crate::cli::url_encode_path_segment(&args.flow_id);
            let verdict = client
                .post_json::<_, kheish_daemon::ProductViewFlowVerificationVerdict>(
                    &format!("/v1/flows/{flow_id}/verify/product-view"),
                    &kheish_daemon::ProductViewFlowVerificationRequest {
                        report_path: args.report_path,
                        required_sections: args.required_section,
                        forbidden_tools: args.forbidden_tool,
                    },
                )
                .await?;
            if let Some(output_file) = args.output_file {
                let bytes = serde_json::to_vec_pretty(&verdict)?;
                tokio::fs::write(output_file, bytes).await?;
            }
            printer.print(&verdict)
        }
        crate::FlowsCommand::Stream { flow_id } => {
            let flow_id = crate::cli::url_encode_path_segment(&flow_id);
            client
                .stream_events(&format!("/v1/flows/{flow_id}/stream"), printer)
                .await
        }
    }
}

async fn read_required_manifest(
    args: crate::PlaybookManifestInputArgs,
) -> Result<kheish_daemon::PlaybookManifest> {
    crate::cli::read_optional_typed_json_input::<kheish_daemon::PlaybookManifest>(
        args.manifest_json.as_deref(),
        args.manifest_file.as_deref(),
    )
    .await?
    .context("playbook command requires --manifest-json or --manifest-file")
}

async fn read_evidence_refs(
    inline: Option<&str>,
    file: Option<&std::path::Path>,
) -> Result<Vec<kheish_daemon::FlowEvidenceRef>> {
    Ok(
        crate::cli::read_optional_typed_json_input::<Vec<kheish_daemon::FlowEvidenceRef>>(
            inline, file,
        )
        .await?
        .unwrap_or_default(),
    )
}
