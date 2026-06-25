use std::sync::Arc;

use anyhow::Result;
use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use kheish_auth::AuthSlotId;
use kheish_core::ModelDriver;

use crate::api::asset_raw_response_headers;
use crate::connectors::config::ResolvedExternalConnector;
use crate::connectors::{
    EXTERNAL_CONNECTOR_INGRESS_PROTOCOL_VERSION, ExternalConnectorIngressBatchItemResponse,
    ExternalConnectorIngressBatchItemStatus, ExternalConnectorIngressBatchRequest,
    ExternalConnectorIngressBatchResponse, ExternalConnectorIngressEvent,
    ExternalConnectorIngressRequest, ExternalConnectorIngressResponse,
    ExternalConnectorIngressStatus, is_supported_external_connector_ingress_protocol_version,
};
use crate::state::ConnectorIngressLookup;
use crate::{DaemonState, SubmitInputRequest};

use super::multimodal::ensure_connector_request_not_empty;
use super::{
    ConnectorIngressGuard, acquire_connector_ingress_with_fingerprint,
    connector_retry_after_problem_response, internal_error, release_connector_ingress,
    submit_connector_run_with_guard, unauthorized,
};

pub(crate) const EXTERNAL_CONNECTOR_HTTP_BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024;

const MAX_EXTERNAL_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_EXTERNAL_INPUT_ITEMS: usize = 64;
const MAX_EXTERNAL_ATTACHMENTS: usize = 10;
const MAX_EXTERNAL_INLINE_ASSET_BYTES: usize = 1024 * 1024;
const MAX_EXTERNAL_BATCH_EVENTS: usize = 64;

enum ProcessExternalEventOutcome {
    Response(ExternalConnectorIngressResponse),
    RateLimited {
        event_id: Option<String>,
        reason: String,
        retry_after_ms: u64,
    },
}

fn external_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, message.into()).into_response()
}

fn external_retry_after_error(reason: String, retry_after_ms: u64) -> Response {
    connector_retry_after_problem_response("external_ingress_rate_limited", reason, retry_after_ms)
}

fn verify_shared_token(
    headers: &HeaderMap,
    expected: Option<&str>,
    allow_unauthenticated_ingress: bool,
) -> Result<()> {
    let Some(expected) = expected else {
        anyhow::ensure!(
            allow_unauthenticated_ingress,
            "external connector ingress authentication is not configured"
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
        "external connector bearer token mismatch"
    );
    Ok(())
}

pub(super) async fn external_event<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    Json(payload): Json<ExternalConnectorIngressRequest>,
) -> Result<Json<ExternalConnectorIngressResponse>, Response>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let connector = external_connector(&state, &name)
        .map_err(|(status, message)| external_error(status, message))?;
    verify_shared_token(
        &headers,
        connector.shared_token.as_deref(),
        connector.allow_unauthenticated_ingress,
    )
    .map_err(|error| {
        let (status, message) = unauthorized(error.to_string());
        external_error(status, message)
    })?;
    match process_external_event(&state, &connector, payload.protocol_version, &payload.event).await
    {
        Ok(ProcessExternalEventOutcome::Response(response)) => Ok(Json(response)),
        Ok(ProcessExternalEventOutcome::RateLimited {
            event_id: _,
            reason,
            retry_after_ms,
        }) => Err(external_retry_after_error(reason, retry_after_ms)),
        Err(error) => {
            state
                .external_connector_runtime()
                .note_ingress_rejected(&connector)
                .await;
            let (status, message) = internal_error(error);
            Err(external_error(status, message))
        }
    }
}

pub(super) async fn external_event_batch<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    Json(payload): Json<ExternalConnectorIngressBatchRequest>,
) -> Result<Json<ExternalConnectorIngressBatchResponse>, (StatusCode, String)>
where
    M: ModelDriver + Send + Sync + 'static,
{
    if payload.events.len() > MAX_EXTERNAL_BATCH_EVENTS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("external connector batch exceeds the {MAX_EXTERNAL_BATCH_EVENTS} event limit"),
        ));
    }
    let connector = external_connector(&state, &name)?;
    verify_shared_token(
        &headers,
        connector.shared_token.as_deref(),
        connector.allow_unauthenticated_ingress,
    )
    .map_err(|error| unauthorized(error.to_string()))?;

    if !is_supported_external_connector_ingress_protocol_version(payload.protocol_version) {
        for _ in &payload.events {
            state
                .external_connector_runtime()
                .note_ingress_rejected(&connector)
                .await;
        }
        return Ok(Json(ExternalConnectorIngressBatchResponse {
            results: payload
                .events
                .iter()
                .map(|event| {
                    batch_item_response_from_single(unsupported_protocol_response(
                        payload.protocol_version,
                        Some(&event.event_id),
                    ))
                })
                .collect(),
        }));
    }

    let mut results = Vec::with_capacity(payload.events.len());
    for event in &payload.events {
        let response =
            match process_external_event(&state, &connector, payload.protocol_version, event).await
            {
                Ok(ProcessExternalEventOutcome::Response(response)) => {
                    batch_item_response_from_single(response)
                }
                Ok(ProcessExternalEventOutcome::RateLimited {
                    event_id,
                    reason,
                    retry_after_ms,
                }) => ExternalConnectorIngressBatchItemResponse {
                    event_id,
                    status: ExternalConnectorIngressBatchItemStatus::RateLimited,
                    session_id: None,
                    run_id: None,
                    reason: Some(reason),
                    retry_after_ms: Some(retry_after_ms),
                },
                Err(error) => {
                    state
                        .external_connector_runtime()
                        .note_ingress_rejected(&connector)
                        .await;
                    ExternalConnectorIngressBatchItemResponse {
                        event_id: Some(event.event_id.clone()),
                        status: ExternalConnectorIngressBatchItemStatus::Error,
                        session_id: None,
                        run_id: None,
                        reason: Some(format!(
                            "internal error while processing batch item: {error}"
                        )),
                        retry_after_ms: None,
                    }
                }
            };
        results.push(response);
    }
    Ok(Json(ExternalConnectorIngressBatchResponse { results }))
}

fn external_connector<M>(
    state: &Arc<DaemonState<M>>,
    name: &str,
) -> Result<ResolvedExternalConnector, (StatusCode, String)>
where
    M: ModelDriver + Send + Sync + 'static,
{
    state.connectors().external(name).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("unknown external connector {name}"),
        )
    })
}

async fn process_external_event<M>(
    state: &Arc<DaemonState<M>>,
    connector: &ResolvedExternalConnector,
    protocol_version: u32,
    payload: &ExternalConnectorIngressEvent,
) -> Result<ProcessExternalEventOutcome>
where
    M: ModelDriver + Send + Sync + 'static,
{
    if !is_supported_external_connector_ingress_protocol_version(protocol_version) {
        state
            .external_connector_runtime()
            .note_ingress_rejected(connector)
            .await;
        return Ok(ProcessExternalEventOutcome::Response(
            unsupported_protocol_response(protocol_version, Some(&payload.event_id)),
        ));
    }

    let event_id = payload.event_id.trim();
    if event_id.is_empty() {
        state
            .external_connector_runtime()
            .note_ingress_rejected(connector)
            .await;
        return Ok(ProcessExternalEventOutcome::Response(rejected_response(
            None,
            "event_id is required",
        )));
    }
    if let Err(error) = validate_external_ingress_payload(payload) {
        state
            .external_connector_runtime()
            .note_ingress_rejected(connector)
            .await;
        return Ok(ProcessExternalEventOutcome::Response(rejected_response(
            Some(event_id),
            error.to_string(),
        )));
    }
    if let Err(error) = state
        .external_connector_runtime()
        .validate_child_process_instance_id(connector, &payload.instance_id)
        .await
    {
        state
            .external_connector_runtime()
            .note_ingress_rejected(connector)
            .await;
        return Ok(ProcessExternalEventOutcome::Response(rejected_response(
            Some(event_id),
            error.to_string(),
        )));
    }

    let binding_keys = payload
        .thread
        .binding_keys_with_routing_key(connector, payload.routing_key.as_deref());
    let session_id = connector
        .fixed_session_id
        .clone()
        .or(state.bound_session_id(&binding_keys).await?)
        .or_else(|| {
            payload
                .thread
                .natural_session_id_with_routing_key(connector, payload.routing_key.as_deref())
        })
        .ok_or_else(|| {
            format!(
                "external connector {} requires thread.path, routing_key, or fixed_session_id",
                connector.name
            )
        });
    let session_id = match session_id {
        Ok(session_id) => session_id,
        Err(reason) => {
            state
                .external_connector_runtime()
                .note_ingress_rejected(connector)
                .await;
            return Ok(ProcessExternalEventOutcome::Response(rejected_response(
                Some(event_id),
                reason,
            )));
        }
    };
    let ingress_fingerprint = external_ingress_fingerprint(payload);
    let ingress_key = external_ingress_key(&connector.name, event_id);
    let mut ingress_lookup = state.lookup_connector_ingress(&ingress_key).await?;
    if let ConnectorIngressLookup::Existing { run_id } = &ingress_lookup
        && state.get_run(run_id).await.is_err()
    {
        state.forget_connector_ingress(&ingress_key).await?;
        ingress_lookup = ConnectorIngressLookup::Absent;
    }
    if matches!(ingress_lookup, ConnectorIngressLookup::Absent)
        && let Err(retry_after_ms) = state
            .external_connector_runtime()
            .allow_ingress(connector)
            .await
    {
        state
            .external_connector_runtime()
            .note_ingress_rate_limited(connector)
            .await;
        return Ok(ProcessExternalEventOutcome::RateLimited {
            event_id: Some(event_id.to_string()),
            reason: format!(
                "external connector {} ingress rate limit exceeded; retry_after_ms={retry_after_ms}",
                connector.name
            ),
            retry_after_ms,
        });
    }
    let ingress = match acquire_connector_ingress_with_fingerprint(
        state,
        Some(&ingress_key),
        &ingress_fingerprint,
    )
    .await
    {
        Ok(ingress) => ingress,
        Err(error) if is_connector_ingress_fingerprint_conflict(&error) => {
            state
                .external_connector_runtime()
                .note_ingress_rejected(connector)
                .await;
            let existing = existing_external_ingress_run_for_key(state, &ingress_key).await?;
            return Ok(ProcessExternalEventOutcome::Response(
                external_fingerprint_conflict_response(event_id, existing.as_ref()),
            ));
        }
        Err(error) => return Err(error),
    };
    if let ConnectorIngressGuard::Existing(run) = &ingress {
        if existing_external_ingress_fingerprint(run)
            .is_some_and(|existing| existing != ingress_fingerprint)
        {
            state
                .external_connector_runtime()
                .note_ingress_rejected(connector)
                .await;
            return Ok(ProcessExternalEventOutcome::Response(
                ExternalConnectorIngressResponse {
                    event_id: Some(event_id.to_string()),
                    status: ExternalConnectorIngressStatus::Rejected,
                    session_id: Some(run.session_id.clone()),
                    run_id: Some(run.run_id.clone()),
                    reason: Some(format!(
                        "event_id {event_id} was already submitted with a different payload"
                    )),
                },
            ));
        }
        state
            .external_connector_runtime()
            .note_ingress_duplicate(connector)
            .await;
        return Ok(ProcessExternalEventOutcome::Response(
            ExternalConnectorIngressResponse {
                event_id: Some(event_id.to_string()),
                status: ExternalConnectorIngressStatus::Duplicate,
                session_id: Some(run.session_id.clone()),
                run_id: Some(run.run_id.clone()),
                reason: None,
            },
        ));
    }

    let reply_targets = connector.reply_targets(payload.reply_route.clone());
    let request = SubmitInputRequest {
        provider: None,
        source_plugin: Some("external".to_string()),
        source_kind: payload
            .source_kind
            .clone()
            .or_else(|| Some(connector.platform.clone())),
        actor_id: payload
            .actor_id
            .clone()
            .or(Some("external-user".to_string())),
        content: payload.content.clone(),
        input_items: payload.input_items.clone(),
        attachments: payload.attachments.clone(),
        generation: None,
        completion_requirements: None,
        metadata: Some(external_event_metadata(
            payload,
            protocol_version,
            &ingress_key,
            &ingress_fingerprint,
        )),
        binding_keys,
        reply_targets,
        reply_plugin: None,
        reply_address: None,
    };
    if let Err(error) = ensure_connector_request_not_empty(&request) {
        state
            .external_connector_runtime()
            .note_ingress_rejected(connector)
            .await;
        release_connector_ingress(state, ingress).await?;
        return Ok(ProcessExternalEventOutcome::Response(
            ExternalConnectorIngressResponse {
                event_id: Some(event_id.to_string()),
                status: ExternalConnectorIngressStatus::Rejected,
                session_id: Some(session_id),
                run_id: None,
                reason: Some(error.to_string()),
            },
        ));
    }
    let run = submit_connector_run_with_guard(
        state,
        &session_id,
        request,
        ingress,
        &connector.session_policy,
        &format!("external connector {}", connector.name),
    )
    .await?;
    state
        .external_connector_runtime()
        .note_ingress_accepted(connector)
        .await;
    Ok(ProcessExternalEventOutcome::Response(
        ExternalConnectorIngressResponse {
            event_id: Some(event_id.to_string()),
            status: ExternalConnectorIngressStatus::Accepted,
            session_id: Some(session_id),
            run_id: Some(run.run_id),
            reason: None,
        },
    ))
}

fn unsupported_protocol_response(
    protocol_version: u32,
    event_id: Option<&str>,
) -> ExternalConnectorIngressResponse {
    rejected_response(
        event_id,
        format!(
            "unsupported protocol_version {protocol_version}; supported range is 1..={}",
            EXTERNAL_CONNECTOR_INGRESS_PROTOCOL_VERSION
        ),
    )
}

fn rejected_response(
    event_id: Option<&str>,
    reason: impl Into<String>,
) -> ExternalConnectorIngressResponse {
    ExternalConnectorIngressResponse {
        event_id: event_id.map(str::to_string),
        status: ExternalConnectorIngressStatus::Rejected,
        session_id: None,
        run_id: None,
        reason: Some(reason.into()),
    }
}

fn is_connector_ingress_fingerprint_conflict(error: &anyhow::Error) -> bool {
    error
        .to_string()
        .contains("was already submitted with a different payload")
}

fn external_fingerprint_conflict_response(
    event_id: &str,
    existing: Option<&crate::RunView>,
) -> ExternalConnectorIngressResponse {
    ExternalConnectorIngressResponse {
        event_id: Some(event_id.to_string()),
        status: ExternalConnectorIngressStatus::Rejected,
        session_id: existing.map(|run| run.session_id.clone()),
        run_id: existing.map(|run| run.run_id.clone()),
        reason: Some(format!(
            "event_id {event_id} was already submitted with a different payload"
        )),
    }
}

async fn existing_external_ingress_run_for_key<M>(
    state: &Arc<DaemonState<M>>,
    ingress_key: &str,
) -> Result<Option<crate::RunView>>
where
    M: ModelDriver + Send + Sync + 'static,
{
    match state.lookup_connector_ingress(ingress_key).await? {
        ConnectorIngressLookup::Existing { run_id } => Ok(state.get_run(&run_id).await.ok()),
        ConnectorIngressLookup::Pending | ConnectorIngressLookup::Absent => Ok(None),
    }
}

fn batch_item_response_from_single(
    response: ExternalConnectorIngressResponse,
) -> ExternalConnectorIngressBatchItemResponse {
    let status = match response.status {
        ExternalConnectorIngressStatus::Accepted => {
            ExternalConnectorIngressBatchItemStatus::Accepted
        }
        ExternalConnectorIngressStatus::Duplicate => {
            ExternalConnectorIngressBatchItemStatus::Duplicate
        }
        ExternalConnectorIngressStatus::Rejected => {
            ExternalConnectorIngressBatchItemStatus::Rejected
        }
    };
    ExternalConnectorIngressBatchItemResponse {
        event_id: response.event_id,
        status,
        session_id: response.session_id,
        run_id: response.run_id,
        reason: response.reason,
        retry_after_ms: None,
    }
}

pub(super) async fn external_credential<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((name, env_key)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let connector = state.connectors().external(&name).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("unknown external connector {name}"),
        )
    })?;
    let Some(child_process) = connector.child_process.as_ref() else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("external connector {name} is not a child-process sidecar"),
        ));
    };
    let credential_token = state
        .external_connector_runtime()
        .child_process_credential_token(&connector)
        .await;
    verify_shared_token(&headers, credential_token.as_deref(), false)
        .map_err(|error| unauthorized(error.to_string()))?;
    let provided_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| unauthorized("missing external connector bearer token".to_string()))?;
    let Some(secret_ref) = child_process.credential_slots.get(&env_key) else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("external connector {name} does not expose credential {env_key}"),
        ));
    };
    let lease = state
        .auth_manager()
        .validate_connector_lease_for_secret_ref(
            provided_token,
            &connector.name,
            &env_key,
            &AuthSlotId::new(secret_ref.clone()),
        )
        .map_err(|error| unauthorized(error.to_string()))?;
    let value = state
        .generic_secret_value(secret_ref)
        .map_err(internal_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("external connector {name} credential {env_key} is not configured"),
            )
        })?;
    Ok(Json(serde_json::json!({
        "value": value,
        "lease_id": lease.id,
        "grant_id": lease.grant_id,
        "expires_at_ms": lease.expires_at_ms,
    })))
}

pub(super) async fn external_asset_raw<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath((name, delivery_id, asset_id)): AxumPath<(String, String, String)>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, String)>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let connector = state.connectors().external(&name).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("unknown external connector {name}"),
        )
    })?;
    verify_shared_token(&headers, connector.shared_token.as_deref(), false)
        .map_err(|error| unauthorized(error.to_string()))?;
    let delivery = state
        .pending_delivery_record(&delivery_id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("unknown pending delivery {delivery_id}"),
            )
        })?;
    if delivery.reply.plugin != "external" {
        return Err((
            StatusCode::NOT_FOUND,
            format!("delivery {delivery_id} is not an external connector delivery"),
        ));
    }
    let reply_route = crate::connectors::decode_external_reply_route(&delivery.reply.address)
        .map_err(internal_error)?;
    if reply_route.connector != name {
        return Err((
            StatusCode::NOT_FOUND,
            format!("delivery {delivery_id} does not belong to external connector {name}"),
        ));
    }
    if !delivery_references_asset(&delivery, &asset_id) {
        return Err((
            StatusCode::NOT_FOUND,
            format!("delivery {delivery_id} does not reference asset {asset_id}"),
        ));
    }
    let (asset, bytes) = state
        .get_asset_raw(&asset_id)
        .await
        .map_err(internal_error)?;
    let headers = asset_raw_response_headers(&asset).map_err(internal_error)?;
    Ok((headers, bytes))
}

fn external_event_metadata(
    payload: &ExternalConnectorIngressEvent,
    protocol_version: u32,
    ingress_key: &str,
    ingress_fingerprint: &str,
) -> Value {
    let mut metadata = match payload.metadata.clone().unwrap_or(Value::Null) {
        Value::Object(map) => map,
        Value::Null => Map::new(),
        other => {
            let mut map = Map::new();
            map.insert("source_metadata".to_string(), other);
            map
        }
    };
    if let Some(occurred_at_ms) = payload.occurred_at_ms {
        metadata.insert("occurred_at_ms".to_string(), Value::from(occurred_at_ms));
    }
    metadata.insert(
        "external_instance_id".to_string(),
        Value::String(payload.instance_id.clone()),
    );
    metadata.insert(
        "external_event_id".to_string(),
        Value::String(payload.event_id.clone()),
    );
    metadata.insert(
        "connector_ingress_key".to_string(),
        Value::String(ingress_key.to_string()),
    );
    metadata.insert(
        "external_event_key_sha256".to_string(),
        Value::String(kheish_codec::digest_text(&payload.event_id)),
    );
    metadata.insert(
        "external_event_fingerprint".to_string(),
        Value::String(ingress_fingerprint.to_string()),
    );
    metadata.insert(
        "external_protocol_version".to_string(),
        Value::from(protocol_version),
    );
    if let Some(intent) = payload
        .intent
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        metadata.insert(
            "external_intent".to_string(),
            Value::String(intent.to_string()),
        );
    }
    if let Some(routing_key) = payload
        .routing_key
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        metadata.insert(
            "external_routing_key".to_string(),
            Value::String(routing_key.to_string()),
        );
    }
    if let Some(relation) = payload.relation.as_ref() {
        metadata.insert(
            "external_relation".to_string(),
            serde_json::json!({
                "kind": relation.kind,
                "target_event_id": relation.target_event_id,
            }),
        );
    }
    Value::Object(metadata)
}

fn external_ingress_key(connector_name: &str, event_id: &str) -> String {
    let digest = kheish_codec::digest_text(event_id);
    format!("external:{connector_name}:{}", &digest[..32])
}

fn validate_external_ingress_payload(payload: &ExternalConnectorIngressEvent) -> Result<()> {
    if let Some(intent) = payload.intent.as_ref() {
        anyhow::ensure!(!intent.trim().is_empty(), "intent must not be blank");
    }
    if let Some(routing_key) = payload.routing_key.as_ref() {
        anyhow::ensure!(
            !routing_key.trim().is_empty(),
            "routing_key must not be blank"
        );
    }
    if let Some(relation) = payload.relation.as_ref() {
        anyhow::ensure!(
            !relation.kind.trim().is_empty(),
            "relation.kind must not be blank"
        );
        anyhow::ensure!(
            !relation.target_event_id.trim().is_empty(),
            "relation.target_event_id must not be blank"
        );
    }
    anyhow::ensure!(
        payload.content.as_bytes().len() <= MAX_EXTERNAL_CONTENT_BYTES,
        "content exceeds the {} byte external ingress limit",
        MAX_EXTERNAL_CONTENT_BYTES
    );
    anyhow::ensure!(
        payload.input_items.len() <= MAX_EXTERNAL_INPUT_ITEMS,
        "input_items exceeds the {} item external ingress limit",
        MAX_EXTERNAL_INPUT_ITEMS
    );
    anyhow::ensure!(
        payload.attachments.len() <= MAX_EXTERNAL_ATTACHMENTS,
        "attachments exceeds the {} item external ingress limit",
        MAX_EXTERNAL_ATTACHMENTS
    );
    anyhow::ensure!(
        payload.input_items.is_empty()
            || (payload.content.trim().is_empty() && payload.attachments.is_empty()),
        "input_items cannot be combined with legacy content or attachments fields"
    );
    for attachment in &payload.attachments {
        validate_inline_asset_request(attachment)?;
    }
    for item in &payload.input_items {
        validate_inline_asset_item(item)?;
    }
    Ok(())
}

fn validate_inline_asset_request(attachment: &crate::InputAttachmentRequest) -> Result<()> {
    if let crate::InputAttachmentRequest::InlineAsset(upload) = attachment {
        validate_inline_asset_upload(upload)?;
    }
    Ok(())
}

fn validate_inline_asset_item(item: &crate::SubmitInputItemRequest) -> Result<()> {
    if let crate::SubmitInputItemRequest::InlineAsset(upload) = item {
        validate_inline_asset_upload(upload)?;
    }
    Ok(())
}

fn validate_inline_asset_upload(upload: &crate::InlineAssetUpload) -> Result<()> {
    let decoded = STANDARD
        .decode(upload.content_base64.trim())
        .map_err(|_| anyhow::anyhow!("attachment content_base64 must be valid base64"))?;
    anyhow::ensure!(
        decoded.len() <= MAX_EXTERNAL_INLINE_ASSET_BYTES,
        "inline assets exceed the {} byte external ingress limit",
        MAX_EXTERNAL_INLINE_ASSET_BYTES
    );
    Ok(())
}

fn external_ingress_fingerprint(payload: &ExternalConnectorIngressEvent) -> String {
    let encoded = serde_json::to_vec(&serde_json::json!({
        "event_id": payload.event_id,
        "fingerprint": payload.fingerprint,
        "occurred_at_ms": payload.occurred_at_ms,
        "actor_id": payload.actor_id,
        "source_kind": payload.source_kind,
        "intent": payload.intent,
        "relation": payload.relation,
        "thread": payload.thread,
        "routing_key": payload.routing_key,
        "content": payload.content,
        "input_items": payload.input_items,
        "attachments": payload.attachments,
        "reply_route": payload.reply_route,
        "metadata": payload.metadata,
    }))
    .expect("external ingress request should serialize");
    let digest = Sha256::digest(encoded);
    hex::encode(digest)
}

fn existing_external_ingress_fingerprint(run: &crate::RunView) -> Option<&str> {
    run.input_metadata
        .as_ref()
        .and_then(|metadata| metadata.get("external_event_fingerprint"))
        .and_then(Value::as_str)
}

fn delivery_references_asset(
    delivery: &crate::delivery::PendingDeliveryRecord,
    asset_id: &str,
) -> bool {
    delivery.artifacts.iter().any(|asset| asset.id == asset_id)
        || delivery.parts.iter().any(|part| {
            matches!(
                part,
                kheish_types::ContentPart::Attachment { attachment } if attachment.id == asset_id
            )
        })
}

#[cfg(test)]
mod tests {
    use crate::SubmitInputItemRequest;
    use crate::connectors::{
        ExternalConnectorIngressEvent, ExternalConnectorIngressEventRelation, ExternalThreadRef,
    };

    use super::{
        external_event_metadata, external_ingress_fingerprint, external_ingress_key,
        validate_external_ingress_payload,
    };

    #[test]
    fn ingress_fingerprint_is_stable_for_the_same_event() {
        let request = ExternalConnectorIngressEvent {
            instance_id: "instance-1".to_string(),
            event_id: "evt-1".to_string(),
            fingerprint: None,
            occurred_at_ms: Some(1),
            actor_id: Some("user-1".to_string()),
            source_kind: Some("message".to_string()),
            intent: Some("message".to_string()),
            relation: None,
            thread: ExternalThreadRef {
                path: vec!["guild".to_string(), "thread".to_string()],
            },
            routing_key: None,
            content: "hello".to_string(),
            input_items: Vec::new(),
            attachments: Vec::new(),
            reply_route: Some("reply".to_string()),
            metadata: None,
        };
        assert_eq!(
            external_ingress_fingerprint(&request),
            external_ingress_fingerprint(&request)
        );
    }

    #[test]
    fn ingress_fingerprint_uses_sidecar_fingerprint_when_present() {
        let mut request = ExternalConnectorIngressEvent {
            instance_id: "instance-1".to_string(),
            event_id: "evt-1".to_string(),
            fingerprint: Some("fp-1".to_string()),
            occurred_at_ms: Some(1),
            actor_id: Some("user-1".to_string()),
            source_kind: Some("message".to_string()),
            intent: Some("message".to_string()),
            relation: None,
            thread: ExternalThreadRef {
                path: vec!["guild".to_string(), "thread".to_string()],
            },
            routing_key: None,
            content: "hello".to_string(),
            input_items: Vec::new(),
            attachments: Vec::new(),
            reply_route: Some("reply".to_string()),
            metadata: None,
        };
        let first = external_ingress_fingerprint(&request);
        request.fingerprint = Some("fp-2".to_string());
        assert_ne!(first, external_ingress_fingerprint(&request));
    }

    #[test]
    fn ingress_fingerprint_uses_v2_fields() {
        let mut request = ExternalConnectorIngressEvent {
            instance_id: "instance-1".to_string(),
            event_id: "evt-1".to_string(),
            fingerprint: None,
            occurred_at_ms: Some(1),
            actor_id: Some("user-1".to_string()),
            source_kind: Some("alert".to_string()),
            intent: Some("domain_event".to_string()),
            relation: Some(ExternalConnectorIngressEventRelation {
                kind: "replaces".to_string(),
                target_event_id: "evt-0".to_string(),
            }),
            thread: ExternalThreadRef::default(),
            routing_key: Some("alerts/team-a".to_string()),
            content: "hello".to_string(),
            input_items: Vec::new(),
            attachments: Vec::new(),
            reply_route: None,
            metadata: None,
        };
        let first = external_ingress_fingerprint(&request);
        request.routing_key = Some("alerts/team-b".to_string());
        assert_ne!(first, external_ingress_fingerprint(&request));
    }

    #[test]
    fn external_ingress_rejects_mixed_ordered_and_legacy_input_shapes() {
        let request = ExternalConnectorIngressEvent {
            instance_id: "instance-1".to_string(),
            event_id: "evt-1".to_string(),
            fingerprint: None,
            occurred_at_ms: Some(1),
            actor_id: Some("user-1".to_string()),
            source_kind: Some("message".to_string()),
            intent: Some("message".to_string()),
            relation: None,
            thread: ExternalThreadRef {
                path: vec!["guild".to_string(), "thread".to_string()],
            },
            routing_key: None,
            content: "legacy content".to_string(),
            input_items: vec![SubmitInputItemRequest::Text {
                text: "ordered content".to_string(),
            }],
            attachments: Vec::new(),
            reply_route: Some("reply".to_string()),
            metadata: None,
        };
        let error = validate_external_ingress_payload(&request)
            .expect_err("mixed legacy and ordered inputs should be rejected");
        assert!(
            error.to_string().contains("input_items cannot be combined"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn external_event_metadata_projects_v2_fields() {
        let request = ExternalConnectorIngressEvent {
            instance_id: "instance-1".to_string(),
            event_id: "evt-1".to_string(),
            fingerprint: None,
            occurred_at_ms: Some(1),
            actor_id: Some("user-1".to_string()),
            source_kind: Some("alert".to_string()),
            intent: Some("domain_event".to_string()),
            relation: Some(ExternalConnectorIngressEventRelation {
                kind: "reply_to".to_string(),
                target_event_id: "evt-0".to_string(),
            }),
            thread: ExternalThreadRef::default(),
            routing_key: Some("alerts/team-a".to_string()),
            content: "payload".to_string(),
            input_items: Vec::new(),
            attachments: Vec::new(),
            reply_route: None,
            metadata: Some(serde_json::json!({"custom": true})),
        };
        let ingress_key = external_ingress_key("test", "evt-1");
        let metadata = external_event_metadata(&request, 2, &ingress_key, "fp-1");
        assert_eq!(
            metadata
                .get("connector_ingress_key")
                .and_then(serde_json::Value::as_str),
            Some(ingress_key.as_str())
        );
        assert!(!ingress_key.contains("evt-1"));
        assert_eq!(
            metadata
                .get("external_event_key_sha256")
                .and_then(serde_json::Value::as_str),
            Some(kheish_codec::digest_text("evt-1").as_str())
        );
        assert_eq!(
            metadata
                .get("external_protocol_version")
                .and_then(serde_json::Value::as_u64),
            Some(2)
        );
        assert_eq!(
            metadata
                .get("external_intent")
                .and_then(serde_json::Value::as_str),
            Some("domain_event")
        );
        assert_eq!(
            metadata
                .get("external_routing_key")
                .and_then(serde_json::Value::as_str),
            Some("alerts/team-a")
        );
        assert_eq!(
            metadata
                .get("external_relation")
                .and_then(serde_json::Value::as_object)
                .and_then(|relation| relation.get("target_event_id"))
                .and_then(serde_json::Value::as_str),
            Some("evt-0")
        );
    }
}
