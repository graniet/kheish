use reqwest::StatusCode;
use serde_json::Value;

use crate::debug::{DebugCaptureLevel, redact_text, summarize_json_value};

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
    if !details.is_empty() {
        rendered.push_str(": ");
        rendered.push_str(&details.join(", "));
    }

    rendered
}

/// Serializes one already-sanitized provider error payload for the requested debug level.
pub(crate) fn safe_error_payload_for_level(level: DebugCaptureLevel, value: &Value) -> Value {
    match level {
        DebugCaptureLevel::Off => Value::Null,
        DebugCaptureLevel::On => summarize_json_value(value),
        DebugCaptureLevel::Redacted | DebugCaptureLevel::Full => value.clone(),
    }
}
