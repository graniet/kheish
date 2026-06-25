use std::sync::Arc;

use anyhow::Result;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use kheish_core::ModelDriver;
use kheish_types::ReplyHandle;

use crate::state::ConnectorIngressLookup;
use crate::{
    DaemonState, InputAttachmentRequest, RunView, SubmitInputItemRequest, SubmitInputRequest,
};

use super::multimodal::ensure_connector_request_not_empty;
use super::{
    ConnectorIngressGuard, acquire_connector_ingress_with_fingerprint, as_json_response,
    connector_retry_after_problem_response, internal_error, release_connector_ingress,
    submit_connector_run_with_guard, take_connector_ingress_rate_limit, unauthorized,
};

type HmacSha256 = Hmac<Sha256>;

const HTTP_SIGNATURE_HEADER: &str = "x-kheish-signature";
const HTTP_TIMESTAMP_HEADER: &str = "x-kheish-timestamp";
const HTTP_SIGNATURE_VERSION: &str = "v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HttpWebhookPayload {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    binding_keys: Vec<String>,
    #[serde(default)]
    actor_id: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    input_items: Vec<SubmitInputItemRequest>,
    #[serde(default)]
    attachments: Vec<InputAttachmentRequest>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    reply_targets: Vec<ReplyHandle>,
    #[serde(default)]
    reply_plugin: Option<String>,
    #[serde(default)]
    reply_address: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
}

fn verify_bearer(
    headers: &HeaderMap,
    expected: Option<&str>,
    allow_unauthenticated_ingress: bool,
) -> Result<()> {
    let Some(expected) = expected else {
        anyhow::ensure!(
            allow_unauthenticated_ingress,
            "http connector ingress authentication is not configured"
        );
        return Ok(());
    };
    let actual = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    anyhow::ensure!(
        actual
            .map(|value| value.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() == 1)
            .unwrap_or(false),
        "http connector bearer token mismatch"
    );
    Ok(())
}

fn http_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, message.into()).into_response()
}

fn http_internal_error(error: anyhow::Error) -> Response {
    let (status, message) = internal_error(error);
    http_error(status, message)
}

fn http_retry_after_error(name: &str, retry_after_ms: u64) -> Response {
    let message = format!(
        "http connector {name} ingress rate limit exceeded; retry_after_ms={retry_after_ms}"
    );
    connector_retry_after_problem_response("http_ingress_rate_limited", message, retry_after_ms)
}

fn verify_hmac_signature(
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
    secret: Option<&str>,
    required: bool,
    max_age_secs: u64,
) -> Result<()> {
    let Some(secret) = secret else {
        anyhow::ensure!(
            !required,
            "http connector hmac authentication is not configured"
        );
        return Ok(());
    };
    if !required {
        return Ok(());
    }
    let timestamp = required_single_http_header(headers, HTTP_TIMESTAMP_HEADER)?;
    let timestamp_secs = timestamp
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("invalid {HTTP_TIMESTAMP_HEADER}"))?;
    let now_secs = crate::now_ms() / 1000;
    anyhow::ensure!(
        now_secs.abs_diff(timestamp_secs) <= max_age_secs,
        "http connector request timestamp is too old or too far in the future"
    );

    let signature = required_single_http_header(headers, HTTP_SIGNATURE_HEADER)?;
    let signature = signature
        .strip_prefix("v1=")
        .ok_or_else(|| anyhow::anyhow!("invalid {HTTP_SIGNATURE_HEADER}"))?;
    anyhow::ensure!(
        signature.len() == 64 && signature.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid {HTTP_SIGNATURE_HEADER}"
    );
    let signature =
        hex::decode(signature).map_err(|_| anyhow::anyhow!("invalid {HTTP_SIGNATURE_HEADER}"))?;
    let mut mac = http_hmac_mac(secret)?;
    mac.update(&http_hmac_canonical_bytes(method, uri, timestamp, body));
    mac.verify_slice(&signature)
        .map_err(|_| anyhow::anyhow!("http connector hmac signature mismatch"))?;
    Ok(())
}

fn required_single_http_header<'a>(headers: &'a HeaderMap, name: &'static str) -> Result<&'a str> {
    let values = headers.get_all(name);
    let mut iter = values.iter();
    let Some(value) = iter.next() else {
        anyhow::bail!("missing {name}");
    };
    anyhow::ensure!(iter.next().is_none(), "duplicate {name}");
    value
        .to_str()
        .map_err(|_| anyhow::anyhow!("invalid {name}"))
}

fn http_hmac_mac(secret: &str) -> Result<HmacSha256> {
    HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| anyhow::anyhow!("invalid http connector hmac secret"))
}

fn http_hmac_canonical_bytes(method: &Method, uri: &Uri, timestamp: &str, body: &[u8]) -> Vec<u8> {
    let mut canonical = Vec::new();
    canonical.extend_from_slice(HTTP_SIGNATURE_VERSION.as_bytes());
    canonical.extend_from_slice(b":");
    canonical.extend_from_slice(method.as_str().as_bytes());
    canonical.extend_from_slice(b":");
    canonical.extend_from_slice(
        uri.path_and_query()
            .map(|value| value.as_str())
            .unwrap_or(uri.path())
            .as_bytes(),
    );
    canonical.extend_from_slice(b":");
    canonical.extend_from_slice(timestamp.as_bytes());
    canonical.extend_from_slice(b":");
    canonical.extend_from_slice(body);
    canonical
}

#[cfg(test)]
fn http_hmac_signature_hex(
    secret: &str,
    method: &Method,
    uri: &Uri,
    timestamp: &str,
    body: &[u8],
) -> Result<String> {
    let mut mac = http_hmac_mac(secret)?;
    mac.update(&http_hmac_canonical_bytes(method, uri, timestamp, body));
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn http_ingress_fingerprint(payload: &HttpWebhookPayload) -> Result<String> {
    kheish_codec::digest_serialize(payload)
}

fn http_ingress_key(connector_name: &str, idempotency_key: &str) -> String {
    let digest = kheish_codec::digest_text(idempotency_key);
    format!("http:{connector_name}:{}", &digest[..32])
}

fn http_ingress_metadata(
    metadata: Option<Value>,
    ingress_key: &str,
    idempotency_key: &str,
    ingress_fingerprint: &str,
) -> Value {
    let mut object = match metadata.unwrap_or(Value::Null) {
        Value::Object(object) => object,
        other => {
            let mut object = Map::new();
            object.insert("payload".to_string(), other);
            object
        }
    };
    object.insert(
        "http_ingress_key".to_string(),
        Value::String(ingress_key.to_string()),
    );
    object.insert(
        "connector_ingress_key".to_string(),
        Value::String(ingress_key.to_string()),
    );
    object.insert(
        "http_ingress_fingerprint".to_string(),
        Value::String(ingress_fingerprint.to_string()),
    );
    object.insert(
        "http_ingress_key_sha256".to_string(),
        Value::String(kheish_codec::digest_text(idempotency_key)),
    );
    Value::Object(object)
}

fn existing_http_ingress_fingerprint(run: &RunView) -> Option<&str> {
    run.input_metadata
        .as_ref()?
        .get("http_ingress_fingerprint")?
        .as_str()
}

fn http_connector_request_is_authenticated(
    connector: &crate::connectors::config::ResolvedHttpInputConnector,
) -> bool {
    connector.bearer_token.is_some() || connector.require_hmac_signature
}

fn http_metadata_contains_reserved_ingress_key(metadata: Option<&Value>) -> Option<&'static str> {
    let object = metadata?.as_object()?;
    [
        "connector_ingress_key",
        "http_ingress_key",
        "http_ingress_key_sha256",
        "http_ingress_fingerprint",
    ]
    .into_iter()
    .find(|key| object.contains_key(*key))
}

fn http_payload_contains_asset_or_board_input(
    input_items: &[SubmitInputItemRequest],
    attachments: &[InputAttachmentRequest],
) -> bool {
    !attachments.is_empty()
        || input_items.iter().any(|item| {
            matches!(
                item,
                SubmitInputItemRequest::AssetReference { .. }
                    | SubmitInputItemRequest::BoardReference { .. }
                    | SubmitInputItemRequest::InlineAsset(_)
            )
        })
}

pub(super) async fn http_webhook<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(name): AxumPath<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<RunView>, Response>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let connector = state.connectors().http(&name).ok_or_else(|| {
        http_error(
            StatusCode::NOT_FOUND,
            format!("unknown http connector {name}"),
        )
    })?;
    verify_bearer(
        &headers,
        connector.bearer_token.as_deref(),
        connector.allow_unauthenticated_ingress || connector.require_hmac_signature,
    )
    .map_err(|error| {
        let (status, message) = unauthorized(error.to_string());
        http_error(status, message)
    })?;
    verify_hmac_signature(
        &method,
        &uri,
        &headers,
        &body,
        connector.hmac_secret.as_deref(),
        connector.require_hmac_signature,
        connector.signature_max_age_secs,
    )
    .map_err(|error| {
        let (status, message) = unauthorized(error.to_string());
        http_error(status, message)
    })?;
    let payload = serde_json::from_slice::<HttpWebhookPayload>(&body).map_err(|error| {
        http_error(
            StatusCode::BAD_REQUEST,
            format!("invalid http connector payload: {error}"),
        )
    })?;
    let ingress_fingerprint = http_ingress_fingerprint(&payload).map_err(http_internal_error)?;
    let HttpWebhookPayload {
        session_id: payload_session_id,
        binding_keys: payload_binding_keys,
        actor_id,
        content,
        input_items,
        attachments,
        metadata,
        reply_targets: payload_reply_targets,
        reply_plugin,
        reply_address,
        idempotency_key,
    } = payload;
    let request_is_authenticated = http_connector_request_is_authenticated(&connector);
    if !request_is_authenticated {
        if payload_session_id.is_some() {
            return Err(http_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "http connector {name} requires bearer or HMAC auth for payload session_id"
                ),
            ));
        }
        if !payload_binding_keys.is_empty() {
            return Err(http_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "http connector {name} requires bearer or HMAC auth for payload binding_keys"
                ),
            ));
        }
        if http_payload_contains_asset_or_board_input(&input_items, &attachments) {
            return Err(http_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "http connector {name} requires bearer or HMAC auth for asset or board payloads"
                ),
            ));
        }
    }
    let has_payload_reply_override =
        !payload_reply_targets.is_empty() || reply_plugin.is_some() || reply_address.is_some();
    if has_payload_reply_override && !request_is_authenticated {
        return Err(http_error(
            StatusCode::BAD_REQUEST,
            format!("http connector {name} requires bearer or HMAC auth for payload reply targets"),
        ));
    }
    if has_payload_reply_override && !connector.allow_payload_reply_targets {
        return Err(http_error(
            StatusCode::BAD_REQUEST,
            format!("http connector {name} does not allow payload reply target overrides"),
        ));
    }
    if let Some(key) = http_metadata_contains_reserved_ingress_key(metadata.as_ref()) {
        return Err(http_error(
            StatusCode::BAD_REQUEST,
            format!("http connector {name} metadata key `{key}` is daemon-owned"),
        ));
    }
    let binding_keys = connector.binding_keys(payload_binding_keys);
    let session_id = connector
        .fixed_session_id
        .clone()
        .or(payload_session_id)
        .or(state
            .bound_session_id(&binding_keys)
            .await
            .map_err(http_internal_error)?)
        .or_else(|| connector.fallback_session_id(&binding_keys))
        .ok_or_else(|| {
            http_error(
                StatusCode::BAD_REQUEST,
                format!("http connector {name} requires either session_id or binding_keys"),
            )
        })?;
    let reply_targets = if payload_reply_targets.is_empty() {
        connector.default_reply_targets.clone()
    } else {
        payload_reply_targets
    };
    let idempotency_key = idempotency_key
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty());
    if connector.require_idempotency_key && idempotency_key.is_none() {
        return Err(http_error(
            StatusCode::BAD_REQUEST,
            format!("http connector {name} requires idempotency_key"),
        ));
    }
    let ingress_key = idempotency_key
        .as_ref()
        .map(|key| http_ingress_key(&name, key));
    if let Some(ingress_key) = ingress_key.as_deref()
        && let ConnectorIngressLookup::Absent = state
            .lookup_connector_ingress(ingress_key)
            .await
            .map_err(http_internal_error)?
        && let Some(retry_after_ms) = take_http_ingress_rate_limit(&state, &name, &connector).await
    {
        return Err(http_retry_after_error(&name, retry_after_ms));
    }
    let ingress = acquire_connector_ingress_with_fingerprint(
        &state,
        ingress_key.as_deref(),
        &ingress_fingerprint,
    )
    .await
    .map_err(http_internal_error)?;
    if let ConnectorIngressGuard::Existing(run) = &ingress {
        if existing_http_ingress_fingerprint(run)
            .is_some_and(|existing| existing != ingress_fingerprint)
        {
            return Err(http_error(
                StatusCode::CONFLICT,
                format!(
                    "http connector {name} idempotency_key was already submitted with a different payload"
                ),
            ));
        }
        return Ok(as_json_response(run.clone()));
    }
    if ingress_key.is_none()
        && let Some(retry_after_ms) = take_http_ingress_rate_limit(&state, &name, &connector).await
    {
        release_connector_ingress(&state, ingress)
            .await
            .map_err(http_internal_error)?;
        return Err(http_retry_after_error(&name, retry_after_ms));
    }
    let metadata = match (ingress_key.as_deref(), idempotency_key.as_deref()) {
        (Some(key), Some(idempotency_key)) => {
            http_ingress_metadata(metadata, key, idempotency_key, &ingress_fingerprint)
        }
        _ => metadata.unwrap_or(Value::Null),
    };
    let request = SubmitInputRequest {
        provider: None,
        source_plugin: Some("http".to_string()),
        source_kind: Some(name.clone()),
        actor_id: actor_id
            .or_else(|| connector.actor_id.clone())
            .or(Some("http-webhook".to_string())),
        content: content.unwrap_or_default(),
        input_items,
        attachments,
        generation: None,
        completion_requirements: None,
        metadata: Some(metadata),
        binding_keys,
        reply_targets,
        reply_plugin,
        reply_address,
    };
    if let Err(error) = ensure_connector_request_not_empty(&request) {
        release_connector_ingress(&state, ingress)
            .await
            .map_err(http_internal_error)?;
        return Err(http_error(StatusCode::BAD_REQUEST, error.to_string()));
    }
    submit_connector_run_with_guard(
        &state,
        &session_id,
        request,
        ingress,
        &connector.session_policy,
        &format!("http connector {name}"),
    )
    .await
    .map(as_json_response)
    .map_err(http_internal_error)
}

async fn take_http_ingress_rate_limit<M>(
    state: &Arc<DaemonState<M>>,
    name: &str,
    connector: &crate::connectors::config::ResolvedHttpInputConnector,
) -> Option<u64>
where
    M: ModelDriver + Send + Sync + 'static,
{
    take_connector_ingress_rate_limit(
        format!("http:{}:{name}", state.control_plane_base_url()),
        connector.ingress_events_per_second,
    )
    .await
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn signed_headers(method: &Method, uri: &Uri, body: &[u8], secret: &str) -> HeaderMap {
        let timestamp = (crate::now_ms() / 1000).to_string();
        let signature = format!(
            "v1={}",
            http_hmac_signature_hex(secret, method, uri, &timestamp, body).unwrap()
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            HTTP_TIMESTAMP_HEADER,
            HeaderValue::from_str(&timestamp).unwrap(),
        );
        headers.insert(
            HTTP_SIGNATURE_HEADER,
            HeaderValue::from_str(&signature).unwrap(),
        );
        headers
    }

    #[test]
    fn http_hmac_signature_matches_published_fixed_vector() {
        let method = Method::POST;
        let uri: Uri = "/v1/connectors/http/orders?source=a%2Fb&attempt=1"
            .parse()
            .unwrap();
        let timestamp = "1710000000";
        let body = br#"{"content":"hello","idempotency_key":"order-123","metadata":{"k":"v"}}"#;
        let canonical = http_hmac_canonical_bytes(&method, &uri, timestamp, body);
        assert_eq!(
            std::str::from_utf8(&canonical).unwrap(),
            r#"v1:POST:/v1/connectors/http/orders?source=a%2Fb&attempt=1:1710000000:{"content":"hello","idempotency_key":"order-123","metadata":{"k":"v"}}"#
        );
        assert_eq!(
            http_hmac_signature_hex("hmac-test-secret", &method, &uri, timestamp, body)
                .expect("fixed-vector HMAC should compute"),
            "f13a4b8c5099a2ffc6b8a913e0998d6765d61a693c27f594ca34ede2e0d4e557"
        );
    }

    #[test]
    fn http_hmac_signature_accepts_fresh_canonical_request() {
        let method = Method::POST;
        let uri: Uri = "/v1/connectors/http/ingress".parse().unwrap();
        let body = br#"{"content":"hello","idempotency_key":"req-1"}"#;
        let headers = signed_headers(&method, &uri, body, "secret");

        verify_hmac_signature(&method, &uri, &headers, body, Some("secret"), true, 300)
            .expect("fresh signature should verify");
    }

    #[test]
    fn http_hmac_signature_rejects_bad_signature_and_stale_timestamp() {
        let method = Method::POST;
        let uri: Uri = "/v1/connectors/http/ingress".parse().unwrap();
        let body = br#"{"content":"hello","idempotency_key":"req-1"}"#;
        let mut headers = signed_headers(&method, &uri, body, "secret");
        headers.insert(HTTP_SIGNATURE_HEADER, HeaderValue::from_static("v1=00"));
        assert!(
            verify_hmac_signature(&method, &uri, &headers, body, Some("secret"), true, 300)
                .is_err()
        );

        let stale = ((crate::now_ms() / 1000).saturating_sub(1000)).to_string();
        let mut headers = signed_headers(&method, &uri, body, "secret");
        headers.insert(
            HTTP_TIMESTAMP_HEADER,
            HeaderValue::from_str(&stale).unwrap(),
        );
        assert!(
            verify_hmac_signature(&method, &uri, &headers, body, Some("secret"), true, 300)
                .is_err()
        );

        let mut headers = signed_headers(&method, &uri, body, "secret");
        headers.insert(
            HTTP_TIMESTAMP_HEADER,
            HeaderValue::from_static("18446744073709551615"),
        );
        assert!(
            verify_hmac_signature(&method, &uri, &headers, body, Some("secret"), true, 300)
                .is_err()
        );
    }

    #[test]
    fn http_hmac_signature_rejects_legacy_or_duplicate_signature_headers() {
        let method = Method::POST;
        let uri: Uri = "/v1/connectors/http/ingress".parse().unwrap();
        let body = br#"{"content":"hello","idempotency_key":"req-1"}"#;
        let mut headers = signed_headers(&method, &uri, body, "secret");
        headers.insert(
            HTTP_SIGNATURE_HEADER,
            HeaderValue::from_static(
                "sha256=9431cfad7c42ccd8d2cb7edc93ac14e79ba1f804f8e00272cf3b2541a18f88a0",
            ),
        );
        assert!(
            verify_hmac_signature(&method, &uri, &headers, body, Some("secret"), true, 300)
                .is_err(),
            "legacy sha256= signatures should be rejected"
        );

        let mut headers = signed_headers(&method, &uri, body, "secret");
        headers.insert(
            HTTP_SIGNATURE_HEADER,
            HeaderValue::from_static(
                "9431cfad7c42ccd8d2cb7edc93ac14e79ba1f804f8e00272cf3b2541a18f88a0",
            ),
        );
        assert!(
            verify_hmac_signature(&method, &uri, &headers, body, Some("secret"), true, 300)
                .is_err(),
            "bare hex signatures should be rejected"
        );

        let mut headers = signed_headers(&method, &uri, body, "secret");
        headers.append(HTTP_SIGNATURE_HEADER, HeaderValue::from_static("v1=00"));
        assert!(
            verify_hmac_signature(&method, &uri, &headers, body, Some("secret"), true, 300)
                .is_err(),
            "duplicate signature headers should be rejected"
        );

        let mut headers = signed_headers(&method, &uri, body, "secret");
        headers.append(
            HTTP_TIMESTAMP_HEADER,
            HeaderValue::from_static("1710000000"),
        );
        assert!(
            verify_hmac_signature(&method, &uri, &headers, body, Some("secret"), true, 300)
                .is_err(),
            "duplicate timestamp headers should be rejected"
        );
    }

    #[test]
    fn http_webhook_payload_rejects_unknown_fields() {
        let error = serde_json::from_str::<HttpWebhookPayload>(
            r#"{"content":"hello","idempotency_key":"req-1","surprise":true}"#,
        )
        .expect_err("strict HTTP payload should reject unknown fields");
        assert!(error.to_string().contains("unknown field"));
    }
}
