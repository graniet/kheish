use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use kheish_output::{OutputManifest, OutputPlugin, ResponseEnvelope};
use kheish_runtime::RuntimeObserver;
use kheish_session::write_json_pretty_atomically;
use kheish_types::ContentPart;
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::task;

use crate::assets::{FileAssetStore, StoredAssetRecord};
use crate::connectors::config::{ConnectorRegistry, decode_slack_reply_route};
use crate::delivery::{DeliveryTransport, retry_after_delivery_error, terminal_delivery_error};
use crate::state_files::read_json_or_quarantine;

use super::{OutputAuditSpan, output_http_client, stable_target_digest, summarize_delivery_target};

pub struct SlackOutputPlugin {
    connectors: Arc<ConnectorRegistry>,
    assets: Arc<FileAssetStore>,
    client: reqwest::Client,
    observer: Arc<dyn RuntimeObserver>,
    progress_store: SlackDeliveryProgressStore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ThreadAnchorSelection {
    text: String,
    consumed_text_part_index: Option<usize>,
}

impl SlackOutputPlugin {
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
            progress_store: SlackDeliveryProgressStore::new(progress_root),
        }
    }
}

#[derive(Clone, Debug)]
struct SlackDeliveryProgressStore {
    root: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SlackDeliveryProgress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thread_ts: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    messages: BTreeMap<String, SlackMessageProgress>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    uploads: BTreeMap<String, SlackUploadProgress>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SlackMessageProgress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ts: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_msg_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SlackUploadProgress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_id: Option<String>,
    #[serde(default)]
    uploaded: bool,
    #[serde(default)]
    completion_requested: bool,
    #[serde(default)]
    completed: bool,
}

impl SlackDeliveryProgressStore {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn load(&self, delivery_id: &str) -> Result<SlackDeliveryProgress> {
        let path = self.progress_path(delivery_id);
        let progress_file_existed = path.try_exists().with_context(|| {
            format!(
                "failed to inspect slack delivery progress {}",
                path.display()
            )
        })?;
        if !progress_file_existed {
            return Ok(SlackDeliveryProgress::default());
        }
        read_json_or_quarantine(&path, "slack delivery progress")?.ok_or_else(|| {
            terminal_delivery_error(format!(
                "slack delivery progress {} was corrupt and has been quarantined; inspect before replaying",
                path.display()
            ))
        })
    }

    fn save(&self, delivery_id: &str, progress: &SlackDeliveryProgress) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        write_json_pretty_atomically(&self.progress_path(delivery_id), progress)
    }

    fn progress_path(&self, delivery_id: &str) -> PathBuf {
        self.root
            .join(format!("{}.json", safe_progress_file_id(delivery_id)))
    }
}

#[derive(Debug, Deserialize)]
struct SlackMessageResponse {
    ok: bool,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackUploadUrlResponse {
    ok: bool,
    #[serde(default)]
    upload_url: Option<String>,
    #[serde(default)]
    file_id: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackOkResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

#[async_trait]
impl OutputPlugin for SlackOutputPlugin {
    fn manifest(&self) -> OutputManifest {
        OutputManifest {
            name: "slack".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Slack bot output".to_string(),
        }
    }

    async fn deliver(&self, response: ResponseEnvelope) -> Result<()> {
        let reply = response
            .reply
            .as_ref()
            .ok_or_else(|| anyhow!("slack output requires one reply target"))?;
        let route = decode_slack_reply_route(&reply.address)?;
        let connector = self
            .connectors
            .slack(&route.connector)
            .ok_or_else(|| anyhow!("unknown slack connector {}", route.connector))?;
        if !connector.is_channel_allowed(&route.channel_id)
            || !connector.is_enterprise_allowed(route.enterprise_id.as_deref())
            || !connector.is_team_allowed(route.team_id.as_deref())
        {
            return Err(terminal_delivery_error(
                "slack reply target is outside connector allowlist",
            ));
        }
        let bot_token = connector
            .bot_token_for_team(route.team_id.as_deref())
            .ok_or_else(|| {
                anyhow!(
                    "slack connector {} has no bot token configured",
                    connector.name
                )
            })?;

        let audit = OutputAuditSpan::start(
            self.observer.clone(),
            slack_audit_target(&connector.name, &route.channel_id),
            &json!({
                "channel_id": &route.channel_id,
                "thread_ts": &route.thread_ts,
                "conversation": &response.conversation,
                "content": &response.content,
                "parts": &response.parts,
                "artifacts": &response.artifacts,
                "metadata": &response.metadata,
            }),
        )?;
        let delivery = async {
            let progress_id = slack_delivery_progress_id(&response)?;
            let mut progress = self.progress_store.load(&progress_id)?;
            let requested_thread_ts = route.thread_ts.clone();
            let mut thread_ts = requested_thread_ts
                .clone()
                .or_else(|| progress.thread_ts.clone());
            if progress.thread_ts.is_none() {
                progress.thread_ts = thread_ts.clone();
            }
            let api_base_url = connector.api_base_url.trim_end_matches('/').to_string();
            let anchor = select_thread_anchor(
                &response.parts,
                &response.content,
                requested_thread_ts.as_deref(),
            );
            if let Some(anchor) = &anchor {
                deliver_text_step(
                    &self.progress_store,
                    &progress_id,
                    &mut progress,
                    &self.client,
                    &api_base_url,
                    bot_token,
                    &route.channel_id,
                    &mut thread_ts,
                    "anchor",
                    &anchor.text,
                )
                .await
                .with_context(|| {
                    format!("failed to deliver slack output via {}", connector.name)
                })?;
            }
            if response.parts.is_empty() {
                if !response.content.trim().is_empty() {
                    deliver_text_step(
                        &self.progress_store,
                        &progress_id,
                        &mut progress,
                        &self.client,
                        &api_base_url,
                        bot_token,
                        &route.channel_id,
                        &mut thread_ts,
                        "content",
                        &response.content,
                    )
                    .await
                    .with_context(|| {
                        format!("failed to deliver slack output via {}", connector.name)
                    })?;
                }
                return Ok::<_, anyhow::Error>(());
            }

            for (index, part) in response.parts.iter().enumerate() {
                match part {
                    ContentPart::Text { .. }
                        if anchor
                            .as_ref()
                            .and_then(|selection| selection.consumed_text_part_index)
                            == Some(index) => {}
                    ContentPart::Text { text } if !text.trim().is_empty() => {
                        deliver_text_step(
                            &self.progress_store,
                            &progress_id,
                            &mut progress,
                            &self.client,
                            &api_base_url,
                            bot_token,
                            &route.channel_id,
                            &mut thread_ts,
                            &format!("text:{index}"),
                            text,
                        )
                        .await
                        .with_context(|| {
                            format!("failed to deliver slack output via {}", connector.name)
                        })?;
                    }
                    ContentPart::Text { .. } => {}
                    ContentPart::Attachment { attachment } => {
                        let (record, bytes) =
                            load_attachment_bytes(self.assets.clone(), attachment.id.clone())
                                .await?;
                        deliver_upload_step(
                            &self.progress_store,
                            &progress_id,
                            &mut progress,
                            &self.client,
                            &api_base_url,
                            bot_token,
                            &route.channel_id,
                            thread_ts.as_deref(),
                            &format!("attachment:{index}:{}", attachment.id),
                            &record.file_name,
                            &record.media_type,
                            bytes,
                        )
                        .await
                        .with_context(|| {
                            format!("failed to deliver slack output via {}", connector.name)
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
impl DeliveryTransport for SlackOutputPlugin {
    async fn deliver(&self, response: ResponseEnvelope) -> Result<()> {
        <Self as OutputPlugin>::deliver(self, response).await
    }
}

fn slack_audit_target(connector_name: &str, channel_id: &str) -> String {
    format!(
        "slack:{}:channel_sha256:{}",
        summarize_delivery_target(connector_name),
        stable_target_digest(channel_id)
    )
}

async fn post_text_message(
    client: &reqwest::Client,
    api_base_url: &str,
    bot_token: &str,
    channel_id: &str,
    thread_ts: Option<&str>,
    client_msg_id: Option<&str>,
    text: &str,
) -> Result<Option<String>> {
    let mut payload = serde_json::Map::new();
    payload.insert("channel".to_string(), json!(channel_id));
    payload.insert("text".to_string(), json!(text));
    if let Some(thread_ts) = thread_ts {
        payload.insert("thread_ts".to_string(), json!(thread_ts));
    }
    if let Some(client_msg_id) = client_msg_id {
        payload.insert("client_msg_id".to_string(), json!(client_msg_id));
    }
    let response = client
        .post(format!("{api_base_url}/chat.postMessage"))
        .bearer_auth(bot_token)
        .json(&payload)
        .send()
        .await?;
    let body = slack_json_response::<SlackMessageResponse>(
        response,
        "chat.postMessage",
        "failed to decode slack message response",
    )
    .await?;
    if !body.ok {
        return Err(slack_api_error(
            body.error.as_deref(),
            "slack output failed while posting the text message",
        ));
    }
    Ok(body.ts)
}

async fn deliver_text_step(
    store: &SlackDeliveryProgressStore,
    progress_id: &str,
    progress: &mut SlackDeliveryProgress,
    client: &reqwest::Client,
    api_base_url: &str,
    bot_token: &str,
    channel_id: &str,
    thread_ts: &mut Option<String>,
    step_key: &str,
    text: &str,
) -> Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    if let Some(existing) = progress.messages.get(step_key) {
        if thread_ts.is_none() {
            *thread_ts = existing.ts.clone();
            progress.thread_ts = thread_ts.clone();
            store.save(progress_id, progress)?;
        }
        return Ok(());
    }
    let client_msg_id = stable_slack_client_msg_id(progress_id, step_key);
    let posted_ts = post_text_message(
        client,
        api_base_url,
        bot_token,
        channel_id,
        thread_ts.as_deref(),
        Some(&client_msg_id),
        text,
    )
    .await?;
    if thread_ts.is_none() {
        *thread_ts = posted_ts.clone();
    }
    if progress.thread_ts.is_none() {
        progress.thread_ts = thread_ts.clone();
    }
    progress.messages.insert(
        step_key.to_string(),
        SlackMessageProgress {
            ts: posted_ts,
            client_msg_id: Some(client_msg_id),
        },
    );
    store.save(progress_id, progress)
}

async fn deliver_upload_step(
    store: &SlackDeliveryProgressStore,
    progress_id: &str,
    progress: &mut SlackDeliveryProgress,
    client: &reqwest::Client,
    api_base_url: &str,
    bot_token: &str,
    channel_id: &str,
    thread_ts: Option<&str>,
    step_key: &str,
    file_name: &str,
    media_type: &str,
    bytes: Vec<u8>,
) -> Result<()> {
    let mut step = progress.uploads.get(step_key).cloned().unwrap_or_default();
    if step.completed {
        return Ok(());
    }
    if step.completion_requested {
        return Err(terminal_delivery_error(format!(
            "slack upload completion for {step_key} is ambiguous after a previous files.completeUploadExternal request; inspect Slack before replaying"
        )));
    }
    if !step.uploaded {
        let descriptor =
            request_upload_descriptor(client, api_base_url, bot_token, file_name, bytes.len())
                .await?;
        step.file_id = Some(descriptor.file_id.clone());
        progress.uploads.insert(step_key.to_string(), step.clone());
        store.save(progress_id, progress)?;

        upload_file_bytes(client, &descriptor.upload_url, media_type, bytes).await?;
        step.uploaded = true;
        progress.uploads.insert(step_key.to_string(), step.clone());
        store.save(progress_id, progress)?;
    }
    let file_id = step
        .file_id
        .clone()
        .ok_or_else(|| anyhow!("slack upload progress missing file_id"))?;
    step.completion_requested = true;
    progress.uploads.insert(step_key.to_string(), step.clone());
    store.save(progress_id, progress)?;
    match complete_upload(
        client,
        api_base_url,
        bot_token,
        channel_id,
        thread_ts,
        &file_id,
        file_name,
    )
    .await
    {
        SlackCompleteUploadOutcome::Completed => {}
        SlackCompleteUploadOutcome::RetryAfter(error) => {
            step.completion_requested = false;
            progress.uploads.insert(step_key.to_string(), step);
            store.save(progress_id, progress)?;
            return Err(error);
        }
        SlackCompleteUploadOutcome::Failed(error) => return Err(error),
    }
    step.completed = true;
    progress.uploads.insert(step_key.to_string(), step);
    store.save(progress_id, progress)
}

#[derive(Clone, Debug)]
struct SlackUploadDescriptor {
    upload_url: String,
    file_id: String,
}

async fn request_upload_descriptor(
    client: &reqwest::Client,
    api_base_url: &str,
    bot_token: &str,
    file_name: &str,
    byte_len: usize,
) -> Result<SlackUploadDescriptor> {
    let descriptor = client
        .post(format!("{api_base_url}/files.getUploadURLExternal"))
        .bearer_auth(bot_token)
        .form(&[
            ("filename", file_name.to_string()),
            ("length", byte_len.to_string()),
        ])
        .send()
        .await?;
    let descriptor = slack_json_response::<SlackUploadUrlResponse>(
        descriptor,
        "files.getUploadURLExternal",
        "failed to decode slack upload-url response",
    )
    .await?;
    if !descriptor.ok {
        return Err(slack_api_error(
            descriptor.error.as_deref(),
            "slack output failed while requesting an upload URL",
        ));
    }
    let upload_url = descriptor
        .upload_url
        .ok_or_else(|| anyhow!("slack upload-url response missing upload_url"))?;
    let file_id = descriptor
        .file_id
        .ok_or_else(|| anyhow!("slack upload-url response missing file_id"))?;
    Ok(SlackUploadDescriptor {
        upload_url,
        file_id,
    })
}

async fn upload_file_bytes(
    client: &reqwest::Client,
    upload_url: &str,
    media_type: &str,
    bytes: Vec<u8>,
) -> Result<()> {
    client
        .post(upload_url)
        .header(reqwest::header::CONTENT_TYPE, media_type)
        .body(bytes)
        .send()
        .await
        .context("slack file upload failed")
        .and_then(|response| slack_status_response(response, "file upload"))
        .context("slack file upload failed")?;
    Ok(())
}

async fn complete_upload(
    client: &reqwest::Client,
    api_base_url: &str,
    bot_token: &str,
    channel_id: &str,
    thread_ts: Option<&str>,
    file_id: &str,
    file_name: &str,
) -> SlackCompleteUploadOutcome {
    let mut form = vec![
        (
            "files",
            json!([{ "id": file_id, "title": file_name }]).to_string(),
        ),
        ("channel_id", channel_id.to_string()),
    ];
    if let Some(thread_ts) = thread_ts {
        form.push(("thread_ts", thread_ts.to_string()));
    }
    let response = match client
        .post(format!("{api_base_url}/files.completeUploadExternal"))
        .bearer_auth(bot_token)
        .form(&form)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return SlackCompleteUploadOutcome::Failed(slack_ambiguous_completion_error(format!(
                "transport error while completing upload: {error}"
            )));
        }
    };
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        return SlackCompleteUploadOutcome::RetryAfter(slack_retry_after_error(
            response.headers(),
            "files.completeUploadExternal",
        ));
    }
    let status = response.status();
    if status.is_server_error() {
        return SlackCompleteUploadOutcome::Failed(slack_ambiguous_completion_error(format!(
            "slack files.completeUploadExternal returned HTTP {status}"
        )));
    }
    let response = match response.error_for_status() {
        Ok(response) => response,
        Err(error) => {
            return SlackCompleteUploadOutcome::Failed(terminal_delivery_error(format!(
                "slack upload completion failed permanently: {error}"
            )));
        }
    };
    let body = match response.json::<SlackOkResponse>().await {
        Ok(body) => body,
        Err(error) => {
            return SlackCompleteUploadOutcome::Failed(slack_ambiguous_completion_error(format!(
                "failed to decode slack upload completion response: {error}"
            )));
        }
    };
    if !body.ok {
        return slack_complete_upload_error(body.error.as_deref());
    }
    SlackCompleteUploadOutcome::Completed
}

enum SlackCompleteUploadOutcome {
    Completed,
    RetryAfter(anyhow::Error),
    Failed(anyhow::Error),
}

fn slack_complete_upload_error(error: Option<&str>) -> SlackCompleteUploadOutcome {
    let error = error.map(str::trim).filter(|value| !value.is_empty());
    if error == Some("ratelimited") {
        return SlackCompleteUploadOutcome::RetryAfter(retry_after_delivery_error(
            1_000,
            "slack files.completeUploadExternal rate limited; retry after 1000ms",
        ));
    }
    if matches!(error, Some("fatal_error" | "internal_error")) {
        return SlackCompleteUploadOutcome::Failed(slack_ambiguous_completion_error(format!(
            "slack files.completeUploadExternal returned {}",
            error.unwrap_or("an ambiguous error")
        )));
    }
    let detail = error
        .map(|error| format!("slack upload completion failed permanently: {error}"))
        .unwrap_or_else(|| "slack upload completion failed permanently".to_string());
    SlackCompleteUploadOutcome::Failed(terminal_delivery_error(detail))
}

fn slack_ambiguous_completion_error(detail: impl AsRef<str>) -> anyhow::Error {
    terminal_delivery_error(format!(
        "{}; Slack documents this method as single-use, so Kheish will not call it again automatically",
        detail.as_ref()
    ))
}

async fn slack_json_response<T>(
    response: reqwest::Response,
    method: &str,
    decode_context: &'static str,
) -> Result<T>
where
    T: DeserializeOwned,
{
    let status = response.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(slack_retry_after_error(response.headers(), method));
    }
    let response = response.error_for_status()?;
    response.json::<T>().await.context(decode_context)
}

fn slack_retry_after_error(headers: &HeaderMap, method: &str) -> anyhow::Error {
    let retry_after_ms = parse_slack_retry_after_ms(headers).unwrap_or(1_000);
    retry_after_delivery_error(
        retry_after_ms,
        format!("slack {method} rate limited; retry after {retry_after_ms}ms"),
    )
}

fn slack_status_response(response: reqwest::Response, method: &str) -> Result<()> {
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        return Err(slack_retry_after_error(response.headers(), method));
    }
    response.error_for_status()?;
    Ok(())
}

fn parse_slack_retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| seconds.saturating_mul(1_000).max(1))
}

fn slack_api_error(error: Option<&str>, fallback: &'static str) -> anyhow::Error {
    let Some(error) = error.map(str::trim).filter(|value| !value.is_empty()) else {
        return anyhow!(fallback);
    };
    if error == "ratelimited" {
        return retry_after_delivery_error(1_000, "slack output rate limited; retry after 1000ms");
    }
    if terminal_slack_error(error) {
        return terminal_delivery_error(format!("slack output failed permanently: {error}"));
    }
    anyhow!("slack output failed: {error}")
}

fn terminal_slack_error(error: &str) -> bool {
    matches!(
        error,
        "account_inactive"
            | "channel_not_found"
            | "ekm_access_denied"
            | "invalid_auth"
            | "is_archived"
            | "missing_scope"
            | "not_allowed_token_type"
            | "not_authed"
            | "not_in_channel"
            | "token_revoked"
    )
}

fn slack_delivery_progress_id(response: &ResponseEnvelope) -> Result<String> {
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

fn safe_progress_file_id(value: &str) -> String {
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

fn stable_slack_client_msg_id(progress_id: &str, step_key: &str) -> String {
    let digest = kheish_codec::digest_text(&format!("slack-delivery:{progress_id}:{step_key}"));
    format!(
        "{}-{}-4{}-8{}-{}",
        &digest[0..8],
        &digest[8..12],
        &digest[13..16],
        &digest[17..20],
        &digest[20..32],
    )
}

async fn load_attachment_bytes(
    assets: Arc<FileAssetStore>,
    asset_id: String,
) -> Result<(StoredAssetRecord, Vec<u8>)> {
    task::spawn_blocking(move || assets.read_raw(&asset_id))
        .await
        .map_err(|error| anyhow!("slack asset load task failed: {error}"))?
}

fn select_thread_anchor(
    parts: &[ContentPart],
    content: &str,
    thread_ts: Option<&str>,
) -> Option<ThreadAnchorSelection> {
    if thread_ts.is_some() {
        return None;
    }
    let first_visible = parts.iter().find(|part| match part {
        ContentPart::Text { text } => !text.trim().is_empty(),
        ContentPart::Attachment { .. } => true,
    })?;
    if !matches!(first_visible, ContentPart::Attachment { .. }) {
        return None;
    }
    if let Some((index, text)) = parts
        .iter()
        .enumerate()
        .find_map(|(index, part)| match part {
            ContentPart::Text { text } if !text.trim().is_empty() => Some((index, text.trim())),
            _ => None,
        })
    {
        return Some(ThreadAnchorSelection {
            text: text.to_string(),
            consumed_text_part_index: Some(index),
        });
    }
    let fallback = content.trim();
    (!fallback.is_empty()).then(|| ThreadAnchorSelection {
        text: fallback.to_string(),
        consumed_text_part_index: None,
    })
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::sync::Arc;

    use axum::body::Bytes;
    use axum::extract::{Path as AxumPath, State};
    use axum::http::StatusCode as AxumStatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::{Json, Router};
    use kheish_auth::AuthManager;
    use kheish_output::{OutputPlugin, ResponseEnvelope};
    use kheish_runtime::NoopObserver;
    use kheish_types::{AttachmentRef, ContentPart, ConversationKey, ReplyHandle};
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use serde_json::{Value, json};
    use tempfile::tempdir;
    use tokio::net::TcpListener;

    use super::{
        SlackOutputPlugin, ThreadAnchorSelection, parse_slack_retry_after_ms, select_thread_anchor,
        slack_api_error, slack_audit_target, stable_slack_client_msg_id, terminal_slack_error,
    };
    use crate::assets::FileAssetStore;
    use crate::connectors::config::{
        ConnectorRegistry, ConnectorSessionPolicy, ConnectorSettings, SlackConnectorConfig,
        SlackReplyRoute, encode_slack_reply_route,
    };

    #[derive(Clone, Default)]
    struct FakeSlackOutputState {
        posts: Arc<Mutex<Vec<Value>>>,
        complete_failures_remaining: Arc<Mutex<usize>>,
    }

    #[test]
    fn slack_audit_target_redacts_channel_id() {
        let target = slack_audit_target("workspace", "C_OPS_SECRETISH");
        let repeated = slack_audit_target("workspace", "C_OPS_SECRETISH");
        assert_eq!(target, repeated);
        assert!(target.starts_with("slack:workspace:channel_sha256:"));
        assert!(!target.contains("C_OPS_SECRETISH"));
    }

    #[test]
    fn attachment_only_outputs_use_content_fallback_as_thread_anchor() {
        let parts = vec![ContentPart::Attachment {
            attachment: AttachmentRef {
                id: "asset-1".to_string(),
                media_type: "image/png".to_string(),
                uri: "asset://raw/asset-1.png".to_string(),
                file_name: Some("asset-1.png".to_string()),
                sha256: None,
                byte_length: None,
                text_uri: None,
                text_sha256: None,
                text_byte_length: None,
                preview_image_uri: None,
                preview_image_media_type: None,
                preview_image_sha256: None,
                preview_image_byte_length: None,
            },
        }];

        let anchor = select_thread_anchor(&parts, "Attached asset: asset-1.png (image/png)", None);
        assert_eq!(
            anchor,
            Some(ThreadAnchorSelection {
                text: "Attached asset: asset-1.png (image/png)".to_string(),
                consumed_text_part_index: None,
            })
        );
    }

    #[test]
    fn attachment_first_outputs_reuse_the_first_text_part_as_thread_anchor() {
        let parts = vec![
            ContentPart::Attachment {
                attachment: AttachmentRef {
                    id: "asset-1".to_string(),
                    media_type: "image/png".to_string(),
                    uri: "asset://raw/asset-1.png".to_string(),
                    file_name: Some("asset-1.png".to_string()),
                    sha256: None,
                    byte_length: None,
                    text_uri: None,
                    text_sha256: None,
                    text_byte_length: None,
                    preview_image_uri: None,
                    preview_image_media_type: None,
                    preview_image_sha256: None,
                    preview_image_byte_length: None,
                },
            },
            ContentPart::Text {
                text: "Here is the generated image.".to_string(),
            },
        ];

        let anchor = select_thread_anchor(&parts, "fallback", None);
        assert_eq!(
            anchor,
            Some(ThreadAnchorSelection {
                text: "Here is the generated image.".to_string(),
                consumed_text_part_index: Some(1),
            })
        );
    }

    #[test]
    fn slack_retry_after_header_is_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("3"));
        assert_eq!(parse_slack_retry_after_ms(&headers), Some(3_000));
    }

    #[test]
    fn slack_permanent_errors_are_terminal() {
        assert!(terminal_slack_error("invalid_auth"));
        assert!(terminal_slack_error("channel_not_found"));
        assert!(!terminal_slack_error("internal_error"));
    }

    #[test]
    fn slack_json_ratelimited_errors_are_retry_after() {
        let error = slack_api_error(Some("ratelimited"), "fallback");
        assert!(
            error.to_string().contains("retry after 1000ms"),
            "unexpected Slack rate-limit error: {error:#}"
        );
    }

    #[test]
    fn slack_client_msg_ids_are_stable_uuid_shaped() {
        let first = stable_slack_client_msg_id("delivery-1", "anchor");
        let second = stable_slack_client_msg_id("delivery-1", "anchor");
        assert_eq!(first, second);
        assert_eq!(first.len(), 36);
        assert_eq!(first.as_bytes()[14], b'4');
        assert_eq!(first.as_bytes()[19], b'8');
    }

    #[test]
    fn missing_slack_delivery_progress_loads_default() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let store = super::SlackDeliveryProgressStore::new(temp.path().join("progress"));

        assert_eq!(
            store.load("delivery-missing")?,
            super::SlackDeliveryProgress::default()
        );

        Ok(())
    }

    #[test]
    fn corrupt_slack_delivery_progress_fails_closed() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let store = super::SlackDeliveryProgressStore::new(temp.path().join("progress"));
        let path = store.progress_path("delivery-corrupt");
        std::fs::create_dir_all(path.parent().expect("progress path should have parent"))?;
        std::fs::write(&path, b"{ not valid json")?;

        let error = store
            .load("delivery-corrupt")
            .expect_err("corrupt Slack progress should fail closed");
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
    async fn slack_output_progress_skips_completed_steps_on_rate_limit_retry() -> anyhow::Result<()>
    {
        async fn chat_post_message(
            State(state): State<FakeSlackOutputState>,
            Json(payload): Json<Value>,
        ) -> Json<Value> {
            state
                .posts
                .lock()
                .push(json!({ "kind": "chat_postMessage", "payload": payload }));
            Json(json!({ "ok": true, "ts": "1710000000.000777" }))
        }

        async fn upload_bytes(
            State(state): State<FakeSlackOutputState>,
            AxumPath(file_id): AxumPath<String>,
            body: Bytes,
        ) -> Json<Value> {
            state.posts.lock().push(json!({
                "kind": "uploaded_bytes",
                "file_id": file_id,
                "byte_length": body.len(),
            }));
            Json(json!({ "ok": true }))
        }

        async fn complete_upload(State(state): State<FakeSlackOutputState>) -> Response {
            let mut failures = state.complete_failures_remaining.lock();
            if *failures > 0 {
                *failures -= 1;
                drop(failures);
                state
                    .posts
                    .lock()
                    .push(json!({ "kind": "complete_upload_failed" }));
                return (
                    AxumStatusCode::TOO_MANY_REQUESTS,
                    [("Retry-After", "1")],
                    Json(json!({ "ok": false, "error": "ratelimited" })),
                )
                    .into_response();
            }
            drop(failures);
            state
                .posts
                .lock()
                .push(json!({ "kind": "complete_upload" }));
            (AxumStatusCode::OK, Json(json!({ "ok": true }))).into_response()
        }

        let temp = tempdir()?;
        let state = FakeSlackOutputState {
            complete_failures_remaining: Arc::new(Mutex::new(1)),
            ..FakeSlackOutputState::default()
        };
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let upload_url = format!("http://{address}/upload/F1");
        let router = Router::new()
            .route("/chat.postMessage", post(chat_post_message))
            .route(
                "/files.getUploadURLExternal",
                post({
                    let upload_url = upload_url.clone();
                    move |State(state): State<FakeSlackOutputState>| {
                        let upload_url = upload_url.clone();
                        async move {
                            state
                                .posts
                                .lock()
                                .push(json!({ "kind": "upload_descriptor" }));
                            Json(json!({
                                "ok": true,
                                "upload_url": upload_url,
                                "file_id": "F1"
                            }))
                        }
                    }
                }),
            )
            .route("/files.completeUploadExternal", post(complete_upload))
            .route("/upload/{file_id}", post(upload_bytes))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let asset = assets.import_bytes("demo.txt", Some("text/plain"), b"hello")?;
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let registry = Arc::new(ConnectorRegistry::resolve(
            ConnectorSettings {
                slack_connectors: vec![SlackConnectorConfig {
                    name: "workspace".to_string(),
                    bot_token: Some("xoxb-test".to_string()),
                    bot_token_env: None,
                    bot_token_secret_ref: None,
                    signing_secret: None,
                    signing_secret_env: None,
                    signing_secret_secret_ref: None,
                    allow_unauthenticated_ingress: true,
                    api_base_url: Some(format!("http://{address}")),
                    fixed_session_id: None,
                    include_self_output: false,
                    additional_reply_targets: Vec::new(),
                    additional_binding_keys: Vec::new(),
                    session_policy: ConnectorSessionPolicy::default(),
                    ingress_events_per_second:
                        crate::connectors::slack_default_ingress_events_per_second(),
                    allowed_api_app_ids: Vec::new(),
                    allowed_enterprise_ids: Vec::new(),
                    allowed_team_ids: Vec::new(),
                    allowed_channel_ids: Vec::new(),
                    allowed_file_hosts: Vec::new(),
                    team_bot_tokens: Vec::new(),
                }],
                ..ConnectorSettings::default()
            },
            auth_manager.as_ref(),
        )?);
        let plugin = SlackOutputPlugin::new(
            registry,
            assets,
            Arc::new(NoopObserver),
            temp.path().join("progress"),
        );
        let reply = ReplyHandle {
            plugin: "slack".to_string(),
            address: encode_slack_reply_route(&SlackReplyRoute {
                connector: "workspace".to_string(),
                enterprise_id: None,
                team_id: None,
                channel_id: "C123".to_string(),
                thread_ts: None,
            }),
        };
        let envelope = ResponseEnvelope {
            conversation: ConversationKey {
                session_id: "session-1".to_string(),
                thread_id: None,
            },
            reply_targets: vec![reply.clone()],
            reply: Some(reply),
            content: "Attached asset: demo.txt (text/plain)".to_string(),
            parts: vec![
                ContentPart::Attachment {
                    attachment: asset.attachment_ref(),
                },
                ContentPart::Text {
                    text: "Here is the file.".to_string(),
                },
            ],
            artifacts: Vec::new(),
            metadata: json!({ "delivery_id": "delivery-1" }),
        };

        <SlackOutputPlugin as OutputPlugin>::deliver(&plugin, envelope.clone())
            .await
            .expect_err("first delivery should fail after upload bytes");
        <SlackOutputPlugin as OutputPlugin>::deliver(&plugin, envelope).await?;

        let posts = state.posts.lock().clone();
        let count_kind = |kind: &str| {
            posts
                .iter()
                .filter(|post| post.get("kind").and_then(Value::as_str) == Some(kind))
                .count()
        };
        assert_eq!(count_kind("chat_postMessage"), 1, "{posts:#?}");
        assert_eq!(count_kind("upload_descriptor"), 1, "{posts:#?}");
        assert_eq!(count_kind("uploaded_bytes"), 1, "{posts:#?}");
        assert_eq!(count_kind("complete_upload_failed"), 1, "{posts:#?}");
        assert_eq!(count_kind("complete_upload"), 1, "{posts:#?}");

        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn slack_output_uses_team_bot_tokens_for_enterprise_grid_routes() -> anyhow::Result<()> {
        async fn chat_post_message(
            State(state): State<FakeSlackOutputState>,
            headers: HeaderMap,
            Json(payload): Json<Value>,
        ) -> Json<Value> {
            let authorization = headers
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            state.posts.lock().push(json!({
                "kind": "chat_postMessage",
                "authorization": authorization,
                "payload": payload,
            }));
            Json(json!({ "ok": true, "ts": "1710000000.000777" }))
        }

        let temp = tempdir()?;
        let state = FakeSlackOutputState::default();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = Router::new()
            .route("/chat.postMessage", post(chat_post_message))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let registry = Arc::new(ConnectorRegistry::resolve(
            ConnectorSettings {
                slack_connectors: vec![SlackConnectorConfig {
                    name: "enterprise".to_string(),
                    bot_token: Some("xoxb-fallback".to_string()),
                    bot_token_env: None,
                    bot_token_secret_ref: None,
                    signing_secret: None,
                    signing_secret_env: None,
                    signing_secret_secret_ref: None,
                    allow_unauthenticated_ingress: true,
                    api_base_url: Some(format!("http://{address}")),
                    fixed_session_id: None,
                    include_self_output: false,
                    additional_reply_targets: Vec::new(),
                    additional_binding_keys: Vec::new(),
                    session_policy: ConnectorSessionPolicy::default(),
                    ingress_events_per_second:
                        crate::connectors::slack_default_ingress_events_per_second(),
                    allowed_api_app_ids: Vec::new(),
                    allowed_enterprise_ids: vec!["E_GRID".to_string()],
                    allowed_team_ids: vec!["T_ONE".to_string(), "T_TWO".to_string()],
                    allowed_channel_ids: vec!["C_ONE".to_string(), "C_TWO".to_string()],
                    allowed_file_hosts: Vec::new(),
                    team_bot_tokens: vec![
                        crate::connectors::SlackTeamBotTokenConfig {
                            team_id: "T_ONE".to_string(),
                            bot_token: Some("xoxb-team-one".to_string()),
                            bot_token_env: None,
                            bot_token_secret_ref: None,
                        },
                        crate::connectors::SlackTeamBotTokenConfig {
                            team_id: "T_TWO".to_string(),
                            bot_token: Some("xoxb-team-two".to_string()),
                            bot_token_env: None,
                            bot_token_secret_ref: None,
                        },
                    ],
                }],
                ..ConnectorSettings::default()
            },
            auth_manager.as_ref(),
        )?);
        let plugin = SlackOutputPlugin::new(
            registry,
            Arc::new(FileAssetStore::new(temp.path())?),
            Arc::new(NoopObserver),
            temp.path().join("progress"),
        );

        for (delivery_id, team_id, channel_id) in [
            ("delivery-team-one", "T_ONE", "C_ONE"),
            ("delivery-team-two", "T_TWO", "C_TWO"),
        ] {
            let reply = ReplyHandle {
                plugin: "slack".to_string(),
                address: encode_slack_reply_route(&SlackReplyRoute {
                    connector: "enterprise".to_string(),
                    enterprise_id: Some("E_GRID".to_string()),
                    team_id: Some(team_id.to_string()),
                    channel_id: channel_id.to_string(),
                    thread_ts: Some("1710000000.000100".to_string()),
                }),
            };
            <SlackOutputPlugin as OutputPlugin>::deliver(
                &plugin,
                ResponseEnvelope {
                    conversation: ConversationKey {
                        session_id: "session-1".to_string(),
                        thread_id: None,
                    },
                    reply_targets: vec![reply.clone()],
                    reply: Some(reply),
                    content: format!("hello {team_id}"),
                    parts: Vec::new(),
                    artifacts: Vec::new(),
                    metadata: json!({ "delivery_id": delivery_id }),
                },
            )
            .await?;
        }

        let posts = state.posts.lock().clone();
        let authorizations = posts
            .iter()
            .filter_map(|post| post.get("authorization").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(
            authorizations,
            vec!["Bearer xoxb-team-one", "Bearer xoxb-team-two"],
            "{posts:#?}"
        );

        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn slack_output_does_not_retry_ambiguous_upload_completion() -> anyhow::Result<()> {
        async fn chat_post_message(
            State(state): State<FakeSlackOutputState>,
            Json(payload): Json<Value>,
        ) -> Json<Value> {
            state
                .posts
                .lock()
                .push(json!({ "kind": "chat_postMessage", "payload": payload }));
            Json(json!({ "ok": true, "ts": "1710000000.000888" }))
        }

        async fn upload_bytes(
            State(state): State<FakeSlackOutputState>,
            AxumPath(file_id): AxumPath<String>,
            body: Bytes,
        ) -> Json<Value> {
            state.posts.lock().push(json!({
                "kind": "uploaded_bytes",
                "file_id": file_id,
                "byte_length": body.len(),
            }));
            Json(json!({ "ok": true }))
        }

        async fn complete_upload(State(state): State<FakeSlackOutputState>) -> Json<Value> {
            state
                .posts
                .lock()
                .push(json!({ "kind": "complete_upload_ambiguous" }));
            Json(json!({ "ok": false, "error": "internal_error" }))
        }

        let temp = tempdir()?;
        let state = FakeSlackOutputState::default();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let upload_url = format!("http://{address}/upload/F1");
        let router = Router::new()
            .route("/chat.postMessage", post(chat_post_message))
            .route(
                "/files.getUploadURLExternal",
                post({
                    let upload_url = upload_url.clone();
                    move |State(state): State<FakeSlackOutputState>| {
                        let upload_url = upload_url.clone();
                        async move {
                            state
                                .posts
                                .lock()
                                .push(json!({ "kind": "upload_descriptor" }));
                            Json(json!({
                                "ok": true,
                                "upload_url": upload_url,
                                "file_id": "F1"
                            }))
                        }
                    }
                }),
            )
            .route("/files.completeUploadExternal", post(complete_upload))
            .route("/upload/{file_id}", post(upload_bytes))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let asset = assets.import_bytes("demo.txt", Some("text/plain"), b"hello")?;
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let registry = Arc::new(ConnectorRegistry::resolve(
            ConnectorSettings {
                slack_connectors: vec![SlackConnectorConfig {
                    name: "workspace".to_string(),
                    bot_token: Some("xoxb-test".to_string()),
                    bot_token_env: None,
                    bot_token_secret_ref: None,
                    signing_secret: None,
                    signing_secret_env: None,
                    signing_secret_secret_ref: None,
                    allow_unauthenticated_ingress: true,
                    api_base_url: Some(format!("http://{address}")),
                    fixed_session_id: None,
                    include_self_output: false,
                    additional_reply_targets: Vec::new(),
                    additional_binding_keys: Vec::new(),
                    session_policy: ConnectorSessionPolicy::default(),
                    ingress_events_per_second:
                        crate::connectors::slack_default_ingress_events_per_second(),
                    allowed_api_app_ids: Vec::new(),
                    allowed_enterprise_ids: Vec::new(),
                    allowed_team_ids: Vec::new(),
                    allowed_channel_ids: Vec::new(),
                    allowed_file_hosts: Vec::new(),
                    team_bot_tokens: Vec::new(),
                }],
                ..ConnectorSettings::default()
            },
            auth_manager.as_ref(),
        )?);
        let plugin = SlackOutputPlugin::new(
            registry,
            assets,
            Arc::new(NoopObserver),
            temp.path().join("progress"),
        );
        let reply = ReplyHandle {
            plugin: "slack".to_string(),
            address: encode_slack_reply_route(&SlackReplyRoute {
                connector: "workspace".to_string(),
                enterprise_id: None,
                team_id: None,
                channel_id: "C123".to_string(),
                thread_ts: None,
            }),
        };
        let envelope = ResponseEnvelope {
            conversation: ConversationKey {
                session_id: "session-1".to_string(),
                thread_id: None,
            },
            reply_targets: vec![reply.clone()],
            reply: Some(reply),
            content: "Attached asset: demo.txt (text/plain)".to_string(),
            parts: vec![
                ContentPart::Attachment {
                    attachment: asset.attachment_ref(),
                },
                ContentPart::Text {
                    text: "Here is the file.".to_string(),
                },
            ],
            artifacts: Vec::new(),
            metadata: json!({ "delivery_id": "delivery-ambiguous" }),
        };

        let first_error = <SlackOutputPlugin as OutputPlugin>::deliver(&plugin, envelope.clone())
            .await
            .expect_err("ambiguous completion should fail closed");
        let first_error_chain = format!("{first_error:#}");
        assert!(
            first_error_chain.contains("files.completeUploadExternal returned internal_error"),
            "unexpected first error: {first_error_chain}"
        );
        let second_error = <SlackOutputPlugin as OutputPlugin>::deliver(&plugin, envelope)
            .await
            .expect_err("retry after ambiguous completion should not call Slack again");
        let second_error_chain = format!("{second_error:#}");
        assert!(
            second_error_chain
                .contains("ambiguous after a previous files.completeUploadExternal request"),
            "unexpected second error: {second_error_chain}"
        );

        let posts = state.posts.lock().clone();
        let count_kind = |kind: &str| {
            posts
                .iter()
                .filter(|post| post.get("kind").and_then(Value::as_str) == Some(kind))
                .count()
        };
        assert_eq!(count_kind("chat_postMessage"), 1, "{posts:#?}");
        assert_eq!(count_kind("upload_descriptor"), 1, "{posts:#?}");
        assert_eq!(count_kind("uploaded_bytes"), 1, "{posts:#?}");
        assert_eq!(count_kind("complete_upload_ambiguous"), 1, "{posts:#?}");

        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn slack_upload_completion_non_rate_limit_errors_are_terminal() -> anyhow::Result<()> {
        async fn chat_post_message(
            State(state): State<FakeSlackOutputState>,
            Json(payload): Json<Value>,
        ) -> Json<Value> {
            state
                .posts
                .lock()
                .push(json!({ "kind": "chat_postMessage", "payload": payload }));
            Json(json!({ "ok": true, "ts": "1710000000.000999" }))
        }

        async fn upload_bytes(
            State(state): State<FakeSlackOutputState>,
            AxumPath(file_id): AxumPath<String>,
            body: Bytes,
        ) -> Json<Value> {
            state.posts.lock().push(json!({
                "kind": "uploaded_bytes",
                "file_id": file_id,
                "byte_length": body.len(),
            }));
            Json(json!({ "ok": true }))
        }

        async fn complete_upload(State(state): State<FakeSlackOutputState>) -> Json<Value> {
            state
                .posts
                .lock()
                .push(json!({ "kind": "complete_upload_invalid" }));
            Json(json!({ "ok": false, "error": "invalid_arguments" }))
        }

        let temp = tempdir()?;
        let state = FakeSlackOutputState::default();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let upload_url = format!("http://{address}/upload/F1");
        let router = Router::new()
            .route("/chat.postMessage", post(chat_post_message))
            .route(
                "/files.getUploadURLExternal",
                post({
                    let upload_url = upload_url.clone();
                    move |State(state): State<FakeSlackOutputState>| {
                        let upload_url = upload_url.clone();
                        async move {
                            state
                                .posts
                                .lock()
                                .push(json!({ "kind": "upload_descriptor" }));
                            Json(json!({
                                "ok": true,
                                "upload_url": upload_url,
                                "file_id": "F1"
                            }))
                        }
                    }
                }),
            )
            .route("/files.completeUploadExternal", post(complete_upload))
            .route("/upload/{file_id}", post(upload_bytes))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let asset = assets.import_bytes("demo.txt", Some("text/plain"), b"hello")?;
        let auth_manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let registry = Arc::new(ConnectorRegistry::resolve(
            ConnectorSettings {
                slack_connectors: vec![SlackConnectorConfig {
                    name: "workspace".to_string(),
                    bot_token: Some("xoxb-test".to_string()),
                    bot_token_env: None,
                    bot_token_secret_ref: None,
                    signing_secret: None,
                    signing_secret_env: None,
                    signing_secret_secret_ref: None,
                    allow_unauthenticated_ingress: true,
                    api_base_url: Some(format!("http://{address}")),
                    fixed_session_id: None,
                    include_self_output: false,
                    additional_reply_targets: Vec::new(),
                    additional_binding_keys: Vec::new(),
                    session_policy: ConnectorSessionPolicy::default(),
                    ingress_events_per_second:
                        crate::connectors::slack_default_ingress_events_per_second(),
                    allowed_api_app_ids: Vec::new(),
                    allowed_enterprise_ids: Vec::new(),
                    allowed_team_ids: Vec::new(),
                    allowed_channel_ids: Vec::new(),
                    allowed_file_hosts: Vec::new(),
                    team_bot_tokens: Vec::new(),
                }],
                ..ConnectorSettings::default()
            },
            auth_manager.as_ref(),
        )?);
        let plugin = SlackOutputPlugin::new(
            registry,
            assets,
            Arc::new(NoopObserver),
            temp.path().join("progress"),
        );
        let reply = ReplyHandle {
            plugin: "slack".to_string(),
            address: encode_slack_reply_route(&SlackReplyRoute {
                connector: "workspace".to_string(),
                enterprise_id: None,
                team_id: None,
                channel_id: "C123".to_string(),
                thread_ts: None,
            }),
        };
        let envelope = ResponseEnvelope {
            conversation: ConversationKey {
                session_id: "session-1".to_string(),
                thread_id: None,
            },
            reply_targets: vec![reply.clone()],
            reply: Some(reply),
            content: "Attached asset: demo.txt (text/plain)".to_string(),
            parts: vec![
                ContentPart::Attachment {
                    attachment: asset.attachment_ref(),
                },
                ContentPart::Text {
                    text: "Here is the file.".to_string(),
                },
            ],
            artifacts: Vec::new(),
            metadata: json!({ "delivery_id": "delivery-invalid-completion" }),
        };

        let error = <SlackOutputPlugin as OutputPlugin>::deliver(&plugin, envelope)
            .await
            .expect_err("invalid completion response should be terminal");
        let error_chain = format!("{error:#}");
        assert!(
            error_chain.contains("slack upload completion failed permanently: invalid_arguments"),
            "unexpected error: {error_chain}"
        );

        let posts = state.posts.lock().clone();
        let count_kind = |kind: &str| {
            posts
                .iter()
                .filter(|post| post.get("kind").and_then(Value::as_str) == Some(kind))
                .count()
        };
        assert_eq!(count_kind("chat_postMessage"), 1, "{posts:#?}");
        assert_eq!(count_kind("upload_descriptor"), 1, "{posts:#?}");
        assert_eq!(count_kind("uploaded_bytes"), 1, "{posts:#?}");
        assert_eq!(count_kind("complete_upload_invalid"), 1, "{posts:#?}");

        server.abort();
        Ok(())
    }
}
