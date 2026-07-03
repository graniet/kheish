//! Session command handlers.

use anyhow::{Result, bail};
use serde_json::Value;

fn parse_session_goal_status(value: &str) -> Result<kheish_types::SessionGoalStatus> {
    match value {
        "active" => Ok(kheish_types::SessionGoalStatus::Active),
        "paused" => Ok(kheish_types::SessionGoalStatus::Paused),
        "budget_limited" => Ok(kheish_types::SessionGoalStatus::BudgetLimited),
        "complete" => Ok(kheish_types::SessionGoalStatus::Complete),
        _ => bail!("status must be one of active, paused, budget_limited, complete"),
    }
}

/// Handles `sessions ...`.
pub(crate) async fn run_sessions_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::SessionsCommand,
) -> Result<()> {
    // Vacuum is an offline maintenance command: it operates on the state root
    // directly under the daemon lock and never talks to the control plane.
    if let crate::SessionsCommand::Vacuum {
        session_id,
        state_root,
    } = &command
    {
        let _lock = crate::cli::state_lock::StateRootLock::acquire(state_root)?;
        let report = tokio::task::block_in_place(|| {
            kheish_session::vacuum_session(&state_root.join("sessions"), session_id)
        })?;
        return printer.print(&serde_json::json!({
            "session_id": session_id,
            "path": report.path,
            "backup_path": report.backup_path,
            "bytes_before": report.bytes_before,
            "bytes_after": report.bytes_after,
            "kept_by_type": report.kept_by_type,
            "metadata_keys_kept": report.metadata_keys_kept,
            "metadata_records_dropped": report.metadata_records_dropped,
            "torn_tail_dropped": report.torn_tail_dropped,
        }));
    }
    match command {
        crate::SessionsCommand::List { pagination } => {
            let mut path = "/v1/sessions".to_string();
            let mut params = Vec::new();
            pagination.append_query_params(&mut params);
            if !params.is_empty() {
                path.push('?');
                path.push_str(&params.join("&"));
            }
            if pagination.wants_page() {
                let sessions = client
                    .get_list_page_compat::<kheish_daemon::SessionViewSummary>(&path)
                    .await?;
                printer.print(&sessions)
            } else {
                let sessions = client
                    .get_json::<Vec<kheish_daemon::SessionViewSummary>>(&path)
                    .await?;
                printer.print(&sessions)
            }
        }
        crate::SessionsCommand::Create {
            session_id,
            thread_id,
            persona_id,
            capability_scope_json,
            capability_scope_file,
            credential_scope_json,
            credential_scope_file,
        } => {
            let capability_scope =
                crate::cli::read_optional_typed_json_input::<kheish_types::CapabilityScope>(
                    capability_scope_json.as_deref(),
                    capability_scope_file.as_deref(),
                )
                .await?;
            let credential_scope =
                crate::cli::read_optional_typed_json_input::<kheish_types::CredentialScope>(
                    credential_scope_json.as_deref(),
                    credential_scope_file.as_deref(),
                )
                .await?;
            let session = client
                .post_json::<_, kheish_daemon::SessionView>(
                    "/v1/sessions",
                    &kheish_daemon::CreateSessionRequest {
                        session_id,
                        thread_id,
                        persona_id,
                        capability_scope,
                        credential_scope,
                    },
                )
                .await?;
            printer.print(&session)
        }
        crate::SessionsCommand::Get { session_id } => {
            let session_id = crate::cli::url_encode_path_segment(&session_id);
            let session = client
                .get_json::<kheish_daemon::SessionView>(&format!("/v1/sessions/{session_id}"))
                .await?;
            printer.print(&session)
        }
        crate::SessionsCommand::MemoryContext { session_id, query } => {
            let session_id = crate::cli::url_encode_path_segment(&session_id);
            let path = if let Some(query) = query {
                let encoded = urlencoding::encode(&query);
                format!("/v1/sessions/{session_id}/memory-context?query={encoded}")
            } else {
                format!("/v1/sessions/{session_id}/memory-context")
            };
            let context = client
                .get_json::<kheish_daemon::SessionMemoryContextView>(&path)
                .await?;
            printer.print(&context)
        }
        crate::SessionsCommand::MemorySearch {
            session_id,
            query,
            limit,
        } => {
            let session_id = crate::cli::url_encode_path_segment(&session_id);
            let mut path = format!("/v1/sessions/{session_id}/memory-search");
            let mut params = Vec::new();
            if let Some(query) = query {
                params.push(format!("query={}", urlencoding::encode(&query)));
            }
            if let Some(limit) = limit {
                params.push(format!("limit={limit}"));
            }
            if !params.is_empty() {
                path.push('?');
                path.push_str(&params.join("&"));
            }
            let search = client
                .get_json::<kheish_daemon::SessionMemorySearchView>(&path)
                .await?;
            printer.print(&search)
        }
        crate::SessionsCommand::Skills { session_id, query } => {
            let session_id = crate::cli::url_encode_path_segment(&session_id);
            let path = if let Some(query) = query {
                let encoded = urlencoding::encode(&query);
                format!("/v1/sessions/{session_id}/skills?query={encoded}")
            } else {
                format!("/v1/sessions/{session_id}/skills")
            };
            let skills = client
                .get_json::<Vec<kheish_daemon::SkillSummaryView>>(&path)
                .await?;
            printer.print(&skills)
        }
        crate::SessionsCommand::Events { session_id } => {
            let session_id = crate::cli::url_encode_path_segment(&session_id);
            let events = client
                .get_json::<kheish_daemon::SessionEventLogView>(&format!(
                    "/v1/sessions/{session_id}/events"
                ))
                .await?;
            printer.print(&events)
        }
        crate::SessionsCommand::Stream { session_id, cursor } => {
            let session_id = crate::cli::url_encode_path_segment(&session_id);
            let suffix = cursor
                .map(|cursor| format!("?cursor={cursor}"))
                .unwrap_or_default();
            client
                .stream_events(
                    &format!("/v1/sessions/{session_id}/stream{suffix}"),
                    printer,
                )
                .await
        }
        crate::SessionsCommand::Input(args) => {
            let completion_requirements = crate::cli::build_completion_requirements(&args);
            let attachments =
                crate::cli::build_session_input_attachments(&args.files, &args.asset_ids).await?;
            let content = if args.content.is_some() || args.content_file.is_some() || args.stdin {
                crate::cli::read_text_input(args.content, args.content_file.as_deref(), args.stdin)
                    .await?
            } else if attachments.is_empty() {
                bail!("provide inline content, --content-file, --stdin, --file, or --asset");
            } else {
                String::new()
            };
            let metadata = crate::cli::read_optional_json_input(
                args.metadata_json.as_deref(),
                args.metadata_file.as_deref(),
            )
            .await?
            .unwrap_or(Value::Null);
            let route_ids = crate::cli::fetch_known_route_ids(client).await?;
            let (provider, generation) = crate::cli::normalize_provider_and_generation(
                args.provider.clone(),
                args.generation.build().await?,
                &route_ids,
            )?;
            let request = kheish_daemon::SubmitInputRequest {
                provider,
                source_plugin: args.source_plugin,
                source_kind: args.source_kind,
                actor_id: args.actor_id,
                content,
                input_items: Vec::new(),
                attachments,
                generation,
                completion_requirements,
                metadata: Some(metadata),
                binding_keys: Vec::new(),
                reply_targets: Vec::new(),
                reply_plugin: args.reply_plugin,
                reply_address: args.reply_address,
            };
            let session_id = crate::cli::url_encode_path_segment(&args.session_id);
            let path = format!("/v1/sessions/{session_id}/runs");
            let run = if let Some(idempotency_key) = args.idempotency_key.as_deref() {
                let capabilities = client
                    .get_json::<kheish_daemon::DaemonCapabilities>("/v1/capabilities")
                    .await?;
                if !capabilities.session_run_idempotency {
                    bail!("daemon does not support session run idempotency");
                }
                client
                    .post_json_with_idempotency_key::<_, kheish_daemon::RunView>(
                        &path,
                        idempotency_key,
                        &request,
                    )
                    .await?
            } else {
                client
                    .post_json::<_, kheish_daemon::RunView>(&path, &request)
                    .await?
            };
            if args.wait {
                let run =
                    crate::cli::wait_for_run(client, &run.run_id, args.poll_interval_ms).await?;
                printer.print(&run)
            } else {
                printer.print(&run)
            }
        }
        crate::SessionsCommand::Goal { command } => match command {
            crate::SessionGoalCommand::Get { session_id } => {
                let session_id = crate::cli::url_encode_path_segment(&session_id);
                let goal = client
                    .get_json::<kheish_daemon::SessionGoalResponse>(&format!(
                        "/v1/sessions/{session_id}/goal"
                    ))
                    .await?;
                printer.print(&goal)
            }
            crate::SessionGoalCommand::Set {
                session_id,
                objective,
                token_budget,
                status,
            } => {
                let status = status
                    .as_deref()
                    .map(parse_session_goal_status)
                    .transpose()?;
                let session_id = crate::cli::url_encode_path_segment(&session_id);
                let goal = client
                    .put_json::<_, kheish_daemon::SessionGoalResponse>(
                        &format!("/v1/sessions/{session_id}/goal"),
                        &kheish_daemon::SetSessionGoalRequest {
                            objective,
                            token_budget,
                            status,
                        },
                    )
                    .await?;
                printer.print(&goal)
            }
            crate::SessionGoalCommand::Clear { session_id } => {
                let session_id = crate::cli::url_encode_path_segment(&session_id);
                let goal = client
                    .delete_json::<kheish_daemon::SessionGoalResponse>(&format!(
                        "/v1/sessions/{session_id}/goal"
                    ))
                    .await?;
                printer.print(&goal)
            }
        },
        crate::SessionsCommand::SetRoute(args) => {
            let route_ids = crate::cli::fetch_known_route_ids(client).await?;
            let route_policy = crate::cli::build_session_route_policy(&args, &route_ids).await?;
            let session = client
                .post_json::<_, kheish_daemon::SessionView>(
                    &format!(
                        "/v1/sessions/{}/route-policy",
                        crate::cli::url_encode_path_segment(&args.session_id)
                    ),
                    &kheish_daemon::SetSessionRoutePolicyRequest { route_policy },
                )
                .await?;
            printer.print(&session)
        }
        crate::SessionsCommand::SetCapabilityScope(args) => {
            let capability_scope = if args.clear {
                if args.capability_scope_json.is_some() || args.capability_scope_file.is_some() {
                    bail!("--clear cannot be combined with capability scope input");
                }
                None
            } else {
                let capability_scope =
                    crate::cli::read_optional_typed_json_input::<kheish_types::CapabilityScope>(
                        args.capability_scope_json.as_deref(),
                        args.capability_scope_file.as_deref(),
                    )
                    .await?;
                if capability_scope.is_none() {
                    bail!("provide --capability-scope-json, --capability-scope-file, or --clear");
                }
                capability_scope
            };
            let session = client
                .post_json::<_, kheish_daemon::SessionView>(
                    &format!(
                        "/v1/sessions/{}/capability-scope",
                        crate::cli::url_encode_path_segment(&args.session_id)
                    ),
                    &kheish_daemon::SetSessionCapabilityScopeRequest { capability_scope },
                )
                .await?;
            printer.print(&session)
        }
        crate::SessionsCommand::SetCredentialScope(args) => {
            let credential_scope = if args.clear {
                if args.credential_scope_json.is_some() || args.credential_scope_file.is_some() {
                    bail!("--clear cannot be combined with credential scope input");
                }
                None
            } else {
                let credential_scope =
                    crate::cli::read_optional_typed_json_input::<kheish_types::CredentialScope>(
                        args.credential_scope_json.as_deref(),
                        args.credential_scope_file.as_deref(),
                    )
                    .await?;
                if credential_scope.is_none() {
                    bail!("provide --credential-scope-json, --credential-scope-file, or --clear");
                }
                credential_scope
            };
            let session = client
                .post_json::<_, kheish_daemon::SessionView>(
                    &format!(
                        "/v1/sessions/{}/credential-scope",
                        crate::cli::url_encode_path_segment(&args.session_id)
                    ),
                    &kheish_daemon::SetSessionCredentialScopeRequest { credential_scope },
                )
                .await?;
            printer.print(&session)
        }
        crate::SessionsCommand::SetReplyTargets(args) => {
            let request = crate::cli::read_required_typed_json_input::<
                kheish_daemon::SetSessionReplyTargetsRequest,
            >(
                args.reply_targets_json.as_deref(),
                args.reply_targets_file.as_deref(),
                "reply-target JSON",
            )
            .await?;
            let session = client
                .post_json::<_, kheish_daemon::SessionView>(
                    &format!(
                        "/v1/sessions/{}/reply-targets",
                        crate::cli::url_encode_path_segment(&args.session_id)
                    ),
                    &request,
                )
                .await?;
            printer.print(&session)
        }
        crate::SessionsCommand::ClearReplyTargets { session_id } => {
            let session_id = crate::cli::url_encode_path_segment(&session_id);
            let session = client
                .delete_json::<kheish_daemon::SessionView>(&format!(
                    "/v1/sessions/{session_id}/reply-targets"
                ))
                .await?;
            printer.print(&session)
        }
        crate::SessionsCommand::SetPersona {
            session_id,
            persona_id,
        } => {
            let session_id = crate::cli::url_encode_path_segment(&session_id);
            let session = client
                .post_json::<_, kheish_daemon::SessionView>(
                    &format!("/v1/sessions/{session_id}/persona"),
                    &kheish_daemon::SetSessionPersonaRequest { persona_id },
                )
                .await?;
            printer.print(&session)
        }
        crate::SessionsCommand::ClearPersona { session_id } => {
            let session_id = crate::cli::url_encode_path_segment(&session_id);
            let session = client
                .delete_json::<kheish_daemon::SessionView>(&format!(
                    "/v1/sessions/{session_id}/persona"
                ))
                .await?;
            printer.print(&session)
        }
        crate::SessionsCommand::Approve(args) => {
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
        crate::SessionsCommand::Deny(args) => {
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
        crate::SessionsCommand::Interrupt { session_id } => {
            let session_id = crate::cli::url_encode_path_segment(&session_id);
            let response = client
                .post_empty_json::<kheish_daemon::InterruptSessionResponse>(&format!(
                    "/v1/sessions/{session_id}/interrupt"
                ))
                .await?;
            printer.print(&response)
        }
        crate::SessionsCommand::End { session_id, reason } => {
            let session_id = crate::cli::url_encode_path_segment(&session_id);
            let session = client
                .post_json::<_, kheish_daemon::SessionView>(
                    &format!("/v1/sessions/{session_id}/end"),
                    &kheish_daemon::EndSessionRequest { reason },
                )
                .await?;
            printer.print(&session)
        }
        crate::SessionsCommand::Vacuum { .. } => {
            unreachable!("vacuum is handled before control-plane routing")
        }
    }
}
