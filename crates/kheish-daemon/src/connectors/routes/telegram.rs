use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use kheish_core::ModelDriver;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tracing::warn;

use crate::connectors::config::{
    ResolvedTelegramConnector, TelegramIngressMode, telegram_bot_api_method_url,
    telegram_bot_file_url,
};
use crate::{DaemonState, RunView, SubmitInputRequest};

use super::multimodal::{
    ConnectorMediaRef, apply_connector_multimodal_input, connector_ingress_http_client,
    download_connector_media,
};
use super::{
    ConnectorIngressGuard, acquire_connector_ingress_with_fingerprint,
    connector_retry_after_problem_response, internal_error, release_connector_ingress,
    submit_connector_run_with_guard, take_connector_ingress_rate_limit, unauthorized,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TelegramWebhookEnvelope {
    #[serde(default)]
    pub(crate) update_id: Option<i64>,
    #[serde(default)]
    message: Option<TelegramMessage>,
    #[serde(default)]
    edited_message: Option<TelegramMessage>,
    #[serde(default)]
    callback_query: Option<TelegramCallbackQuery>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TelegramMessage {
    pub(crate) message_id: i64,
    #[serde(default)]
    pub(crate) message_thread_id: Option<i64>,
    #[serde(default)]
    pub(crate) text: Option<String>,
    #[serde(default)]
    pub(crate) caption: Option<String>,
    #[serde(default)]
    pub(crate) photo: Vec<TelegramPhotoSize>,
    #[serde(default)]
    pub(crate) document: Option<TelegramDocument>,
    pub(crate) chat: TelegramChat,
    #[serde(default)]
    pub(crate) from: Option<TelegramUser>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TelegramPhotoSize {
    pub(crate) file_id: String,
    #[serde(default)]
    pub(crate) file_size: Option<u64>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TelegramDocument {
    pub(crate) file_id: String,
    #[serde(default)]
    pub(crate) file_name: Option<String>,
    #[serde(default)]
    pub(crate) mime_type: Option<String>,
    #[serde(default)]
    pub(crate) file_size: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TelegramChat {
    pub(crate) id: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TelegramUser {
    pub(crate) id: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TelegramCallbackQuery {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) data: Option<String>,
    #[serde(default)]
    pub(crate) message: Option<TelegramMessage>,
    pub(crate) from: TelegramUser,
}

fn verify_telegram_secret(
    headers: &HeaderMap,
    expected: Option<&str>,
    allow_unauthenticated_ingress: bool,
) -> Result<()> {
    let Some(expected) = expected else {
        anyhow::ensure!(
            allow_unauthenticated_ingress,
            "telegram connector ingress authentication is not configured"
        );
        return Ok(());
    };
    let actual = headers
        .get("x-telegram-bot-api-secret-token")
        .and_then(|value| value.to_str().ok());
    anyhow::ensure!(
        actual
            .map(|value| value.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() == 1)
            .unwrap_or(false),
        "telegram connector secret token mismatch"
    );
    Ok(())
}

pub(super) async fn telegram_webhook<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    Json(payload): Json<TelegramWebhookEnvelope>,
) -> Result<Response, (StatusCode, String)>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let connector = state.connectors().telegram(&name).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("unknown telegram connector {name}"),
        )
    })?;
    if connector.ingress_mode != TelegramIngressMode::Webhook {
        return Err((
            StatusCode::CONFLICT,
            format!("telegram connector {name} is not configured for webhook ingress"),
        ));
    }
    verify_telegram_secret(
        &headers,
        connector.secret_token.as_deref(),
        connector.allow_unauthenticated_ingress,
    )
    .map_err(|error| unauthorized(error.to_string()))?;
    let metadata = serde_json::to_value(&payload)
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    let TelegramWebhookEnvelope {
        update_id,
        message,
        edited_message,
        callback_query,
    } = payload;
    let update_id = update_id.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "telegram webhook update_id is required".to_string(),
        )
    })?;
    let run = if let Some(message) = message {
        submit_telegram_message(
            &state,
            &name,
            &connector,
            Some(update_id),
            message,
            metadata,
        )
        .await
    } else if let Some(message) = edited_message {
        submit_telegram_message_with_kind(
            &state,
            &name,
            &connector,
            Some(update_id),
            message,
            metadata,
            "edited_message",
        )
        .await
    } else if let Some(callback_query) = callback_query {
        submit_telegram_callback_query(
            &state,
            &name,
            &connector,
            Some(update_id),
            callback_query,
            metadata,
        )
        .await
    } else {
        return Ok(Json(serde_json::json!({
            "accepted": true,
            "skipped": true,
            "reason": "unsupported_update",
            "update_id": update_id,
        }))
        .into_response());
    };
    match run {
        Ok(run) => Ok(Json(run).into_response()),
        Err(error) => telegram_ingress_retry_after_ms(&error)
            .map(|retry_after_ms| telegram_retry_after_response(&name, retry_after_ms))
            .ok_or_else(|| internal_error(error)),
    }
}

pub(crate) async fn submit_telegram_message<M>(
    state: &Arc<DaemonState<M>>,
    connector_name: &str,
    connector: &ResolvedTelegramConnector,
    update_id: Option<i64>,
    message: TelegramMessage,
    metadata: serde_json::Value,
) -> Result<RunView>
where
    M: ModelDriver + Send + Sync + 'static,
{
    submit_telegram_message_with_kind(
        state,
        connector_name,
        connector,
        update_id,
        message,
        metadata,
        connector_name,
    )
    .await
}

pub(crate) async fn submit_telegram_message_with_kind<M>(
    state: &Arc<DaemonState<M>>,
    connector_name: &str,
    connector: &ResolvedTelegramConnector,
    update_id: Option<i64>,
    message: TelegramMessage,
    metadata: serde_json::Value,
    source_kind: &str,
) -> Result<RunView>
where
    M: ModelDriver + Send + Sync + 'static,
{
    ensure_telegram_chat_allowed(connector, message.chat.id)?;
    let ingress_fingerprint = telegram_ingress_fingerprint(&metadata)?;
    let content = message
        .text
        .clone()
        .or(message.caption.clone())
        .filter(|value| !value.trim().is_empty());
    let binding_keys = connector.binding_keys(message.chat.id, message.message_thread_id);
    let session_id = connector
        .fixed_session_id
        .clone()
        .or(state.bound_session_id(&binding_keys).await?)
        .unwrap_or_else(|| {
            connector.natural_session_id(message.chat.id, message.message_thread_id)
        });
    let reply_targets = connector.reply_targets(
        message.chat.id,
        message.message_thread_id,
        Some(message.message_id),
    );
    let ingress_key = update_id.map(|update_id| format!("telegram:{connector_name}:{update_id}"));
    let ingress = acquire_connector_ingress_with_fingerprint(
        state,
        ingress_key.as_deref(),
        &ingress_fingerprint,
    )
    .await?;
    if let ConnectorIngressGuard::Existing(run) = &ingress {
        ensure_existing_telegram_ingress_fingerprint(run, &ingress_fingerprint)?;
        return Ok(run.clone());
    }
    if let Some(retry_after_ms) =
        take_telegram_ingress_rate_limit(state, connector_name, connector).await
    {
        release_connector_ingress(state, ingress).await?;
        return Err(telegram_ingress_retry_after_error(
            retry_after_ms,
            format!(
                "telegram connector {connector_name} ingress rate limit exceeded; retry_after_ms={retry_after_ms}"
            ),
        ));
    }
    let media = match telegram_media_refs(connector, &message).await {
        Ok(media) => media,
        Err(error) => {
            release_connector_ingress(state, ingress).await?;
            return Err(error);
        }
    };
    let uploads = match download_connector_media(&media).await {
        Ok(uploads) => uploads,
        Err(error) => {
            release_connector_ingress(state, ingress).await?;
            return Err(error);
        }
    };
    let mut request = SubmitInputRequest {
        provider: None,
        source_plugin: Some("telegram".to_string()),
        source_kind: Some(source_kind.to_string()),
        actor_id: message
            .from
            .map(|user| user.id.to_string())
            .or(Some("telegram-user".to_string())),
        content: String::new(),
        input_items: Vec::new(),
        attachments: Vec::new(),
        generation: None,
        completion_requirements: None,
        metadata: Some(telegram_metadata_with_ingress_key(
            metadata,
            ingress_key.as_deref(),
            &ingress_fingerprint,
        )),
        binding_keys,
        reply_targets,
        reply_plugin: None,
        reply_address: None,
    };
    if let Err(error) = apply_connector_multimodal_input(&mut request, content, uploads) {
        release_connector_ingress(state, ingress).await?;
        return Err(error);
    }
    submit_connector_run_with_guard(
        state,
        &session_id,
        request,
        ingress,
        &connector.session_policy,
        &format!("telegram connector {connector_name}"),
    )
    .await
}

pub(crate) async fn submit_telegram_callback_query<M>(
    state: &Arc<DaemonState<M>>,
    connector_name: &str,
    connector: &ResolvedTelegramConnector,
    update_id: Option<i64>,
    callback: TelegramCallbackQuery,
    metadata: serde_json::Value,
) -> Result<RunView>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let message = callback
        .message
        .clone()
        .ok_or_else(|| anyhow!("missing telegram callback message"))?;
    ensure_telegram_chat_allowed(connector, message.chat.id)?;
    let ingress_fingerprint = telegram_ingress_fingerprint(&metadata)?;
    let content = callback
        .data
        .clone()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("missing telegram callback data"))?;
    let binding_keys = connector.binding_keys(message.chat.id, message.message_thread_id);
    let session_id = connector
        .fixed_session_id
        .clone()
        .or(state.bound_session_id(&binding_keys).await?)
        .unwrap_or_else(|| {
            connector.natural_session_id(message.chat.id, message.message_thread_id)
        });
    let reply_targets = connector.reply_targets(
        message.chat.id,
        message.message_thread_id,
        Some(message.message_id),
    );
    let ingress_key = update_id.map(|update_id| format!("telegram:{connector_name}:{update_id}"));
    let ingress = acquire_connector_ingress_with_fingerprint(
        state,
        ingress_key.as_deref(),
        &ingress_fingerprint,
    )
    .await?;
    if let ConnectorIngressGuard::Existing(run) = &ingress {
        ensure_existing_telegram_ingress_fingerprint(run, &ingress_fingerprint)?;
        return Ok(run.clone());
    }
    if let Some(retry_after_ms) =
        take_telegram_ingress_rate_limit(state, connector_name, connector).await
    {
        release_connector_ingress(state, ingress).await?;
        return Err(telegram_ingress_retry_after_error(
            retry_after_ms,
            format!(
                "telegram connector {connector_name} ingress rate limit exceeded; retry_after_ms={retry_after_ms}"
            ),
        ));
    }
    let mut request = SubmitInputRequest {
        provider: None,
        source_plugin: Some("telegram".to_string()),
        source_kind: Some("callback_query".to_string()),
        actor_id: Some(callback.from.id.to_string()),
        content: String::new(),
        input_items: Vec::new(),
        attachments: Vec::new(),
        generation: None,
        completion_requirements: None,
        metadata: Some(telegram_metadata_with_ingress_key(
            metadata,
            ingress_key.as_deref(),
            &ingress_fingerprint,
        )),
        binding_keys,
        reply_targets,
        reply_plugin: None,
        reply_address: None,
    };
    if let Err(error) = apply_connector_multimodal_input(&mut request, Some(content), Vec::new()) {
        release_connector_ingress(state, ingress).await?;
        return Err(error);
    }
    let run = submit_connector_run_with_guard(
        state,
        &session_id,
        request,
        ingress,
        &connector.session_policy,
        &format!("telegram connector {connector_name}"),
    )
    .await?;
    if let Some(bot_token) = connector.bot_token.as_deref()
        && let Err(error) = answer_telegram_callback_query(connector, bot_token, &callback.id).await
    {
        warn!(
            connector = connector_name,
            error = %redact_telegram_bot_token(&error.to_string(), bot_token),
            "telegram callback ack failed"
        );
    }
    Ok(run)
}

fn ensure_telegram_chat_allowed(connector: &ResolvedTelegramConnector, chat_id: i64) -> Result<()> {
    anyhow::ensure!(
        connector.allows_chat_id(chat_id),
        "telegram chat {chat_id} is outside connector {} allowlist",
        connector.name
    );
    Ok(())
}

async fn take_telegram_ingress_rate_limit<M>(
    state: &Arc<DaemonState<M>>,
    connector_name: &str,
    connector: &ResolvedTelegramConnector,
) -> Option<u64>
where
    M: ModelDriver + Send + Sync + 'static,
{
    take_connector_ingress_rate_limit(
        format!(
            "telegram:{}:{connector_name}",
            state.control_plane_base_url()
        ),
        connector.ingress_events_per_second,
    )
    .await
}

#[derive(Debug)]
struct TelegramIngressRetryAfterError {
    retry_after_ms: u64,
    detail: String,
}

impl std::fmt::Display for TelegramIngressRetryAfterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for TelegramIngressRetryAfterError {}

fn telegram_ingress_retry_after_error(
    retry_after_ms: u64,
    detail: impl Into<String>,
) -> anyhow::Error {
    TelegramIngressRetryAfterError {
        retry_after_ms: retry_after_ms.max(1),
        detail: detail.into(),
    }
    .into()
}

fn telegram_ingress_retry_after_ms(error: &anyhow::Error) -> Option<u64> {
    error
        .downcast_ref::<TelegramIngressRetryAfterError>()
        .map(|error| error.retry_after_ms)
}

fn telegram_retry_after_response(name: &str, retry_after_ms: u64) -> Response {
    let message = format!(
        "telegram connector {name} ingress rate limit exceeded; retry_after_ms={retry_after_ms}"
    );
    connector_retry_after_problem_response("telegram_ingress_rate_limited", message, retry_after_ms)
}

fn telegram_metadata_with_ingress_key(
    mut metadata: serde_json::Value,
    ingress_key: Option<&str>,
    ingress_fingerprint: &str,
) -> serde_json::Value {
    match &mut metadata {
        serde_json::Value::Object(map) => {
            if let Some(ingress_key) = ingress_key {
                map.insert(
                    "connector_ingress_key".to_string(),
                    serde_json::Value::String(ingress_key.to_string()),
                );
            }
            map.insert(
                "telegram_ingress_fingerprint".to_string(),
                serde_json::Value::String(ingress_fingerprint.to_string()),
            );
            metadata
        }
        _ => serde_json::json!({
            "telegram": metadata,
            "connector_ingress_key": ingress_key,
            "telegram_ingress_fingerprint": ingress_fingerprint,
        }),
    }
}

fn telegram_ingress_fingerprint(metadata: &serde_json::Value) -> Result<String> {
    kheish_codec::digest_serialize(metadata)
}

fn existing_telegram_ingress_fingerprint(run: &RunView) -> Option<&str> {
    run.input_metadata
        .as_ref()?
        .get("telegram_ingress_fingerprint")?
        .as_str()
}

fn ensure_existing_telegram_ingress_fingerprint(
    run: &RunView,
    ingress_fingerprint: &str,
) -> Result<()> {
    if existing_telegram_ingress_fingerprint(run)
        .is_some_and(|existing| existing != ingress_fingerprint)
    {
        bail!("telegram connector update_id was already submitted with a different payload");
    }
    Ok(())
}

async fn answer_telegram_callback_query(
    connector: &ResolvedTelegramConnector,
    bot_token: &str,
    callback_query_id: &str,
) -> Result<()> {
    let response = connector_ingress_http_client()
        .post(telegram_bot_api_method_url(
            &connector.api_base_url,
            bot_token,
            "answerCallbackQuery",
        ))
        .json(&serde_json::json!({ "callback_query_id": callback_query_id }))
        .send()
        .await
        .map_err(|error| {
            anyhow!(
                "failed to answer telegram callback query: {}",
                redact_telegram_bot_token(&error.to_string(), bot_token)
            )
        })?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read telegram callback answer response")?;
    if !status.is_success() {
        bail!(
            "telegram answerCallbackQuery returned HTTP {status}: {}",
            redact_telegram_bot_token(&body, bot_token)
        );
    }
    let body = serde_json::from_str::<serde_json::Value>(&body)
        .context("failed to decode telegram callback answer response")?;
    if !body
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        bail!("telegram answerCallbackQuery returned ok=false");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct TelegramGetFileResponse {
    ok: bool,
    #[serde(default)]
    result: Option<TelegramGetFileResult>,
}

#[derive(Debug, Deserialize)]
struct TelegramGetFileResult {
    file_path: String,
}

async fn telegram_media_refs(
    connector: &ResolvedTelegramConnector,
    message: &TelegramMessage,
) -> Result<Vec<ConnectorMediaRef>> {
    let mut files = Vec::new();
    if let Some(document) = message.document.as_ref() {
        files.push(telegram_document_media_ref(connector, document).await?);
    }
    if let Some(photo) = message.photo.iter().max_by_key(|candidate| {
        candidate
            .file_size
            .unwrap_or((candidate.width as u64) * (candidate.height as u64))
    }) {
        files.push(telegram_photo_media_ref(connector, photo).await?);
    }
    Ok(files)
}

async fn telegram_document_media_ref(
    connector: &ResolvedTelegramConnector,
    document: &TelegramDocument,
) -> Result<ConnectorMediaRef> {
    let file_path = resolve_telegram_file_path(connector, &document.file_id).await?;
    Ok(ConnectorMediaRef {
        file_name: document
            .file_name
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "telegram-document.bin".to_string()),
        media_type: document.mime_type.clone(),
        byte_length_hint: document.file_size,
        url: telegram_bot_file_url(
            &connector.api_base_url,
            connector.bot_token.as_deref().ok_or_else(|| {
                anyhow!("telegram connector requires bot_token for media downloads")
            })?,
            &file_path,
        ),
        bearer_token: None,
    })
}

async fn telegram_photo_media_ref(
    connector: &ResolvedTelegramConnector,
    photo: &TelegramPhotoSize,
) -> Result<ConnectorMediaRef> {
    let file_path = resolve_telegram_file_path(connector, &photo.file_id).await?;
    Ok(ConnectorMediaRef {
        file_name: format!("telegram-photo-{}.jpg", photo.file_id),
        media_type: Some("image/jpeg".to_string()),
        byte_length_hint: photo.file_size,
        url: telegram_bot_file_url(
            &connector.api_base_url,
            connector.bot_token.as_deref().ok_or_else(|| {
                anyhow!("telegram connector requires bot_token for media downloads")
            })?,
            &file_path,
        ),
        bearer_token: None,
    })
}

async fn resolve_telegram_file_path(
    connector: &ResolvedTelegramConnector,
    file_id: &str,
) -> Result<String> {
    let bot_token = connector
        .bot_token
        .as_deref()
        .ok_or_else(|| anyhow!("telegram connector requires bot_token for media downloads"))?;
    let response = connector_ingress_http_client()
        .post(telegram_bot_api_method_url(
            &connector.api_base_url,
            bot_token,
            "getFile",
        ))
        .json(&serde_json::json!({ "file_id": file_id }))
        .send()
        .await
        .map_err(|error| {
            anyhow!(
                "failed to fetch Telegram file metadata for {file_id}: {}",
                redact_telegram_bot_token(&error.to_string(), bot_token)
            )
        })?;
    let status = response.status();
    let raw_body = response
        .text()
        .await
        .context("failed to read Telegram getFile response")?;
    if !status.is_success() {
        bail!(
            "telegram getFile returned HTTP {status}: {}",
            redact_telegram_bot_token(&raw_body, bot_token)
        );
    }
    let body = serde_json::from_str::<TelegramGetFileResponse>(&raw_body)
        .context("failed to decode Telegram getFile response")?;
    if !body.ok {
        bail!("telegram getFile returned ok=false for {file_id}");
    }
    body.result
        .map(|result| result.file_path)
        .ok_or_else(|| anyhow!("telegram getFile response missing file_path"))
}

fn redact_telegram_bot_token(value: &str, bot_token: &str) -> String {
    if bot_token.is_empty() {
        value.to_string()
    } else {
        value.replace(bot_token, "<redacted>")
    }
}
