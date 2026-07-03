use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use kheish_output::{OutputManifest, OutputPlugin, ResponseEnvelope};
use kheish_runtime::RuntimeObserver;
use kheish_session::write_json_pretty_atomically;
use kheish_types::ContentPart;
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::task;

use crate::assets::{FileAssetStore, StoredAssetRecord};
use crate::connectors::config::{
    ConnectorRegistry, decode_telegram_reply_route, telegram_bot_api_method_url,
};
use crate::delivery::{DeliveryTransport, retry_after_delivery_error, terminal_delivery_error};
use crate::state_files::read_json_or_quarantine;

use super::{OutputAuditSpan, output_http_client, stable_target_digest, summarize_delivery_target};

const TELEGRAM_TEXT_LIMIT: usize = 4_096;
const MAX_TELEGRAM_RETRY_AFTER_MS: u64 = 60 * 60 * 1_000;

pub struct TelegramOutputPlugin {
    connectors: Arc<ConnectorRegistry>,
    assets: Arc<FileAssetStore>,
    client: reqwest::Client,
    observer: Arc<dyn RuntimeObserver>,
    progress_store: TelegramDeliveryProgressStore,
}

impl TelegramOutputPlugin {
    pub fn new(
        connectors: Arc<ConnectorRegistry>,
        assets: Arc<FileAssetStore>,
        observer: Arc<dyn RuntimeObserver>,
        progress_root: PathBuf,
    ) -> Self {
        Self {
            connectors,
            assets,
            client: output_http_client(),
            observer,
            progress_store: TelegramDeliveryProgressStore::new(progress_root),
        }
    }
}

#[derive(Clone, Debug)]
struct TelegramDeliveryProgressStore {
    root: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct TelegramDeliveryProgress {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    steps: BTreeMap<String, TelegramStepProgress>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct TelegramStepProgress {
    sent: bool,
}

impl TelegramDeliveryProgressStore {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn load(&self, delivery_id: &str) -> Result<TelegramDeliveryProgress> {
        let path = self.progress_path(delivery_id);
        let progress_file_existed = path.try_exists().with_context(|| {
            format!(
                "failed to inspect telegram delivery progress {}",
                path.display()
            )
        })?;
        if !progress_file_existed {
            return Ok(TelegramDeliveryProgress::default());
        }
        read_json_or_quarantine(&path, "telegram delivery progress")?.ok_or_else(|| {
            terminal_delivery_error(format!(
                "telegram delivery progress {} was corrupt and has been quarantined; inspect before replaying",
                path.display()
            ))
        })
    }

    fn save(&self, delivery_id: &str, progress: &TelegramDeliveryProgress) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        write_json_pretty_atomically(&self.progress_path(delivery_id), progress)
    }

    fn progress_path(&self, delivery_id: &str) -> PathBuf {
        self.root.join(format!(
            "{}.json",
            safe_telegram_progress_file_id(delivery_id)
        ))
    }
}

#[async_trait]
impl OutputPlugin for TelegramOutputPlugin {
    fn manifest(&self) -> OutputManifest {
        OutputManifest {
            name: "telegram".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Telegram bot output".to_string(),
        }
    }

    async fn deliver(&self, response: ResponseEnvelope) -> Result<()> {
        let reply = response
            .reply
            .as_ref()
            .ok_or_else(|| anyhow!("telegram output requires one reply target"))?;
        let route = decode_telegram_reply_route(&reply.address)?;
        let connector = self
            .connectors
            .telegram(&route.connector)
            .ok_or_else(|| anyhow!("unknown telegram connector {}", route.connector))?;
        if !connector.allows_chat_id(route.chat_id) {
            return Err(terminal_delivery_error(
                "telegram reply target is outside connector chat allowlist",
            ));
        }
        let bot_token = connector.bot_token.as_deref().ok_or_else(|| {
            anyhow!(
                "telegram connector {} has no bot token configured",
                connector.name
            )
        })?;

        let audit = OutputAuditSpan::start(
            self.observer.clone(),
            telegram_audit_target(&connector.name, route.chat_id),
            &json!({
                "chat_id": route.chat_id,
                "message_thread_id": route.message_thread_id,
                "reply_to_message_id": route.reply_to_message_id,
                "conversation": &response.conversation,
                "content": &response.content,
                "parts": &response.parts,
                "artifacts": &response.artifacts,
                "metadata": &response.metadata,
            }),
        )?;
        let delivery = async {
            let progress_id = telegram_delivery_progress_id(&response)?;
            let mut progress = self.progress_store.load(&progress_id)?;
            if response.parts.is_empty() {
                send_text_chunk_steps(
                    &self.progress_store,
                    &progress_id,
                    &mut progress,
                    &self.client,
                    &connector.api_base_url,
                    bot_token,
                    route.chat_id,
                    route.message_thread_id,
                    route.reply_to_message_id,
                    "content",
                    &response.content,
                )
                .await
                .with_context(|| {
                    format!("failed to deliver telegram output via {}", connector.name)
                })?;
                return Ok::<_, anyhow::Error>(());
            }

            for (index, part) in response.parts.iter().enumerate() {
                match part {
                    ContentPart::Text { text } if !text.trim().is_empty() => {
                        send_text_chunk_steps(
                            &self.progress_store,
                            &progress_id,
                            &mut progress,
                            &self.client,
                            &connector.api_base_url,
                            bot_token,
                            route.chat_id,
                            route.message_thread_id,
                            route.reply_to_message_id,
                            &format!("text:{index}"),
                            text,
                        )
                        .await
                        .with_context(|| {
                            format!("failed to deliver telegram output via {}", connector.name)
                        })?;
                    }
                    ContentPart::Text { .. } => {}
                    ContentPart::Attachment { attachment } => {
                        let (record, bytes) =
                            load_attachment_bytes(self.assets.clone(), attachment.id.clone())
                                .await?;
                        send_attachment_step(
                            &self.progress_store,
                            &progress_id,
                            &mut progress,
                            &self.client,
                            &connector.api_base_url,
                            bot_token,
                            route.chat_id,
                            route.message_thread_id,
                            route.reply_to_message_id,
                            &format!("attachment:{index}:{}", attachment.id),
                            &record.media_type,
                            record
                                .file_name
                                .as_str()
                                .trim()
                                .is_empty()
                                .then_some("attachment.bin")
                                .unwrap_or(record.file_name.as_str()),
                            bytes,
                            None,
                        )
                        .await
                        .with_context(|| {
                            format!("failed to deliver telegram output via {}", connector.name)
                        })?;
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        match delivery {
            Ok(()) => {
                audit.record_success(&json!({ "status": "delivered" }), "delivered")?;
                Ok(())
            }
            Err(error) => {
                audit.record_failure(&error)?;
                Err(error)
            }
        }
    }
}

#[async_trait]
impl DeliveryTransport for TelegramOutputPlugin {
    async fn deliver(&self, response: ResponseEnvelope) -> Result<()> {
        <Self as OutputPlugin>::deliver(self, response).await
    }
}

fn telegram_audit_target(connector_name: &str, chat_id: i64) -> String {
    format!(
        "telegram:{}:chat_sha256:{}",
        summarize_delivery_target(connector_name),
        stable_target_digest(&chat_id.to_string())
    )
}

async fn send_text(
    client: &reqwest::Client,
    api_base_url: &str,
    bot_token: &str,
    chat_id: i64,
    message_thread_id: Option<i64>,
    reply_to_message_id: Option<i64>,
    text: &str,
) -> Result<()> {
    let response = client
        .post(telegram_bot_api_method_url(
            api_base_url,
            bot_token,
            "sendMessage",
        ))
        .json(&json!({
            "chat_id": chat_id,
            "message_thread_id": message_thread_id,
            "reply_to_message_id": reply_to_message_id,
            "text": text,
        }))
        .send()
        .await
        .map_err(|error| {
            anyhow!(
                "failed to call telegram sendMessage: {}",
                redact_telegram_bot_token(&error.to_string(), bot_token)
            )
        })?;
    telegram_ok_response(response, "sendMessage", bot_token).await
}

async fn send_text_chunk_steps(
    store: &TelegramDeliveryProgressStore,
    progress_id: &str,
    progress: &mut TelegramDeliveryProgress,
    client: &reqwest::Client,
    api_base_url: &str,
    bot_token: &str,
    chat_id: i64,
    message_thread_id: Option<i64>,
    reply_to_message_id: Option<i64>,
    step_prefix: &str,
    text: &str,
) -> Result<()> {
    for (chunk_index, chunk) in split_telegram_text(text).into_iter().enumerate() {
        let step_key = format!("{step_prefix}:chunk:{chunk_index}");
        if progress.steps.get(&step_key).is_some_and(|step| step.sent) {
            continue;
        }
        send_text(
            client,
            api_base_url,
            bot_token,
            chat_id,
            message_thread_id,
            reply_to_message_id,
            &chunk,
        )
        .await?;
        progress
            .steps
            .insert(step_key, TelegramStepProgress { sent: true });
        store.save(progress_id, progress)?;
    }
    Ok(())
}

#[cfg(test)]
async fn send_text_chunks(
    client: &reqwest::Client,
    api_base_url: &str,
    bot_token: &str,
    chat_id: i64,
    message_thread_id: Option<i64>,
    reply_to_message_id: Option<i64>,
    text: &str,
) -> Result<()> {
    for chunk in split_telegram_text(text) {
        send_text(
            client,
            api_base_url,
            bot_token,
            chat_id,
            message_thread_id,
            reply_to_message_id,
            &chunk,
        )
        .await?;
    }
    Ok(())
}

async fn send_attachment_step(
    store: &TelegramDeliveryProgressStore,
    progress_id: &str,
    progress: &mut TelegramDeliveryProgress,
    client: &reqwest::Client,
    api_base_url: &str,
    bot_token: &str,
    chat_id: i64,
    message_thread_id: Option<i64>,
    reply_to_message_id: Option<i64>,
    step_key: &str,
    media_type: &str,
    file_name: &str,
    bytes: Vec<u8>,
    caption: Option<String>,
) -> Result<()> {
    if progress.steps.get(step_key).is_some_and(|step| step.sent) {
        return Ok(());
    }
    send_attachment(
        client,
        api_base_url,
        bot_token,
        chat_id,
        message_thread_id,
        reply_to_message_id,
        media_type,
        file_name,
        bytes,
        caption,
    )
    .await?;
    progress
        .steps
        .insert(step_key.to_string(), TelegramStepProgress { sent: true });
    store.save(progress_id, progress)
}

async fn send_attachment(
    client: &reqwest::Client,
    api_base_url: &str,
    bot_token: &str,
    chat_id: i64,
    message_thread_id: Option<i64>,
    reply_to_message_id: Option<i64>,
    media_type: &str,
    file_name: &str,
    bytes: Vec<u8>,
    caption: Option<String>,
) -> Result<()> {
    let (method, field_name) = telegram_attachment_method(media_type);
    let part = Part::bytes(bytes)
        .file_name(file_name.to_string())
        .mime_str(media_type)?;
    let mut form = Form::new()
        .text("chat_id", chat_id.to_string())
        .part(field_name.to_string(), part);
    if let Some(message_thread_id) = message_thread_id {
        form = form.text("message_thread_id", message_thread_id.to_string());
    }
    if let Some(reply_to_message_id) = reply_to_message_id {
        form = form.text("reply_to_message_id", reply_to_message_id.to_string());
    }
    if let Some(caption) = caption.filter(|value| !value.trim().is_empty()) {
        form = form.text("caption", caption);
    }
    let response = client
        .post(telegram_bot_api_method_url(api_base_url, bot_token, method))
        .multipart(form)
        .send()
        .await
        .map_err(|error| {
            anyhow!(
                "failed to call telegram {method}: {}",
                redact_telegram_bot_token(&error.to_string(), bot_token)
            )
        })?;
    telegram_ok_response(response, method, bot_token).await
}

fn telegram_attachment_method(media_type: &str) -> (&'static str, &'static str) {
    if matches!(media_type, "image/png" | "image/jpeg") {
        return ("sendPhoto", "photo");
    }
    if media_type.starts_with("audio/") {
        return ("sendAudio", "audio");
    }
    ("sendDocument", "document")
}

async fn load_attachment_bytes(
    assets: Arc<FileAssetStore>,
    asset_id: String,
) -> Result<(StoredAssetRecord, Vec<u8>)> {
    task::spawn_blocking(move || assets.read_raw(&asset_id))
        .await
        .map_err(|error| anyhow!("telegram asset load task failed: {error}"))?
}

#[derive(Debug, Deserialize)]
struct TelegramApiResponse {
    ok: bool,
    #[serde(default)]
    error_code: Option<u16>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<TelegramResponseParameters>,
}

#[derive(Debug, Deserialize)]
struct TelegramResponseParameters {
    #[serde(default)]
    retry_after: Option<u64>,
}

async fn telegram_ok_response(
    response: reqwest::Response,
    method: &str,
    bot_token: &str,
) -> Result<()> {
    let status = response.status();
    let headers = response.headers().clone();
    let raw = response
        .text()
        .await
        .context("failed to read telegram output response")?;
    let header_retry_after_ms = telegram_retry_after_header_ms(&headers);
    let body = match serde_json::from_str::<TelegramApiResponse>(&raw) {
        Ok(body) => body,
        Err(_) if status == StatusCode::TOO_MANY_REQUESTS && header_retry_after_ms.is_some() => {
            let retry_after_ms = header_retry_after_ms.unwrap_or(1_000);
            return Err(retry_after_delivery_error(
                retry_after_ms,
                format!("telegram {method} flood-wait; retry after {retry_after_ms}ms"),
            ));
        }
        Err(error) => {
            return Err(anyhow!(
                "failed to decode telegram {method} response: {error}"
            ));
        }
    };
    if let Some(retry_after_ms) = telegram_retry_after_ms(&headers, Some(&body))
        && (status == StatusCode::TOO_MANY_REQUESTS || body.error_code == Some(429))
    {
        return Err(retry_after_delivery_error(
            retry_after_ms,
            format!("telegram {method} flood-wait; retry after {retry_after_ms}ms"),
        ));
    }
    if status.is_success() && body.ok {
        return Ok(());
    }
    Err(telegram_api_error(status, &body, &raw, method, bot_token))
}

fn telegram_retry_after_ms(headers: &HeaderMap, body: Option<&TelegramApiResponse>) -> Option<u64> {
    telegram_retry_after_header_ms(headers).or_else(|| {
        body.and_then(|body| {
            body.parameters
                .as_ref()
                .and_then(|parameters| parameters.retry_after)
                .map(|seconds| seconds.saturating_mul(1_000).max(1))
        })
    })
}

fn telegram_retry_after_header_ms(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_telegram_retry_after_ms)
}

fn parse_telegram_retry_after_ms(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(seconds) = trimmed.parse::<u64>() {
        return Some(cap_telegram_retry_after_ms(seconds.saturating_mul(1_000)));
    }
    let date = httpdate::parse_http_date(trimmed).ok()?;
    let delay = date
        .duration_since(SystemTime::now())
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(1);
    Some(cap_telegram_retry_after_ms(delay))
}

fn cap_telegram_retry_after_ms(value: u64) -> u64 {
    value.clamp(1, MAX_TELEGRAM_RETRY_AFTER_MS)
}

fn telegram_api_error(
    status: StatusCode,
    body: &TelegramApiResponse,
    raw_body: &str,
    method: &str,
    bot_token: &str,
) -> anyhow::Error {
    let description = redact_telegram_bot_token(
        body.description.as_deref().unwrap_or("telegram error"),
        bot_token,
    );
    if telegram_terminal_error(status, body) {
        return terminal_delivery_error(format!(
            "telegram {method} failed permanently: {description}"
        ));
    }
    anyhow!(
        "telegram {method} failed with HTTP {}: {}",
        status.as_u16(),
        redact_telegram_bot_token(raw_body, bot_token)
    )
}

fn telegram_terminal_error(status: StatusCode, body: &TelegramApiResponse) -> bool {
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
        || matches!(body.error_code, Some(401 | 403))
    {
        return true;
    }
    let description = body
        .description
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(body.error_code, Some(400))
        && (description.contains("message is too long")
            || description.contains("chat not found")
            || description.contains("message thread not found")
            || description.contains("bot was blocked"))
}

fn split_telegram_text(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    let mut chars_in_chunk = 0usize;
    for ch in text.chars() {
        if chars_in_chunk >= TELEGRAM_TEXT_LIMIT {
            chunks.push(std::mem::take(&mut chunk));
            chars_in_chunk = 0;
        }
        chunk.push(ch);
        chars_in_chunk += 1;
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

fn redact_telegram_bot_token(value: &str, bot_token: &str) -> String {
    if bot_token.is_empty() {
        value.to_string()
    } else {
        value.replace(bot_token, "<redacted>")
    }
}

fn telegram_delivery_progress_id(response: &ResponseEnvelope) -> Result<String> {
    if let Some(delivery_id) = response
        .metadata
        .get("delivery_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(delivery_id.to_string());
    }
    let digest = kheish_codec::digest_serialize(response)?;
    Ok(format!(
        "envelope-{}",
        digest.get(..32).unwrap_or(digest.as_str())
    ))
}

fn safe_telegram_progress_file_id(value: &str) -> String {
    let safe = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if safe {
        return value.to_string();
    }
    let digest = kheish_codec::digest_text(value);
    format!("id-{}", digest.get(..32).unwrap_or(digest.as_str()))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::http::StatusCode as AxumStatusCode;
    use axum::{Json, Router, extract::State, routing::post};
    use kheish_auth::AuthManager;
    use kheish_output::{OutputPlugin, ResponseEnvelope};
    use kheish_runtime::NoopObserver;
    use kheish_types::{ConversationKey, ReplyHandle};
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use serde_json::{Value, json};
    use tempfile::tempdir;
    use tokio::net::TcpListener;

    use crate::assets::FileAssetStore;
    use crate::connectors::config::{
        ConnectorRegistry, ConnectorSessionPolicy, ConnectorSettings, TelegramConnectorConfig,
        TelegramIngressMode, TelegramReplyRoute, encode_telegram_reply_route,
    };

    use super::{
        TELEGRAM_TEXT_LIMIT, TelegramApiResponse, TelegramOutputPlugin, TelegramResponseParameters,
        parse_telegram_retry_after_ms, send_text_chunks, split_telegram_text,
        telegram_audit_target, telegram_retry_after_ms, telegram_terminal_error,
    };

    #[test]
    fn telegram_audit_target_redacts_chat_id() {
        let target = telegram_audit_target("bot", 123456789);
        let repeated = telegram_audit_target("bot", 123456789);
        assert_eq!(target, repeated);
        assert!(target.starts_with("telegram:bot:chat_sha256:"));
        assert!(!target.contains("123456789"));
    }

    #[test]
    fn telegram_text_chunks_are_utf8_safe_and_bounded() {
        let text = format!("{}{}", "a".repeat(TELEGRAM_TEXT_LIMIT), "é".repeat(3));
        let chunks = split_telegram_text(&text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), TELEGRAM_TEXT_LIMIT);
        assert_eq!(chunks[1], "ééé");
    }

    #[test]
    fn telegram_retry_after_prefers_header_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("2"));
        let body = TelegramApiResponse {
            ok: false,
            error_code: Some(429),
            description: Some("Too Many Requests".to_string()),
            parameters: Some(TelegramResponseParameters {
                retry_after: Some(9),
            }),
        };
        assert_eq!(telegram_retry_after_ms(&headers, Some(&body)), Some(2_000));
    }

    #[test]
    fn telegram_retry_after_reads_body_parameters() {
        let headers = HeaderMap::new();
        let body = TelegramApiResponse {
            ok: false,
            error_code: Some(429),
            description: Some("Too Many Requests".to_string()),
            parameters: Some(TelegramResponseParameters {
                retry_after: Some(7),
            }),
        };
        assert_eq!(telegram_retry_after_ms(&headers, Some(&body)), Some(7_000));
    }

    #[test]
    fn telegram_output_retry_after_caps_large_values_and_reads_http_dates() {
        assert_eq!(
            parse_telegram_retry_after_ms("7200"),
            Some(super::MAX_TELEGRAM_RETRY_AFTER_MS)
        );

        let future = httpdate::fmt_http_date(
            std::time::SystemTime::now() + std::time::Duration::from_secs(2),
        );
        let parsed = parse_telegram_retry_after_ms(&future)
            .expect("future HTTP-date retry-after should parse");
        assert!(
            (1..=2_000).contains(&parsed),
            "unexpected parsed retry-after: {parsed}"
        );
    }

    #[test]
    fn telegram_terminal_errors_cover_auth_and_blocked_bot() {
        let auth = TelegramApiResponse {
            ok: false,
            error_code: Some(401),
            description: Some("Unauthorized".to_string()),
            parameters: None,
        };
        assert!(telegram_terminal_error(
            reqwest::StatusCode::UNAUTHORIZED,
            &auth
        ));

        let blocked = TelegramApiResponse {
            ok: false,
            error_code: Some(400),
            description: Some("Bad Request: bot was blocked by the user".to_string()),
            parameters: None,
        };
        assert!(telegram_terminal_error(
            reqwest::StatusCode::BAD_REQUEST,
            &blocked
        ));
    }

    #[test]
    fn missing_telegram_delivery_progress_loads_default() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let store = super::TelegramDeliveryProgressStore::new(temp.path().join("progress"));

        assert_eq!(
            store.load("delivery-missing")?,
            super::TelegramDeliveryProgress::default()
        );

        Ok(())
    }

    #[test]
    fn corrupt_telegram_delivery_progress_fails_closed() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let store = super::TelegramDeliveryProgressStore::new(temp.path().join("progress"));
        let path = store.progress_path("delivery-corrupt");
        std::fs::create_dir_all(path.parent().expect("progress path should have parent"))?;
        std::fs::write(&path, b"{ not valid json")?;

        let error = store
            .load("delivery-corrupt")
            .expect_err("corrupt Telegram progress should fail closed");
        assert!(
            error
                .to_string()
                .contains("was corrupt and has been quarantined"),
            "unexpected error: {error:#}"
        );
        assert!(!path.exists(), "corrupt progress should be quarantined");
        assert!(
            path.parent()
                .expect("progress path should have parent")
                .read_dir()?
                .any(|entry| {
                    entry
                        .ok()
                        .and_then(|entry| entry.file_name().into_string().ok())
                        .is_some_and(|name| name.starts_with("delivery-corrupt.json.corrupt-"))
                }),
            "quarantined progress sibling was not found"
        );
        Ok(())
    }

    #[tokio::test]
    async fn telegram_text_delivery_splits_long_messages() -> anyhow::Result<()> {
        async fn sink(
            State(posts): State<Arc<Mutex<Vec<Value>>>>,
            Json(payload): Json<Value>,
        ) -> Json<Value> {
            posts
                .lock()
                .expect("telegram sink mutex poisoned")
                .push(payload);
            Json(json!({ "ok": true, "result": { "message_id": 1 } }))
        }

        let posts = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let app = Router::new()
            .route("/bottoken/sendMessage", post(sink))
            .with_state(posts.clone());
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("telegram test sink should stay alive");
        });

        let long = format!("{}{}{}", "a".repeat(4_096), "b".repeat(4_096), "çç");
        send_text_chunks(
            &reqwest::Client::new(),
            &format!("http://{address}"),
            "token",
            123,
            None,
            None,
            &long,
        )
        .await?;

        let posts = posts.lock().expect("telegram sink mutex poisoned");
        assert_eq!(posts.len(), 3);
        for post in posts.iter() {
            let text = post.get("text").and_then(Value::as_str).unwrap_or_default();
            assert!(text.chars().count() <= TELEGRAM_TEXT_LIMIT);
        }
        assert_eq!(posts[2].get("text").and_then(Value::as_str), Some("çç"));
        Ok(())
    }

    #[tokio::test]
    async fn telegram_output_progress_skips_sent_chunks_on_retry() -> anyhow::Result<()> {
        #[derive(Clone, Default)]
        struct StateData {
            posts: Arc<Mutex<Vec<Value>>>,
            failures_remaining: Arc<Mutex<usize>>,
        }

        async fn flaky_sink(
            State(state): State<StateData>,
            Json(payload): Json<Value>,
        ) -> (AxumStatusCode, Json<Value>) {
            state
                .posts
                .lock()
                .expect("telegram progress sink mutex poisoned")
                .push(payload);
            let mut failures = state
                .failures_remaining
                .lock()
                .expect("telegram progress failures mutex poisoned");
            if *failures > 0
                && state
                    .posts
                    .lock()
                    .expect("telegram progress sink mutex poisoned")
                    .len()
                    == 2
            {
                *failures -= 1;
                return (
                    AxumStatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "ok": false,
                        "error_code": 500,
                        "description": "forced failure"
                    })),
                );
            }
            (
                AxumStatusCode::OK,
                Json(json!({ "ok": true, "result": { "message_id": 1 } })),
            )
        }

        let temp = tempdir()?;
        let state = StateData {
            failures_remaining: Arc::new(Mutex::new(1)),
            ..StateData::default()
        };
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let app = Router::new()
            .route("/bottelegram-token/sendMessage", post(flaky_sink))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("telegram progress sink should stay alive");
        });

        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let registry = Arc::new(ConnectorRegistry::resolve(
            ConnectorSettings {
                telegram_connectors: vec![TelegramConnectorConfig {
                    name: "bot".to_string(),
                    bot_token: Some("telegram-token".to_string()),
                    bot_token_env: None,
                    bot_token_secret_ref: None,
                    secret_token: None,
                    secret_token_env: None,
                    secret_token_secret_ref: None,
                    allow_unauthenticated_ingress: true,
                    api_base_url: Some(format!("http://{address}")),
                    ingress_mode: TelegramIngressMode::Webhook,
                    polling_timeout_seconds: 30,
                    ingress_events_per_second: 100,
                    allowed_chat_ids: Vec::new(),
                    fixed_session_id: None,
                    include_self_output: false,
                    additional_reply_targets: Vec::new(),
                    additional_binding_keys: Vec::new(),
                    session_policy: ConnectorSessionPolicy::default(),
                }],
                ..ConnectorSettings::default()
            },
            auth_manager.as_ref(),
        )?);
        let plugin = TelegramOutputPlugin::new(
            registry,
            Arc::new(FileAssetStore::new(temp.path())?),
            Arc::new(NoopObserver),
            temp.path().join("telegram-progress"),
        );
        let reply = ReplyHandle {
            plugin: "telegram".to_string(),
            address: encode_telegram_reply_route(&TelegramReplyRoute {
                connector: "bot".to_string(),
                chat_id: 123,
                message_thread_id: None,
                reply_to_message_id: None,
            }),
        };
        let envelope = ResponseEnvelope {
            conversation: ConversationKey {
                session_id: "session-1".to_string(),
                thread_id: None,
            },
            reply_targets: vec![reply.clone()],
            reply: Some(reply),
            content: format!("{}b", "a".repeat(TELEGRAM_TEXT_LIMIT)),
            parts: Vec::new(),
            artifacts: Vec::new(),
            metadata: json!({ "delivery_id": "delivery-1" }),
        };

        <TelegramOutputPlugin as OutputPlugin>::deliver(&plugin, envelope.clone())
            .await
            .expect_err("first delivery should fail on second chunk");
        <TelegramOutputPlugin as OutputPlugin>::deliver(&plugin, envelope).await?;

        let posts = state
            .posts
            .lock()
            .expect("telegram progress sink mutex poisoned")
            .clone();
        let first_chunk_count = posts
            .iter()
            .filter(|post| {
                post.get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.chars().count() == TELEGRAM_TEXT_LIMIT)
            })
            .count();
        let second_chunk_count = posts
            .iter()
            .filter(|post| post.get("text").and_then(Value::as_str) == Some("b"))
            .count();
        assert_eq!(first_chunk_count, 1, "{posts:#?}");
        assert_eq!(second_chunk_count, 2, "{posts:#?}");

        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn telegram_output_rejects_reply_target_outside_chat_allowlist() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let registry = Arc::new(ConnectorRegistry::resolve(
            ConnectorSettings {
                telegram_connectors: vec![TelegramConnectorConfig {
                    name: "bot".to_string(),
                    bot_token: Some("telegram-token".to_string()),
                    bot_token_env: None,
                    bot_token_secret_ref: None,
                    secret_token: None,
                    secret_token_env: None,
                    secret_token_secret_ref: None,
                    allow_unauthenticated_ingress: true,
                    api_base_url: Some("http://127.0.0.1:1".to_string()),
                    ingress_mode: TelegramIngressMode::Webhook,
                    polling_timeout_seconds: 30,
                    ingress_events_per_second: 100,
                    allowed_chat_ids: vec![123],
                    fixed_session_id: None,
                    include_self_output: false,
                    additional_reply_targets: Vec::new(),
                    additional_binding_keys: Vec::new(),
                    session_policy: ConnectorSessionPolicy::default(),
                }],
                ..ConnectorSettings::default()
            },
            auth_manager.as_ref(),
        )?);
        let plugin = TelegramOutputPlugin::new(
            registry,
            Arc::new(FileAssetStore::new(temp.path())?),
            Arc::new(NoopObserver),
            temp.path().join("telegram-progress"),
        );
        let reply = ReplyHandle {
            plugin: "telegram".to_string(),
            address: encode_telegram_reply_route(&TelegramReplyRoute {
                connector: "bot".to_string(),
                chat_id: 999,
                message_thread_id: None,
                reply_to_message_id: None,
            }),
        };
        let envelope = ResponseEnvelope {
            conversation: ConversationKey {
                session_id: "session-1".to_string(),
                thread_id: None,
            },
            reply_targets: vec![reply.clone()],
            reply: Some(reply),
            content: "blocked".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            metadata: json!({ "delivery_id": "delivery-1" }),
        };

        let error = <TelegramOutputPlugin as OutputPlugin>::deliver(&plugin, envelope)
            .await
            .expect_err("chat outside allowlist should be rejected before network I/O");
        assert!(
            error
                .to_string()
                .contains("outside connector chat allowlist"),
            "unexpected error: {error:#}"
        );
        Ok(())
    }
}
