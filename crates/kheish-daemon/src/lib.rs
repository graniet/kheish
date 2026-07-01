//! Daemon-first HTTP control plane for Kheish.

mod api;
mod assets;
mod audio_generation;
mod boards;
mod builders;
mod capture_provision;
mod channels;
mod config;
mod connectors;
mod control_tools;
mod debug;
mod delivery;
mod derivations;
mod events;
mod hooks;
mod image_generation;
mod learning;
mod memory;
mod model_routing;
mod observation_ingress;
mod observation_transcripts;
mod observations;
mod personas;
mod playbooks;
mod problems;
mod procedural_skills;
mod projects;
mod runs;
mod scheduler;
mod service;
mod services;
mod shell_tasks;
mod stack;
mod state;
mod state_files;
mod transcription;
mod web_search;

#[cfg(test)]
mod tests;

pub use api::{
    AckMailboxResponse, AgentAuditListQuery, AgentSummaryCountsView, AgentSummaryListPage,
    AgentSummaryListQuery, AgentSummaryView, AssetDeleteQuery, AssetDeletionFileView,
    AssetDeletionPlanView, AssetGcPlanView, AssetGcRequest, AssetListQuery, AssetReferenceView,
    AssetReferencesView, AssetStartupRepairDiagnosticView, AssetStartupRepairStatusView,
    AssetSummaryView, AssetView, BoardListQuery, CancelUserQuestionRequest, ChannelListQuery,
    ChannelMemberRequest, ChannelMessageListQuery, ChannelStimulusListQuery,
    ChannelThreadWorkListQuery, CheckPermissionMatrixRequest, CheckPermissionRequest,
    ConnectorSecretInput, ConnectorSecretView, ConnectorSourceView, ConnectorView,
    CreateAssetRequest, CreateBoardRequest, CreateBoardRevisionRequest, CreateChannelRequest,
    CreateChannelStimulusRequest, CreateDerivationRequest, CreateLearningCandidateRequest,
    CreateLearningSkillRequest, CreatePersonaRequest, CreateProjectRequest,
    CreateProjectTaskRequest, CreateScheduleRequest, CreateSessionRequest,
    DaemonAgentStatusSummaryView, DaemonCapabilities, DaemonControlPlaneAuthTokenFileStatusView,
    DaemonControlPlaneCorsPolicy, DaemonControlPlaneStatusView, DaemonEventStatusView,
    DaemonHealthSeverity, DaemonHealthView, DaemonHealthWarningView, DaemonProviderReadinessView,
    DaemonProviderRouteReadinessView, DaemonReadinessState, DaemonRunStatusSummaryView,
    DaemonScheduleStatusSummaryView, DaemonSessionStatusSummaryView, DaemonStateRootLockStatusView,
    DaemonStatusProbeState, DaemonStatusView, DaemonStorageProbeView, DaemonStorageStatusView,
    DaemonTaskStatusSummaryView, DeliveryBackpressureResetRequest, DeliveryBulkReplayRequest,
    DeliveryListQuery, DeliveryResolveRequest, DerivationListQuery, EndSessionRequest,
    EventStreamQuery, ExternalConnectorView, HookDeadLetterView, HookStatusView, HttpConnectorView,
    InlineAssetUpload, InputAttachmentRequest, InterruptSessionResponse,
    LearningCandidateListQuery, LearningListQuery, LearningSkillRolloutResultRequest,
    LearningSkillsListQuery, ListPage, ListPageMeta, ListPageQuery, ObservationAuditListQuery,
    ObservationTranscriptListQuery, ObservationTranscriptSegmentListQuery, PatchSessionGoalRequest,
    PendingQuestionListQuery, PendingQuestionView, PermissionMatrixModeView, PermissionMatrixView,
    PersonaListQuery, PersonaSummaryView, PersonaView, PostChannelMessageRequest,
    PostMailboxRequest, PostMailboxResponse, ProblemDetails, ProjectChannelLinkRequest,
    ProjectListQuery, ProjectMemberRequest, ProjectTaskAssignmentRequest, ProjectTaskListQuery,
    PublishLearningCandidateRequest, PutExternalConnectorRequest, PutHttpConnectorRequest,
    PutSlackConnectorRequest, PutTelegramConnectorRequest, ResolveApprovalsRequest,
    ResolveHookDeadLetterRequest, ResolveUserQuestionRequest, RevokeLearningRequest,
    RevokeLearningSkillRequest, RevokeMatchingLearningsRequest, RollbackLearningSkillRequest,
    RunListQuery, RunRetentionPruneRequest, RunRetentionPruneResponse, RuntimeConfigMetadataView,
    RuntimeConfigRevisionListResponse, RuntimeConfigRevisionView, RuntimeRollbackRequest,
    RuntimeSettingsView, RuntimeSkillsView, ScheduleListQuery, ScheduleMutationResponse,
    SessionEventLogView, SessionGoalResponse, SessionListQuery, SessionMemoryContextQuery,
    SessionMemoryContextView, SessionMemorySearchQuery, SessionMemorySearchResultKind,
    SessionMemorySearchResultView, SessionMemorySearchView, SessionPermissionAuditListView,
    SessionPersonaSummaryView, SessionReplyTargetRequest, SessionReplyTargetsView, SessionView,
    SessionViewSummary, SetAgentNicknameRequest, SetChannelReactionRequest, SetDebugLevelRequest,
    SetHooksRequest, SetLearningPolicyRequest, SetModelRequest, SetPermissionModeRequest,
    SetRunMemoryPolicyRequest, SetSessionCapabilityScopeRequest, SetSessionCredentialScopeRequest,
    SetSessionGoalRequest, SetSessionPersonaRequest, SetSessionReplyTargetsRequest,
    SetSessionRoutePolicyRequest, SetSystemPromptRequest, SetToolRuntimeLimitsRequest,
    SidechainSubtaskRequest, SkillListQuery, SkillRuntimeView, SkillSummaryView, SkillView,
    SlackConnectorView, SpawnSidechainRequest, StackApplyRequest, StackDownRequest,
    StackImportRequest, StackManifestRequest, StackPlanRequest, StartProjectTaskRequest,
    StopTaskRequest, SubmitInputItemRequest, SubmitInputRequest, SubmitRunRequest,
    SupersedeLearningRequest, TaskListQuery, TaskOutputQuery, TelegramConnectorView,
    UpdateBoardRequest, UpdateChannelRequest, UpdatePersonaRequest, UpdateProjectRequest,
    UpdateProjectTaskRequest,
};
pub use boards::{BoardRevisionView, BoardSummaryView, BoardView};
pub use builders::{
    build_anthropic_daemon, build_anthropic_daemon_with_openai_fallback, build_google_daemon,
    build_google_daemon_with_openai_fallback, build_openai_daemon,
    build_openai_daemon_with_anthropic_fallback, build_openrouter_daemon, build_provider_daemon,
    build_provider_daemon_with_extension, build_xai_daemon, build_xai_daemon_with_openai_fallback,
};
#[cfg(test)]
use builders::{daemon_model_budget, daemon_model_retry_policy, restore_supervisor};
pub use capture_provision::{
    CaptureAgentAlertView, CaptureAgentHeartbeatRequest, CaptureAgentHeartbeatResponse,
    CaptureAgentProvisionRequest, CaptureAgentProvisionResponse, CaptureAgentProvisionSources,
    CaptureAgentProvisionTarget, CaptureAgentProvisionedAgent, CaptureAgentStatus,
    CaptureAgentView, CaptureHeartbeatState, CaptureOsProfile, CaptureProvisionedSource,
    CaptureSourceLeaseView, RevokeCaptureAgentRequest,
};
pub use channels::{
    ChannelAutonomyPolicy, ChannelEvent, ChannelEventEntry, ChannelMemberDisplayNameMode,
    ChannelMemberKind, ChannelMemberView, ChannelMessageView, ChannelParticipationMode,
    ChannelProgressSnapshotView, ChannelReactionView, ChannelStimulusKind, ChannelStimulusScope,
    ChannelStimulusState, ChannelStimulusView, ChannelStimulusVisibilityHint, ChannelSummaryView,
    ChannelThreadTopicKind, ChannelThreadWorkStateView, ChannelThreadWorkStatus,
    ChannelTurnLeaseView, ChannelView, ChannelWorkBindingKind, ChannelWorkBindingView,
};
pub use config::{
    ControlPlaneAuthConfig, ControlPlaneAuthTokenFiles, ControlPlaneCorsConfig,
    DEFAULT_EVENT_HISTORY_CAPACITY, DaemonConfig, DaemonOutputRecord, DaemonOutputSourceKind,
    SubagentPolicyConfig, SubagentPolicyDecisionView, SubagentPolicyEstimateView,
    SubagentPolicyLimitPatch, SubagentPolicyLimits, SubagentPolicyRule, SubagentPolicyScopeView,
    SubagentPolicySelector, SubagentPolicyStatusView, SubagentPolicyUsageView,
    SubagentReservationStatusView, is_loopback_control_plane_origin,
};
pub use connectors::{
    ConnectorKind, ConnectorSessionPolicy, ConnectorSettings, ExternalChildProcessConfig,
    ExternalConnectorConfig, ExternalConnectorMode, ExternalReplyRoute, ExternalThreadRef,
    HttpInputConnectorConfig, SlackConnectorConfig, SlackTeamBotTokenConfig,
    TelegramConnectorConfig, TelegramIngressMode,
};
pub use debug::{DebugArtifactSummary, DebugCapturePolicyView, FileDebugStore, RunDebugView};
pub use delivery::{
    DeliveryBackpressureResetResponse, DeliveryBulkReplayAction, DeliveryBulkReplayItem,
    DeliveryBulkReplayResponse, DeliveryQueueStatusView, DeliveryReplayResponse, DeliveryStatus,
    DeliveryView,
};
pub use derivations::{
    DerivationBackendProvenance, DerivationCacheStatus, DerivationCreateControls,
    DerivationCreateRequest, DerivationProfile, DerivationStatus, DerivationSubject,
    DerivationTranscriptionOptions, DerivationView,
};
pub use events::{DaemonEvent, StreamGapReason, StreamGapScope};
pub use hooks::{
    MAX_CONFIGURED_HOOK_TIMEOUT_MS, hook_http_target_blocks_ip, redacted_hook_settings,
    validate_hook_settings,
};
pub use image_generation::AdditionalImageBackendConfig;
pub use learning::{
    LearningAutomationMode, LearningAutomationPolicyConfig, LearningAutomationReview,
    LearningCandidateOrigin, LearningCandidateState, LearningCandidateView, LearningCapturePolicy,
    LearningJudgeConfig, LearningJudgeReview, LearningPublicationAction, LearningPublicationPolicy,
    LearningPublicationRule, LearningSemanticCaptureConfig, LearningView,
    SessionMemoryMetricsSnapshot, SessionMemoryStatusView,
};
pub use memory::{
    FileRunMemoryStore, RunMemoryMaintenanceDiagnosticView, RunMemoryMaintenanceStatusView,
    RunMemoryMetricsSnapshot, RunMemoryPolicyConfig, RunMemoryRecord, RunMemorySearchVisibility,
    RunMemoryStatusView, build_run_memory_record,
};
pub(crate) use model_routing::DaemonModelControl;
pub use model_routing::{
    ConfiguredModelRoute, ModelRouteConfig, ModelSupportPolicy, ROUTE_CAPABILITY_MATRIX_VERSION,
    ResolvedModelRoute, RouteCapabilities, RouteDiagnosticSeverity, RouteDiagnosticView,
};
#[cfg(test)]
use model_routing::{DynamicModelRoute, RoutedModelControl};
pub use observation_transcripts::{
    ObservationTranscriptArtifactView, ObservationTranscriptCreateRequest,
    ObservationTranscriptJobView, ObservationTranscriptPhase, ObservationTranscriptProgress,
    ObservationTranscriptSegmentView, ObservationTranscriptSelection, ObservationTranscriptStatus,
    ObservationTranscriptTranscriptionOptions,
};
pub use observations::{
    CreateObservationRequest, CreateObservationSourceRequest, ObservationAuditRecord,
    ObservationMaterializationRequest, ObservationRawAssetPolicy, ObservationRetentionState,
    ObservationSelection, ObservationSensitivity, ObservationSourceKind, ObservationSourceStatus,
    ObservationSourceView, ObservationView, RevokeObservationSourceTokenRequest,
    RotateObservationSourceTokenRequest,
};
pub use playbooks::{
    AppendFlowEvidenceRequest, CreatePlaybookRequest, FlowContractCheck, FlowContractValidation,
    FlowEvidenceRef, FlowListQuery, FlowPhaseState, FlowPhaseStatus, FlowPrimitiveRefs, FlowStatus,
    FlowVerificationCheck, FlowView, KHEISH_FLOW_METADATA_KEY, PlaybookEvidenceRequirement,
    PlaybookInputSpec, PlaybookListQuery, PlaybookManifest, PlaybookPhase, PlaybookRecord,
    PlaybookReleaseStatus, PlaybookRole, PlaybookRuntimeDefaults, PlaybookScopePolicy,
    PlaybookToolPolicy, PlaybookValidationResult, PlaybookVersionRecord, PlaybookVersionRef,
    PlaybookVersionSummary, PlaybookView, ProductViewFlowVerificationRequest,
    ProductViewFlowVerificationVerdict, PublishPlaybookRequest, RevokePlaybookRequest,
    StartFlowRequest, ValidatePlaybookRequest,
};
pub use procedural_skills::{
    LearningSkillLifecycleEvent, LearningSkillRolloutKind, LearningSkillStatus, LearningSkillView,
};
pub use projects::{
    ProjectChannelLinkView, ProjectMemberView, ProjectStatus, ProjectSummaryView,
    ProjectTaskDiscussionRef, ProjectTaskView, ProjectView,
};
pub use runs::{
    ChannelDeliveryRunRequest, DaemonRunKind, DaemonRunStatus, FileRunStore,
    ParentClarificationRunRequest, RunEvent, RunEventEntry, RunRecord, RunRequestPayload,
    RunRequestSummary, RunView, SessionRunState, now_ms, rebuild_session_run_state,
    summarize_approval_request, summarize_channel_delivery_request, summarize_input_request,
    summarize_mailbox_request, summarize_parent_clarification_request,
    summarize_user_question_request,
};
pub use scheduler::{
    ScheduleCadence, ScheduleCreateRequest, ScheduleExecutionRecord, ScheduleExecutionStatus,
    ScheduleMisfirePolicy, ScheduleOverlapPolicy, ScheduleRecord, ScheduleStatus, ScheduleView,
    SchedulerPolicyConfig, summarize_schedule_create_request,
};
pub use service::DaemonService;
pub use services::ExternalActionAuditRecord;
pub use shell_tasks::TaskOutputView;
pub use stack::{
    STACK_MANIFEST_BODY_LIMIT_BYTES, StackAction, StackApplyReport, StackDownReport,
    StackImportReport, StackPlan, StackPlanSummary, StackValidation, StackVerificationCheck,
    StackVerificationReport, generic_stack_template, validate_stack_manifest_source,
};
pub(crate) use state::{
    ConnectorIngressReservation, DaemonOutputPlugin, DaemonOutputReceiver, DaemonState,
    DaemonToolControlAdapter, FileDaemonStore,
};
pub use transcription::AdditionalTranscriptionBackendConfig;
