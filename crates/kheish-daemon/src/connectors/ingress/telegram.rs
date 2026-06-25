use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use kheish_core::ModelDriver;

use crate::DaemonState;
use crate::connectors::config::{ResolvedTelegramConnector, telegram_bot_api_method_url};
use crate::connectors::routes::telegram::{
    TelegramCallbackQuery, TelegramMessage, submit_telegram_callback_query,
    submit_telegram_message, submit_telegram_message_with_kind,
};

const ERROR_RETRY_DELAY: Duration = Duration::from_secs(2);
const POLLING_WARN_THROTTLE: Duration = Duration::from_secs(30);
const MAX_TELEGRAM_RETRY_AFTER_MS: u64 = 60 * 60 * 1_000;
const TERMINAL_UPDATE_ERROR_PATTERNS: &[&str] = &[
    "session auto-creation is disabled by the connector session policy",
    "unknown persona",
    "is already bound to a different persona",
    "is already bound to a different capability scope",
    "missing connector content",
    "unsupported connector file url scheme",
    "connector file is missing a file name",
    "failed to decode image/",
    "normalized image exceeds",
    "unsupported attachment",
    "telegram getFile returned ok=false",
    "telegram getFile response missing file_path",
];

#[derive(Debug, Deserialize)]
struct TelegramUpdatesResponse {
    ok: bool,
    #[serde(default)]
    result: Vec<TelegramUpdate>,
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

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    #[serde(default)]
    message: Option<TelegramMessage>,
    #[serde(default)]
    edited_message: Option<TelegramMessage>,
    #[serde(default)]
    callback_query: Option<TelegramCallbackQuery>,
}

#[derive(Debug)]
pub(crate) struct TelegramPollingState {
    client: reqwest::Client,
}

impl Default for TelegramPollingState {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("kheish-daemon/telegram-polling")
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("telegram polling client should build"),
        }
    }
}

#[derive(Debug, Default)]
struct ThrottledWarnState {
    last_emit: Option<Instant>,
    suppressed: u64,
}

impl ThrottledWarnState {
    fn warn(
        &mut self,
        connector: &str,
        context: &'static str,
        error: &anyhow::Error,
        extra: impl FnOnce() -> Vec<(&'static str, String)>,
    ) {
        let now = Instant::now();
        if self
            .last_emit
            .is_some_and(|last_emit| now.duration_since(last_emit) < POLLING_WARN_THROTTLE)
        {
            self.suppressed = self.suppressed.saturating_add(1);
            return;
        }

        let suppressed = std::mem::take(&mut self.suppressed);
        self.last_emit = Some(now);
        let extra = extra();
        if extra.is_empty() {
            let error = error.to_string();
            warn!(
                connector,
                suppressed_since_last_log = suppressed,
                error = %error,
                "{context}"
            );
            return;
        }

        let extra = extra
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" ");
        warn!(
            connector,
            suppressed_since_last_log = suppressed,
            error = %error,
            extra = %extra,
            "{context}"
        );
    }
}

pub(crate) async fn poll_connector_loop<M>(
    state: Arc<DaemonState<M>>,
    polling_state: Arc<TelegramPollingState>,
    connector: ResolvedTelegramConnector,
) -> Result<()>
where
    M: ModelDriver + Send + Sync + 'static,
{
    let mut next_update_id = state
        .connector_next_update_id(&connector.polling_cursor_key())
        .await
        .unwrap_or(0);
    let mut submit_warns = ThrottledWarnState::default();
    let mut cursor_warns = ThrottledWarnState::default();
    let mut fetch_warns = ThrottledWarnState::default();
    loop {
        match fetch_updates(&polling_state, &connector, next_update_id).await {
            Ok(updates) => {
                let mut cursor_dirty = false;
                for update in updates {
                    let submit = if let Some(message) = update.message {
                        let metadata = json!({
                            "update_id": update.update_id,
                            "message": message,
                            "ingress_mode": "polling",
                        });
                        submit_telegram_message(
                            &state,
                            &connector.name,
                            &connector,
                            Some(update.update_id),
                            message,
                            metadata,
                        )
                        .await
                    } else if let Some(message) = update.edited_message {
                        let metadata = json!({
                            "update_id": update.update_id,
                            "edited_message": message,
                            "ingress_mode": "polling",
                        });
                        submit_telegram_message_with_kind(
                            &state,
                            &connector.name,
                            &connector,
                            Some(update.update_id),
                            message,
                            metadata,
                            "edited_message",
                        )
                        .await
                    } else if let Some(callback_query) = update.callback_query {
                        let metadata = json!({
                            "update_id": update.update_id,
                            "callback_query": callback_query,
                            "ingress_mode": "polling",
                        });
                        submit_telegram_callback_query(
                            &state,
                            &connector.name,
                            &connector,
                            Some(update.update_id),
                            callback_query,
                            metadata,
                        )
                        .await
                    } else {
                        next_update_id = next_update_id.max(update.update_id.saturating_add(1));
                        cursor_dirty = true;
                        continue;
                    };
                    if let Err(error) = submit {
                        if is_terminal_polling_submission_error(&error) {
                            submit_warns.warn(
                                &connector.name,
                                "skipping terminally invalid telegram polling update",
                                &error,
                                || vec![("update_id", update.update_id.to_string())],
                            );
                            next_update_id = next_update_id.max(update.update_id.saturating_add(1));
                            cursor_dirty = true;
                            continue;
                        }
                        submit_warns.warn(
                            &connector.name,
                            "telegram polling update submission failed",
                            &error,
                            || vec![("update_id", update.update_id.to_string())],
                        );
                        tokio::time::sleep(ERROR_RETRY_DELAY).await;
                        break;
                    }
                    next_update_id = next_update_id.max(update.update_id.saturating_add(1));
                    cursor_dirty = true;
                }
                if cursor_dirty
                    && let Err(error) = state
                        .remember_connector_next_update_id(
                            &connector.polling_cursor_key(),
                            next_update_id,
                        )
                        .await
                {
                    cursor_warns.warn(
                        &connector.name,
                        "telegram polling cursor persistence failed",
                        &error,
                        || vec![("next_update_id", next_update_id.to_string())],
                    );
                }
            }
            Err(error) => {
                let retry_after_ms = error
                    .downcast_ref::<TelegramPollingRetryAfterError>()
                    .map(|error| error.retry_after_ms);
                fetch_warns.warn(
                    &connector.name,
                    "telegram polling fetch failed",
                    &error,
                    Vec::new,
                );
                tokio::time::sleep(
                    retry_after_ms
                        .map(Duration::from_millis)
                        .unwrap_or(ERROR_RETRY_DELAY),
                )
                .await;
            }
        }
    }
}

async fn fetch_updates(
    polling_state: &TelegramPollingState,
    connector: &ResolvedTelegramConnector,
    next_update_id: i64,
) -> Result<Vec<TelegramUpdate>> {
    let bot_token = connector.bot_token.as_deref().ok_or_else(|| {
        anyhow!(
            "telegram connector {} has no bot token configured",
            connector.name
        )
    })?;
    let response = polling_state
        .client
        .post(telegram_bot_api_method_url(
            &connector.api_base_url,
            bot_token,
            "getUpdates",
        ))
        .json(&json!({
            "offset": next_update_id,
            "timeout": connector.polling_timeout_seconds,
            "allowed_updates": ["message", "edited_message", "callback_query"],
        }))
        .timeout(Duration::from_secs(
            connector.polling_timeout_seconds.saturating_add(10),
        ))
        .send()
        .await
        .map_err(|error| {
            anyhow!(
                "failed to poll telegram connector {}: {}",
                connector.name,
                redact_telegram_bot_token(&error.to_string(), bot_token)
            )
        })?;
    let status = response.status();
    let headers = response.headers().clone();
    let raw = response
        .text()
        .await
        .context("failed to read telegram polling response")?;
    let header_retry_after_ms = telegram_retry_after_header_ms(&headers);
    let body = match serde_json::from_str::<TelegramUpdatesResponse>(&raw) {
        Ok(body) => body,
        Err(error)
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS
                && header_retry_after_ms.is_some() =>
        {
            let retry_after_ms = header_retry_after_ms.unwrap_or(1_000);
            return Err(telegram_polling_retry_after_error(
                retry_after_ms,
                format!(
                    "telegram connector {} flood-wait; retry after {retry_after_ms}ms",
                    connector.name
                ),
            ));
        }
        Err(error) => bail!("failed to decode telegram polling response: {error}"),
    };
    if let Some(retry_after_ms) = telegram_retry_after_ms(&headers, &body)
        && (status == reqwest::StatusCode::TOO_MANY_REQUESTS || body.error_code == Some(429))
    {
        return Err(telegram_polling_retry_after_error(
            retry_after_ms,
            format!(
                "telegram connector {} flood-wait; retry after {retry_after_ms}ms",
                connector.name
            ),
        ));
    }
    if !status.is_success() {
        bail!(
            "telegram connector {} returned HTTP {}: {}",
            connector.name,
            status,
            redact_telegram_bot_token(&raw, bot_token)
        );
    }
    if !body.ok {
        bail!(
            "telegram connector {} returned ok=false: {}",
            connector.name,
            body.description
                .as_deref()
                .unwrap_or("telegram polling error")
        );
    }
    Ok(body.result)
}

#[derive(Debug)]
struct TelegramPollingRetryAfterError {
    retry_after_ms: u64,
    detail: String,
}

impl std::fmt::Display for TelegramPollingRetryAfterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for TelegramPollingRetryAfterError {}

fn telegram_polling_retry_after_error(
    retry_after_ms: u64,
    detail: impl Into<String>,
) -> anyhow::Error {
    TelegramPollingRetryAfterError {
        retry_after_ms: retry_after_ms.max(1),
        detail: detail.into(),
    }
    .into()
}

fn telegram_retry_after_ms(
    headers: &reqwest::header::HeaderMap,
    body: &TelegramUpdatesResponse,
) -> Option<u64> {
    telegram_retry_after_header_ms(headers).or_else(|| {
        body.parameters
            .as_ref()
            .and_then(|parameters| parameters.retry_after)
            .map(|seconds| seconds.saturating_mul(1_000).max(1))
    })
}

fn telegram_retry_after_header_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
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

fn is_terminal_polling_submission_error(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    TERMINAL_UPDATE_ERROR_PATTERNS
        .iter()
        .any(|pattern| message.contains(pattern))
}

fn redact_telegram_bot_token(value: &str, bot_token: &str) -> String {
    if bot_token.is_empty() {
        value.to_string()
    } else {
        value.replace(bot_token, "<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    use super::{
        TelegramResponseParameters, TelegramUpdatesResponse, is_terminal_polling_submission_error,
        parse_telegram_retry_after_ms, redact_telegram_bot_token, telegram_retry_after_ms,
    };

    #[test]
    fn terminal_polling_submission_errors_skip_the_update() {
        assert!(is_terminal_polling_submission_error(&anyhow!(
            "unknown persona persona-404"
        )));
        assert!(is_terminal_polling_submission_error(&anyhow!(
            "missing connector content"
        )));
        assert!(is_terminal_polling_submission_error(&anyhow!(
            "session demo is already bound to a different capability scope"
        )));
    }

    #[test]
    fn transient_polling_submission_errors_still_retry() {
        assert!(!is_terminal_polling_submission_error(&anyhow!(
            "failed to persist auth store"
        )));
        assert!(!is_terminal_polling_submission_error(&anyhow!(
            "connector ingress submission is already in progress for key telegram:ops:1"
        )));
    }

    #[test]
    fn telegram_polling_error_redacts_bot_token() {
        let redacted = redact_telegram_bot_token(
            "request failed for https://api.telegram.org/bot123:secret-token/getUpdates",
            "123:secret-token",
        );
        assert!(redacted.contains("bot<redacted>/getUpdates"));
        assert!(!redacted.contains("123:secret-token"));
    }

    #[test]
    fn telegram_polling_retry_after_reads_header_or_body() {
        let body = TelegramUpdatesResponse {
            ok: false,
            result: Vec::new(),
            error_code: Some(429),
            description: Some("Too Many Requests".to_string()),
            parameters: Some(TelegramResponseParameters {
                retry_after: Some(8),
            }),
        };
        let headers = HeaderMap::new();
        assert_eq!(telegram_retry_after_ms(&headers, &body), Some(8_000));

        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("3"));
        assert_eq!(telegram_retry_after_ms(&headers, &body), Some(3_000));
    }

    #[test]
    fn telegram_polling_retry_after_caps_large_values_and_reads_http_dates() {
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
}
