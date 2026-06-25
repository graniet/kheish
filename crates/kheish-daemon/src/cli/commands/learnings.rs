//! Learning command handlers.

use anyhow::{Result, bail};

fn scope_from_parts(
    scope_kind: Option<crate::LearningScopeKindArg>,
    scope_id: Option<String>,
) -> Result<Option<kheish_types::LearningScope>> {
    match (scope_kind, scope_id) {
        (None, None) => Ok(None),
        (Some(crate::LearningScopeKindArg::Workspace), None) => {
            Ok(Some(kheish_types::LearningScope {
                kind: kheish_types::LearningScopeKind::Workspace,
                id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
            }))
        }
        (Some(kind), Some(id)) if !id.trim().is_empty() => Ok(Some(kheish_types::LearningScope {
            kind: kind.into(),
            id,
        })),
        (Some(_), _) => bail!("--scope-id is required for the selected scope"),
        (None, Some(_)) => bail!("--scope-kind is required when --scope-id is provided"),
    }
}

fn query_params(
    query: Option<String>,
    scope_kind: Option<crate::LearningScopeKindArg>,
    scope_id: Option<String>,
    kind: Option<crate::LearningKindArg>,
    state_or_status: Option<&str>,
) -> Result<Vec<(String, String)>> {
    if scope_kind.is_none()
        && scope_id
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
    {
        bail!("--scope-kind is required when --scope-id is provided");
    }
    let mut params = Vec::new();
    if let Some(query) = query.filter(|value| !value.trim().is_empty()) {
        params.push(("query".to_string(), query));
    }
    if let Some(scope_kind) = scope_kind {
        params.push((
            "scope_kind".to_string(),
            format!("{:?}", scope_kind).to_ascii_lowercase(),
        ));
    }
    if let Some(scope_id) = scope_id.filter(|value| !value.trim().is_empty()) {
        params.push(("scope_id".to_string(), scope_id));
    }
    if let Some(kind) = kind {
        params.push((
            "kind".to_string(),
            match kind {
                crate::LearningKindArg::RunSummary => "run_summary",
                crate::LearningKindArg::Fact => "fact",
                crate::LearningKindArg::Preference => "preference",
                crate::LearningKindArg::Decision => "decision",
                crate::LearningKindArg::Procedure => "procedure",
            }
            .to_string(),
        ));
    }
    if let Some(state_or_status) = state_or_status {
        params.push(("state".to_string(), state_or_status.to_string()));
    }
    Ok(params)
}

fn build_path(base: &str, params: Vec<(String, String)>) -> String {
    if params.is_empty() {
        return base.to_string();
    }
    let query = params
        .into_iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                crate::cli::url_encode_component(&key),
                crate::cli::url_encode_component(&value)
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{query}")
}

fn append_learning_list_filters(
    params: &mut Vec<(String, String)>,
    status: Option<crate::LearningStatusArg>,
    policy_decision: Option<crate::LearningPolicyDecisionArg>,
    policy_actor: Option<String>,
    matched_rule_name: Option<String>,
) {
    if let Some(status) = status {
        params.push((
            "status".to_string(),
            match status {
                crate::LearningStatusArg::Provisional => "provisional",
                crate::LearningStatusArg::Active => "active",
                crate::LearningStatusArg::Superseded => "superseded",
                crate::LearningStatusArg::Revoked => "revoked",
            }
            .to_string(),
        ));
    }
    if let Some(policy_decision) = policy_decision {
        params.push((
            "policy_decision".to_string(),
            match policy_decision {
                crate::LearningPolicyDecisionArg::Manual => "manual",
                crate::LearningPolicyDecisionArg::Automatic => "automatic",
                crate::LearningPolicyDecisionArg::Escalated => "escalated",
            }
            .to_string(),
        ));
    }
    if let Some(policy_actor) = policy_actor.filter(|value| !value.trim().is_empty()) {
        params.push(("policy_actor".to_string(), policy_actor));
    }
    if let Some(matched_rule_name) = matched_rule_name.filter(|value| !value.trim().is_empty()) {
        params.push(("matched_rule_name".to_string(), matched_rule_name));
    }
}

/// Handles `learnings ...`.
pub(crate) async fn run_learnings_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::LearningsCommand,
) -> Result<()> {
    match command {
        crate::LearningsCommand::Candidates { command } => match command {
            crate::LearningCandidatesCommand::List {
                query,
                scope_kind,
                scope_id,
                kind,
                state,
            } => {
                let state = state.map(|value| match value {
                    crate::LearningCandidateStateArg::Pending => "pending",
                    crate::LearningCandidateStateArg::Escalated => "escalated",
                    crate::LearningCandidateStateArg::Published => "published",
                    crate::LearningCandidateStateArg::Rejected => "rejected",
                });
                let candidates = client
                    .get_json::<Vec<kheish_daemon::LearningCandidateView>>(&build_path(
                        "/v1/learning-candidates",
                        query_params(query, scope_kind, scope_id, kind, state)?,
                    ))
                    .await?;
                printer.print(&candidates)
            }
            crate::LearningCandidatesCommand::Get { candidate_id } => {
                let candidate = client
                    .get_json::<kheish_daemon::LearningCandidateView>(&format!(
                        "/v1/learning-candidates/{candidate_id}"
                    ))
                    .await?;
                printer.print(&candidate)
            }
            crate::LearningCandidatesCommand::Create(args) => {
                let content = crate::cli::read_text_input(
                    args.content,
                    args.content_file.as_deref(),
                    args.stdin,
                )
                .await?;
                let candidate = client
                    .post_json::<_, kheish_daemon::LearningCandidateView>(
                        "/v1/learning-candidates",
                        &kheish_daemon::CreateLearningCandidateRequest {
                            scope: scope_from_parts(Some(args.scope_kind), args.scope_id)?
                                .expect("scope"),
                            kind: args.kind.into(),
                            sensitivity: args.sensitivity.into(),
                            content,
                            confidence: args.confidence,
                            source: kheish_types::LearningSourceRef {
                                run_id: args.source_run_id,
                                session_id: args.source_session_id,
                                agent_id: args.source_agent_id,
                                input_event_offset: args.source_input_event_offset,
                                observation_id: args.source_observation_id,
                                derivation_id: args.source_derivation_id,
                            },
                            evidence_refs: Vec::new(),
                            expires_at_ms: args.expires_at_ms,
                        },
                    )
                    .await?;
                printer.print(&candidate)
            }
            crate::LearningCandidatesCommand::Publish(args) => {
                let content = crate::cli::read_optional_text_input(
                    args.content,
                    args.content_file.as_deref(),
                    args.stdin,
                )
                .await?;
                let candidate_id = crate::cli::url_encode_path_segment(&args.candidate_id);
                let published = client
                    .post_json::<_, kheish_daemon::LearningView>(
                        &format!("/v1/learning-candidates/{candidate_id}/publish"),
                        &kheish_daemon::PublishLearningCandidateRequest {
                            scope: scope_from_parts(args.scope_kind, args.scope_id)?,
                            kind: args.kind.map(Into::into),
                            sensitivity: args.sensitivity.map(Into::into),
                            content,
                            confidence: args.confidence,
                            expires_at_ms: args.expires_at_ms,
                            publish_tier: args.publish_tier.map(Into::into),
                            evidence_refs: Vec::new(),
                            supersedes: args.supersedes,
                        },
                    )
                    .await?;
                printer.print(&published)
            }
            crate::LearningCandidatesCommand::Reject { candidate_id } => {
                let candidate_id = crate::cli::url_encode_path_segment(&candidate_id);
                let candidate = client
                    .post_json::<_, kheish_daemon::LearningCandidateView>(
                        &format!("/v1/learning-candidates/{candidate_id}/reject"),
                        &serde_json::json!({}),
                    )
                    .await?;
                printer.print(&candidate)
            }
        },
        crate::LearningsCommand::Skills { command } => match command {
            crate::LearningSkillsCommand::List {
                source_learning_id,
                status,
            } => {
                let mut params = Vec::new();
                if let Some(source_learning_id) =
                    source_learning_id.filter(|value| !value.trim().is_empty())
                {
                    params.push(("source_learning_id".to_string(), source_learning_id));
                }
                if let Some(status) = status {
                    params.push((
                        "status".to_string(),
                        match status {
                            crate::LearningSkillStatusArg::Draft => "draft",
                            crate::LearningSkillStatusArg::Verified => "verified",
                            crate::LearningSkillStatusArg::Canary => "canary",
                            crate::LearningSkillStatusArg::Active => "active",
                            crate::LearningSkillStatusArg::Revoked => "revoked",
                        }
                        .to_string(),
                    ));
                }
                let skills = client
                    .get_json::<Vec<kheish_daemon::LearningSkillView>>(&build_path(
                        "/v1/learning-skills",
                        params,
                    ))
                    .await?;
                printer.print(&skills)
            }
            crate::LearningSkillsCommand::Get { skill_name } => {
                let skill_name = crate::cli::url_encode_path_segment(&skill_name);
                let skill = client
                    .get_json::<kheish_daemon::LearningSkillView>(&format!(
                        "/v1/learning-skills/{skill_name}"
                    ))
                    .await?;
                printer.print(&skill)
            }
            crate::LearningSkillsCommand::Promote(args) => {
                let instructions = crate::cli::read_text_input(
                    args.instructions,
                    args.instructions_file.as_deref(),
                    args.stdin,
                )
                .await?;
                let learning_id = crate::cli::url_encode_path_segment(&args.learning_id);
                let promoted = client
                    .post_json::<_, kheish_daemon::LearningSkillView>(
                        &format!("/v1/learnings/{learning_id}/promote-skill"),
                        &kheish_daemon::CreateLearningSkillRequest {
                            skill_name: args.skill_name,
                            description: args.description,
                            when_to_use: args.when_to_use,
                            version: args.version,
                            instructions,
                            allowed_tools: args.allowed_tools,
                            blocked_tools: args.blocked_tools,
                            context: args.context.into(),
                            agent_profile: args.agent_profile,
                            provider: args.provider,
                            model: args.model,
                            fallback_model: args.fallback_model,
                            status: args.status.map(Into::into),
                        },
                    )
                    .await?;
                printer.print(&promoted)
            }
            crate::LearningSkillsCommand::RolloutResult(args) => {
                let skill_name = crate::cli::url_encode_path_segment(&args.skill_name);
                let skill = client
                    .post_json::<_, kheish_daemon::LearningSkillView>(
                        &format!("/v1/learning-skills/{skill_name}/rollout-result"),
                        &kheish_daemon::LearningSkillRolloutResultRequest {
                            kind: args.kind.into(),
                            run_id: args.run_id,
                            expected_output_contains: args.expected_output_contains,
                            definition_fingerprint: args.definition_fingerprint,
                        },
                    )
                    .await?;
                printer.print(&skill)
            }
            crate::LearningSkillsCommand::Revoke { skill_name, reason } => {
                let skill_name = crate::cli::url_encode_path_segment(&skill_name);
                let skill = client
                    .post_json::<_, kheish_daemon::LearningSkillView>(
                        &format!("/v1/learning-skills/{skill_name}/revoke"),
                        &kheish_daemon::RevokeLearningSkillRequest { reason },
                    )
                    .await?;
                printer.print(&skill)
            }
            crate::LearningSkillsCommand::Rollback { skill_name, reason } => {
                let skill_name = crate::cli::url_encode_path_segment(&skill_name);
                let skill = client
                    .post_json::<_, kheish_daemon::LearningSkillView>(
                        &format!("/v1/learning-skills/{skill_name}/rollback"),
                        &kheish_daemon::RollbackLearningSkillRequest { reason },
                    )
                    .await?;
                printer.print(&skill)
            }
        },
        crate::LearningsCommand::List {
            query,
            scope_kind,
            scope_id,
            kind,
            status,
            policy_decision,
            policy_actor,
            matched_rule_name,
        } => {
            let mut params = query_params(query, scope_kind, scope_id, kind, None)?;
            append_learning_list_filters(
                &mut params,
                status,
                policy_decision,
                policy_actor,
                matched_rule_name,
            );
            let learnings = client
                .get_json::<Vec<kheish_daemon::LearningView>>(&build_path("/v1/learnings", params))
                .await?;
            printer.print(&learnings)
        }
        crate::LearningsCommand::Get { learning_id } => {
            let learning_id = crate::cli::url_encode_path_segment(&learning_id);
            let learning = client
                .get_json::<kheish_daemon::LearningView>(&format!("/v1/learnings/{learning_id}"))
                .await?;
            printer.print(&learning)
        }
        crate::LearningsCommand::Revoke {
            learning_id,
            reason,
        } => {
            let learning_id = crate::cli::url_encode_path_segment(&learning_id);
            let learning = client
                .post_json::<_, kheish_daemon::LearningView>(
                    &format!("/v1/learnings/{learning_id}/revoke"),
                    &kheish_daemon::RevokeLearningRequest { reason },
                )
                .await?;
            printer.print(&learning)
        }
        crate::LearningsCommand::RevokeMatching {
            query,
            scope_kind,
            scope_id,
            kind,
            status,
            policy_decision,
            policy_actor,
            matched_rule_name,
            reason,
        } => {
            let scope = scope_from_parts(scope_kind, scope_id)?;
            let learnings = client
                .post_json::<_, Vec<kheish_daemon::LearningView>>(
                    "/v1/learnings/revoke-matching",
                    &kheish_daemon::RevokeMatchingLearningsRequest {
                        query,
                        scope_kind: scope.as_ref().map(|scope| scope.kind.clone()),
                        scope_id: scope.map(|scope| scope.id),
                        kind: kind.map(Into::into),
                        status: status.map(Into::into),
                        policy_decision: policy_decision.map(Into::into),
                        policy_actor,
                        matched_rule_name,
                        reason,
                    },
                )
                .await?;
            printer.print(&learnings)
        }
        crate::LearningsCommand::Supersede(args) => {
            let content =
                crate::cli::read_text_input(args.content, args.content_file.as_deref(), args.stdin)
                    .await?;
            let learning_id = crate::cli::url_encode_path_segment(&args.learning_id);
            let learning = client
                .post_json::<_, kheish_daemon::LearningView>(
                    &format!("/v1/learnings/{learning_id}/supersede"),
                    &kheish_daemon::SupersedeLearningRequest {
                        scope: scope_from_parts(args.scope_kind, args.scope_id)?,
                        kind: args.kind.map(Into::into),
                        sensitivity: args.sensitivity.map(Into::into),
                        content,
                        confidence: args.confidence,
                        expires_at_ms: args.expires_at_ms,
                    },
                )
                .await?;
            printer.print(&learning)
        }
    }
}
