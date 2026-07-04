//! Built-in authentication for the daemon control-plane HTTP API.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, ORIGIN, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tracing::warn;

use super::types::ProblemDetails;
use crate::config::{
    ControlPlaneAuthConfig, ControlPlaneAuthTokenFiles, ControlPlaneCorsConfig,
    is_loopback_control_plane_origin,
};

const DEFAULT_AUTH_FAILURE_RATE_LIMIT_MAX: usize = 60;
const DEFAULT_AUTH_FAILURE_RATE_LIMIT_WINDOW_MS: u64 = 60_000;
const MAX_AUTH_FAILURE_RATE_LIMIT_SAMPLES: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlPlaneAccess {
    ReadOnly,
    Admin,
}

impl ControlPlaneAccess {
    fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Admin => "admin",
        }
    }
}

#[derive(Debug)]
pub(crate) struct ControlPlaneAuthorizer {
    admin_token_digest: Option<[u8; 32]>,
    read_only_token_digest: Option<[u8; 32]>,
    token_files: Option<Mutex<ControlPlaneTokenFileDigests>>,
    cors: ControlPlaneCorsConfig,
    monitor: Option<Arc<ControlPlaneAuthMonitor>>,
}

impl ControlPlaneAuthorizer {
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new(config: &ControlPlaneAuthConfig) -> Self {
        Self::with_cors(config, &ControlPlaneCorsConfig::loopback())
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_cors(
        config: &ControlPlaneAuthConfig,
        cors: &ControlPlaneCorsConfig,
    ) -> Self {
        Self {
            admin_token_digest: config.admin_token.as_deref().map(digest_token),
            read_only_token_digest: config.read_only_token.as_deref().map(digest_token),
            token_files: None,
            cors: cors.clone(),
            monitor: None,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_cors_and_audit(
        config: &ControlPlaneAuthConfig,
        cors: &ControlPlaneCorsConfig,
        audit_path: PathBuf,
    ) -> Self {
        Self::with_cors_audit_token_files_and_rate_limit(
            config,
            cors,
            ControlPlaneAuthTokenFiles::default(),
            audit_path,
            ControlPlaneAuthRateLimit::default(),
        )
    }

    #[must_use]
    pub(crate) fn with_cors_audit_and_token_files(
        config: &ControlPlaneAuthConfig,
        cors: &ControlPlaneCorsConfig,
        token_files: ControlPlaneAuthTokenFiles,
        audit_path: PathBuf,
    ) -> Self {
        Self::with_cors_audit_token_files_and_rate_limit(
            config,
            cors,
            token_files,
            audit_path,
            ControlPlaneAuthRateLimit::default(),
        )
    }

    #[must_use]
    fn with_cors_audit_token_files_and_rate_limit(
        config: &ControlPlaneAuthConfig,
        cors: &ControlPlaneCorsConfig,
        token_files: ControlPlaneAuthTokenFiles,
        audit_path: PathBuf,
        rate_limit: ControlPlaneAuthRateLimit,
    ) -> Self {
        let admin_token_digest = config.admin_token.as_deref().map(digest_token);
        let read_only_token_digest = config.read_only_token.as_deref().map(digest_token);
        Self {
            admin_token_digest,
            read_only_token_digest,
            token_files: ControlPlaneTokenFileDigests::new(
                token_files,
                admin_token_digest,
                read_only_token_digest,
            )
            .map(Mutex::new),
            cors: cors.clone(),
            monitor: Some(Arc::new(ControlPlaneAuthMonitor::new(
                audit_path, rate_limit,
            ))),
        }
    }

    #[must_use]
    pub(crate) fn is_enabled(&self) -> bool {
        self.admin_token_digest.is_some()
            || self.read_only_token_digest.is_some()
            || self.token_files.is_some()
    }

    fn authorize(&self, headers: &HeaderMap) -> Result<ControlPlaneAccess, AuthFailure> {
        let token = parse_bearer_token(headers)?;
        let digest = digest_token(token);
        let (admin_token_digest, read_only_token_digest) = self.current_token_digests();
        if admin_token_digest
            .as_ref()
            .is_some_and(|expected| expected.ct_eq(&digest).into())
        {
            return Ok(ControlPlaneAccess::Admin);
        }
        if read_only_token_digest
            .as_ref()
            .is_some_and(|expected| expected.ct_eq(&digest).into())
        {
            return Ok(ControlPlaneAccess::ReadOnly);
        }
        Err(AuthFailure::InvalidToken)
    }

    fn current_token_digests(&self) -> (Option<[u8; 32]>, Option<[u8; 32]>) {
        let Some(token_files) = &self.token_files else {
            return reject_duplicate_token_digests(
                self.admin_token_digest,
                self.read_only_token_digest,
            );
        };
        let mut token_files = token_files.lock();
        if let Err(error) = token_files.reload_from_disk() {
            warn!(error = %error, "failed to reload control-plane auth token file");
        }
        let admin_digest = if token_files.has_admin_source() {
            token_files.admin_digest()
        } else {
            self.admin_token_digest
        };
        let read_only_digest = if token_files.has_read_only_source() {
            token_files.read_only_digest()
        } else {
            self.read_only_token_digest
        };
        reject_duplicate_token_digests(admin_digest, read_only_digest)
    }

    fn allowed_origin(&self, headers: &HeaderMap) -> Result<Option<HeaderValue>, ()> {
        let Some(origin) = headers.get(ORIGIN) else {
            return Ok(None);
        };
        let origin_str = origin.to_str().map_err(|_| ())?;
        let allowed = if self.cors.is_loopback_policy() {
            is_loopback_control_plane_origin(origin_str)
        } else {
            self.cors
                .allowed_origins
                .iter()
                .any(|allowed_origin| allowed_origin == origin_str)
        };
        if allowed {
            Ok(Some(origin.clone()))
        } else {
            Err(())
        }
    }

    fn preflight_response(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
        remote_addr: Option<std::net::SocketAddr>,
    ) -> Response {
        let cors_origin = match self.allowed_origin(headers) {
            Ok(cors_origin) => cors_origin,
            Err(()) => {
                self.record_cors_rejection(method, path, headers, remote_addr);
                return problem_response(
                    StatusCode::FORBIDDEN,
                    "cors",
                    "origin_not_allowed",
                    "control-plane CORS origin is not allowed",
                );
            }
        };
        with_control_plane_cors(StatusCode::NO_CONTENT.into_response(), cors_origin)
    }

    fn record_cors_rejection(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
        remote_addr: Option<std::net::SocketAddr>,
    ) {
        let Some(monitor) = &self.monitor else {
            return;
        };
        monitor.record_cors_rejection(method, path, headers, remote_addr);
    }

    fn cors_origin_or_reject(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
        remote_addr: Option<std::net::SocketAddr>,
    ) -> Result<Option<HeaderValue>, Response> {
        self.allowed_origin(headers).map_err(|()| {
            self.record_cors_rejection(method, path, headers, remote_addr);
            problem_response(
                StatusCode::FORBIDDEN,
                "cors",
                "origin_not_allowed",
                "control-plane CORS origin is not allowed",
            )
        })
    }

    fn reject_with_audit(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
        failure: AuthFailure,
        required_access: ControlPlaneAccess,
        remote_addr: Option<std::net::SocketAddr>,
    ) -> Response {
        let rate_limited = self.monitor.as_ref().is_some_and(|monitor| {
            monitor.record_auth_failure(
                method,
                path,
                headers,
                failure,
                required_access,
                remote_addr,
            )
        });
        if rate_limited {
            AuthFailure::RateLimited.into_response()
        } else {
            failure.into_response()
        }
    }
}

fn reject_duplicate_token_digests(
    admin_digest: Option<[u8; 32]>,
    read_only_digest: Option<[u8; 32]>,
) -> (Option<[u8; 32]>, Option<[u8; 32]>) {
    if matches!(
        (admin_digest.as_ref(), read_only_digest.as_ref()),
        (Some(admin), Some(read_only)) if admin.ct_eq(read_only).into()
    ) {
        warn!(
            "control-plane admin and read-only bearer tokens are identical; refusing all bearer tokens until the configured tokens are distinct"
        );
        return (None, None);
    }
    (admin_digest, read_only_digest)
}

pub(crate) async fn control_plane_auth_middleware(
    State(authorizer): State<Arc<ControlPlaneAuthorizer>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let remote_addr = request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|connect_info| connect_info.0);
    if request.method() == Method::OPTIONS {
        return authorizer.preflight_response(&method, &path, request.headers(), remote_addr);
    }

    if !authorizer.is_enabled() {
        let cors_origin = match authorizer.cors_origin_or_reject(
            &method,
            &path,
            request.headers(),
            remote_addr,
        ) {
            Ok(cors_origin) => cors_origin,
            Err(response) => return response,
        };
        return with_control_plane_cors(next.run(request).await, cors_origin);
    }

    let required_access = required_access_for_request(&method, &path);
    let cors_origin =
        match authorizer.cors_origin_or_reject(&method, &path, request.headers(), remote_addr) {
            Ok(cors_origin) => cors_origin,
            Err(response) => return response,
        };
    let response = match authorizer.authorize(request.headers()) {
        Ok(ControlPlaneAccess::Admin) => next.run(request).await,
        Ok(actual_access) if required_access == actual_access => next.run(request).await,
        Ok(ControlPlaneAccess::ReadOnly) => authorizer.reject_with_audit(
            &method,
            &path,
            request.headers(),
            AuthFailure::InsufficientAccess,
            required_access,
            remote_addr,
        ),
        Err(error) => authorizer.reject_with_audit(
            &method,
            &path,
            request.headers(),
            error,
            required_access,
            remote_addr,
        ),
    };
    with_control_plane_cors(response, cors_origin)
}

fn required_access_for_request(method: &Method, path: &str) -> ControlPlaneAccess {
    if !matches!(method, &Method::GET | &Method::HEAD | &Method::OPTIONS) {
        return ControlPlaneAccess::Admin;
    }
    read_access_for_path(path).unwrap_or(ControlPlaneAccess::Admin)
}

fn read_access_for_path(path: &str) -> Option<ControlPlaneAccess> {
    let parts = path.split('/').collect::<Vec<_>>();
    if read_path_requires_admin(parts.as_slice()) {
        return Some(ControlPlaneAccess::Admin);
    }
    if read_path_allows_read_only(parts.as_slice()) {
        return Some(ControlPlaneAccess::ReadOnly);
    }
    None
}

#[cfg(test)]
pub(crate) fn read_path_has_explicit_access(path: &str) -> bool {
    read_access_for_path(path).is_some()
}

fn read_path_requires_admin(parts: &[&str]) -> bool {
    matches!(parts, ["", "v1", "assets", _, "raw", ..])
        || matches!(parts, ["", "v1", "runs", _, "debug", ..])
        || matches!(parts, ["", "v1", "runtime", "auth", "subjects", _, ..])
        || matches!(parts, ["", "v1", "runtime", "auth", "leases", _, ..])
        || matches!(parts, ["", "v1", "runtime", "secrets", ..])
        || matches!(parts, ["", "v1", "runtime", "auth", "accounts", ..])
        || matches!(parts, ["", "v1", "runtime", "hooks", ..])
        || matches!(parts, ["", "v1", "runtime", "revisions"])
        || matches!(parts, ["", "v1", "stacks"])
        || matches!(parts, ["", "v1", "stacks", _, "ledger"])
}

fn read_path_allows_read_only(parts: &[&str]) -> bool {
    matches!(
        parts,
        ["", "v1", "status"]
            | ["", "v1", "capabilities"]
            | ["", "v1", "openapi.json"]
            | ["", "v1", "runtime"]
            | ["", "v1", "runtime", "subagent-policy", "quotas"]
            | ["", "v1", "runtime", "learning-policy"]
            | ["", "v1", "runtime", "run-memory-policy"]
            | ["", "v1", "runtime", "tool-limits"]
            | ["", "v1", "runtime", "connectors"]
            | ["", "v1", "runtime", "connectors", _, _]
            | ["", "v1", "runtime", "deliveries", "metrics"]
            | ["", "v1", "events", "stream"]
            | ["", "v1", "sessions"]
            | ["", "v1", "sessions", _]
            | ["", "v1", "sessions", _, "goal"]
            | ["", "v1", "sessions", _, "events"]
            | ["", "v1", "sessions", _, "stream"]
            | ["", "v1", "sessions", _, "questions"]
            | ["", "v1", "sessions", _, "memory-context"]
            | ["", "v1", "sessions", _, "memory-search"]
            | ["", "v1", "sessions", _, "permission-audits"]
            | ["", "v1", "sessions", _, "skills"]
            | ["", "v1", "sessions", _, "operator"]
            | ["", "v1", "sessions", _, "tool-overrides"]
            | ["", "v1", "sessions", _, "reply-targets"]
            | ["", "v1", "sessions", _, "tasks"]
            | ["", "v1", "sessions", _, "tasks", _]
            | ["", "v1", "sessions", _, "tasks", _, "output"]
            | ["", "v1", "runs"]
            | ["", "v1", "runs", _]
            | ["", "v1", "runs", _, "events"]
            | ["", "v1", "runs", _, "stream"]
            | ["", "v1", "runs", _, "external-actions"]
            | ["", "v1", "questions"]
            | ["", "v1", "agents"]
            | ["", "v1", "agents", "audit"]
            | ["", "v1", "agents", "summaries"]
            | ["", "v1", "agents", _]
            | ["", "v1", "agents", _, "audit"]
            | ["", "v1", "agents", _, "mailbox"]
            | ["", "v1", "agents", _, "mailbox", "dead-letter"]
            | ["", "v1", "assets"]
            | ["", "v1", "assets", _]
            | ["", "v1", "assets", _, "references"]
            | ["", "v1", "capture-alerts"]
            | ["", "v1", "observation-audit"]
            | ["", "v1", "observations"]
            | ["", "v1", "observations", _]
            | ["", "v1", "observation-transcripts"]
            | ["", "v1", "observation-transcripts", _]
            | ["", "v1", "observation-transcripts", _, "segments"]
            | ["", "v1", "skills"]
            | ["", "v1", "skills", _]
            | ["", "v1", "boards"]
            | ["", "v1", "boards", _]
            | ["", "v1", "boards", _, "revisions"]
            | ["", "v1", "boards", _, "revisions", _]
            | ["", "v1", "capture-agents"]
            | ["", "v1", "capture-agents", _]
            | ["", "v1", "channels"]
            | ["", "v1", "channels", _]
            | ["", "v1", "channels", _, "members"]
            | ["", "v1", "channels", _, "messages"]
            | ["", "v1", "channels", _, "stimuli"]
            | ["", "v1", "channels", _, "thread-work"]
            | ["", "v1", "channels", _, "leases"]
            | ["", "v1", "projects"]
            | ["", "v1", "projects", _]
            | ["", "v1", "projects", _, "members"]
            | ["", "v1", "projects", _, "channels"]
            | ["", "v1", "projects", _, "tasks"]
            | ["", "v1", "projects", _, "tasks", _]
            | ["", "v1", "playbooks"]
            | ["", "v1", "playbooks", _]
            | ["", "v1", "flows"]
            | ["", "v1", "flows", _]
            | ["", "v1", "flows", _, "stream"]
            | ["", "v1", "schedules"]
            | ["", "v1", "schedules", _]
            | ["", "v1", "deliveries"]
            | ["", "v1", "deliveries", "dead-letter"]
            | ["", "v1", "deliveries", _]
            | ["", "v1", "observation-sources"]
            | ["", "v1", "observation-sources", _]
            | ["", "v1", "derivations"]
            | ["", "v1", "derivations", _]
            | ["", "v1", "learning-candidates"]
            | ["", "v1", "learning-candidates", _]
            | ["", "v1", "learnings"]
            | ["", "v1", "learnings", _]
            | ["", "v1", "learning-skills"]
            | ["", "v1", "learning-skills", _]
            | ["", "v1", "personas"]
            | ["", "v1", "personas", _]
    )
}

/// Applies local-control-plane CORS headers to a response when an origin is allowed.
fn with_control_plane_cors(mut response: Response, origin: Option<HeaderValue>) -> Response {
    let Some(origin) = origin else {
        return response;
    };
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("access-control-allow-origin"),
        origin,
    );
    headers.insert(
        HeaderName::from_static("access-control-allow-methods"),
        HeaderValue::from_static("GET,HEAD,POST,PUT,PATCH,DELETE,OPTIONS"),
    );
    headers.insert(
        HeaderName::from_static("access-control-allow-headers"),
        HeaderValue::from_static(
            "authorization,content-type,x-kheish-daemon-token,x-kheish-daemon-url",
        ),
    );
    headers.insert(
        HeaderName::from_static("access-control-max-age"),
        HeaderValue::from_static("600"),
    );
    headers.append(
        HeaderName::from_static("vary"),
        HeaderValue::from_static("origin"),
    );
    headers.append(
        HeaderName::from_static("vary"),
        HeaderValue::from_static("access-control-request-method"),
    );
    headers.append(
        HeaderName::from_static("vary"),
        HeaderValue::from_static("access-control-request-headers"),
    );
    response
}

pub(crate) fn parse_bearer_token(headers: &HeaderMap) -> Result<&str, AuthFailure> {
    let header = headers
        .get(AUTHORIZATION)
        .ok_or(AuthFailure::MissingToken)?
        .to_str()
        .map_err(|_| AuthFailure::MalformedToken)?;
    let (scheme, token) = header.split_once(' ').ok_or(AuthFailure::MalformedToken)?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.trim().is_empty() {
        return Err(AuthFailure::MalformedToken);
    }
    Ok(token.trim())
}

pub(crate) fn digest_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthFailure {
    MissingToken,
    MalformedToken,
    InvalidToken,
    InsufficientAccess,
    RateLimited,
}

impl AuthFailure {
    fn code(self) -> &'static str {
        match self {
            Self::MissingToken => "missing_token",
            Self::MalformedToken => "malformed_token",
            Self::InvalidToken => "invalid_token",
            Self::InsufficientAccess => "insufficient_access",
            Self::RateLimited => "rate_limited",
        }
    }
}

impl IntoResponse for AuthFailure {
    fn into_response(self) -> Response {
        match self {
            Self::RateLimited => {
                let mut response = problem_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    "auth",
                    self.code(),
                    "too many failed daemon auth attempts",
                );
                response.headers_mut().insert(
                    HeaderName::from_static("retry-after"),
                    HeaderValue::from_static("60"),
                );
                response
            }
            Self::InsufficientAccess => problem_response(
                StatusCode::FORBIDDEN,
                "auth",
                self.code(),
                "daemon token does not allow this operation",
            ),
            Self::MissingToken | Self::MalformedToken | Self::InvalidToken => {
                let mut response = problem_response(
                    StatusCode::UNAUTHORIZED,
                    "auth",
                    self.code(),
                    "missing or invalid daemon bearer token",
                );
                response.headers_mut().insert(
                    WWW_AUTHENTICATE,
                    "Bearer realm=\"kheish-daemon\""
                        .parse()
                        .expect("static bearer challenge"),
                );
                response
            }
        }
    }
}

fn problem_response(
    status: StatusCode,
    domain: &'static str,
    code: &'static str,
    detail: &'static str,
) -> Response {
    let problem = ProblemDetails::new(status.as_u16(), code, detail).with_domain(domain);
    (
        status,
        [(CONTENT_TYPE, "application/problem+json")],
        Json(problem),
    )
        .into_response()
}

#[derive(Debug)]
struct ControlPlaneTokenFileDigests {
    admin: Option<ControlPlaneTokenFileDigest>,
    read_only: Option<ControlPlaneTokenFileDigest>,
}

impl ControlPlaneTokenFileDigests {
    fn new(
        token_files: ControlPlaneAuthTokenFiles,
        admin_digest: Option<[u8; 32]>,
        read_only_digest: Option<[u8; 32]>,
    ) -> Option<Self> {
        if token_files.is_empty() {
            return None;
        }
        Some(Self {
            admin: token_files
                .admin_token_file
                .map(|path| ControlPlaneTokenFileDigest::new(path, admin_digest)),
            read_only: token_files
                .read_only_token_file
                .map(|path| ControlPlaneTokenFileDigest::new(path, read_only_digest)),
        })
    }

    fn reload_from_disk(&mut self) -> std::io::Result<()> {
        let mut first_error = None;
        if let Some(admin) = &mut self.admin {
            if let Err(error) = admin.reload_from_disk() {
                first_error.get_or_insert(error);
            }
        }
        if let Some(read_only) = &mut self.read_only {
            if let Err(error) = read_only.reload_from_disk() {
                first_error.get_or_insert(error);
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    fn admin_digest(&self) -> Option<[u8; 32]> {
        self.admin.as_ref().and_then(|token| token.digest)
    }

    fn read_only_digest(&self) -> Option<[u8; 32]> {
        self.read_only.as_ref().and_then(|token| token.digest)
    }

    fn has_admin_source(&self) -> bool {
        self.admin.is_some()
    }

    fn has_read_only_source(&self) -> bool {
        self.read_only.is_some()
    }
}

#[derive(Debug)]
struct ControlPlaneTokenFileDigest {
    path: PathBuf,
    digest: Option<[u8; 32]>,
}

impl ControlPlaneTokenFileDigest {
    fn new(path: PathBuf, digest: Option<[u8; 32]>) -> Self {
        Self { path, digest }
    }

    fn reload_from_disk(&mut self) -> std::io::Result<()> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => {
                let token = raw.trim();
                if token.is_empty() {
                    self.digest = None;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "control-plane auth token file is empty",
                    ));
                }
                self.digest = Some(digest_token(token));
                Ok(())
            }
            Err(error) => {
                self.digest = None;
                Err(error)
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ControlPlaneAuthRateLimit {
    max_failures: usize,
    window_ms: u64,
}

impl Default for ControlPlaneAuthRateLimit {
    fn default() -> Self {
        Self {
            max_failures: DEFAULT_AUTH_FAILURE_RATE_LIMIT_MAX,
            window_ms: DEFAULT_AUTH_FAILURE_RATE_LIMIT_WINDOW_MS,
        }
    }
}

#[derive(Debug)]
struct ControlPlaneAuthMonitor {
    audit_path: PathBuf,
    rate_limit: ControlPlaneAuthRateLimit,
    limiter: Mutex<AuthFailureRateLimiter>,
    audit_lock: Mutex<()>,
}

impl ControlPlaneAuthMonitor {
    fn new(audit_path: PathBuf, rate_limit: ControlPlaneAuthRateLimit) -> Self {
        Self {
            audit_path,
            rate_limit,
            limiter: Mutex::new(AuthFailureRateLimiter::default()),
            audit_lock: Mutex::new(()),
        }
    }

    fn record_auth_failure(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
        failure: AuthFailure,
        required_access: ControlPlaneAccess,
        remote_addr: Option<std::net::SocketAddr>,
    ) -> bool {
        let now_ms = crate::now_ms();
        match self.limiter.lock().note_failure(
            auth_rate_limit_bucket(failure.code(), remote_addr),
            now_ms,
            self.rate_limit,
        ) {
            AuthFailureAuditDecision::Allowed => {
                self.append_audit_record(&ControlPlaneAuthAuditRecord {
                    timestamp_ms: now_ms,
                    event: "auth_failure",
                    method: method.as_str(),
                    path,
                    reason: failure.code(),
                    origin: sanitized_origin(headers),
                    remote_addr: remote_addr.map(|address| address.to_string()),
                    required_access: Some(required_access.as_str()),
                });
                false
            }
            AuthFailureAuditDecision::RateLimited { audit_transition } => {
                if audit_transition {
                    self.append_audit_record(&ControlPlaneAuthAuditRecord {
                        timestamp_ms: now_ms,
                        event: "auth_rate_limited",
                        method: method.as_str(),
                        path,
                        reason: failure.code(),
                        origin: sanitized_origin(headers),
                        remote_addr: remote_addr.map(|address| address.to_string()),
                        required_access: Some(required_access.as_str()),
                    });
                }
                true
            }
        }
    }

    fn record_cors_rejection(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
        remote_addr: Option<std::net::SocketAddr>,
    ) {
        let now_ms = crate::now_ms();
        let decision = self.limiter.lock().note_failure(
            auth_rate_limit_bucket("cors_origin_rejected", remote_addr),
            now_ms,
            self.rate_limit,
        );
        match decision {
            AuthFailureAuditDecision::Allowed => {
                self.append_audit_record(&ControlPlaneAuthAuditRecord {
                    timestamp_ms: now_ms,
                    event: "cors_origin_rejected",
                    method: method.as_str(),
                    path,
                    reason: "origin_not_allowed",
                    origin: sanitized_origin(headers),
                    remote_addr: remote_addr.map(|address| address.to_string()),
                    required_access: None,
                });
            }
            AuthFailureAuditDecision::RateLimited { audit_transition } => {
                if audit_transition {
                    self.append_audit_record(&ControlPlaneAuthAuditRecord {
                        timestamp_ms: now_ms,
                        event: "cors_origin_rate_limited",
                        method: method.as_str(),
                        path,
                        reason: "origin_not_allowed",
                        origin: sanitized_origin(headers),
                        remote_addr: remote_addr.map(|address| address.to_string()),
                        required_access: None,
                    });
                }
            }
        }
    }

    fn append_audit_record(&self, record: &ControlPlaneAuthAuditRecord<'_>) {
        if let Err(error) = self.try_append_audit_record(record) {
            warn!(
                audit_path = %self.audit_path.display(),
                error = %error,
                "failed to append control-plane auth audit record"
            );
        }
    }

    fn try_append_audit_record(
        &self,
        record: &ControlPlaneAuthAuditRecord<'_>,
    ) -> std::io::Result<()> {
        let _guard = self.audit_lock.lock();
        if let Some(parent) = self.audit_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut line = serde_json::to_vec(record)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))?;
        line.push(b'\n');
        let mut file = options.open(&self.audit_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&line)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthFailureAuditDecision {
    Allowed,
    RateLimited { audit_transition: bool },
}

#[derive(Default, Debug)]
struct AuthFailureRateLimiter {
    failures: VecDeque<AuthFailureRateSample>,
    rate_limit_audits: VecDeque<AuthFailureRateSample>,
}

impl AuthFailureRateLimiter {
    fn note_failure(
        &mut self,
        bucket: String,
        now_ms: u64,
        rate_limit: ControlPlaneAuthRateLimit,
    ) -> AuthFailureAuditDecision {
        if rate_limit.max_failures == 0 {
            return AuthFailureAuditDecision::Allowed;
        }
        prune_rate_limit_samples(&mut self.failures, now_ms, rate_limit.window_ms);
        prune_rate_limit_samples(&mut self.rate_limit_audits, now_ms, rate_limit.window_ms);

        let recent_bucket_failures = self
            .failures
            .iter()
            .filter(|sample| sample.bucket == bucket)
            .count();
        self.failures.push_back(AuthFailureRateSample {
            timestamp_ms: now_ms,
            bucket: bucket.clone(),
        });
        cap_rate_limit_samples(&mut self.failures);
        if recent_bucket_failures < rate_limit.max_failures {
            return AuthFailureAuditDecision::Allowed;
        }

        let already_audited = self
            .rate_limit_audits
            .iter()
            .any(|sample| sample.bucket == bucket);
        if !already_audited {
            self.rate_limit_audits.push_back(AuthFailureRateSample {
                timestamp_ms: now_ms,
                bucket,
            });
            cap_rate_limit_samples(&mut self.rate_limit_audits);
        }
        AuthFailureAuditDecision::RateLimited {
            audit_transition: !already_audited,
        }
    }
}

fn prune_rate_limit_samples(
    samples: &mut VecDeque<AuthFailureRateSample>,
    now_ms: u64,
    window_ms: u64,
) {
    while samples
        .front()
        .is_some_and(|sample| now_ms.saturating_sub(sample.timestamp_ms) > window_ms)
    {
        samples.pop_front();
    }
}

fn cap_rate_limit_samples(samples: &mut VecDeque<AuthFailureRateSample>) {
    while samples.len() > MAX_AUTH_FAILURE_RATE_LIMIT_SAMPLES {
        samples.pop_front();
    }
}

fn auth_rate_limit_bucket(reason: &str, remote_addr: Option<std::net::SocketAddr>) -> String {
    match remote_addr {
        Some(remote_addr) => format!("{reason}:{}", remote_addr.ip()),
        None => format!("{reason}:unknown"),
    }
}

#[derive(Debug)]
struct AuthFailureRateSample {
    timestamp_ms: u64,
    bucket: String,
}

#[derive(Serialize)]
struct ControlPlaneAuthAuditRecord<'a> {
    timestamp_ms: u64,
    event: &'static str,
    method: &'a str,
    path: &'a str,
    reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    required_access: Option<&'static str>,
}

fn sanitized_origin(headers: &HeaderMap) -> Option<String> {
    headers.get(ORIGIN)?.to_str().ok().map(|origin| {
        origin
            .chars()
            .filter(|character| !character.is_control())
            .take(512)
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::routing::get;
    use axum::{Router, middleware};
    use reqwest::Client;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    use super::*;

    async fn spawn_auth_router(
        authorizer: ControlPlaneAuthorizer,
    ) -> anyhow::Result<(String, oneshot::Sender<()>)> {
        async fn get_ok() -> &'static str {
            "GET_OK"
        }

        async fn post_ok() -> &'static str {
            "POST_OK"
        }

        let router = Router::new()
            .route("/resource", get(get_ok).post(post_ok))
            .route("/v1/status", get(get_ok))
            .route("/v1/assets/{asset_id}/raw", get(get_ok))
            .route(
                "/v1/runs/{run_id}/debug/artifacts/{artifact_id}",
                get(get_ok),
            )
            .route("/v1/runtime/secrets", get(get_ok))
            .route("/v1/runtime/secrets/{secret_ref}", get(get_ok))
            .route("/v1/runtime/auth/subjects/{subject_id}", get(get_ok))
            .route("/v1/runtime/auth/leases/{lease_id}", get(get_ok))
            .route("/v1/runtime/auth/accounts", get(get_ok))
            .route("/v1/runtime/auth/accounts/{slot_id}", get(get_ok))
            .route("/v1/runtime/hooks", get(get_ok))
            .route("/v1/runtime/hooks/dead-letter", get(get_ok))
            .route("/v1/runtime/revisions", get(get_ok))
            .layer(middleware::from_fn_with_state(
                Arc::new(authorizer),
                control_plane_auth_middleware,
            ));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address: SocketAddr = listener.local_addr()?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        Ok((format!("http://{address}"), shutdown_tx))
    }

    async fn assert_problem_response(
        response: reqwest::Response,
        status: StatusCode,
        code: &str,
    ) -> anyhow::Result<ProblemDetails> {
        assert_eq!(response.status(), status);
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .expect("problem response content-type")
            .to_str()?
            .to_string();
        assert!(
            content_type.starts_with("application/problem+json"),
            "unexpected content-type {content_type}"
        );
        let problem = response.json::<ProblemDetails>().await?;
        assert_eq!(problem.status, status.as_u16());
        assert_eq!(problem.code, code);
        Ok(problem)
    }

    #[tokio::test]
    async fn disabled_authorizer_allows_requests_without_token() -> anyhow::Result<()> {
        let (base, shutdown) = spawn_auth_router(ControlPlaneAuthorizer::new(
            &ControlPlaneAuthConfig::disabled(),
        ))
        .await?;
        let client = Client::new();
        let response = client.get(format!("{base}/resource")).send().await?;
        assert_eq!(response.status(), StatusCode::OK);
        shutdown.send(()).ok();
        Ok(())
    }

    #[tokio::test]
    async fn read_only_token_can_read_but_not_write() -> anyhow::Result<()> {
        let (base, shutdown) =
            spawn_auth_router(ControlPlaneAuthorizer::new(&ControlPlaneAuthConfig {
                admin_token: Some("admin-secret".to_string()),
                read_only_token: Some("readonly-secret".to_string()),
            }))
            .await?;
        let client = Client::new();
        let get_response = client
            .get(format!("{base}/v1/status"))
            .bearer_auth("readonly-secret")
            .send()
            .await?;
        assert_eq!(get_response.status(), StatusCode::OK);
        let post_response = client
            .post(format!("{base}/resource"))
            .bearer_auth("readonly-secret")
            .send()
            .await?;
        assert_problem_response(post_response, StatusCode::FORBIDDEN, "insufficient_access")
            .await?;
        shutdown.send(()).ok();
        Ok(())
    }

    #[tokio::test]
    async fn read_only_token_cannot_read_sensitive_paths() -> anyhow::Result<()> {
        let (base, shutdown) =
            spawn_auth_router(ControlPlaneAuthorizer::new(&ControlPlaneAuthConfig {
                admin_token: Some("admin-secret".to_string()),
                read_only_token: Some("readonly-secret".to_string()),
            }))
            .await?;
        let client = Client::new();
        for path in [
            "/v1/assets/asset-1/raw",
            "/v1/runs/run-1/debug/artifacts/request",
            "/v1/runtime/secrets",
            "/v1/runtime/secrets/openai",
            "/v1/runtime/auth/subjects/session:demo",
            "/v1/runtime/auth/leases/lease-1",
            "/v1/runtime/auth/accounts",
            "/v1/runtime/auth/accounts/openai.prod",
            "/v1/runtime/hooks",
            "/v1/runtime/hooks/dead-letter",
            "/v1/runtime/revisions",
        ] {
            let read_only_response = client
                .get(format!("{base}{path}"))
                .bearer_auth("readonly-secret")
                .send()
                .await?;
            assert_eq!(
                read_only_response.status(),
                StatusCode::FORBIDDEN,
                "{path} should reject read-only access",
            );

            let admin_response = client
                .get(format!("{base}{path}"))
                .bearer_auth("admin-secret")
                .send()
                .await?;
            assert_eq!(
                admin_response.status(),
                StatusCode::OK,
                "{path} should allow admin access",
            );
        }
        shutdown.send(()).ok();
        Ok(())
    }

    #[test]
    fn sensitive_read_paths_require_admin_access() {
        assert_eq!(
            required_access_for_request(&Method::GET, "/v1/assets/asset-1/raw"),
            ControlPlaneAccess::Admin
        );
        assert_eq!(
            required_access_for_request(&Method::HEAD, "/v1/runs/run-1/debug/artifacts/request"),
            ControlPlaneAccess::Admin
        );
        assert_eq!(
            required_access_for_request(&Method::GET, "/v1/runtime/auth/subjects/session:demo"),
            ControlPlaneAccess::Admin
        );
        assert_eq!(
            required_access_for_request(&Method::GET, "/v1/runtime/auth/leases/lease-1"),
            ControlPlaneAccess::Admin
        );
        assert_eq!(
            required_access_for_request(&Method::GET, "/v1/runtime/secrets"),
            ControlPlaneAccess::Admin
        );
        assert_eq!(
            required_access_for_request(&Method::GET, "/v1/runtime/auth/accounts"),
            ControlPlaneAccess::Admin
        );
        assert_eq!(
            required_access_for_request(&Method::GET, "/v1/runtime/hooks"),
            ControlPlaneAccess::Admin
        );
        assert_eq!(
            required_access_for_request(&Method::GET, "/v1/runtime/hooks/dead-letter"),
            ControlPlaneAccess::Admin
        );
        assert_eq!(
            required_access_for_request(&Method::GET, "/v1/runtime/revisions"),
            ControlPlaneAccess::Admin
        );
        assert_eq!(
            required_access_for_request(&Method::GET, "/v1/stacks"),
            ControlPlaneAccess::Admin
        );
        assert_eq!(
            required_access_for_request(&Method::GET, "/v1/stacks/demo/ledger"),
            ControlPlaneAccess::Admin
        );
        assert_eq!(
            required_access_for_request(&Method::GET, "/v1/runs/run-1"),
            ControlPlaneAccess::ReadOnly
        );
    }

    #[test]
    fn read_only_access_matrix_covers_known_read_routes() {
        for path in [
            "/v1/status",
            "/v1/capabilities",
            "/v1/openapi.json",
            "/v1/runtime",
            "/v1/runtime/subagent-policy/quotas",
            "/v1/runtime/learning-policy",
            "/v1/runtime/run-memory-policy",
            "/v1/runtime/tool-limits",
            "/v1/runtime/connectors",
            "/v1/runtime/connectors/external/metrics",
            "/v1/runtime/deliveries/metrics",
            "/v1/runtime/connectors/slack/main",
            "/v1/events/stream",
            "/v1/sessions",
            "/v1/sessions/session-1",
            "/v1/sessions/session-1/goal",
            "/v1/sessions/session-1/events",
            "/v1/sessions/session-1/stream",
            "/v1/sessions/session-1/questions",
            "/v1/sessions/session-1/memory-context",
            "/v1/sessions/session-1/memory-search",
            "/v1/sessions/session-1/permission-audits",
            "/v1/sessions/session-1/skills",
            "/v1/sessions/session-1/operator",
            "/v1/sessions/session-1/reply-targets",
            "/v1/sessions/session-1/tasks",
            "/v1/sessions/session-1/tasks/task-1",
            "/v1/sessions/session-1/tasks/task-1/output",
            "/v1/runs",
            "/v1/runs/run-1",
            "/v1/runs/run-1/events",
            "/v1/runs/run-1/stream",
            "/v1/runs/run-1/external-actions",
            "/v1/questions",
            "/v1/agents",
            "/v1/agents/audit",
            "/v1/agents/summaries",
            "/v1/agents/agent-1",
            "/v1/agents/agent-1/audit",
            "/v1/agents/agent-1/mailbox",
            "/v1/agents/agent-1/mailbox/dead-letter",
            "/v1/assets",
            "/v1/assets/asset-1",
            "/v1/assets/asset-1/references",
            "/v1/boards",
            "/v1/boards/board-1",
            "/v1/boards/board-1/revisions",
            "/v1/boards/board-1/revisions/revision-1",
            "/v1/capture-agents",
            "/v1/capture-agents/machine-1",
            "/v1/capture-alerts",
            "/v1/channels",
            "/v1/channels/channel-1",
            "/v1/channels/channel-1/members",
            "/v1/channels/channel-1/messages",
            "/v1/channels/channel-1/stimuli",
            "/v1/channels/channel-1/thread-work",
            "/v1/channels/channel-1/leases",
            "/v1/projects",
            "/v1/projects/project-1",
            "/v1/projects/project-1/members",
            "/v1/projects/project-1/channels",
            "/v1/projects/project-1/tasks",
            "/v1/projects/project-1/tasks/task-1",
            "/v1/playbooks",
            "/v1/playbooks/playbook-1",
            "/v1/flows",
            "/v1/flows/flow-1",
            "/v1/flows/flow-1/stream",
            "/v1/schedules",
            "/v1/schedules/schedule-1",
            "/v1/deliveries",
            "/v1/deliveries/dead-letter",
            "/v1/deliveries/delivery-1",
            "/v1/observation-sources",
            "/v1/observation-sources/source-1",
            "/v1/observation-audit",
            "/v1/observations",
            "/v1/observations/observation-1",
            "/v1/derivations",
            "/v1/derivations/derivation-1",
            "/v1/learning-candidates",
            "/v1/learning-candidates/candidate-1",
            "/v1/learnings",
            "/v1/learnings/learning-1",
            "/v1/learning-skills",
            "/v1/learning-skills/skill-1",
            "/v1/skills",
            "/v1/skills/skill-1",
            "/v1/personas",
            "/v1/personas/persona-1",
        ] {
            assert_eq!(
                required_access_for_request(&Method::GET, path),
                ControlPlaneAccess::ReadOnly,
                "{path} should be explicitly read-only",
            );
        }
    }

    #[test]
    fn unknown_read_paths_require_admin_access() {
        for path in [
            "/v1/runtime/unknown",
            "/v1/projects/project-1/secrets",
            "/v1/assets/asset-1/raw/download",
            "/v1/not-a-route",
        ] {
            assert_eq!(
                required_access_for_request(&Method::GET, path),
                ControlPlaneAccess::Admin,
                "{path} should require admin by default",
            );
        }
    }

    #[tokio::test]
    async fn missing_or_invalid_token_is_rejected() -> anyhow::Result<()> {
        let (base, shutdown) =
            spawn_auth_router(ControlPlaneAuthorizer::new(&ControlPlaneAuthConfig {
                admin_token: Some("admin-secret".to_string()),
                read_only_token: None,
            }))
            .await?;
        let client = Client::new();
        let missing = client.get(format!("{base}/resource")).send().await?;
        assert_problem_response(missing, StatusCode::UNAUTHORIZED, "missing_token").await?;
        let invalid = client
            .get(format!("{base}/resource"))
            .bearer_auth("wrong")
            .send()
            .await?;
        assert_problem_response(invalid, StatusCode::UNAUTHORIZED, "invalid_token").await?;
        shutdown.send(()).ok();
        Ok(())
    }

    #[tokio::test]
    async fn local_browser_preflight_does_not_require_token() -> anyhow::Result<()> {
        let (base, shutdown) =
            spawn_auth_router(ControlPlaneAuthorizer::new(&ControlPlaneAuthConfig {
                admin_token: Some("admin-secret".to_string()),
                read_only_token: Some("readonly-secret".to_string()),
            }))
            .await?;
        let client = Client::new();
        let response = client
            .request(reqwest::Method::OPTIONS, format!("{base}/resource"))
            .header("origin", "http://127.0.0.1:5173")
            .header("access-control-request-method", "GET")
            .header(
                "access-control-request-headers",
                "authorization,content-type",
            )
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .expect("allow origin")
                .to_str()?,
            "http://127.0.0.1:5173"
        );
        assert!(
            response
                .headers()
                .get("access-control-allow-headers")
                .expect("allow headers")
                .to_str()?
                .contains("authorization")
        );
        shutdown.send(()).ok();
        Ok(())
    }

    #[tokio::test]
    async fn auth_errors_include_cors_for_local_browser_origins() -> anyhow::Result<()> {
        let (base, shutdown) =
            spawn_auth_router(ControlPlaneAuthorizer::new(&ControlPlaneAuthConfig {
                admin_token: Some("admin-secret".to_string()),
                read_only_token: None,
            }))
            .await?;
        let client = Client::new();
        let response = client
            .get(format!("{base}/resource"))
            .header("origin", "http://localhost:5173")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .expect("allow origin")
                .to_str()?,
            "http://localhost:5173"
        );
        shutdown.send(()).ok();
        Ok(())
    }

    #[tokio::test]
    async fn exact_cors_allowlist_only_allows_configured_origin() -> anyhow::Result<()> {
        let (base, shutdown) = spawn_auth_router(ControlPlaneAuthorizer::with_cors(
            &ControlPlaneAuthConfig {
                admin_token: Some("admin-secret".to_string()),
                read_only_token: None,
            },
            &ControlPlaneCorsConfig::exact(vec!["http://127.0.0.1:5173".to_string()]),
        ))
        .await?;
        let client = Client::new();
        let allowed = client
            .request(reqwest::Method::OPTIONS, format!("{base}/resource"))
            .header("origin", "http://127.0.0.1:5173")
            .header("access-control-request-method", "GET")
            .send()
            .await?;
        assert_eq!(allowed.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            allowed
                .headers()
                .get("access-control-allow-origin")
                .expect("allow origin")
                .to_str()?,
            "http://127.0.0.1:5173"
        );

        let rejected = client
            .request(reqwest::Method::OPTIONS, format!("{base}/resource"))
            .header("origin", "http://127.0.0.1:5174")
            .header("access-control-request-method", "GET")
            .send()
            .await?;
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        assert!(
            rejected
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
        shutdown.send(()).ok();
        Ok(())
    }

    #[tokio::test]
    async fn auth_errors_include_cors_only_for_exact_allowlist() -> anyhow::Result<()> {
        let (base, shutdown) = spawn_auth_router(ControlPlaneAuthorizer::with_cors(
            &ControlPlaneAuthConfig {
                admin_token: Some("admin-secret".to_string()),
                read_only_token: None,
            },
            &ControlPlaneCorsConfig::exact(vec!["http://localhost:5173".to_string()]),
        ))
        .await?;
        let client = Client::new();
        let allowed = client
            .get(format!("{base}/resource"))
            .header("origin", "http://localhost:5173")
            .send()
            .await?;
        assert_eq!(allowed.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            allowed
                .headers()
                .get("access-control-allow-origin")
                .expect("allow origin")
                .to_str()?,
            "http://localhost:5173"
        );

        let rejected = client
            .get(format!("{base}/resource"))
            .header("origin", "http://localhost:5174")
            .send()
            .await?;
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        assert!(
            rejected
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
        shutdown.send(()).ok();
        Ok(())
    }

    #[tokio::test]
    async fn non_loopback_browser_preflight_is_rejected() -> anyhow::Result<()> {
        let (base, shutdown) =
            spawn_auth_router(ControlPlaneAuthorizer::new(&ControlPlaneAuthConfig {
                admin_token: Some("admin-secret".to_string()),
                read_only_token: None,
            }))
            .await?;
        let client = Client::new();
        let response = client
            .request(reqwest::Method::OPTIONS, format!("{base}/resource"))
            .header("origin", "https://example.com")
            .header("access-control-request-method", "GET")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
        shutdown.send(()).ok();
        Ok(())
    }

    #[test]
    fn loopback_origin_parser_rejects_spoofed_hosts() {
        assert!(is_loopback_control_plane_origin("http://127.0.0.1:5173"));
        assert!(is_loopback_control_plane_origin("http://localhost:5173"));
        assert!(is_loopback_control_plane_origin("http://[::1]:5173"));
        assert!(!is_loopback_control_plane_origin(
            "http://127.0.0.1.example.com:5173"
        ));
        assert!(!is_loopback_control_plane_origin("https://example.com"));
        assert!(!is_loopback_control_plane_origin(
            "http://localhost.evil.test"
        ));
        assert!(!is_loopback_control_plane_origin("file://localhost"));
    }

    #[tokio::test]
    async fn admin_token_can_write() -> anyhow::Result<()> {
        let (base, shutdown) =
            spawn_auth_router(ControlPlaneAuthorizer::new(&ControlPlaneAuthConfig {
                admin_token: Some("admin-secret".to_string()),
                read_only_token: Some("readonly-secret".to_string()),
            }))
            .await?;
        let client = Client::new();
        let response = client
            .post(format!("{base}/resource"))
            .bearer_auth("admin-secret")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        shutdown.send(()).ok();
        Ok(())
    }

    #[tokio::test]
    async fn auth_failures_are_audited_and_rate_limited_without_leaking_tokens()
    -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let audit_path = temp.path().join("audit.jsonl");
        let (base, shutdown) = spawn_auth_router(
            ControlPlaneAuthorizer::with_cors_audit_token_files_and_rate_limit(
                &ControlPlaneAuthConfig {
                    admin_token: Some("admin-secret".to_string()),
                    read_only_token: Some("readonly-secret".to_string()),
                },
                &ControlPlaneCorsConfig::loopback(),
                ControlPlaneAuthTokenFiles::default(),
                audit_path.clone(),
                ControlPlaneAuthRateLimit {
                    max_failures: 2,
                    window_ms: 60_000,
                },
            ),
        )
        .await?;
        let client = Client::new();

        for _ in 0..2 {
            let response = client
                .get(format!("{base}/resource"))
                .bearer_auth("wrong-token")
                .send()
                .await?;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let rate_limited = client
            .get(format!("{base}/resource"))
            .bearer_auth("wrong-token")
            .send()
            .await?;
        assert_problem_response(rate_limited, StatusCode::TOO_MANY_REQUESTS, "rate_limited")
            .await?;
        let suppressed = client
            .get(format!("{base}/resource"))
            .bearer_auth("wrong-token")
            .send()
            .await?;
        assert_eq!(suppressed.status(), StatusCode::TOO_MANY_REQUESTS);

        let valid = client
            .get(format!("{base}/v1/status"))
            .bearer_auth("readonly-secret")
            .send()
            .await?;
        assert_eq!(valid.status(), StatusCode::OK);

        let audit = std::fs::read_to_string(&audit_path)?;
        assert!(audit.contains("\"event\":\"auth_failure\""));
        assert!(audit.contains("\"event\":\"auth_rate_limited\""));
        assert!(audit.contains("\"reason\":\"invalid_token\""));
        assert!(!audit.contains("wrong-token"));
        assert!(!audit.contains("readonly-secret"));
        assert!(!audit.contains("admin-secret"));
        assert_eq!(audit.lines().count(), 3);
        shutdown.send(()).ok();
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_auth_failures_produce_parseable_bounded_audit_jsonl() -> anyhow::Result<()>
    {
        let temp = tempfile::tempdir()?;
        let audit_path = temp.path().join("audit.jsonl");
        let (base, shutdown) = spawn_auth_router(
            ControlPlaneAuthorizer::with_cors_audit_token_files_and_rate_limit(
                &ControlPlaneAuthConfig {
                    admin_token: Some("admin-secret".to_string()),
                    read_only_token: None,
                },
                &ControlPlaneCorsConfig::loopback(),
                ControlPlaneAuthTokenFiles::default(),
                audit_path.clone(),
                ControlPlaneAuthRateLimit {
                    max_failures: 100,
                    window_ms: 60_000,
                },
            ),
        )
        .await?;
        let client = Client::new();
        let mut handles = Vec::new();
        for index in 0..40 {
            let client = client.clone();
            let url = format!("{base}/resource");
            handles.push(tokio::spawn(async move {
                client
                    .get(url)
                    .bearer_auth(format!("wrong-token-{index}"))
                    .send()
                    .await
                    .expect("auth request")
                    .status()
            }));
        }
        for handle in handles {
            assert_eq!(handle.await?, StatusCode::UNAUTHORIZED);
        }

        let audit = std::fs::read_to_string(&audit_path)?;
        let lines = audit.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 40);
        for line in lines {
            let value: serde_json::Value = serde_json::from_str(line)?;
            assert_eq!(value["event"], "auth_failure");
            assert_eq!(value["reason"], "invalid_token");
            assert!(!line.contains("wrong-token-"));
        }
        shutdown.send(()).ok();
        Ok(())
    }

    #[tokio::test]
    async fn rejected_cors_preflights_are_audited_without_tokens() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let audit_path = temp.path().join("audit.jsonl");
        let (base, shutdown) = spawn_auth_router(ControlPlaneAuthorizer::with_cors_and_audit(
            &ControlPlaneAuthConfig {
                admin_token: Some("admin-secret".to_string()),
                read_only_token: None,
            },
            &ControlPlaneCorsConfig::loopback(),
            audit_path.clone(),
        ))
        .await?;
        let client = Client::new();
        let response = client
            .request(reqwest::Method::OPTIONS, format!("{base}/resource"))
            .header("origin", "https://example.com")
            .header("authorization", "Bearer admin-secret")
            .header("access-control-request-method", "GET")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let audit = std::fs::read_to_string(&audit_path)?;
        assert!(audit.contains("\"event\":\"cors_origin_rejected\""));
        assert!(audit.contains("\"origin\":\"https://example.com\""));
        assert!(!audit.contains("admin-secret"));
        shutdown.send(()).ok();
        Ok(())
    }

    #[tokio::test]
    async fn cors_rejections_are_rate_limited_and_audited_once() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let audit_path = temp.path().join("audit.jsonl");
        let (base, shutdown) = spawn_auth_router(
            ControlPlaneAuthorizer::with_cors_audit_token_files_and_rate_limit(
                &ControlPlaneAuthConfig {
                    admin_token: Some("admin-secret".to_string()),
                    read_only_token: None,
                },
                &ControlPlaneCorsConfig::loopback(),
                ControlPlaneAuthTokenFiles::default(),
                audit_path.clone(),
                ControlPlaneAuthRateLimit {
                    max_failures: 1,
                    window_ms: 60_000,
                },
            ),
        )
        .await?;
        let client = Client::new();

        for _ in 0..3 {
            let response = client
                .request(reqwest::Method::OPTIONS, format!("{base}/resource"))
                .header("origin", "https://example.com")
                .header("access-control-request-method", "GET")
                .send()
                .await?;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }

        let audit = std::fs::read_to_string(&audit_path)?;
        assert_eq!(
            audit.matches("\"event\":\"cors_origin_rejected\"").count(),
            1
        );
        assert_eq!(
            audit
                .matches("\"event\":\"cors_origin_rate_limited\"")
                .count(),
            1
        );
        assert!(audit.contains("\"reason\":\"origin_not_allowed\""));
        shutdown.send(()).ok();
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn audit_append_repairs_existing_file_permissions() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir()?;
        let audit_path = temp.path().join("audit.jsonl");
        std::fs::write(&audit_path, "")?;
        std::fs::set_permissions(&audit_path, std::fs::Permissions::from_mode(0o644))?;

        let (base, shutdown) = spawn_auth_router(ControlPlaneAuthorizer::with_cors_and_audit(
            &ControlPlaneAuthConfig {
                admin_token: Some("admin-secret".to_string()),
                read_only_token: None,
            },
            &ControlPlaneCorsConfig::loopback(),
            audit_path.clone(),
        ))
        .await?;
        let client = Client::new();
        let response = client
            .get(format!("{base}/resource"))
            .bearer_auth("wrong-token")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let mode = std::fs::metadata(&audit_path)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let audit = std::fs::read_to_string(&audit_path)?;
        assert!(audit.contains("\"event\":\"auth_failure\""));
        shutdown.send(()).ok();
        Ok(())
    }

    #[tokio::test]
    async fn file_backed_tokens_reload_when_rotated() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let admin_file = temp.path().join("admin.token");
        let read_only_file = temp.path().join("readonly.token");
        std::fs::write(&admin_file, "admin-v1\n")?;
        std::fs::write(&read_only_file, "readonly-v1\n")?;
        let (base, shutdown) =
            spawn_auth_router(ControlPlaneAuthorizer::with_cors_audit_and_token_files(
                &ControlPlaneAuthConfig {
                    admin_token: Some("admin-v1".to_string()),
                    read_only_token: Some("readonly-v1".to_string()),
                },
                &ControlPlaneCorsConfig::loopback(),
                ControlPlaneAuthTokenFiles {
                    admin_token_file: Some(admin_file.clone()),
                    read_only_token_file: Some(read_only_file.clone()),
                },
                temp.path().join("audit.jsonl"),
            ))
            .await?;
        let client = Client::new();
        let first = client
            .post(format!("{base}/resource"))
            .bearer_auth("admin-v1")
            .send()
            .await?;
        assert_eq!(first.status(), StatusCode::OK);

        std::fs::write(&admin_file, "admin-v2\n")?;
        std::fs::write(&read_only_file, "readonly-v2\n")?;
        let rotated = client
            .post(format!("{base}/resource"))
            .bearer_auth("admin-v2")
            .send()
            .await?;
        assert_eq!(rotated.status(), StatusCode::OK);
        let old = client
            .post(format!("{base}/resource"))
            .bearer_auth("admin-v1")
            .send()
            .await?;
        assert_eq!(old.status(), StatusCode::UNAUTHORIZED);
        let read_only = client
            .get(format!("{base}/v1/status"))
            .bearer_auth("readonly-v2")
            .send()
            .await?;
        assert_eq!(read_only.status(), StatusCode::OK);

        std::fs::write(&admin_file, "same-token\n")?;
        std::fs::write(&read_only_file, "same-token\n")?;
        let ambiguous_admin = client
            .post(format!("{base}/resource"))
            .bearer_auth("same-token")
            .send()
            .await?;
        assert_eq!(ambiguous_admin.status(), StatusCode::UNAUTHORIZED);

        std::fs::write(&admin_file, "admin-v3-distinct\n")?;
        std::fs::write(&read_only_file, "readonly-v3-distinct\n")?;
        let recovered = client
            .post(format!("{base}/resource"))
            .bearer_auth("admin-v3-distinct")
            .send()
            .await?;
        assert_eq!(recovered.status(), StatusCode::OK);

        std::fs::write(&admin_file, "\n")?;
        let empty_admin = client
            .post(format!("{base}/resource"))
            .bearer_auth("admin-v3-distinct")
            .send()
            .await?;
        assert_eq!(empty_admin.status(), StatusCode::UNAUTHORIZED);

        std::fs::write(&read_only_file, "readonly-v4-while-admin-empty\n")?;
        let read_only_after_admin_error = client
            .get(format!("{base}/v1/status"))
            .bearer_auth("readonly-v4-while-admin-empty")
            .send()
            .await?;
        assert_eq!(read_only_after_admin_error.status(), StatusCode::OK);

        std::fs::write(&admin_file, "admin-v4-recovered\n")?;
        let recovered_again = client
            .post(format!("{base}/resource"))
            .bearer_auth("admin-v4-recovered")
            .send()
            .await?;
        assert_eq!(recovered_again.status(), StatusCode::OK);

        std::fs::remove_file(&read_only_file)?;
        let missing_read_only = client
            .get(format!("{base}/v1/status"))
            .bearer_auth("readonly-v4-while-admin-empty")
            .send()
            .await?;
        assert_eq!(missing_read_only.status(), StatusCode::UNAUTHORIZED);

        std::fs::write(&read_only_file, "readonly-v5-recovered\n")?;
        let read_only_recovered = client
            .get(format!("{base}/v1/status"))
            .bearer_auth("readonly-v5-recovered")
            .send()
            .await?;
        assert_eq!(read_only_recovered.status(), StatusCode::OK);
        shutdown.send(()).ok();
        Ok(())
    }

    #[test]
    fn auth_rate_limit_is_scoped_by_remote_ip_and_reason() {
        let mut limiter = AuthFailureRateLimiter::default();
        let rate_limit = ControlPlaneAuthRateLimit {
            max_failures: 1,
            window_ms: 60_000,
        };
        let ip_one = "127.0.0.1:4100".parse().expect("socket addr");
        let ip_two = "127.0.0.2:4100".parse().expect("socket addr");

        assert_eq!(
            limiter.note_failure(
                auth_rate_limit_bucket("invalid_token", Some(ip_one)),
                1,
                rate_limit
            ),
            AuthFailureAuditDecision::Allowed
        );
        assert_eq!(
            limiter.note_failure(
                auth_rate_limit_bucket("invalid_token", Some(ip_one)),
                2,
                rate_limit
            ),
            AuthFailureAuditDecision::RateLimited {
                audit_transition: true
            }
        );
        assert_eq!(
            limiter.note_failure(
                auth_rate_limit_bucket("invalid_token", Some(ip_two)),
                3,
                rate_limit
            ),
            AuthFailureAuditDecision::Allowed
        );
        assert_eq!(
            limiter.note_failure(
                auth_rate_limit_bucket("missing_token", Some(ip_one)),
                4,
                rate_limit
            ),
            AuthFailureAuditDecision::Allowed
        );
    }

    #[test]
    fn auth_rate_limit_samples_are_capped() {
        let mut limiter = AuthFailureRateLimiter::default();
        let rate_limit = ControlPlaneAuthRateLimit {
            max_failures: 1,
            window_ms: 60_000,
        };
        for index in 0..(MAX_AUTH_FAILURE_RATE_LIMIT_SAMPLES + 16) {
            let addr = std::net::SocketAddr::from((
                std::net::Ipv4Addr::new(127, 0, (index / 255) as u8, (index % 255) as u8),
                4100,
            ));
            assert_eq!(
                limiter.note_failure(
                    auth_rate_limit_bucket("invalid_token", Some(addr)),
                    index as u64,
                    rate_limit,
                ),
                AuthFailureAuditDecision::Allowed
            );
        }
        assert_eq!(limiter.failures.len(), MAX_AUTH_FAILURE_RATE_LIMIT_SAMPLES);
        assert_eq!(limiter.rate_limit_audits.len(), 0);

        let repeated = "127.250.0.1:4100".parse().expect("socket addr");
        assert_eq!(
            limiter.note_failure(
                auth_rate_limit_bucket("invalid_token", Some(repeated)),
                (MAX_AUTH_FAILURE_RATE_LIMIT_SAMPLES + 17) as u64,
                rate_limit,
            ),
            AuthFailureAuditDecision::Allowed
        );
        assert_eq!(limiter.failures.len(), MAX_AUTH_FAILURE_RATE_LIMIT_SAMPLES);
    }
}
