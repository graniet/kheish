//! Daemon service assembly and HTTP router construction.

use parking_lot::RwLock;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result};
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::warn;

use kheish_agent::{AgentOrchestrator, AgentSupervisor};
use kheish_auth::AuthManager;
use kheish_mcp::McpManager;
use kheish_mcp::McpRuntimeSnapshot;
use kheish_output::OutputHost;
use kheish_runtime::{
    DebugControl, McpRuntimeSurface, PermissionEngine, SessionPermissionUpdateStore,
    SystemPromptBuilder, ToolRuntime,
};
use kheish_session::FileSessionStore;
use kheish_skills::SharedSkillRegistry;

use crate::SchedulerPolicyConfig;
use crate::api::{ControlPlaneAuthorizer, build_router};
use crate::assets::FileAssetStore;
use crate::audio_generation::AudioGenerationService;
use crate::boards::FileBoardStore;
use crate::builders::{next_session_seed, spawn_output_collector};
use crate::capture_provision::FileCaptureAgentStore;
use crate::channels::{
    ChannelEvent, ChannelMessageView, ChannelStimulusView, ChannelThreadWorkStateView,
    FileChannelStore,
};
use crate::config::{
    ControlPlaneAuthConfig, ControlPlaneAuthTokenFiles, ControlPlaneCorsConfig,
    SubagentPolicyConfig,
};
use crate::connectors;
use crate::control_tools::{DaemonToolControl, DaemonToolControlHandle};
use crate::delivery::DeliveryQueue;
use crate::derivations::{DerivationStatus, FileDerivationStore};
use crate::events::DaemonEventBus;
use crate::hooks::DaemonHookDispatcher;
use crate::image_generation::ImageGenerationService;
use crate::learning::FileLearningStore;
use crate::memory::{FileRunMemoryStore, RunMemoryControl, rebuild_run_memory_index_with_policy};
use crate::observation_transcripts::FileObservationTranscriptStore;
use crate::observations::FileObservationStore;
use crate::personas::FilePersonaStore;
use crate::procedural_skills::FileLearningSkillStore;
use crate::projects::FileProjectStore;
use crate::runs::{
    FileRunStore, RunRecord, RunRequestPayload, rebuild_pending_question_index,
    rebuild_session_run_state, repair_pending_approval_payloads_from_events,
};
use crate::scheduler::FileScheduleStore;
use crate::services::{ConnectorIngressService, ConnectorService};
use crate::transcription::TranscriptionService;
use crate::{
    DaemonModelControl, DaemonOutputReceiver, DaemonState, FileDaemonStore, FileDebugStore,
};
use kheish_agent::AgentSupervisorSnapshot;

struct DaemonSessionPermissionUpdateStore<M>(Arc<DaemonState<M>>);

const HTTP_SERVER_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn apply_persisted_runtime_config(
    revision: &crate::RuntimeConfigRevisionView,
    model_control: Option<&dyn DaemonModelControl>,
    permissions: &PermissionEngine,
    system_prompt: &SystemPromptBuilder,
    hooks: &DaemonHookDispatcher,
    debug: &DebugControl,
    debug_store: &FileDebugStore,
    run_memory: &RunMemoryControl,
    tools: &ToolRuntime,
) -> Result<()> {
    if let Some(model) = revision.model.clone() {
        let control = model_control.ok_or_else(|| {
            anyhow::anyhow!("model reconfiguration is not supported by this daemon")
        })?;
        let route_id = revision
            .route_id
            .as_deref()
            .or(revision.provider.as_deref());
        control.set_route(route_id, model)?;
    }
    permissions.set_mode(revision.permission_mode.clone());
    system_prompt.set_settings(revision.system_prompt.clone());
    if hooks.settings() != revision.hooks {
        if let Err(error) = hooks.set_settings(revision.hooks.clone()) {
            tracing::warn!(
                revision = revision.revision,
                error = ?error,
                "persisted runtime hook settings are invalid for this daemon build; hooks remain disabled until reconfigured"
            );
            hooks
                .set_settings(kheish_types::HookSettings::default())
                .context("failed to disable invalid persisted runtime hooks")?;
        }
    }
    validate_persisted_debug_capture_config(revision, debug_store)?;
    debug.set_level(revision.debug_level);
    run_memory.set_policy(revision.run_memory_policy.clone())?;
    tools.set_limits(revision.tool_runtime_limits.clone())?;
    Ok(())
}

fn validate_persisted_debug_capture_config(
    revision: &crate::RuntimeConfigRevisionView,
    debug_store: &FileDebugStore,
) -> Result<()> {
    let level = revision.debug_level;
    if !level.is_enabled() {
        return Ok(());
    }
    if let Some(error) = debug_store.encryption_key_error() {
        anyhow::bail!(
            "persisted runtime config revision {} enables debug capture at {:?}, but debug capture encryption key configuration is invalid: {error}",
            revision.revision,
            level
        );
    }
    if level.captures_redacted_content()
        && let Some(error) = kheish_runtime::debug_redaction_config_error()
    {
        anyhow::bail!(
            "persisted runtime config revision {} enables debug capture at {:?}, but debug redaction extension configuration is invalid: {error}",
            revision.revision,
            level
        );
    }
    Ok(())
}

pub(crate) fn loopback_control_plane_base_url(bind: SocketAddr) -> String {
    let loopback_ip = match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    format!("http://{}", SocketAddr::new(loopback_ip, bind.port()))
}

#[async_trait::async_trait]
impl<M> SessionPermissionUpdateStore for DaemonSessionPermissionUpdateStore<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    async fn persist_session_rule_updates(
        &self,
        session_id: &str,
        updates: &[kheish_types::HookPermissionUpdate],
    ) -> Result<()> {
        self.0
            .persist_session_permission_updates(session_id, updates.to_vec())
            .await
    }
}

fn rebuild_session_index_topology(
    index: &mut crate::state::SessionIndex,
    runs: &std::collections::BTreeMap<String, RunRecord>,
    supervisor: &AgentSupervisorSnapshot,
) -> bool {
    let mut changed = false;
    let mut desired_sessions = std::collections::BTreeMap::new();
    let mut supervisor_sessions_by_agent = std::collections::BTreeMap::new();

    for (agent_id, agent) in &supervisor.agents {
        let session_id = &agent.conversation.session_id;
        desired_sessions.insert(session_id.clone(), agent_id.0.clone());
        supervisor_sessions_by_agent.insert(agent_id.0.clone(), session_id.clone());
    }
    for (agent_id, snapshot) in &supervisor.terminal_snapshots {
        let session_id = &snapshot.agent.conversation.session_id;
        desired_sessions
            .entry(session_id.clone())
            .or_insert_with(|| agent_id.0.clone());
        supervisor_sessions_by_agent
            .entry(agent_id.0.clone())
            .or_insert_with(|| session_id.clone());
    }

    let mut ordered_runs = runs.values().collect::<Vec<_>>();
    ordered_runs.sort_by(|left, right| {
        left.view
            .submitted_at_ms
            .cmp(&right.view.submitted_at_ms)
            .then_with(|| {
                run_id_chronology_key(&left.view.run_id)
                    .cmp(&run_id_chronology_key(&right.view.run_id))
            })
    });

    for record in ordered_runs.iter().rev() {
        let Some(supervisor_session_id) = supervisor_sessions_by_agent.get(&record.view.agent_id)
        else {
            warn!(
                session_id = %record.view.session_id,
                run_id = %record.view.run_id,
                agent_id = %record.view.agent_id,
                "skipping run-derived session index mapping for unknown supervisor agent"
            );
            continue;
        };
        if supervisor_session_id != &record.view.session_id {
            warn!(
                session_id = %record.view.session_id,
                supervisor_session_id = %supervisor_session_id,
                run_id = %record.view.run_id,
                agent_id = %record.view.agent_id,
                "skipping run-derived session index mapping that disagrees with supervisor session ownership"
            );
            continue;
        }
        desired_sessions
            .entry(record.view.session_id.clone())
            .or_insert_with(|| record.view.agent_id.clone());
    }

    if index.sessions != desired_sessions {
        let mismatched_sessions = index
            .sessions
            .iter()
            .filter(|(session_id, agent_id)| {
                desired_sessions
                    .get(*session_id)
                    .is_some_and(|desired_agent_id| desired_agent_id != *agent_id)
            })
            .count();
        let stale_sessions = index
            .sessions
            .keys()
            .filter(|session_id| !desired_sessions.contains_key(*session_id))
            .count();
        let missing_sessions = desired_sessions
            .keys()
            .filter(|session_id| !index.sessions.contains_key(*session_id))
            .count();
        warn!(
            mismatched_sessions,
            stale_sessions,
            missing_sessions,
            "repaired daemon session index from restored supervisor topology and runs"
        );
        index.sessions = desired_sessions;
        changed = true;
    }

    for record in ordered_runs.iter().rev() {
        match &record.payload {
            RunRequestPayload::Input { request, .. }
            | RunRequestPayload::ScheduledInput { request, .. } => {
                for binding in &request.binding_keys {
                    if !index.bindings.contains_key(binding) {
                        index
                            .bindings
                            .insert(binding.clone(), record.view.session_id.clone());
                        changed = true;
                    }
                }
            }
            RunRequestPayload::ObservationMaterialization { request }
            | RunRequestPayload::ScheduledObservationMaterialization { request, .. } => {
                for binding in &request.request.binding_keys {
                    if !index.bindings.contains_key(binding) {
                        index
                            .bindings
                            .insert(binding.clone(), record.view.session_id.clone());
                        changed = true;
                    }
                }
            }
            RunRequestPayload::MailboxDelivery { .. }
            | RunRequestPayload::ChannelDelivery { .. }
            | RunRequestPayload::ParentClarification { .. }
            | RunRequestPayload::ApprovalResume { .. }
            | RunRequestPayload::UserQuestionResume { .. } => {}
        }
    }

    changed
}

fn run_id_chronology_key(run_id: &str) -> (u8, u64, &str) {
    run_id
        .strip_prefix("run-")
        .and_then(|suffix| suffix.parse::<u64>().ok())
        .map(|sequence| (1, sequence, run_id))
        .unwrap_or((0, 0, run_id))
}

fn load_channel_messages(
    store: &FileChannelStore,
    channels: &std::collections::BTreeMap<String, crate::ChannelView>,
) -> Result<
    std::collections::BTreeMap<String, std::collections::BTreeMap<String, ChannelMessageView>>,
> {
    let mut messages = std::collections::BTreeMap::new();
    for channel_id in channels.keys() {
        let mut by_id = std::collections::BTreeMap::<String, ChannelMessageView>::new();
        for entry in store.load_events(channel_id)? {
            match entry.event {
                ChannelEvent::MessagePosted { message } => {
                    by_id.insert(message.message_id.clone(), message);
                }
                ChannelEvent::ReactionSet {
                    message_id,
                    actor_id,
                    emoji,
                } => {
                    if let Some(message) = by_id.get_mut(&message_id) {
                        let mut reactions = message
                            .reactions
                            .iter()
                            .map(|reaction| {
                                (
                                    reaction.emoji.clone(),
                                    reaction
                                        .actor_ids
                                        .iter()
                                        .cloned()
                                        .collect::<std::collections::BTreeSet<_>>(),
                                )
                            })
                            .collect::<std::collections::BTreeMap<_, _>>();
                        reactions.entry(emoji).or_default().insert(actor_id);
                        message.reactions = reactions
                            .into_iter()
                            .map(|(emoji, actor_ids)| crate::ChannelReactionView {
                                count: actor_ids.len() as u64,
                                actor_ids: actor_ids.into_iter().collect(),
                                emoji,
                            })
                            .collect();
                    }
                }
                ChannelEvent::ReactionUnset {
                    message_id,
                    actor_id,
                    emoji,
                } => {
                    if let Some(message) = by_id.get_mut(&message_id) {
                        let mut reactions = message
                            .reactions
                            .iter()
                            .map(|reaction| {
                                (
                                    reaction.emoji.clone(),
                                    reaction
                                        .actor_ids
                                        .iter()
                                        .cloned()
                                        .collect::<std::collections::BTreeSet<_>>(),
                                )
                            })
                            .collect::<std::collections::BTreeMap<_, _>>();
                        if let Some(actors) = reactions.get_mut(&emoji) {
                            actors.remove(&actor_id);
                            if actors.is_empty() {
                                reactions.remove(&emoji);
                            }
                        }
                        message.reactions = reactions
                            .into_iter()
                            .map(|(emoji, actor_ids)| crate::ChannelReactionView {
                                count: actor_ids.len() as u64,
                                actor_ids: actor_ids.into_iter().collect(),
                                emoji,
                            })
                            .collect();
                    }
                }
                ChannelEvent::MemberJoined { .. } | ChannelEvent::MemberLeft { .. } => {}
            }
        }
        messages.insert(channel_id.clone(), by_id);
    }
    Ok(messages)
}

fn load_channel_leases(
    store: &FileChannelStore,
    channels: &std::collections::BTreeMap<String, crate::ChannelView>,
) -> Result<
    std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, crate::ChannelTurnLeaseView>,
    >,
> {
    let persisted = store.load_leases()?;
    Ok(channels
        .keys()
        .map(|channel_id| {
            let leases = persisted
                .get(channel_id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|lease| (lease.turn_id.clone(), lease))
                .collect::<std::collections::BTreeMap<_, _>>();
            (channel_id.clone(), leases)
        })
        .collect())
}

fn load_channel_stimuli(
    store: &FileChannelStore,
    channels: &std::collections::BTreeMap<String, crate::ChannelView>,
) -> Result<
    std::collections::BTreeMap<String, std::collections::BTreeMap<String, ChannelStimulusView>>,
> {
    let persisted = store.load_stimuli()?;
    Ok(channels
        .keys()
        .map(|channel_id| {
            let stimuli = persisted
                .get(channel_id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|stimulus| (stimulus.stimulus_id.clone(), stimulus))
                .collect::<std::collections::BTreeMap<_, _>>();
            (channel_id.clone(), stimuli)
        })
        .collect())
}

fn load_channel_thread_states(
    store: &FileChannelStore,
    channels: &std::collections::BTreeMap<String, crate::ChannelView>,
) -> Result<
    std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, ChannelThreadWorkStateView>,
    >,
> {
    let persisted = store.load_thread_states()?;
    Ok(channels
        .keys()
        .map(|channel_id| {
            let states = persisted
                .get(channel_id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|state| (state.thread_root_message_id.clone(), state))
                .collect::<std::collections::BTreeMap<_, _>>();
            (channel_id.clone(), states)
        })
        .collect())
}

/// A running daemon service.
pub struct DaemonService {
    router: Router,
    readiness: Arc<AtomicBool>,
    mcp_manager: Option<Arc<McpManager>>,
    ingress_tasks: Vec<JoinHandle<()>>,
    ingress_shutdown: Option<watch::Sender<bool>>,
    delivery_task: Option<JoinHandle<()>>,
    learning_publication_task: Option<JoinHandle<()>>,
    scheduler_task: Option<JoinHandle<()>>,
    observation_transcript_task: Option<JoinHandle<()>>,
    user_question_expiration_task: Option<JoinHandle<()>>,
    debug_retention_task: Option<JoinHandle<()>>,
    channel_lease_task: Option<JoinHandle<()>>,
    channel_stimulus_task: Option<JoinHandle<()>>,
    channel_heartbeat_task: Option<JoinHandle<()>>,
}

impl DaemonService {
    /// Builds a daemon service from an orchestrator and state root.
    pub(crate) async fn new<M>(
        control_plane_base_url: String,
        control_plane_bind: SocketAddr,
        state_root: impl Into<PathBuf>,
        workspace_root: impl Into<PathBuf>,
        assets: Arc<FileAssetStore>,
        orchestrator: AgentOrchestrator<M>,
        supervisor: Arc<AgentSupervisor>,
        permissions: Arc<PermissionEngine>,
        sessions: Arc<FileSessionStore>,
        system_prompt: Arc<SystemPromptBuilder>,
        hooks: Arc<DaemonHookDispatcher>,
        debug: DebugControl,
        observer: Arc<dyn kheish_runtime::RuntimeObserver>,
        tools: Arc<ToolRuntime>,
        mcp: McpRuntimeSnapshot,
        mcp_surface: Arc<RwLock<McpRuntimeSurface>>,
        mcp_manager: Option<Arc<McpManager>>,
        model_control: Option<Arc<dyn DaemonModelControl>>,
        events: DaemonEventBus,
        output_host: Arc<OutputHost>,
        output_receiver: DaemonOutputReceiver,
        delivery_queue: Arc<DeliveryQueue>,
        audio_generation: Option<Arc<AudioGenerationService>>,
        image_generation: Option<Arc<ImageGenerationService>>,
        transcription_service: Option<Arc<TranscriptionService>>,
        runtime_config_service: crate::services::RuntimeConfigService,
        auth_manager: Arc<AuthManager>,
        skills: Arc<SharedSkillRegistry>,
        tool_control: DaemonToolControlHandle,
        connectors: Arc<connectors::ConnectorRegistry>,
        external_connector_runtime: Arc<connectors::ExternalConnectorRuntimeService>,
        connector_service: Arc<ConnectorService>,
        subagent_policy: SubagentPolicyConfig,
        scheduler_policy: SchedulerPolicyConfig,
        scheduler_enabled: bool,
        control_plane_auth: ControlPlaneAuthConfig,
        control_plane_auth_token_files: ControlPlaneAuthTokenFiles,
        control_plane_cors: ControlPlaneCorsConfig,
        state_root_lock_held: bool,
    ) -> Result<Self>
    where
        M: kheish_core::ModelDriver + Send + Sync + 'static,
    {
        let state_root = state_root.into();
        let workspace_root = workspace_root.into();
        let store = FileDaemonStore::new(&state_root);
        let run_store = FileRunStore::new(&state_root);
        let run_memory_store = FileRunMemoryStore::new(&state_root);
        let run_memory_control = RunMemoryControl::default();
        if let Some(revision) = runtime_config_service.current() {
            run_memory_control
                .set_policy(revision.run_memory_policy.clone())
                .with_context(|| {
                    format!(
                        "failed to apply persisted run-memory policy revision {}",
                        revision.revision
                    )
                })?;
        }
        let learning_store = FileLearningStore::new(&state_root);
        let learning_skill_store = FileLearningSkillStore::new(&state_root);
        let debug_store = FileDebugStore::new(&state_root);
        let schedule_store = FileScheduleStore::new(&state_root);
        let board_store = FileBoardStore::new(&state_root);
        let channel_store = FileChannelStore::new(&state_root);
        let derivation_store = FileDerivationStore::new(&state_root);
        let observation_store = FileObservationStore::new(&state_root);
        let observation_transcript_store = FileObservationTranscriptStore::new(&state_root);
        let capture_agent_store = FileCaptureAgentStore::new(&state_root);
        let persona_store = FilePersonaStore::new(&state_root);
        let project_store = FileProjectStore::new(&state_root);
        let playbook_store = crate::playbooks::FilePlaybookStore::new(&state_root);
        let index_modified_at = store.index_modified_at()?;
        let mut runs = run_store.load_runs()?;
        let repaired_pending_approval_payloads =
            repair_pending_approval_payloads_from_events(&run_store, &mut runs)?;
        if repaired_pending_approval_payloads > 0 {
            warn!(
                count = repaired_pending_approval_payloads,
                "backfilled pending approval payloads from run event logs during daemon startup"
            );
        }
        let mut index = store.load_index()?;
        let channel_index = channel_store.load_index()?;
        let connector_ingress_service = Arc::new(ConnectorIngressService::load(&state_root));
        if connector_ingress_service
            .migrate_legacy_index_state(&mut index)
            .await?
        {
            store.save_index(&index)?;
        }
        let persona_index = persona_store.load_index_repaired()?;
        connector_service
            .validate_session_policies(&persona_index)
            .await?;
        let supervisor_snapshot = supervisor.snapshot();
        let rebuilt_index_topology =
            rebuild_session_index_topology(&mut index, &runs, &supervisor_snapshot);
        let run_memory_rebuild_started_at = crate::runs::now_ms();
        let rebuilt_run_memories = match rebuild_run_memory_index_with_policy(
            &runs,
            &run_memory_store,
            run_memory_rebuild_started_at,
            &run_memory_control.policy(),
        ) {
            Ok(rebuilt) => rebuilt,
            Err(error) => {
                run_memory_control.record_maintenance(
                    crate::RunMemoryMaintenanceStatusView::scan_error(
                        "startup",
                        run_memory_rebuild_started_at,
                        error.to_string(),
                    ),
                );
                return Err(error);
            }
        };
        let rebuilt_run_memory_index = index.run_memories != rebuilt_run_memories.index;
        let mut run_memory_maintenance = crate::RunMemoryMaintenanceStatusView::from_rebuild(
            "startup",
            run_memory_rebuild_started_at,
            rebuilt_run_memory_index,
            &rebuilt_run_memories,
        );
        for run_id in &rebuilt_run_memories.pruned_run_ids {
            if let Err(error) = run_memory_store.delete_run_memory(run_id) {
                run_memory_maintenance.record_prune_error(
                    "delete_run_memory",
                    "delete_failed",
                    Some(run_id.clone()),
                    None,
                    error.to_string(),
                );
                run_memory_control.record_maintenance(run_memory_maintenance);
                return Err(error);
            }
        }
        for path in &rebuilt_run_memories.pruned_orphan_files {
            if let Err(error) = run_memory_store.delete_run_memory_file(path) {
                run_memory_maintenance.record_prune_error(
                    "delete_run_memory_file",
                    "delete_failed",
                    None,
                    Some(path.display().to_string()),
                    error.to_string(),
                );
                run_memory_control.record_maintenance(run_memory_maintenance);
                return Err(error);
            }
        }
        run_memory_control.record_pruned_ttl(rebuilt_run_memories.pruned_ttl_run_ids.len());
        run_memory_control
            .record_pruned_overflow(rebuilt_run_memories.pruned_overflow_run_ids.len());
        run_memory_control.record_pruned_orphan(rebuilt_run_memories.pruned_orphan_files.len());
        run_memory_control.record_maintenance(run_memory_maintenance);
        if rebuilt_run_memory_index {
            index.run_memories = rebuilt_run_memories.index;
        }
        if rebuilt_index_topology || rebuilt_run_memory_index {
            store.save_index(&index)?;
        }
        let schedules = schedule_store.load_schedules()?;
        let mut boards = board_store.load_boards()?;
        let mut board_revisions = board_store.load_revisions()?;
        board_store.repair_invalid_revisions(&mut board_revisions, |revision| {
            let Some(render_asset) = assets.get(&revision.render_asset_id) else {
                return false;
            };
            if !render_asset.media_type.starts_with("image/") {
                return false;
            }
            if assets.read_raw(&revision.render_asset_id).is_err() {
                return false;
            }
            if let Some(state_asset_id) = revision.state_asset_id.as_deref() {
                let Some(state_asset) = assets.get(state_asset_id) else {
                    return false;
                };
                if state_asset.media_type != "application/json" {
                    return false;
                }
                let Ok((_, state_bytes)) = assets.read_raw(state_asset_id) else {
                    return false;
                };
                if crate::boards::validate_board_state_payload(
                    &revision.board_id,
                    revision.previous_revision_id.as_deref(),
                    &state_bytes,
                )
                .is_err()
                {
                    return false;
                }
                let Ok(embedded_asset_ids) =
                    crate::boards::board_state_payload_asset_ids(&state_bytes)
                else {
                    return false;
                };
                if embedded_asset_ids
                    .iter()
                    .any(|asset_id| assets.read_raw(asset_id).is_err())
                {
                    return false;
                }
            }
            true
        })?;
        board_store.repair_boards_from_revisions(&mut boards, &board_revisions)?;
        let channels = channel_store.load_channels()?;
        let channel_messages = load_channel_messages(&channel_store, &channels)?;
        let channel_leases = load_channel_leases(&channel_store, &channels)?;
        let channel_stimuli = load_channel_stimuli(&channel_store, &channels)?;
        let channel_thread_states = load_channel_thread_states(&channel_store, &channels)?;
        let mut derivations = derivation_store.load_derivations()?;
        let repaired_derivations = derivation_store
            .repair_loaded_derivations(&mut derivations, |asset_id| {
                assets.get(asset_id).is_some()
            })?;
        if repaired_derivations > 0 {
            warn!(
                count = repaired_derivations,
                "repaired completed derivations with missing result assets during daemon startup"
            );
        }
        let mut backfilled_derivation_refs = 0usize;
        for derivation in derivations.values() {
            if derivation.status != DerivationStatus::Completed
                || derivation.result_asset_id.trim().is_empty()
            {
                continue;
            }
            for asset_id in std::iter::once(derivation.result_asset_id.as_str()).chain(
                derivation
                    .backend
                    .as_ref()
                    .and_then(|backend| backend.timestamp_asset_id.as_deref()),
            ) {
                let Some(asset) = assets.get(asset_id) else {
                    continue;
                };
                if asset
                    .derivation_ids
                    .iter()
                    .any(|derivation_id| derivation_id == &derivation.derivation_id)
                {
                    continue;
                }
                let _ = assets.attach_derivation(asset_id, &derivation.derivation_id)?;
                backfilled_derivation_refs += 1;
            }
        }
        if backfilled_derivation_refs > 0 {
            warn!(
                count = backfilled_derivation_refs,
                "backfilled asset derivation provenance during daemon startup"
            );
        }
        let learning_candidates = learning_store.load_candidates()?;
        let learnings = learning_store.load_records()?;
        let learning_skills = learning_skill_store.load()?;
        let observation_sources = observation_store.load_sources()?;
        let capture_agents = capture_agent_store.load_agents()?;
        let observations = observation_store.load_observations()?;
        let observation_transcript_jobs = observation_transcript_store.load_jobs()?;
        let projects = project_store.load_projects()?;
        let project_tasks = project_store.load_tasks()?;
        let playbooks = playbook_store.load_playbooks()?;
        let flows = playbook_store.load_flows()?;
        let pending_questions = rebuild_pending_question_index(&runs);
        let session_runs = rebuild_session_run_state(&runs);
        let needs_persona_cache_repair = rebuilt_index_topology
            || (index.session_personas.is_empty()
                && !index.sessions.is_empty()
                && !persona_index.personas.is_empty());
        let needs_reply_target_cache_repair = rebuilt_index_topology
            || (index.reply_targets.is_empty() && !index.sessions.is_empty());
        let needs_task_summary_repair = rebuilt_index_topology
            || index.task_summaries.len() != index.sessions.len()
            || index
                .sessions
                .keys()
                .any(|session_id| !index.task_summaries.contains_key(session_id));
        let persona_cache_dirty_sessions = index_modified_at
            .map(|index_modified_at| {
                let sessions = FileSessionStore::new(state_root.join("sessions"));
                sessions.list_session_ids_modified_since(index_modified_at)
            })
            .transpose()?
            .unwrap_or_default();
        let reply_target_cache_dirty_sessions = persona_cache_dirty_sessions.clone();
        let needs_persona_cache_prune = index
            .session_personas
            .keys()
            .any(|session_id| !index.sessions.contains_key(session_id));
        let needs_reply_target_cache_prune = index
            .reply_targets
            .keys()
            .any(|session_id| !index.sessions.contains_key(session_id));
        let next_session_id = AtomicU64::new(next_session_seed(&store));
        let next_run_id = AtomicU64::new(
            run_store
                .next_seed()
                .max(next_session_run_idempotency_seed(&index)),
        );
        let next_schedule_id = AtomicU64::new(schedule_store.next_seed());
        let next_board_id = AtomicU64::new(board_store.next_board_seed());
        let next_board_revision_id = AtomicU64::new(board_store.next_revision_seed());
        let next_channel_id = AtomicU64::new(channel_index.next_channel_id);
        let next_channel_message_id = AtomicU64::new(channel_index.next_message_id);
        let next_channel_turn_id = AtomicU64::new(channel_index.next_turn_id);
        let next_channel_stimulus_id = AtomicU64::new(channel_index.next_stimulus_id);
        let next_derivation_id = AtomicU64::new(derivation_store.next_seed());
        let next_learning_candidate_id = AtomicU64::new(learning_store.next_candidate_seed());
        let next_learning_id = AtomicU64::new(learning_store.next_learning_seed());
        let next_source_id = AtomicU64::new(observation_store.next_source_seed());
        let next_observation_id = AtomicU64::new(observation_store.next_observation_seed());
        let next_observation_transcript_id =
            AtomicU64::new(observation_transcript_store.next_job_seed());
        let next_project_id = AtomicU64::new(project_store.next_project_seed());
        let next_project_task_id = AtomicU64::new(project_store.next_task_seed());
        let next_flow_id = AtomicU64::new(playbook_store.next_flow_seed());
        if let Some(revision) = runtime_config_service.current() {
            apply_persisted_runtime_config(
                &revision,
                model_control.as_deref(),
                permissions.as_ref(),
                system_prompt.as_ref(),
                hooks.as_ref(),
                &debug,
                &debug_store,
                &run_memory_control,
                tools.as_ref(),
            )
            .with_context(|| {
                format!(
                    "failed to apply persisted runtime config revision {}",
                    revision.revision
                )
            })?;
        }
        let state = Arc::new(DaemonState::new(
            control_plane_base_url,
            control_plane_bind,
            control_plane_auth.clone(),
            control_plane_auth_token_files.clone(),
            control_plane_cors.clone(),
            state_root_lock_held,
            state_root.clone(),
            workspace_root,
            orchestrator,
            supervisor,
            permissions.clone(),
            sessions,
            system_prompt,
            hooks,
            debug,
            run_memory_control,
            observer,
            tools,
            model_control,
            events.clone(),
            store,
            assets,
            board_store,
            channel_store,
            audio_generation,
            image_generation,
            transcription_service,
            run_store,
            runtime_config_service.clone(),
            run_memory_store,
            debug_store,
            schedule_store,
            derivation_store,
            learning_store,
            learning_skill_store,
            observation_store,
            observation_transcript_store,
            capture_agent_store,
            persona_store,
            project_store,
            playbook_store,
            index,
            persona_index,
            channel_index,
            output_host,
            delivery_queue.clone(),
            runs,
            session_runs,
            pending_questions,
            schedules,
            boards,
            board_revisions,
            channels,
            channel_messages,
            channel_leases,
            channel_stimuli,
            channel_thread_states,
            derivations,
            learning_candidates,
            learnings,
            learning_skills,
            observation_sources,
            capture_agents,
            observations,
            observation_transcript_jobs,
            projects,
            project_tasks,
            playbooks,
            flows,
            mcp,
            mcp_surface,
            mcp_manager.clone(),
            auth_manager,
            skills,
            connectors,
            external_connector_runtime,
            connector_service,
            connector_ingress_service,
            subagent_policy,
            scheduler_enabled,
            scheduler_policy,
            next_session_id,
            next_run_id,
            next_schedule_id,
            next_board_id,
            next_board_revision_id,
            next_channel_id,
            next_channel_message_id,
            next_channel_turn_id,
            next_channel_stimulus_id,
            next_derivation_id,
            next_learning_candidate_id,
            next_learning_id,
            next_source_id,
            next_observation_id,
            next_observation_transcript_id,
            next_project_id,
            next_project_task_id,
            next_flow_id,
        ));
        if let Some(revision) = runtime_config_service.current() {
            state
                .restore_runtime_learning_policy_from_config(&revision)
                .await
                .with_context(|| {
                    format!(
                        "failed to apply persisted runtime learning policy revision {}",
                        revision.revision
                    )
                })?;
        }
        state.repair_learning_skill_catalog().await?;
        if needs_persona_cache_repair {
            state.repair_session_persona_index().await?;
        } else if !persona_cache_dirty_sessions.is_empty() {
            state
                .repair_session_persona_index_for_sessions(persona_cache_dirty_sessions)
                .await?;
        } else if needs_persona_cache_prune {
            state.prune_session_persona_index().await?;
        }
        if needs_reply_target_cache_repair {
            state.repair_session_reply_target_index().await?;
        } else if !reply_target_cache_dirty_sessions.is_empty() {
            state
                .repair_session_reply_target_index_for_sessions(reply_target_cache_dirty_sessions)
                .await?;
        } else if needs_reply_target_cache_prune {
            state.prune_session_reply_target_index().await?;
        }
        if needs_task_summary_repair {
            state.repair_session_task_summary_index().await?;
        }
        permissions.bind_session_rule_update_store(Arc::new(DaemonSessionPermissionUpdateStore(
            state.clone(),
        )));
        let tool_control_state: Arc<dyn DaemonToolControl> =
            Arc::new(crate::DaemonToolControlAdapter(state.clone()));
        tool_control.bind(&tool_control_state);
        state.bind_tool_control_state(tool_control_state);
        spawn_output_collector(state.clone(), output_receiver);
        state.restore_registered_agents().await?;
        state.reconcile_channel_member_display_names().await?;
        state.archive_settled_subagents_on_boot().await?;
        state.restore_background_shell_tasks_on_boot().await?;
        state.restore_session_permission_state().await?;
        state
            .reconcile_pending_sidechain_spawn_receipts_on_boot()
            .await?;
        state.recover_run_scheduler_on_boot().await?;
        state
            .replay_committed_sidechain_spawn_receipts_on_boot()
            .await?;
        state.start_restored_run_scheduler_on_boot().await?;
        state.archive_settled_subagents_on_boot().await?;
        state
            .restore_parent_clarification_completions_on_boot()
            .await?;
        state.restore_channel_stimulus_worker_on_boot().await?;
        state.restore_semantic_capture_on_boot().await?;
        state.restore_learning_publication_worker_on_boot().await?;
        state.reconcile_project_tasks_from_runs_on_boot().await?;
        state.restore_schedule_worker_on_boot().await?;
        state
            .restore_observation_transcript_worker_on_boot()
            .await?;
        state.restore_channel_lease_worker_on_boot().await?;
        state.reap_close_on_settle_agents().await?;
        if let Err(error) = state.gc_orphaned_daemon_worktrees_on_boot().await {
            warn!(
                error = ?error,
                "failed to reclaim orphaned daemon-owned worktrees during daemon startup"
            );
        }
        if let Err(error) = state.prune_expired_debug_evidence_on_boot().await {
            warn!(
                error = ?error,
                "failed to prune expired terminal-run debug capture artifacts during daemon startup"
            );
        }
        let delivery_task = Some(delivery_queue.spawn_worker());
        let learning_publication_task = Some(state.spawn_learning_publication_worker());
        let scheduler_task = if scheduler_enabled {
            Some(state.spawn_schedule_worker())
        } else {
            warn!("background schedule dispatch worker disabled by configuration");
            None
        };
        let observation_transcript_task = Some(state.spawn_observation_transcript_worker());
        let user_question_expiration_task = Some(state.spawn_user_question_expiration_worker());
        let debug_retention_task = Some(state.spawn_debug_retention_worker());
        let channel_lease_task = Some(state.spawn_channel_lease_worker());
        let channel_stimulus_task = Some(state.spawn_channel_stimulus_worker());
        let channel_heartbeat_task = Some(state.spawn_channel_heartbeat_worker());
        let ingress_tasks = connectors::spawn_ingress_tasks(state.clone());
        let readiness = state.readiness_handle();
        let router = build_router::<M>(
            state.clone(),
            Arc::new(ControlPlaneAuthorizer::with_cors_audit_and_token_files(
                &control_plane_auth,
                &control_plane_cors,
                control_plane_auth_token_files,
                state_root.join("control-plane-auth").join("audit.jsonl"),
            )),
        )
        .merge(connectors::build_router(state.clone()))
        .merge(crate::observation_ingress::build_router(state));
        Ok(Self {
            router,
            readiness,
            mcp_manager,
            ingress_tasks: ingress_tasks.handles,
            ingress_shutdown: Some(ingress_tasks.shutdown),
            delivery_task,
            learning_publication_task,
            scheduler_task,
            observation_transcript_task,
            user_question_expiration_task,
            debug_retention_task,
            channel_lease_task,
            channel_stimulus_task,
            channel_heartbeat_task,
        })
    }

    /// Returns a cloneable router for HTTP serving.
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    /// Serves the daemon until the provided shutdown future resolves.
    pub async fn serve_with_shutdown<S>(mut self, listener: TcpListener, shutdown: S) -> Result<()>
    where
        S: Future<Output = ()> + Send + 'static,
    {
        let mcp_manager = self.mcp_manager.clone();
        let shutdown_mcp_manager = self.mcp_manager.clone();
        let ingress_shutdown = self.ingress_shutdown.clone();
        let readiness = self.readiness.clone();
        let (shutdown_started_tx, shutdown_started_rx) = oneshot::channel::<()>();
        let router = self.router.clone();
        let mut serve_task = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                shutdown.await;
                readiness.store(false, Ordering::SeqCst);
                let _ = shutdown_started_tx.send(());
                if let Some(sender) = ingress_shutdown {
                    let _ = sender.send(true);
                }
                if let Some(manager) = shutdown_mcp_manager {
                    manager.begin_shutdown().await;
                }
            })
            .await
        });
        let serve_result = tokio::select! {
            result = &mut serve_task => {
                Some(result)
            }
            _ = async move {
                let _ = shutdown_started_rx.await;
                tokio::time::sleep(HTTP_SERVER_SHUTDOWN_TIMEOUT).await;
            } => {
                warn!(
                    timeout_secs = HTTP_SERVER_SHUTDOWN_TIMEOUT.as_secs(),
                    "forcing daemon HTTP server shutdown after grace-period timeout"
                );
                serve_task.abort();
                let _ = serve_task.await;
                None
            }
        };
        if let Some(manager) = mcp_manager {
            manager.shutdown().await;
        }
        if let Some(sender) = self.ingress_shutdown.take() {
            let _ = sender.send(true);
        }
        for task in self.ingress_tasks.drain(..) {
            let mut task = task;
            if tokio::time::timeout(std::time::Duration::from_secs(12), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
        if let Some(result) = serve_result {
            result.map_err(anyhow::Error::from)??;
        }
        Ok(())
    }
}

fn next_session_run_idempotency_seed(index: &crate::state::SessionIndex) -> u64 {
    index
        .session_run_idempotency_receipts
        .values()
        .filter_map(|receipt| receipt.run_id().strip_prefix("run-")?.parse().ok())
        .max()
        .unwrap_or(0)
}

impl Drop for DaemonService {
    fn drop(&mut self) {
        if let Some(sender) = self.ingress_shutdown.take() {
            let _ = sender.send(true);
        }
        for task in &self.ingress_tasks {
            task.abort();
        }
        if let Some(task) = &self.delivery_task {
            task.abort();
        }
        if let Some(task) = &self.learning_publication_task {
            task.abort();
        }
        if let Some(task) = &self.scheduler_task {
            task.abort();
        }
        if let Some(task) = &self.observation_transcript_task {
            task.abort();
        }
        if let Some(task) = &self.user_question_expiration_task {
            task.abort();
        }
        if let Some(task) = &self.debug_retention_task {
            task.abort();
        }
        if let Some(task) = &self.channel_lease_task {
            task.abort();
        }
        if let Some(task) = &self.channel_stimulus_task {
            task.abort();
        }
        if let Some(task) = &self.channel_heartbeat_task {
            task.abort();
        }
    }
}
