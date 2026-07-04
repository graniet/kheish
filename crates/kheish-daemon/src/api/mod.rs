//! HTTP API surface for the daemon control plane.

mod auth;
mod handlers;
mod headers;
mod types;

pub(crate) use auth::ControlPlaneAuthorizer;
pub(crate) use auth::{digest_token, parse_bearer_token};
pub(crate) use handlers::build_router;
pub(crate) use headers::asset_raw_response_headers;
pub use types::{
    AddMcpServerRequest,
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
    DaemonScheduleStatusSummaryView, DaemonSessionStatusSummaryView,
    DaemonSessionStorageStatusView, DaemonStateRootLockStatusView, DaemonStatusProbeState,
    DaemonStatusView, DaemonStorageProbeView, DaemonStorageStatusView, DaemonTaskStatusSummaryView,
    DeliveryBackpressureResetRequest, DeliveryBulkReplayRequest, DeliveryListQuery,
    DeliveryResolveRequest, DerivationListQuery, EndSessionRequest, EventStreamQuery,
    ExternalConnectorView, HookDeadLetterView, HookStatusView, HttpConnectorView,
    InlineAssetUpload, InputAttachmentRequest, InterruptSessionResponse,
    LearningCandidateListQuery, LearningListQuery, LearningSkillRolloutResultRequest,
    LearningSkillsListQuery, ListPage, ListPageMeta, ListPageQuery, McpToolCallRequest,
    McpToolCallResponse, ObservationAuditListQuery, ObservationTranscriptListQuery,
    ObservationTranscriptSegmentListQuery, PatchSessionGoalRequest, PendingQuestionListQuery,
    PendingQuestionView, PermissionMatrixModeView, PermissionMatrixView, PersonaListQuery,
    PersonaSummaryView, PersonaView, PostChannelMessageRequest, PostMailboxRequest,
    PostMailboxResponse, ProblemDetails, ProjectChannelLinkRequest, ProjectListQuery,
    ProjectMemberRequest, ProjectTaskAssignmentRequest, ProjectTaskListQuery,
    PublishLearningCandidateRequest, PutExternalConnectorRequest, PutHttpConnectorRequest,
    PutSlackConnectorRequest, PutTelegramConnectorRequest, ResolveApprovalsRequest,
    ResolveHookDeadLetterRequest, ResolveUserQuestionRequest, RevokeLearningRequest,
    RevokeLearningSkillRequest, RevokeMatchingLearningsRequest, RollbackLearningSkillRequest,
    RunListQuery, RunRetentionPruneRequest, RunRetentionPruneResponse, RuntimeConfigMetadataView,
    RuntimeConfigRevisionListResponse, RuntimeConfigRevisionView, RuntimeRollbackRequest,
    RuntimeSettingsView, RuntimeSkillsView, ScheduleListQuery, ScheduleMutationResponse,
    SessionEventLogView, SessionGoalResponse, SessionListQuery, SessionMemoryContextQuery,
    SessionMemoryContextView, SessionMemorySearchQuery, SessionMemorySearchResultKind,
    SessionMemorySearchResultView, SessionMemorySearchView, SessionOperatorConfigView,
    SessionPermissionAuditListView, SessionPersonaSummaryView, SessionReplyTargetRequest,
    SessionReplyTargetsView, SessionView, SessionViewSummary, SetAgentNicknameRequest,
    SetChannelReactionRequest, SetDebugLevelRequest, SetHooksRequest, SetLearningPolicyRequest,
    SetModelRequest, SetPermissionModeRequest, SetRunMemoryPolicyRequest,
    SetSessionCapabilityScopeRequest, SetSessionCredentialScopeRequest, SetSessionGoalRequest,
    SetSessionOperatorConfigRequest, SetSessionPersonaRequest, SetSessionReplyTargetsRequest,
    SetSessionRoutePolicyRequest, SetSystemPromptRequest, SetToolRuntimeLimitsRequest,
    SidechainSubtaskRequest, SkillListQuery, SkillRuntimeView, SkillSummaryView, SkillView,
    SlackConnectorView, SpawnSidechainRequest, StackApplyRequest, StackDownRequest,
    StackImportRequest, StackManifestRequest, StackPlanRequest, StartProjectTaskRequest,
    StopTaskRequest, SubmitInputItemRequest, SubmitInputRequest, SubmitRunRequest,
    SupersedeLearningRequest, TaskListQuery, TaskOutputQuery, TelegramConnectorView,
    UpdateBoardRequest, UpdateChannelRequest, UpdatePersonaRequest, UpdateProjectRequest,
    UpdateProjectTaskRequest,
};
pub(crate) use types::{validate_input_attachment_requests, validate_submit_input_items};
