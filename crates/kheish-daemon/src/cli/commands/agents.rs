//! Agent and sidechain command handlers.

use anyhow::Result;

async fn build_spawn_sidechain_request(
    client: &crate::cli::DaemonHttpClient,
    args: crate::SpawnSidechainArgs,
) -> Result<kheish_daemon::SpawnSidechainRequest> {
    let subtask = crate::cli::build_subtask_request(&args).await?;
    let capability_scope =
        crate::cli::read_optional_typed_json_input::<kheish_types::CapabilityScope>(
            args.capability_scope_json.as_deref(),
            args.capability_scope_file.as_deref(),
        )
        .await?;
    let credential_scope =
        crate::cli::read_optional_typed_json_input::<kheish_types::CredentialScope>(
            args.credential_scope_json.as_deref(),
            args.credential_scope_file.as_deref(),
        )
        .await?;
    let route_ids = crate::cli::fetch_known_route_ids(client).await?;
    let (provider, generation) = crate::cli::normalize_provider_and_generation(
        args.provider.clone(),
        args.generation.build().await?,
        &route_ids,
    )?;
    Ok(kheish_daemon::SpawnSidechainRequest {
        session_id: args.session_id,
        thread_id: args.thread_id,
        route_policy: None,
        provider: provider.clone(),
        permission_mode: None,
        retention: args
            .retention
            .map(crate::ChildRetentionPolicyArg::into_retention),
        nickname: args.nickname,
        spawned_by_run_id: None,
        spawn_request_id: args.spawn_request_id,
        fork_context: kheish_agent::ForkContext {
            parent_assistant_message: args.parent_assistant_message,
            inherited_tool_call_ids: args.inherited_tool_call_ids,
            team_name: None,
            isolation: None,
            system_prompt: args.system_prompt,
            prompt_merge_mode: if args.append_system_prompt {
                kheish_runtime::PromptMergeMode::Append
            } else {
                kheish_runtime::PromptMergeMode::Replace
            },
            provider,
            generation,
            tool_surface: kheish_types::ToolSurfaceFilter {
                allowlist: args.allowed_tools,
                denylist: args.blocked_tools,
            },
            worktree_path: args.worktree_path,
        },
        generation: None,
        tool_surface: None,
        capability_scope,
        credential_scope,
        subtask,
    })
}

/// Handles `agents ...`.
pub(crate) async fn run_agents_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::AgentsCommand,
) -> Result<()> {
    match command {
        crate::AgentsCommand::List => {
            let agents = client
                .get_json::<Vec<kheish_agent::ManagedAgentSnapshot>>("/v1/agents")
                .await?;
            printer.print(&agents)
        }
        crate::AgentsCommand::Summaries {
            root_agent_id,
            session_id,
            status,
            has_runtime,
            pagination,
        } => {
            let mut path = "/v1/agents/summaries".to_string();
            let mut params = Vec::new();
            if let Some(root_agent_id) = root_agent_id {
                params.push(format!(
                    "root_agent_id={}",
                    crate::cli::url_encode_component(&root_agent_id)
                ));
            }
            if let Some(session_id) = session_id {
                params.push(format!(
                    "session_id={}",
                    crate::cli::url_encode_component(&session_id)
                ));
            }
            if let Some(status) = status {
                params.push(format!("status={}", status.as_str()));
            }
            if let Some(has_runtime) = has_runtime {
                params.push(format!("has_runtime={has_runtime}"));
            }
            pagination.append_query_params(&mut params);
            if !params.is_empty() {
                path.push('?');
                path.push_str(&params.join("&"));
            }
            if pagination.wants_page() {
                let agents = client.get_agent_summary_list_page_compat(&path).await?;
                printer.print(&agents)
            } else {
                let agents = client
                    .get_json::<Vec<kheish_daemon::AgentSummaryView>>(&path)
                    .await?;
                printer.print(&agents)
            }
        }
        crate::AgentsCommand::Audit { agent_id } => {
            let path = match agent_id {
                Some(agent_id) => {
                    format!(
                        "/v1/agents/audit?agent_id={}",
                        crate::cli::url_encode_component(&agent_id)
                    )
                }
                None => "/v1/agents/audit".to_string(),
            };
            let audit = client
                .get_json::<Vec<kheish_agent::AgentSupervisorAuditEntry>>(&path)
                .await?;
            printer.print(&audit)
        }
        crate::AgentsCommand::Get { agent_id } => {
            let agent_id = crate::cli::url_encode_path_segment(&agent_id);
            let agent = client
                .get_json::<serde_json::Value>(&format!("/v1/agents/{agent_id}"))
                .await?;
            printer.print(&agent)
        }
        crate::AgentsCommand::Rename { agent_id, nickname } => {
            let agent_id = crate::cli::url_encode_path_segment(&agent_id);
            let snapshot = client
                .put_json::<_, kheish_agent::ManagedAgentSnapshot>(
                    &format!("/v1/agents/{agent_id}/nickname"),
                    &kheish_daemon::SetAgentNicknameRequest { nickname },
                )
                .await?;
            printer.print(&snapshot)
        }
        crate::AgentsCommand::ClearNickname { agent_id } => {
            let agent_id = crate::cli::url_encode_path_segment(&agent_id);
            let snapshot = client
                .delete_json::<kheish_agent::ManagedAgentSnapshot>(&format!(
                    "/v1/agents/{agent_id}/nickname"
                ))
                .await?;
            printer.print(&snapshot)
        }
        crate::AgentsCommand::SpawnSidechain(args) => {
            let parent_agent_id = crate::cli::url_encode_path_segment(&args.parent_agent_id);
            let request = build_spawn_sidechain_request(client, args).await?;
            let session = client
                .post_json::<_, kheish_daemon::SessionView>(
                    &format!("/v1/agents/{parent_agent_id}/sidechains"),
                    &request,
                )
                .await?;
            printer.print(&session)
        }
        crate::AgentsCommand::ExplainSidechain(args) => {
            let parent_agent_id = crate::cli::url_encode_path_segment(&args.parent_agent_id);
            let request = build_spawn_sidechain_request(client, args).await?;
            let decision = client
                .post_json::<_, kheish_daemon::SubagentPolicyDecisionView>(
                    &format!("/v1/agents/{parent_agent_id}/sidechains/explain"),
                    &request,
                )
                .await?;
            printer.print(&decision)
        }
        crate::AgentsCommand::DrainMailbox { agent_id } => {
            let agent_id = crate::cli::url_encode_path_segment(&agent_id);
            let mailbox = client
                .get_json::<Vec<serde_json::Value>>(&format!("/v1/agents/{agent_id}/mailbox"))
                .await?;
            printer.print(&mailbox)
        }
        crate::AgentsCommand::MailboxDeadLetters { agent_id } => {
            let agent_id = crate::cli::url_encode_path_segment(&agent_id);
            let mailbox = client
                .get_json::<Vec<serde_json::Value>>(&format!(
                    "/v1/agents/{agent_id}/mailbox/dead-letter"
                ))
                .await?;
            printer.print(&mailbox)
        }
        crate::AgentsCommand::AckMailbox {
            agent_id,
            message_id,
        } => {
            let agent_id = crate::cli::url_encode_path_segment(&agent_id);
            let message_id = crate::cli::url_encode_path_segment(&message_id);
            let response = client
                .post_json::<_, kheish_daemon::AckMailboxResponse>(
                    &format!("/v1/agents/{agent_id}/mailbox/{message_id}/ack"),
                    &serde_json::json!({}),
                )
                .await?;
            printer.print(&response)
        }
    }
}
