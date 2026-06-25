//! HTTP ingress routes for daemon-managed connectors.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::body::{Body, to_bytes};
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use tokio::sync::Mutex;

use kheish_core::ModelDriver;

use crate::api::ProblemDetails;
use crate::{
    ConnectorIngressReservation, ConnectorSessionPolicy, CreateSessionRequest, DaemonState,
    RunView, SubmitInputRequest,
};

mod external;
mod http;
mod multimodal;
mod slack;
pub(crate) mod telegram;

const CONNECTOR_INGRESS_PENDING_WAIT: Duration = Duration::from_secs(150);

static CONNECTOR_INGRESS_RATE_LIMITS: OnceLock<
    Mutex<BTreeMap<String, ConnectorIngressRateLimitState>>,
> = OnceLock::new();

#[derive(Debug, Clone)]
struct ConnectorIngressRateLimitState {
    capacity: f64,
    tokens: f64,
    last_refill: Instant,
}

impl ConnectorIngressRateLimitState {
    fn new(rate_per_second: u32) -> Self {
        let capacity = f64::from(rate_per_second.max(1));
        Self {
            capacity,
            tokens: capacity,
            last_refill: Instant::now(),
        }
    }

    fn take(&mut self, rate_per_second: u32) -> Option<u64> {
        let now = Instant::now();
        let rate = f64::from(rate_per_second.max(1));
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * self.capacity).min(self.capacity);
        if (self.capacity - rate).abs() > f64::EPSILON {
            self.capacity = rate;
            self.tokens = self.tokens.min(self.capacity);
        }
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return None;
        }
        let missing = 1.0 - self.tokens;
        Some(((missing / self.capacity) * 1000.0).ceil().max(1.0) as u64)
    }
}

/// One acquired ingress idempotency lease held while a connector submission is being materialized.
pub(super) struct ConnectorIngressLease {
    key: String,
}

/// The shared result of connector ingress idempotency acquisition.
pub(super) enum ConnectorIngressGuard {
    Untracked,
    Reserved(ConnectorIngressLease),
    Existing(RunView),
}

pub(crate) fn build_router<M>(state: Arc<DaemonState<M>>) -> Router
where
    M: ModelDriver + Send + Sync + 'static,
{
    let external_routes = Router::new()
        .route(
            "/v1/connectors/external/{name}/events",
            post(external::external_event::<M>),
        )
        .route(
            "/v1/connectors/external/{name}/events/batch",
            post(external::external_event_batch::<M>),
        )
        .layer(DefaultBodyLimit::max(
            external::EXTERNAL_CONNECTOR_HTTP_BODY_LIMIT_BYTES,
        ));
    Router::new()
        .route("/v1/connectors/http/{name}", post(http::http_webhook::<M>))
        .merge(external_routes)
        .route(
            "/v1/connectors/external/{name}/credentials/{env_key}",
            axum::routing::get(external::external_credential::<M>),
        )
        .route(
            "/v1/connectors/external/{name}/deliveries/{delivery_id}/assets/{asset_id}/raw",
            axum::routing::get(external::external_asset_raw::<M>),
        )
        .route(
            "/v1/connectors/slack/{name}",
            post(slack::slack_webhook::<M>),
        )
        .route(
            "/v1/connectors/telegram/{name}",
            post(telegram::telegram_webhook::<M>),
        )
        .layer(middleware::from_fn(
            connector_ingress_problem_details_middleware,
        ))
        .with_state(state)
}

#[derive(Debug)]
struct ConnectorIngressProblem {
    status: StatusCode,
    code: &'static str,
    detail: String,
}

impl ConnectorIngressProblem {
    fn new(status: StatusCode, code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status,
            code,
            detail: detail.into(),
        }
    }
}

impl IntoResponse for ConnectorIngressProblem {
    fn into_response(self) -> Response {
        (
            self.status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            Json(
                ProblemDetails::new(self.status.as_u16(), self.code, self.detail)
                    .with_domain("connector_ingress"),
            ),
        )
            .into_response()
    }
}

async fn connector_ingress_problem_details_middleware(
    request: Request<Body>,
    next: middleware::Next,
) -> Response {
    let response = next.run(request).await;
    if response.status().is_success() || is_structured_error_response(&response) {
        return response;
    }

    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body();
    let detail = match to_bytes(body, 64 * 1024).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).trim().to_string(),
        Err(error) => format!("failed to read connector ingress error body: {error}"),
    };
    let detail = if detail.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("connector ingress request failed")
            .to_string()
    } else {
        detail
    };

    let mut response = ConnectorIngressProblem::new(
        status,
        connector_ingress_status_code_problem_code(status),
        detail,
    )
    .into_response();
    let response_headers = response.headers_mut();
    for (name, value) in &headers {
        if name != header::CONTENT_TYPE && name != header::CONTENT_LENGTH {
            response_headers.append(name, value.clone());
        }
    }
    response
}

fn is_structured_error_response(response: &Response) -> bool {
    let Some(content_type) = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    content_type.starts_with("application/problem+json")
        || content_type.starts_with("application/json")
}

fn connector_ingress_status_code_problem_code(status: StatusCode) -> &'static str {
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
        _ => "connector_ingress_error",
    }
}

pub(super) fn internal_error(error: anyhow::Error) -> (StatusCode, String) {
    let message = error.to_string();
    let status = if message.contains("unknown persona") {
        StatusCode::BAD_REQUEST
    } else if message.starts_with("unknown connector ")
        || message.contains("reply target references unknown connector")
    {
        StatusCode::NOT_FOUND
    } else if message.contains("session auto-creation is disabled by the connector session policy")
        || message.contains("is already bound to a different persona")
        || message.contains("is already bound to a different capability scope")
        || message.contains("is already bound to a different credential scope")
        || message.contains("was already submitted with a different payload")
    {
        StatusCode::CONFLICT
    } else if message
        .contains("input_items cannot be combined with legacy content or attachments fields")
        || message.contains("content or attachments or input_items is required")
        || message.contains("attachment content_base64 is required")
        || message.contains("attachment file_name is required")
        || message.contains("missing connector content")
        || message.contains("missing telegram callback data")
        || message.contains("missing telegram callback message")
        || (message.contains("telegram chat")
            && message.contains("outside connector")
            && message.contains("allowlist"))
    {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, message)
}

pub(super) fn unauthorized(error: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::UNAUTHORIZED, error.into())
}

pub(super) fn connector_problem_response(
    status: StatusCode,
    code: &'static str,
    detail: impl Into<String>,
) -> Response {
    ConnectorIngressProblem::new(status, code, detail).into_response()
}

pub(super) fn connector_retry_after_problem_response(
    code: &'static str,
    detail: impl Into<String>,
    retry_after_ms: u64,
) -> Response {
    let retry_after_secs = retry_after_ms.div_ceil(1_000).max(1);
    let mut response =
        connector_problem_response(StatusCode::TOO_MANY_REQUESTS, code, detail).into_response();
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&retry_after_secs.to_string())
            .expect("retry-after seconds should be a valid header value"),
    );
    response
}

pub(super) async fn ensure_session<M>(
    state: &Arc<DaemonState<M>>,
    session_id: &str,
    policy: &ConnectorSessionPolicy,
    connector_label: &str,
) -> Result<()>
where
    M: ModelDriver + Send + Sync + 'static,
{
    if state.agent_id_for_session(session_id).await.is_err() && !policy.create_if_missing {
        anyhow::bail!(
            "{connector_label} resolved session {session_id} but session auto-creation is disabled by the connector session policy"
        );
    }
    state
        .create_session(CreateSessionRequest {
            session_id: Some(session_id.to_string()),
            thread_id: None,
            persona_id: policy.persona_id.clone(),
            capability_scope: (!policy.capability_scope.is_empty())
                .then(|| policy.capability_scope.clone()),
            credential_scope: (!policy.credential_scope.is_empty())
                .then(|| policy.credential_scope.clone()),
        })
        .await?;
    Ok(())
}

pub(super) async fn submit_connector_run<M>(
    state: &Arc<DaemonState<M>>,
    session_id: &str,
    request: SubmitInputRequest,
    policy: &ConnectorSessionPolicy,
    connector_label: &str,
) -> Result<RunView>
where
    M: ModelDriver + Send + Sync + 'static,
{
    ensure_session(state, session_id, policy, connector_label).await?;
    state.submit_input_run(session_id, request).await
}

pub(super) async fn acquire_connector_ingress_with_fingerprint<M>(
    state: &Arc<DaemonState<M>>,
    ingress_key: Option<&str>,
    fingerprint: &str,
) -> Result<ConnectorIngressGuard>
where
    M: ModelDriver + Send + Sync + 'static,
{
    acquire_connector_ingress_inner(state, ingress_key, Some(fingerprint)).await
}

async fn acquire_connector_ingress_inner<M>(
    state: &Arc<DaemonState<M>>,
    ingress_key: Option<&str>,
    fingerprint: Option<&str>,
) -> Result<ConnectorIngressGuard>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let Some(ingress_key) = ingress_key else {
        return Ok(ConnectorIngressGuard::Untracked);
    };

    let started_at = Instant::now();
    loop {
        let reservation = match fingerprint {
            Some(fingerprint) => {
                state
                    .begin_connector_ingress_with_fingerprint(ingress_key, fingerprint)
                    .await?
            }
            None => state.begin_connector_ingress(ingress_key).await?,
        };
        match reservation {
            ConnectorIngressReservation::Reserved => {
                return Ok(ConnectorIngressGuard::Reserved(ConnectorIngressLease {
                    key: ingress_key.to_string(),
                }));
            }
            ConnectorIngressReservation::Pending => {
                if let Some(existing) = state.find_run_by_connector_ingress_key(ingress_key).await {
                    state
                        .remember_connector_ingress_run(ingress_key, &existing.run_id)
                        .await?;
                    return Ok(ConnectorIngressGuard::Existing(existing));
                }
                if started_at.elapsed() >= CONNECTOR_INGRESS_PENDING_WAIT {
                    anyhow::bail!(
                        "connector ingress submission is still pending for key {}; retry after the pending reservation expires or the original run is recovered",
                        ingress_key
                    );
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            ConnectorIngressReservation::Existing { run_id } => {
                match state.get_run(&run_id).await {
                    Ok(run) => return Ok(ConnectorIngressGuard::Existing(run)),
                    Err(_) => state.forget_connector_ingress(ingress_key).await?,
                }
            }
        }
    }
}

pub(super) async fn submit_connector_run_with_guard<M>(
    state: &Arc<DaemonState<M>>,
    session_id: &str,
    request: SubmitInputRequest,
    ingress: ConnectorIngressGuard,
    policy: &ConnectorSessionPolicy,
    connector_label: &str,
) -> Result<RunView>
where
    M: ModelDriver + Send + Sync + 'static,
{
    match ingress {
        ConnectorIngressGuard::Existing(run) => Ok(run),
        ConnectorIngressGuard::Untracked => {
            submit_connector_run(state, session_id, request, policy, connector_label).await
        }
        ConnectorIngressGuard::Reserved(lease) => {
            let result =
                submit_connector_run(state, session_id, request, policy, connector_label).await;
            match result {
                Ok(run) => {
                    state
                        .remember_connector_ingress_run(&lease.key, &run.run_id)
                        .await?;
                    Ok(run)
                }
                Err(error) => {
                    state.forget_connector_ingress(&lease.key).await?;
                    Err(error)
                }
            }
        }
    }
}

pub(super) async fn release_connector_ingress<M>(
    state: &Arc<DaemonState<M>>,
    ingress: ConnectorIngressGuard,
) -> Result<()>
where
    M: ModelDriver + Send + Sync + 'static,
{
    if let ConnectorIngressGuard::Reserved(lease) = ingress {
        state.forget_connector_ingress(&lease.key).await?;
    }
    Ok(())
}

pub(super) fn as_json_response(run: RunView) -> Json<RunView> {
    Json(run)
}

pub(super) async fn take_connector_ingress_rate_limit(
    scope: impl Into<String>,
    rate_per_second: u32,
) -> Option<u64> {
    let limits = CONNECTOR_INGRESS_RATE_LIMITS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut limits = limits.lock().await;
    let state = limits
        .entry(scope.into())
        .or_insert_with(|| ConnectorIngressRateLimitState::new(rate_per_second));
    state.take(rate_per_second)
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use axum::http::StatusCode;

    use super::{internal_error, take_connector_ingress_rate_limit};

    #[test]
    fn internal_error_classifies_connector_session_policy_conflicts() {
        assert_eq!(
            internal_error(anyhow!(
                "http connector ingress resolved session demo but session auto-creation is disabled by the connector session policy"
            ))
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            internal_error(anyhow!(
                "session demo is already bound to a different capability scope"
            ))
            .0,
            StatusCode::CONFLICT
        );
    }

    #[test]
    fn internal_error_classifies_unknown_persona_as_bad_request() {
        assert_eq!(
            internal_error(anyhow!("unknown persona persona-404")).0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn internal_error_classifies_generic_input_validation_as_bad_request() {
        assert_eq!(
            internal_error(anyhow!(
                "input_items cannot be combined with legacy content or attachments fields"
            ))
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            internal_error(anyhow!("attachment content_base64 is required")).0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn connector_ingress_rate_limit_is_scoped_and_reports_retry_after() {
        let scope = format!("test-rate-limit-{}", crate::now_ms());

        assert_eq!(
            take_connector_ingress_rate_limit(scope.clone(), 1).await,
            None
        );
        let retry_after_ms = take_connector_ingress_rate_limit(scope.clone(), 1)
            .await
            .expect("second immediate request should be rate limited");
        assert!(retry_after_ms > 0, "retry_after_ms should be positive");

        assert_eq!(
            take_connector_ingress_rate_limit(format!("{scope}:other"), 1).await,
            None,
            "different connector scopes should not share a token bucket"
        );
    }
}
