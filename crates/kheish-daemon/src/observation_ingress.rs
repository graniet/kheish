//! Source-scoped observation ingest routes exposed outside the control-plane admin auth.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::body::{Body, to_bytes};
use axum::extract::{DefaultBodyLimit, Path as AxumPath, State};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};

use kheish_core::ModelDriver;
use sha2::Digest;

use crate::api::{ProblemDetails, digest_token, parse_bearer_token};
use crate::assets::MAX_ASSET_BYTES;
use crate::observations::validate_observation_source_id;
use crate::services::{ObservationIngressRateLimitDecision, ObservationUploadAuthorization};
use crate::state::ObservationIngressReservation;
use crate::{
    CaptureAgentHeartbeatRequest, CaptureAgentHeartbeatResponse, CreateObservationRequest,
    DaemonState, ObservationAuditRecord, ObservationView,
};

const OBSERVATION_INGRESS_PENDING_WAIT: Duration = Duration::from_secs(150);
const OBSERVATION_INGRESS_JSON_BODY_LIMIT_BYTES: usize = MAX_ASSET_BYTES * 2;

enum ObservationIngressGuard {
    Reserved { key: String, fingerprint: String },
    Existing(ObservationView),
}

#[derive(Debug)]
struct IngressProblem {
    status: StatusCode,
    code: &'static str,
    detail: String,
}

impl IngressProblem {
    fn new(status: StatusCode, code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status,
            code,
            detail: detail.into(),
        }
    }

    fn detail(&self) -> &str {
        &self.detail
    }
}

impl IntoResponse for IngressProblem {
    fn into_response(self) -> Response {
        (
            self.status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            Json(
                ProblemDetails::new(self.status.as_u16(), self.code, self.detail)
                    .with_domain("observation_ingress"),
            ),
        )
            .into_response()
    }
}

pub(crate) fn build_router<M>(state: Arc<DaemonState<M>>) -> Router
where
    M: ModelDriver + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/v1/observation-sources/{source_id}/observations",
            post(upload_observation::<M>),
        )
        .route(
            "/v1/capture-agents/{machine_id}/heartbeat",
            post(capture_agent_heartbeat::<M>),
        )
        .layer(DefaultBodyLimit::max(
            OBSERVATION_INGRESS_JSON_BODY_LIMIT_BYTES,
        ))
        .layer(middleware::from_fn(
            observation_ingress_problem_details_middleware,
        ))
        .with_state(state)
}

async fn observation_ingress_problem_details_middleware(
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
        Err(error) => format!("failed to read observation ingress error body: {error}"),
    };
    let detail = if detail.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("observation ingress request failed")
            .to_string()
    } else {
        detail
    };

    let mut response =
        IngressProblem::new(status, ingress_status_code_problem_code(status), detail)
            .into_response();
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

fn ingress_status_code_problem_code(status: StatusCode) -> &'static str {
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
        _ => "observation_ingress_error",
    }
}

fn unauthorized(message: impl Into<String>) -> IngressProblem {
    IngressProblem::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
}

fn bad_request(message: impl Into<String>) -> IngressProblem {
    IngressProblem::new(StatusCode::BAD_REQUEST, "bad_request", message)
}

fn internal_error(error: anyhow::Error) -> IngressProblem {
    IngressProblem::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        error.to_string(),
    )
}

fn too_many_requests(retry_after_ms: u64) -> IngressProblem {
    IngressProblem::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        format!("observation source ingress rate limit exceeded; retry_after_ms={retry_after_ms}"),
    )
}

fn observation_ingress_error(error: anyhow::Error) -> IngressProblem {
    let message = error.to_string();
    if message.contains("idempotency_key is required")
        || message.contains("idempotency_key must not exceed")
        || message.contains("idempotency_key must not contain")
        || message.contains("upload.file_name is required")
        || message.contains("upload.content_base64 is required")
        || message.contains("source_id is required")
        || message.contains("source_id must not")
        || message.contains("source_id cannot")
        || message.contains("source_id may contain only")
        || message.contains("stream_id is required")
        || message.contains("stream_id must not")
        || message.contains("stream_id cannot")
        || message.contains("stream_id may contain only")
        || message.contains("missing or invalid observation bearer token")
        || message.contains("does not accept new uploads")
        || message.contains("reused with a different payload")
        || message.contains("only accepts")
        || message.contains("unknown observation source")
        || message.contains("attachment is not a valid")
        || message.contains("unsupported attachment media type")
        || message.contains("audio transcription ")
        || message.contains("audio duration")
        || message.contains("WAV ")
        || message.contains("WebM ")
        || message.contains("MP3 ")
    {
        return bad_request(message);
    }
    internal_error(error)
}

async fn acquire_observation_ingress<M>(
    state: &Arc<DaemonState<M>>,
    source_id: &str,
    request: &CreateObservationRequest,
) -> Result<ObservationIngressGuard>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let fingerprint = request.fingerprint()?;
    let key = format!("observation:{source_id}:{}", request.idempotency_key.trim());
    let started_at = Instant::now();
    loop {
        if let Some(observation) = state
            .find_observation_by_ingest_key(source_id, &request.idempotency_key, &fingerprint)
            .await?
        {
            let _ = state
                .remember_observation_ingress(&key, &observation.observation_id, &fingerprint)
                .await;
            return Ok(ObservationIngressGuard::Existing(observation));
        }
        match state.begin_observation_ingress(&key, &fingerprint).await? {
            ObservationIngressReservation::Reserved => {
                return Ok(ObservationIngressGuard::Reserved { key, fingerprint });
            }
            ObservationIngressReservation::Pending => {
                if started_at.elapsed() >= OBSERVATION_INGRESS_PENDING_WAIT {
                    anyhow::bail!(
                        "observation ingest is already in progress for key {}",
                        request.idempotency_key.trim()
                    );
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            ObservationIngressReservation::Existing { observation_id } => {
                match state.get_observation(&observation_id).await {
                    Ok(observation) => return Ok(ObservationIngressGuard::Existing(observation)),
                    Err(_) => state.forget_observation_ingress(&key).await?,
                }
            }
        }
    }
}

async fn upload_observation<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(source_id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<CreateObservationRequest>,
) -> Result<Json<ObservationView>, IngressProblem>
where
    M: ModelDriver + Send + Sync + 'static,
{
    if let Err(error) = validate_observation_source_id(&source_id) {
        record_upload_audit(
            &state,
            &source_id,
            "upload_rejected",
            Some("invalid_source_id"),
            None,
            None,
            None,
            None,
        )
        .await;
        return Err(bad_request(error.to_string()));
    }
    let token = match parse_bearer_token(&headers) {
        Ok(token) => token,
        Err(_) => {
            record_upload_audit(
                &state,
                &source_id,
                "upload_rejected",
                Some("missing_or_invalid_token"),
                None,
                None,
                None,
                None,
            )
            .await;
            return Err(unauthorized("missing or invalid observation bearer token"));
        }
    };
    let request_idempotency_key = request.idempotency_key.clone();
    let digest = digest_token(token);
    match state
        .authorize_observation_upload_token(&source_id, &digest)
        .await
    {
        ObservationUploadAuthorization::Authorized => {}
        decision => {
            let reason = match decision {
                ObservationUploadAuthorization::Authorized => unreachable!(),
                ObservationUploadAuthorization::InvalidToken => "invalid_token",
                ObservationUploadAuthorization::ExpiredToken => "expired_token",
                ObservationUploadAuthorization::RevokedToken => "revoked_token",
                ObservationUploadAuthorization::SourceInactive => "source_not_active",
            };
            record_upload_audit(
                &state,
                &source_id,
                "upload_rejected",
                Some(reason),
                None,
                None,
                None,
                None,
            )
            .await;
            return Err(unauthorized("missing or invalid observation bearer token"));
        }
    }
    let guard = match acquire_observation_ingress(&state, &source_id, &request).await {
        Ok(guard) => guard,
        Err(error) => {
            let rejected = observation_ingress_error(error);
            record_upload_audit(
                &state,
                &source_id,
                "upload_rejected",
                Some(safe_rejection_reason(rejected.detail())),
                Some(&request_idempotency_key),
                None,
                None,
                None,
            )
            .await;
            return Err(rejected);
        }
    };
    match guard {
        ObservationIngressGuard::Existing(observation) => {
            record_upload_audit(
                &state,
                &source_id,
                "upload_replayed",
                None,
                Some(&request_idempotency_key),
                Some(&observation.request_fingerprint),
                Some(&observation),
                None,
            )
            .await;
            Ok(Json(observation))
        }
        ObservationIngressGuard::Reserved { key, fingerprint } => {
            match state
                .reserve_observation_ingress_slot(&source_id, crate::now_ms())
                .await
            {
                Ok(ObservationIngressRateLimitDecision::Accepted) => {}
                Ok(ObservationIngressRateLimitDecision::Limited { retry_after_ms }) => {
                    let _ = state.forget_observation_ingress(&key).await;
                    record_upload_audit(
                        &state,
                        &source_id,
                        "upload_rate_limited",
                        Some("rate_limited"),
                        Some(&request_idempotency_key),
                        Some(&fingerprint),
                        None,
                        Some(retry_after_ms),
                    )
                    .await;
                    return Err(too_many_requests(retry_after_ms));
                }
                Err(error) => {
                    let _ = state.forget_observation_ingress(&key).await;
                    return Err(internal_error(error));
                }
            }
            if let Err(error) = state
                .mark_observation_source_authenticated(&source_id, crate::now_ms())
                .await
            {
                let _ = state.forget_observation_ingress(&key).await;
                return Err(internal_error(error));
            }
            match state.ingest_observation(&source_id, request).await {
                Ok(observation) => {
                    if let Err(error) = state
                        .remember_observation_ingress(
                            &key,
                            &observation.observation_id,
                            &fingerprint,
                        )
                        .await
                    {
                        return Err(internal_error(error));
                    }
                    record_upload_audit(
                        &state,
                        &source_id,
                        "upload_succeeded",
                        None,
                        Some(&request_idempotency_key),
                        Some(&fingerprint),
                        Some(&observation),
                        None,
                    )
                    .await;
                    Ok(Json(observation))
                }
                Err(error) => {
                    if let Ok(Some(observation)) = state
                        .find_observation_by_ingest_key(
                            &source_id,
                            &request_idempotency_key,
                            &fingerprint,
                        )
                        .await
                    {
                        let _ = state
                            .remember_observation_ingress(
                                &key,
                                &observation.observation_id,
                                &fingerprint,
                            )
                            .await;
                        return Ok(Json(observation));
                    }
                    let _ = state.forget_observation_ingress(&key).await;
                    let rejected = observation_ingress_error(error);
                    record_upload_audit(
                        &state,
                        &source_id,
                        "upload_rejected",
                        Some(safe_rejection_reason(rejected.detail())),
                        Some(&request_idempotency_key),
                        Some(&fingerprint),
                        None,
                        None,
                    )
                    .await;
                    Err(rejected)
                }
            }
        }
    }
}

async fn capture_agent_heartbeat<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(machine_id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<CaptureAgentHeartbeatRequest>,
) -> Result<Json<CaptureAgentHeartbeatResponse>, IngressProblem>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let token = match parse_bearer_token(&headers) {
        Ok(token) => token,
        Err(_) => {
            return Err(unauthorized(
                "missing or invalid capture agent bearer token",
            ));
        }
    };
    let digest = digest_token(token);
    state
        .record_capture_agent_heartbeat(&machine_id, &digest, request)
        .await
        .map(Json)
        .map_err(capture_agent_heartbeat_error)
}

fn capture_agent_heartbeat_error(error: anyhow::Error) -> IngressProblem {
    let message = error.to_string();
    if message.contains("unknown capture agent") {
        return unauthorized("missing or invalid capture agent bearer token");
    }
    if message.contains("heartbeat observed_source_ids") {
        return bad_request(message);
    }
    if message.contains("token expired")
        || message.contains("token revoked")
        || message.contains("invalid capture agent token")
        || message.contains("capture agent is revoked")
    {
        return unauthorized("missing or invalid capture agent bearer token");
    }
    internal_error(error)
}

async fn record_upload_audit<M>(
    state: &Arc<DaemonState<M>>,
    source_id: &str,
    event: &str,
    reason: Option<&str>,
    idempotency_key: Option<&str>,
    request_fingerprint: Option<&str>,
    observation: Option<&ObservationView>,
    retry_after_ms: Option<u64>,
) where
    M: ModelDriver + Send + Sync + 'static,
{
    let upload_token_version = match state.get_observation_source(source_id).await {
        Ok(source) => Some(source.upload_token_version),
        Err(_) => None,
    };
    state
        .record_observation_audit(ObservationAuditRecord {
            recorded_at_ms: crate::now_ms(),
            event: event.to_string(),
            source_id: source_id.to_string(),
            observation_id: observation.map(|view| view.observation_id.clone()),
            reason: reason.map(str::to_string),
            upload_token_version,
            idempotency_key_sha256: idempotency_key
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| hex::encode(sha2::Sha256::digest(value.as_bytes()))),
            request_fingerprint: request_fingerprint.map(str::to_string),
            media_type: observation.map(|view| view.media_type.clone()),
            byte_length: observation.map(|view| view.byte_length),
            retry_after_ms,
            purged_asset_ids: Vec::new(),
        })
        .await;
}

fn safe_rejection_reason(message: &str) -> &str {
    if message.contains("reused with a different payload") {
        "idempotency_conflict"
    } else if message.contains("only accepts") {
        "unsupported_media_type"
    } else if message.contains("does not accept new uploads") {
        "source_not_active"
    } else if message.contains("failed to decode attachment content_base64") {
        "invalid_base64"
    } else {
        "invalid_request"
    }
}
