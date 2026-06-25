use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;

use kheish_core::ModelDriver;

use crate::connectors::config::ResolvedSlackConnector;
use crate::{DaemonState, SubmitInputRequest};

use super::multimodal::{
    ConnectorMediaRef, apply_connector_multimodal_input, download_connector_media,
};
use super::{
    ConnectorIngressGuard, acquire_connector_ingress_with_fingerprint,
    connector_retry_after_problem_response, internal_error, release_connector_ingress,
    submit_connector_run_with_guard, take_connector_ingress_rate_limit, unauthorized,
};

type HmacSha256 = Hmac<Sha256>;
const SLACK_SIGNATURE_MAX_AGE_SECS: i64 = 300;

#[derive(Debug, Deserialize)]
struct SlackWebhookEnvelope {
    #[serde(rename = "type")]
    type_name: String,
    #[serde(default)]
    challenge: Option<String>,
    #[serde(default)]
    event_id: Option<String>,
    #[serde(default)]
    api_app_id: Option<String>,
    #[serde(default)]
    enterprise_id: Option<String>,
    #[serde(default)]
    team_id: Option<String>,
    #[serde(default)]
    context_enterprise_id: Option<String>,
    #[serde(default)]
    context_team_id: Option<String>,
    #[serde(default)]
    authorizations: Vec<SlackAuthorization>,
    #[serde(default)]
    minute_rate_limited: Option<i64>,
    #[serde(default)]
    event: Option<SlackEvent>,
}

#[derive(Debug, Deserialize)]
struct SlackAuthorization {
    #[serde(default)]
    enterprise_id: Option<String>,
    #[serde(default)]
    team_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct SlackEvent {
    #[serde(rename = "type")]
    type_name: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    team: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    files: Vec<SlackFile>,
}

#[derive(Clone, Debug, Deserialize)]
struct SlackFile {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    mimetype: Option<String>,
    #[serde(default)]
    url_private: Option<String>,
    #[serde(default)]
    url_private_download: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

fn verify_slack_signature(
    headers: &HeaderMap,
    body: &str,
    expected: Option<&str>,
    allow_unauthenticated_ingress: bool,
) -> Result<()> {
    let Some(expected) = expected else {
        anyhow::ensure!(
            allow_unauthenticated_ingress,
            "slack connector ingress authentication is not configured"
        );
        return Ok(());
    };
    let timestamp = headers
        .get("x-slack-request-timestamp")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow!("missing x-slack-request-timestamp"))?;
    let signature = headers
        .get("x-slack-signature")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow!("missing x-slack-signature"))?;
    let signature = signature
        .strip_prefix("v0=")
        .ok_or_else(|| anyhow!("invalid x-slack-signature"))?;
    let signature = hex::decode(signature).map_err(|_| anyhow!("invalid x-slack-signature"))?;
    let timestamp_secs = timestamp
        .parse::<i64>()
        .map_err(|_| anyhow!("invalid x-slack-request-timestamp"))?;
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow!("system time should be after unix epoch"))?
        .as_secs() as i64;
    let skew = now_secs
        .checked_sub(timestamp_secs)
        .and_then(|value| value.checked_abs())
        .unwrap_or(i64::MAX);
    anyhow::ensure!(
        skew <= SLACK_SIGNATURE_MAX_AGE_SECS,
        "slack request timestamp is too old or too far in the future"
    );
    let mut mac = HmacSha256::new_from_slice(expected.as_bytes())
        .map_err(|_| anyhow!("invalid slack signing secret"))?;
    mac.update(format!("v0:{timestamp}:{body}").as_bytes());
    mac.verify_slice(&signature)
        .map_err(|_| anyhow!("slack request signature mismatch"))?;
    Ok(())
}

pub(super) async fn slack_webhook<M>(
    State(state): State<Arc<DaemonState<M>>>,
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, (StatusCode, String)>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let connector = state.connectors().slack(&name).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("unknown slack connector {name}"),
        )
    })?;
    let raw =
        std::str::from_utf8(&body).map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    verify_slack_signature(
        &headers,
        raw,
        connector.signing_secret.as_deref(),
        connector.allow_unauthenticated_ingress,
    )
    .map_err(|error| unauthorized(error.to_string()))?;
    let payload = serde_json::from_str::<SlackWebhookEnvelope>(raw).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid slack webhook payload: {error}"),
        )
    })?;
    let raw_value = serde_json::from_str::<Value>(raw).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid slack webhook payload: {error}"),
        )
    })?;
    if payload.type_name == "url_verification" {
        return Ok(Json(json!({
            "challenge": payload.challenge.unwrap_or_default(),
        }))
        .into_response());
    }
    if payload.type_name != "event_callback" && payload.type_name != "app_rate_limited" {
        let team_id = slack_team_id(&payload, &SlackEvent::default());
        let enterprise_id = slack_enterprise_id(&payload);
        validate_slack_origin_without_channel(
            &connector,
            payload.api_app_id.as_deref(),
            enterprise_id.as_deref(),
            team_id.as_deref(),
        )
        .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        validate_slack_authorizations(&connector, &payload.authorizations)
            .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        return Ok(slack_ignored_response("unsupported_envelope"));
    }
    if payload.type_name == "app_rate_limited" {
        let team_id = slack_team_id(&payload, &SlackEvent::default());
        let enterprise_id = slack_enterprise_id(&payload);
        validate_slack_origin_without_channel(
            &connector,
            payload.api_app_id.as_deref(),
            enterprise_id.as_deref(),
            team_id.as_deref(),
        )
        .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        validate_slack_authorizations(&connector, &payload.authorizations)
            .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        return Ok(Json(json!({
            "ignored": true,
            "type": "app_rate_limited",
            "minute_rate_limited": payload.minute_rate_limited,
        }))
        .into_response());
    }
    let event = payload
        .event
        .clone()
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing slack event".to_string()))?;
    let team_id = slack_team_id(&payload, &event);
    let enterprise_id = slack_enterprise_id(&payload);
    if event.type_name != "message" {
        validate_slack_origin_without_channel(
            &connector,
            payload.api_app_id.as_deref(),
            enterprise_id.as_deref(),
            team_id.as_deref(),
        )
        .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        validate_slack_authorizations(&connector, &payload.authorizations)
            .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        return Ok(slack_ignored_response("unsupported_event"));
    }
    if event.bot_id.is_some()
        || event.subtype.as_deref() == Some("bot_message")
        || !slack_message_subtype_supported(event.subtype.as_deref())
    {
        validate_slack_message_origin_optional_channel(
            &connector,
            payload.api_app_id.as_deref(),
            enterprise_id.as_deref(),
            team_id.as_deref(),
            event.channel.as_deref(),
        )
        .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        validate_slack_authorizations(&connector, &payload.authorizations)
            .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        return Ok(slack_ignored_response("unsupported_message_subtype"));
    }
    let channel_id = event
        .channel
        .clone()
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing slack channel".to_string()))?;
    let root_ts = event
        .thread_ts
        .clone()
        .or(event.ts.clone())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "missing slack timestamp".to_string(),
            )
        })?;
    validate_slack_origin(
        &connector,
        payload.api_app_id.as_deref(),
        enterprise_id.as_deref(),
        team_id.as_deref(),
        &channel_id,
    )
    .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
    validate_slack_authorizations(&connector, &payload.authorizations)
        .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
    let binding_keys = connector.binding_keys_scoped(
        &channel_id,
        &root_ts,
        enterprise_id.as_deref(),
        team_id.as_deref(),
    );
    let session_id = connector
        .fixed_session_id
        .clone()
        .or(state
            .bound_session_id(&binding_keys)
            .await
            .map_err(internal_error)?)
        .unwrap_or_else(|| {
            connector.natural_session_id_scoped(
                &channel_id,
                &root_ts,
                enterprise_id.as_deref(),
                team_id.as_deref(),
            )
        });
    let reply_targets = connector.reply_targets_scoped(
        &channel_id,
        Some(root_ts.clone()),
        enterprise_id.clone(),
        team_id.clone(),
    );
    let text = event.text.clone().filter(|text| !text.trim().is_empty());
    let ingress_key = payload
        .event_id
        .or_else(|| event.ts.clone().map(|ts| format!("{channel_id}:{ts}")))
        .map(|key| {
            let scope = slack_ingress_scope(enterprise_id.as_deref(), team_id.as_deref());
            format!("slack:{name}:{scope}:{key}")
        });
    let ingress_fingerprint = slack_ingress_fingerprint(&raw_value).map_err(internal_error)?;
    let ingress = acquire_connector_ingress_with_fingerprint(
        &state,
        ingress_key.as_deref(),
        &ingress_fingerprint,
    )
    .await
    .map_err(internal_error)?;
    if let ConnectorIngressGuard::Existing(run) = &ingress {
        return Ok(Json(run.clone()).into_response());
    }
    let rate_scope = slack_ingress_rate_scope(
        &state.control_plane_base_url(),
        &name,
        enterprise_id.as_deref(),
        team_id.as_deref(),
        &channel_id,
    );
    if let Some(retry_after_ms) =
        take_connector_ingress_rate_limit(rate_scope, connector.ingress_events_per_second).await
    {
        release_connector_ingress(&state, ingress)
            .await
            .map_err(internal_error)?;
        return Ok(slack_retry_after_response(&name, retry_after_ms));
    }
    let files = match slack_media_refs(
        &event,
        &connector,
        connector.bot_token_for_team(team_id.as_deref()),
    ) {
        Ok(files) => files,
        Err(error) => {
            release_connector_ingress(&state, ingress)
                .await
                .map_err(internal_error)?;
            return Err((StatusCode::BAD_REQUEST, error.to_string()));
        }
    };
    let uploads = match download_connector_media(&files).await {
        Ok(uploads) => uploads,
        Err(error) => {
            release_connector_ingress(&state, ingress)
                .await
                .map_err(internal_error)?;
            return Err(internal_error(error));
        }
    };
    let mut request = SubmitInputRequest {
        provider: None,
        source_plugin: Some("slack".to_string()),
        source_kind: Some(event.type_name.clone()),
        actor_id: event.user.clone().or(Some("slack-user".to_string())),
        content: String::new(),
        input_items: Vec::new(),
        attachments: Vec::new(),
        generation: None,
        completion_requirements: None,
        metadata: Some(slack_ingress_metadata(raw_value, &ingress_fingerprint)),
        binding_keys,
        reply_targets,
        reply_plugin: None,
        reply_address: None,
    };
    if let Err(error) = apply_connector_multimodal_input(&mut request, text, uploads) {
        release_connector_ingress(&state, ingress)
            .await
            .map_err(internal_error)?;
        return Err((StatusCode::BAD_REQUEST, error.to_string()));
    }
    let run = submit_connector_run_with_guard(
        &state,
        &session_id,
        request,
        ingress,
        &connector.session_policy,
        &format!("slack connector {name}"),
    )
    .await
    .map_err(internal_error)?;
    Ok(Json(run).into_response())
}

fn slack_media_refs(
    event: &SlackEvent,
    connector: &ResolvedSlackConnector,
    bearer_token: Option<&str>,
) -> Result<Vec<ConnectorMediaRef>> {
    if !event.files.is_empty() && bearer_token.is_none() {
        bail!("slack connector requires bot_token to download inbound files");
    }
    if !event.files.is_empty()
        && connector.signing_secret.is_none()
        && connector.allow_unauthenticated_ingress
    {
        bail!("slack file downloads require authenticated ingress");
    }
    event
        .files
        .iter()
        .map(|file| {
            let url = file
                .url_private_download
                .clone()
                .or(file.url_private.clone())
                .ok_or_else(|| anyhow!("slack file is missing a download URL"))?;
            validate_slack_file_url(connector, &url)?;
            let file_name = file
                .name
                .clone()
                .or(file.id.clone())
                .ok_or_else(|| anyhow!("slack file is missing a file name"))?;
            Ok(ConnectorMediaRef {
                file_name,
                media_type: file.mimetype.clone(),
                byte_length_hint: file.size,
                url,
                bearer_token: bearer_token.map(str::to_string),
            })
        })
        .collect()
}

fn slack_message_subtype_supported(subtype: Option<&str>) -> bool {
    matches!(
        subtype.map(str::trim).filter(|value| !value.is_empty()),
        None | Some("file_share")
    )
}

fn slack_ignored_response(reason: &'static str) -> Response {
    Json(json!({
        "ignored": true,
        "reason": reason,
    }))
    .into_response()
}

fn slack_ingress_fingerprint(payload: &Value) -> Result<String> {
    kheish_codec::digest_serialize(payload)
}

fn slack_ingress_metadata(mut payload: Value, fingerprint: &str) -> Value {
    if let Value::Object(map) = &mut payload {
        map.insert(
            "slack_ingress_fingerprint".to_string(),
            Value::String(fingerprint.to_string()),
        );
        return payload;
    }
    json!({
        "slack_payload": payload,
        "slack_ingress_fingerprint": fingerprint,
    })
}

fn validate_slack_origin(
    connector: &ResolvedSlackConnector,
    api_app_id: Option<&str>,
    enterprise_id: Option<&str>,
    team_id: Option<&str>,
    channel_id: &str,
) -> Result<()> {
    anyhow::ensure!(
        connector.is_api_app_allowed(api_app_id),
        "slack api_app_id is not allowed"
    );
    anyhow::ensure!(
        connector.is_enterprise_allowed(enterprise_id),
        "slack enterprise_id is not allowed"
    );
    anyhow::ensure!(
        connector.is_team_allowed(team_id),
        "slack team_id is not allowed"
    );
    anyhow::ensure!(
        connector.is_channel_allowed(channel_id),
        "slack channel_id is not allowed"
    );
    Ok(())
}

fn validate_slack_origin_without_channel(
    connector: &ResolvedSlackConnector,
    api_app_id: Option<&str>,
    enterprise_id: Option<&str>,
    team_id: Option<&str>,
) -> Result<()> {
    anyhow::ensure!(
        connector.is_api_app_allowed(api_app_id),
        "slack api_app_id is not allowed"
    );
    anyhow::ensure!(
        connector.is_enterprise_allowed(enterprise_id),
        "slack enterprise_id is not allowed"
    );
    anyhow::ensure!(
        connector.is_team_allowed(team_id),
        "slack team_id is not allowed"
    );
    Ok(())
}

fn validate_slack_message_origin_optional_channel(
    connector: &ResolvedSlackConnector,
    api_app_id: Option<&str>,
    enterprise_id: Option<&str>,
    team_id: Option<&str>,
    channel_id: Option<&str>,
) -> Result<()> {
    match channel_id.map(str::trim).filter(|value| !value.is_empty()) {
        Some(channel_id) => {
            validate_slack_origin(connector, api_app_id, enterprise_id, team_id, channel_id)
        }
        None => {
            validate_slack_origin_without_channel(connector, api_app_id, enterprise_id, team_id)
        }
    }
}

fn validate_slack_authorizations(
    connector: &ResolvedSlackConnector,
    authorizations: &[SlackAuthorization],
) -> Result<()> {
    for authorization in authorizations {
        if !connector.is_enterprise_allowed(authorization.enterprise_id.as_deref()) {
            bail!("slack authorization enterprise_id is not allowed");
        }
        if !connector.is_team_allowed(authorization.team_id.as_deref()) {
            bail!("slack authorization team_id is not allowed");
        }
    }
    Ok(())
}

fn slack_team_id(payload: &SlackWebhookEnvelope, event: &SlackEvent) -> Option<String> {
    payload
        .context_team_id
        .as_deref()
        .or(payload.team_id.as_deref())
        .or(event.team.as_deref())
        .or_else(|| {
            payload
                .authorizations
                .iter()
                .find_map(|authorization| authorization.team_id.as_deref())
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn slack_enterprise_id(payload: &SlackWebhookEnvelope) -> Option<String> {
    payload
        .context_enterprise_id
        .as_deref()
        .or(payload.enterprise_id.as_deref())
        .or_else(|| {
            payload
                .authorizations
                .iter()
                .find_map(|authorization| authorization.enterprise_id.as_deref())
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn slack_ingress_scope(enterprise_id: Option<&str>, team_id: Option<&str>) -> String {
    match (
        enterprise_id
            .map(str::trim)
            .filter(|value| !value.is_empty()),
        team_id.map(str::trim).filter(|value| !value.is_empty()),
    ) {
        (Some(enterprise_id), Some(team_id)) => {
            format!("enterprise:{enterprise_id}:team:{team_id}")
        }
        (Some(enterprise_id), None) => format!("enterprise:{enterprise_id}"),
        (None, Some(team_id)) => format!("team:{team_id}"),
        (None, None) => "workspace".to_string(),
    }
}

fn slack_ingress_rate_scope(
    base_url: &str,
    connector_name: &str,
    enterprise_id: Option<&str>,
    team_id: Option<&str>,
    channel_id: &str,
) -> String {
    format!(
        "slack:{base_url}:{connector_name}:{}:channel:{}",
        slack_ingress_scope(enterprise_id, team_id),
        channel_id.trim()
    )
}

fn slack_retry_after_response(name: &str, retry_after_ms: u64) -> Response {
    let message = format!(
        "slack connector {name} ingress rate limit exceeded; retry_after_ms={retry_after_ms}"
    );
    connector_retry_after_problem_response("slack_ingress_rate_limited", message, retry_after_ms)
}

fn validate_slack_file_url(connector: &ResolvedSlackConnector, url: &str) -> Result<()> {
    let parsed = Url::parse(url).map_err(|_| anyhow!("invalid slack file URL"))?;
    let Some(host) = parsed.host_str().map(|host| host.to_ascii_lowercase()) else {
        bail!("slack file URL must include a host");
    };
    let allowed = if connector.allowed_file_hosts.is_empty() {
        default_slack_file_host_allowed(connector, &parsed, &host)
    } else {
        parsed.scheme() == "https"
            && connector
                .allowed_file_hosts
                .iter()
                .any(|allowed| allowed == &host)
    };
    anyhow::ensure!(allowed, "slack file URL host is not allowed");
    Ok(())
}

fn default_slack_file_host_allowed(
    connector: &ResolvedSlackConnector,
    parsed: &Url,
    host: &str,
) -> bool {
    if matches!(
        host,
        "files.slack.com" | "files.slack-edge.com" | "slack-files.com"
    ) {
        return parsed.scheme() == "https";
    }
    let Ok(api_url) = Url::parse(&connector.api_base_url) else {
        return false;
    };
    api_url
        .host_str()
        .map(|api_host| api_host.eq_ignore_ascii_case(host) && api_url.scheme() == parsed.scheme())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::connectors::config::ConnectorSessionPolicy;

    use super::{
        ResolvedSlackConnector, SlackEvent, slack_ingress_rate_scope, slack_team_id,
        validate_slack_file_url,
    };

    fn test_connector() -> ResolvedSlackConnector {
        ResolvedSlackConnector {
            name: "workspace".to_string(),
            bot_token: Some("xoxb-test".to_string()),
            signing_secret: None,
            allow_unauthenticated_ingress: true,
            api_base_url: "https://slack.com/api".to_string(),
            fixed_session_id: None,
            include_self_output: true,
            additional_reply_targets: Vec::new(),
            additional_binding_keys: Vec::new(),
            session_policy: ConnectorSessionPolicy::default(),
            ingress_events_per_second: crate::connectors::slack_default_ingress_events_per_second(),
            allowed_api_app_ids: Vec::new(),
            allowed_enterprise_ids: Vec::new(),
            allowed_team_ids: Vec::new(),
            allowed_channel_ids: Vec::new(),
            allowed_file_hosts: vec!["files.slack.com".to_string()],
            team_bot_tokens: BTreeMap::new(),
        }
    }

    #[test]
    fn explicit_slack_file_hosts_require_https() {
        let connector = test_connector();
        validate_slack_file_url(
            &connector,
            "https://files.slack.com/files-pri/T123-F123/download/demo.txt",
        )
        .expect("https allowlisted slack file host should pass");
        let error = validate_slack_file_url(
            &connector,
            "http://files.slack.com/files-pri/T123-F123/download/demo.txt",
        )
        .expect_err("http allowlisted slack file host should fail");
        assert!(
            error.to_string().contains("host is not allowed"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn slack_app_rate_limited_team_selection_uses_top_level_team() {
        let payload = super::SlackWebhookEnvelope {
            type_name: "app_rate_limited".to_string(),
            challenge: None,
            event_id: None,
            api_app_id: Some("A123".to_string()),
            enterprise_id: None,
            team_id: Some("T_RATE_LIMITED".to_string()),
            context_enterprise_id: None,
            context_team_id: None,
            authorizations: Vec::new(),
            minute_rate_limited: Some(1_712_486_400),
            event: None,
        };

        assert_eq!(
            slack_team_id(&payload, &SlackEvent::default()),
            Some("T_RATE_LIMITED".to_string())
        );
    }

    #[test]
    fn slack_ingress_rate_scope_separates_team_and_channel() {
        assert_ne!(
            slack_ingress_rate_scope("http://127.0.0.1:4000", "ops", Some("E1"), Some("T1"), "C1"),
            slack_ingress_rate_scope("http://127.0.0.1:4000", "ops", Some("E1"), Some("T2"), "C1")
        );
        assert_ne!(
            slack_ingress_rate_scope("http://127.0.0.1:4000", "ops", None, Some("T1"), "C1"),
            slack_ingress_rate_scope("http://127.0.0.1:4000", "ops", None, Some("T1"), "C2")
        );
    }
}
