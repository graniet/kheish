//! Shared request builders for CLI commands.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

/// Builds completion requirements for a session input command.
pub(crate) fn build_completion_requirements(
    args: &crate::SessionInputArgs,
) -> Option<Vec<kheish_types::CompletionRequirement>> {
    if let Some(path) = args.require_workspace_file_path.clone() {
        return Some(vec![kheish_types::CompletionRequirement::WorkspaceFile {
            path: Some(path),
        }]);
    }
    args.require_workspace_file_write
        .then(|| vec![kheish_types::CompletionRequirement::WorkspaceFile { path: None }])
}

/// Builds an optional subtask payload for sidechain spawning.
pub(crate) async fn build_subtask_request(
    args: &crate::SpawnSidechainArgs,
) -> Result<Option<kheish_daemon::SidechainSubtaskRequest>> {
    let has_subtask = args.subtask_name.is_some()
        || args.subtask_description.is_some()
        || args.subtask_content.is_some()
        || args.subtask_content_file.is_some()
        || args.subtask_stdin;
    if !has_subtask {
        return Ok(None);
    }

    let name = args
        .subtask_name
        .clone()
        .context("missing --subtask-name for sidechain subtask")?;
    let description = args
        .subtask_description
        .clone()
        .context("missing --subtask-description for sidechain subtask")?;
    let content = crate::cli::read_text_input(
        args.subtask_content.clone(),
        args.subtask_content_file.as_deref(),
        args.subtask_stdin,
    )
    .await?;
    Ok(Some(kheish_daemon::SidechainSubtaskRequest {
        name,
        description,
        content,
        input_items: Vec::new(),
        attachments: Vec::new(),
    }))
}

/// Builds the create-schedule request used by the CLI.
pub(crate) async fn build_schedule_create_request(
    args: &crate::CreateScheduleArgs,
    known_route_ids: &BTreeSet<String>,
) -> Result<kheish_daemon::CreateScheduleRequest> {
    let content = crate::cli::read_text_input(
        args.content.clone(),
        args.content_file.as_deref(),
        args.stdin,
    )
    .await?;
    let cadence = build_schedule_cadence(args)?;
    let (provider, generation) = crate::cli::normalize_provider_and_generation(
        args.provider.clone(),
        args.generation.build().await?,
        known_route_ids,
    )?;
    Ok(kheish_daemon::CreateScheduleRequest {
        name: args.name.clone(),
        target_session_id: args.session_id.clone(),
        target_agent_id: None,
        owner_session_id: None,
        owner_agent_id: None,
        created_by_run_id: None,
        cadence,
        max_executions: args.max_executions,
        overlap_policy: args
            .overlap_policy
            .unwrap_or(crate::ScheduleOverlapPolicyArg::Skip)
            .into(),
        misfire_policy: args
            .misfire_policy
            .unwrap_or(crate::ScheduleMisfirePolicyArg::CoalesceOnce)
            .into(),
        request: Some(kheish_daemon::SubmitInputRequest {
            provider,
            source_plugin: Some("scheduler".to_string()),
            source_kind: Some("cli_schedule".to_string()),
            actor_id: Some("operator".to_string()),
            content,
            input_items: Vec::new(),
            attachments: Vec::new(),
            generation,
            completion_requirements: None,
            metadata: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
            reply_address: None,
        }),
        observation_materialization: None,
    })
}

/// Builds the observation selection payload shared by materialization commands.
pub(crate) fn build_observation_selection(
    observation_ids: &[String],
    source_id: Option<&str>,
    stream_id: Option<&str>,
    capture_group_id: Option<&str>,
    max_observations: u64,
    lookback_seconds: Option<u64>,
) -> Result<kheish_daemon::ObservationSelection> {
    let observation_ids = observation_ids
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if !observation_ids.is_empty() {
        if source_id.is_some() || stream_id.is_some() || capture_group_id.is_some() {
            bail!(
                "--source-id, --stream-id, and --capture-group-id cannot be combined with --observation-id"
            );
        }
        return Ok(kheish_daemon::ObservationSelection::ObservationIds { observation_ids });
    }
    let capture_group_id = capture_group_id
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(capture_group_id) = capture_group_id {
        if source_id.is_some() || stream_id.is_some() {
            bail!("--capture-group-id cannot be combined with --source-id or --stream-id");
        }
        return Ok(kheish_daemon::ObservationSelection::ObservationGroup {
            capture_group_id: capture_group_id.to_string(),
            max_observations,
            lookback_seconds,
        });
    }
    let stream_id = match stream_id {
        Some(stream_id) => {
            let stream_id = stream_id.trim();
            anyhow::ensure!(!stream_id.is_empty(), "--stream-id cannot be empty");
            Some(stream_id)
        }
        None => None,
    };
    if stream_id.is_some() && source_id.is_none() {
        bail!("--stream-id requires --source-id");
    }
    let source_id = source_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!("provide --source-id, --capture-group-id, or at least one --observation-id")
        })?;
    Ok(match stream_id {
        Some(stream_id) => kheish_daemon::ObservationSelection::LatestFromStream {
            source_id: source_id.to_string(),
            stream_id: stream_id.to_string(),
            max_observations,
            lookback_seconds,
        },
        None => kheish_daemon::ObservationSelection::LatestFromSource {
            source_id: source_id.to_string(),
            max_observations,
            lookback_seconds,
        },
    })
}

/// Resolves the raw-asset policy flags for observation materialization.
pub(crate) fn resolve_observation_raw_asset_policy(
    without_raw_assets: bool,
    raw_assets_policy: Option<crate::ObservationRawAssetPolicyArg>,
) -> Result<Option<kheish_daemon::ObservationRawAssetPolicy>> {
    if without_raw_assets && raw_assets_policy.is_some() {
        bail!("--without-raw-assets cannot be combined with --raw-assets-policy");
    }
    Ok(match (without_raw_assets, raw_assets_policy) {
        (true, _) => Some(kheish_daemon::ObservationRawAssetPolicy::Never),
        (false, Some(policy)) => Some(policy.into()),
        (false, None) => None,
    })
}

async fn build_observation_submit_input_request(
    target_session_id: &str,
    content: Option<String>,
    content_file: Option<&Path>,
    stdin: bool,
    metadata_json: Option<&str>,
    metadata_file: Option<&Path>,
    reply_plugin: Option<String>,
    reply_address: Option<String>,
    provider: Option<String>,
    generation: crate::GenerationArgs,
    known_route_ids: &BTreeSet<String>,
) -> Result<kheish_daemon::SubmitInputRequest> {
    let content = crate::cli::read_optional_text_input(content, content_file, stdin)
        .await?
        .unwrap_or_default();
    let metadata = crate::cli::read_optional_json_input(metadata_json, metadata_file)
        .await?
        .unwrap_or(Value::Null);
    let (provider, generation) = crate::cli::normalize_provider_and_generation(
        provider,
        generation.build().await?,
        known_route_ids,
    )?;
    Ok(kheish_daemon::SubmitInputRequest {
        provider,
        source_plugin: None,
        source_kind: None,
        actor_id: Some(target_session_id.to_string()),
        content,
        input_items: Vec::new(),
        attachments: Vec::new(),
        generation,
        completion_requirements: None,
        metadata: Some(metadata),
        binding_keys: Vec::new(),
        reply_targets: Vec::new(),
        reply_plugin,
        reply_address,
    })
}

/// Builds an observation materialization request from CLI flags.
pub(crate) async fn build_observation_materialization_request(
    args: &crate::ObservationMaterializeArgs,
    known_route_ids: &BTreeSet<String>,
) -> Result<kheish_daemon::ObservationMaterializationRequest> {
    Ok(kheish_daemon::ObservationMaterializationRequest {
        target_session_id: args.target_session_id.clone(),
        selection: build_observation_selection(
            &args.observation_ids,
            args.source_id.as_deref(),
            args.stream_id.as_deref(),
            args.capture_group_id.as_deref(),
            args.max_observations,
            args.lookback_seconds,
        )?,
        request: build_observation_submit_input_request(
            &args.target_session_id,
            args.content.clone(),
            args.content_file.as_deref(),
            args.stdin,
            args.metadata_json.as_deref(),
            args.metadata_file.as_deref(),
            args.reply_plugin.clone(),
            args.reply_address.clone(),
            args.provider.clone(),
            args.generation.clone(),
            known_route_ids,
        )
        .await?,
        include_raw_assets: !args.without_raw_assets,
        raw_asset_policy: resolve_observation_raw_asset_policy(
            args.without_raw_assets,
            args.raw_assets_policy,
        )?,
        fail_when_empty: !args.allow_empty,
    })
}

/// Builds a schedule request that triggers observation materialization.
pub(crate) async fn build_observation_schedule_create_request(
    args: &crate::ObservationScheduleArgs,
    known_route_ids: &BTreeSet<String>,
) -> Result<kheish_daemon::CreateScheduleRequest> {
    Ok(kheish_daemon::CreateScheduleRequest {
        name: args.name.clone(),
        target_session_id: args.session_id.clone(),
        target_agent_id: None,
        owner_session_id: None,
        owner_agent_id: None,
        created_by_run_id: None,
        cadence: build_schedule_cadence_from_parts(
            args.at.as_deref(),
            args.every_seconds,
            args.cron.as_deref(),
            args.timezone.as_deref(),
        )?,
        max_executions: args.max_executions,
        overlap_policy: args
            .overlap_policy
            .unwrap_or(crate::ScheduleOverlapPolicyArg::Skip)
            .into(),
        misfire_policy: args
            .misfire_policy
            .unwrap_or(crate::ScheduleMisfirePolicyArg::CoalesceOnce)
            .into(),
        request: None,
        observation_materialization: Some(kheish_daemon::ObservationMaterializationRequest {
            target_session_id: args.session_id.clone(),
            selection: build_observation_selection(
                &args.observation_ids,
                args.source_id.as_deref(),
                args.stream_id.as_deref(),
                args.capture_group_id.as_deref(),
                args.max_observations,
                args.lookback_seconds,
            )?,
            request: build_observation_submit_input_request(
                &args.session_id,
                args.content.clone(),
                args.content_file.as_deref(),
                args.stdin,
                args.metadata_json.as_deref(),
                args.metadata_file.as_deref(),
                args.reply_plugin.clone(),
                args.reply_address.clone(),
                args.provider.clone(),
                args.generation.clone(),
                known_route_ids,
            )
            .await?,
            include_raw_assets: !args.without_raw_assets,
            raw_asset_policy: resolve_observation_raw_asset_policy(
                args.without_raw_assets,
                args.raw_assets_policy,
            )?,
            fail_when_empty: !args.allow_empty,
        }),
    })
}

/// Builds an optional per-session route policy from CLI flags.
pub(crate) async fn build_session_route_policy(
    args: &crate::SessionSetRouteArgs,
    known_route_ids: &BTreeSet<String>,
) -> Result<Option<kheish_types::SessionRoutePolicy>> {
    if args.clear {
        if args.provider.is_some() || args.generation.is_set() {
            bail!("--clear cannot be combined with provider or generation overrides");
        }
        return Ok(None);
    }

    let (provider, generation) = crate::cli::normalize_provider_and_generation(
        args.provider.clone(),
        args.generation.build().await?,
        known_route_ids,
    )?;
    if provider.is_none() && generation.is_none() {
        bail!("provide --provider, one generation override, or --clear");
    }
    Ok(Some(kheish_types::SessionRoutePolicy {
        provider,
        generation,
    }))
}

fn build_schedule_cadence(
    args: &crate::CreateScheduleArgs,
) -> Result<kheish_daemon::ScheduleCadence> {
    build_schedule_cadence_from_parts(
        args.at.as_deref(),
        args.every_seconds,
        args.cron.as_deref(),
        args.timezone.as_deref(),
    )
}

pub(crate) fn build_schedule_cadence_from_parts(
    at: Option<&str>,
    every_seconds: Option<u64>,
    cron: Option<&str>,
    timezone: Option<&str>,
) -> Result<kheish_daemon::ScheduleCadence> {
    let mode_count = [at.is_some(), every_seconds.is_some(), cron.is_some()]
        .into_iter()
        .filter(|flag| *flag)
        .count();
    if mode_count != 1 {
        bail!("provide exactly one of --at, --every-seconds, or --cron");
    }
    if let Some(at) = at {
        let timestamp = chrono::DateTime::parse_from_rfc3339(at)
            .with_context(|| format!("invalid RFC3339 timestamp {at:?}"))?
            .timestamp_millis();
        anyhow::ensure!(timestamp > 0, "--at must be after the Unix epoch");
        return Ok(kheish_daemon::ScheduleCadence::Once {
            fire_at_ms: timestamp as u64,
        });
    }
    if let Some(every_seconds) = every_seconds {
        return Ok(kheish_daemon::ScheduleCadence::Interval { every_seconds });
    }
    Ok(kheish_daemon::ScheduleCadence::Cron {
        expression: cron
            .map(str::to_string)
            .expect("cron is present when mode_count == 1"),
        timezone: timezone.unwrap_or("UTC").to_string(),
    })
}
