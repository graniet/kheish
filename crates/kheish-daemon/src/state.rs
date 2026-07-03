//! Shared daemon state, persistence, and runtime orchestration glue.

mod asset_workflow;
mod board_workflow;
mod channel_workflow;
mod derivation_workflow;
mod goal_workflow;
mod learning_workflow;
mod observation_transcript_workflow;
mod observation_workflow;
mod output_workflow;
mod persistence;
mod persona_state;
mod playbook_workflow;
mod project_workflow;
mod run_workflow;
mod runtime;
mod scheduler_workflow;
mod session_actions;
mod session_ingress;
mod session_lifecycle;
mod session_state;
mod status;
mod subagent_lifecycle;
mod subagent_spawn;
mod task_workflow;
mod tool_control;
mod transcription_workflow;
mod views;

use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::{Instant, sleep_until};
use tracing::{debug, error, info, warn};

use kheish_agent::{
    AgentId, AgentOrchestrator, AgentRecord, AgentStatus, AgentSupervisor,
    AgentSupervisorAuditEntry, AgentSupervisorSnapshot, ChildRetentionPolicy, DaemonOwnedWorktree,
    MailboxMessage, ManagedAgentSnapshot, SubtaskSpec,
};
use kheish_auth::{
    AUTH_STORE_MASTER_KEY_ENV, AuthManager, AuthProvider, AuthSlotRecord, AuthSlotStatus,
    AuthSubjectStatus, CredentialLease, CredentialLeaseAudience, CredentialLeaseStatus,
    load_auth_store_master_key_from_env,
};
use kheish_coding_tools::configure_resolved_bash_command_workdir;
use kheish_core::{HookDispatcher, render_user_question_resolution};
use kheish_mcp::{McpManager, McpRuntimeSnapshot};
use kheish_output::{OutputHost, ResponseEnvelope};
use kheish_runtime::{
    DebugCaptureLevel, DebugControl, ExecutionScope, McpRuntimeSurface, ModelGenerationConfig,
    PermissionEngine, PermissionExplanation, PermissionMode, SystemPromptBuilder,
    SystemPromptSettings, ToolExecutionOutput, ToolRuntime, bounded_workspace_root,
    current_cancellation_token, interrupted_error, scope_execution,
};
use kheish_session::FileSessionStore;
use kheish_skills::SharedSkillRegistry;
use kheish_types::{
    ActorRef, AttachmentRef, CompletionRequirement, ConversationKey, HookEventName, HookInvocation,
    HookRuntimeState, HookSettings, InputEnvelope, InputPayload, RecoveredMemoryBundle,
    ReplyHandle, RichOutput, SessionControlState, SessionOperatorConfig, SessionRoutePolicy,
    SourceRef, ToolCallRecord, UserQuestionRequest, UserQuestionResolution,
    metadata_with_recovered_memory, normalize_reply_targets,
};
use tokio_util::sync::CancellationToken;

use crate::assets::{FileAssetStore, StoredAssetRecord};
use crate::audio_generation::AudioGenerationService;
use crate::boards::{BoardRevisionView, BoardView, FileBoardStore};
use crate::capture_provision::{CaptureAgentRecord, FileCaptureAgentStore};
use crate::channels::{
    ChannelMessageView, ChannelStimulusView, ChannelThreadWorkStateView, ChannelTurnLeaseView,
    ChannelView, FileChannelStore,
};
use crate::connectors::ExternalConnectorRuntimeService;
use crate::connectors::{ConnectorKind, ConnectorRegistry};
use crate::control_tools::{
    self, DaemonToolControl, EditImageToolRequest, EditImageToolResponse, GenerateAudioToolRequest,
    GenerateAudioToolResponse, GenerateImageToolRequest, GenerateImageToolResponse,
    MessageAgentToolRequest, PARENT_CLARIFICATION_ANSWER_MESSAGE_TYPE,
    PARENT_CLARIFICATION_ANSWER_SUBJECT, ParentClarificationToolResponse, SpawnAgentToolRequest,
    SpawnAgentToolResponse, mailbox_request_from_tool, parse_permission_mode,
    sidechain_request_from_tool, wait_for_agent_snapshot,
};
use crate::debug::{FileDebugStore, RunDebugView};
use crate::derivations::{
    DerivationCreateControls, DerivationCreateRequest, DerivationProfile, DerivationStatus,
    DerivationSubject, DerivationView, FileDerivationStore, StoredDerivationRecord,
    derivation_cache_key,
};
use crate::events::{DaemonEvent, DaemonEventBus};
use crate::hooks::DaemonHookDispatcher;
use crate::image_generation::{ImageGenerationService, ImageToolExecutionContext};
use crate::learning::{FileLearningStore, LearningCandidateView, LearningView};
use crate::memory::{
    FileRunMemoryStore, RunMemoryControl, RunMemoryIndex, RunMemoryPolicyConfig, RunMemoryRecord,
    build_run_memory_record_with_policy, rank_run_memory_record, run_memory_entry_expired,
};
use crate::model_routing::DaemonModelControl;
use crate::observation_transcripts::{
    FileObservationTranscriptStore, ObservationTranscriptJobRecord, ObservationTranscriptJobView,
};
use crate::observations::{
    CreateObservationRequest, CreateObservationSourceRequest, FileObservationStore,
    ObservationMaterializationRequest, ObservationSourceRecord, ObservationView,
    summarize_observation_materialization_request,
};
use crate::personas::{FilePersonaStore, PersonaIndex, PersonaIndexEntry, PersonaRecord};
use crate::playbooks::{
    AppendFlowEvidenceRequest, FilePlaybookStore, FlowContractCheck, FlowContractValidation,
    FlowListQuery, FlowPhaseState, FlowPhaseStatus, FlowPrimitiveRefs, FlowRecord, FlowStatus,
    FlowVerificationCheck, FlowView, KHEISH_FLOW_METADATA_KEY, PlaybookEvidenceRequirement,
    PlaybookListQuery, PlaybookManifest, PlaybookView, ProductViewFlowVerificationRequest,
    ProductViewFlowVerificationVerdict, StartFlowRequest, contains_flow_metadata,
    flow_correlation_metadata, insert_daemon_metadata, run_matches_flow_record,
};
use crate::procedural_skills::{FileLearningSkillStore, LearningSkillView};
use crate::projects::{FileProjectStore, ProjectTaskView, ProjectView};
use crate::runs::{
    DaemonRunKind, DaemonRunStatus, FileRunStore, ParentClarificationCompletionReason,
    ParentClarificationCompletionState, ParentClarificationRunRequest, RunEvent, RunEventEntry,
    RunInputIdempotency, RunRecord, RunRequestPayload, RunView, ScheduledRunOrigin,
    SessionRunState, now_ms, summarize_input_request, summarize_mailbox_request,
    summarize_parent_clarification_request,
};
use crate::scheduler::{
    DEFAULT_MAX_OWNER_SCHEDULES, FileScheduleStore, ScheduleCreateRequest, ScheduleRecord,
    ScheduleStatus, ScheduleView, build_schedule_record, resolved_flow_start_for_schedule,
    resolved_observation_materialization_request_for_schedule, resolved_request_for_schedule,
    validate_schedule_create_request,
};
use crate::services::{
    BackgroundShellTaskFinalState, BackgroundShellTaskHandle, BoardService,
    ChannelReactionMutation, ChannelService, ConnectorConfigRecord, ConnectorIngressService,
    ConnectorService, CreateChannelMessageRecord, CreateChannelRecord, DeliveryService,
    DerivationService, FinalizedBackgroundShellTask, GoalService, LearningCandidateListFilter,
    LearningExtractionService, LearningJudgeService, LearningListFilter, LearningMutationMode,
    LearningPolicyService, LearningService, LearningSkillService, ObservationService,
    PersonaService, PlaybookService, ProjectService, RunService, RuntimeConfigService,
    ScheduleDueCompletion, ScheduleDueDecision, ScheduleService, SchedulerSnapshot,
    SessionGoalPatch, SessionService, SpawnRequestReservation, SpawnReservation, SubagentService,
    TaskService, apply_background_shell_shutdown_outcome,
};
use crate::shell_tasks::{
    BACKGROUND_SHELL_STALL_THRESHOLD, BACKGROUND_SHELL_TASK_ID_ENV,
    BACKGROUND_SHELL_WATCHDOG_INTERVAL, BackgroundShellShutdownGuard, BackgroundShellTaskRequest,
    ShellTaskOutputStats, ShellTaskOutputWriter, background_shell_metadata,
    background_shell_process_group_id, background_shell_process_started_at,
    background_shell_shutdown_targets_visible, configure_background_shell_command,
    looks_like_interactive_prompt, read_task_output_progress, shutdown_background_shell_processes,
};
use crate::transcription::TranscriptionService;
use crate::{
    AckMailboxResponse, AgentSummaryView, AssetSummaryView, AssetView, CancelUserQuestionRequest,
    CheckPermissionMatrixRequest, CheckPermissionRequest, CreateSessionRequest, DaemonOutputRecord,
    HookDeadLetterView, InlineAssetUpload, InputAttachmentRequest, InterruptSessionResponse,
    PendingQuestionView, PermissionMatrixModeView, PermissionMatrixView, PostMailboxRequest,
    PostMailboxResponse, ResolveApprovalsRequest, ResolveUserQuestionRequest,
    RunRetentionPruneRequest, RunRetentionPruneResponse, RuntimeSettingsView,
    SchedulerPolicyConfig, SessionEventLogView, SessionGoalResponse, SessionMemoryContextView,
    SessionMemorySearchResultKind, SessionMemorySearchResultView, SessionMemorySearchView,
    SessionPermissionAuditListView, SessionView, SessionViewSummary, SkillSummaryView, SkillView,
    SpawnSidechainRequest, SubagentPolicyConfig, SubmitInputItemRequest, SubmitInputRequest,
    TaskOutputView,
};

use persistence::normalized_submit_input_items;
pub(crate) use persistence::{
    ConnectorCursorState, ConnectorIngressLookup, ConnectorIngressReceiptState,
    ConnectorIngressReservation, DaemonOutputPlugin, DaemonOutputReceiver, FileDaemonStore,
    ObservationIngressReceiptState, ObservationIngressReservation, SessionIndex,
    SessionRunIdempotencyReceiptState, SessionRunIdempotencyReservation,
    SessionTaskStatusSummaryState, SidechainSpawnReceiptState, prune_observation_ingress_receipts,
    prune_run_operation_idempotency_receipts, prune_session_run_idempotency_receipts,
};
use persistence::{ResolvedInputPart, import_inline_asset};
pub(crate) use tool_control::DaemonToolControlAdapter;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveredMemoryBundleUsage {
    Prompt,
    Preview,
}

fn render_task_status(status: &kheish_types::TaskStatus) -> &'static str {
    match status {
        kheish_types::TaskStatus::Pending => "pending",
        kheish_types::TaskStatus::InProgress => "in_progress",
        kheish_types::TaskStatus::Blocked => "blocked",
        kheish_types::TaskStatus::Completed => "completed",
        kheish_types::TaskStatus::Failed => "failed",
        kheish_types::TaskStatus::Cancelled => "cancelled",
    }
}

fn spawn_pipe_collector<R>(
    reader: Option<R>,
    output: Arc<tokio::sync::Mutex<ShellTaskOutputWriter>>,
    error_tx: tokio::sync::mpsc::UnboundedSender<String>,
) -> tokio::task::JoinHandle<Result<()>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(mut reader) = reader else {
            return Ok(());
        };
        let mut buffer = vec![0u8; 8 * 1024];
        loop {
            let read = match reader.read(&mut buffer).await {
                Ok(read) => read,
                Err(error) => {
                    let message = error.to_string();
                    let _ = error_tx.send(message.clone());
                    return Err(error.into());
                }
            };
            if read == 0 {
                return Ok(());
            }
            if let Err(error) = output.lock().await.append(&buffer[..read]).await {
                let message = error.to_string();
                let _ = error_tx.send(message);
                return Err(error);
            }
        }
    })
}

async fn await_pipe_collector(handle: tokio::task::JoinHandle<Result<()>>) -> Result<()> {
    let mut handle = handle;
    tokio::select! {
        result = &mut handle => {
            result.map_err(|error| anyhow!("background shell output task panicked: {error}"))?
        }
        _ = tokio::time::sleep(Duration::from_secs(5)) => {
            handle.abort();
            match handle.await {
                Ok(result) => result,
                Err(error) if error.is_cancelled() => Ok(()),
                Err(error) => Err(anyhow!("background shell output task panicked: {error}")),
            }
        }
    }
}

pub(crate) struct DaemonState<M> {
    control_plane_base_url: String,
    control_plane_bind: SocketAddr,
    control_plane_auth: crate::ControlPlaneAuthConfig,
    control_plane_auth_token_files: crate::ControlPlaneAuthTokenFiles,
    control_plane_cors: crate::ControlPlaneCorsConfig,
    state_root_lock_held: bool,
    state_root: PathBuf,
    workspace_root: PathBuf,
    orchestrator: Arc<AgentOrchestrator<M>>,
    supervisor: Arc<AgentSupervisor>,
    permissions: Arc<PermissionEngine>,
    system_prompt: Arc<SystemPromptBuilder>,
    hooks: Arc<DaemonHookDispatcher>,
    debug: DebugControl,
    run_memory: RunMemoryControl,
    observer: Arc<dyn kheish_runtime::RuntimeObserver>,
    tools: Arc<ToolRuntime>,
    model_control: Option<Arc<dyn DaemonModelControl>>,
    events: DaemonEventBus,
    store: FileDaemonStore,
    session_service: SessionService,
    goal_service: GoalService,
    assets: Arc<FileAssetStore>,
    board_service: BoardService,
    channel_service: ChannelService,
    channel_turn_transition_locks:
        tokio::sync::Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
    derivation_service: DerivationService,
    learning_service: LearningService,
    learning_extraction_service: LearningExtractionService,
    learning_judge_service: LearningJudgeService,
    learning_policy_service: LearningPolicyService,
    learning_skill_service: LearningSkillService,
    semantic_capture_runs: tokio::sync::Mutex<BTreeSet<String>>,
    session_run_idempotency_inflight: tokio::sync::Mutex<BTreeSet<String>>,
    mailbox_topology_lock: tokio::sync::Mutex<()>,
    observation_service: ObservationService,
    observation_transcript_store: FileObservationTranscriptStore,
    observation_transcript_jobs:
        tokio::sync::Mutex<BTreeMap<String, ObservationTranscriptJobRecord>>,
    observation_transcript_idempotency: tokio::sync::Mutex<BTreeMap<String, String>>,
    observation_transcript_notify: tokio::sync::Notify,
    next_observation_transcript_id: AtomicU64,
    audio_generation: Option<Arc<AudioGenerationService>>,
    image_generation: Option<Arc<ImageGenerationService>>,
    transcription_service: Option<Arc<TranscriptionService>>,
    run_service: RunService,
    runtime_config_service: RuntimeConfigService,
    delivery_service: DeliveryService,
    schedule_service: ScheduleService,
    schedule_dispatch_worker_enabled: bool,
    task_service: TaskService,
    subagent_service: SubagentService,
    tool_control_state: Mutex<Option<Arc<dyn control_tools::DaemonToolControl>>>,
    mcp: Mutex<McpRuntimeSnapshot>,
    mcp_surface: Arc<RwLock<McpRuntimeSurface>>,
    mcp_manager: Option<Arc<McpManager>>,
    auth_manager: Arc<AuthManager>,
    skills: Arc<SharedSkillRegistry>,
    connectors: Arc<ConnectorRegistry>,
    external_connector_runtime: Arc<ExternalConnectorRuntimeService>,
    connector_service: Arc<ConnectorService>,
    connector_ingress_service: Arc<ConnectorIngressService>,
    subagent_policy: SubagentPolicyConfig,
    persona_service: PersonaService,
    project_service: ProjectService,
    playbook_service: PlaybookService,
    readiness: Arc<AtomicBool>,
}

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        control_plane_base_url: String,
        control_plane_bind: SocketAddr,
        control_plane_auth: crate::ControlPlaneAuthConfig,
        control_plane_auth_token_files: crate::ControlPlaneAuthTokenFiles,
        control_plane_cors: crate::ControlPlaneCorsConfig,
        state_root_lock_held: bool,
        state_root: PathBuf,
        workspace_root: PathBuf,
        orchestrator: AgentOrchestrator<M>,
        supervisor: Arc<AgentSupervisor>,
        permissions: Arc<PermissionEngine>,
        sessions: Arc<FileSessionStore>,
        system_prompt: Arc<SystemPromptBuilder>,
        hooks: Arc<DaemonHookDispatcher>,
        debug: DebugControl,
        run_memory: RunMemoryControl,
        observer: Arc<dyn kheish_runtime::RuntimeObserver>,
        tools: Arc<ToolRuntime>,
        model_control: Option<Arc<dyn DaemonModelControl>>,
        events: DaemonEventBus,
        store: FileDaemonStore,
        assets: Arc<FileAssetStore>,
        board_store: FileBoardStore,
        channel_store: FileChannelStore,
        audio_generation: Option<Arc<AudioGenerationService>>,
        image_generation: Option<Arc<ImageGenerationService>>,
        transcription_service: Option<Arc<TranscriptionService>>,
        run_store: FileRunStore,
        runtime_config_service: RuntimeConfigService,
        run_memory_store: FileRunMemoryStore,
        debug_store: FileDebugStore,
        schedule_store: FileScheduleStore,
        derivation_store: FileDerivationStore,
        learning_store: FileLearningStore,
        learning_skill_store: FileLearningSkillStore,
        observation_store: FileObservationStore,
        observation_transcript_store: FileObservationTranscriptStore,
        capture_agent_store: FileCaptureAgentStore,
        persona_store: FilePersonaStore,
        project_store: FileProjectStore,
        playbook_store: FilePlaybookStore,
        index: SessionIndex,
        persona_index: PersonaIndex,
        channel_index: crate::channels::ChannelIndex,
        output_host: Arc<OutputHost>,
        delivery_queue: Arc<crate::delivery::DeliveryQueue>,
        runs: BTreeMap<String, RunRecord>,
        session_runs: BTreeMap<String, SessionRunState>,
        pending_questions: BTreeMap<String, PendingQuestionView>,
        schedules: BTreeMap<String, ScheduleRecord>,
        boards: BTreeMap<String, BoardView>,
        board_revisions: BTreeMap<String, BoardRevisionView>,
        channels: BTreeMap<String, ChannelView>,
        channel_messages: BTreeMap<String, BTreeMap<String, ChannelMessageView>>,
        channel_leases: BTreeMap<String, BTreeMap<String, ChannelTurnLeaseView>>,
        channel_stimuli: BTreeMap<String, BTreeMap<String, ChannelStimulusView>>,
        channel_thread_states: BTreeMap<String, BTreeMap<String, ChannelThreadWorkStateView>>,
        derivations: BTreeMap<String, StoredDerivationRecord>,
        learning_candidates: BTreeMap<String, LearningCandidateView>,
        learnings: BTreeMap<String, LearningView>,
        learning_skills: BTreeMap<String, LearningSkillView>,
        observation_sources: BTreeMap<String, ObservationSourceRecord>,
        capture_agents: BTreeMap<String, CaptureAgentRecord>,
        observations: BTreeMap<String, ObservationView>,
        observation_transcript_jobs: BTreeMap<String, ObservationTranscriptJobRecord>,
        projects: BTreeMap<String, ProjectView>,
        project_tasks: BTreeMap<String, ProjectTaskView>,
        playbooks: BTreeMap<String, crate::PlaybookRecord>,
        flows: BTreeMap<String, FlowRecord>,
        mcp: McpRuntimeSnapshot,
        mcp_surface: Arc<RwLock<McpRuntimeSurface>>,
        mcp_manager: Option<Arc<McpManager>>,
        auth_manager: Arc<AuthManager>,
        skills: Arc<SharedSkillRegistry>,
        connectors: Arc<ConnectorRegistry>,
        external_connector_runtime: Arc<ExternalConnectorRuntimeService>,
        connector_service: Arc<ConnectorService>,
        connector_ingress_service: Arc<ConnectorIngressService>,
        subagent_policy: SubagentPolicyConfig,
        scheduler_enabled: bool,
        scheduler_policy: SchedulerPolicyConfig,
        next_session_id: AtomicU64,
        next_run_id: AtomicU64,
        next_schedule_id: AtomicU64,
        next_board_id: AtomicU64,
        next_board_revision_id: AtomicU64,
        next_channel_id: AtomicU64,
        next_channel_message_id: AtomicU64,
        next_channel_turn_id: AtomicU64,
        next_channel_stimulus_id: AtomicU64,
        next_derivation_id: AtomicU64,
        next_learning_candidate_id: AtomicU64,
        next_learning_id: AtomicU64,
        next_source_id: AtomicU64,
        next_observation_id: AtomicU64,
        next_observation_transcript_id: AtomicU64,
        next_project_id: AtomicU64,
        next_project_task_id: AtomicU64,
        next_flow_id: AtomicU64,
    ) -> Self {
        let run_events = events.clone();
        Self {
            control_plane_base_url,
            control_plane_bind,
            control_plane_auth,
            control_plane_auth_token_files,
            control_plane_cors,
            state_root_lock_held,
            state_root,
            workspace_root,
            orchestrator: Arc::new(orchestrator),
            supervisor,
            permissions,
            system_prompt,
            hooks: hooks.clone(),
            debug,
            run_memory,
            observer,
            tools,
            model_control,
            events: events.clone(),
            store: store.clone(),
            session_service: SessionService::new(
                sessions.clone(),
                store.clone(),
                index,
                next_session_id,
            ),
            goal_service: GoalService::new(sessions, events.clone()),
            assets: assets.clone(),
            board_service: BoardService::new(
                board_store,
                boards,
                board_revisions,
                next_board_id,
                next_board_revision_id,
            ),
            channel_service: ChannelService::new(
                channel_store,
                channel_index,
                channels,
                channel_messages,
                channel_leases,
                channel_stimuli,
                channel_thread_states,
                next_channel_id,
                next_channel_message_id,
                next_channel_turn_id,
                next_channel_stimulus_id,
            ),
            channel_turn_transition_locks: tokio::sync::Mutex::new(BTreeMap::new()),
            derivation_service: DerivationService::new(
                derivation_store,
                derivations,
                next_derivation_id,
            ),
            learning_service: LearningService::new(
                learning_store,
                learning_candidates,
                learnings,
                next_learning_candidate_id,
                next_learning_id,
            ),
            learning_extraction_service: LearningExtractionService::new(hooks.clone()),
            learning_judge_service: LearningJudgeService::new(hooks.clone()),
            learning_policy_service: LearningPolicyService::new(store.root())
                .expect("learning policy service should initialize"),
            learning_skill_service: LearningSkillService::new(
                learning_skill_store,
                learning_skills,
                skills.clone(),
            ),
            semantic_capture_runs: tokio::sync::Mutex::new(BTreeSet::new()),
            session_run_idempotency_inflight: tokio::sync::Mutex::new(BTreeSet::new()),
            mailbox_topology_lock: tokio::sync::Mutex::new(()),
            observation_service: ObservationService::new(
                observation_store,
                capture_agent_store,
                assets.clone(),
                observation_sources,
                capture_agents,
                observations,
                next_source_id,
                next_observation_id,
            ),
            observation_transcript_idempotency: tokio::sync::Mutex::new(
                observation_transcript_jobs
                    .values()
                    .map(|record| {
                        (
                            record.view.idempotency_key.clone(),
                            record.view.transcript_job_id.clone(),
                        )
                    })
                    .collect(),
            ),
            observation_transcript_store,
            observation_transcript_jobs: tokio::sync::Mutex::new(observation_transcript_jobs),
            observation_transcript_notify: tokio::sync::Notify::new(),
            next_observation_transcript_id,
            audio_generation,
            image_generation,
            transcription_service,
            run_service: RunService::new(
                run_store,
                run_memory_store,
                debug_store,
                run_events,
                runs,
                session_runs,
                pending_questions,
                next_run_id,
            ),
            runtime_config_service,
            delivery_service: DeliveryService::new(store.clone(), output_host, delivery_queue),
            schedule_service: ScheduleService::new(
                schedule_store,
                schedules,
                next_schedule_id,
                scheduler_policy,
            ),
            schedule_dispatch_worker_enabled: scheduler_enabled,
            task_service: TaskService::new(),
            subagent_service: SubagentService::new(store.root().to_path_buf()),
            tool_control_state: Mutex::new(None),
            mcp: Mutex::new(mcp),
            mcp_surface,
            mcp_manager,
            auth_manager,
            skills,
            connectors,
            external_connector_runtime,
            connector_service,
            connector_ingress_service,
            subagent_policy,
            persona_service: PersonaService::new(persona_store, persona_index),
            project_service: ProjectService::new(
                project_store,
                projects,
                project_tasks,
                next_project_id,
                next_project_task_id,
            ),
            playbook_service: PlaybookService::new(playbook_store, playbooks, flows, next_flow_id),
            readiness: Arc::new(AtomicBool::new(true)),
        }
    }

    pub(crate) fn control_plane_base_url(&self) -> &str {
        &self.control_plane_base_url
    }

    pub(crate) fn readiness(&self) -> bool {
        self.readiness.load(Ordering::SeqCst)
    }

    pub(crate) fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub(crate) fn readiness_handle(&self) -> Arc<AtomicBool> {
        self.readiness.clone()
    }

    pub(crate) fn bind_tool_control_state(&self, tool_control_state: Arc<dyn DaemonToolControl>) {
        *self.tool_control_state.lock() = Some(tool_control_state);
    }

    pub(crate) fn connectors(&self) -> &Arc<ConnectorRegistry> {
        &self.connectors
    }

    pub(crate) fn external_connector_runtime(&self) -> &Arc<ExternalConnectorRuntimeService> {
        &self.external_connector_runtime
    }

    pub(crate) async fn list_connectors(&self) -> Vec<ConnectorConfigRecord> {
        self.connector_service.list_connectors().await
    }

    pub(crate) async fn connector(&self, kind: &str, name: &str) -> Option<ConnectorConfigRecord> {
        self.connector_service.connector(kind, name).await
    }

    pub(crate) async fn put_telegram_connector(
        &self,
        config: crate::TelegramConnectorConfig,
    ) -> Result<crate::TelegramConnectorConfig> {
        let name = config.name.clone();
        self.reject_non_idle_reply_target_dependents(ConnectorKind::Telegram, &name)
            .await?;
        let applied = self
            .connector_service
            .put_telegram_connector(config)
            .await?;
        self.clear_invalid_session_reply_targets_referencing_connector(
            ConnectorKind::Telegram,
            &name,
        )
        .await?;
        Ok(applied)
    }

    pub(crate) async fn put_external_connector(
        &self,
        config: crate::ExternalConnectorConfig,
    ) -> Result<crate::ExternalConnectorConfig> {
        let name = config.name.clone();
        self.reject_non_idle_reply_target_dependents(ConnectorKind::External, &name)
            .await?;
        let applied = self
            .connector_service
            .put_external_connector(config)
            .await?;
        self.clear_invalid_session_reply_targets_referencing_connector(
            ConnectorKind::External,
            &name,
        )
        .await?;
        Ok(applied)
    }

    pub(crate) async fn put_slack_connector(
        &self,
        config: crate::SlackConnectorConfig,
    ) -> Result<crate::SlackConnectorConfig> {
        let name = config.name.clone();
        self.reject_non_idle_reply_target_dependents(ConnectorKind::Slack, &name)
            .await?;
        let applied = self.connector_service.put_slack_connector(config).await?;
        self.clear_invalid_session_reply_targets_referencing_connector(ConnectorKind::Slack, &name)
            .await?;
        Ok(applied)
    }

    pub(crate) async fn put_http_connector(
        &self,
        config: crate::HttpInputConnectorConfig,
    ) -> Result<crate::HttpInputConnectorConfig> {
        self.connector_service.put_http_connector(config).await
    }

    pub(crate) async fn put_http_connector_if_absent(
        &self,
        config: crate::HttpInputConnectorConfig,
    ) -> Result<crate::HttpInputConnectorConfig> {
        self.connector_service
            .put_http_connector_if_absent(config)
            .await
    }

    pub(crate) async fn delete_connector(&self, kind: &str, name: &str) -> Result<bool> {
        self.reject_connector_schedule_dependencies(kind, name)
            .await?;
        if let Ok(kind) = ConnectorKind::parse(kind) {
            self.reject_non_idle_reply_target_dependents(kind, name)
                .await?;
        }
        let deleted = self.connector_service.delete_connector(kind, name).await?;
        if deleted {
            let session_ids = self.session_service.cached_reply_target_session_ids().await;
            self.clear_invalid_session_reply_targets_for_sessions(session_ids)
                .await?;
        }
        Ok(deleted)
    }

    async fn reject_connector_schedule_dependencies(&self, kind: &str, name: &str) -> Result<()> {
        let referenced = self
            .schedule_service
            .schedule_records()
            .await
            .into_iter()
            .filter(|record| schedule_record_references_connector(record, kind, name))
            .map(|record| record.view.schedule_id)
            .collect::<Vec<_>>();
        if !referenced.is_empty() {
            anyhow::bail!(
                "cannot delete {kind}/{name}; it is still referenced by schedules {}",
                referenced.join(", ")
            );
        }
        Ok(())
    }

    pub(crate) async fn list_auth_statuses(&self) -> Result<Vec<AuthSlotStatus>> {
        self.auth_manager.list_statuses().await
    }

    pub(crate) fn auth_manager(&self) -> &Arc<AuthManager> {
        &self.auth_manager
    }

    pub(crate) async fn auth_status(&self, slot_id: &str) -> Result<AuthSlotStatus> {
        self.auth_manager
            .status(&kheish_auth::AuthSlotId::new(slot_id))
            .await
            .and_then(|status| status.ok_or_else(|| anyhow!("secret `{slot_id}` was not found")))
    }

    pub(crate) fn auth_subject_status(&self, subject_id: &str) -> Result<AuthSubjectStatus> {
        self.auth_manager
            .subject_status(subject_id)
            .ok_or_else(|| anyhow!("credential subject `{subject_id}` was not found"))
    }

    pub(crate) fn auth_lease_status(&self, lease_id: &str) -> Result<CredentialLeaseStatus> {
        self.auth_manager
            .lease_status(lease_id)
            .ok_or_else(|| anyhow!("credential lease `{lease_id}` was not found"))
    }

    pub(crate) async fn revoke_auth_subject(&self, subject_id: &str) -> Result<AuthSubjectStatus> {
        let active_mcp_secret_refs = self.active_mcp_lease_secret_refs_for_subject(subject_id)?;
        let active_connector_secret_refs =
            self.active_connector_lease_secret_refs_for_subject(subject_id)?;
        self.auth_manager.revoke_subject_and_leases(subject_id)?;
        self.shutdown_mcp_servers_referencing_secret_refs(&active_mcp_secret_refs, true)
            .await;
        self.disable_connectors_referencing_secret_refs(&active_connector_secret_refs)
            .await?;
        self.auth_subject_status(subject_id)
    }

    pub(crate) async fn revoke_auth_lease(&self, lease_id: &str) -> Result<CredentialLeaseStatus> {
        let status = self
            .auth_manager
            .lease_status(lease_id)
            .ok_or_else(|| anyhow!("credential lease `{lease_id}` was not found"))?;
        self.auth_manager
            .revoke_lease(lease_id, status.lease.expires_at_ms)?;
        if status.active {
            self.shutdown_mcp_servers_referencing_lease(&status.lease, false)
                .await;
            self.disable_connectors_referencing_lease(&status.lease)
                .await?;
        }
        self.auth_manager
            .lease_status(lease_id)
            .ok_or_else(|| anyhow!("credential lease `{lease_id}` was not found"))
    }

    pub(crate) async fn revoke_auth_slot_leases(&self, slot_id: &str) -> Result<usize> {
        self.auth_status(slot_id).await?;
        let revoked = self
            .auth_manager
            .revoke_slot_leases(&kheish_auth::AuthSlotId::new(slot_id))?;
        self.shutdown_mcp_servers_referencing_secret_ref(slot_id, false)
            .await;
        self.disable_connectors_referencing_revoked_secret_ref(slot_id)
            .await?;
        Ok(revoked)
    }

    pub(crate) async fn put_auth_record(&self, record: AuthSlotRecord) -> Result<AuthSlotStatus> {
        load_auth_store_master_key_from_env()?.ok_or_else(|| {
            anyhow!(
                "{AUTH_STORE_MASTER_KEY_ENV} must be set before using the daemon secret manager"
            )
        })?;
        let slot_id = record.slot_id.0.clone();
        let connector_or_mcp_secret_ref = slot_id.starts_with("connectors.")
            || slot_id.starts_with("mcp.")
            || self.connector_service.uses_secret_ref(&slot_id).await;
        if connector_or_mcp_secret_ref
            && !matches!(
                record.provider,
                AuthProvider::Generic | AuthProvider::McpOAuth
            )
        {
            anyhow::bail!(
                "connector and MCP secret slots must use generic opaque or MCP OAuth records"
            );
        }
        if record.provider == AuthProvider::McpOAuth && !slot_id.starts_with("mcp.oauth.") {
            anyhow::bail!("MCP OAuth account slots must use the `mcp.oauth.` namespace");
        }
        let status = self.auth_manager.put_record(record).await?;
        self.shutdown_mcp_servers_referencing_secret_ref(&slot_id, true)
            .await;
        self.connector_service.reload_resolved().await?;
        Ok(status)
    }

    pub(crate) async fn put_auth_record_if_absent(
        &self,
        record: AuthSlotRecord,
    ) -> Result<AuthSlotStatus> {
        load_auth_store_master_key_from_env()?.ok_or_else(|| {
            anyhow!(
                "{AUTH_STORE_MASTER_KEY_ENV} must be set before using the daemon secret manager"
            )
        })?;
        let slot_id = record.slot_id.0.clone();
        let connector_or_mcp_secret_ref = slot_id.starts_with("connectors.")
            || slot_id.starts_with("mcp.")
            || self.connector_service.uses_secret_ref(&slot_id).await;
        if connector_or_mcp_secret_ref
            && !matches!(
                record.provider,
                AuthProvider::Generic | AuthProvider::McpOAuth
            )
        {
            anyhow::bail!(
                "connector and MCP secret slots must use generic opaque or MCP OAuth records"
            );
        }
        if record.provider == AuthProvider::McpOAuth && !slot_id.starts_with("mcp.oauth.") {
            anyhow::bail!("MCP OAuth account slots must use the `mcp.oauth.` namespace");
        }
        let status = self.auth_manager.put_record_if_absent(record).await?;
        self.shutdown_mcp_servers_referencing_secret_ref(&slot_id, true)
            .await;
        self.connector_service.reload_resolved().await?;
        Ok(status)
    }

    pub(crate) async fn put_mcp_oauth_account(
        &self,
        input: kheish_auth::McpOAuthAccountRecordInput,
    ) -> Result<AuthSlotStatus> {
        load_auth_store_master_key_from_env()?.ok_or_else(|| {
            anyhow!(
                "{AUTH_STORE_MASTER_KEY_ENV} must be set before using the daemon secret manager"
            )
        })?;
        if !input.slot_id.0.starts_with("mcp.oauth.") {
            anyhow::bail!("MCP OAuth account slots must use the `mcp.oauth.` namespace");
        }
        let slot_id = input.slot_id.0.clone();
        let status = self.auth_manager.store_mcp_oauth_account(input).await?;
        self.shutdown_mcp_servers_referencing_secret_ref(&slot_id, true)
            .await;
        self.connector_service.reload_resolved().await?;
        Ok(status)
    }

    pub(crate) fn generic_secret_value(&self, secret_ref: &str) -> Result<Option<String>> {
        self.auth_manager
            .secret_value(&kheish_auth::AuthSlotId::new(secret_ref))
    }

    pub(crate) async fn refresh_auth_slot(&self, slot_id: &str) -> Result<AuthSlotStatus> {
        self.auth_manager
            .refresh_status(&kheish_auth::AuthSlotId::new(slot_id))
            .await
    }

    pub(crate) async fn put_generic_secret_without_reload(
        &self,
        secret_ref: &str,
        value: impl Into<String>,
    ) -> Result<AuthSlotStatus> {
        load_auth_store_master_key_from_env()?.ok_or_else(|| {
            anyhow!(
                "{AUTH_STORE_MASTER_KEY_ENV} must be set before using the daemon secret manager"
            )
        })?;
        self.auth_manager
            .store_generic_secret(kheish_auth::AuthSlotId::new(secret_ref), value)
            .await
    }

    pub(crate) async fn note_secret_ref_changed_without_connector_reload(
        &self,
        secret_ref: &str,
        preserve_lazy_oauth_startup: bool,
    ) {
        self.shutdown_mcp_servers_referencing_secret_ref(secret_ref, preserve_lazy_oauth_startup)
            .await;
    }

    pub(crate) async fn delete_auth_slot_without_reload(&self, slot_id: &str) -> Result<bool> {
        load_auth_store_master_key_from_env()?.ok_or_else(|| {
            anyhow!(
                "{AUTH_STORE_MASTER_KEY_ENV} must be set before using the daemon secret manager"
            )
        })?;
        self.auth_manager
            .delete(&kheish_auth::AuthSlotId::new(slot_id))
            .await
    }

    pub(crate) async fn revoke_auth_account_slot(&self, slot_id: &str) -> Result<bool> {
        load_auth_store_master_key_from_env()?.ok_or_else(|| {
            anyhow!(
                "{AUTH_STORE_MASTER_KEY_ENV} must be set before using the daemon secret manager"
            )
        })?;
        let deleted = self
            .auth_manager
            .delete(&kheish_auth::AuthSlotId::new(slot_id))
            .await?;
        if deleted {
            self.shutdown_mcp_servers_referencing_secret_ref(slot_id, false)
                .await;
            self.disable_connectors_referencing_revoked_secret_ref(slot_id)
                .await?;
        }
        Ok(deleted)
    }

    pub(crate) async fn reload_connectors(&self) -> Result<()> {
        self.connector_service.reload_resolved().await?;
        let session_ids = self.session_service.cached_reply_target_session_ids().await;
        self.clear_invalid_session_reply_targets_for_sessions(session_ids)
            .await
    }

    pub(crate) async fn delete_auth_slot(&self, slot_id: &str) -> Result<bool> {
        load_auth_store_master_key_from_env()?.ok_or_else(|| {
            anyhow!(
                "{AUTH_STORE_MASTER_KEY_ENV} must be set before using the daemon secret manager"
            )
        })?;
        if self.connector_service.uses_secret_ref(slot_id).await {
            anyhow::bail!(
                "secret `{slot_id}` is still referenced by one or more runtime connectors"
            );
        }
        let mcp_snapshot = self.mcp.lock().clone();
        if mcp_snapshot.servers.iter().any(|server| {
            server
                .credential_secret_refs
                .iter()
                .any(|secret_ref| secret_ref == slot_id)
        }) {
            anyhow::bail!("secret `{slot_id}` is still referenced by one or more MCP servers");
        }
        let deleted = self
            .auth_manager
            .delete(&kheish_auth::AuthSlotId::new(slot_id))
            .await?;
        self.connector_service.reload_resolved().await?;
        Ok(deleted)
    }

    async fn shutdown_mcp_servers_referencing_secret_ref(
        &self,
        secret_ref: &str,
        preserve_lazy_oauth_startup: bool,
    ) {
        let Some(manager) = self.mcp_manager.as_ref() else {
            return;
        };
        let changed = manager
            .shutdown_servers_referencing_secret_ref(
                secret_ref,
                preserve_lazy_oauth_startup,
                Some(&self.auth_manager),
            )
            .await;
        if changed == 0 {
            return;
        }
        let snapshot = manager.runtime_snapshot().await;
        let surface = snapshot.runtime_surface();
        *self.mcp.lock() = snapshot;
        *self.mcp_surface.write() = surface;
        info!(
            secret_ref,
            changed, "updated MCP servers after auth secret change"
        );
    }

    async fn shutdown_mcp_servers_referencing_secret_refs(
        &self,
        secret_refs: &[String],
        preserve_lazy_oauth_startup: bool,
    ) {
        let mut unique = secret_refs.to_vec();
        unique.sort();
        unique.dedup();
        for secret_ref in unique {
            self.shutdown_mcp_servers_referencing_secret_ref(
                &secret_ref,
                preserve_lazy_oauth_startup,
            )
            .await;
        }
    }

    async fn shutdown_mcp_servers_referencing_lease(
        &self,
        lease: &CredentialLease,
        preserve_lazy_oauth_startup: bool,
    ) {
        if let CredentialLeaseAudience::McpServer { slot_id, .. } = &lease.audience {
            self.shutdown_mcp_servers_referencing_secret_ref(
                &slot_id.0,
                preserve_lazy_oauth_startup,
            )
            .await;
        }
    }

    async fn disable_connectors_referencing_revoked_secret_ref(
        &self,
        secret_ref: &str,
    ) -> Result<usize> {
        self.connector_service.reload_resolved().await?;
        let referenced = self
            .connector_service
            .connectors_referencing_secret_ref(secret_ref)
            .await;
        let removed = self
            .connector_service
            .disable_secret_ref_connectors(secret_ref)
            .await?;
        if !referenced.is_empty() {
            let session_ids = self.session_service.cached_reply_target_session_ids().await;
            self.clear_invalid_session_reply_targets_for_sessions(session_ids)
                .await?;
        }
        Ok(removed)
    }

    async fn disable_connectors_referencing_secret_refs(
        &self,
        secret_refs: &[String],
    ) -> Result<()> {
        let mut unique = secret_refs.to_vec();
        unique.sort();
        unique.dedup();
        for secret_ref in unique {
            self.disable_connectors_referencing_revoked_secret_ref(&secret_ref)
                .await?;
        }
        Ok(())
    }

    async fn disable_connectors_referencing_lease(&self, lease: &CredentialLease) -> Result<()> {
        if let CredentialLeaseAudience::Connector { secret_refs, .. } = &lease.audience {
            let secret_refs = secret_refs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            self.disable_connectors_referencing_secret_refs(&secret_refs)
                .await?;
        }
        Ok(())
    }

    fn active_mcp_lease_secret_refs_for_subject(&self, subject_id: &str) -> Result<Vec<String>> {
        let status = self.auth_subject_status(subject_id)?;
        status
            .active_mcp_lease_ids
            .iter()
            .map(|lease_id| {
                let status = self.auth_lease_status(lease_id)?;
                Ok(match status.lease.audience {
                    CredentialLeaseAudience::McpServer { slot_id, .. } => Some(slot_id.to_string()),
                    _ => None,
                })
            })
            .filter_map(Result::transpose)
            .collect()
    }

    fn active_connector_lease_secret_refs_for_subject(
        &self,
        subject_id: &str,
    ) -> Result<Vec<String>> {
        let status = self.auth_subject_status(subject_id)?;
        let mut secret_refs = Vec::new();
        for lease_id in status.active_connector_lease_ids {
            let status = self.auth_lease_status(&lease_id)?;
            if let CredentialLeaseAudience::Connector {
                secret_refs: refs, ..
            } = status.lease.audience
            {
                secret_refs.extend(refs.into_iter().map(|slot_id| slot_id.to_string()));
            }
        }
        secret_refs.sort();
        secret_refs.dedup();
        Ok(secret_refs)
    }
}

pub(crate) fn completion_requirements_for_request(
    request: &SubmitInputRequest,
) -> Vec<CompletionRequirement> {
    request
        .completion_requirements
        .clone()
        .filter(|requirements| !requirements.is_empty())
        .unwrap_or_default()
}

fn schedule_record_references_connector(
    record: &crate::scheduler::ScheduleRecord,
    kind: &str,
    name: &str,
) -> bool {
    record
        .request
        .as_ref()
        .map(|request| request.reply_targets.iter())
        .into_iter()
        .flatten()
        .chain(
            record
                .observation_materialization
                .as_ref()
                .map(|request| request.request.reply_targets.iter())
                .into_iter()
                .flatten(),
        )
        .any(|target| reply_target_references_connector(target, kind, name))
}

fn reply_target_references_connector(target: &ReplyHandle, kind: &str, name: &str) -> bool {
    crate::connectors::ConnectorKind::parse(kind)
        .map(|kind| crate::connectors::reply_target_references_connector(target, kind, name))
        .unwrap_or(false)
}

pub(crate) fn merge_generation_override(
    base: Option<ModelGenerationConfig>,
    override_generation: Option<ModelGenerationConfig>,
) -> Option<ModelGenerationConfig> {
    ModelGenerationConfig::merge_override(base, override_generation)
}

fn mailbox_message_type(message: &MailboxMessage) -> &str {
    message
        .payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
}

fn render_mailbox_message(index: usize, message: &MailboxMessage) -> String {
    let payload = serde_json::to_string_pretty(&message.payload)
        .unwrap_or_else(|_| message.payload.to_string());
    format!(
        "[Mailbox Message {}]\nId: {}\nSchema-Version: {}\nState: {:?}\nAttempts: {}\nFrom: {}\nSubject: {}\nType: {}\nPayload:\n{}",
        index + 1,
        message.id,
        message.schema_version,
        message.state,
        message.delivery_attempts,
        message.from.0,
        message.subject,
        mailbox_message_type(message),
        payload
    )
}

fn render_mailbox_messages(messages: &[MailboxMessage]) -> String {
    let mut sections = vec![String::from(
        "You received mailbox messages from other agents. Treat them as new work items and act on them using the available tools when appropriate.",
    )];
    sections.extend(
        messages
            .iter()
            .enumerate()
            .map(|(index, message)| render_mailbox_message(index, message)),
    );
    sections.join("\n\n")
}
