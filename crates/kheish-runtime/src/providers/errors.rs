use reqwest::StatusCode;
use serde_json::Value;

use crate::debug::{DebugCaptureLevel, redact_text, summarize_json_value};
use kheish_types::{ProviderErrorKind, classify_provider_error_message};

const CONTEXT_OVERFLOW_MARKER: &str = "input length and `max_tokens` exceed context limit:";

/// Builds a sanitized provider failure summary safe for logs and debug artifacts.
///
/// Upstream providers may echo credentials, prompts, or other sensitive values in
/// free-form error messages. Kheish keeps only structured identifiers such as the
/// HTTP status, provider error type, and provider error code. The only exception
/// is the known context-overflow message shape used by runtime retry adjustment,
/// which is preserved after token redaction.
pub(crate) fn sanitize_upstream_error_message(
    provider: &str,
    phase: &str,
    status: Option<StatusCode>,
    error_type: Option<&str>,
    error_code: Option<&str>,
    message: Option<&str>,
) -> String {
    if let Some(message) = message.map(str::trim).filter(|message| !message.is_empty()) {
        if message.contains(CONTEXT_OVERFLOW_MARKER) {
            return redact_text(message);
        }
    }

    let mut rendered = format!("{provider} {phase}");
    if let Some(status) = status {
        rendered.push_str(&format!(" with status {}", status.as_u16()));
    }

    let mut details = Vec::new();
    if let Some(error_type) = error_type.map(str::trim).filter(|value| !value.is_empty()) {
        details.push(format!("type={error_type}"));
    }
    if let Some(error_code) = error_code.map(str::trim).filter(|value| !value.is_empty()) {
        details.push(format!("code={error_code}"));
    }
    if classify_upstream_error_kind(status, error_type, error_code, message)
        == ProviderErrorKind::ContextWindowExceeded
    {
        details.push("kind=context_window_exceeded".to_string());
    }
    if !details.is_empty() {
        rendered.push_str(": ");
        rendered.push_str(&details.join(", "));
    }

    rendered
}

fn classify_upstream_error_kind(
    status: Option<StatusCode>,
    error_type: Option<&str>,
    error_code: Option<&str>,
    message: Option<&str>,
) -> ProviderErrorKind {
    let mut parts = Vec::new();
    if let Some(status) = status {
        parts.push(format!("status {}", status.as_u16()));
    }
    if let Some(error_type) = error_type.map(str::trim).filter(|value| !value.is_empty()) {
        parts.push(format!("type={error_type}"));
    }
    if let Some(error_code) = error_code.map(str::trim).filter(|value| !value.is_empty()) {
        parts.push(format!("code={error_code}"));
    }
    if let Some(message) = message.map(str::trim).filter(|value| !value.is_empty()) {
        parts.push(message.to_string());
    }

    classify_provider_error_message(&parts.join(" "))
}

/// Serializes one already-sanitized provider error payload for the requested debug level.
pub(crate) fn safe_error_payload_for_level(level: DebugCaptureLevel, value: &Value) -> Value {
    match level {
        DebugCaptureLevel::Off => Value::Null,
        DebugCaptureLevel::On => summarize_json_value(value),
        DebugCaptureLevel::Redacted | DebugCaptureLevel::Full => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitized_context_window_errors_keep_stable_kind_without_raw_message() {
        let message = sanitize_upstream_error_message(
            "Anthropic",
            "request failed",
            Some(StatusCode::BAD_REQUEST),
            Some("invalid_request_error"),
            None,
            Some("prompt is too long and includes user-secret-value"),
        );

        assert_eq!(
            message,
            "Anthropic request failed with status 400: type=invalid_request_error, kind=context_window_exceeded"
        );
        assert!(!message.contains("user-secret-value"));
        assert_eq!(
            classify_provider_error_message(&message),
            ProviderErrorKind::ContextWindowExceeded
        );
    }

    #[test]
    fn sanitized_auth_errors_do_not_add_context_kind() {
        let message = sanitize_upstream_error_message(
            "Google",
            "request error",
            Some(StatusCode::UNAUTHORIZED),
            Some("UNAUTHENTICATED"),
            Some("401"),
            Some("bad api key secret"),
        );

        assert_eq!(
            message,
            "Google request error with status 401: type=UNAUTHENTICATED, code=401"
        );
        assert!(!message.contains("secret"));
        assert_eq!(
            classify_provider_error_message(&message),
            ProviderErrorKind::Auth
        );
    }
}
