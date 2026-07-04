//! HTTP handlers and router assembly for daemon APIs.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::{DefaultBodyLimit, OriginalUri, Path as AxumPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::middleware;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};

use anyhow::{Context as _, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use kheish_agent::{AgentId, AgentSupervisorAuditEntry, MailboxMessage, ManagedAgentSnapshot};
use kheish_auth::{
    AuthSlotRecord, AuthSlotStatus, AuthSubjectStatus, CredentialLeaseStatus,
    McpOAuthAccountRecordInput,
};
use kheish_core::ModelDriver;
use kheish_types::HookSettings;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::events::sse_stream;
use crate::{DaemonState, RunDebugView, RunEventEntry, RunView, TaskOutputView};

use super::auth::{ControlPlaneAuthorizer, control_plane_auth_middleware};
use super::types::{
    AckMailboxResponse, AgentAuditListQuery, AgentSummaryCountsView, AgentSummaryListPage,
    AgentSummaryListQuery, AssetDeleteQuery, AssetDeletionPlanView, AssetGcPlanView,
    AssetGcRequest, AssetListQuery, AssetReferencesView, AssetView, BoardListQuery,
    CancelUserQuestionRequest, ChannelListQuery, ChannelMemberRequest, ChannelMessageListQuery,
    ChannelStimulusListQuery, ChannelThreadWorkListQuery, CheckPermissionMatrixRequest,
    CheckPermissionRequest, ConnectorView, CreateAssetRequest, CreateBoardRequest,
    CreateBoardRevisionRequest, CreateChannelRequest, CreateChannelStimulusRequest,
    CreateDerivationRequest, CreateLearningCandidateRequest, CreateLearningSkillRequest,
    CreatePersonaRequest, CreateProjectRequest, CreateProjectTaskRequest, CreateScheduleRequest,
    CreateSessionRequest, DaemonCapabilities, DaemonStatusView, DeliveryBackpressureResetRequest,
    DeliveryBulkReplayRequest, DeliveryListQuery, DeliveryReplayQuery, DeliveryResolveRequest,
    DerivationCreateQuery, DerivationListQuery, EndSessionRequest, EventStreamQuery,
    HookDeadLetterView, InterruptSessionResponse, LearningCandidateListQuery, LearningListQuery,
    LearningSkillRolloutResultRequest, LearningSkillsListQuery, ListPage, ListPageMeta,
    ListPageQuery, McpToolCallRequest, McpToolCallResponse, ObservationAuditListQuery,
    ObservationListQuery, ObservationSourceListQuery, ObservationTranscriptListQuery,
    ObservationTranscriptSegmentListQuery, PatchSessionGoalRequest, PendingQuestionListQuery,
    PersonaListQuery, PersonaSummaryView, PersonaView, PostChannelMessageRequest,
    PostMailboxRequest, PostMailboxResponse, ProblemDetails, ProjectChannelLinkRequest,
    ProjectListQuery, ProjectMemberRequest, ProjectTaskListQuery, PublishLearningCandidateRequest,
    PutExternalConnectorRequest, PutHttpConnectorRequest, PutSlackConnectorRequest,
    PutTelegramConnectorRequest, ResolveApprovalsRequest, ResolveHookDeadLetterRequest,
    ResolveUserQuestionRequest, RevokeLearningRequest, RevokeLearningSkillRequest,
    RevokeMatchingLearningsRequest, RollbackLearningSkillRequest, RunListQuery,
    RunRetentionPruneRequest, RunRetentionPruneResponse, RuntimeConfigRevisionListResponse,
    RuntimeRollbackRequest, RuntimeSettingsView, ScheduleListQuery, ScheduleMutationResponse,
    SessionEventLogView, SessionGoalResponse, SessionListQuery, SessionMemoryContextQuery,
    SessionMemoryContextView, SessionMemorySearchQuery, SessionMemorySearchView,
    SessionOperatorConfigView, SessionReplyTargetRequest, SessionReplyTargetsView, SessionView,
    SetAgentNicknameRequest, SetChannelReactionRequest, SetDebugLevelRequest, SetHooksRequest,
    SetLearningPolicyRequest, SetModelRequest, SetPermissionModeRequest, SetRunMemoryPolicyRequest,
    SetSessionCapabilityScopeRequest, SetSessionCredentialScopeRequest, SetSessionGoalRequest,
    SetSessionOperatorConfigRequest, SetSessionPersonaRequest, SetSessionReplyTargetsRequest,
    SetSessionRoutePolicyRequest, SetSystemPromptRequest, SetToolRuntimeLimitsRequest,
    SkillListQuery, SkillSummaryView, SkillView, SpawnSidechainRequest, StackApplyRequest,
    StackDownRequest, StackImportRequest, StackManifestRequest, StackPlanRequest,
    StartProjectTaskRequest, StopTaskRequest, SubmitInputRequest, SubmitRunRequest,
    SupersedeLearningRequest, TaskListQuery, TaskOutputQuery, UpdateBoardRequest,
    UpdateChannelRequest, UpdatePersonaRequest, UpdateProjectRequest, UpdateProjectTaskRequest,
};
use crate::assets::MAX_ASSET_BYTES;
use crate::problems::DaemonProblem;
use crate::services::ConnectorConfigRecord;
use crate::{
    AppendFlowEvidenceRequest, BoardRevisionView, BoardView, CaptureAgentAlertView,
    CaptureAgentProvisionRequest, CaptureAgentProvisionResponse, CaptureAgentView,
    ChannelMessageView, ChannelStimulusView, ChannelThreadWorkStateView, ChannelTurnLeaseView,
    ChannelView, CreateObservationSourceRequest, CreatePlaybookRequest, DerivationView,
    FlowListQuery, LearningCandidateView, LearningSkillView, LearningView, ObservationAuditRecord,
    ObservationMaterializationRequest, ObservationSourceView, ObservationTranscriptCreateRequest,
    ObservationView, PlaybookListQuery, PlaybookValidationResult, PlaybookView,
    ProjectChannelLinkView, ProjectMemberView, ProjectTaskView, ProjectView,
    PublishPlaybookRequest, RevokeCaptureAgentRequest, RevokeObservationSourceTokenRequest,
    RevokePlaybookRequest, RotateObservationSourceTokenRequest, StartFlowRequest,
    ValidatePlaybookRequest,
};

// Inline assets are sent as base64 inside JSON on the control-plane API. Keep
// the HTTP body budget above the raw asset limit so valid images are not
// rejected before they reach asset validation.
const CONTROL_PLANE_JSON_BODY_LIMIT_BYTES: usize = MAX_ASSET_BYTES * 2;
const STACK_CONTROL_PLANE_JSON_BODY_LIMIT_BYTES: usize = 1024 * 1024;
const DEFAULT_LIST_PAGE_LIMIT: usize = 50;
const MAX_LIST_PAGE_LIMIT: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    domain: Option<&'static str>,
    detail: String,
}

impl ApiError {
    fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self {
            status,
            code: status_code_problem_code(status),
            domain: None,
            detail: detail.into(),
        }
    }

    fn coded(
        status: StatusCode,
        domain: &'static str,
        code: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            status,
            code,
            domain: Some(domain),
            detail: detail.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut problem = ProblemDetails::new(self.status.as_u16(), self.code, self.detail);
        if let Some(domain) = self.domain {
            problem = problem.with_domain(domain);
        }
        (
            self.status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            Json(problem),
        )
            .into_response()
    }
}

fn status_code_problem_code(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "bad_request",
        StatusCode::UNAUTHORIZED => "unauthorized",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
        StatusCode::CONFLICT => "conflict",
        StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
        StatusCode::TOO_MANY_REQUESTS => "rate_limited",
        StatusCode::SERVICE_UNAVAILABLE => "service_unavailable",
        StatusCode::INTERNAL_SERVER_ERROR => "internal_error",
        _ => "daemon_error",
    }
}

fn list_or_page<T, F>(
    mut items: Vec<T>,
    page: &ListPageQuery,
    limit: Option<usize>,
    order: &str,
    key: F,
) -> Result<Json<Value>, ApiError>
where
    T: Serialize,
    F: Fn(&T) -> String,
{
    if !page.enabled() {
        if let Some(limit) = limit {
            let limit = normalize_page_limit(Some(limit))?;
            items.sort_by_key(key);
            items.truncate(limit);
        }
        return json_value(items);
    }
    json_value(paginate_items(items, page, limit, order, key)?)
}

fn paginate_items<T, F>(
    mut items: Vec<T>,
    page: &ListPageQuery,
    limit: Option<usize>,
    order: &str,
    key: F,
) -> Result<ListPage<T>, ApiError>
where
    F: Fn(&T) -> String,
{
    let limit = normalize_page_limit(limit)?;
    let cursor_key = page.cursor.as_deref().map(decode_page_cursor).transpose()?;

    items.sort_by_key(|item| key(item));
    let total_count = items.len();
    let mut filtered = items
        .into_iter()
        .filter(|item| cursor_key.as_ref().is_none_or(|cursor| key(item) > *cursor))
        .take(limit + 1)
        .collect::<Vec<_>>();
    let has_more = filtered.len() > limit;
    if has_more {
        filtered.truncate(limit);
    }
    let next_cursor = if has_more {
        filtered.last().map(|item| encode_page_cursor(&key(item)))
    } else {
        None
    };
    Ok(ListPage {
        items: filtered,
        pagination: ListPageMeta {
            limit,
            total_count,
            has_more,
            next_cursor,
            order: order.to_string(),
        },
    })
}

fn normalize_page_limit(limit: Option<usize>) -> Result<usize, ApiError> {
    let limit = limit.unwrap_or(DEFAULT_LIST_PAGE_LIMIT);
    if limit == 0 {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "pagination",
            "invalid_limit",
            "pagination limit must be greater than zero",
        ));
    }
    Ok(limit.min(MAX_LIST_PAGE_LIMIT))
}

fn encode_page_cursor(key: &str) -> String {
    URL_SAFE_NO_PAD.encode(key.as_bytes())
}

fn decode_page_cursor(cursor: &str) -> Result<String, ApiError> {
    let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| {
        ApiError::coded(
            StatusCode::BAD_REQUEST,
            "pagination",
            "invalid_cursor",
            "pagination cursor is not valid",
        )
    })?;
    String::from_utf8(bytes).map_err(|_| {
        ApiError::coded(
            StatusCode::BAD_REQUEST,
            "pagination",
            "invalid_cursor",
            "pagination cursor is not valid UTF-8",
        )
    })
}

fn event_stream_cursor(
    headers: &HeaderMap,
    query: &EventStreamQuery,
) -> Result<Option<u64>, ApiError> {
    let header_cursor = match headers.get("last-event-id") {
        Some(header_value) => {
            let value = header_value.to_str().map_err(|_| {
                ApiError::coded(
                    StatusCode::BAD_REQUEST,
                    "events",
                    "invalid_event_cursor",
                    "Last-Event-ID must be valid ASCII",
                )
            })?;
            if value.trim().is_empty() {
                None
            } else {
                Some(value.parse::<u64>().map_err(|_| {
                    ApiError::coded(
                        StatusCode::BAD_REQUEST,
                        "events",
                        "invalid_event_cursor",
                        "Last-Event-ID must be a numeric daemon event id",
                    )
                })?)
            }
        }
        None => None,
    };
    let query_cursor = query
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                ApiError::coded(
                    StatusCode::BAD_REQUEST,
                    "events",
                    "invalid_event_cursor",
                    "cursor must be a numeric daemon event id string",
                )
            })
        })
        .transpose()?;
    Ok(query_cursor.max(header_cursor))
}

fn json_value<T: Serialize>(value: T) -> Result<Json<Value>, ApiError> {
    serde_json::to_value(value)
        .map(Json)
        .map_err(|error| internal_error(error.into()))
}

fn run_page_key(run: &RunView) -> String {
    format!("{:020}:{}", run.submitted_at_ms, run.run_id)
}

fn delivery_page_key(delivery: &crate::DeliveryView) -> String {
    format!(
        "{:020}:{}",
        delivery_sort_millis(delivery),
        delivery.delivery_id
    )
}

fn delivery_sort_millis(delivery: &crate::DeliveryView) -> u64 {
    delivery
        .dead_lettered_at_ms
        .or(delivery.delivered_at_ms)
        .or(delivery.next_attempt_at_ms)
        .unwrap_or_default()
}

pub(crate) fn build_router<M>(
    state: Arc<DaemonState<M>>,
    authorizer: Arc<ControlPlaneAuthorizer>,
) -> Router
where
    M: ModelDriver + Send + Sync + 'static,
{
    let probe_state = state.clone();
    let stack_routes = Router::new()
        .route("/v1/stacks", get(list_stacks::<M>))
        .route("/v1/stacks/validate", post(validate_stack::<M>))
        .route("/v1/stacks/plan", post(plan_stack::<M>))
        .route("/v1/stacks/apply", post(apply_stack::<M>))
        .route("/v1/stacks/verify", post(verify_stack::<M>))
        .route("/v1/stacks/import", post(import_stack::<M>))
        .route("/v1/stacks/down", post(down_stack::<M>))
        .route(
            "/v1/stacks/{ownership_id}/ledger",
            get(get_stack_ledger::<M>),
        )
        .layer(DefaultBodyLimit::max(
            STACK_CONTROL_PLANE_JSON_BODY_LIMIT_BYTES,
        ));
    let control_plane = Router::new()
        .route("/v1/status", get(status::<M>))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/openapi.json", get(openapi))
        .route("/v1/runtime", get(get_runtime::<M>))
        .route(
            "/v1/runtime/mcp/tools/{tool_name}/call",
            post(call_runtime_mcp_tool::<M>),
        )
        .route(
            "/v1/runtime/subagent-policy/quotas",
            get(get_subagent_policy_quotas::<M>),
        )
        .route(
            "/v1/runtime/learning-policy",
            get(get_learning_policy::<M>).post(set_learning_policy::<M>),
        )
        .route(
            "/v1/runtime/run-memory-policy",
            get(get_run_memory_policy::<M>).post(set_run_memory_policy::<M>),
        )
        .route("/v1/runtime/connectors", get(list_runtime_connectors::<M>))
        .route(
            "/v1/runtime/connectors/external/metrics",
            get(get_external_connector_metrics::<M>),
        )
        .route(
            "/v1/runtime/deliveries/metrics",
            get(get_delivery_queue_metrics::<M>),
        )
        .route(
            "/v1/runtime/connectors/{kind}/{name}",
            get(get_runtime_connector::<M>)
                .put(put_runtime_connector::<M>)
                .delete(delete_runtime_connector::<M>),
        )
        .route(
            "/v1/runtime/secrets",
            get(list_runtime_secrets::<M>).post(put_runtime_secret::<M>),
        )
        .route(
            "/v1/runtime/secrets/{secret_ref}",
            get(get_runtime_secret::<M>).delete(delete_runtime_secret::<M>),
        )
        .route(
            "/v1/runtime/auth/subjects/{subject_id}",
            get(get_runtime_auth_subject::<M>),
        )
        .route(
            "/v1/runtime/auth/subjects/{subject_id}/revoke",
            post(revoke_runtime_auth_subject::<M>),
        )
        .route(
            "/v1/runtime/auth/leases/{lease_id}",
            get(get_runtime_auth_lease::<M>),
        )
        .route(
            "/v1/runtime/auth/leases/{lease_id}/revoke",
            post(revoke_runtime_auth_lease::<M>),
        )
        .route(
            "/v1/runtime/auth/slots/{slot_id}/revoke",
            post(revoke_runtime_auth_slot::<M>),
        )
        .route(
            "/v1/runtime/auth/accounts",
            get(list_runtime_auth_accounts::<M>).post(put_runtime_mcp_oauth_account::<M>),
        )
        .route(
            "/v1/runtime/auth/accounts/mcp-oauth",
            post(put_runtime_mcp_oauth_account::<M>),
        )
        .route(
            "/v1/runtime/auth/accounts/{slot_id}",
            get(get_runtime_auth_account::<M>).delete(revoke_runtime_auth_account::<M>),
        )
        .route(
            "/v1/runtime/auth/accounts/{slot_id}/refresh",
            post(refresh_runtime_auth_account::<M>),
        )
        .route(
            "/v1/runtime/auth/accounts/{slot_id}/revoke",
            post(revoke_runtime_auth_account::<M>),
        )
        .route("/v1/assets", get(list_assets::<M>).post(import_asset::<M>))
        .route("/v1/assets/gc", post(gc_assets::<M>))
        .route(
            "/v1/assets/{asset_id}",
            get(get_asset::<M>).delete(delete_asset::<M>),
        )
        .route(
            "/v1/assets/{asset_id}/references",
            get(get_asset_references::<M>),
        )
        .route("/v1/assets/{asset_id}/raw", get(get_asset_raw::<M>))
        .route("/v1/boards", get(list_boards::<M>).post(create_board::<M>))
        .route(
            "/v1/boards/{board_id}",
            get(get_board::<M>).put(update_board::<M>),
        )
        .route(
            "/v1/boards/{board_id}/revisions",
            get(list_board_revisions::<M>).post(create_board_revision::<M>),
        )
        .route(
            "/v1/boards/{board_id}/revisions/{revision_id}",
            get(get_board_revision::<M>),
        )
        .route(
            "/v1/channels",
            get(list_channels::<M>).post(create_channel::<M>),
        )
        .route(
            "/v1/channels/{channel_id}",
            get(get_channel::<M>)
                .put(update_channel::<M>)
                .delete(delete_channel::<M>),
        )
        .route(
            "/v1/channels/{channel_id}/members",
            get(list_channel_members::<M>).post(upsert_channel_member::<M>),
        )
        .route(
            "/v1/channels/{channel_id}/members/{member_id}",
            delete(remove_channel_member::<M>),
        )
        .route(
            "/v1/channels/{channel_id}/messages",
            get(list_channel_messages::<M>).post(post_channel_message::<M>),
        )
        .route(
            "/v1/channels/{channel_id}/messages/{message_id}/reactions",
            post(set_channel_reaction::<M>).delete(unset_channel_reaction::<M>),
        )
        .route(
            "/v1/channels/{channel_id}/stimuli",
            get(list_channel_stimuli::<M>).post(create_channel_stimulus::<M>),
        )
        .route(
            "/v1/channels/{channel_id}/thread-work",
            get(list_channel_thread_work::<M>),
        )
        .route(
            "/v1/channels/{channel_id}/leases",
            get(list_channel_leases::<M>),
        )
        .route(
            "/v1/projects",
            get(list_projects::<M>).post(create_project::<M>),
        )
        .route(
            "/v1/projects/{project_id}",
            get(get_project::<M>)
                .put(update_project::<M>)
                .delete(delete_project::<M>),
        )
        .route(
            "/v1/projects/{project_id}/members",
            get(list_project_members::<M>).post(upsert_project_member::<M>),
        )
        .route(
            "/v1/projects/{project_id}/members/{member_id}",
            delete(remove_project_member::<M>),
        )
        .route(
            "/v1/projects/{project_id}/channels",
            get(list_project_channels::<M>).post(upsert_project_channel::<M>),
        )
        .route(
            "/v1/projects/{project_id}/channels/{channel_id}",
            delete(remove_project_channel::<M>),
        )
        .route(
            "/v1/projects/{project_id}/tasks",
            get(list_project_tasks::<M>).post(create_project_task::<M>),
        )
        .route(
            "/v1/projects/{project_id}/tasks/{task_id}",
            get(get_project_task::<M>)
                .put(update_project_task::<M>)
                .delete(delete_project_task::<M>),
        )
        .route(
            "/v1/projects/{project_id}/tasks/{task_id}/start",
            post(start_project_task::<M>),
        )
        .merge(stack_routes)
        .route(
            "/v1/playbooks",
            get(list_playbooks::<M>).post(create_playbook::<M>),
        )
        .route("/v1/playbooks/validate", post(validate_playbook::<M>))
        .route("/v1/playbooks/{playbook_id}", get(get_playbook::<M>))
        .route(
            "/v1/playbooks/{playbook_id}/publish",
            post(publish_playbook::<M>),
        )
        .route(
            "/v1/playbooks/{playbook_id}/revoke",
            post(revoke_playbook::<M>),
        )
        .route("/v1/flows", get(list_flows::<M>).post(start_flow::<M>))
        .route("/v1/flows/{flow_id}", get(get_flow::<M>))
        .route("/v1/flows/{flow_id}/cancel", post(cancel_flow::<M>))
        .route(
            "/v1/flows/{flow_id}/evidence",
            post(append_flow_evidence::<M>),
        )
        .route(
            "/v1/flows/{flow_id}/verify/product-view",
            post(verify_product_view_flow::<M>),
        )
        .route("/v1/flows/{flow_id}/stream", get(stream_flow_events::<M>))
        .route(
            "/v1/observation-sources",
            get(list_observation_sources::<M>).post(create_observation_source::<M>),
        )
        .route(
            "/v1/observation-sources/{source_id}",
            get(get_observation_source::<M>),
        )
        .route(
            "/v1/observation-sources/{source_id}/rotate-token",
            post(rotate_observation_source_token::<M>),
        )
        .route(
            "/v1/observation-sources/{source_id}/revoke-token",
            post(revoke_observation_source_token::<M>),
        )
        .route("/v1/observation-audit", get(list_observation_audit::<M>))
        .route("/v1/observations", get(list_observations::<M>))
        .route(
            "/v1/observations/{observation_id}",
            get(get_observation::<M>),
        )
        .route(
            "/v1/observation-materializations",
            post(create_observation_materialization::<M>),
        )
        .route(
            "/v1/observation-transcripts",
            get(list_observation_transcripts::<M>).post(create_observation_transcript::<M>),
        )
        .route(
            "/v1/observation-transcripts/{transcript_job_id}",
            get(get_observation_transcript::<M>),
        )
        .route(
            "/v1/observation-transcripts/{transcript_job_id}/segments",
            get(list_observation_transcript_segments::<M>),
        )
        .route(
            "/v1/observation-transcripts/{transcript_job_id}/retry",
            post(retry_observation_transcript::<M>),
        )
        .route(
            "/v1/observation-transcripts/{transcript_job_id}/cancel",
            post(cancel_observation_transcript::<M>),
        )
        .route(
            "/v1/capture-agent-provisions",
            post(provision_capture_agents::<M>),
        )
        .route("/v1/capture-agents", get(list_capture_agents::<M>))
        .route("/v1/capture-alerts", get(list_capture_alerts::<M>))
        .route(
            "/v1/capture-agents/{machine_id}",
            get(get_capture_agent::<M>),
        )
        .route(
            "/v1/capture-agents/{machine_id}/revoke",
            post(revoke_capture_agent::<M>),
        )
        .route(
            "/v1/derivations",
            get(list_derivations::<M>).post(create_derivation::<M>),
        )
        .route("/v1/derivations/{derivation_id}", get(get_derivation::<M>))
        .route(
            "/v1/learning-candidates",
            get(list_learning_candidates::<M>).post(create_learning_candidate::<M>),
        )
        .route(
            "/v1/learning-candidates/{candidate_id}",
            get(get_learning_candidate::<M>),
        )
        .route(
            "/v1/learning-candidates/{candidate_id}/publish",
            post(publish_learning_candidate::<M>),
        )
        .route(
            "/v1/learning-candidates/{candidate_id}/reject",
            post(reject_learning_candidate::<M>),
        )
        .route("/v1/learnings", get(list_learnings::<M>))
        .route(
            "/v1/learnings/revoke-matching",
            post(revoke_matching_learnings::<M>),
        )
        .route("/v1/learnings/{learning_id}", get(get_learning::<M>))
        .route(
            "/v1/learnings/{learning_id}/revoke",
            post(revoke_learning::<M>),
        )
        .route(
            "/v1/learnings/{learning_id}/supersede",
            post(supersede_learning::<M>),
        )
        .route(
            "/v1/learnings/{learning_id}/promote-skill",
            post(promote_learning_to_skill::<M>),
        )
        .route("/v1/learning-skills", get(list_learning_skills::<M>))
        .route(
            "/v1/learning-skills/{skill_name}",
            get(get_learning_skill::<M>),
        )
        .route(
            "/v1/learning-skills/{skill_name}/rollout-result",
            post(record_learning_skill_rollout_result::<M>),
        )
        .route(
            "/v1/learning-skills/{skill_name}/revoke",
            post(revoke_learning_skill::<M>),
        )
        .route(
            "/v1/learning-skills/{skill_name}/rollback",
            post(rollback_learning_skill::<M>),
        )
        .route("/v1/skills", get(list_skills::<M>))
        .route("/v1/skills/{skill_name}", get(get_skill::<M>))
        .route(
            "/v1/personas",
            get(list_personas::<M>).post(create_persona::<M>),
        )
        .route(
            "/v1/personas/{persona_id}",
            get(get_persona::<M>).put(update_persona::<M>),
        )
        .route("/v1/runtime/model", post(set_model::<M>))
        .route(
            "/v1/runtime/revisions",
            get(list_runtime_config_revisions::<M>),
        )
        .route("/v1/runtime/rollback", post(rollback_runtime_config::<M>))
        .route("/v1/runtime/system-prompt", post(set_system_prompt::<M>))
        .route(
            "/v1/runtime/hooks",
            get(get_hooks::<M>).post(set_hooks::<M>),
        )
        .route(
            "/v1/runtime/hooks/dead-letter",
            get(list_hook_dead_letters::<M>),
        )
        .route(
            "/v1/runtime/hooks/dead-letter/{dead_letter_id}/resolve",
            post(resolve_hook_dead_letter::<M>),
        )
        .route(
            "/v1/runtime/tool-limits",
            get(get_tool_runtime_limits::<M>).post(set_tool_runtime_limits::<M>),
        )
        .route("/v1/runtime/debug-level", post(set_debug_level::<M>))
        .route(
            "/v1/runtime/permission-mode",
            post(set_permission_mode::<M>),
        )
        .route("/v1/runtime/permissions/check", post(check_permission::<M>))
        .route(
            "/v1/runtime/permissions/matrix",
            post(check_permission_matrix::<M>),
        )
        .route("/v1/events/stream", get(stream_all_events::<M>))
        .route(
            "/v1/sessions",
            post(create_session::<M>).get(list_sessions::<M>),
        )
        .route("/v1/sessions/{session_id}", get(get_session::<M>))
        .route(
            "/v1/sessions/{session_id}/permission-audits",
            get(get_session_permission_audits::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/goal",
            get(get_session_goal::<M>)
                .post(create_session_goal::<M>)
                .put(set_session_goal::<M>)
                .patch(patch_session_goal::<M>)
                .delete(clear_session_goal::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/memory-context",
            get(get_session_memory_context::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/memory-search",
            get(get_session_memory_search::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/skills",
            get(list_session_skills::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/persona",
            post(set_session_persona::<M>)
                .put(replace_session_persona::<M>)
                .delete(clear_session_persona::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/route-policy",
            post(set_session_route_policy::<M>)
                .put(replace_session_route_policy::<M>)
                .delete(clear_session_route_policy::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/operator",
            get(get_session_operator_config::<M>)
                .post(set_session_operator_config::<M>)
                .put(replace_session_operator_config::<M>)
                .delete(clear_session_operator_config::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/capability-scope",
            post(set_session_capability_scope::<M>)
                .put(replace_session_capability_scope::<M>)
                .delete(clear_session_capability_scope::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/credential-scope",
            post(set_session_credential_scope::<M>)
                .put(replace_session_credential_scope::<M>)
                .delete(clear_session_credential_scope::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/reply-targets",
            get(get_session_reply_targets::<M>)
                .post(set_session_reply_targets::<M>)
                .put(replace_session_reply_targets::<M>)
                .delete(clear_session_reply_targets::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/events",
            get(get_session_events::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/stream",
            get(stream_session_events::<M>),
        )
        .route("/v1/sessions/{session_id}/input", post(submit_input::<M>))
        .route("/v1/sessions/{session_id}/tasks", get(list_tasks::<M>))
        .route(
            "/v1/sessions/{session_id}/tasks/{task_id}",
            get(get_task::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/tasks/{task_id}/output",
            get(get_task_output::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/tasks/{task_id}/stop",
            post(stop_task::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/runs",
            post(submit_input_run::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/approvals",
            post(resolve_approvals::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/questions",
            get(get_session_questions::<M>).post(resolve_user_question::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/approval-runs",
            post(resolve_approval_run::<M>),
        )
        .route(
            "/v1/sessions/{session_id}/interrupt",
            post(interrupt_session::<M>),
        )
        .route("/v1/sessions/{session_id}/end", post(end_session::<M>))
        .route("/v1/runs", get(list_runs::<M>))
        .route("/v1/runs/prune", post(prune_runs::<M>))
        .route("/v1/runs/{run_id}", get(get_run::<M>))
        .route("/v1/runs/{run_id}/debug", get(get_run_debug::<M>))
        .route(
            "/v1/runs/{run_id}/debug/artifacts/{artifact_id}",
            get(get_run_debug_artifact::<M>),
        )
        .route(
            "/v1/runs/{run_id}/approvals",
            post(resolve_run_approvals::<M>),
        )
        .route(
            "/v1/runs/{run_id}/questions",
            post(resolve_run_user_question::<M>),
        )
        .route(
            "/v1/runs/{run_id}/questions/{request_id}/cancel",
            post(cancel_run_question::<M>),
        )
        .route("/v1/runs/{run_id}/events", get(get_run_events::<M>))
        .route(
            "/v1/runs/{run_id}/external-actions",
            get(get_run_external_actions::<M>),
        )
        .route("/v1/runs/{run_id}/stream", get(stream_run_events::<M>))
        .route("/v1/runs/{run_id}/cancel", post(cancel_run::<M>))
        .route("/v1/deliveries", get(list_deliveries::<M>))
        .route(
            "/v1/deliveries/dead-letter",
            get(list_dead_letter_deliveries::<M>),
        )
        .route(
            "/v1/deliveries/backpressure/reset",
            post(reset_delivery_backpressure::<M>),
        )
        .route("/v1/deliveries/{delivery_id}", get(get_delivery::<M>))
        .route(
            "/v1/deliveries/{delivery_id}/replay",
            post(replay_delivery::<M>),
        )
        .route(
            "/v1/deliveries/replay-bulk",
            post(bulk_replay_deliveries::<M>),
        )
        .route(
            "/v1/deliveries/{delivery_id}/resolve",
            post(resolve_delivery::<M>),
        )
        .route("/v1/questions", get(list_questions::<M>))
        .route(
            "/v1/schedules",
            get(list_schedules::<M>).post(create_schedule::<M>),
        )
        .route("/v1/schedules/{schedule_id}", get(get_schedule::<M>))
        .route(
            "/v1/schedules/{schedule_id}/cancel",
            post(cancel_schedule::<M>),
        )
        .route(
            "/v1/schedules/{schedule_id}/pause",
            post(pause_schedule::<M>),
        )
        .route(
            "/v1/schedules/{schedule_id}/resume",
            post(resume_schedule::<M>),
        )
        .route(
            "/v1/schedules/{schedule_id}/trigger",
            post(trigger_schedule::<M>),
        )
        .route("/v1/agents", get(list_agents::<M>))
        .route("/v1/agents/audit", get(list_agent_audit::<M>))
        .route("/v1/agents/summaries", get(list_agent_summaries::<M>))
        .route("/v1/agents/{agent_id}", get(get_agent::<M>))
        .route("/v1/agents/{agent_id}/audit", get(get_agent_audit::<M>))
        .route(
            "/v1/agents/{agent_id}/nickname",
            post(set_agent_nickname::<M>)
                .put(replace_agent_nickname::<M>)
                .delete(clear_agent_nickname::<M>),
        )
        .route(
            "/v1/agents/{agent_id}/sidechains",
            post(spawn_sidechain::<M>),
        )
        .route(
            "/v1/agents/{agent_id}/sidechains/explain",
            post(explain_sidechain_spawn::<M>),
        )
        .route("/v1/agents/{agent_id}/mailbox", get(drain_mailbox::<M>))
        .route(
            "/v1/agents/{agent_id}/mailbox/dead-letter",
            get(get_mailbox_dead_letters::<M>),
        )
        .route(
            "/v1/agents/{agent_id}/mailbox/{message_id}/ack",
            post(ack_mailbox_message::<M>),
        )
        .route("/v1/mailboxes", post(post_mailbox::<M>))
        .fallback(control_plane_not_found)
        .layer(DefaultBodyLimit::max(CONTROL_PLANE_JSON_BODY_LIMIT_BYTES))
        .layer(middleware::from_fn_with_state(
            authorizer,
            control_plane_auth_middleware,
        ))
        .layer(middleware::from_fn(
            control_plane_problem_details_middleware,
        ))
        .with_state(state);
    let probes = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz::<M>))
        .layer(middleware::from_fn(
            control_plane_problem_details_middleware,
        ))
        .with_state(probe_state);
    Router::new().merge(probes).merge(control_plane)
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

async fn readyz<M>(State(state): State<Arc<DaemonState<M>>>) -> impl IntoResponse
where
    M: ModelDriver + Send + Sync + 'static,
{
    if state.readiness() {
        (StatusCode::OK, "ready\n").into_response()
    } else {
        ApiError::coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "readiness",
            "daemon_draining",
            "daemon is draining",
        )
        .into_response()
    }
}

async fn control_plane_not_found(OriginalUri(uri): OriginalUri) -> ApiError {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        "api",
        "route_not_found",
        format!("unknown daemon API route {}", uri.path()),
    )
}

async fn control_plane_problem_details_middleware(
    request: Request<Body>,
    next: middleware::Next,
) -> Response {
    let response = next.run(request).await;
    if response.status().is_success() || is_problem_response(&response) {
        return response;
    }

    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body();
    let detail = match to_bytes(body, 64 * 1024).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).trim().to_string(),
        Err(error) => format!("failed to read daemon error body: {error}"),
    };
    let detail = if detail.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("daemon request failed")
            .to_string()
    } else {
        detail
    };

    let mut response = ApiError::new(status, detail).into_response();
    let response_headers = response.headers_mut();
    for (name, value) in &headers {
        if name != header::CONTENT_TYPE && name != header::CONTENT_LENGTH {
            response_headers.append(name, value.clone());
        }
    }
    response
}

fn is_problem_response(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/problem+json"))
}

fn daemon_capabilities() -> DaemonCapabilities {
    DaemonCapabilities {
        control_plane_version: env!("CARGO_PKG_VERSION").to_string(),
        api_revision: 3,
        route_capability_matrix_version: crate::ROUTE_CAPABILITY_MATRIX_VERSION,
        approvals: true,
        sidechains: true,
        mailboxes: true,
        session_events: true,
        restart_restore: true,
        live_events: true,
        session_run_idempotency: true,
        playbooks: true,
        flows: true,
        problem_details: true,
        openapi: true,
        cursor_pagination: true,
        paginated_lists: true,
        domain_errors: true,
        sse_replay: true,
        typed_sse_heartbeat: true,
        agent_supervisor_audit: true,
        spawn_policies: true,
    }
}

async fn capabilities() -> Json<DaemonCapabilities> {
    Json(daemon_capabilities())
}

async fn openapi() -> Json<Value> {
    Json(openapi_spec())
}

fn openapi_spec() -> Value {
    let mut paths = serde_json::Map::new();
    for spec in CONTROL_PLANE_OPENAPI_ROUTES {
        let mut methods = serde_json::Map::new();
        for method in spec.methods {
            methods.insert(
                method.to_ascii_lowercase(),
                serde_json::json!({
                    "operationId": operation_id(method, spec.path),
                    "responses": openapi_base_responses(),
                    "security": openapi_operation_security(spec.path, method)
                }),
            );
        }
        paths.insert(spec.path.to_string(), Value::Object(methods));
    }
    attach_path_parameters(&mut paths);
    attach_common_pagination_parameters(&mut paths);
    attach_list_filter_parameters(&mut paths);
    attach_sse_parameters(&mut paths);
    if let Some(Value::Object(path)) = paths.get_mut("/v1/agents/summaries")
        && let Some(Value::Object(get)) = path.get_mut("get")
    {
        get.insert(
            "parameters".to_string(),
            serde_json::json!([
                {
                    "name": "root_agent_id",
                    "in": "query",
                    "required": false,
                    "schema": { "type": "string" },
                    "description": "Restricts summaries to one root agent tree."
                },
                {
                    "name": "session_id",
                    "in": "query",
                    "required": false,
                    "schema": { "type": "string" },
                    "description": "Restricts summaries to one session."
                },
                {
                    "name": "status",
                    "in": "query",
                    "required": false,
                    "schema": {
                        "type": "string",
                        "enum": ["idle", "running", "waiting_for_approval", "waiting_for_user_input", "failed", "completed"]
                    }
                },
                {
                    "name": "has_runtime",
                    "in": "query",
                    "required": false,
                    "schema": { "type": "boolean" }
                },
                {
                    "name": "page",
                    "in": "query",
                    "required": false,
                    "schema": { "type": "boolean" }
                },
                {
                    "name": "limit",
                    "in": "query",
                    "required": false,
                    "schema": { "type": "integer", "minimum": 1, "maximum": 100 }
                },
                {
                    "name": "cursor",
                    "in": "query",
                    "required": false,
                    "schema": { "type": "string" }
                }
            ]),
        );
        get.insert(
            "responses".to_string(),
            serde_json::json!({
                "200": {
                    "description": "Legacy array by default, or a paginated envelope with exact counts when page=true or cursor is supplied."
                },
                "400": { "$ref": "#/components/responses/Problem" },
                "401": { "$ref": "#/components/responses/Problem" },
                "403": { "$ref": "#/components/responses/Problem" },
                "404": { "$ref": "#/components/responses/Problem" },
                "405": { "$ref": "#/components/responses/Problem" },
                "409": { "$ref": "#/components/responses/Problem" },
                "413": { "$ref": "#/components/responses/Problem" },
                "422": { "$ref": "#/components/responses/Problem" },
                "429": { "$ref": "#/components/responses/Problem" },
                "503": { "$ref": "#/components/responses/Problem" },
                "500": { "$ref": "#/components/responses/Problem" }
            }),
        );
    }

    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Kheish daemon control-plane API",
            "version": env!("CARGO_PKG_VERSION"),
            "x-api-revision": 3,
            "x-sse-replay": true,
            "x-typed-sse-heartbeat": true
        },
        "paths": paths,
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer"
                },
                "connectorBearer": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "Connector-scoped bearer token used by configured external or HTTP connector ingress routes."
                },
                "httpConnectorHmac": {
                    "type": "apiKey",
                    "in": "header",
                    "name": "x-kheish-signature",
                    "description": "HTTP connector HMAC signature."
                },
                "httpConnectorTimestamp": {
                    "type": "apiKey",
                    "in": "header",
                    "name": "x-kheish-timestamp",
                    "description": "HTTP connector HMAC timestamp used for replay protection."
                },
                "slackSignature": {
                    "type": "apiKey",
                    "in": "header",
                    "name": "x-slack-signature"
                },
                "slackRequestTimestamp": {
                    "type": "apiKey",
                    "in": "header",
                    "name": "x-slack-request-timestamp",
                    "description": "Slack request timestamp used for replay protection."
                },
                "telegramSecret": {
                    "type": "apiKey",
                    "in": "header",
                    "name": "x-telegram-bot-api-secret-token"
                },
                "observationUploadToken": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "Source-scoped observation upload token issued for one observation source."
                },
                "captureAgentToken": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "Machine-scoped capture-agent heartbeat token issued during capture provisioning."
                }
            },
            "responses": {
                "Problem": {
                    "description": "RFC 7807-style daemon problem response",
                    "content": {
                        "application/problem+json": {
                            "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                        }
                    }
                }
            },
            "schemas": {
                "ProblemDetails": {
                    "type": "object",
                    "required": ["type", "title", "status", "detail", "code"],
                    "properties": {
                        "type": { "type": "string" },
                        "title": { "type": "string" },
                        "status": { "type": "integer", "minimum": 100, "maximum": 599 },
                        "detail": { "type": "string" },
                        "code": { "type": "string" },
                        "domain": { "type": "string" }
                    }
                },
                "ListPage": {
                    "type": "object",
                    "required": ["items", "pagination"],
                    "properties": {
                        "items": { "type": "array", "items": {} },
                        "pagination": { "$ref": "#/components/schemas/ListPageMeta" }
                    }
                },
                "ListPageMeta": {
                    "type": "object",
                    "required": ["limit", "total_count", "has_more", "order"],
                    "properties": {
                        "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
                        "total_count": { "type": "integer", "minimum": 0 },
                        "has_more": { "type": "boolean" },
                        "next_cursor": { "type": "string" },
                        "order": { "type": "string" }
                    }
                }
            }
        }
    })
}

fn openapi_base_responses() -> Value {
    serde_json::json!({
        "2XX": { "description": "Success" },
        "400": { "$ref": "#/components/responses/Problem" },
        "401": { "$ref": "#/components/responses/Problem" },
        "403": { "$ref": "#/components/responses/Problem" },
        "404": { "$ref": "#/components/responses/Problem" },
        "405": { "$ref": "#/components/responses/Problem" },
        "409": { "$ref": "#/components/responses/Problem" },
        "413": { "$ref": "#/components/responses/Problem" },
        "422": { "$ref": "#/components/responses/Problem" },
        "429": { "$ref": "#/components/responses/Problem" },
        "502": { "$ref": "#/components/responses/Problem" },
        "503": { "$ref": "#/components/responses/Problem" },
        "500": { "$ref": "#/components/responses/Problem" }
    })
}

fn attach_common_pagination_parameters(paths: &mut serde_json::Map<String, Value>) {
    for path_name in OPENAPI_PAGINATED_LIST_PATHS {
        append_openapi_parameters(
            paths,
            path_name,
            "get",
            vec![
                serde_json::json!({
                    "name": "page",
                    "in": "query",
                    "required": false,
                    "schema": { "type": "boolean" },
                    "description": "When true, returns a cursor-paginated envelope instead of the legacy JSON array."
                }),
                serde_json::json!({
                    "name": "limit",
                    "in": "query",
                    "required": false,
                    "schema": { "type": "integer", "minimum": 1, "maximum": MAX_LIST_PAGE_LIMIT },
                    "description": "Maximum number of items in a paginated response. Use with page=true or a cursor to keep the legacy array contract for limit-only requests."
                }),
                serde_json::json!({
                    "name": "cursor",
                    "in": "query",
                    "required": false,
                    "schema": { "type": "string" },
                    "description": "Cursor returned by the previous paginated response."
                }),
            ],
        );
        let Some(Value::Object(path)) = paths.get_mut(*path_name) else {
            continue;
        };
        let Some(Value::Object(get)) = path.get_mut("get") else {
            continue;
        };
        get.insert(
            "responses".to_string(),
            serde_json::json!({
                "200": {
                    "description": "Legacy array by default, or a paginated envelope with exact counts when page=true or cursor is supplied."
                },
                "400": { "$ref": "#/components/responses/Problem" },
                "401": { "$ref": "#/components/responses/Problem" },
                "403": { "$ref": "#/components/responses/Problem" },
                "404": { "$ref": "#/components/responses/Problem" },
                "405": { "$ref": "#/components/responses/Problem" },
                "409": { "$ref": "#/components/responses/Problem" },
                "413": { "$ref": "#/components/responses/Problem" },
                "422": { "$ref": "#/components/responses/Problem" },
                "429": { "$ref": "#/components/responses/Problem" },
                "503": { "$ref": "#/components/responses/Problem" },
                "500": { "$ref": "#/components/responses/Problem" }
            }),
        );
    }
}

fn attach_path_parameters(paths: &mut serde_json::Map<String, Value>) {
    for spec in CONTROL_PLANE_OPENAPI_ROUTES {
        let parameters = openapi_path_parameters(spec.path);
        if parameters.is_empty() {
            continue;
        }
        for method in spec.methods {
            append_openapi_parameters(
                paths,
                spec.path,
                &method.to_ascii_lowercase(),
                parameters.clone(),
            );
        }
    }
}

fn attach_list_filter_parameters(paths: &mut serde_json::Map<String, Value>) {
    for (path, parameters) in [
        (
            "/v1/assets",
            vec![openapi_query_parameter(
                "query",
                serde_json::json!({ "type": "string" }),
                "Filters assets by identifier, file name, MIME type, or digest substring.",
            )],
        ),
        ("/v1/deliveries", openapi_delivery_filter_parameters()),
        (
            "/v1/deliveries/dead-letter",
            openapi_delivery_filter_parameters(),
        ),
        (
            "/v1/personas",
            vec![openapi_query_parameter(
                "query",
                serde_json::json!({ "type": "string" }),
                "Filters personas by identifier or display name substring.",
            )],
        ),
        (
            "/v1/observation-transcripts",
            vec![
                openapi_query_parameter(
                    "capture_group_id",
                    serde_json::json!({ "type": "string" }),
                    "Restricts transcript jobs to one capture group.",
                ),
                openapi_query_parameter(
                    "recording_id",
                    serde_json::json!({ "type": "string" }),
                    "Restricts transcript jobs to one Aurora recording.",
                ),
                openapi_query_parameter(
                    "status",
                    serde_json::json!({
                        "type": "string",
                        "enum": ["queued", "running", "completed", "failed", "cancelled"]
                    }),
                    "Restricts transcript jobs to one lifecycle status.",
                ),
            ],
        ),
        (
            "/v1/questions",
            vec![openapi_query_parameter(
                "session_id",
                serde_json::json!({ "type": "string" }),
                "Restricts pending questions to one session.",
            )],
        ),
        (
            "/v1/runs",
            vec![
                openapi_query_parameter(
                    "session_id",
                    serde_json::json!({ "type": "string" }),
                    "Restricts runs to one session.",
                ),
                openapi_query_parameter(
                    "priority_active",
                    serde_json::json!({ "type": "boolean" }),
                    "When true on legacy array responses, returns active runs first.",
                ),
            ],
        ),
        (
            "/v1/schedules",
            vec![openapi_query_parameter(
                "session_id",
                serde_json::json!({ "type": "string" }),
                "Restricts schedules to one session.",
            )],
        ),
        (
            "/v1/sessions",
            vec![openapi_query_parameter(
                "persona_id",
                serde_json::json!({ "type": "string" }),
                "Restricts sessions to one persona.",
            )],
        ),
        (
            "/v1/sessions/{session_id}/tasks",
            vec![openapi_query_parameter(
                "status",
                serde_json::json!({ "type": "string" }),
                "Restricts tasks to one lifecycle status.",
            )],
        ),
    ] {
        append_openapi_parameters(paths, path, "get", parameters);
    }
}

fn attach_sse_parameters(paths: &mut serde_json::Map<String, Value>) {
    for path in [
        "/v1/events/stream",
        "/v1/sessions/{session_id}/stream",
        "/v1/runs/{run_id}/stream",
        "/v1/flows/{flow_id}/stream",
    ] {
        append_openapi_parameters(
            paths,
            path,
            "get",
            vec![
                openapi_query_parameter(
                    "cursor",
                    serde_json::json!({ "type": "string", "pattern": "^[0-9]+$" }),
                    "Optional daemon event id cursor encoded as a decimal string. Only events with larger ids are replayed.",
                ),
                openapi_header_parameter(
                    "Last-Event-ID",
                    serde_json::json!({ "type": "string", "pattern": "^[0-9]+$" }),
                    "Standard SSE reconnect cursor encoded as a decimal string. When both header and query cursor are present, the larger value wins.",
                ),
            ],
        );
        attach_sse_response(paths, path);
    }
    append_openapi_parameters(
        paths,
        "/v1/events/stream",
        "get",
        vec![
            openapi_query_parameter(
                "session_id",
                serde_json::json!({ "type": "string" }),
                "Restricts the daemon-wide stream to one session.",
            ),
            openapi_query_parameter(
                "run_id",
                serde_json::json!({ "type": "string" }),
                "Restricts the daemon-wide stream to one run.",
            ),
        ],
    );
}

fn openapi_delivery_filter_parameters() -> Vec<Value> {
    vec![
        openapi_query_parameter(
            "session_id",
            serde_json::json!({ "type": "string" }),
            "Restricts deliveries to one session.",
        ),
        openapi_query_parameter(
            "run_id",
            serde_json::json!({ "type": "string" }),
            "Restricts deliveries to one run.",
        ),
        openapi_query_parameter(
            "plugin",
            serde_json::json!({ "type": "string" }),
            "Restricts deliveries to one plugin.",
        ),
        openapi_query_parameter(
            "status",
            serde_json::json!({ "type": "string" }),
            "Restricts deliveries to one delivery status.",
        ),
    ]
}

fn openapi_query_parameter(name: &str, schema: Value, description: &str) -> Value {
    serde_json::json!({
        "name": name,
        "in": "query",
        "required": false,
        "schema": schema,
        "description": description
    })
}

fn openapi_header_parameter(name: &str, schema: Value, description: &str) -> Value {
    serde_json::json!({
        "name": name,
        "in": "header",
        "required": false,
        "schema": schema,
        "description": description
    })
}

fn attach_sse_response(paths: &mut serde_json::Map<String, Value>, path_name: &str) {
    let Some(Value::Object(path)) = paths.get_mut(path_name) else {
        return;
    };
    let Some(Value::Object(operation)) = path.get_mut("get") else {
        return;
    };
    let Some(Value::Object(responses)) = operation.get_mut("responses") else {
        return;
    };
    responses.insert(
        "2XX".to_string(),
        serde_json::json!({
            "description": "Server-sent event stream",
            "content": {
                "text/event-stream": {
                    "schema": { "type": "string" }
                }
            }
        }),
    );
}

fn openapi_path_parameters(path: &str) -> Vec<Value> {
    let mut parameters = Vec::new();
    let mut cursor = 0;
    while let Some(open_offset) = path[cursor..].find('{') {
        let open = cursor + open_offset;
        let Some(close_offset) = path[open + 1..].find('}') else {
            break;
        };
        let close = open + 1 + close_offset;
        let name = &path[open + 1..close];
        parameters.push(serde_json::json!({
            "name": name,
            "in": "path",
            "required": true,
            "schema": { "type": "string" }
        }));
        cursor = close + 1;
    }
    parameters
}

fn append_openapi_parameters(
    paths: &mut serde_json::Map<String, Value>,
    path_name: &str,
    method: &str,
    parameters: Vec<Value>,
) {
    let Some(Value::Object(path)) = paths.get_mut(path_name) else {
        return;
    };
    let Some(Value::Object(operation)) = path.get_mut(method) else {
        return;
    };
    let entry = operation
        .entry("parameters".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(existing) = entry.as_array_mut() else {
        return;
    };
    for parameter in parameters {
        let name = parameter.get("name").and_then(Value::as_str);
        let location = parameter.get("in").and_then(Value::as_str);
        if existing.iter().any(|candidate| {
            candidate.get("name").and_then(Value::as_str) == name
                && candidate.get("in").and_then(Value::as_str) == location
        }) {
            continue;
        }
        existing.push(parameter);
    }
}

fn operation_id(method: &str, path: &str) -> String {
    let mut id = method.to_ascii_lowercase();
    for character in path.trim_start_matches('/').chars() {
        match character {
            '/' | '-' | '.' => id.push('_'),
            '{' | '}' => {}
            _ if character.is_ascii_alphanumeric() || character == '_' => id.push(character),
            _ => id.push('_'),
        }
    }
    id
}

struct OpenApiRouteSpec {
    path: &'static str,
    methods: &'static [&'static str],
}

fn openapi_operation_security(path: &str, method: &str) -> Value {
    match (path, method) {
        ("/healthz", _) | ("/readyz", _) => serde_json::json!([]),
        ("/v1/connectors/http/{name}", "POST") => serde_json::json!([
            { "connectorBearer": [] },
            { "httpConnectorHmac": [], "httpConnectorTimestamp": [] }
        ]),
        ("/v1/connectors/external/{name}/events", "POST")
        | ("/v1/connectors/external/{name}/events/batch", "POST") => {
            serde_json::json!([{ "connectorBearer": [] }])
        }
        ("/v1/connectors/external/{name}/credentials/{env_key}", "GET")
        | (
            "/v1/connectors/external/{name}/deliveries/{delivery_id}/assets/{asset_id}/raw",
            "GET",
        ) => serde_json::json!([{ "connectorBearer": [] }]),
        ("/v1/connectors/slack/{name}", "POST") => {
            serde_json::json!([{ "slackSignature": [], "slackRequestTimestamp": [] }])
        }
        ("/v1/connectors/telegram/{name}", "POST") => {
            serde_json::json!([{ "telegramSecret": [] }])
        }
        ("/v1/observation-sources/{source_id}/observations", "POST") => {
            serde_json::json!([{ "observationUploadToken": [] }])
        }
        ("/v1/capture-agents/{machine_id}/heartbeat", "POST") => {
            serde_json::json!([{ "captureAgentToken": [] }])
        }
        _ => serde_json::json!([{ "bearerAuth": [] }]),
    }
}

const OPENAPI_PAGINATED_LIST_PATHS: &[&str] = &[
    "/v1/agents/summaries",
    "/v1/assets",
    "/v1/deliveries",
    "/v1/deliveries/dead-letter",
    "/v1/observation-transcripts",
    "/v1/observation-transcripts/{transcript_job_id}/segments",
    "/v1/personas",
    "/v1/questions",
    "/v1/runs",
    "/v1/schedules",
    "/v1/sessions",
    "/v1/sessions/{session_id}/questions",
    "/v1/sessions/{session_id}/tasks",
];

const CONTROL_PLANE_OPENAPI_ROUTES: &[OpenApiRouteSpec] = &[
    OpenApiRouteSpec {
        path: "/healthz",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/readyz",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/status",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/capabilities",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/openapi.json",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/events/stream",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/mcp/tools/{tool_name}/call",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/subagent-policy/quotas",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/learning-policy",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/run-memory-policy",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/tool-limits",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/hooks",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/hooks/dead-letter",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/hooks/dead-letter/{dead_letter_id}/resolve",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/model",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/revisions",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/rollback",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/permission-mode",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/permissions/check",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/permissions/matrix",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/debug-level",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/system-prompt",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/secrets",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/secrets/{secret_ref}",
        methods: &["GET", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/connectors",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/connectors/external/metrics",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/deliveries/metrics",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/connectors/{kind}/{name}",
        methods: &["GET", "PUT", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/auth/subjects/{subject_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/auth/subjects/{subject_id}/revoke",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/auth/leases/{lease_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/auth/leases/{lease_id}/revoke",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/auth/slots/{slot_id}/revoke",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/auth/accounts",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/auth/accounts/mcp-oauth",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/auth/accounts/{slot_id}",
        methods: &["GET", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/auth/accounts/{slot_id}/refresh",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runtime/auth/accounts/{slot_id}/revoke",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/permission-audits",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/goal",
        methods: &["GET", "POST", "PUT", "PATCH", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/input",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/runs",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/events",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/stream",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/approvals",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/questions",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/memory-context",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/memory-search",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/skills",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/persona",
        methods: &["POST", "PUT", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/route-policy",
        methods: &["POST", "PUT", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/operator",
        methods: &["GET", "POST", "PUT", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/capability-scope",
        methods: &["POST", "PUT", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/credential-scope",
        methods: &["POST", "PUT", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/reply-targets",
        methods: &["GET", "POST", "PUT", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/tasks",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/tasks/{task_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/tasks/{task_id}/output",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/tasks/{task_id}/stop",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/approval-runs",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/interrupt",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/sessions/{session_id}/end",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runs",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runs/{run_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runs/{run_id}/cancel",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runs/{run_id}/events",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runs/{run_id}/stream",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runs/{run_id}/debug",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runs/{run_id}/debug/artifacts/{artifact_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runs/{run_id}/approvals",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runs/{run_id}/questions",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runs/{run_id}/questions/{request_id}/cancel",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/runs/{run_id}/external-actions",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/runs/prune",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/questions",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/agents",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/agents/audit",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/agents/summaries",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/agents/{agent_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/agents/{agent_id}/audit",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/agents/{agent_id}/nickname",
        methods: &["POST", "PUT", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/agents/{agent_id}/sidechains",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/agents/{agent_id}/sidechains/explain",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/agents/{agent_id}/mailbox",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/agents/{agent_id}/mailbox/dead-letter",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/agents/{agent_id}/mailbox/{message_id}/ack",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/mailboxes",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/assets",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/assets/gc",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/assets/{asset_id}",
        methods: &["GET", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/assets/{asset_id}/references",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/assets/{asset_id}/raw",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/boards",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/boards/{board_id}",
        methods: &["GET", "PUT"],
    },
    OpenApiRouteSpec {
        path: "/v1/boards/{board_id}/revisions",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/boards/{board_id}/revisions/{revision_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/capture-agent-provisions",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/capture-agents",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/capture-agents/{machine_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/capture-agents/{machine_id}/revoke",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/capture-agents/{machine_id}/heartbeat",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/capture-alerts",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/channels",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/channels/{channel_id}",
        methods: &["GET", "PUT", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/channels/{channel_id}/members",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/channels/{channel_id}/members/{member_id}",
        methods: &["DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/channels/{channel_id}/messages",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/channels/{channel_id}/messages/{message_id}/reactions",
        methods: &["POST", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/channels/{channel_id}/stimuli",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/channels/{channel_id}/thread-work",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/channels/{channel_id}/leases",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/projects",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/projects/{project_id}",
        methods: &["GET", "PUT", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/projects/{project_id}/members",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/projects/{project_id}/members/{member_id}",
        methods: &["DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/projects/{project_id}/channels",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/projects/{project_id}/channels/{channel_id}",
        methods: &["DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/projects/{project_id}/tasks",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/projects/{project_id}/tasks/{task_id}",
        methods: &["GET", "PUT", "DELETE"],
    },
    OpenApiRouteSpec {
        path: "/v1/projects/{project_id}/tasks/{task_id}/start",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/stacks",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/stacks/validate",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/stacks/plan",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/stacks/apply",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/stacks/verify",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/stacks/import",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/stacks/down",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/stacks/{ownership_id}/ledger",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/playbooks",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/playbooks/validate",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/playbooks/{playbook_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/playbooks/{playbook_id}/publish",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/playbooks/{playbook_id}/revoke",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/flows",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/flows/{flow_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/flows/{flow_id}/cancel",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/flows/{flow_id}/evidence",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/flows/{flow_id}/verify/product-view",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/flows/{flow_id}/stream",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/observation-sources",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/observation-sources/{source_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/observation-sources/{source_id}/rotate-token",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/observation-sources/{source_id}/revoke-token",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/observation-sources/{source_id}/observations",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/observation-audit",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/observations",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/observations/{observation_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/observation-materializations",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/observation-transcripts",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/observation-transcripts/{transcript_job_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/observation-transcripts/{transcript_job_id}/segments",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/observation-transcripts/{transcript_job_id}/retry",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/observation-transcripts/{transcript_job_id}/cancel",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/derivations",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/derivations/{derivation_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/learning-candidates",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/learning-candidates/{candidate_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/learning-candidates/{candidate_id}/publish",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/learning-candidates/{candidate_id}/reject",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/learnings",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/learnings/revoke-matching",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/learnings/{learning_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/learnings/{learning_id}/revoke",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/learnings/{learning_id}/supersede",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/learnings/{learning_id}/promote-skill",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/learning-skills",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/learning-skills/{skill_name}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/learning-skills/{skill_name}/rollout-result",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/learning-skills/{skill_name}/revoke",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/learning-skills/{skill_name}/rollback",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/skills",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/skills/{skill_name}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/connectors/http/{name}",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/connectors/external/{name}/events",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/connectors/external/{name}/events/batch",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/connectors/external/{name}/credentials/{env_key}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/connectors/external/{name}/deliveries/{delivery_id}/assets/{asset_id}/raw",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/connectors/slack/{name}",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/connectors/telegram/{name}",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/personas",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/personas/{persona_id}",
        methods: &["GET", "PUT"],
    },
    OpenApiRouteSpec {
        path: "/v1/schedules",
        methods: &["GET", "POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/schedules/{schedule_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/schedules/{schedule_id}/cancel",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/schedules/{schedule_id}/pause",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/schedules/{schedule_id}/resume",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/schedules/{schedule_id}/trigger",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/deliveries",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/deliveries/dead-letter",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/deliveries/backpressure/reset",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/deliveries/{delivery_id}",
        methods: &["GET"],
    },
    OpenApiRouteSpec {
        path: "/v1/deliveries/{delivery_id}/replay",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/deliveries/replay-bulk",
        methods: &["POST"],
    },
    OpenApiRouteSpec {
        path: "/v1/deliveries/{delivery_id}/resolve",
        methods: &["POST"],
    },
];

async fn status<M>(State(state): State<Arc<DaemonState<M>>>) -> Json<DaemonStatusView>
where
    M: ModelDriver + Send + Sync + 'static,
{
    Json(state.status_snapshot(daemon_capabilities()).await)
}

async fn get_runtime<M>(State(state): State<Arc<DaemonState<M>>>) -> Json<RuntimeSettingsView>
where
    M: ModelDriver + Send + Sync + 'static,
{
    Json(state.runtime_settings().await)
}

async fn call_runtime_mcp_tool<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(tool_name): AxumPath<String>,
    Json(request): Json<McpToolCallRequest>,
) -> Result<Json<McpToolCallResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .call_mcp_tool(&tool_name, request.input)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_runtime_config_revisions<M>(
    State(state): State<Arc<DaemonState<M>>>,
) -> Json<RuntimeConfigRevisionListResponse>
where
    M: ModelDriver + Send + Sync + 'static,
{
    Json(state.runtime_config_revisions().await)
}

async fn rollback_runtime_config<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<RuntimeRollbackRequest>,
) -> Result<Json<RuntimeSettingsView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .rollback_runtime_config(request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_subagent_policy_quotas<M>(
    State(state): State<Arc<DaemonState<M>>>,
) -> Json<crate::SubagentPolicyStatusView>
where
    M: ModelDriver + Send + Sync + 'static,
{
    Json(state.subagent_policy_status())
}

async fn get_learning_policy<M>(
    State(state): State<Arc<DaemonState<M>>>,
) -> Json<crate::LearningAutomationPolicyConfig>
where
    M: ModelDriver + Send + Sync + 'static,
{
    Json(state.learning_policy_settings_snapshot().await)
}

async fn list_runtime_secrets<M>(
    State(state): State<Arc<DaemonState<M>>>,
) -> Result<Json<Vec<AuthSlotStatus>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_auth_statuses()
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_runtime_secret<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(secret_ref): AxumPath<String>,
) -> Result<Json<AuthSlotStatus>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .auth_status(&secret_ref)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn put_runtime_secret<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(record): Json<AuthSlotRecord>,
) -> Result<Json<AuthSlotStatus>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .put_auth_record(record)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn delete_runtime_secret<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(secret_ref): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .delete_auth_slot(&secret_ref)
        .await
        .map(|accepted| Json(serde_json::json!({ "accepted": accepted })))
        .map_err(internal_error)
}

async fn get_runtime_auth_subject<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(subject_id): AxumPath<String>,
) -> Result<Json<AuthSubjectStatus>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .auth_subject_status(&subject_id)
        .map(Json)
        .map_err(internal_error)
}

async fn revoke_runtime_auth_subject<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(subject_id): AxumPath<String>,
) -> Result<Json<AuthSubjectStatus>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .revoke_auth_subject(&subject_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_runtime_auth_lease<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(lease_id): AxumPath<String>,
) -> Result<Json<CredentialLeaseStatus>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .auth_lease_status(&lease_id)
        .map(Json)
        .map_err(internal_error)
}

async fn revoke_runtime_auth_lease<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(lease_id): AxumPath<String>,
) -> Result<Json<CredentialLeaseStatus>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .revoke_auth_lease(&lease_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn revoke_runtime_auth_slot<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(slot_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .revoke_auth_slot_leases(&slot_id)
        .await
        .map(|revoked_leases| {
            Json(serde_json::json!({
                "slot_id": slot_id,
                "revoked_leases": revoked_leases,
            }))
        })
        .map_err(internal_error)
}

async fn list_runtime_auth_accounts<M>(
    State(state): State<Arc<DaemonState<M>>>,
) -> Result<Json<Vec<AuthSlotStatus>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_auth_statuses()
        .await
        .map(|statuses| {
            statuses
                .into_iter()
                .filter(|status| status.mode == kheish_auth::AuthMode::OAuthAccount)
                .collect::<Vec<_>>()
        })
        .map(Json)
        .map_err(internal_error)
}

async fn put_runtime_mcp_oauth_account<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(input): Json<McpOAuthAccountRecordInput>,
) -> Result<Json<AuthSlotStatus>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .put_mcp_oauth_account(input)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_runtime_auth_account<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(slot_id): AxumPath<String>,
) -> Result<Json<AuthSlotStatus>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let status = state.auth_status(&slot_id).await.map_err(internal_error)?;
    if status.mode != kheish_auth::AuthMode::OAuthAccount {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("auth slot `{slot_id}` is not an OAuth account"),
        ));
    }
    Ok(Json(status))
}

async fn refresh_runtime_auth_account<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(slot_id): AxumPath<String>,
) -> Result<Json<AuthSlotStatus>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let status = state.auth_status(&slot_id).await.map_err(internal_error)?;
    if status.mode != kheish_auth::AuthMode::OAuthAccount {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("auth slot `{slot_id}` is not an OAuth account"),
        ));
    }
    state
        .refresh_auth_slot(&slot_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn revoke_runtime_auth_account<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(slot_id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let status = state.auth_status(&slot_id).await.map_err(internal_error)?;
    if status.mode != kheish_auth::AuthMode::OAuthAccount {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("auth slot `{slot_id}` is not an OAuth account"),
        ));
    }
    state
        .revoke_auth_account_slot(&slot_id)
        .await
        .map(|accepted| Json(serde_json::json!({ "accepted": accepted })))
        .map_err(internal_error)
}

async fn list_runtime_connectors<M>(
    State(state): State<Arc<DaemonState<M>>>,
) -> Result<Json<Vec<ConnectorView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let connectors = state
        .list_connectors()
        .await
        .into_iter()
        .map(ConnectorView::from)
        .collect::<Vec<_>>();
    Ok(Json(connectors))
}

async fn get_external_connector_metrics<M>(
    State(state): State<Arc<DaemonState<M>>>,
) -> Result<impl IntoResponse, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let body = state
        .external_connector_runtime()
        .render_prometheus_metrics()
        .await;
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4"),
        )],
        body,
    ))
}

async fn get_delivery_queue_metrics<M>(
    State(state): State<Arc<DaemonState<M>>>,
) -> Result<impl IntoResponse, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let body = state
        .delivery_queue_status_snapshot(crate::now_ms())
        .await
        .map_err(internal_error)?
        .render_prometheus_metrics();
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4"),
        )],
        body,
    ))
}

async fn get_runtime_connector<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((kind, name)): AxumPath<(String, String)>,
) -> Result<Json<ConnectorView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .connector(&kind, &name)
        .await
        .map(ConnectorView::from)
        .map(Json)
        .ok_or_else(|| internal_error(anyhow!("unknown connector {kind}/{name}")))
}

async fn put_runtime_connector<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((kind, name)): AxumPath<(String, String)>,
    Json(payload): Json<Value>,
) -> Result<Json<ConnectorView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let payload_object = payload
        .as_object()
        .cloned()
        .ok_or_else(|| internal_error(anyhow!("connector payload must be a JSON object")))?;
    match kind.as_str() {
        "external" => {
            let request = serde_json::from_value::<PutExternalConnectorRequest>(payload.clone())
                .context("failed to decode external connector payload")
                .map_err(internal_error)?;
            let built =
                build_external_connector_config(state.as_ref(), &name, &payload_object, request)
                    .await
                    .map_err(internal_error)?;
            let applied = apply_connector_secret_writes(state.as_ref(), &built.secret_writes)
                .await
                .map_err(internal_error)?;
            if let Err(error) = state.put_external_connector(built.config).await {
                rollback_connector_secret_writes(state.as_ref(), applied)
                    .await
                    .map_err(internal_error)?;
                return Err(internal_error(error));
            }
            notify_connector_secret_writes(state.as_ref(), &built.secret_writes).await;
        }
        "telegram" => {
            let request = serde_json::from_value::<PutTelegramConnectorRequest>(payload.clone())
                .context("failed to decode telegram connector payload")
                .map_err(internal_error)?;
            let built =
                build_telegram_connector_config(state.as_ref(), &name, &payload_object, request)
                    .await
                    .map_err(internal_error)?;
            let applied = apply_connector_secret_writes(state.as_ref(), &built.secret_writes)
                .await
                .map_err(internal_error)?;
            if let Err(error) = state.put_telegram_connector(built.config).await {
                rollback_connector_secret_writes(state.as_ref(), applied)
                    .await
                    .map_err(internal_error)?;
                return Err(internal_error(error));
            }
            notify_connector_secret_writes(state.as_ref(), &built.secret_writes).await;
        }
        "slack" => {
            let request = serde_json::from_value::<PutSlackConnectorRequest>(payload.clone())
                .context("failed to decode slack connector payload")
                .map_err(internal_error)?;
            let built =
                build_slack_connector_config(state.as_ref(), &name, &payload_object, request)
                    .await
                    .map_err(internal_error)?;
            let applied = apply_connector_secret_writes(state.as_ref(), &built.secret_writes)
                .await
                .map_err(internal_error)?;
            if let Err(error) = state.put_slack_connector(built.config).await {
                rollback_connector_secret_writes(state.as_ref(), applied)
                    .await
                    .map_err(internal_error)?;
                return Err(internal_error(error));
            }
            notify_connector_secret_writes(state.as_ref(), &built.secret_writes).await;
        }
        "http" => {
            let request = serde_json::from_value::<PutHttpConnectorRequest>(payload)
                .context("failed to decode http connector payload")
                .map_err(internal_error)?;
            let built =
                build_http_connector_config(state.as_ref(), &name, &payload_object, request)
                    .await
                    .map_err(internal_error)?;
            let applied = apply_connector_secret_writes(state.as_ref(), &built.secret_writes)
                .await
                .map_err(internal_error)?;
            if let Err(error) = state.put_http_connector(built.config).await {
                rollback_connector_secret_writes(state.as_ref(), applied)
                    .await
                    .map_err(internal_error)?;
                return Err(internal_error(error));
            }
            notify_connector_secret_writes(state.as_ref(), &built.secret_writes).await;
        }
        _ => return Err(internal_error(anyhow!("unknown connector kind {kind}"))),
    }
    state
        .connector(&kind, &name)
        .await
        .map(ConnectorView::from)
        .map(Json)
        .ok_or_else(|| internal_error(anyhow!("unknown connector {kind}/{name}")))
}

async fn delete_runtime_connector<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((kind, name)): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    if state.connector(&kind, &name).await.is_none() {
        return Err(internal_error(anyhow!("unknown connector {kind}/{name}")));
    }
    state
        .delete_connector(&kind, &name)
        .await
        .map(|accepted| Json(serde_json::json!({ "accepted": accepted })))
        .map_err(internal_error)
}

async fn list_assets<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<AssetListQuery>,
) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let assets = state
        .list_assets(query.query.as_deref())
        .await
        .map_err(internal_error)?;
    let page = query.page_query();
    list_or_page(assets, &page, query.limit, "asset_id_asc", |asset| {
        asset.asset_id.clone()
    })
}

async fn get_asset<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(asset_id): AxumPath<String>,
) -> Result<Json<AssetView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_asset(&asset_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn delete_asset<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(asset_id): AxumPath<String>,
    Query(query): Query<AssetDeleteQuery>,
) -> Result<Json<AssetDeletionPlanView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .delete_asset(&asset_id, query.dry_run.unwrap_or(false))
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_asset_references<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(asset_id): AxumPath<String>,
) -> Result<Json<AssetReferencesView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .asset_references(&asset_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn gc_assets<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<AssetGcRequest>,
) -> Result<Json<AssetGcPlanView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .gc_assets(request.dry_run.unwrap_or(true))
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn import_asset<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<CreateAssetRequest>,
) -> Result<Json<AssetView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .import_asset(&request.upload)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_asset_raw<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(asset_id): AxumPath<String>,
) -> Result<impl IntoResponse, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let (asset, bytes) = state
        .get_asset_raw(&asset_id)
        .await
        .map_err(internal_error)?;
    let headers = super::asset_raw_response_headers(&asset).map_err(internal_error)?;
    Ok((headers, bytes))
}

async fn list_boards<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<BoardListQuery>,
) -> Result<Json<Vec<BoardView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_boards(query.owner_session_id.as_deref(), query.query.as_deref())
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn create_board<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<CreateBoardRequest>,
) -> Result<Json<BoardView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .create_board(request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_board<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(board_id): AxumPath<String>,
) -> Result<Json<BoardView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_board(&board_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn update_board<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(board_id): AxumPath<String>,
    Json(request): Json<UpdateBoardRequest>,
) -> Result<Json<BoardView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .update_board(&board_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_board_revisions<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(board_id): AxumPath<String>,
) -> Result<Json<Vec<BoardRevisionView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_board_revisions(&board_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn create_board_revision<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(board_id): AxumPath<String>,
    Json(request): Json<CreateBoardRevisionRequest>,
) -> Result<Json<BoardRevisionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .create_board_revision(&board_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_board_revision<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((board_id, revision_id)): AxumPath<(String, String)>,
) -> Result<Json<BoardRevisionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_board_revision(&board_id, &revision_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_channels<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<ChannelListQuery>,
) -> Result<Json<Vec<ChannelView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_channels(query.query.as_deref())
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn create_channel<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<CreateChannelRequest>,
) -> Result<Json<ChannelView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .create_channel(request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_channel<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(channel_id): AxumPath<String>,
) -> Result<Json<ChannelView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_channel(&channel_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn update_channel<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(channel_id): AxumPath<String>,
    Json(request): Json<UpdateChannelRequest>,
) -> Result<Json<ChannelView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .update_channel(&channel_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn delete_channel<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(channel_id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .delete_channel(&channel_id)
        .await
        .map(|()| Json(serde_json::json!({ "accepted": true })))
        .map_err(internal_error)
}

async fn list_channel_members<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(channel_id): AxumPath<String>,
) -> Result<Json<Vec<crate::ChannelMemberView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_channel(&channel_id)
        .await
        .map(|channel| Json(channel.members))
        .map_err(internal_error)
}

async fn upsert_channel_member<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(channel_id): AxumPath<String>,
    Json(request): Json<ChannelMemberRequest>,
) -> Result<Json<ChannelView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .upsert_channel_member(&channel_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn remove_channel_member<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((channel_id, member_id)): AxumPath<(String, String)>,
) -> Result<Json<ChannelView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .remove_channel_member(&channel_id, &member_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_channel_messages<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(channel_id): AxumPath<String>,
    Query(query): Query<ChannelMessageListQuery>,
) -> Result<Json<Vec<ChannelMessageView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_channel_messages(
            &channel_id,
            query.thread_root_message_id.as_deref(),
            query.query.as_deref(),
            query.limit,
        )
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn post_channel_message<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(channel_id): AxumPath<String>,
    Json(request): Json<PostChannelMessageRequest>,
) -> Result<Json<ChannelMessageView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .post_channel_message(&channel_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn set_channel_reaction<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((channel_id, message_id)): AxumPath<(String, String)>,
    Json(request): Json<SetChannelReactionRequest>,
) -> Result<Json<ChannelMessageView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_channel_reaction(&channel_id, &message_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn unset_channel_reaction<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((channel_id, message_id)): AxumPath<(String, String)>,
    Json(request): Json<SetChannelReactionRequest>,
) -> Result<Json<ChannelMessageView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .unset_channel_reaction(&channel_id, &message_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_channel_stimuli<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(channel_id): AxumPath<String>,
    Query(query): Query<ChannelStimulusListQuery>,
) -> Result<Json<Vec<ChannelStimulusView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_channel_stimuli(
            &channel_id,
            query.thread_root_message_id.as_deref(),
            query.state,
            query.limit,
        )
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn create_channel_stimulus<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(channel_id): AxumPath<String>,
    Json(request): Json<CreateChannelStimulusRequest>,
) -> Result<Json<ChannelStimulusView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .create_channel_stimulus(&channel_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_channel_thread_work<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(channel_id): AxumPath<String>,
    Query(query): Query<ChannelThreadWorkListQuery>,
) -> Result<Json<Vec<ChannelThreadWorkStateView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_channel_thread_work(&channel_id, query.thread_root_message_id.as_deref())
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_channel_leases<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(channel_id): AxumPath<String>,
) -> Result<Json<Vec<ChannelTurnLeaseView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_channel_leases(&channel_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_projects<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<ProjectListQuery>,
) -> Result<Json<Vec<ProjectView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_projects(
            query.query.as_deref(),
            query.status.as_ref(),
            query.member_session_id.as_deref(),
            query.channel_id.as_deref(),
        )
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn create_project<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<CreateProjectRequest>,
) -> Result<Json<ProjectView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .create_project(request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_project<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(project_id): AxumPath<String>,
) -> Result<Json<ProjectView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_project(&project_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn update_project<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(project_id): AxumPath<String>,
    Json(request): Json<UpdateProjectRequest>,
) -> Result<Json<ProjectView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .update_project(&project_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn delete_project<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(project_id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .delete_project(&project_id)
        .await
        .map(|()| Json(serde_json::json!({ "accepted": true })))
        .map_err(internal_error)
}

async fn list_project_members<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(project_id): AxumPath<String>,
) -> Result<Json<Vec<ProjectMemberView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_project(&project_id)
        .await
        .map(|project| Json(project.members))
        .map_err(internal_error)
}

async fn upsert_project_member<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(project_id): AxumPath<String>,
    Json(request): Json<ProjectMemberRequest>,
) -> Result<Json<ProjectView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .upsert_project_member(&project_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn remove_project_member<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((project_id, member_id)): AxumPath<(String, String)>,
) -> Result<Json<ProjectView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .remove_project_member(&project_id, &member_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_project_channels<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(project_id): AxumPath<String>,
) -> Result<Json<Vec<ProjectChannelLinkView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_project(&project_id)
        .await
        .map(|project| Json(project.channel_links))
        .map_err(internal_error)
}

async fn upsert_project_channel<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(project_id): AxumPath<String>,
    Json(request): Json<ProjectChannelLinkRequest>,
) -> Result<Json<ProjectView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .upsert_project_channel_link(&project_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn remove_project_channel<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((project_id, channel_id)): AxumPath<(String, String)>,
) -> Result<Json<ProjectView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .remove_project_channel_link(&project_id, &channel_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_project_tasks<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(project_id): AxumPath<String>,
    Query(query): Query<ProjectTaskListQuery>,
) -> Result<Json<Vec<ProjectTaskView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_project_tasks(
            &project_id,
            query.query.as_deref(),
            query.status.as_ref(),
            query.assignee_member_id.as_deref(),
        )
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn create_project_task<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(project_id): AxumPath<String>,
    Json(request): Json<CreateProjectTaskRequest>,
) -> Result<Json<ProjectTaskView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .create_project_task(&project_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_project_task<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((project_id, task_id)): AxumPath<(String, String)>,
) -> Result<Json<ProjectTaskView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_project_task(&project_id, &task_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn update_project_task<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((project_id, task_id)): AxumPath<(String, String)>,
    Json(request): Json<UpdateProjectTaskRequest>,
) -> Result<Json<ProjectTaskView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .update_project_task(&project_id, &task_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn delete_project_task<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((project_id, task_id)): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .delete_project_task(&project_id, &task_id)
        .await
        .map(|deleted| Json(serde_json::json!({ "deleted": deleted })))
        .map_err(internal_error)
}

async fn start_project_task<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((project_id, task_id)): AxumPath<(String, String)>,
    Json(request): Json<StartProjectTaskRequest>,
) -> Result<Json<RunView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .start_project_task(&project_id, &task_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

fn stack_context_from_request<M>(
    state: &DaemonState<M>,
    request: StackManifestRequest,
) -> Result<crate::stack::StackContext, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let root = request
        .file_root
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    crate::stack::StackContext::from_manifest(
        &request.manifest,
        root,
        Some(state.state_root().to_path_buf()),
        request.strict_scopes.unwrap_or(true),
    )
    .map_err(internal_error)
}

async fn parse_stack_json_request<T>(request: Request<Body>) -> Result<T, ApiError>
where
    T: DeserializeOwned,
{
    let bytes = to_bytes(request.into_body(), STACK_CONTROL_PLANE_JSON_BODY_LIMIT_BYTES)
        .await
        .map_err(|_| {
            ApiError::coded(
                StatusCode::PAYLOAD_TOO_LARGE,
                "stacks",
                "stack_payload_too_large",
                format!(
                    "KheishStack request body exceeds the {STACK_CONTROL_PLANE_JSON_BODY_LIMIT_BYTES} byte limit"
                ),
            )
        })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        ApiError::coded(
            StatusCode::BAD_REQUEST,
            "stacks",
            "stack_invalid_json",
            format!("failed to parse KheishStack request JSON: {error}"),
        )
    })
}

async fn validate_stack<M>(
    State(state): State<Arc<DaemonState<M>>>,
    request: Request<Body>,
) -> Result<Json<crate::StackValidation>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let request = parse_stack_json_request::<StackManifestRequest>(request).await?;
    let context = stack_context_from_request(state.as_ref(), request)?;
    crate::stack::validate_stack_context(&context)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn plan_stack<M>(
    State(state): State<Arc<DaemonState<M>>>,
    request: Request<Body>,
) -> Result<Json<crate::StackPlan>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let request = parse_stack_json_request::<StackPlanRequest>(request).await?;
    let only_changes = request.only_changes;
    let allow_secret_env = request.allow_secret_env;
    let context = stack_context_from_request(state.as_ref(), request.stack)?;
    let client = crate::stack::StateStackControlPlane::new(state);
    crate::stack::plan_stack(&client, &context, only_changes, allow_secret_env)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn apply_stack<M>(
    State(state): State<Arc<DaemonState<M>>>,
    request: Request<Body>,
) -> Result<Json<crate::StackApplyReport>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let request = parse_stack_json_request::<StackApplyRequest>(request).await?;
    let context = stack_context_from_request(state.as_ref(), request.stack)?;
    let client = crate::stack::StateStackControlPlane::new(state);
    crate::stack::apply_stack(
        &client,
        context,
        crate::stack::StackApplyOptions {
            dry_run: request.dry_run,
            force_restart: request.force_restart,
            allow_secret_env: request.allow_secret_env,
            prune: request.prune,
        },
    )
    .await
    .map(Json)
    .map_err(internal_error)
}

async fn verify_stack<M>(
    State(state): State<Arc<DaemonState<M>>>,
    request: Request<Body>,
) -> Result<Json<crate::StackVerificationReport>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let request = parse_stack_json_request::<StackManifestRequest>(request).await?;
    let context = stack_context_from_request(state.as_ref(), request)?;
    let client = crate::stack::StateStackControlPlane::new(state);
    crate::stack::verify_stack(&client, &context)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn import_stack<M>(
    State(state): State<Arc<DaemonState<M>>>,
    request: Request<Body>,
) -> Result<Json<crate::StackImportReport>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let request = parse_stack_json_request::<StackImportRequest>(request).await?;
    let context = stack_context_from_request(state.as_ref(), request.stack)?;
    let client = crate::stack::StateStackControlPlane::new(state);
    crate::stack::import_stack(
        &client,
        context,
        crate::stack::StackImportOptions {
            resources: request.resources,
            allow_secret_env: request.allow_secret_env,
        },
    )
    .await
    .map(Json)
    .map_err(internal_error)
}

async fn down_stack<M>(
    State(state): State<Arc<DaemonState<M>>>,
    request: Request<Body>,
) -> Result<Json<crate::StackDownReport>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let request = parse_stack_json_request::<StackDownRequest>(request).await?;
    let context = stack_context_from_request(state.as_ref(), request.stack)?;
    let client = crate::stack::StateStackControlPlane::new(state);
    crate::stack::down_stack(
        &client,
        context,
        crate::stack::StackDownOptions { yes: request.yes },
    )
    .await
    .map(Json)
    .map_err(internal_error)
}

/// Lists every ledger-owned stack with a compact per-resource-type summary,
/// so consoles can enumerate stacks without knowing ownership ids up front.
async fn list_stacks<M>(State(state): State<Arc<DaemonState<M>>>) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let path = crate::stack::stack_ledger_path(state.state_root());
    let ledger = match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).map_err(|error| {
            internal_error(anyhow!(
                "failed to parse stack ledger {}: {error}",
                path.display()
            ))
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return json_value(serde_json::json!([]));
        }
        Err(error) => {
            return Err(internal_error(
                anyhow!(error).context(format!("failed to read {}", path.display())),
            ));
        }
    };
    let mut summaries = Vec::new();
    if let Some(stacks) = ledger.get("stacks").and_then(Value::as_object) {
        for (ownership_id, stack) in stacks {
            let resources = stack
                .get("resources")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let mut by_type = serde_json::Map::new();
            let mut updated_at_ms: Option<u64> = None;
            for (key, resource) in &resources {
                let resource_type = key.split('/').next().unwrap_or("resource");
                let count = by_type
                    .get(resource_type)
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                by_type.insert(resource_type.to_string(), Value::from(count + 1));
                if let Some(applied) = resource.get("last_applied_at_ms").and_then(Value::as_u64) {
                    updated_at_ms = Some(updated_at_ms.unwrap_or(0).max(applied));
                }
            }
            if let Some(last_operation_at_ms) = stack
                .get("operations")
                .and_then(Value::as_array)
                .and_then(|operations| operations.last())
                .and_then(|operation| operation.get("at_ms"))
                .and_then(Value::as_u64)
            {
                updated_at_ms = Some(updated_at_ms.unwrap_or(0).max(last_operation_at_ms));
            }
            summaries.push(serde_json::json!({
                "ownership_id": ownership_id,
                "resource_count": resources.len(),
                "updated_at_ms": updated_at_ms,
                "resources": Value::Object(by_type),
            }));
        }
    }
    json_value(Value::Array(summaries))
}

async fn get_stack_ledger<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(ownership_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let path = crate::stack::stack_ledger_path(state.state_root());
    let ledger = match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).map_err(|error| {
            internal_error(anyhow!(
                "failed to parse stack ledger {}: {error}",
                path.display()
            ))
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({
            "version": 1,
            "stacks": {},
        }),
        Err(error) => {
            return Err(internal_error(
                anyhow!(error).context(format!("failed to read {}", path.display())),
            ));
        }
    };
    let stack = ledger
        .get("stacks")
        .and_then(|stacks| stacks.get(&ownership_id))
        .cloned()
        .unwrap_or(Value::Null);
    json_value(serde_json::json!({
        "ownership_id": ownership_id,
        "ledger_path": path.display().to_string(),
        "stack": stack,
    }))
}

async fn list_playbooks<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<PlaybookListQuery>,
) -> Result<Json<Vec<PlaybookView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    Ok(Json(state.list_playbooks(query).await))
}

async fn validate_playbook<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<ValidatePlaybookRequest>,
) -> Result<Json<PlaybookValidationResult>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    Ok(Json(state.validate_playbook(request)))
}

async fn create_playbook<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<CreatePlaybookRequest>,
) -> Result<(StatusCode, Json<PlaybookView>), ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .create_playbook(request)
        .await
        .map(|view| (StatusCode::CREATED, Json(view)))
        .map_err(internal_error)
}

async fn get_playbook<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(playbook_id): AxumPath<String>,
) -> Result<Json<PlaybookView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_playbook(&playbook_id, None)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn publish_playbook<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(playbook_id): AxumPath<String>,
    Json(request): Json<PublishPlaybookRequest>,
) -> Result<Json<PlaybookView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .publish_playbook(&playbook_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn revoke_playbook<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(playbook_id): AxumPath<String>,
    Json(request): Json<RevokePlaybookRequest>,
) -> Result<Json<PlaybookView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .revoke_playbook(&playbook_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_flows<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<FlowListQuery>,
) -> Result<Json<Vec<crate::FlowView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_flows(query)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn start_flow<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<StartFlowRequest>,
) -> Result<(StatusCode, Json<crate::FlowView>), ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .start_flow(request)
        .await
        .map(|view| (StatusCode::ACCEPTED, Json(view)))
        .map_err(internal_error)
}

async fn get_flow<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(flow_id): AxumPath<String>,
) -> Result<Json<crate::FlowView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_flow(&flow_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn cancel_flow<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(flow_id): AxumPath<String>,
) -> Result<Json<crate::FlowView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .cancel_flow(&flow_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn append_flow_evidence<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(flow_id): AxumPath<String>,
    Json(request): Json<AppendFlowEvidenceRequest>,
) -> Result<Json<crate::FlowView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .append_flow_evidence(&flow_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn verify_product_view_flow<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(flow_id): AxumPath<String>,
    Json(request): Json<crate::ProductViewFlowVerificationRequest>,
) -> Result<Json<crate::ProductViewFlowVerificationVerdict>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .verify_product_view_flow(&flow_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn stream_flow_events<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(flow_id): AxumPath<String>,
    headers: HeaderMap,
    Query(query): Query<EventStreamQuery>,
) -> Result<Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let flow = state.get_flow(&flow_id).await.map_err(internal_error)?;
    let run_id = flow.run_id.ok_or_else(|| {
        ApiError::coded(
            StatusCode::CONFLICT,
            "flow",
            "flow_stream_unavailable",
            format!("flow {flow_id} has no run stream yet"),
        )
    })?;
    let cursor = event_stream_cursor(&headers, &query)?;
    let event_bus = state.event_bus();
    Ok(sse_stream(
        event_bus.subscribe_after(cursor),
        None,
        Some(run_id),
    ))
}

async fn list_observation_sources<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<ObservationSourceListQuery>,
) -> Result<Json<Vec<ObservationSourceView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_observation_sources(query.query.as_deref())
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_observation_source<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(source_id): AxumPath<String>,
) -> Result<Json<ObservationSourceView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_observation_source(&source_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn create_observation_source<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<CreateObservationSourceRequest>,
) -> Result<Json<ObservationSourceView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .create_observation_source(request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn rotate_observation_source_token<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(source_id): AxumPath<String>,
    Json(request): Json<RotateObservationSourceTokenRequest>,
) -> Result<Json<ObservationSourceView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .rotate_observation_source_token(&source_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn revoke_observation_source_token<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(source_id): AxumPath<String>,
    Json(request): Json<RevokeObservationSourceTokenRequest>,
) -> Result<Json<ObservationSourceView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .revoke_observation_source_token(&source_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_observation_audit<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<ObservationAuditListQuery>,
) -> Result<Json<Vec<ObservationAuditRecord>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_observation_audit(
            query.source_id.as_deref(),
            query.event.as_deref(),
            query.limit,
        )
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_observations<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<ObservationListQuery>,
) -> Result<Json<Vec<ObservationView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let source_id = query
        .source_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let stream_id = match query.stream_id.as_deref() {
        Some(stream_id) => {
            let stream_id = stream_id.trim();
            if stream_id.is_empty() {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "stream_id is required",
                ));
            }
            if source_id.is_none() {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "stream_id requires source_id",
                ));
            }
            Some(stream_id)
        }
        None => None,
    };
    state
        .list_observations(
            source_id,
            stream_id,
            query.after_ms,
            query.before_ms,
            query.include_purged,
        )
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_observation<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(observation_id): AxumPath<String>,
) -> Result<Json<ObservationView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_observation(&observation_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn create_observation_materialization<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<ObservationMaterializationRequest>,
) -> Result<Json<RunView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .submit_observation_materialization_run(request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn create_observation_transcript<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<ObservationTranscriptCreateRequest>,
) -> Result<Json<crate::ObservationTranscriptJobView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .create_observation_transcript_job(request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_observation_transcripts<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<ObservationTranscriptListQuery>,
) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let capture_group_id = query
        .capture_group_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let recording_id = query
        .recording_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let jobs = state
        .list_observation_transcript_jobs(capture_group_id, recording_id, query.status.clone())
        .await;
    list_or_page(
        jobs,
        &query.page_query(),
        query.limit,
        "created_at_ms,transcript_job_id",
        |job| format!("{:020}:{}", job.created_at_ms, job.transcript_job_id),
    )
}

async fn get_observation_transcript<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(transcript_job_id): AxumPath<String>,
) -> Result<Json<crate::ObservationTranscriptJobView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_observation_transcript_job(&transcript_job_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_observation_transcript_segments<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(transcript_job_id): AxumPath<String>,
    Query(query): Query<ObservationTranscriptSegmentListQuery>,
) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let segments = state
        .list_observation_transcript_segments(&transcript_job_id)
        .await
        .map_err(internal_error)?;
    list_or_page(
        segments,
        &query.page_query(),
        query.limit,
        "captured_at_ms,segment_id",
        |segment| format!("{:020}:{}", segment.captured_at_ms, segment.segment_id),
    )
}

async fn retry_observation_transcript<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(transcript_job_id): AxumPath<String>,
) -> Result<Json<crate::ObservationTranscriptJobView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .retry_observation_transcript_job(&transcript_job_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn cancel_observation_transcript<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(transcript_job_id): AxumPath<String>,
) -> Result<Json<crate::ObservationTranscriptJobView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .cancel_observation_transcript_job(&transcript_job_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn provision_capture_agents<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<CaptureAgentProvisionRequest>,
) -> Result<Json<CaptureAgentProvisionResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .provision_capture_agents(request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_capture_agents<M>(
    State(state): State<Arc<DaemonState<M>>>,
) -> Result<Json<Vec<CaptureAgentView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_capture_agents()
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_capture_agent<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(machine_id): AxumPath<String>,
) -> Result<Json<CaptureAgentView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_capture_agent(&machine_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_capture_alerts<M>(
    State(state): State<Arc<DaemonState<M>>>,
) -> Result<Json<Vec<CaptureAgentAlertView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_capture_alerts()
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn revoke_capture_agent<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(machine_id): AxumPath<String>,
    Json(request): Json<RevokeCaptureAgentRequest>,
) -> Result<Json<CaptureAgentView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .revoke_capture_agent(&machine_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_derivations<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<DerivationListQuery>,
) -> Result<Json<Vec<DerivationView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_derivations(query.query.as_deref())
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_derivation<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(derivation_id): AxumPath<String>,
) -> Result<Json<DerivationView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_derivation(&derivation_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn create_derivation<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<DerivationCreateQuery>,
    Json(request): Json<CreateDerivationRequest>,
) -> Result<Json<DerivationView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .create_derivation(
            request,
            crate::DerivationCreateControls {
                force_refresh: query.force_refresh.unwrap_or(false),
                retry_failed: query.retry_failed.unwrap_or(false),
            },
        )
        .await
        .map(Json)
        .map_err(internal_error)
}

fn learning_scope_from_query(
    scope_kind: Option<kheish_types::LearningScopeKind>,
    scope_id: Option<String>,
) -> Result<Option<kheish_types::LearningScope>, ApiError> {
    match (scope_kind, scope_id) {
        (None, None) => Ok(None),
        (Some(kheish_types::LearningScopeKind::Workspace), None) => {
            normalize_learning_scope(kheish_types::LearningScope {
                kind: kheish_types::LearningScopeKind::Workspace,
                id: kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string(),
            })
            .map(Some)
        }
        (Some(kind), Some(id)) if !id.trim().is_empty() => {
            normalize_learning_scope(kheish_types::LearningScope { kind, id }).map(Some)
        }
        (Some(_), _) => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "scope_id is required for the selected learning scope",
        )),
        (None, Some(_)) => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "scope_kind is required when scope_id is provided",
        )),
    }
}

fn normalize_learning_scope(
    scope: kheish_types::LearningScope,
) -> Result<kheish_types::LearningScope, ApiError> {
    let id = match scope.kind {
        kheish_types::LearningScopeKind::Workspace => {
            let trimmed = scope.id.trim();
            if trimmed.is_empty() {
                kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID.to_string()
            } else if trimmed != kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "workspace learning scope id must be `{}`",
                        kheish_types::DEFAULT_WORKSPACE_LEARNING_SCOPE_ID
                    ),
                ));
            } else {
                trimmed.to_string()
            }
        }
        _ => {
            let trimmed = scope.id.trim();
            if trimmed.is_empty() {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "learning scope id is required",
                ));
            }
            trimmed.to_string()
        }
    };
    Ok(kheish_types::LearningScope {
        kind: scope.kind,
        id,
    })
}

async fn list_learning_candidates<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<LearningCandidateListQuery>,
) -> Result<Json<Vec<LearningCandidateView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let scope = learning_scope_from_query(query.scope_kind, query.scope_id)?;
    state
        .list_learning_candidates(&crate::services::LearningCandidateListFilter {
            query: query.query,
            scope,
            kind: query.kind,
            state: query.state,
        })
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_learning_candidate<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(candidate_id): AxumPath<String>,
) -> Result<Json<LearningCandidateView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_learning_candidate(&candidate_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn create_learning_candidate<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<CreateLearningCandidateRequest>,
) -> Result<Json<LearningCandidateView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let candidate = LearningCandidateView {
        candidate_id: state.next_learning_candidate_id(),
        origin: crate::LearningCandidateOrigin::Api,
        scope: normalize_learning_scope(request.scope)?,
        kind: request.kind,
        sensitivity: request.sensitivity,
        content: request.content,
        confidence: request.confidence,
        source: request.source,
        evidence_refs: request.evidence_refs,
        created_at_ms: crate::now_ms(),
        expires_at_ms: request.expires_at_ms,
        state: crate::LearningCandidateState::Pending,
        automation_review: None,
        published_learning_id: None,
    };
    state
        .create_learning_candidate(candidate)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn publish_learning_candidate<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(candidate_id): AxumPath<String>,
    Json(request): Json<PublishLearningCandidateRequest>,
) -> Result<Json<LearningView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let candidate = state
        .get_learning_candidate(&candidate_id)
        .await
        .map_err(internal_error)?;
    let publish_tier = request
        .publish_tier
        .clone()
        .unwrap_or(kheish_types::LearningPublishTier::Active);
    let learning = LearningView {
        learning_id: state.next_learning_id(),
        scope: normalize_learning_scope(request.scope.unwrap_or(candidate.scope.clone()))?,
        kind: request.kind.unwrap_or(candidate.kind.clone()),
        sensitivity: request.sensitivity.unwrap_or(candidate.sensitivity.clone()),
        content: request.content.unwrap_or(candidate.content.clone()),
        confidence: request.confidence.unwrap_or(candidate.confidence),
        source: candidate.source.clone(),
        evidence_refs: if request.evidence_refs.is_empty() {
            candidate.evidence_refs.clone()
        } else {
            request.evidence_refs
        },
        source_candidate_id: None,
        created_at_ms: candidate.created_at_ms,
        published_at_ms: crate::now_ms(),
        expires_at_ms: request.expires_at_ms.or(candidate.expires_at_ms),
        status: match publish_tier {
            kheish_types::LearningPublishTier::Provisional => {
                kheish_types::LearningStatus::Provisional
            }
            kheish_types::LearningPublishTier::Active => kheish_types::LearningStatus::Active,
        },
        publish_tier,
        policy_decision: None,
        policy_actor: None,
        verification_status: kheish_types::LearningVerificationStatus::Unverified,
        supersedes: request.supersedes,
        superseded_by: None,
        revoked_at_ms: None,
        revoked_reason: None,
    };
    state
        .publish_learning_candidate(&candidate_id, learning)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn reject_learning_candidate<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(candidate_id): AxumPath<String>,
) -> Result<Json<LearningCandidateView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .reject_learning_candidate(&candidate_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_learnings<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<LearningListQuery>,
) -> Result<Json<Vec<LearningView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let scope = learning_scope_from_query(query.scope_kind, query.scope_id)?;
    state
        .list_learnings(&crate::services::LearningListFilter {
            query: query.query,
            scope,
            kind: query.kind,
            status: query.status,
            policy_decision: query.policy_decision,
            policy_actor: query.policy_actor,
            matched_rule_name: query.matched_rule_name,
        })
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_learning<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(learning_id): AxumPath<String>,
) -> Result<Json<LearningView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_learning(&learning_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn revoke_learning<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(learning_id): AxumPath<String>,
    Json(request): Json<RevokeLearningRequest>,
) -> Result<Json<LearningView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .revoke_learning(&learning_id, request.reason)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn revoke_matching_learnings<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<RevokeMatchingLearningsRequest>,
) -> Result<Json<Vec<LearningView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let scope = learning_scope_from_query(request.scope_kind, request.scope_id)?;
    state
        .revoke_matching_learnings(
            &crate::services::LearningListFilter {
                query: request.query,
                scope,
                kind: request.kind,
                status: request.status,
                policy_decision: request.policy_decision,
                policy_actor: request.policy_actor,
                matched_rule_name: request.matched_rule_name,
            },
            request.reason,
        )
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn supersede_learning<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(learning_id): AxumPath<String>,
    Json(request): Json<SupersedeLearningRequest>,
) -> Result<Json<LearningView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let current = state
        .get_learning(&learning_id)
        .await
        .map_err(internal_error)?;
    let replacement = LearningView {
        learning_id: state.next_learning_id(),
        scope: normalize_learning_scope(request.scope.unwrap_or(current.scope.clone()))?,
        kind: request.kind.unwrap_or(current.kind.clone()),
        sensitivity: request.sensitivity.unwrap_or(current.sensitivity.clone()),
        content: request.content,
        confidence: request.confidence.unwrap_or(current.confidence),
        source: current.source.clone(),
        evidence_refs: current.evidence_refs.clone(),
        source_candidate_id: None,
        created_at_ms: current.created_at_ms,
        published_at_ms: crate::now_ms(),
        expires_at_ms: request.expires_at_ms.or(current.expires_at_ms),
        status: kheish_types::LearningStatus::Active,
        publish_tier: kheish_types::LearningPublishTier::Active,
        policy_decision: Some(kheish_types::LearningPolicyDecision::Manual),
        policy_actor: None,
        verification_status: current.verification_status,
        supersedes: Some(learning_id.clone()),
        superseded_by: None,
        revoked_at_ms: None,
        revoked_reason: None,
    };
    state
        .supersede_learning(&learning_id, replacement)
        .await
        .map(Json)
        .map_err(internal_error)
}

fn learning_skill_draft_from_request(
    request: CreateLearningSkillRequest,
) -> crate::procedural_skills::LearningSkillDraft {
    crate::procedural_skills::LearningSkillDraft {
        skill_name: request.skill_name,
        description: request.description,
        when_to_use: request.when_to_use,
        version: request.version,
        instructions: request.instructions,
        runtime: kheish_skills::SkillRuntimeConfig {
            allowed_tools: request.allowed_tools,
            blocked_tools: request.blocked_tools,
            context: request.context,
            agent_profile: request
                .agent_profile
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            provider: request
                .provider
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            model: request
                .model
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            fallback_model: request
                .fallback_model
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
        },
        status: request.status.unwrap_or(crate::LearningSkillStatus::Draft),
        evidence_refs: Vec::new(),
        verification_status: kheish_types::LearningVerificationStatus::Unverified,
        successful_run_count: 0,
        distinct_session_count: 0,
        verifier_run_ids: Vec::new(),
        real_daemon_verified: false,
        last_verified_workspace_digest: None,
        canary_success_count: 0,
        canary_failure_count: 0,
    }
}

async fn promote_learning_to_skill<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(learning_id): AxumPath<String>,
    Json(request): Json<CreateLearningSkillRequest>,
) -> Result<Json<LearningSkillView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .promote_learning_to_skill(&learning_id, learning_skill_draft_from_request(request))
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_learning_skills<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<LearningSkillsListQuery>,
) -> Result<Json<Vec<LearningSkillView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let mut skills = state.list_learning_skills().await.map_err(internal_error)?;
    if let Some(source_learning_id) = query
        .source_learning_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        skills.retain(|skill| skill.source_learning_id == source_learning_id);
    }
    if let Some(status) = query.status {
        skills.retain(|skill| skill.status == status);
    }
    Ok(Json(skills))
}

async fn get_learning_skill<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(skill_name): AxumPath<String>,
) -> Result<Json<LearningSkillView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_learning_skill(&skill_name)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn record_learning_skill_rollout_result<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(skill_name): AxumPath<String>,
    Json(request): Json<LearningSkillRolloutResultRequest>,
) -> Result<Json<LearningSkillView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .record_learning_skill_rollout_result(&skill_name, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn revoke_learning_skill<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(skill_name): AxumPath<String>,
    Json(request): Json<RevokeLearningSkillRequest>,
) -> Result<Json<LearningSkillView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .revoke_learning_skill(&skill_name, request.reason)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn rollback_learning_skill<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(skill_name): AxumPath<String>,
    Json(request): Json<RollbackLearningSkillRequest>,
) -> Result<Json<LearningSkillView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .rollback_learning_skill(&skill_name, request.reason)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_skills<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<SkillListQuery>,
) -> Result<Json<Vec<SkillSummaryView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_skills(query.query.as_deref())
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_skill<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(skill_name): AxumPath<String>,
) -> Result<Json<SkillView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_skill(&skill_name)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn set_model<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<SetModelRequest>,
) -> Result<Json<RuntimeSettingsView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_model(request.provider, request.model, request.expected_revision)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn set_permission_mode<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<SetPermissionModeRequest>,
) -> Result<Json<RuntimeSettingsView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_permission_mode(request.mode, request.expected_revision)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn check_permission<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<CheckPermissionRequest>,
) -> Result<Json<kheish_runtime::PermissionExplanation>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .check_permission(request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn check_permission_matrix<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<CheckPermissionMatrixRequest>,
) -> Result<Json<crate::PermissionMatrixView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .check_permission_matrix(request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn set_system_prompt<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<SetSystemPromptRequest>,
) -> Result<Json<RuntimeSettingsView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_system_prompt(request.settings, request.expected_revision)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_hooks<M>(State(state): State<Arc<DaemonState<M>>>) -> Json<HookSettings>
where
    M: ModelDriver + Send + Sync + 'static,
{
    Json(state.hook_settings_snapshot().await)
}

async fn list_hook_dead_letters<M>(
    State(state): State<Arc<DaemonState<M>>>,
) -> Result<Json<Vec<HookDeadLetterView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .hook_dead_letters_snapshot()
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn resolve_hook_dead_letter<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(dead_letter_id): AxumPath<String>,
    Json(request): Json<ResolveHookDeadLetterRequest>,
) -> Result<Json<HookDeadLetterView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let reason = request.reason.as_deref().unwrap_or("operator resolved");
    match state
        .resolve_hook_dead_letter(&dead_letter_id, reason)
        .await
        .map_err(internal_error)?
    {
        Some(view) => Ok(Json(view)),
        None => Err(ApiError::coded(
            StatusCode::NOT_FOUND,
            "runtime",
            "unknown_hook_dead_letter",
            format!("unknown hook dead-letter record {dead_letter_id}"),
        )),
    }
}

async fn set_hooks<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<SetHooksRequest>,
) -> Result<Json<RuntimeSettingsView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_hooks(
            request.settings,
            request.expected_revision,
            request.skip_hooks,
        )
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn set_debug_level<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<SetDebugLevelRequest>,
) -> Result<Json<RuntimeSettingsView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_debug_level(request.level, request.expected_revision)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_tool_runtime_limits<M>(
    State(state): State<Arc<DaemonState<M>>>,
) -> Json<kheish_runtime::ToolRuntimeLimits>
where
    M: ModelDriver + Send + Sync + 'static,
{
    Json(state.runtime_settings().await.tool_runtime_limits)
}

async fn set_tool_runtime_limits<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<SetToolRuntimeLimitsRequest>,
) -> Result<Json<RuntimeSettingsView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_tool_runtime_limits(request.limits, request.expected_revision)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn set_learning_policy<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<SetLearningPolicyRequest>,
) -> Result<Json<RuntimeSettingsView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_learning_policy(request.policy, request.expected_revision)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_run_memory_policy<M>(
    State(state): State<Arc<DaemonState<M>>>,
) -> Json<crate::RunMemoryPolicyConfig>
where
    M: ModelDriver + Send + Sync + 'static,
{
    Json(state.runtime_settings().await.run_memory_policy)
}

async fn set_run_memory_policy<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<SetRunMemoryPolicyRequest>,
) -> Result<Json<RuntimeSettingsView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_run_memory_policy(request.policy, request.expected_revision)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn stream_all_events<M>(
    State(state): State<Arc<DaemonState<M>>>,
    headers: HeaderMap,
    Query(query): Query<EventStreamQuery>,
) -> Result<Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let cursor = event_stream_cursor(&headers, &query)?;
    let event_bus = state.event_bus();
    Ok(sse_stream(
        event_bus.subscribe_after(cursor),
        query.session_id,
        query.run_id,
    ))
}

async fn list_personas<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<PersonaListQuery>,
) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let filter = query
        .query
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let personas = state
        .list_persona_records()
        .await
        .into_iter()
        .filter(|persona| {
            filter.is_none_or(|filter| {
                persona.persona_id.contains(filter) || persona.display_name.contains(filter)
            })
        })
        .map(PersonaSummaryView::from)
        .collect::<Vec<_>>();
    let page = query.page_query();
    list_or_page(personas, &page, query.limit, "persona_id_asc", |persona| {
        persona.persona_id.clone()
    })
}

async fn create_persona<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<CreatePersonaRequest>,
) -> Result<(StatusCode, Json<PersonaView>), ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .create_persona_record(
            request.persona_id,
            request.display_name,
            request.soul,
            request.capability_scope.unwrap_or_default(),
            request.default_skills.unwrap_or_default(),
            request.metadata.unwrap_or(Value::Null),
        )
        .await
        .map(PersonaView::from)
        .map(|view| (StatusCode::CREATED, Json(view)))
        .map_err(internal_error)
}

async fn get_persona<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(persona_id): AxumPath<String>,
) -> Result<Json<PersonaView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_persona_record(&persona_id)
        .await
        .map(PersonaView::from)
        .map(Json)
        .map_err(internal_error)
}

async fn update_persona<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(persona_id): AxumPath<String>,
    Json(request): Json<UpdatePersonaRequest>,
) -> Result<Json<PersonaView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .update_persona_record(
            &persona_id,
            request.display_name,
            request.soul,
            request.capability_scope,
            request.default_skills,
            request.metadata,
        )
        .await
        .map(PersonaView::from)
        .map(Json)
        .map_err(internal_error)
}

async fn list_sessions<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<SessionListQuery>,
) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let sessions = state
        .list_sessions(query.persona_id.as_deref())
        .await
        .map_err(internal_error)?;
    let page = query.page_query();
    list_or_page(sessions, &page, query.limit, "session_id_asc", |session| {
        session.session_id.clone()
    })
}

async fn create_session<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<SessionView>), ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .create_session(request)
        .await
        .map(|view| (StatusCode::CREATED, Json(view)))
        .map_err(internal_error)
}

async fn get_session<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let agent_id = state
        .agent_id_for_session(&session_id)
        .await
        .map_err(internal_error)?;
    state
        .session_view(&session_id, &agent_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_session_permission_audits<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<crate::SessionPermissionAuditListView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .session_permission_audits(&session_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_session_goal<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<SessionGoalResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .session_goal_response(&session_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn set_session_goal<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<SetSessionGoalRequest>,
) -> Result<Json<SessionGoalResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_goal(&session_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn create_session_goal<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<SetSessionGoalRequest>,
) -> Result<Json<SessionGoalResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .create_session_goal_from_request(&session_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn patch_session_goal<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<PatchSessionGoalRequest>,
) -> Result<Json<SessionGoalResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .patch_session_goal(&session_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn clear_session_goal<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<SessionGoalResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .clear_session_goal(&session_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_session_memory_context<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Query(query): Query<SessionMemoryContextQuery>,
) -> Result<Json<SessionMemoryContextView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .session_memory_context_view(&session_id, query.query.as_deref())
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_session_memory_search<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Query(query): Query<SessionMemorySearchQuery>,
) -> Result<Json<SessionMemorySearchView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .session_memory_search_view(&session_id, query.query.as_deref(), query.limit)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_session_skills<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Query(query): Query<SkillListQuery>,
) -> Result<Json<Vec<SkillSummaryView>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .list_session_skills(&session_id, query.query.as_deref())
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn set_session_persona<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<SetSessionPersonaRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_persona_view(&session_id, &request.persona_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn replace_session_persona<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<SetSessionPersonaRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_persona_view(&session_id, &request.persona_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn clear_session_persona<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .clear_session_persona_view(&session_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn set_session_route_policy<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<SetSessionRoutePolicyRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_route_policy(&session_id, request.route_policy)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn replace_session_route_policy<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<SetSessionRoutePolicyRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_route_policy(&session_id, request.route_policy)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn clear_session_route_policy<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_route_policy(&session_id, None)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn set_session_capability_scope<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<SetSessionCapabilityScopeRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_capability_scope(&session_id, request.capability_scope)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn replace_session_capability_scope<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<SetSessionCapabilityScopeRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_capability_scope(&session_id, request.capability_scope)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn clear_session_capability_scope<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_capability_scope(&session_id, None)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn set_session_credential_scope<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<SetSessionCredentialScopeRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_credential_scope(&session_id, request.credential_scope)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn replace_session_credential_scope<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<SetSessionCredentialScopeRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_credential_scope(&session_id, request.credential_scope)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn clear_session_credential_scope<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_credential_scope(&session_id, None)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_session_operator_config<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<SessionOperatorConfigView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .agent_id_for_session(&session_id)
        .await
        .map_err(internal_error)?;
    state
        .load_session_operator_config(&session_id)
        .await
        .map(|operator| Json(SessionOperatorConfigView { operator }))
        .map_err(internal_error)
}

async fn set_session_operator_config<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<SetSessionOperatorConfigRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_operator_config(&session_id, Some(request.operator))
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn replace_session_operator_config<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<SetSessionOperatorConfigRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_operator_config(&session_id, Some(request.operator))
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn clear_session_operator_config<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_operator_config(&session_id, None)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_session_reply_targets<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<SessionReplyTargetsView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .agent_id_for_session(&session_id)
        .await
        .map_err(internal_error)?;
    Ok(Json(SessionReplyTargetsView {
        reply_targets: state.session_reply_targets(&session_id).await,
    }))
}

async fn set_session_reply_targets<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<SetSessionReplyTargetsRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let reply_targets = build_session_reply_targets(state.as_ref(), request.reply_targets)
        .await
        .map_err(internal_error)?;
    state
        .set_session_reply_targets(&session_id, reply_targets)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn replace_session_reply_targets<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<SetSessionReplyTargetsRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let reply_targets = build_session_reply_targets(state.as_ref(), request.reply_targets)
        .await
        .map_err(internal_error)?;
    state
        .set_session_reply_targets(&session_id, reply_targets)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn clear_session_reply_targets<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_session_reply_targets(&session_id, Vec::new())
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_session_events<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<SessionEventLogView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .session_events(&session_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn stream_session_events<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    headers: HeaderMap,
    Query(query): Query<EventStreamQuery>,
) -> Result<Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .agent_id_for_session(&session_id)
        .await
        .map_err(internal_error)?;
    let cursor = event_stream_cursor(&headers, &query)?;
    let event_bus = state.event_bus();
    Ok(sse_stream(
        event_bus.subscribe_after(cursor),
        Some(session_id),
        None,
    ))
}

async fn submit_input<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<SubmitInputRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .submit_input(&session_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_tasks<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Query(query): Query<TaskListQuery>,
) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let session_state = state
        .load_session_control_state(&session_id)
        .await
        .map_err(internal_error)?;
    let mut all_tasks = session_state.tasks;
    // Archived terminal tasks stay visible: the API list must not shrink
    // because the daemon compacted its hot state.
    if !session_state.archived_tasks.is_empty() {
        all_tasks.extend(crate::services::archived_terminal_tasks(
            state
                .load_archived_session_tasks(&session_id)
                .await
                .map_err(internal_error)?,
        ));
        all_tasks.sort_by_key(|task| task.created_at_ms);
    }
    let tasks = all_tasks
        .into_iter()
        .filter(|task| {
            query
                .status
                .as_ref()
                .map(|status| status == &task.status)
                .unwrap_or(true)
        })
        .collect();
    let page = query.page_query();
    list_or_page(tasks, &page, query.limit, "task_id_asc", |task| {
        task.id.clone()
    })
}

async fn list_schedules<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<ScheduleListQuery>,
) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let schedules = state
        .list_schedules(query.session_id.as_deref())
        .await
        .map_err(internal_error)?;
    let page = query.page_query();
    list_or_page(
        schedules,
        &page,
        query.limit,
        "schedule_id_asc",
        |schedule| schedule.schedule_id.clone(),
    )
}

async fn create_schedule<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<CreateScheduleRequest>,
) -> Result<(StatusCode, Json<crate::ScheduleView>), ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .create_schedule(request)
        .await
        .map(|view| (StatusCode::CREATED, Json(view)))
        .map_err(internal_error)
}

async fn get_schedule<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(schedule_id): AxumPath<String>,
) -> Result<Json<crate::ScheduleView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_schedule(&schedule_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn cancel_schedule<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(schedule_id): AxumPath<String>,
) -> Result<Json<ScheduleMutationResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .cancel_schedule(&schedule_id)
        .await
        .map(|schedule| Json(ScheduleMutationResponse { schedule }))
        .map_err(internal_error)
}

async fn pause_schedule<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(schedule_id): AxumPath<String>,
) -> Result<Json<ScheduleMutationResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .pause_schedule(&schedule_id)
        .await
        .map(|schedule| Json(ScheduleMutationResponse { schedule }))
        .map_err(internal_error)
}

async fn resume_schedule<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(schedule_id): AxumPath<String>,
) -> Result<Json<ScheduleMutationResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .resume_schedule(&schedule_id)
        .await
        .map(|schedule| Json(ScheduleMutationResponse { schedule }))
        .map_err(internal_error)
}

async fn trigger_schedule<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(schedule_id): AxumPath<String>,
) -> Result<Json<ScheduleMutationResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .trigger_schedule_now(&schedule_id)
        .await
        .map(|schedule| Json(ScheduleMutationResponse { schedule }))
        .map_err(internal_error)
}

async fn get_task<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((session_id, task_id)): AxumPath<(String, String)>,
) -> Result<Json<kheish_types::TaskRecord>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let session_state = state
        .load_session_control_state(&session_id)
        .await
        .map_err(internal_error)?;
    let task = match session_state
        .tasks
        .into_iter()
        .find(|task| task.id == task_id)
    {
        Some(task) => task,
        // Terminal tasks move to the archive; they stay readable here.
        None => state
            .find_archived_session_task(&session_id, &task_id)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| internal_error(anyhow!("unknown task {task_id}")))?,
    };
    Ok(Json(task))
}

async fn get_task_output<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((session_id, task_id)): AxumPath<(String, String)>,
    Query(query): Query<TaskOutputQuery>,
) -> Result<Json<TaskOutputView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .task_output_view(
            &session_id,
            &task_id,
            query.wait.unwrap_or(false),
            Duration::from_millis(query.timeout_ms.unwrap_or(0)),
            query
                .tail_bytes
                .unwrap_or(crate::shell_tasks::DEFAULT_TASK_OUTPUT_TAIL_BYTES)
                .min(crate::shell_tasks::MAX_TASK_OUTPUT_TAIL_BYTES),
            query.full.unwrap_or(false),
        )
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn stop_task<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((session_id, task_id)): AxumPath<(String, String)>,
    Json(request): Json<StopTaskRequest>,
) -> Result<Json<kheish_types::TaskRecord>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .stop_session_task(&session_id, &task_id, request.reason, None)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn submit_input_run<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<SubmitRunRequest>,
) -> Result<(StatusCode, Json<RunView>), ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let idempotency_key = direct_run_idempotency_key(&headers, request.idempotency_key.as_deref())
        .map_err(internal_error)?;
    let run = match idempotency_key {
        Some(key) => {
            state
                .submit_input_run_idempotent(&session_id, request.request, &key)
                .await
        }
        None => state.submit_input_run(&session_id, request.request).await,
    }
    .map_err(internal_error)?;
    Ok((StatusCode::ACCEPTED, Json(run)))
}

fn direct_run_idempotency_key(
    headers: &HeaderMap,
    body_key: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let header_key = headers
        .get("idempotency-key")
        .map(|value| {
            value.to_str().map_err(|_| {
                anyhow::Error::from(DaemonProblem::invalid_idempotency_key(
                    "Idempotency-Key header must be valid ASCII",
                ))
            })
        })
        .transpose()?
        .map(|value| value.trim().to_string());
    let body_key = body_key.map(|value| value.trim().to_string());
    if header_key.as_deref().is_some_and(str::is_empty)
        || body_key.as_deref().is_some_and(str::is_empty)
    {
        return Err(
            DaemonProblem::invalid_idempotency_key("idempotency key cannot be empty").into(),
        );
    }
    if let (Some(header_key), Some(body_key)) = (header_key.as_deref(), body_key.as_deref())
        && header_key != body_key
    {
        return Err(DaemonProblem::invalid_idempotency_key(
            "Idempotency-Key header and idempotency_key body field differ",
        )
        .into());
    }
    Ok(header_key.or(body_key))
}

async fn resolve_approvals<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    headers: HeaderMap,
    Json(mut request): Json<ResolveApprovalsRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    request.idempotency_key =
        direct_run_idempotency_key(&headers, request.idempotency_key.as_deref())
            .map_err(internal_error)?;
    state
        .resolve_approvals(&session_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_session_questions<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Query(query): Query<PendingQuestionListQuery>,
) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let questions = state
        .list_pending_questions(Some(&session_id))
        .await
        .map_err(internal_error)?;
    let page = query.page_query();
    list_or_page(
        questions,
        &page,
        query.limit,
        "session_id_asc,request_id_asc",
        |question| format!("{}:{}", question.session_id, question.request.id),
    )
}

async fn list_questions<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<PendingQuestionListQuery>,
) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let questions = state
        .list_pending_questions(query.session_id.as_deref())
        .await
        .map_err(internal_error)?;
    let page = query.page_query();
    list_or_page(
        questions,
        &page,
        query.limit,
        "session_id_asc,request_id_asc",
        |question| format!("{}:{}", question.session_id, question.request.id),
    )
}

async fn resolve_user_question<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    headers: HeaderMap,
    Json(mut request): Json<ResolveUserQuestionRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    request.idempotency_key =
        direct_run_idempotency_key(&headers, request.idempotency_key.as_deref())
            .map_err(internal_error)?;
    state
        .resolve_user_question_for_session(&session_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn resolve_approval_run<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    headers: HeaderMap,
    Json(mut request): Json<ResolveApprovalsRequest>,
) -> Result<(StatusCode, Json<RunView>), ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    request.idempotency_key =
        direct_run_idempotency_key(&headers, request.idempotency_key.as_deref())
            .map_err(internal_error)?;
    state
        .resolve_approval_run(&session_id, request)
        .await
        .map(|run| (StatusCode::ACCEPTED, Json(run)))
        .map_err(internal_error)
}

async fn resolve_run_user_question<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(run_id): AxumPath<String>,
    headers: HeaderMap,
    Json(mut request): Json<ResolveUserQuestionRequest>,
) -> Result<(StatusCode, Json<RunView>), ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    request.idempotency_key =
        direct_run_idempotency_key(&headers, request.idempotency_key.as_deref())
            .map_err(internal_error)?;
    state
        .resolve_user_question_run(&run_id, request)
        .await
        .map(|run| (StatusCode::ACCEPTED, Json(run)))
        .map_err(internal_error)
}

async fn resolve_run_approvals<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(run_id): AxumPath<String>,
    headers: HeaderMap,
    Json(mut request): Json<ResolveApprovalsRequest>,
) -> Result<(StatusCode, Json<RunView>), ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    request.idempotency_key =
        direct_run_idempotency_key(&headers, request.idempotency_key.as_deref())
            .map_err(internal_error)?;
    state
        .resolve_run_approvals(&run_id, request)
        .await
        .map(|run| (StatusCode::ACCEPTED, Json(run)))
        .map_err(internal_error)
}

async fn interrupt_session<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<InterruptSessionResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .interrupt_session(&session_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn end_session<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<EndSessionRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .end_session(&session_id, request.reason)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_runs<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<RunListQuery>,
) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let mut runs = state
        .list_runs(query.session_id.as_deref())
        .await
        .map_err(internal_error)?;

    let page = query.page_query();
    if page.enabled() {
        return list_or_page(
            runs,
            &page,
            query.limit,
            "submitted_at_ms_asc,run_id_asc",
            run_page_key,
        );
    }

    if let Some(limit) = query.limit {
        let limit = normalize_page_limit(Some(limit))?;

        if query.priority_active.unwrap_or(false) {
            runs.sort_by(|left, right| {
                let left_active = !left.status.is_terminal();
                let right_active = !right.status.is_terminal();

                right_active
                    .cmp(&left_active)
                    .then_with(|| right.updated_at_ms.cmp(&left.updated_at_ms))
                    .then_with(|| right.submitted_at_ms.cmp(&left.submitted_at_ms))
            });
            runs.truncate(limit);
        } else if runs.len() > limit {
            runs = runs.split_off(runs.len() - limit);
        }
    }

    json_value(runs)
}

async fn prune_runs<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<RunRetentionPruneRequest>,
) -> Result<Json<RunRetentionPruneResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .prune_runs(request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_run<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(run_id): AxumPath<String>,
) -> Result<Json<RunView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_run(&run_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_run_external_actions<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(run_id): AxumPath<String>,
) -> Result<Json<Vec<crate::ExternalActionAuditRecord>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .run_external_actions(&run_id)
        .map(Json)
        .map_err(internal_error)
}

async fn get_run_debug<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(run_id): AxumPath<String>,
) -> Result<Json<RunDebugView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .run_debug_view(&run_id)
        .map(Json)
        .map_err(internal_error)
}

async fn get_run_debug_artifact<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((run_id, artifact_id)): AxumPath<(String, String)>,
) -> Result<String, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .run_debug_artifact(&run_id, &artifact_id)
        .map_err(internal_error)
}

async fn get_run_events<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(run_id): AxumPath<String>,
) -> Result<Json<Vec<RunEventEntry>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state.run_events(&run_id).map(Json).map_err(internal_error)
}

async fn stream_run_events<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(run_id): AxumPath<String>,
    headers: HeaderMap,
    Query(query): Query<EventStreamQuery>,
) -> Result<Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state.get_run(&run_id).await.map_err(internal_error)?;
    let cursor = event_stream_cursor(&headers, &query)?;
    let event_bus = state.event_bus();
    Ok(sse_stream(
        event_bus.subscribe_after(cursor),
        None,
        Some(run_id),
    ))
}

async fn cancel_run<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(run_id): AxumPath<String>,
) -> Result<Json<RunView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .cancel_run(&run_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn cancel_run_question<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((run_id, request_id)): AxumPath<(String, String)>,
    headers: HeaderMap,
    request: Option<Json<CancelUserQuestionRequest>>,
) -> Result<Json<RunView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let mut request = request.map(|Json(request)| request).unwrap_or_default();
    request.idempotency_key =
        direct_run_idempotency_key(&headers, request.idempotency_key.as_deref())
            .map_err(internal_error)?;
    state
        .cancel_user_question_run(&run_id, &request_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn list_deliveries<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<DeliveryListQuery>,
) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let deliveries = state
        .list_deliveries(delivery_filter_from_query(&query, None))
        .await
        .map_err(internal_error)?;
    let page = query.page_query();
    list_or_page(
        deliveries,
        &page,
        query.limit,
        "delivery_time_asc,delivery_id_asc",
        delivery_page_key,
    )
}

async fn list_dead_letter_deliveries<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<DeliveryListQuery>,
) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let deliveries = state
        .list_deliveries(delivery_filter_from_query(
            &query,
            Some(crate::DeliveryStatus::DeadLettered),
        ))
        .await
        .map_err(internal_error)?;
    let page = query.page_query();
    list_or_page(
        deliveries,
        &page,
        query.limit,
        "delivery_time_asc,delivery_id_asc",
        delivery_page_key,
    )
}

async fn get_delivery<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(delivery_id): AxumPath<String>,
) -> Result<Json<crate::DeliveryView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    match state
        .get_delivery(&delivery_id)
        .await
        .map_err(internal_error)?
    {
        Some(delivery) => Ok(Json(delivery)),
        None => Err(ApiError::coded(
            StatusCode::NOT_FOUND,
            "deliveries",
            "delivery_not_found",
            format!("unknown delivery {delivery_id}"),
        )),
    }
}

async fn replay_delivery<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(delivery_id): AxumPath<String>,
    Query(query): Query<DeliveryReplayQuery>,
) -> Result<Json<crate::DeliveryReplayResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    match state
        .replay_dead_letter_delivery(&delivery_id, query.force)
        .await
        .map_err(internal_error)?
    {
        Some(response) => Ok(Json(response)),
        None => Err(ApiError::coded(
            StatusCode::NOT_FOUND,
            "deliveries",
            "delivery_not_found",
            format!("unknown dead-letter delivery {delivery_id}"),
        )),
    }
}

async fn bulk_replay_deliveries<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<DeliveryBulkReplayRequest>,
) -> Result<Json<crate::DeliveryBulkReplayResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .bulk_replay_dead_letter_deliveries(
            crate::delivery::DeliveryListFilter {
                session_id: request.session_id,
                run_id: request.run_id,
                plugin: request.plugin,
                status: Some(crate::DeliveryStatus::DeadLettered),
            },
            request.force,
            request.dry_run,
            request.unresolved_only,
            request.limit,
        )
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn resolve_delivery<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(delivery_id): AxumPath<String>,
    Json(request): Json<DeliveryResolveRequest>,
) -> Result<Json<crate::DeliveryView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let reason = request.reason.as_deref().unwrap_or("operator resolved");
    match state
        .resolve_dead_letter_delivery(&delivery_id, reason)
        .await
        .map_err(internal_error)?
    {
        Some(response) => Ok(Json(response)),
        None => Err(ApiError::coded(
            StatusCode::NOT_FOUND,
            "deliveries",
            "delivery_not_found",
            format!("unknown dead-letter delivery {delivery_id}"),
        )),
    }
}

async fn reset_delivery_backpressure<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<DeliveryBackpressureResetRequest>,
) -> Result<Json<crate::DeliveryBackpressureResetResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let target = request
        .target
        .as_deref()
        .map(str::trim)
        .filter(|target| !target.is_empty());
    let plugin = request
        .plugin
        .as_deref()
        .map(str::trim)
        .filter(|plugin| !plugin.is_empty());
    if target.is_none() && plugin.is_none() {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "deliveries",
            "delivery_backpressure_selector_required",
            "target or plugin is required",
        ));
    }
    state
        .reset_delivery_backpressure(target, plugin, request.dry_run)
        .await
        .map(Json)
        .map_err(internal_error)
}

fn delivery_filter_from_query(
    query: &DeliveryListQuery,
    forced_status: Option<crate::DeliveryStatus>,
) -> crate::delivery::DeliveryListFilter {
    crate::delivery::DeliveryListFilter {
        session_id: query.session_id.clone(),
        run_id: query.run_id.clone(),
        plugin: query.plugin.clone(),
        status: forced_status.or(query.status),
    }
}

async fn list_agents<M>(
    State(state): State<Arc<DaemonState<M>>>,
) -> Result<Json<Vec<ManagedAgentSnapshot>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state.list_agents().await.map(Json).map_err(internal_error)
}

async fn list_agent_summaries<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<AgentSummaryListQuery>,
) -> Result<Json<Value>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let summaries = match query.root_agent_id.as_deref() {
        Some(root_agent_id) => state
            .list_agent_summaries_for_root(&AgentId(root_agent_id.to_string()))
            .await
            .map_err(internal_error)?,
        None => state.list_agent_summaries().await,
    };
    let total_count = summaries.len();
    let summaries = summaries
        .into_iter()
        .filter(|summary| query.matches_summary(summary))
        .collect::<Vec<_>>();
    let counts = AgentSummaryCountsView::from_summaries(total_count, &summaries);
    let page = query.page_query();
    if !page.enabled() {
        if let Some(limit) = query.limit {
            let limit = normalize_page_limit(Some(limit))?;
            let mut summaries = summaries;
            summaries.sort_by_key(|summary| summary.agent_id.clone());
            summaries.truncate(limit);
            return json_value(summaries);
        }
        return json_value(summaries);
    }
    let page = paginate_items(summaries, &page, query.limit, "agent_id_asc", |agent| {
        agent.agent_id.clone()
    })?;
    json_value(AgentSummaryListPage {
        items: page.items,
        pagination: page.pagination,
        counts,
    })
}

async fn list_agent_audit<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Query(query): Query<AgentAuditListQuery>,
) -> Result<Json<Vec<AgentSupervisorAuditEntry>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    Ok(Json(
        state.agent_supervisor_audit(query.agent_id.as_deref()),
    ))
}

async fn get_agent<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(agent_id): AxumPath<String>,
) -> Result<Json<ManagedAgentSnapshot>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .get_agent(&agent_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_agent_audit<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(agent_id): AxumPath<String>,
) -> Result<Json<Vec<AgentSupervisorAuditEntry>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    Ok(Json(state.agent_supervisor_audit(Some(&agent_id))))
}

async fn set_agent_nickname<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(agent_id): AxumPath<String>,
    Json(request): Json<SetAgentNicknameRequest>,
) -> Result<Json<ManagedAgentSnapshot>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let nickname = request.nickname.trim();
    if nickname.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "nickname is required",
        ));
    }
    state
        .set_agent_nickname(&agent_id, Some(nickname.to_string()))
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn replace_agent_nickname<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(agent_id): AxumPath<String>,
    Json(request): Json<SetAgentNicknameRequest>,
) -> Result<Json<ManagedAgentSnapshot>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    set_agent_nickname(State(state), AxumPath(agent_id), Json(request)).await
}

async fn clear_agent_nickname<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(agent_id): AxumPath<String>,
) -> Result<Json<ManagedAgentSnapshot>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .set_agent_nickname(&agent_id, None)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn spawn_sidechain<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(agent_id): AxumPath<String>,
    headers: HeaderMap,
    Json(mut request): Json<SpawnSidechainRequest>,
) -> Result<Json<SessionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    request.spawn_request_id =
        direct_run_idempotency_key(&headers, request.spawn_request_id.as_deref())
            .map_err(internal_error)?;
    state
        .spawn_sidechain(&agent_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn explain_sidechain_spawn<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(agent_id): AxumPath<String>,
    headers: HeaderMap,
    Json(mut request): Json<SpawnSidechainRequest>,
) -> Result<Json<crate::SubagentPolicyDecisionView>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    request.spawn_request_id =
        direct_run_idempotency_key(&headers, request.spawn_request_id.as_deref())
            .map_err(internal_error)?;
    state
        .explain_sidechain_spawn(&agent_id, request)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn post_mailbox<M>(
    State(state): State<Arc<DaemonState<M>>>,
    Json(request): Json<PostMailboxRequest>,
) -> Result<(StatusCode, Json<PostMailboxResponse>), ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .post_mailbox(request)
        .await
        .map(|response| (StatusCode::ACCEPTED, Json(response)))
        .map_err(internal_error)
}

async fn drain_mailbox<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(agent_id): AxumPath<String>,
) -> Result<Json<Vec<MailboxMessage>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .drain_mailbox(&agent_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_mailbox_dead_letters<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(agent_id): AxumPath<String>,
) -> Result<Json<Vec<MailboxMessage>>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .mailbox_dead_letters(&agent_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn ack_mailbox_message<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((agent_id, message_id)): AxumPath<(String, String)>,
) -> Result<Json<AckMailboxResponse>, ApiError>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state
        .ack_mailbox_message(&agent_id, &message_id)
        .await
        .map(Json)
        .map_err(internal_error)
}

#[derive(Clone, Debug)]
struct ResolvedConnectorSecretInput {
    inline: Option<String>,
    env: Option<String>,
    secret_ref: Option<String>,
    pending_write: Option<ConnectorSecretWrite>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ConnectorSecretWrite {
    secret_ref: String,
    value: String,
}

#[derive(Clone, Debug)]
struct ConnectorSecretRollback {
    secret_ref: String,
    previous_value: Option<String>,
}

#[derive(Clone, Debug)]
struct BuiltConnectorConfig<T> {
    config: T,
    secret_writes: Vec<ConnectorSecretWrite>,
}

fn payload_contains_field(payload: &serde_json::Map<String, Value>, field: &str) -> bool {
    payload.contains_key(field)
}

fn normalized_optional_string(
    value: Option<String>,
    _field: &str,
) -> Result<Option<String>, anyhow::Error> {
    match value {
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        None => Ok(None),
    }
}

fn normalized_string_vec(values: Vec<String>, _field: &str) -> Result<Vec<String>, anyhow::Error> {
    let mut values = values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    Ok(values)
}

async fn resolve_connector_secret_input<M>(
    state: &DaemonState<M>,
    kind: &str,
    name: &str,
    field: &str,
    input: Option<super::types::ConnectorSecretInput>,
    current_inline: Option<String>,
    current_env: Option<String>,
    current_secret_ref: Option<String>,
) -> Result<ResolvedConnectorSecretInput, anyhow::Error>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let Some(input) = input else {
        return Ok(ResolvedConnectorSecretInput {
            inline: current_inline,
            env: current_env,
            secret_ref: current_secret_ref,
            pending_write: None,
        });
    };
    let value = normalized_optional_string(input.value, &format!("{field}.value"))?;
    let env = normalized_optional_string(input.env, &format!("{field}.env"))?;
    let secret_ref = normalized_optional_string(input.secret_ref, &format!("{field}.secret_ref"))?;
    if env.is_some() && (value.is_some() || secret_ref.is_some()) {
        bail!(
            "connector secret input `{field}` must use either `env` or `secret_ref`/`value`, not both"
        );
    }
    if let Some(value) = value {
        let secret_ref = secret_ref.unwrap_or_else(|| format!("connectors.{kind}.{name}.{field}"));
        return Ok(ResolvedConnectorSecretInput {
            inline: None,
            env: None,
            secret_ref: Some(secret_ref.clone()),
            pending_write: Some(ConnectorSecretWrite { secret_ref, value }),
        });
    }
    if let Some(env) = env {
        return Ok(ResolvedConnectorSecretInput {
            inline: None,
            env: Some(env),
            secret_ref: None,
            pending_write: None,
        });
    }
    if let Some(secret_ref) = secret_ref {
        let status = state.auth_status(&secret_ref).await?;
        anyhow::ensure!(
            status.provider == kheish_auth::AuthProvider::Generic,
            "connector secret ref `{secret_ref}` must reference a generic opaque secret slot"
        );
        return Ok(ResolvedConnectorSecretInput {
            inline: None,
            env: None,
            secret_ref: Some(secret_ref),
            pending_write: None,
        });
    }
    Ok(ResolvedConnectorSecretInput {
        inline: None,
        env: None,
        secret_ref: None,
        pending_write: None,
    })
}

fn push_connector_secret_write(
    writes: &mut Vec<ConnectorSecretWrite>,
    pending: Option<ConnectorSecretWrite>,
) -> Result<(), anyhow::Error> {
    let Some(pending) = pending else {
        return Ok(());
    };
    if let Some(existing) = writes
        .iter()
        .find(|existing| existing.secret_ref == pending.secret_ref)
    {
        anyhow::ensure!(
            existing.value == pending.value,
            "connector secret input references `{}` with conflicting values in one request",
            pending.secret_ref
        );
        return Ok(());
    }
    writes.push(pending);
    Ok(())
}

async fn apply_connector_secret_writes<M>(
    state: &DaemonState<M>,
    writes: &[ConnectorSecretWrite],
) -> Result<Vec<ConnectorSecretRollback>, anyhow::Error>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let mut applied = Vec::with_capacity(writes.len());
    for write in writes {
        let previous_value = state.generic_secret_value(&write.secret_ref)?;
        if let Err(error) = state
            .put_generic_secret_without_reload(&write.secret_ref, write.value.clone())
            .await
        {
            rollback_connector_secret_writes(state, applied).await?;
            return Err(error);
        }
        applied.push(ConnectorSecretRollback {
            secret_ref: write.secret_ref.clone(),
            previous_value,
        });
    }
    Ok(applied)
}

async fn rollback_connector_secret_writes<M>(
    state: &DaemonState<M>,
    applied: Vec<ConnectorSecretRollback>,
) -> Result<(), anyhow::Error>
where
    M: ModelDriver + Send + Sync + 'static,
{
    for rollback in applied.into_iter().rev() {
        match rollback.previous_value {
            Some(previous_value) => {
                state
                    .put_generic_secret_without_reload(&rollback.secret_ref, previous_value)
                    .await?;
            }
            None => {
                let _ = state
                    .delete_auth_slot_without_reload(&rollback.secret_ref)
                    .await?;
            }
        }
    }
    state.reload_connectors().await?;
    Ok(())
}

async fn notify_connector_secret_writes<M>(state: &DaemonState<M>, writes: &[ConnectorSecretWrite])
where
    M: ModelDriver + Send + Sync + 'static,
{
    for write in writes {
        state
            .note_secret_ref_changed_without_connector_reload(&write.secret_ref, true)
            .await;
    }
}

fn connector_record_as_telegram(
    record: Option<ConnectorConfigRecord>,
    name: &str,
) -> Result<crate::TelegramConnectorConfig, anyhow::Error> {
    match record {
        Some(ConnectorConfigRecord::Telegram { config, .. }) => Ok(config),
        Some(_) => bail!("connector kind mismatch for telegram/{name}"),
        None => Ok(crate::TelegramConnectorConfig {
            name: name.to_string(),
            bot_token: None,
            bot_token_env: None,
            bot_token_secret_ref: None,
            secret_token: None,
            secret_token_env: None,
            secret_token_secret_ref: None,
            allow_unauthenticated_ingress: false,
            api_base_url: None,
            ingress_mode: crate::TelegramIngressMode::default(),
            polling_timeout_seconds: 30,
            ingress_events_per_second:
                crate::connectors::default_telegram_ingress_events_per_second(),
            allowed_chat_ids: Vec::new(),
            fixed_session_id: None,
            include_self_output: true,
            additional_reply_targets: Vec::new(),
            additional_binding_keys: Vec::new(),
            session_policy: crate::ConnectorSessionPolicy::default(),
        }),
    }
}

fn connector_record_as_external(
    record: Option<ConnectorConfigRecord>,
    name: &str,
) -> Result<crate::ExternalConnectorConfig, anyhow::Error> {
    match record {
        Some(ConnectorConfigRecord::External { config, .. }) => Ok(config),
        Some(_) => bail!("connector kind mismatch for external/{name}"),
        None => Ok(crate::ExternalConnectorConfig {
            name: name.to_string(),
            platform: String::new(),
            mode: crate::ExternalConnectorMode::default(),
            base_url: String::new(),
            allow_private_network: false,
            shared_token: None,
            shared_token_env: None,
            shared_token_secret_ref: None,
            allow_unauthenticated_ingress: false,
            fixed_session_id: None,
            include_self_output: true,
            additional_reply_targets: Vec::new(),
            additional_binding_keys: Vec::new(),
            session_policy: crate::ConnectorSessionPolicy::default(),
            ingress_events_per_second: 100,
            child_process: None,
        }),
    }
}

fn connector_record_as_slack(
    record: Option<ConnectorConfigRecord>,
    name: &str,
) -> Result<crate::SlackConnectorConfig, anyhow::Error> {
    match record {
        Some(ConnectorConfigRecord::Slack { config, .. }) => Ok(config),
        Some(_) => bail!("connector kind mismatch for slack/{name}"),
        None => Ok(crate::SlackConnectorConfig {
            name: name.to_string(),
            bot_token: None,
            bot_token_env: None,
            bot_token_secret_ref: None,
            signing_secret: None,
            signing_secret_env: None,
            signing_secret_secret_ref: None,
            allow_unauthenticated_ingress: false,
            api_base_url: None,
            fixed_session_id: None,
            include_self_output: true,
            additional_reply_targets: Vec::new(),
            additional_binding_keys: Vec::new(),
            session_policy: crate::ConnectorSessionPolicy::default(),
            ingress_events_per_second: crate::connectors::slack_default_ingress_events_per_second(),
            allowed_api_app_ids: Vec::new(),
            allowed_enterprise_ids: Vec::new(),
            allowed_team_ids: Vec::new(),
            allowed_channel_ids: Vec::new(),
            allowed_file_hosts: Vec::new(),
            team_bot_tokens: Vec::new(),
        }),
    }
}

fn connector_record_as_http(
    record: Option<ConnectorConfigRecord>,
    name: &str,
) -> Result<crate::HttpInputConnectorConfig, anyhow::Error> {
    match record {
        Some(ConnectorConfigRecord::Http { config, .. }) => Ok(config),
        Some(_) => bail!("connector kind mismatch for http/{name}"),
        None => Ok(crate::HttpInputConnectorConfig {
            name: name.to_string(),
            fixed_session_id: None,
            actor_id: None,
            bearer_token: None,
            bearer_token_env: None,
            bearer_token_secret_ref: None,
            hmac_secret: None,
            hmac_secret_env: None,
            hmac_secret_secret_ref: None,
            allow_unauthenticated_ingress: false,
            require_hmac_signature: false,
            signature_max_age_secs: crate::connectors::http_default_signature_max_age_secs(),
            require_idempotency_key: true,
            ingress_events_per_second: crate::connectors::http_default_ingress_events_per_second(),
            allow_payload_reply_targets: false,
            default_reply_targets: Vec::new(),
            default_binding_keys: Vec::new(),
            session_policy: crate::ConnectorSessionPolicy::default(),
        }),
    }
}

async fn validate_connector_session_policy<M>(
    state: &DaemonState<M>,
    policy: crate::ConnectorSessionPolicy,
) -> Result<crate::ConnectorSessionPolicy, anyhow::Error>
where
    M: ModelDriver + Send + Sync + 'static,
{
    if let Some(persona_id) = policy.persona_id.as_deref() {
        state.get_persona_record(persona_id).await?;
    }
    Ok(policy.normalized())
}

async fn build_external_connector_config<M>(
    state: &DaemonState<M>,
    name: &str,
    payload: &serde_json::Map<String, Value>,
    request: PutExternalConnectorRequest,
) -> Result<BuiltConnectorConfig<crate::ExternalConnectorConfig>, anyhow::Error>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let existing_record = state.connector("external", name).await;
    let mut config = connector_record_as_external(existing_record.clone(), name)?;
    let mut secret_writes = Vec::new();
    config.name = name.to_string();
    if payload_contains_field(payload, "platform") {
        let platform = normalized_string_required(request.platform, "platform")?;
        if let Some(ConnectorConfigRecord::External {
            config: existing, ..
        }) = &existing_record
            && existing.platform != platform
        {
            bail!("external connector {name} platform is immutable once created");
        }
        config.platform = platform;
    }
    if payload_contains_field(payload, "mode") {
        config.mode = request.mode.unwrap_or_default();
    }
    if payload_contains_field(payload, "base_url") {
        config.base_url = normalized_string_required(request.base_url, "base_url")?;
    }
    if payload_contains_field(payload, "allow_private_network") {
        config.allow_private_network = request.allow_private_network.unwrap_or(false);
    }
    if payload_contains_field(payload, "allow_unauthenticated_ingress") {
        config.allow_unauthenticated_ingress =
            request.allow_unauthenticated_ingress.unwrap_or(false);
    }
    if payload_contains_field(payload, "fixed_session_id") {
        config.fixed_session_id =
            normalized_optional_string(request.fixed_session_id, "fixed_session_id")?;
    }
    if payload_contains_field(payload, "include_self_output") {
        config.include_self_output = request.include_self_output.unwrap_or(true);
    }
    if payload_contains_field(payload, "additional_reply_targets") {
        let additional_reply_targets = request.additional_reply_targets.unwrap_or_default();
        state.validate_persisted_reply_targets(&additional_reply_targets)?;
        config.additional_reply_targets = additional_reply_targets;
    }
    if payload_contains_field(payload, "additional_binding_keys") {
        config.additional_binding_keys = request.additional_binding_keys.unwrap_or_default();
    }
    if payload_contains_field(payload, "session_policy") {
        config.session_policy =
            validate_connector_session_policy(state, request.session_policy.unwrap_or_default())
                .await?;
    }
    if payload_contains_field(payload, "ingress_events_per_second") {
        config.ingress_events_per_second = request.ingress_events_per_second.unwrap_or(100).max(1);
    }
    if payload_contains_field(payload, "child_process") {
        config.child_process = request.child_process;
    }
    if payload_contains_field(payload, "shared_token") {
        let resolved = resolve_connector_secret_input(
            state,
            "external",
            name,
            "shared_token",
            request.shared_token,
            config.shared_token.clone(),
            config.shared_token_env.clone(),
            config.shared_token_secret_ref.clone(),
        )
        .await?;
        config.shared_token = resolved.inline;
        config.shared_token_env = resolved.env;
        config.shared_token_secret_ref = resolved.secret_ref;
        push_connector_secret_write(&mut secret_writes, resolved.pending_write)?;
    }
    Ok(BuiltConnectorConfig {
        config,
        secret_writes,
    })
}

async fn build_telegram_connector_config<M>(
    state: &DaemonState<M>,
    name: &str,
    payload: &serde_json::Map<String, Value>,
    request: PutTelegramConnectorRequest,
) -> Result<BuiltConnectorConfig<crate::TelegramConnectorConfig>, anyhow::Error>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let mut config = connector_record_as_telegram(state.connector("telegram", name).await, name)?;
    let mut secret_writes = Vec::new();
    config.name = name.to_string();
    if payload_contains_field(payload, "api_base_url") {
        config.api_base_url = normalized_optional_string(request.api_base_url, "api_base_url")?;
    }
    if payload_contains_field(payload, "allow_unauthenticated_ingress") {
        config.allow_unauthenticated_ingress =
            request.allow_unauthenticated_ingress.unwrap_or(false);
    }
    if payload_contains_field(payload, "ingress_mode") {
        config.ingress_mode = request.ingress_mode;
    }
    if payload_contains_field(payload, "polling_timeout_seconds") {
        config.polling_timeout_seconds = request.polling_timeout_seconds.unwrap_or(30);
    }
    if payload_contains_field(payload, "ingress_events_per_second") {
        config.ingress_events_per_second = request
            .ingress_events_per_second
            .unwrap_or_else(crate::connectors::default_telegram_ingress_events_per_second)
            .max(1);
    }
    if payload_contains_field(payload, "allowed_chat_ids") {
        let mut allowed_chat_ids = request.allowed_chat_ids.unwrap_or_default();
        allowed_chat_ids.sort_unstable();
        allowed_chat_ids.dedup();
        config.allowed_chat_ids = allowed_chat_ids;
    }
    if payload_contains_field(payload, "fixed_session_id") {
        config.fixed_session_id =
            normalized_optional_string(request.fixed_session_id, "fixed_session_id")?;
    }
    if payload_contains_field(payload, "include_self_output") {
        config.include_self_output = request.include_self_output.unwrap_or(true);
    }
    if payload_contains_field(payload, "additional_reply_targets") {
        let additional_reply_targets = request.additional_reply_targets.unwrap_or_default();
        state.validate_persisted_reply_targets(&additional_reply_targets)?;
        config.additional_reply_targets = additional_reply_targets;
    }
    if payload_contains_field(payload, "additional_binding_keys") {
        config.additional_binding_keys = request.additional_binding_keys.unwrap_or_default();
    }
    if payload_contains_field(payload, "session_policy") {
        config.session_policy =
            validate_connector_session_policy(state, request.session_policy.unwrap_or_default())
                .await?;
    }
    if payload_contains_field(payload, "bot_token") {
        let resolved = resolve_connector_secret_input(
            state,
            "telegram",
            name,
            "bot_token",
            request.bot_token,
            config.bot_token.clone(),
            config.bot_token_env.clone(),
            config.bot_token_secret_ref.clone(),
        )
        .await?;
        config.bot_token = resolved.inline;
        config.bot_token_env = resolved.env;
        config.bot_token_secret_ref = resolved.secret_ref;
        push_connector_secret_write(&mut secret_writes, resolved.pending_write)?;
    }
    if payload_contains_field(payload, "secret_token") {
        let resolved = resolve_connector_secret_input(
            state,
            "telegram",
            name,
            "secret_token",
            request.secret_token,
            config.secret_token.clone(),
            config.secret_token_env.clone(),
            config.secret_token_secret_ref.clone(),
        )
        .await?;
        config.secret_token = resolved.inline;
        config.secret_token_env = resolved.env;
        config.secret_token_secret_ref = resolved.secret_ref;
        push_connector_secret_write(&mut secret_writes, resolved.pending_write)?;
    }
    Ok(BuiltConnectorConfig {
        config,
        secret_writes,
    })
}

async fn build_slack_connector_config<M>(
    state: &DaemonState<M>,
    name: &str,
    payload: &serde_json::Map<String, Value>,
    request: PutSlackConnectorRequest,
) -> Result<BuiltConnectorConfig<crate::SlackConnectorConfig>, anyhow::Error>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let mut config = connector_record_as_slack(state.connector("slack", name).await, name)?;
    let mut secret_writes = Vec::new();
    config.name = name.to_string();
    if payload_contains_field(payload, "api_base_url") {
        config.api_base_url = normalized_optional_string(request.api_base_url, "api_base_url")?;
    }
    if payload_contains_field(payload, "allow_unauthenticated_ingress") {
        config.allow_unauthenticated_ingress =
            request.allow_unauthenticated_ingress.unwrap_or(false);
    }
    if payload_contains_field(payload, "fixed_session_id") {
        config.fixed_session_id =
            normalized_optional_string(request.fixed_session_id, "fixed_session_id")?;
    }
    if payload_contains_field(payload, "include_self_output") {
        config.include_self_output = request.include_self_output.unwrap_or(true);
    }
    if payload_contains_field(payload, "additional_reply_targets") {
        let additional_reply_targets = request.additional_reply_targets.unwrap_or_default();
        state.validate_persisted_reply_targets(&additional_reply_targets)?;
        config.additional_reply_targets = additional_reply_targets;
    }
    if payload_contains_field(payload, "additional_binding_keys") {
        config.additional_binding_keys = request.additional_binding_keys.unwrap_or_default();
    }
    if payload_contains_field(payload, "session_policy") {
        config.session_policy =
            validate_connector_session_policy(state, request.session_policy.unwrap_or_default())
                .await?;
    }
    if payload_contains_field(payload, "ingress_events_per_second") {
        config.ingress_events_per_second = request
            .ingress_events_per_second
            .unwrap_or_else(crate::connectors::slack_default_ingress_events_per_second)
            .max(1);
    }
    if payload_contains_field(payload, "allowed_api_app_ids") {
        config.allowed_api_app_ids = normalized_string_vec(
            request.allowed_api_app_ids.unwrap_or_default(),
            "allowed_api_app_ids",
        )?;
    }
    if payload_contains_field(payload, "allowed_enterprise_ids") {
        config.allowed_enterprise_ids = normalized_string_vec(
            request.allowed_enterprise_ids.unwrap_or_default(),
            "allowed_enterprise_ids",
        )?;
    }
    if payload_contains_field(payload, "allowed_team_ids") {
        config.allowed_team_ids = normalized_string_vec(
            request.allowed_team_ids.unwrap_or_default(),
            "allowed_team_ids",
        )?;
    }
    if payload_contains_field(payload, "allowed_channel_ids") {
        config.allowed_channel_ids = normalized_string_vec(
            request.allowed_channel_ids.unwrap_or_default(),
            "allowed_channel_ids",
        )?;
    }
    if payload_contains_field(payload, "allowed_file_hosts") {
        config.allowed_file_hosts = normalized_string_vec(
            request.allowed_file_hosts.unwrap_or_default(),
            "allowed_file_hosts",
        )?
        .into_iter()
        .map(|host| host.to_ascii_lowercase())
        .collect();
    }
    if payload_contains_field(payload, "team_bot_tokens") {
        let mut team_bot_tokens = Vec::new();
        for entry in request.team_bot_tokens.unwrap_or_default() {
            let team_id =
                normalized_string_required(Some(entry.team_id), "team_bot_tokens.team_id")?;
            let field = format!("team_bot_tokens.{team_id}.bot_token");
            let resolved = resolve_connector_secret_input(
                state,
                "slack",
                name,
                &field,
                Some(entry.bot_token),
                None,
                None,
                None,
            )
            .await?;
            anyhow::ensure!(
                resolved.inline.is_some()
                    || resolved.env.is_some()
                    || resolved.secret_ref.is_some(),
                "team_bot_tokens.{team_id}.bot_token is required"
            );
            push_connector_secret_write(&mut secret_writes, resolved.pending_write)?;
            team_bot_tokens.push(crate::SlackTeamBotTokenConfig {
                team_id,
                bot_token: resolved.inline,
                bot_token_env: resolved.env,
                bot_token_secret_ref: resolved.secret_ref,
            });
        }
        team_bot_tokens.sort_by(|left, right| left.team_id.cmp(&right.team_id));
        let before = team_bot_tokens.len();
        team_bot_tokens.dedup_by(|left, right| left.team_id == right.team_id);
        anyhow::ensure!(
            team_bot_tokens.len() == before,
            "team_bot_tokens contains duplicate team_id"
        );
        config.team_bot_tokens = team_bot_tokens;
    }
    if payload_contains_field(payload, "bot_token") {
        let resolved = resolve_connector_secret_input(
            state,
            "slack",
            name,
            "bot_token",
            request.bot_token,
            config.bot_token.clone(),
            config.bot_token_env.clone(),
            config.bot_token_secret_ref.clone(),
        )
        .await?;
        config.bot_token = resolved.inline;
        config.bot_token_env = resolved.env;
        config.bot_token_secret_ref = resolved.secret_ref;
        push_connector_secret_write(&mut secret_writes, resolved.pending_write)?;
    }
    if payload_contains_field(payload, "signing_secret") {
        let resolved = resolve_connector_secret_input(
            state,
            "slack",
            name,
            "signing_secret",
            request.signing_secret,
            config.signing_secret.clone(),
            config.signing_secret_env.clone(),
            config.signing_secret_secret_ref.clone(),
        )
        .await?;
        config.signing_secret = resolved.inline;
        config.signing_secret_env = resolved.env;
        config.signing_secret_secret_ref = resolved.secret_ref;
        push_connector_secret_write(&mut secret_writes, resolved.pending_write)?;
    }
    Ok(BuiltConnectorConfig {
        config,
        secret_writes,
    })
}

async fn build_http_connector_config<M>(
    state: &DaemonState<M>,
    name: &str,
    payload: &serde_json::Map<String, Value>,
    request: PutHttpConnectorRequest,
) -> Result<BuiltConnectorConfig<crate::HttpInputConnectorConfig>, anyhow::Error>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let mut config = connector_record_as_http(state.connector("http", name).await, name)?;
    let mut secret_writes = Vec::new();
    config.name = name.to_string();
    if payload_contains_field(payload, "actor_id") {
        config.actor_id = normalized_optional_string(request.actor_id, "actor_id")?;
    }
    if payload_contains_field(payload, "allow_unauthenticated_ingress") {
        config.allow_unauthenticated_ingress =
            request.allow_unauthenticated_ingress.unwrap_or(false);
    }
    if payload_contains_field(payload, "require_hmac_signature") {
        config.require_hmac_signature = request.require_hmac_signature.unwrap_or(false);
    }
    if payload_contains_field(payload, "signature_max_age_secs") {
        config.signature_max_age_secs = request
            .signature_max_age_secs
            .unwrap_or_else(crate::connectors::http_default_signature_max_age_secs);
    }
    if payload_contains_field(payload, "require_idempotency_key") {
        config.require_idempotency_key = request.require_idempotency_key.unwrap_or(true);
    }
    if payload_contains_field(payload, "ingress_events_per_second") {
        config.ingress_events_per_second = request
            .ingress_events_per_second
            .unwrap_or_else(crate::connectors::http_default_ingress_events_per_second);
    }
    if payload_contains_field(payload, "allow_payload_reply_targets") {
        config.allow_payload_reply_targets = request.allow_payload_reply_targets.unwrap_or(false);
    }
    if payload_contains_field(payload, "fixed_session_id") {
        config.fixed_session_id =
            normalized_optional_string(request.fixed_session_id, "fixed_session_id")?;
    }
    if payload_contains_field(payload, "default_reply_targets") {
        let default_reply_targets = request.default_reply_targets.unwrap_or_default();
        state.validate_persisted_reply_targets(&default_reply_targets)?;
        config.default_reply_targets = default_reply_targets;
    }
    if payload_contains_field(payload, "default_binding_keys") {
        config.default_binding_keys = request.default_binding_keys.unwrap_or_default();
    }
    if payload_contains_field(payload, "session_policy") {
        config.session_policy =
            validate_connector_session_policy(state, request.session_policy.unwrap_or_default())
                .await?;
    }
    if payload_contains_field(payload, "bearer_token") {
        let resolved = resolve_connector_secret_input(
            state,
            "http",
            name,
            "bearer_token",
            request.bearer_token,
            config.bearer_token.clone(),
            config.bearer_token_env.clone(),
            config.bearer_token_secret_ref.clone(),
        )
        .await?;
        config.bearer_token = resolved.inline;
        config.bearer_token_env = resolved.env;
        config.bearer_token_secret_ref = resolved.secret_ref;
        push_connector_secret_write(&mut secret_writes, resolved.pending_write)?;
    }
    if payload_contains_field(payload, "hmac_secret") {
        let resolved = resolve_connector_secret_input(
            state,
            "http",
            name,
            "hmac_secret",
            request.hmac_secret,
            config.hmac_secret.clone(),
            config.hmac_secret_env.clone(),
            config.hmac_secret_secret_ref.clone(),
        )
        .await?;
        config.hmac_secret = resolved.inline;
        config.hmac_secret_env = resolved.env;
        config.hmac_secret_secret_ref = resolved.secret_ref;
        push_connector_secret_write(&mut secret_writes, resolved.pending_write)?;
    }
    Ok(BuiltConnectorConfig {
        config,
        secret_writes,
    })
}

async fn build_session_reply_targets<M>(
    state: &DaemonState<M>,
    requests: Vec<SessionReplyTargetRequest>,
) -> Result<Vec<kheish_types::ReplyHandle>, anyhow::Error>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let reply_targets = requests
        .into_iter()
        .map(SessionReplyTargetRequest::into_reply_handle)
        .collect::<Vec<_>>();
    state.validate_persisted_reply_targets(&reply_targets)?;
    Ok(reply_targets)
}

fn normalized_string_required(value: Option<String>, field: &str) -> Result<String, anyhow::Error> {
    let value = value.ok_or_else(|| anyhow!("{field} is required"))?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{field} is required");
    }
    Ok(trimmed.to_string())
}

fn is_invalid_learning_policy_error(message: &str) -> bool {
    message.contains("learning publication default_action")
        || message.contains("learning publication quarantine names")
        || message.contains("learning judge timeout_ms")
        || message.contains("semantic capture timeout_ms")
        || message.contains("semantic capture max_candidates_per_run")
        || message.contains("automatic active publication is not supported")
        || message.contains("learning publication rule #")
}

fn internal_error(error: anyhow::Error) -> ApiError {
    if let Some(problem) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<DaemonProblem>())
    {
        let status =
            StatusCode::from_u16(problem.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return ApiError::coded(
            status,
            problem.domain,
            problem.code,
            problem.detail().to_string(),
        );
    }
    if let Some(error) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<kheish_core::UserQuestionValidationError>())
    {
        return ApiError::coded(
            StatusCode::BAD_REQUEST,
            "questions",
            error.problem_code(),
            error.to_string(),
        );
    }
    if let Some(error) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<kheish_runtime::HookBlockedError>())
    {
        return ApiError::coded(
            StatusCode::FORBIDDEN,
            "hooks",
            "hook_blocked",
            error.to_string(),
        );
    }

    let message = error.to_string();
    if legacy_hook_block_message(&message) {
        return ApiError::coded(StatusCode::FORBIDDEN, "hooks", "hook_blocked", message);
    }
    if message == "session has no goal" {
        return ApiError::coded(StatusCode::NOT_FOUND, "goals", "goal_not_found", message);
    }
    if message == "session already has a goal" {
        return ApiError::coded(
            StatusCode::CONFLICT,
            "goals",
            "goal_already_exists",
            message,
        );
    }
    if message == "session goal changed" || message == "session goal version changed" {
        return ApiError::coded(StatusCode::CONFLICT, "goals", "goal_conflict", message);
    }
    if message.contains("has active or queued runs") {
        return ApiError::coded(
            StatusCode::CONFLICT,
            "goals",
            "goal_session_not_idle",
            message,
        );
    }
    if message.contains("goal objective")
        || message.contains("goal token budget")
        || message.contains("session goal patch requires")
        || message.contains("session goal completion requires")
        || message == "run is not bound to a session goal"
    {
        return ApiError::coded(
            StatusCode::BAD_REQUEST,
            "goals",
            "goal_invalid_request",
            message,
        );
    }
    if message.contains("expects previous revision") {
        return ApiError::coded(
            StatusCode::CONFLICT,
            "boards",
            "board_revision_conflict",
            message,
        );
    }
    if message.contains("route `") && message.contains(" is not ready: ") {
        return ApiError::coded(
            StatusCode::BAD_REQUEST,
            "routes",
            "route_not_ready",
            message,
        );
    }
    if message.contains("board state asset") {
        return ApiError::coded(
            StatusCode::BAD_REQUEST,
            "boards",
            "board_state_invalid",
            message,
        );
    }
    if message.contains("board revision render asset") && message.contains("missing raw payload") {
        return ApiError::coded(
            StatusCode::BAD_REQUEST,
            "boards",
            "board_asset_missing",
            message,
        );
    }
    let status = if message.contains("unknown session")
        || message.contains("unknown agent")
        || message.contains("unknown run")
        || message.contains("unknown schedule")
        || message.contains("unknown task")
        || message.contains("secret `") && message.contains("was not found")
        || message.contains("credential subject `") && message.contains("was not found")
        || message.contains("credential lease `") && message.contains("was not found")
        || message.contains("unknown learning candidate")
        || message.contains("unknown learning skill")
        || message.contains("unknown learning ")
        || message.contains("unknown asset")
        || message.contains("unknown derivation")
        || message.contains("unknown observation source")
        || message.contains("unknown observation")
        || message.contains("unknown observation transcript job")
        || message.contains("unknown capture agent")
        || message.contains("unknown skill")
        || message.contains("unknown persona")
        || message.contains("unknown board ")
        || message.contains("unknown previous board revision")
        || message.contains("unknown board revision ")
        || message.contains("unknown channel ")
        || message.contains("unknown channel message ")
        || message.contains("unknown project ")
        || message.contains("unknown project task ")
        || message.contains("unknown project member ")
        || message.contains("unknown playbook ")
        || message.contains("unknown flow ")
        || message.starts_with("unknown connector ")
    {
        StatusCode::NOT_FOUND
    } else if message.contains("reply target references unknown connector") {
        StatusCode::BAD_REQUEST
    } else if message.contains("escapes workspace root")
        || message.contains("workspace_root metadata")
        || message.contains("expected an absolute path")
        || message.contains("expected a relative path")
        || message.contains("escapes filesystem root")
        || message.contains("unknown reply plugin")
        || message.contains("reply_address is required")
        || message.contains("no output plugin matched the requested routes")
        || message
            .contains("input_items cannot be combined with legacy content or attachments fields")
        || message.contains("cannot be combined with input_items")
        || message.contains("or input_items is required")
        || message.contains("mailbox payload input_items must be an array")
        || message.contains("mailbox payload asset_ids must be an array")
        || message.contains("asset_ids entries must not be empty")
        || message.contains("asset_id is required")
        || message.contains("or asset_ids or input_items is required")
        || message.contains("content or attachments or input_items is required")
        || message.contains("unsupported attachment")
        || message.contains("attachment file_name is required")
        || message.contains("attachment content_base64 is required")
        || message.contains("asset payload")
        || message.contains("failed to decode attachment")
        || message.contains("attachment is not valid")
        || message.contains("attachment is not a valid")
        || message.contains("attachment is not a decodable")
        || message.contains("audio transcription")
        || message.starts_with("WAV ")
        || message.contains("WebM ")
        || message.contains("MP3 audio frame")
        || message.contains("MP3 ID3 tag")
        || message.contains("Ogg Opus")
        || message.contains("AAC ADTS")
        || message.contains("FLAC")
        || message.contains("PCM payload")
        || message.contains("audio duration")
        || message.contains("MP4/M4A")
        || message.contains("does not match detected type")
        || message.contains("PDF attachment has")
        || message.contains("PDF attachment stream")
        || message.contains("PDF attachment streams total")
        || message.contains("PDF attachment extracted text")
        || message.contains("failed to parse PDF attachment")
        || message.contains("failed to extract text from PDF attachment")
        || message.contains("normalized image exceeds")
        || message.contains("failed to decode image/")
        || message.contains("failed to decode image")
        || message.contains("does not contain an InputReceived event")
        || message.contains("do not expose a visual preview")
        || message.contains("does not expose a visual preview")
        || message.contains("observation_ids must belong to the same source")
        || message.contains("observation selection did not resolve any active records")
        || message.contains("is no longer materializable")
        || message.contains("does not allow materialization")
        || message.contains("observation source ") && message.contains(" is disabled")
        || message.contains("does not accept new uploads")
        || message.contains("upload_token is required")
        || message.contains("idempotency_key is required")
        || message.contains("Idempotency-Key header")
        || message.contains("idempotency key is required")
        || message.contains("idempotency key must not exceed")
        || message.contains("idempotency key must not contain control characters")
        || message.contains("client_revision_id is required")
        || message.contains("client_revision_id must not exceed")
        || message.contains("client_revision_id must not contain control characters")
        || message.contains("display_name is required")
        || message.contains("title is required")
        || message.contains("member_id is required")
        || message.contains("sender_actor_id is required")
        || message.contains("emoji is required")
        || message.contains("channel messages require content or input_items")
        || message.contains("thread_root_message_id must point at a thread root message")
        || message.contains("is not a member of channel")
        || message.contains("board references in channel messages require")
        || message.contains("reply_to_message_id ")
            && message.contains(" does not belong to thread ")
        || message.contains("board revisions require an image render asset")
        || message.contains("has no revisions")
        || message.contains("does not belong to board")
        || message.contains("does not belong to session")
        || message.contains("does not reference render asset")
        || message.contains("does not reference state asset")
        || message.contains("is owned by session")
        || message.contains("source_id cannot be empty")
        || message.contains("source_id must not")
        || message.contains("source_id cannot be '.' or '..'")
        || message.contains("source_id may contain only")
        || message.contains("stream_id must not")
        || message.contains("stream_id cannot be '.' or '..'")
        || message.contains("stream_id may contain only")
        || message.contains("batch_id must")
        || message.contains("daemon_base_url must")
        || message.contains("agents must not be empty")
        || message.contains("agents cannot contain more than")
        || message.contains("at least one source kind must be enabled")
        || message.contains("interval_ms must be greater than zero")
        || message.contains("max_runs must be greater than zero")
        || message.contains("duration_ms must be greater than zero")
        || message.contains("machine_id must")
        || message.contains("machine_id may contain only")
        || message.contains("identifier must contain an ASCII letter or digit")
        || message.contains("duplicate machine_id after normalization")
        || message.contains("os_profile ") && message.contains(" does not support ")
        || message.contains("token_ttl_ms must be greater than zero")
        || message.contains("heartbeat_interval_ms must be greater than zero")
        || message.contains("heartbeat_grace_ms must be greater than zero")
        || message.contains("requires camera_unique_id or camera_name")
        || message.contains("cannot set both camera_unique_id and camera_name")
        || message.contains("duplicate observation source") && message.contains("in batch")
        || message.contains("capture metadata for source")
        || message.contains("capture metadata machine_id")
        || message.contains("capture_group_id ") && message.contains(" must start with ")
        || message.contains("retention_seconds must be greater than zero")
        || message.contains("older_than_ms must be greater than zero")
        || message.contains("max_active_observations must be greater than zero")
        || message.contains("max_active_bytes must be greater than zero")
        || message.contains("ingest_rate_limit_window_ms must be greater than zero")
        || message.contains("ingest_rate_limit_burst must be greater than zero")
        || message.contains("max_observations must be greater than zero")
        || message.contains("source_id is required")
        || message.contains("stream_id is required")
        || message.contains("upload_token must differ from the revoked current token")
        || message.contains("asset_id is required")
        || message.contains("observation_id is required")
        || message.contains("session_id is required")
        || message.contains("thread_root_message_id is required for thread-scoped stimuli")
        || message.contains("thread_root_message_id is only valid for thread-scoped stimuli")
        || message.contains("max_parallel_public_speakers must be greater than zero")
        || message.contains("max_agent_replies_per_human_message must be greater than zero")
        || message.contains("lease_timeout_ms must be greater than zero")
        || message.contains("max_pending_stimuli must be greater than zero")
        || message.contains("observation_ids must not be empty")
        || message.contains("observation transcript selection did not resolve")
        || message.contains("capture_group_id is required")
        || message.contains("recording_id must not")
        || message.contains("target_chunk_seconds must be")
        || message.contains("max_chunk_bytes must be")
        || message.contains("schedule request must define exactly one payload")
        || message.contains("persona skill assignment name cannot be empty")
        || message.contains("persona skill `") && message.contains("is assigned more than once")
        || message.contains("persona skill `")
            && message.contains("is excluded by the persona capability scope")
        || message.contains("persona default skill `") && message.contains("is not installed")
        || message.contains("connector payload must be a JSON object")
        || message.contains("connector name ") && message.contains("cannot contain ':'")
        || message.contains("failed to decode telegram connector payload")
        || message.contains("failed to decode external connector payload")
        || message.contains("failed to decode slack connector payload")
        || message.contains("failed to decode http connector payload")
        || message.contains("external connector ") && message.contains(" base_url")
        || message.contains("telegram connector ") && message.contains(" api_base_url")
        || message.contains("connector secret input `")
        || message.contains("unknown connector kind")
        || message.contains("invalid external reply route")
        || message.contains("invalid telegram reply route")
        || message.contains("invalid slack reply route")
        || message.contains("invalid http reply route")
        || message.contains("daemon reply targets require a non-empty session address")
        || message.contains("missing secret-store slot ")
        || message.contains("is not a generic secret")
        || message.contains("connector and MCP secret slots must use generic opaque secret records")
        || message
            .contains("connector and MCP secret slots must use generic opaque or MCP OAuth records")
        || message.contains("still referenced by projects")
        || message.contains("still a project member in")
        || message.contains("still assigned to project tasks")
        || message.contains("still blocks tasks")
        || message.contains("still owns tasks")
        || message.contains("does not accept new work")
        || message.contains("already has active run")
        || message.contains("changed while update was being prepared")
        || message.contains("cannot depend on itself")
        || message.contains("introduces a dependency cycle")
        || message.contains("already registered as another project member")
        || message.contains("task assignees must be session-backed project members")
        || message.contains("belongs to session") && message.contains("not assignee session")
        || message.contains("is not linked to project")
        || message.contains(
            "discussion_channel_id and discussion_thread_root_message_id must be set together",
        )
        || message.contains("must be reopened before it can be started again")
        || message.contains("must be terminal when creating a task")
        || message.contains("must be terminal when updating a task")
        || message.contains("output is derived from latest_run_id")
        || message.contains("is not assigned")
        || message.contains("is still blocked by")
        || message
            .contains("observation_materialization.target_session_id must match target_session_id")
        || message.contains(
            "route_policy cannot be combined with conflicting legacy provider or generation fields",
        )
        || message.contains("contains entries outside the parent scope")
        || message.contains("learning scope id is required")
        || message.contains("learning scope id must not contain leading or trailing whitespace")
        || message.contains("workspace learning scope id must be")
        || message.contains("learning content is required")
        || message.contains("learning content exceeds")
        || message.contains("semantic learning content appears to contain secret material")
        || message.contains("learning revocation reason contains secret-like material")
        || message.contains("learning confidence must be between 0 and 100")
        || is_invalid_learning_policy_error(&message)
        || message.contains("flow_id is required")
        || message.contains("flow_id must not contain")
        || message.contains("idempotency_key must not contain")
        || message.contains("is not a procedure learning")
        || message.contains("must be active before promotion")
        || message.contains("must use workspace scope before promotion")
        || message.contains("promoted procedure skills must use fork context")
        || message.contains("skill_name is required")
        || message.contains("skill_name must not contain")
        || message.contains("skill_name may contain only")
        || message.contains("instructions are required")
        || message.contains("description must not contain")
        || message.contains("when_to_use must not contain")
        || message.contains("version must not contain")
        || message.contains("inline promoted skills cannot declare child-only runtime overrides")
        || message.contains("promoted skill instructions appear to contain secret material")
        || message.contains("promoted skill name appears to contain secret material")
        || message.contains("promoted skill description appears to contain secret material")
        || message.contains("promoted skill when_to_use appears to contain secret material")
        || message.contains("promoted skill version appears to contain secret material")
        || message.contains("promoted skill runtime ")
            && message.contains(" appears to contain secret material")
        || message.contains("promoted skill revocation reason appears to contain secret material")
        || message.contains("promoted skill rollback reason appears to contain secret material")
        || message.contains("expected_output_contains is required")
        || message.contains("rollout evidence definition_fingerprint")
        || message.contains("verification rollout evidence requires")
        || message.contains("promoted skills require daemon-validated verification evidence")
        || message.contains("active promoted skills require")
        || message.contains("canary rollout evidence requires promoted skill status canary")
        || message.contains("cannot transition promoted skill")
        || message.contains("invalid playbook manifest")
        || message.contains("digest mismatch for playbook")
        || message.contains("publish status must be verified, canary, or active")
        || message.contains("requires evidence_refs")
        || message.contains("evidence_refs is required")
        || message.contains("KheishStack manifest exceeds")
        || message.contains("KheishStack manifest uses YAML anchor/alias")
        || message.contains("failed to parse KheishStack")
        || message.contains("file reference")
            && message.contains("not allowed through the daemon Stack API")
        || message.contains("file references")
            && message.contains("not allowed through the daemon Stack API")
        || message.contains("evidence kind is required")
        || message.contains("evidence id is required")
        || message.contains("does not resolve inside flow")
        || message.contains("must be appended after the flow projection exists")
        || message.contains("flow requires narrower session capability_scope")
        || message.contains("flow requires narrower session credential_scope")
        || message.contains("credential_scope does not allow route")
        || message.contains("session operator config must allow")
        || message.contains("session operator config with notify_operator enabled requires")
        || message.contains("cannot clear session reply targets while notify_operator is enabled")
        || message.contains("session operator display_name appears to contain secret material")
        || message
            .contains("session operator communication_style appears to contain secret material")
        || message.contains("must not contain delivery addresses or token references")
        || message.contains("metadata key `") && message.contains("` is daemon-owned")
        || message.contains("metadata must be an object when daemon metadata is attached")
        || message.contains("report_path must be workspace-relative")
        || message.contains("report_path must stay inside the workspace")
        || message.contains("report_path must reference a file")
        || message.contains("provider conflicts with fork_context.provider")
        || message.contains("approval resolution batch must not be empty")
        || message.contains("duplicate approval resolution for request ")
        || message.contains("approval resolution references unknown pending request ")
        || message.contains("user-question resolution ")
        || message.contains("declined user-question resolutions must not include answers")
        || message.contains("duplicate answer for question ")
        || message.contains("duplicate option ") && message.contains(" for question ")
        || message.contains("missing answer for question ")
        || message.contains("unknown option ") && message.contains(" for question ")
        || message.contains("allows only one option")
        || message.contains("requires at least one answer")
        || message.contains("resolution contains answers for unknown questions")
        || message.contains("would widen parent mode")
        || message.contains("unknown permission_mode")
        || message.contains("run memory retention_ms")
        || message.contains("run memory max_tracked_per_session")
        || message.contains("run memory max_prompt_entries")
        || message.contains("tool runtime limit ")
        || message.contains("model `") && message.contains(" is not compatible with route `")
        || message.contains("daemon has no route configured for")
        || message.contains("daemon has no model route `")
        || message.contains("model reconfiguration is not supported by this daemon")
        || message.contains("transcription options")
        || message.contains("transcription timestamp")
        || message.contains("transcription prompt exceeds")
        || message.contains("transcription language")
    {
        StatusCode::BAD_REQUEST
    } else if message.contains("board revision client_revision_id")
        && message.contains("different request payload")
    {
        StatusCode::CONFLICT
    } else if message.contains("unknown runtime config revision") {
        StatusCode::NOT_FOUND
    } else if message.contains("asset integrity mismatch") {
        StatusCode::CONFLICT
    } else if message.contains("has hard references; delete blocked") {
        StatusCode::CONFLICT
    } else if message.contains("subagent spawn policy denied") {
        StatusCode::TOO_MANY_REQUESTS
    } else if message.contains("waiting for approval")
        || message.contains("no pending approval batch")
        || message.contains(" has no active run")
        || message.contains("user-question request ") && message.contains(" expired at ")
        || message.contains("was already resolved with a different answer")
        || message.contains("already running")
        || message.contains("already processing background work")
        || message.contains("run interrupted")
        || message.contains("run cancelled")
        || message.contains("not waiting for approval")
        || message.contains("not waiting for user input")
        || message.contains("active waiting run")
        || message.contains("spawn depth")
        || message.contains("live child limit exceeded")
        || message.contains("live descendant limit exceeded")
        || message.contains("spawn limit exceeded")
        || message.contains("spawn request") && message.contains("already in progress")
        || message.contains("spawn conversation") && message.contains("already in progress")
        || message.contains("spawn_request_id is already bound")
        || message.contains("spawn_request_id requires a matching receipt")
        || message.contains(
            "existing sidechain session was created with a different route or fork context",
        )
        || message.contains("existing sidechain session cannot be reused for a new subtask")
        || message
            .contains("existing sidechain session requires spawn_request_id for mutable reuse")
        || message.contains("agent ") && message.contains(" is closed")
        || message.contains("spawned_by_run_id must match the active parent run")
        || message.contains("persona ") && message.contains("already exists")
        || message.contains("board ") && message.contains("already exists")
        || message.contains("observation source ") && message.contains("cannot change kind")
        || message.contains("capture-owned observation source ")
        || message.contains("capture provisioning batch_id ")
        || message.contains("channel ") && message.contains("already exists")
        || message.contains("platform is immutable once created")
        || message.contains("expects previous revision")
        || message.contains("file-backed and cannot be mutated through the daemon control plane")
        || message.contains("secret `")
            && message.contains("is still referenced by one or more runtime connectors")
        || message.contains("secret `")
            && message.contains("is still referenced by one or more MCP servers")
        || message.contains("session ")
            && message.contains("is already bound to a different persona")
        || message.contains("session ")
            && message.contains("is already bound to a different capability scope")
        || message.contains("session ")
            && message.contains("is already bound to a different credential scope")
        || message == "session has non-terminal work or live descendants"
        || message.contains("changes are only allowed while the session is idle")
        || message.contains("connector changes are only allowed after those sessions are idle")
        || message.contains("learning candidate ") && message.contains("was already published")
        || message.contains("learning candidate ") && message.contains("was rejected")
        || message.contains("learning ") && message.contains("was already superseded")
        || message.contains("cannot supersede revoked learning")
        || message.contains("superseding learning scope must match")
        || message.contains("learning conflicts with active learning")
        || message.contains("already promoted as skill")
        || message.contains("promoted skill ") && message.contains("already exists")
        || message.contains("already exists in the daemon catalog")
        || message
            .contains("active promoted skill definition changes must start a new draft rollout")
        || message.contains("no active rollback snapshot exists for learning skill")
        || message.contains("cannot rollback promoted skill")
        || message.contains("already exists with digest")
        || message.contains(" is not startable in status ")
        || message.contains(" already references a different playbook")
        || message.contains(" already targets session ")
        || message.contains(" already has a different input digest")
        || message.contains(" already has a different idempotency key")
        || message.contains(" already has different metadata")
        || message.contains(" already has different evidence refs")
        || message.contains(" already references run ")
        || message.contains("idempotency key is already bound to flow ")
        || message.contains("session run idempotency key")
        || message.contains("run operation idempotency key")
        || message.contains("runtime config revision conflict")
        || message.contains("idempotency key is already bound to observation transcript job")
        || message.contains("runtime config has no previous revision")
        || message.contains("config change blocked by hook")
    {
        StatusCode::CONFLICT
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    if let Some((domain, code)) = typed_problem_code(&message, status) {
        ApiError::coded(status, domain, code, message)
    } else {
        ApiError::new(status, message)
    }
}

fn legacy_hook_block_message(message: &str) -> bool {
    const HOOK_BLOCK_PREFIXES: &[&str] = &[
        "InstructionsLoaded blocked by hook: ",
        "SessionStart blocked by hook: ",
        "Setup blocked by hook: ",
        "Stop blocked by hook: ",
        "SubagentStart blocked by hook: ",
        "UserPromptSubmit blocked by hook: ",
        "WorktreeCreate blocked by hook: ",
    ];
    HOOK_BLOCK_PREFIXES
        .iter()
        .any(|prefix| message.starts_with(prefix))
}

fn typed_problem_code(message: &str, status: StatusCode) -> Option<(&'static str, &'static str)> {
    if status == StatusCode::NOT_FOUND {
        if message == "session has no goal" {
            return Some(("goals", "goal_not_found"));
        }
        if message.contains("unknown session") {
            return Some(("sessions", "session_not_found"));
        }
        if message.contains("unknown runtime config revision") {
            return Some(("runtime", "runtime_revision_not_found"));
        }
        if message.contains("unknown run") {
            return Some(("runs", "run_not_found"));
        }
        if message.contains("unknown task") {
            return Some(("tasks", "task_not_found"));
        }
        if message.contains("unknown schedule") {
            return Some(("schedules", "schedule_not_found"));
        }
        if message.contains("unknown agent") {
            return Some(("agents", "agent_not_found"));
        }
        if message.contains("unknown asset") {
            return Some(("assets", "asset_not_found"));
        }
    }
    if status == StatusCode::CONFLICT {
        if message == "session already has a goal" {
            return Some(("goals", "goal_already_exists"));
        }
        if message == "session goal changed" || message == "session goal version changed" {
            return Some(("goals", "goal_conflict"));
        }
        if message.contains("has active or queued runs") {
            return Some(("goals", "goal_session_not_idle"));
        }
        if message.contains("board revision client_revision_id")
            && message.contains("different request payload")
        {
            return Some(("boards", "board_revision_idempotency_conflict"));
        }
        if message.contains("asset integrity mismatch") {
            return Some(("assets", "asset_integrity_mismatch"));
        }
        if message.contains("has hard references; delete blocked") {
            return Some(("assets", "asset_delete_blocked"));
        }
        if message.contains("session run idempotency key")
            || message.contains("run operation idempotency key")
            || message.contains("idempotency key is already bound")
        {
            return Some(("idempotency", "idempotency_conflict"));
        }
        if message.contains("persona ") && message.contains("already exists") {
            return Some(("personas", "persona_already_exists"));
        }
        if message.contains("already has active run") || message.contains("already running") {
            return Some(("runs", "run_already_active"));
        }
        if message.contains(" has no active run") {
            return Some(("runs", "run_state_conflict"));
        }
        if message.contains("session has non-terminal work or live descendants")
            || message.contains("changes are only allowed while the session is idle")
            || message.contains("connector changes are only allowed after those sessions are idle")
        {
            return Some(("sessions", "session_not_idle"));
        }
        if message.contains("waiting for approval") || message.contains("not waiting for approval")
        {
            return Some(("approvals", "approval_state_conflict"));
        }
        if message.contains("not waiting for user input") {
            return Some(("questions", "question_state_conflict"));
        }
        if message.contains("user-question request ") && message.contains(" expired at ") {
            return Some(("questions", "question_expired"));
        }
        if message.contains("runtime config revision conflict") {
            return Some(("runtime", "runtime_revision_conflict"));
        }
        if message.contains("runtime config has no previous revision") {
            return Some(("runtime", "runtime_rollback_unavailable"));
        }
        if message.contains("config change blocked by hook") {
            return Some(("runtime", "runtime_change_blocked"));
        }
        if message.contains("capture-owned observation source ") {
            return Some(("capture", "capture_source_managed_by_agent"));
        }
        if message.contains("capture provisioning batch_id ") {
            return Some(("capture", "capture_provisioning_batch_conflict"));
        }
    }
    if status == StatusCode::TOO_MANY_REQUESTS && message.contains("subagent spawn policy denied") {
        return Some(("subagent_policy", "spawn_policy_denied"));
    }
    if status == StatusCode::BAD_REQUEST {
        if message.contains("goal objective") || message.contains("goal token budget") {
            return Some(("goals", "goal_invalid_request"));
        }
        if message.contains("user-question resolution ") {
            return Some(("questions", "question_request_mismatch"));
        }
        if message.contains("approval resolution batch must not be empty") {
            return Some(("approvals", "approval_batch_empty"));
        }
        if message.contains("duplicate approval resolution for request ") {
            return Some(("approvals", "approval_duplicate_resolution"));
        }
        if message.contains("approval resolution references unknown pending request ") {
            return Some(("approvals", "approval_request_not_pending"));
        }
        if message.contains("unknown option ") && message.contains(" for question ") {
            return Some(("questions", "question_option_not_found"));
        }
        if message.contains("missing answer for question ") {
            return Some(("questions", "question_answer_missing"));
        }
        if message.contains("duplicate answer for question ") {
            return Some(("questions", "question_duplicate_answer"));
        }
        if message.contains("duplicate option ") && message.contains(" for question ") {
            return Some(("questions", "question_duplicate_option"));
        }
        if message.contains("declined user-question resolutions must not include answers") {
            return Some(("questions", "question_declined_with_answers"));
        }
        if message.contains("allows only one option") {
            return Some(("questions", "question_single_select_violation"));
        }
        if message.contains("requires at least one answer") {
            return Some(("questions", "question_answer_empty"));
        }
        if message.contains("resolution contains answers for unknown questions") {
            return Some(("questions", "question_unknown_answer"));
        }
        if message.contains("idempotency key") || message.contains("Idempotency-Key") {
            return Some(("idempotency", "invalid_idempotency_key"));
        }
        if message.contains("model `") && message.contains(" is not compatible with route `")
            || message.contains("daemon has no route configured for")
            || message.contains("daemon has no model route `")
            || message.contains("model reconfiguration is not supported by this daemon")
        {
            return Some(("runtime", "runtime_validation_failed"));
        }
        if message.contains("tool runtime limit ") {
            return Some(("runtime", "invalid_tool_runtime_limits"));
        }
        if message.contains("credential_scope does not allow route") {
            return Some(("runtime", "route_blocked_by_credential_scope"));
        }
        if message.contains("KheishStack manifest exceeds") {
            return Some(("stacks", "stack_manifest_too_large"));
        }
        if message.contains("KheishStack manifest uses YAML anchor/alias") {
            return Some(("stacks", "yaml_anchors_not_supported"));
        }
        if message.contains("failed to parse KheishStack") {
            return Some(("stacks", "stack_invalid_manifest"));
        }
        if (message.contains("file reference") || message.contains("file references"))
            && message.contains("not allowed through the daemon Stack API")
        {
            return Some(("stacks", "stack_file_refs_not_supported"));
        }
        if is_invalid_learning_policy_error(message) {
            return Some(("runtime", "invalid_learning_policy"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use axum::body::Body;
    use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
    use kheish_runtime::OpenAiProviderConfig;
    use reqwest::Client;
    use serde_json::json;
    use std::fs;
    use std::net::SocketAddr;
    use tempfile::tempdir;
    use tokio::sync::oneshot;

    use crate::problems::DaemonProblem;
    use crate::{DaemonConfig, build_openai_daemon};

    use super::{
        ProblemDetails, STACK_CONTROL_PLANE_JSON_BODY_LIMIT_BYTES, direct_run_idempotency_key,
        internal_error, parse_stack_json_request,
    };

    #[test]
    fn internal_error_classifies_persona_not_found() {
        let error = internal_error(anyhow!("unknown persona persona-404"));
        assert_eq!(error.status, StatusCode::NOT_FOUND);
        assert!(error.detail.contains("unknown persona"));
    }

    #[test]
    fn internal_error_classifies_task_and_schedule_not_found() {
        assert_eq!(
            internal_error(anyhow!("unknown task task-404")).status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            internal_error(anyhow!("unknown schedule schedule-404")).status,
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn internal_error_prefers_typed_daemon_problem() {
        let error = internal_error(
            DaemonProblem::run_state_conflict("opaque run state conflict detail").into(),
        );
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.domain, Some("runs"));
        assert_eq!(error.code, "run_state_conflict");
        assert_eq!(error.detail, "opaque run state conflict detail");

        let busy = internal_error(
            DaemonProblem::session_busy("session demo is already processing background work")
                .into(),
        );
        assert_eq!(busy.status, StatusCode::CONFLICT);
        assert_eq!(busy.domain, Some("sessions"));
        assert_eq!(busy.code, "session_busy");
    }

    #[test]
    fn internal_error_classifies_mcp_tool_call_failures() {
        let error = internal_error(
            DaemonProblem::bad_gateway("mcp", "mcp_tool_call_failed", "MCP tool call failed")
                .into(),
        );

        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(error.domain, Some("mcp"));
        assert_eq!(error.code, "mcp_tool_call_failed");
    }

    #[tokio::test]
    async fn runtime_mcp_tool_call_api_rejects_invalid_requests_fail_closed() {
        let temp = tempdir().expect("tempdir");
        let state_root = temp.path().join("daemon-mcp-tool-call-api");
        let config = DaemonConfig::new(
            "127.0.0.1:0".parse::<SocketAddr>().expect("bind addr"),
            &state_root,
            temp.path(),
        );
        let (service, listener) = build_openai_daemon(
            config,
            OpenAiProviderConfig::new("gpt-test", "test-openai-key"),
        )
        .await
        .expect("daemon should build");
        let address = listener.local_addr().expect("listener addr");
        let (shutdown, shutdown_rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = service
                .serve_with_shutdown(listener, async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        let client = Client::new();
        let base = format!("http://{address}");
        let problem = client
            .post(format!("{base}/v1/runtime/mcp/tools/%20/call"))
            .json(&json!({}))
            .send()
            .await
            .expect("request should complete")
            .json::<ProblemDetails>()
            .await
            .expect("problem details");
        assert_eq!(problem.status, 400);
        assert_eq!(problem.domain.as_deref(), Some("mcp"));
        assert_eq!(problem.code, "mcp_tool_name_empty");

        let problem = client
            .post(format!("{base}/v1/runtime/mcp/tools/mcp__demo__tool/call"))
            .json(&json!({ "input": [] }))
            .send()
            .await
            .expect("request should complete")
            .json::<ProblemDetails>()
            .await
            .expect("problem details");
        assert_eq!(problem.status, 400);
        assert_eq!(problem.domain.as_deref(), Some("mcp"));
        assert_eq!(problem.code, "mcp_tool_input_not_object");

        let problem = client
            .post(format!("{base}/v1/runtime/mcp/tools/mcp__demo__tool/call"))
            .json(&json!({}))
            .send()
            .await
            .expect("request should complete")
            .json::<ProblemDetails>()
            .await
            .expect("problem details");
        assert_eq!(problem.status, 409);
        assert_eq!(problem.domain.as_deref(), Some("mcp"));
        assert_eq!(problem.code, "mcp_not_configured");

        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn runtime_mcp_tool_call_api_returns_not_found_for_unknown_tool() {
        let temp = tempdir().expect("tempdir");
        let state_root = temp.path().join("daemon-mcp-unknown-tool-api");
        let mcp_config = temp.path().join("mcp-config.toml");
        fs::write(
            &mcp_config,
            r#"
[mcp_servers.empty]
command = "true"
required = false
startup_timeout_sec = 1
"#,
        )
        .expect("mcp config should be written");
        let mut config = DaemonConfig::new(
            "127.0.0.1:0".parse::<SocketAddr>().expect("bind addr"),
            &state_root,
            temp.path(),
        );
        config.mcp_config_path = Some(mcp_config);
        let (service, listener) = build_openai_daemon(
            config,
            OpenAiProviderConfig::new("gpt-test", "test-openai-key"),
        )
        .await
        .expect("daemon should build");
        let address = listener.local_addr().expect("listener addr");
        let (shutdown, shutdown_rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = service
                .serve_with_shutdown(listener, async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        let problem = Client::new()
            .post(format!(
                "http://{address}/v1/runtime/mcp/tools/mcp__empty__missing/call"
            ))
            .json(&json!({}))
            .send()
            .await
            .expect("request should complete")
            .json::<ProblemDetails>()
            .await
            .expect("problem details");
        assert_eq!(problem.status, 404);
        assert_eq!(problem.domain.as_deref(), Some("mcp"));
        assert_eq!(problem.code, "mcp_tool_not_found");

        let _ = shutdown.send(());
    }

    #[test]
    fn direct_run_idempotency_key_reports_typed_header_conflicts() {
        let mut headers = HeaderMap::new();
        headers.insert("Idempotency-Key", HeaderValue::from_static("header-key"));
        let error = direct_run_idempotency_key(&headers, Some("body-key"))
            .expect_err("mismatched idempotency keys should fail");
        let problem = internal_error(error);
        assert_eq!(problem.status, StatusCode::BAD_REQUEST);
        assert_eq!(problem.domain, Some("idempotency"));
        assert_eq!(problem.code, "invalid_idempotency_key");
    }

    #[test]
    fn direct_run_idempotency_key_rejects_empty_keys() {
        let mut headers = HeaderMap::new();
        headers.insert("Idempotency-Key", HeaderValue::from_static(""));
        let error = direct_run_idempotency_key(&headers, None)
            .expect_err("empty header idempotency key should fail");
        let problem = internal_error(error);
        assert_eq!(problem.status, StatusCode::BAD_REQUEST);
        assert_eq!(problem.domain, Some("idempotency"));
        assert_eq!(problem.code, "invalid_idempotency_key");

        let error = direct_run_idempotency_key(&HeaderMap::new(), Some("   "))
            .expect_err("empty body idempotency key should fail");
        let problem = internal_error(error);
        assert_eq!(problem.status, StatusCode::BAD_REQUEST);
        assert_eq!(problem.domain, Some("idempotency"));
        assert_eq!(problem.code, "invalid_idempotency_key");
    }

    #[test]
    fn internal_error_classifies_asset_integrity_conflict() {
        let error = internal_error(anyhow!(
            "asset integrity mismatch for asset-1: expected sha256 old and 5 bytes, found sha256 new and 7 bytes"
        ));
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.domain, Some("assets"));
        assert_eq!(error.code, "asset_integrity_mismatch");
    }

    #[test]
    fn internal_error_classifies_pdf_attachment_limits_as_bad_request() {
        let error = internal_error(anyhow!(
            "PDF attachment has 10001 objects, exceeding the 10000 object parsing limit"
        ));
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.code, "bad_request");
    }

    #[test]
    fn internal_error_classifies_audio_attachment_limits_as_bad_request() {
        for message in [
            "MP3 audio frame is truncated: 4 bytes provided, 417 bytes required",
            "Ogg Opus page body is truncated",
            "FLAC STREAMINFO metadata block is truncated",
            "WAV payload is missing non-empty audio data",
            "WAV block_align 4 does not match channels/bits_per_sample 2",
            "WebM payload does not contain a supported Opus/Vorbis audio track",
            "audio transcription WebM preflight failed: WebM payload does not contain non-empty media data",
            "OpenAI audio transcription request exceeds the 26214400 byte limit",
            "FLAC audio duration 1860000ms exceeds the 1800000ms daemon limit",
            "PCM payload byte length is not aligned to 3-byte samples",
        ] {
            let error = internal_error(anyhow!(message));
            assert_eq!(error.status, StatusCode::BAD_REQUEST, "{message}");
            assert_eq!(error.code, "bad_request", "{message}");
        }
    }

    #[test]
    fn internal_error_classifies_stack_manifest_guards() {
        let anchor = internal_error(anyhow!(
            "KheishStack manifest uses YAML anchor/alias token `&` at line 4, column 11; anchors and aliases are disabled"
        ));
        assert_eq!(anchor.status, StatusCode::BAD_REQUEST);
        assert_eq!(anchor.domain, Some("stacks"));
        assert_eq!(anchor.code, "yaml_anchors_not_supported");

        let size = internal_error(anyhow!(
            "KheishStack manifest exceeds the 262144 byte limit"
        ));
        assert_eq!(size.status, StatusCode::BAD_REQUEST);
        assert_eq!(size.domain, Some("stacks"));
        assert_eq!(size.code, "stack_manifest_too_large");

        let file_ref = internal_error(anyhow!(
            "file references are not allowed through the daemon Stack API; submit a self-contained manifest: spec.personas[reviewer].soul_file"
        ));
        assert_eq!(file_ref.status, StatusCode::BAD_REQUEST);
        assert_eq!(file_ref.domain, Some("stacks"));
        assert_eq!(file_ref.code, "stack_file_refs_not_supported");
    }

    #[tokio::test]
    async fn parse_stack_json_request_enforces_stack_body_limit_before_json_parse() {
        let request = Request::builder()
            .body(Body::from(
                "x".repeat(STACK_CONTROL_PLANE_JSON_BODY_LIMIT_BYTES + 1),
            ))
            .expect("request");

        let error = parse_stack_json_request::<crate::StackManifestRequest>(request)
            .await
            .expect_err("oversized stack body should be rejected");

        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error.domain, Some("stacks"));
        assert_eq!(error.code, "stack_payload_too_large");
    }

    #[tokio::test]
    async fn parse_stack_json_request_reports_typed_invalid_json() {
        let request = Request::builder()
            .body(Body::from("{not-json"))
            .expect("request");

        let error = parse_stack_json_request::<crate::StackManifestRequest>(request)
            .await
            .expect_err("invalid stack JSON should be rejected");

        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.domain, Some("stacks"));
        assert_eq!(error.code, "stack_invalid_json");
    }

    #[test]
    fn internal_error_classifies_board_revision_idempotency_conflict() {
        let error = internal_error(anyhow!(
            "board revision client_revision_id client-1 is already bound to revision board-revision-1 with a different request payload"
        ));
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.domain, Some("boards"));
        assert_eq!(error.code, "board_revision_idempotency_conflict");
    }

    #[test]
    fn internal_error_classifies_run_and_idempotency_conflicts() {
        let idempotency = internal_error(anyhow!(
            "session run idempotency key was reused with a different request payload"
        ));
        assert_eq!(idempotency.status, StatusCode::CONFLICT);
        assert_eq!(idempotency.domain, Some("idempotency"));
        assert_eq!(idempotency.code, "idempotency_conflict");

        let run_state = internal_error(anyhow!("session demo has no active run"));
        assert_eq!(run_state.status, StatusCode::CONFLICT);
        assert_eq!(run_state.domain, Some("runs"));
        assert_eq!(run_state.code, "run_state_conflict");
    }

    #[test]
    fn internal_error_classifies_route_readiness_errors() {
        let readiness = internal_error(anyhow!(
            "route `openai` is not ready: route `openai` references missing auth_ref `openai.primary`"
        ));
        assert_eq!(readiness.status, StatusCode::BAD_REQUEST);
        assert_eq!(readiness.domain, Some("routes"));
        assert_eq!(readiness.code, "route_not_ready");
    }

    #[test]
    fn internal_error_classifies_session_operator_validation_errors() {
        for message in [
            "session operator config must allow notify_operator or ask_operator when enabled",
            "session operator config with notify_operator enabled requires at least one session reply target",
            "cannot clear session reply targets while notify_operator is enabled for this session",
            "session operator display_name appears to contain secret material",
            "session operator communication_style appears to contain secret material",
            "session operator display_name must not contain delivery addresses or token references",
        ] {
            let error = internal_error(anyhow!(message));
            assert_eq!(error.status, StatusCode::BAD_REQUEST, "{message}");
            assert_eq!(error.code, "bad_request", "{message}");
        }
    }

    #[test]
    fn internal_error_classifies_goal_conflicts_and_validation() {
        let duplicate = internal_error(anyhow!("session already has a goal"));
        assert_eq!(duplicate.status, StatusCode::CONFLICT);
        assert_eq!(duplicate.domain, Some("goals"));
        assert_eq!(duplicate.code, "goal_already_exists");

        let stale = internal_error(anyhow!("session goal version changed"));
        assert_eq!(stale.status, StatusCode::CONFLICT);
        assert_eq!(stale.domain, Some("goals"));
        assert_eq!(stale.code, "goal_conflict");

        let missing = internal_error(anyhow!("session has no goal"));
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        assert_eq!(missing.domain, Some("goals"));
        assert_eq!(missing.code, "goal_not_found");

        let active = internal_error(anyhow!("session demo has active or queued runs"));
        assert_eq!(active.status, StatusCode::CONFLICT);
        assert_eq!(active.domain, Some("goals"));
        assert_eq!(active.code, "goal_session_not_idle");

        let invalid = internal_error(anyhow!("goal token budget must be greater than zero"));
        assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
        assert_eq!(invalid.domain, Some("goals"));
        assert_eq!(invalid.code, "goal_invalid_request");

        let missing_cas = internal_error(anyhow!(
            "session goal patch requires expected_goal_id and expected_version"
        ));
        assert_eq!(missing_cas.status, StatusCode::BAD_REQUEST);
        assert_eq!(missing_cas.domain, Some("goals"));
        assert_eq!(missing_cas.code, "goal_invalid_request");

        let missing_idle_guard = internal_error(anyhow!(
            "session goal completion requires require_no_active_runs"
        ));
        assert_eq!(missing_idle_guard.status, StatusCode::BAD_REQUEST);
        assert_eq!(missing_idle_guard.domain, Some("goals"));
        assert_eq!(missing_idle_guard.code, "goal_invalid_request");

        let unbound_run = internal_error(anyhow!("run is not bound to a session goal"));
        assert_eq!(unbound_run.status, StatusCode::BAD_REQUEST);
        assert_eq!(unbound_run.domain, Some("goals"));
        assert_eq!(unbound_run.code, "goal_invalid_request");
    }

    #[test]
    fn internal_error_classifies_runtime_config_errors() {
        let revision = internal_error(anyhow!(
            "runtime config revision conflict: expected 1, current 2"
        ));
        assert_eq!(revision.status, StatusCode::CONFLICT);
        assert_eq!(revision.domain, Some("runtime"));
        assert_eq!(revision.code, "runtime_revision_conflict");

        let blocked = internal_error(anyhow!(
            "config change blocked by hook: debug_level: hook requested stop"
        ));
        assert_eq!(blocked.status, StatusCode::CONFLICT);
        assert_eq!(blocked.domain, Some("runtime"));
        assert_eq!(blocked.code, "runtime_change_blocked");

        let invalid = internal_error(anyhow!(
            "model `gpt-5.4` is not compatible with route `anthropic`"
        ));
        assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
        assert_eq!(invalid.domain, Some("runtime"));
        assert_eq!(invalid.code, "runtime_validation_failed");

        let invalid_tool_limits = internal_error(anyhow!(
            "tool runtime limit max_input_bytes must be greater than zero"
        ));
        assert_eq!(invalid_tool_limits.status, StatusCode::BAD_REQUEST);
        assert_eq!(invalid_tool_limits.domain, Some("runtime"));
        assert_eq!(invalid_tool_limits.code, "invalid_tool_runtime_limits");

        let invalid_learning_policy = internal_error(anyhow!(
            "learning publication default_action cannot be publish_active"
        ));
        assert_eq!(invalid_learning_policy.status, StatusCode::BAD_REQUEST);
        assert_eq!(invalid_learning_policy.domain, Some("runtime"));
        assert_eq!(invalid_learning_policy.code, "invalid_learning_policy");

        let invalid_procedure_policy = internal_error(anyhow!(
            "automatic active publication is not supported for procedure learnings"
        ));
        assert_eq!(invalid_procedure_policy.status, StatusCode::BAD_REQUEST);
        assert_eq!(invalid_procedure_policy.domain, Some("runtime"));
        assert_eq!(invalid_procedure_policy.code, "invalid_learning_policy");
    }

    #[test]
    fn internal_error_classifies_hook_block_errors() {
        let typed = internal_error(
            kheish_runtime::HookBlockedError::new(
                kheish_types::HookEventName::UserPromptSubmit,
                "input blocked by hook: blocked",
            )
            .into(),
        );
        assert_eq!(typed.status, StatusCode::FORBIDDEN);
        assert_eq!(typed.domain, Some("hooks"));
        assert_eq!(typed.code, "hook_blocked");

        let legacy = internal_error(anyhow!(
            "UserPromptSubmit blocked by hook: input blocked by hook: blocked"
        ));
        assert_eq!(legacy.status, StatusCode::FORBIDDEN);
        assert_eq!(legacy.domain, Some("hooks"));
        assert_eq!(legacy.code, "hook_blocked");

        let unrelated = internal_error(anyhow!("background task blocked by hook: unrelated"));
        assert_eq!(unrelated.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn internal_error_classifies_user_question_validation_errors() {
        let unknown = internal_error(anyhow!("unknown option latency for question focus"));
        assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
        assert_eq!(unknown.domain, Some("questions"));
        assert_eq!(unknown.code, "question_option_not_found");

        let missing = internal_error(anyhow!("missing answer for question focus"));
        assert_eq!(missing.status, StatusCode::BAD_REQUEST);
        assert_eq!(missing.domain, Some("questions"));
        assert_eq!(missing.code, "question_answer_missing");

        let duplicate_option =
            internal_error(anyhow!("duplicate option memory for question focus"));
        assert_eq!(duplicate_option.status, StatusCode::BAD_REQUEST);
        assert_eq!(duplicate_option.domain, Some("questions"));
        assert_eq!(duplicate_option.code, "question_duplicate_option");

        let duplicate_answer = internal_error(anyhow!("duplicate answer for question focus"));
        assert_eq!(duplicate_answer.status, StatusCode::BAD_REQUEST);
        assert_eq!(duplicate_answer.domain, Some("questions"));
        assert_eq!(duplicate_answer.code, "question_duplicate_answer");

        let declined = internal_error(anyhow!(
            "declined user-question resolutions must not include answers"
        ));
        assert_eq!(declined.status, StatusCode::BAD_REQUEST);
        assert_eq!(declined.domain, Some("questions"));
        assert_eq!(declined.code, "question_declined_with_answers");

        let single_select = internal_error(anyhow!("question focus allows only one option"));
        assert_eq!(single_select.status, StatusCode::BAD_REQUEST);
        assert_eq!(single_select.domain, Some("questions"));
        assert_eq!(single_select.code, "question_single_select_violation");

        let empty = internal_error(anyhow!("question focus requires at least one answer"));
        assert_eq!(empty.status, StatusCode::BAD_REQUEST);
        assert_eq!(empty.domain, Some("questions"));
        assert_eq!(empty.code, "question_answer_empty");

        let unknown_answer =
            internal_error(anyhow!("resolution contains answers for unknown questions"));
        assert_eq!(unknown_answer.status, StatusCode::BAD_REQUEST);
        assert_eq!(unknown_answer.domain, Some("questions"));
        assert_eq!(unknown_answer.code, "question_unknown_answer");

        let mismatch = internal_error(anyhow!(
            "user-question resolution req-2 does not match pending request req-1"
        ));
        assert_eq!(mismatch.status, StatusCode::BAD_REQUEST);
        assert_eq!(mismatch.domain, Some("questions"));
        assert_eq!(mismatch.code, "question_request_mismatch");

        let expired = internal_error(anyhow!("user-question request req-1 expired at 123"));
        assert_eq!(expired.status, StatusCode::CONFLICT);
        assert_eq!(expired.domain, Some("questions"));
        assert_eq!(expired.code, "question_expired");

        let state = internal_error(anyhow!("run run-1 is not waiting for user input"));
        assert_eq!(state.status, StatusCode::CONFLICT);
        assert_eq!(state.domain, Some("questions"));
        assert_eq!(state.code, "question_state_conflict");
    }

    #[test]
    fn internal_error_classifies_approval_validation_errors() {
        let duplicate = internal_error(anyhow!(
            "duplicate approval resolution for request approval-1"
        ));
        assert_eq!(duplicate.status, StatusCode::BAD_REQUEST);
        assert_eq!(duplicate.domain, Some("approvals"));
        assert_eq!(duplicate.code, "approval_duplicate_resolution");

        let unknown = internal_error(anyhow!(
            "approval resolution references unknown pending request approval-missing"
        ));
        assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
        assert_eq!(unknown.domain, Some("approvals"));
        assert_eq!(unknown.code, "approval_request_not_pending");
        assert_eq!(
            internal_error(anyhow!("session idle-session has no active run")).status,
            StatusCode::CONFLICT
        );
    }

    #[test]
    fn internal_error_classifies_persona_conflicts_and_validation() {
        assert_eq!(
            internal_error(anyhow!("persona persona-1 already exists")).status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            internal_error(anyhow!(
                "session demo is already bound to a different capability scope"
            ))
            .status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            internal_error(anyhow!(
                "session demo has non-terminal work or live descendants; persona changes are only allowed while the session is idle"
            ))
            .status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            internal_error(anyhow!("session has non-terminal work or live descendants")).status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            internal_error(anyhow!(
                "persona skill `live-inline-marker` is excluded by the persona capability scope"
            ))
            .status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            internal_error(anyhow!("unknown board board-404")).status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            internal_error(anyhow!("unknown board revision board-revision-404")).status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            internal_error(anyhow!(
                "unknown previous board revision board-revision-403"
            ))
            .status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            internal_error(anyhow!("board board-1 already exists")).status,
            StatusCode::CONFLICT
        );
        let board_conflict = internal_error(anyhow!(
            "board board-1 expects previous revision Some(\"board-revision-1\"), got None"
        ));
        assert_eq!(board_conflict.status, StatusCode::CONFLICT);
        assert_eq!(board_conflict.domain, Some("boards"));
        assert_eq!(board_conflict.code, "board_revision_conflict");
        let board_state_invalid =
            internal_error(anyhow!("board state asset asset-1 requires schema_version"));
        assert_eq!(board_state_invalid.status, StatusCode::BAD_REQUEST);
        assert_eq!(board_state_invalid.domain, Some("boards"));
        assert_eq!(board_state_invalid.code, "board_state_invalid");
        let board_asset_missing = internal_error(anyhow!(
            "board revision render asset asset-1 is missing raw payload"
        ));
        assert_eq!(board_asset_missing.status, StatusCode::BAD_REQUEST);
        assert_eq!(board_asset_missing.domain, Some("boards"));
        assert_eq!(board_asset_missing.code, "board_asset_missing");
        assert_eq!(
            internal_error(anyhow!(
                "board board-1 is owned by session session-1; revisions must come from that session"
            ))
            .status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            internal_error(anyhow!("run run-1 does not reference render asset asset-1")).status,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn internal_error_classifies_learning_errors() {
        assert_eq!(
            internal_error(anyhow!("unknown learning candidate learning-candidate-404")).status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            internal_error(anyhow!("workspace learning scope id must be `default`")).status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            internal_error(anyhow!(
                "learning candidate learning-candidate-1 was already published"
            ))
            .status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            internal_error(anyhow!("learning learning-1 was already superseded")).status,
            StatusCode::CONFLICT
        );
    }

    #[test]
    fn internal_error_classifies_mcp_secret_reference_conflict() {
        assert_eq!(
            internal_error(anyhow!(
                "secret `mcp.custom.secretHttp.BEARER_TOKEN` is still referenced by one or more MCP servers"
            ))
            .status,
            StatusCode::CONFLICT
        );
    }

    #[test]
    fn internal_error_classifies_sidechain_and_flow_verifier_validation() {
        assert_eq!(
            internal_error(anyhow!("provider conflicts with fork_context.provider")).status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            internal_error(anyhow!(
                "existing sidechain session was created with a different route or fork context"
            ))
            .status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            internal_error(anyhow!(
                "existing sidechain session cannot be reused for a new subtask without spawn_request_id"
            ))
            .status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            internal_error(anyhow!("report_path must be workspace-relative")).status,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn internal_error_classifies_capture_provisioning_errors() {
        assert_eq!(
            internal_error(anyhow!("duplicate machine_id after normalization: mac-001")).status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            internal_error(anyhow!(
                "observation source macos-mac-001-screen cannot change kind from ScreenSnapshot to WebcamSnapshot"
            ))
            .status,
            StatusCode::CONFLICT
        );
    }

    #[test]
    fn openapi_spec_advertises_key_paths_and_problem_details() {
        let spec = super::openapi_spec();
        assert_eq!(spec["openapi"], "3.1.0");
        assert!(
            spec["paths"]["/v1/status"]["get"].is_object(),
            "status path should be documented"
        );
        assert!(
            spec["paths"]["/v1/sessions/{session_id}"]["get"].is_object(),
            "session path should be documented"
        );
        assert!(
            spec["paths"]["/v1/sessions/{session_id}"]["get"]["parameters"]
                .as_array()
                .expect("session path parameters")
                .iter()
                .any(|parameter| {
                    parameter["name"] == "session_id"
                        && parameter["in"] == "path"
                        && parameter["required"] == true
                }),
            "templated session route should document required path parameter"
        );
        assert!(
            spec["paths"]["/v1/sessions/{session_id}/memory-context"]["get"].is_object(),
            "session memory-context path should be documented"
        );
        assert!(
            spec["paths"]["/v1/sessions/{session_id}/memory-search"]["get"].is_object(),
            "session memory-search path should be documented"
        );
        for method in ["get", "post", "put", "patch", "delete"] {
            assert!(
                spec["paths"]["/v1/sessions/{session_id}/goal"][method].is_object(),
                "{method} /v1/sessions/{{session_id}}/goal should be documented"
            );
        }
        assert!(
            spec["paths"]["/v1/runs/{run_id}"]["get"].is_object(),
            "run path should be documented"
        );
        assert!(
            spec["paths"]["/v1/agents/audit"]["get"].is_object(),
            "agent audit path should be documented"
        );
        assert!(
            spec["paths"]["/v1/stacks/plan"]["post"].is_object(),
            "stack plan path should be documented"
        );
        assert!(
            spec["paths"]["/v1/stacks/apply"]["post"].is_object(),
            "stack apply path should be documented"
        );
        assert!(
            spec["paths"]["/v1/stacks/{ownership_id}/ledger"]["get"].is_object(),
            "stack ledger path should be documented"
        );
        assert_eq!(
            spec["paths"]["/v1/agents/summaries"]["get"]["parameters"][0]["name"],
            "root_agent_id"
        );
        assert!(
            spec["paths"]["/v1/agents/summaries"]["get"]["parameters"]
                .as_array()
                .expect("agent summary parameters")
                .iter()
                .any(|parameter| parameter["name"] == "status")
        );
        assert!(
            spec["paths"]["/v1/agents/{agent_id}/audit"]["get"].is_object(),
            "scoped agent audit path should be documented"
        );
        assert!(
            spec["paths"]["/v1/runtime/auth/accounts"]["get"].is_object(),
            "runtime auth accounts path should be documented"
        );
        assert!(
            spec["paths"]["/v1/runtime/mcp/tools/{tool_name}/call"]["post"].is_object(),
            "runtime MCP tool call path should be documented"
        );
        assert!(
            spec["paths"]["/v1/runtime/mcp/tools/{tool_name}/call"]["post"]["responses"]["502"]
                .is_object(),
            "runtime MCP tool call path should document upstream failure responses"
        );
        assert!(
            spec["paths"]["/v1/runtime/connectors/external/metrics"]["get"].is_object(),
            "external connector metrics path should be documented"
        );
        assert!(
            spec["paths"]["/healthz"]["get"].is_object(),
            "health probe path should be documented"
        );
        assert!(
            spec["paths"]["/readyz"]["get"].is_object(),
            "readiness probe path should be documented"
        );
        assert!(
            spec["paths"]["/v1/connectors/http/{name}"]["post"].is_object(),
            "HTTP connector ingress path should be documented"
        );
        assert_eq!(
            spec["paths"]["/v1/connectors/http/{name}"]["post"]["security"],
            serde_json::json!([
                { "connectorBearer": [] },
                { "httpConnectorHmac": [], "httpConnectorTimestamp": [] }
            ])
        );
        assert!(
            !spec["paths"]["/v1/connectors/http/{name}"]["post"]["security"]
                .as_array()
                .expect("HTTP connector security")
                .iter()
                .any(|security| security == &serde_json::json!({})),
            "HTTP connector ingress must not advertise unauthenticated access"
        );
        assert!(
            spec["paths"]["/v1/connectors/external/{name}/events/batch"]["post"].is_object(),
            "external connector batch ingress path should be documented"
        );
        assert_eq!(
            spec["paths"]["/v1/connectors/slack/{name}"]["post"]["security"],
            serde_json::json!([{ "slackSignature": [], "slackRequestTimestamp": [] }])
        );
        assert_eq!(
            spec["paths"]["/v1/connectors/telegram/{name}"]["post"]["security"],
            serde_json::json!([{ "telegramSecret": [] }])
        );
        assert!(
            spec["paths"]["/v1/events/stream"]["get"].is_object(),
            "global event stream path should be documented"
        );
        assert_eq!(
            spec["paths"]["/v1/events/stream"]["get"]["responses"]["2XX"]["content"]["text/event-stream"]
                ["schema"]["type"],
            "string"
        );
        let event_stream_parameters = spec["paths"]["/v1/events/stream"]["get"]["parameters"]
            .as_array()
            .expect("events stream parameters");
        assert!(
            event_stream_parameters
                .iter()
                .any(|parameter| parameter["name"] == "Last-Event-ID"
                    && parameter["in"] == "header"
                    && parameter["schema"]["type"] == "string"),
            "events stream should document Last-Event-ID"
        );
        assert!(
            event_stream_parameters
                .iter()
                .any(|parameter| parameter["name"] == "cursor"
                    && parameter["in"] == "query"
                    && parameter["schema"]["type"] == "string"),
            "events stream should document cursor as a string"
        );
        assert!(
            event_stream_parameters
                .iter()
                .any(|parameter| parameter["name"] == "run_id"),
            "events stream should document run_id filter"
        );
        assert!(
            spec["paths"]["/v1/observation-sources"]["get"].is_object(),
            "observation source path should be documented"
        );
        assert!(
            spec["paths"]["/v1/observation-sources/{source_id}/observations"]["post"].is_object(),
            "observation ingress path should be documented"
        );
        assert_eq!(
            spec["paths"]["/v1/observation-sources/{source_id}/observations"]["post"]["security"],
            serde_json::json!([{ "observationUploadToken": [] }])
        );
        assert!(
            spec["paths"]["/v1/derivations"]["get"].is_object(),
            "derivation list path should be documented"
        );
        assert!(
            spec["paths"]["/v1/deliveries/{delivery_id}/resolve"]["post"].is_object(),
            "delivery resolve path should be documented"
        );
        assert!(
            spec["paths"]["/v1/schedules/{schedule_id}/trigger"]["post"].is_object(),
            "schedule trigger path should be documented"
        );
        assert!(
            spec["paths"]["/v1/sessions"]["get"]["parameters"]
                .as_array()
                .expect("session list parameters")
                .iter()
                .any(|parameter| parameter["name"] == "limit"),
            "session list should document paginated limit"
        );
        assert!(
            spec["paths"]["/v1/sessions"]["get"]["parameters"]
                .as_array()
                .expect("session list parameters")
                .iter()
                .any(|parameter| parameter["name"] == "persona_id"),
            "session list should document persona_id filter"
        );
        assert!(
            spec["paths"]["/v1/runs"]["get"]["parameters"]
                .as_array()
                .expect("run list parameters")
                .iter()
                .any(|parameter| parameter["name"] == "priority_active"),
            "run list should document priority_active filter"
        );
        assert!(
            spec["paths"]["/v1/sessions/{session_id}/questions"]["get"]["parameters"]
                .as_array()
                .expect("session questions parameters")
                .iter()
                .any(|parameter| parameter["name"] == "limit"),
            "session-scoped questions should document pagination"
        );
        assert!(
            spec["paths"]["/v1/sessions/{session_id}/tasks"]["get"]["parameters"]
                .as_array()
                .expect("task list parameters")
                .iter()
                .any(|parameter| parameter["name"] == "status"),
            "task list should document status filter"
        );
        for (path, method) in [
            ("/v1/playbooks/validate", "post"),
            ("/v1/playbooks/{playbook_id}/publish", "post"),
            ("/v1/playbooks/{playbook_id}/revoke", "post"),
            ("/v1/flows/{flow_id}/cancel", "post"),
            ("/v1/flows/{flow_id}/evidence", "post"),
            ("/v1/flows/{flow_id}/verify/product-view", "post"),
            ("/v1/flows/{flow_id}/stream", "get"),
        ] {
            assert!(
                spec["paths"][path][method].is_object(),
                "{method} {path} should be documented"
            );
        }
        assert!(
            spec["paths"]["/v1/capture-agents/{machine_id}/heartbeat"]["post"].is_object(),
            "capture heartbeat ingress path should be documented"
        );
        assert_eq!(
            spec["paths"]["/v1/capture-agents/{machine_id}/heartbeat"]["post"]["security"],
            serde_json::json!([{ "captureAgentToken": [] }])
        );
        assert_eq!(
            spec["components"]["securitySchemes"]["observationUploadToken"]["scheme"],
            "bearer"
        );
        assert_eq!(
            spec["components"]["securitySchemes"]["captureAgentToken"]["scheme"],
            "bearer"
        );
        assert_eq!(
            spec["components"]["securitySchemes"]["connectorBearer"]["scheme"],
            "bearer"
        );
        assert_eq!(
            spec["components"]["securitySchemes"]["httpConnectorTimestamp"]["name"],
            "x-kheish-timestamp"
        );
        assert_eq!(
            spec["components"]["securitySchemes"]["slackRequestTimestamp"]["name"],
            "x-slack-request-timestamp"
        );
        assert_eq!(
            spec["components"]["responses"]["Problem"]["content"]["application/problem+json"]["schema"]
                ["$ref"],
            "#/components/schemas/ProblemDetails"
        );
        assert_eq!(
            spec["components"]["schemas"]["ListPage"]["properties"]["pagination"]["$ref"],
            "#/components/schemas/ListPageMeta"
        );
        assert_eq!(
            spec["components"]["schemas"]["ListPageMeta"]["properties"]["total_count"]["type"],
            "integer"
        );
        assert_eq!(
            spec["components"]["schemas"]["ProblemDetails"]["properties"]["domain"]["type"],
            "string"
        );
        assert_eq!(
            spec["paths"]["/v1/sessions"]["get"]["responses"]["429"]["$ref"],
            "#/components/responses/Problem"
        );
        assert_eq!(
            spec["paths"]["/v1/status"]["get"]["responses"]["2XX"]["description"],
            "Success"
        );
        assert_eq!(
            spec["paths"]["/v1/status"]["get"]["responses"]["503"]["$ref"],
            "#/components/responses/Problem"
        );
    }

    #[test]
    fn openapi_spec_has_stable_route_methods_operation_ids_and_pagination_contracts() {
        let spec = super::openapi_spec();
        let paths = spec["paths"].as_object().expect("OpenAPI paths object");
        let mut operation_ids = std::collections::BTreeSet::new();

        for route in super::CONTROL_PLANE_OPENAPI_ROUTES {
            let path = paths
                .get(route.path)
                .and_then(serde_json::Value::as_object)
                .unwrap_or_else(|| panic!("OpenAPI missing route {}", route.path));
            let documented_methods = path
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            let expected_methods = route
                .methods
                .iter()
                .map(|method| method.to_ascii_lowercase())
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                documented_methods, expected_methods,
                "OpenAPI methods drifted for {}",
                route.path
            );

            for method in route.methods {
                let operation = path
                    .get(&method.to_ascii_lowercase())
                    .and_then(serde_json::Value::as_object)
                    .unwrap_or_else(|| panic!("OpenAPI missing operation {method} {}", route.path));
                let operation_id = operation
                    .get("operationId")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_else(|| {
                        panic!(
                            "OpenAPI operation {method} {} has no operationId",
                            route.path
                        )
                    });
                assert!(
                    operation_ids.insert(operation_id.to_string()),
                    "duplicate OpenAPI operationId {operation_id}"
                );
                let responses = operation
                    .get("responses")
                    .and_then(serde_json::Value::as_object)
                    .unwrap_or_else(|| {
                        panic!("OpenAPI operation {method} {} has no responses", route.path)
                    });
                for status in [
                    "400", "401", "403", "404", "405", "409", "413", "422", "429", "503", "500",
                ] {
                    assert_eq!(
                        responses[status]["$ref"], "#/components/responses/Problem",
                        "{method} {} should document {status} as ProblemDetails",
                        route.path
                    );
                }
            }
        }

        for path_name in super::OPENAPI_PAGINATED_LIST_PATHS {
            let get = paths
                .get(*path_name)
                .unwrap_or_else(|| panic!("paginated OpenAPI path {path_name} is missing"))["get"]
                .as_object()
                .unwrap_or_else(|| panic!("paginated OpenAPI path {path_name} has no GET"));
            let parameters = get["parameters"]
                .as_array()
                .unwrap_or_else(|| panic!("paginated OpenAPI path {path_name} has no parameters"));
            for parameter_name in ["page", "limit", "cursor"] {
                assert!(
                    parameters
                        .iter()
                        .any(|parameter| parameter["name"] == parameter_name),
                    "{path_name} should document {parameter_name}"
                );
            }
            assert!(
                get["responses"]["200"]["description"]
                    .as_str()
                    .is_some_and(|description| description.contains("Legacy array")),
                "{path_name} should document legacy array compatibility"
            );
        }
    }

    #[test]
    fn openapi_spec_covers_registered_daemon_http_routes() {
        let mut registered = std::collections::BTreeMap::new();
        for source in [
            include_str!("handlers.rs"),
            include_str!("../observation_ingress.rs"),
            include_str!("../connectors/routes/mod.rs"),
        ] {
            let production_source = source.split("#[cfg(test)]").next().unwrap_or(source);
            for (path, methods) in extract_route_methods(production_source) {
                registered.entry(path).or_insert(methods);
            }
        }

        let documented = super::CONTROL_PLANE_OPENAPI_ROUTES
            .iter()
            .map(|spec| {
                (
                    spec.path.to_string(),
                    spec.methods
                        .iter()
                        .map(|method| method.to_ascii_lowercase())
                        .collect::<std::collections::BTreeSet<_>>(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let registered_paths = registered
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let documented_paths = documented
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let missing = registered
            .keys()
            .filter(|path| !documented.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "OpenAPI is missing registered daemon HTTP routes: {missing:?}"
        );

        let stale = documented
            .keys()
            .filter(|path| !registered.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            stale.is_empty(),
            "OpenAPI documents routes that are no longer registered: {stale:?}"
        );
        assert_eq!(
            registered_paths, documented_paths,
            "OpenAPI route path set drifted"
        );

        let method_mismatches = registered
            .iter()
            .filter_map(|(path, methods)| {
                let documented = documented.get(path)?;
                (methods != documented).then(|| (path.clone(), methods.clone(), documented.clone()))
            })
            .collect::<Vec<_>>();
        assert!(
            method_mismatches.is_empty(),
            "OpenAPI method sets drifted from registered routes: {method_mismatches:?}"
        );
    }

    #[test]
    fn openapi_get_routes_are_classified_by_control_plane_auth() {
        for spec in super::CONTROL_PLANE_OPENAPI_ROUTES {
            if !spec.methods.contains(&"GET") {
                continue;
            }
            if super::openapi_operation_security(spec.path, "GET")
                != serde_json::json!([{ "bearerAuth": [] }])
            {
                continue;
            }
            let sampled_path = sample_openapi_path(spec.path);
            assert!(
                crate::api::auth::read_path_has_explicit_access(&sampled_path),
                "{} should be explicitly classified by control-plane auth",
                spec.path
            );
        }
    }

    fn extract_route_methods(
        source: &str,
    ) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
        let bytes = source.as_bytes();
        let mut routes = std::collections::BTreeMap::new();
        let mut cursor = 0;
        while let Some(offset) = source[cursor..].find(".route(") {
            let route_start = cursor + offset;
            let args_start = route_start + ".route(".len();
            cursor = args_start;
            while bytes
                .get(cursor)
                .is_some_and(|byte| byte.is_ascii_whitespace())
            {
                cursor += 1;
            }
            if bytes.get(cursor) != Some(&b'"') {
                continue;
            }
            cursor += 1;
            let start = cursor;
            while let Some(byte) = bytes.get(cursor) {
                if *byte == b'"' {
                    let path = source[start..cursor].to_string();
                    let Some(route_end) = find_matching_paren(source, args_start) else {
                        break;
                    };
                    let methods = extract_route_method_names(&source[cursor + 1..route_end]);
                    routes.insert(path, methods);
                    cursor = route_end + 1;
                    break;
                }
                cursor += 1;
            }
        }
        routes
    }

    fn find_matching_paren(source: &str, args_start: usize) -> Option<usize> {
        let bytes = source.as_bytes();
        let mut cursor = args_start;
        let mut depth = 1usize;
        let mut in_string = false;
        let mut escaped = false;
        while let Some(byte) = bytes.get(cursor) {
            if in_string {
                if escaped {
                    escaped = false;
                } else if *byte == b'\\' {
                    escaped = true;
                } else if *byte == b'"' {
                    in_string = false;
                }
                cursor += 1;
                continue;
            }
            match byte {
                b'"' => in_string = true,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(cursor);
                    }
                }
                _ => {}
            }
            cursor += 1;
        }
        None
    }

    fn extract_route_method_names(source: &str) -> std::collections::BTreeSet<String> {
        ["get", "post", "put", "patch", "delete"]
            .into_iter()
            .filter(|method| source_contains_method_constructor(source, method))
            .map(str::to_string)
            .collect()
    }

    fn source_contains_method_constructor(source: &str, method: &str) -> bool {
        let pattern = format!("{method}(");
        let mut cursor = 0;
        while let Some(offset) = source[cursor..].find(&pattern) {
            let start = cursor + offset;
            let preceding = source[..start].chars().next_back();
            if preceding
                .is_none_or(|character| !character.is_ascii_alphanumeric() && character != '_')
            {
                return true;
            }
            cursor = start + pattern.len();
        }
        false
    }

    fn sample_openapi_path(path: &str) -> String {
        let mut sampled = String::new();
        let mut chars = path.chars();
        while let Some(character) = chars.next() {
            if character == '{' {
                for inner in chars.by_ref() {
                    if inner == '}' {
                        break;
                    }
                }
                sampled.push_str("sample");
            } else {
                sampled.push(character);
            }
        }
        sampled
    }
}
