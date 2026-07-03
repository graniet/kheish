use parking_lot::{Mutex, RwLock};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use kheish_codec::{digest_serialize, digest_text};
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tracing::warn;

/// Comma- or newline-separated literal tokens that should be scrubbed from debug artifacts.
pub const DEBUG_REDACT_TOKENS_ENV: &str = "KHEISH_DEBUG_REDACT_TOKENS";
/// File containing newline-separated literal tokens that should be scrubbed from debug artifacts.
pub const DEBUG_REDACT_TOKENS_FILE_ENV: &str = "KHEISH_DEBUG_REDACT_TOKENS_FILE";

/// Operator-visible status for configured debug redaction extensions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugRedactionConfigStatus {
    /// Number of literal tokens loaded from environment and file sources.
    pub literal_token_count: usize,
    /// Whether a token file path is configured.
    pub token_file_configured: bool,
    /// File read error, when a configured token file could not be loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_file_error: Option<String>,
}

/// The amount of debug data captured by the runtime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DebugCaptureLevel {
    /// Disable debug capture entirely.
    #[default]
    Off,
    /// Capture structure, timings, sizes, and digests only.
    On,
    /// Capture redacted content safe for day-to-day operator debugging.
    Redacted,
    /// Capture raw content except for credentials and authorization material.
    Full,
}

impl DebugCaptureLevel {
    /// Returns true when content-bearing debug artifacts should be collected.
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Returns true when the full payload should be emitted.
    pub fn captures_full_content(self) -> bool {
        matches!(self, Self::Full)
    }

    /// Returns true when textual payloads should be emitted with redaction applied.
    pub fn captures_redacted_content(self) -> bool {
        matches!(self, Self::Redacted | Self::Full)
    }
}

/// A shared, mutable debug-level control.
#[derive(Clone, Default)]
pub struct DebugControl {
    level: Arc<RwLock<DebugCaptureLevel>>,
    run_levels: Arc<RwLock<BTreeMap<String, DebugCaptureLevel>>>,
}

impl DebugControl {
    /// Creates a debug control initialized with the provided level.
    pub fn new(level: DebugCaptureLevel) -> Self {
        Self {
            level: Arc::new(RwLock::new(level)),
            run_levels: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Returns the current debug level.
    pub fn level(&self) -> DebugCaptureLevel {
        *self.level.read()
    }

    /// Returns the capture level pinned to one run, pinning it lazily on first use.
    pub fn level_for_run(&self, run_id: Option<&str>) -> DebugCaptureLevel {
        let Some(run_id) = run_id.map(str::trim).filter(|run_id| !run_id.is_empty()) else {
            return self.level();
        };

        if let Some(level) = self.run_levels.read().get(run_id).copied() {
            return level;
        }

        let level = self.level();
        self.run_levels
            .write()
            .entry(run_id.to_string())
            .or_insert(level);
        level
    }

    /// Pins the current capture level for a run before runtime execution starts.
    pub fn pin_run_level(&self, run_id: &str, level: DebugCaptureLevel) {
        let run_id = run_id.trim();
        if run_id.is_empty() {
            return;
        }
        self.run_levels
            .write()
            .entry(run_id.to_string())
            .or_insert(level);
    }

    /// Updates the current debug level.
    pub fn set_level(&self, level: DebugCaptureLevel) {
        *self.level.write() = level;
    }

    /// Clears a run-level pin after the run has finished executing.
    pub fn clear_run_level(&self, run_id: &str) {
        self.run_levels.write().remove(run_id);
    }
}

/// Describes the serialization format of one persisted debug artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DebugArtifactFormat {
    Json,
    JsonLines,
}

/// Returns a provider payload prepared for the requested debug level.
pub fn provider_payload_for_level(level: DebugCaptureLevel, value: &Value) -> Value {
    match level {
        DebugCaptureLevel::Off => Value::Null,
        DebugCaptureLevel::On => summarize_json_value(value),
        DebugCaptureLevel::Redacted => {
            redact_audio_transcription_blocks_in_json(&redact_json_value(value))
        }
        DebugCaptureLevel::Full => redact_json_value(value),
    }
}

/// Returns a headers object prepared for the requested debug level.
pub fn headers_payload_for_level(level: DebugCaptureLevel, headers: &HeaderMap) -> Value {
    let redacted = redact_headers(headers);
    match level {
        DebugCaptureLevel::Off => Value::Null,
        DebugCaptureLevel::On => summarize_json_value(&redacted),
        DebugCaptureLevel::Redacted | DebugCaptureLevel::Full => redacted,
    }
}

/// Returns a textual payload prepared for the requested debug level.
pub fn text_payload_for_level(level: DebugCaptureLevel, text: &str) -> Value {
    match level {
        DebugCaptureLevel::Off => Value::Null,
        DebugCaptureLevel::On => summarize_text(text),
        DebugCaptureLevel::Redacted => {
            Value::String(redact_audio_transcription_blocks(&redact_text(text)))
        }
        DebugCaptureLevel::Full => Value::String(redact_text(text)),
    }
}

/// Returns one JSON payload redacted for debug capture at the requested level.
pub fn debug_json_payload_for_level(level: DebugCaptureLevel, value: &Value) -> Value {
    match level {
        DebugCaptureLevel::Off => Value::Null,
        DebugCaptureLevel::On => summarize_json_value(value),
        DebugCaptureLevel::Redacted => {
            redact_audio_transcription_blocks_in_json(&redact_json_value(value))
        }
        DebugCaptureLevel::Full => redact_json_value(value),
    }
}

/// Redacts one provider JSON payload by masking sensitive fields recursively.
pub fn redact_json_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut redacted = Map::new();
            let sibling_media_type = inline_media_type(object);
            for (key, value) in object {
                if is_sensitive_key(key) {
                    redacted.insert(key.clone(), Value::String("<redacted>".to_string()));
                } else if let Value::String(text) = value
                    && let Some(summary) = media_debug_summary(key, text, sibling_media_type)
                {
                    redacted.insert(key.clone(), summary);
                } else {
                    redacted.insert(key.clone(), redact_json_value(value));
                }
            }
            Value::Object(redacted)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact_json_value).collect()),
        Value::String(text) => Value::String(redact_text(text)),
        other => other.clone(),
    }
}

fn redact_audio_transcription_blocks_in_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        redact_audio_transcription_blocks_in_json(value),
                    )
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(redact_audio_transcription_blocks_in_json)
                .collect(),
        ),
        Value::String(text) => Value::String(redact_audio_transcription_blocks(text)),
        other => other.clone(),
    }
}

fn redact_audio_transcription_blocks(text: &str) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    if !lines
        .iter()
        .any(|line| is_audio_document_attachment_header(line))
    {
        return text.to_string();
    }

    let mut rendered = Vec::with_capacity(lines.len());
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index];
        rendered.push(line.to_string());
        index += 1;
        if !is_audio_document_attachment_header(line) {
            continue;
        }

        let block_start = index;
        while index < lines.len() && !is_document_attachment_header(lines[index]) {
            index += 1;
        }
        let block = lines[block_start..index].join("\n");
        if !block.trim().is_empty() {
            rendered.push(format!(
                "[Redacted audio transcription text: chars={}, lines={}, sha256={}]",
                block.chars().count(),
                block.lines().count(),
                digest_text(&block)
            ));
        }
    }
    let mut output = rendered.join("\n");
    if text.ends_with('\n') {
        output.push('\n');
    }
    output
}

fn is_audio_document_attachment_header(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("Document attachment:") && trimmed.contains("(audio/")
}

fn is_document_attachment_header(line: &str) -> bool {
    line.trim_start().starts_with("Document attachment:")
}

fn inline_media_type(object: &Map<String, Value>) -> Option<&str> {
    object
        .get("mimeType")
        .or_else(|| object.get("mime_type"))
        .and_then(Value::as_str)
        .filter(|media_type| media_type.starts_with("image/") || media_type.starts_with("audio/"))
}

fn media_debug_summary(key: &str, text: &str, sibling_media_type: Option<&str>) -> Option<Value> {
    if let Some((media_type, encoded)) = parse_media_data_url(text) {
        return Some(media_summary("data_url", Some(media_type), encoded));
    }
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    if normalized == "data"
        && let Some(media_type) = sibling_media_type
    {
        return Some(media_summary("base64", Some(media_type), text));
    }
    if normalized == "b64_json"
        || normalized == "base64"
        || normalized == "image_base64"
        || (normalized == "data" && looks_like_base64_media_payload(text))
    {
        return Some(media_summary("base64", None, text));
    }
    None
}

fn parse_media_data_url(text: &str) -> Option<(&str, &str)> {
    let raw = text.strip_prefix("data:")?;
    let (meta, payload) = raw.split_once(',')?;
    let media_type = meta.split(';').next().unwrap_or_default();
    if !media_type.starts_with("image/") && !media_type.starts_with("audio/") {
        return None;
    }
    Some((media_type, payload))
}

fn looks_like_base64_media_payload(text: &str) -> bool {
    text.len() >= 128
        && text
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '=' | '\n' | '\r'))
}

fn media_summary(kind: &str, media_type: Option<&str>, encoded: &str) -> Value {
    let compact = encoded
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect::<String>();
    let decoded_bytes = BASE64_STANDARD
        .decode(compact.as_bytes())
        .ok()
        .map(|bytes| bytes.len());
    let mut object = Map::new();
    object.insert(
        "redacted".to_string(),
        Value::String(format!("<redacted {kind}>")),
    );
    if let Some(media_type) = media_type {
        object.insert(
            "media_type".to_string(),
            Value::String(media_type.to_string()),
        );
    }
    object.insert(
        "encoded_chars".to_string(),
        Value::Number(serde_json::Number::from(encoded.len())),
    );
    if let Some(decoded_bytes) = decoded_bytes {
        object.insert(
            "decoded_bytes".to_string(),
            Value::Number(serde_json::Number::from(decoded_bytes)),
        );
    }
    object.insert(
        "encoded_sha256".to_string(),
        Value::String(digest_text(encoded)),
    );
    Value::Object(object)
}

/// Redacts one HTTP header map.
pub fn redact_headers(headers: &HeaderMap) -> Value {
    let mut object = Map::new();
    for (name, value) in headers {
        let key = name.as_str().to_string();
        let is_sensitive = is_sensitive_key(&key);
        let rendered = if is_sensitive {
            "<redacted>".to_string()
        } else {
            let value = value
                .to_str()
                .map(ToString::to_string)
                .unwrap_or_else(|_| "<binary>".to_string());
            if value == "<binary>" {
                value
            } else {
                redact_text(&value)
            }
        };
        object.insert(key, Value::String(rendered));
    }
    Value::Object(object)
}

/// Summarizes one JSON value into a stable digest, size, and coarse shape.
pub fn summarize_json_value(value: &Value) -> Value {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = digest_serialize(value).unwrap_or_else(|_| "unknown".to_string());
    json!({
        "digest": digest,
        "bytes": bytes.len(),
        "kind": json_value_kind(value),
    })
}

/// Summarizes one text payload into a stable digest and size.
pub fn summarize_text(text: &str) -> Value {
    let digest = digest_serialize(&text).unwrap_or_else(|_| "unknown".to_string());
    json!({
        "digest": digest,
        "bytes": text.len(),
        "lines": text.lines().count(),
    })
}

fn json_value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase();
    let separator_normalized = normalized.replace('-', "_");
    let compact_normalized = normalized
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>();
    if [
        "authorization",
        "api_key",
        "api-key",
        "x-api-key",
        "x_goog_api_key",
        "error-json",
        "cookie",
        "set-cookie",
        "proxy-authorization",
        "password",
        "passwd",
        "passphrase",
        "private_key",
        "privatekey",
        "credential",
    ]
    .iter()
    .any(|needle| normalized.contains(needle) || separator_normalized.contains(needle))
    {
        return true;
    }

    separator_normalized == "token"
        || separator_normalized.ends_with("_token")
        || separator_normalized.contains("access_token")
        || separator_normalized.contains("refresh_token")
        || separator_normalized.contains("id_token")
        || separator_normalized.contains("auth_token")
        || separator_normalized.contains("bearer_token")
        || compact_normalized == "token"
        || compact_normalized == "key"
        || compact_normalized == "apikey"
        || compact_normalized.ends_with("apikey")
        || compact_normalized.contains("accesstoken")
        || compact_normalized.contains("refreshtoken")
        || compact_normalized.contains("idtoken")
        || compact_normalized.contains("authtoken")
        || compact_normalized.contains("bearertoken")
        || compact_normalized.contains("oauthtoken")
        || compact_normalized.contains("clientsecret")
        || compact_normalized.contains("subscriptionkey")
        || separator_normalized.contains("secret")
        || separator_normalized.contains("password")
        || separator_normalized.contains("passwd")
        || separator_normalized.contains("passphrase")
        || separator_normalized.contains("private_key")
        || separator_normalized.contains("privatekey")
        || compact_normalized.contains("secret")
        || compact_normalized.contains("password")
        || compact_normalized.contains("passphrase")
        || compact_normalized.contains("privatekey")
        || separator_normalized.contains("credential")
        || compact_normalized.contains("credential")
}

/// Redacts secret-looking spans from one arbitrary text payload.
pub fn redact_text(text: &str) -> String {
    redact_configured_tokens(&redact_builtin_text(text))
}

fn redact_builtin_text(text: &str) -> String {
    let mut redacted = redact_assignment_values(text);
    for prefix in [
        "sk-ant-",
        "sk-proj-",
        "sk-",
        "ghp_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "AKIA",
    ] {
        redacted = redact_prefixed_token(&redacted, prefix);
    }
    let redacted = redact_bearer_tokens(&redact_bearer_tokens(&redacted, "Bearer "), "bearer ");
    let redacted = redact_jwt_tokens(&redacted);
    let redacted = redact_pem_private_key_blocks(&redacted);
    redact_sensitive_url_query_values(&redacted)
}

#[cfg(test)]
fn redact_header_value_with_configured_tokens(
    text: &str,
    tokens: Result<Vec<String>, String>,
) -> String {
    redact_with_configured_tokens(&redact_builtin_text(text), tokens)
}

/// Returns the effective status of configured debug redaction extensions.
pub fn debug_redaction_config_status() -> DebugRedactionConfigStatus {
    match configured_redaction_tokens() {
        Ok(tokens) => DebugRedactionConfigStatus {
            literal_token_count: tokens.len(),
            token_file_configured: std::env::var_os(DEBUG_REDACT_TOKENS_FILE_ENV).is_some(),
            token_file_error: None,
        },
        Err(error) => DebugRedactionConfigStatus {
            literal_token_count: 0,
            token_file_configured: true,
            token_file_error: Some(error),
        },
    }
}

/// Returns a debug redaction configuration error when redacted/full capture would fail closed.
pub fn debug_redaction_config_error() -> Option<String> {
    debug_redaction_config_status().token_file_error
}

fn redact_assignment_values(text: &str) -> String {
    let mut rendered = String::with_capacity(text.len());
    for fragment in split_preserving(text, '\n') {
        if let Some(redacted) = redact_assignment_fragment(fragment) {
            rendered.push_str(&redacted);
        } else {
            rendered.push_str(fragment);
        }
    }
    rendered
}

fn redact_assignment_fragment(fragment: &str) -> Option<String> {
    let separator = first_assignment_separator(fragment)?;
    let (name, value_with_separator) = fragment.split_at(separator);
    let separator = value_with_separator.chars().next()?;
    if separator == ':' && is_authorization_key(name.trim()) {
        return None;
    }
    if !is_sensitive_key(name.trim()) {
        return None;
    }
    let value = &value_with_separator[separator.len_utf8()..];
    let (value, trailing_newline) = value
        .strip_suffix('\n')
        .map_or((value, false), |value| (value, true));
    let leading_ws_len = value
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(index, _)| index)
        .unwrap_or(value.len());
    let leading_ws = &value[..leading_ws_len];
    let value_body = &value[leading_ws_len..];
    let mut rendered = String::with_capacity(fragment.len());
    rendered.push_str(name);
    rendered.push(separator);
    rendered.push_str(leading_ws);
    rendered.push_str(mask_token(value_body.trim_end()));
    if trailing_newline {
        rendered.push('\n');
    }
    Some(rendered)
}

fn first_assignment_separator(fragment: &str) -> Option<usize> {
    let equals = fragment.find('=');
    let colon = fragment.find(':');
    match (equals, colon) {
        (Some(equals), Some(colon)) => Some(equals.min(colon)),
        (Some(index), None) | (None, Some(index)) => Some(index),
        (None, None) => None,
    }
}

fn is_authorization_key(key: &str) -> bool {
    key.trim_matches(['"', '\'', '\\'])
        .eq_ignore_ascii_case("authorization")
}

fn redact_bearer_tokens(text: &str, needle: &str) -> String {
    let mut rendered = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find(needle) {
        let start = cursor + relative;
        rendered.push_str(&text[cursor..start]);
        rendered.push_str(needle);
        let token_start = start + needle.len();
        let token_end = text[token_start..]
            .find(is_token_delimiter)
            .map(|offset| token_start + offset)
            .unwrap_or(text.len());
        rendered.push_str(mask_token(&text[token_start..token_end]));
        cursor = token_end;
    }
    if cursor < text.len() {
        rendered.push_str(&text[cursor..]);
    }
    rendered
}

fn redact_jwt_tokens(text: &str) -> String {
    let mut rendered = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find("eyJ") {
        let start = cursor + relative;
        if start > 0 {
            let previous = text[..start].chars().next_back().unwrap_or_default();
            if !is_token_delimiter(previous) {
                rendered.push_str(&text[cursor..start + 3]);
                cursor = start + 3;
                continue;
            }
        }
        let end = text[start..]
            .find(is_token_delimiter)
            .map(|offset| start + offset)
            .unwrap_or(text.len());
        let token = &text[start..end];
        if looks_like_jwt(token) || looks_like_labelled_jwt_fragment(text, start, token) {
            rendered.push_str(&text[cursor..start]);
            rendered.push_str("<redacted>");
            cursor = end;
        } else {
            rendered.push_str(&text[cursor..start + 3]);
            cursor = start + 3;
        }
    }
    rendered.push_str(&text[cursor..]);
    rendered
}

fn looks_like_jwt(token: &str) -> bool {
    let mut parts = token.split('.');
    let Some(header) = parts.next() else {
        return false;
    };
    let Some(payload) = parts.next() else {
        return false;
    };
    let Some(signature) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && header.starts_with("eyJ")
        && [header, payload, signature]
            .iter()
            .all(|part| part.len() >= 8 && part.chars().all(is_jwt_char))
}

fn looks_like_labelled_jwt_fragment(text: &str, token_start: usize, token: &str) -> bool {
    if token.len() < 12
        || !token.starts_with("eyJ")
        || !token.chars().all(|ch| is_jwt_char(ch) || ch == '.')
    {
        return false;
    }
    let prefix_start = text[..token_start]
        .char_indices()
        .rev()
        .nth(32)
        .map(|(index, _)| index)
        .unwrap_or(0);
    let prefix = text[prefix_start..token_start]
        .trim_end_matches(|ch: char| ch.is_whitespace() || matches!(ch, ':' | '='))
        .to_ascii_lowercase();
    prefix.ends_with("jwt")
        || prefix.ends_with("id_token")
        || prefix.ends_with("access_token")
        || prefix.ends_with("refresh_token")
}

fn is_jwt_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')
}

fn redact_pem_private_key_blocks(text: &str) -> String {
    let mut rendered = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find("-----BEGIN ") {
        let start = cursor + relative;
        let header_end = text[start..]
            .find('\n')
            .map(|offset| start + offset)
            .unwrap_or(text.len());
        let header = &text[start..header_end];
        if !header.contains("PRIVATE KEY-----") {
            rendered.push_str(&text[cursor..header_end]);
            cursor = header_end;
            continue;
        }
        let Some(end_relative) = text[header_end..].find("-----END ") else {
            rendered.push_str(&text[cursor..start]);
            rendered.push_str("<redacted:private-key>");
            cursor = text.len();
            break;
        };
        let end_start = header_end + end_relative;
        let end_line_end = text[end_start..]
            .find('\n')
            .map(|offset| end_start + offset + 1)
            .unwrap_or(text.len());
        let end_line = &text[end_start..end_line_end];
        if !end_line.contains("PRIVATE KEY-----") {
            rendered.push_str(&text[cursor..header_end]);
            cursor = header_end;
            continue;
        }
        rendered.push_str(&text[cursor..start]);
        rendered.push_str("<redacted:private-key>");
        cursor = end_line_end;
    }
    rendered.push_str(&text[cursor..]);
    rendered
}

fn redact_sensitive_url_query_values(text: &str) -> String {
    let mut rendered = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some((start, name_len)) = next_sensitive_url_query_param(text, cursor) {
        let value_start = start + 1 + name_len + 1;
        let value_end = text[value_start..]
            .find(is_url_query_value_delimiter)
            .map(|offset| value_start + offset)
            .unwrap_or(text.len());
        rendered.push_str(&text[cursor..value_start]);
        if value_end > value_start {
            rendered.push_str("<redacted>");
        }
        cursor = value_end;
    }
    rendered.push_str(&text[cursor..]);
    rendered
}

fn next_sensitive_url_query_param(text: &str, cursor: usize) -> Option<(usize, usize)> {
    for (offset, ch) in text[cursor..].char_indices() {
        if !matches!(ch, '?' | '&') {
            continue;
        }
        let start = cursor + offset;
        let rest = &text[start + 1..];
        let Some(name_len) = rest.find('=').filter(|index| *index > 0).filter(|index| {
            !rest[..*index]
                .chars()
                .any(|candidate| is_url_query_value_delimiter(candidate) || candidate == '?')
        }) else {
            continue;
        };
        let name = &rest[..name_len];
        if is_sensitive_url_query_key(name) {
            return Some((start, name_len));
        }
    }
    None
}

fn is_sensitive_url_query_key(key: &str) -> bool {
    let compact = key
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>();
    matches!(compact.as_str(), "key" | "sig" | "signature" | "code") || is_sensitive_key(key)
}

fn is_url_query_value_delimiter(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '&' | '"' | '\'' | '<' | '>' | ')' | ']' | '}')
}

fn redact_prefixed_token(text: &str, prefix: &str) -> String {
    let mut rendered = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find(prefix) {
        let start = cursor + relative;
        if start > 0 {
            let previous = text[..start].chars().next_back().unwrap_or_default();
            if !is_token_delimiter(previous) {
                rendered.push_str(&text[cursor..start + prefix.len()]);
                cursor = start + prefix.len();
                continue;
            }
        }
        rendered.push_str(&text[cursor..start]);
        let end = text[start..]
            .find(is_token_delimiter)
            .map(|offset| start + offset)
            .unwrap_or(text.len());
        rendered.push_str(mask_token(&text[start..end]));
        cursor = end;
    }
    rendered.push_str(&text[cursor..]);
    rendered
}

fn mask_token(token: &str) -> &str {
    if token.is_empty() { "" } else { "<redacted>" }
}

fn redact_configured_tokens(text: &str) -> String {
    redact_with_configured_tokens(text, configured_redaction_tokens())
}

fn redact_with_configured_tokens(text: &str, tokens: Result<Vec<String>, String>) -> String {
    let tokens = match tokens {
        Ok(tokens) => tokens,
        Err(error) => {
            warn!(
                error = %error,
                "debug redaction extension failed; redacting entire text payload"
            );
            return "<redacted:debug-redaction-config-error>".to_string();
        }
    };
    let mut tokens = tokens;
    tokens.sort_by(|left, right| {
        right
            .chars()
            .count()
            .cmp(&left.chars().count())
            .then_with(|| left.cmp(right))
    });
    let mut redacted = text.to_string();
    for token in tokens {
        redacted = redacted.replace(&token, "<redacted>");
    }
    redacted
}

fn configured_redaction_tokens() -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    if let Some(raw) = std::env::var_os(DEBUG_REDACT_TOKENS_ENV) {
        tokens.extend(parse_redaction_tokens(&raw.to_string_lossy()));
    }
    if let Some(path) = std::env::var_os(DEBUG_REDACT_TOKENS_FILE_ENV) {
        let path = PathBuf::from(path);
        match std::fs::read_to_string(&path) {
            Ok(raw) => tokens.extend(parse_redaction_tokens(&raw)),
            Err(error) => {
                let message = format!(
                    "failed to read debug redaction token file {}: {error}",
                    path.display()
                );
                warn_redaction_token_file_error(path, error);
                return Err(message);
            }
        }
    }
    tokens.extend(kheish_auth::debug_redaction_tokens());
    tokens.sort();
    tokens.dedup();
    Ok(tokens)
}

fn warn_redaction_token_file_error(path: PathBuf, error: std::io::Error) {
    static WARNED_PATHS: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();
    let warned_paths = WARNED_PATHS.get_or_init(|| Mutex::new(BTreeSet::new()));
    let mut warned_paths = warned_paths.lock();
    if warned_paths.insert(path.clone()) {
        warn!(
            path = %path.display(),
            error = ?error,
            "failed to read debug redaction token file; configured custom tokens were not applied"
        );
    }
}

fn parse_redaction_tokens(raw: &str) -> impl Iterator<Item = String> + '_ {
    raw.split([',', '\n'])
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

fn split_preserving(text: &str, delimiter: char) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    for (index, ch) in text.char_indices() {
        if ch == delimiter {
            segments.push(&text[start..=index]);
            start = index + ch.len_utf8();
        }
    }
    if start < text.len() {
        segments.push(&text[start..]);
    }
    if segments.is_empty() {
        segments.push(text);
    }
    segments
}

fn is_token_delimiter(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '"' | '\'' | ',' | ';' | ')' | '(' | ']' | '[' | '}' | '{'
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use reqwest::header::{HeaderMap, HeaderValue};
    use serde_json::json;
    use std::sync::OnceLock;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn redacted_text_masks_sensitive_assignments_and_tokens() {
        let payload = text_payload_for_level(
            DebugCaptureLevel::Redacted,
            "ANTHROPIC_API_KEY=anthropic-secret-value\nx-api-key: project-secret-value\n\"clientSecret\": \"json-secret-value\"\npassword=hunter2\npassphrase: swordfish\nprivate_key=inline-key\ncredential=tenant-credential\nAuthorization: Bearer abc123\nplain text",
        );
        let rendered = payload.as_str().expect("redacted text");
        assert!(rendered.contains("ANTHROPIC_API_KEY=<redacted>"));
        assert!(rendered.contains("x-api-key: <redacted>"));
        assert!(rendered.contains("\"clientSecret\": <redacted>"));
        assert!(rendered.contains("password=<redacted>"));
        assert!(rendered.contains("passphrase: <redacted>"));
        assert!(rendered.contains("private_key=<redacted>"));
        assert!(rendered.contains("credential=<redacted>"));
        assert!(rendered.contains("Bearer <redacted>"));
        assert!(rendered.contains("plain text"));
        assert!(!rendered.contains("anthropic-secret-value"));
        assert!(!rendered.contains("project-secret-value"));
        assert!(!rendered.contains("json-secret-value"));
        assert!(!rendered.contains("hunter2"));
        assert!(!rendered.contains("swordfish"));
        assert!(!rendered.contains("inline-key"));
        assert!(!rendered.contains("tenant-credential"));
    }

    #[test]
    fn full_text_still_masks_credentials() {
        let payload = text_payload_for_level(
            DebugCaptureLevel::Full,
            "authorization: bearer lower-secret\nAuthorization: Bearer upper-secret\nplain text",
        );
        let rendered = payload.as_str().expect("full text");
        assert!(rendered.contains("authorization: bearer <redacted>"));
        assert!(rendered.contains("Authorization: Bearer <redacted>"));
        assert!(rendered.contains("plain text"));
        assert!(!rendered.contains("lower-secret"));
        assert!(!rendered.contains("upper-secret"));
    }

    #[test]
    fn redacted_text_masks_jwt_private_keys_and_url_query_secrets() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.sflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let private_key_label = "PRIVATE KEY";
        let private_key_body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC";
        let private_key = format!(
            "-----BEGIN {private_key_label}-----\n{private_key_body}\n-----END {private_key_label}-----"
        );
        let payload = text_payload_for_level(
            DebugCaptureLevel::Full,
            &format!(
                "jwt {jwt}\ninput jwt eyJhbGciOiJIUzI1NiJ9.eyJpbnB1dCI6InNlY3JldCJ9.inputsig123, webhook https://hooks.example.test/incoming?token=hook-secret&safe=1&signature=sig-secret&access_token=oauth-secret&client_secret=client-secret&code=auth-code&password=hunter2&passphrase=swordfish&private_key=inline-key&credential=tenant-credential\n{private_key}\nplain text"
            ),
        );
        let rendered = payload.as_str().expect("full text");
        assert!(rendered.contains("jwt <redacted>"));
        assert!(rendered.contains("token=<redacted>"));
        assert!(rendered.contains("safe=1"));
        assert!(rendered.contains("signature=<redacted>"));
        assert!(rendered.contains("access_token=<redacted>"));
        assert!(rendered.contains("client_secret=<redacted>"));
        assert!(rendered.contains("code=<redacted>"));
        assert!(rendered.contains("password=<redacted>"));
        assert!(rendered.contains("passphrase=<redacted>"));
        assert!(rendered.contains("private_key=<redacted>"));
        assert!(rendered.contains("credential=<redacted>"));
        assert!(rendered.contains("<redacted:private-key>"));
        assert!(rendered.contains("plain text"));
        assert!(!rendered.contains(jwt));
        assert!(!rendered.contains("eyJpbnB1dCI6InNlY3JldCJ9"));
        assert!(!rendered.contains("hook-secret"));
        assert!(!rendered.contains("sig-secret"));
        assert!(!rendered.contains("oauth-secret"));
        assert!(!rendered.contains("client-secret"));
        assert!(!rendered.contains("auth-code"));
        assert!(!rendered.contains("hunter2"));
        assert!(!rendered.contains("swordfish"));
        assert!(!rendered.contains("inline-key"));
        assert!(!rendered.contains("tenant-credential"));
        assert!(!rendered.contains("MIIEvQIBADAN"));
    }

    #[test]
    fn redacted_text_masks_truncated_pem_private_key_and_partial_labelled_jwt() {
        let private_key_label = "PRIVATE KEY";
        let payload = text_payload_for_level(
            DebugCaptureLevel::Full,
            &format!(
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0\n-----BEGIN {private_key_label}-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC"
            ),
        );
        let rendered = payload.as_str().expect("full text");
        assert!(rendered.contains("jwt <redacted>"));
        assert!(rendered.contains("<redacted:private-key>"));
        assert!(!rendered.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(!rendered.contains("MIIEvQIBADAN"));
    }

    #[test]
    fn redacted_json_masks_strings_without_sensitive_keys() {
        let value = redact_json_value(&json!({
            "message": "token sk-proj-secret-value",
            "nested": ["Authorization: Bearer top-secret"],
            "password": "hunter2",
            "passphrase": "swordfish",
            "privateKey": "inline-key",
            "credentials": {
                "username": "visible-user",
                "value": "tenant-credential"
            },
        }));
        assert_eq!(value["message"], "token <redacted>");
        assert_eq!(value["nested"][0], "Authorization: Bearer <redacted>");
        assert_eq!(value["password"], "<redacted>");
        assert_eq!(value["passphrase"], "<redacted>");
        assert_eq!(value["privateKey"], "<redacted>");
        assert_eq!(value["credentials"], "<redacted>");
    }

    #[test]
    fn redacted_json_masks_camel_case_secret_keys_and_url_params() {
        let value = redact_json_value(&json!({
            "apiKey": "api-secret",
            "xGoogApiKey": "google-secret",
            "accessToken": "access-secret",
            "refreshToken": "refresh-secret",
            "idToken": "id-secret",
            "oauthToken": "oauth-secret",
            "clientSecret": "client-secret",
            "subscriptionKey": "subscription-secret",
            "privateKey": "private-secret",
            "message": "callback https://example.test/cb?accessToken=url-access&safe=visible&clientSecret=url-client&subscriptionKey=url-subscription&privateKey=url-private&apiKey=url-api",
            "keyboard": "visible",
        }));
        let rendered = value.to_string();

        assert_eq!(value["apiKey"], "<redacted>");
        assert_eq!(value["xGoogApiKey"], "<redacted>");
        assert_eq!(value["accessToken"], "<redacted>");
        assert_eq!(value["refreshToken"], "<redacted>");
        assert_eq!(value["idToken"], "<redacted>");
        assert_eq!(value["oauthToken"], "<redacted>");
        assert_eq!(value["clientSecret"], "<redacted>");
        assert_eq!(value["subscriptionKey"], "<redacted>");
        assert_eq!(value["privateKey"], "<redacted>");
        assert_eq!(value["keyboard"], "visible");
        assert!(rendered.contains("safe=visible"));
        for secret in [
            "api-secret",
            "google-secret",
            "access-secret",
            "refresh-secret",
            "id-secret",
            "oauth-secret",
            "client-secret",
            "subscription-secret",
            "private-secret",
            "url-access",
            "url-client",
            "url-subscription",
            "url-private",
            "url-api",
        ] {
            assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
        }
    }

    #[test]
    fn full_provider_payload_still_masks_credentials() {
        let value = provider_payload_for_level(
            DebugCaptureLevel::Full,
            &json!({
                "api_key": "sk-secret",
                "max_output_tokens": 4096,
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 20
                },
                "message": "Authorization: Bearer raw-token",
                "plain": "visible",
            }),
        );
        assert_eq!(value["api_key"], "<redacted>");
        assert_eq!(value["max_output_tokens"], 4096);
        assert_eq!(value["usage"]["input_tokens"], 10);
        assert_eq!(value["usage"]["output_tokens"], 20);
        assert_eq!(value["message"], "Authorization: Bearer <redacted>");
        assert_eq!(value["plain"], "visible");
    }

    #[test]
    fn provider_payload_redacts_media_base64_even_at_full_level() {
        let encoded_image = "a".repeat(256);
        let data_url = format!("data:image/png;base64,{encoded_image}");
        let value = provider_payload_for_level(
            DebugCaptureLevel::Full,
            &json!({
                "data": [
                    {
                        "b64_json": encoded_image,
                        "image_url": {
                            "url": data_url
                        }
                    }
                ],
                "inlineData": {
                    "mimeType": "image/png",
                    "data": "b".repeat(256)
                }
            }),
        );

        let rendered = value.to_string();
        assert!(!rendered.contains(&"a".repeat(128)));
        assert!(!rendered.contains(&"b".repeat(128)));
        assert_eq!(
            value["data"][0]["b64_json"]["redacted"],
            "<redacted base64>"
        );
        assert_eq!(
            value["data"][0]["image_url"]["url"]["redacted"],
            "<redacted data_url>"
        );
        assert_eq!(
            value["data"][0]["image_url"]["url"]["media_type"],
            "image/png"
        );
        assert_eq!(value["inlineData"]["data"]["redacted"], "<redacted base64>");

        let tiny = provider_payload_for_level(
            DebugCaptureLevel::Full,
            &json!({
                "inlineData": {
                    "mimeType": "image/png",
                    "data": BASE64_STANDARD.encode(b"tiny")
                }
            }),
        );
        assert_eq!(tiny["inlineData"]["data"]["redacted"], "<redacted base64>");
        assert_eq!(tiny["inlineData"]["data"]["media_type"], "image/png");
    }

    #[test]
    fn redacted_payload_summarizes_audio_transcription_attachment_text() {
        let value = debug_json_payload_for_level(
            DebugCaptureLevel::Redacted,
            &json!({
                "input": [
                    {
                        "role": "user",
                        "content": "Please inspect this.\nDocument attachment: call.wav (audio/wav)\nAUDIO_SECRET_SENTINEL_123\nDocument attachment: note.txt (text/plain)\nVISIBLE_NOTE"
                    }
                ]
            }),
        );
        let rendered = value.to_string();

        assert!(!rendered.contains("AUDIO_SECRET_SENTINEL_123"));
        assert!(rendered.contains("Redacted audio transcription text"));
        assert!(rendered.contains("Document attachment: call.wav (audio/wav)"));
        assert!(rendered.contains("Document attachment: note.txt (text/plain)"));
        assert!(rendered.contains("VISIBLE_NOTE"));
    }

    #[test]
    fn full_payload_keeps_non_secret_audio_transcription_text() {
        let value = debug_json_payload_for_level(
            DebugCaptureLevel::Full,
            &json!({
                "content": "Document attachment: call.wav (audio/wav)\nAUDIO_TRANSCRIPT_VISIBLE_IN_FULL"
            }),
        );

        assert!(
            value
                .to_string()
                .contains("AUDIO_TRANSCRIPT_VISIBLE_IN_FULL")
        );
    }

    #[test]
    fn redacted_headers_mask_error_payload_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-error-json", HeaderValue::from_static("super-secret"));
        headers.insert("x-goog-api-key", HeaderValue::from_static("google-secret"));
        headers.insert(
            "x-provider-api-key",
            HeaderValue::from_static("provider-secret"),
        );
        let value = redact_headers(&headers);
        assert_eq!(value["x-error-json"], "<redacted>");
        assert_eq!(value["x-goog-api-key"], "<redacted>");
        assert_eq!(value["x-provider-api-key"], "<redacted>");
    }

    #[test]
    fn redacted_headers_apply_configured_token_fail_closed() {
        let rendered = redact_with_configured_tokens(
            "tenant-secret",
            Err("failed to read debug redaction token file".to_string()),
        );
        assert_eq!(rendered, "<redacted:debug-redaction-config-error>");
        let header_rendered = redact_header_value_with_configured_tokens(
            "tenant-secret",
            Err("failed to read debug redaction token file".to_string()),
        );
        assert_eq!(header_rendered, "<redacted:debug-redaction-config-error>");
    }

    #[test]
    fn debug_control_pins_level_for_run_until_cleared() {
        let control = DebugControl::new(DebugCaptureLevel::Redacted);

        assert_eq!(
            control.level_for_run(Some("run-a")),
            DebugCaptureLevel::Redacted
        );
        control.set_level(DebugCaptureLevel::Full);
        assert_eq!(
            control.level_for_run(Some("run-a")),
            DebugCaptureLevel::Redacted
        );
        assert_eq!(
            control.level_for_run(Some("run-b")),
            DebugCaptureLevel::Full
        );
        control.clear_run_level("run-a");
        assert_eq!(
            control.level_for_run(Some("run-a")),
            DebugCaptureLevel::Full
        );
    }

    #[test]
    fn redaction_can_be_extended_with_operator_tokens() {
        let _guard = env_lock().lock();
        unsafe {
            std::env::set_var(DEBUG_REDACT_TOKENS_ENV, "tenant-secret, another-secret");
            std::env::remove_var(DEBUG_REDACT_TOKENS_FILE_ENV);
        }

        let rendered = redact_text("visible tenant-secret and another-secret");

        unsafe {
            std::env::remove_var(DEBUG_REDACT_TOKENS_ENV);
        }
        assert_eq!(rendered, "visible <redacted> and <redacted>");
    }

    #[test]
    fn redaction_includes_auth_managed_tokens() {
        let _guard = env_lock().lock();
        unsafe {
            std::env::remove_var(DEBUG_REDACT_TOKENS_ENV);
            std::env::remove_var(DEBUG_REDACT_TOKENS_FILE_ENV);
        }
        kheish_auth::replace_auth_store_debug_redaction_tokens(vec![
            "opaque-auth-canary".to_string(),
        ]);

        let rendered = redact_text("tool output leaked opaque-auth-canary");

        kheish_auth::replace_auth_store_debug_redaction_tokens(Vec::<String>::new());
        assert_eq!(rendered, "tool output leaked <redacted>");
    }

    #[test]
    fn redaction_token_file_failure_fails_closed() {
        let rendered = redact_with_configured_tokens(
            "visible tenant-secret",
            Err("failed to read debug redaction token file".to_string()),
        );
        assert_eq!(rendered, "<redacted:debug-redaction-config-error>");
    }

    #[test]
    fn redaction_prefers_longest_configured_tokens() {
        let rendered = redact_with_configured_tokens(
            "rotated secret opaque-token-v2",
            Ok(vec![
                "opaque-token".to_string(),
                "opaque-token-v2".to_string(),
            ]),
        );
        assert_eq!(rendered, "rotated secret <redacted>");
    }
}
