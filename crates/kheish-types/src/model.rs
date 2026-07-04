use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable metadata key used to carry run completion requirements.
pub const COMPLETION_REQUIREMENTS_METADATA_KEY: &str = "completion_requirements";
/// Stable metadata key carrying the structured output contract of a run.
pub const STRUCTURED_OUTPUT_CONTRACT_METADATA_KEY: &str = "structured_output_contract";
/// Claude Code-style capped default output token ceiling.
pub const CAPPED_DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8_000;
/// Claude Code-style escalated output token ceiling used for recovery.
pub const ESCALATED_MAX_OUTPUT_TOKENS: u32 = 64_000;

/// Coarse provider failure category used by the core engine.
///
/// Providers keep their native error payloads, but the engine must not depend on
/// brittle provider-specific strings to decide whether a request exhausted the
/// active context window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    ContextWindowExceeded,
    RateLimited,
    Auth,
    InvalidRequest,
    ServerOverloaded,
    Transport,
    #[default]
    Unknown,
}

/// Provider error type shared across the runtime/core boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
    pub retryable: bool,
    pub retry_after_ms: Option<u64>,
}

impl ModelProviderError {
    pub fn new(
        kind: ProviderErrorKind,
        message: impl Into<String>,
        retryable: bool,
        retry_after_ms: Option<u64>,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable,
            retry_after_ms,
        }
    }

    pub fn from_message(
        message: impl Into<String>,
        retryable: bool,
        retry_after_ms: Option<u64>,
    ) -> Self {
        let message = message.into();
        Self::new(
            classify_provider_error_message(&message),
            message,
            retryable,
            retry_after_ms,
        )
    }
}

impl Display for ModelProviderError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ModelProviderError {}

/// Classifies provider messages into stable engine-level error kinds.
pub fn classify_provider_error_message(message: &str) -> ProviderErrorKind {
    let message = message.to_ascii_lowercase();
    let is_media_413 = message.contains("status 413") || message.contains("http error 413");
    let is_media_payload = message.contains("attachment")
        || message.contains("image")
        || message.contains("pdf")
        || message.contains("media");
    if message.contains("context_length_exceeded")
        || message.contains("context_window_exceeded")
        || message.contains("model_context_window_exceeded")
        || message.contains("context window exceeded")
        || message.contains("maximum context length")
        || message.contains("context length")
        || message.contains("context limit")
        || message.contains("prompt too long")
        || message.contains("prompt is too long")
        || message.contains("too many tokens")
        || message.contains("token count exceeds")
        || message.contains("exceeds the model context")
        || (is_media_413 && !is_media_payload)
    {
        return ProviderErrorKind::ContextWindowExceeded;
    }
    if message.contains("rate limit")
        || message.contains("rate_limit")
        || message.contains("too many requests")
        || message.contains("status 429")
        || message.contains("http error 429")
    {
        return ProviderErrorKind::RateLimited;
    }
    if message.contains("invalid api key")
        || message.contains("authentication")
        || message.contains("unauthorized")
        || message.contains("forbidden")
        || message.contains("status 401")
        || message.contains("status 403")
        || message.contains("http error 401")
        || message.contains("http error 403")
    {
        return ProviderErrorKind::Auth;
    }
    if message.contains("overloaded")
        || message.contains("temporarily unavailable")
        || message.contains("status 503")
        || message.contains("status 529")
        || message.contains("http error 503")
        || message.contains("http error 529")
    {
        return ProviderErrorKind::ServerOverloaded;
    }
    if message.contains("timeout")
        || message.contains("timed out")
        || message.contains("inactive")
        || message.contains("transport")
        || message.contains("connection")
        || message.contains("network")
    {
        return ProviderErrorKind::Transport;
    }
    if message.contains("invalid_request_error")
        || message.contains("invalid request")
        || message.contains("bad request")
        || message.contains("status 400")
        || message.contains("http error 400")
    {
        return ProviderErrorKind::InvalidRequest;
    }
    ProviderErrorKind::Unknown
}

/// Default and upper-limit output token settings for one model family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelMaxOutputTokens {
    pub default: u32,
    pub upper_limit: u32,
}

/// Returns the best-known context window for a model identifier.
///
/// The values are intentionally conservative and only cover families that the
/// daemon already routes in normal operation. Unknown models return `None` so
/// callers can fall back to safer runtime heuristics.
pub fn model_context_window(model: &str) -> Option<usize> {
    let canonical = model.to_lowercase();
    if canonical.starts_with("claude-") {
        return Some(200_000);
    }
    if canonical.starts_with("gemini-2.5-") {
        return Some(1_048_576);
    }
    if canonical.starts_with("gemini-3") || canonical.contains("image-preview") {
        return Some(65_536);
    }
    if canonical.starts_with("gpt-5") {
        return Some(400_000);
    }
    if canonical.starts_with("gpt-4.1") {
        return Some(1_047_576);
    }
    if canonical.starts_with("gpt-4o") {
        return Some(128_000);
    }
    None
}

/// Returns native output token defaults and upper bounds for a model identifier.
pub fn model_max_output_tokens(model: &str) -> ModelMaxOutputTokens {
    let canonical = model.to_lowercase();
    let (default, upper_limit) = if canonical.contains("opus-4-6") {
        (64_000, 128_000)
    } else if canonical.contains("sonnet-4-6") {
        (32_000, 128_000)
    } else if canonical.contains("opus-4-5")
        || canonical.contains("sonnet-4")
        || canonical.contains("haiku-4")
    {
        (32_000, 64_000)
    } else if canonical.contains("opus-4-1") || canonical.contains("opus-4") {
        (32_000, 32_000)
    } else if canonical.contains("claude-3-opus") {
        (4_096, 4_096)
    } else if canonical.contains("claude-3-sonnet")
        || canonical.contains("3-5-sonnet")
        || canonical.contains("3-5-haiku")
    {
        (8_192, 8_192)
    } else if canonical.contains("claude-3-haiku") {
        (4_096, 4_096)
    } else if canonical.contains("3-7-sonnet") {
        (32_000, 64_000)
    } else if canonical.starts_with("gemini-2.5-") {
        (65_536, 65_536)
    } else if canonical.starts_with("gemini-3") || canonical.contains("image-preview") {
        (32_768, 32_768)
    } else {
        (
            CAPPED_DEFAULT_MAX_OUTPUT_TOKENS,
            ESCALATED_MAX_OUTPUT_TOKENS,
        )
    };
    ModelMaxOutputTokens {
        default,
        upper_limit: upper_limit.max(default),
    }
}

/// Returns the Claude Code-style capped default output token ceiling for a model identifier.
pub fn capped_default_max_output_tokens(model: &str) -> u32 {
    model_max_output_tokens(model)
        .default
        .min(CAPPED_DEFAULT_MAX_OUTPUT_TOKENS)
}

/// Captures token and cost usage for one model turn or stream snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

/// Provider-reported usage attached to one persisted assistant message.
pub type ApiUsage = ModelUsage;

/// Normalized finish reasons returned by model providers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFinishReason {
    Completed,
    ToolCalls,
    MaxTokens,
    StopSequence,
    Blocked,
    Cancelled,
    Unknown,
}

impl ModelFinishReason {
    /// Returns the stable snake_case representation used in snapshots and parity checks.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::ToolCalls => "tool_calls",
            Self::MaxTokens => "max_tokens",
            Self::StopSequence => "stop_sequence",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

/// Selects how a model is allowed or required to choose tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Specific { name: String },
}

impl Default for ToolChoice {
    fn default() -> Self {
        Self::Auto
    }
}

/// Describes the supported structured output value kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredValueKind {
    /// Any JSON value.
    Any,
    /// A JSON string.
    String,
    /// A JSON number.
    Number,
    /// A JSON boolean.
    Boolean,
    /// A JSON object.
    Object,
    /// A JSON array.
    Array,
}

/// A recursive structured-output schema used by runtime-side validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredFieldSchema {
    /// The JSON value kind accepted by this schema node.
    pub kind: StructuredValueKind,
    /// Required object fields keyed by property name.
    #[serde(default)]
    pub fields: BTreeMap<String, StructuredFieldSchema>,
    /// Optional object fields keyed by property name.
    #[serde(default)]
    pub optional_fields: BTreeMap<String, StructuredFieldSchema>,
    /// Array item schema when `kind` is `StructuredValueKind::Array`.
    pub items: Option<Box<StructuredFieldSchema>>,
}

impl StructuredFieldSchema {
    /// Creates an unconstrained schema node for the provided kind.
    pub fn new(kind: StructuredValueKind) -> Self {
        Self {
            kind,
            fields: BTreeMap::new(),
            optional_fields: BTreeMap::new(),
            items: None,
        }
    }

    /// Validates a JSON value against this schema, reporting the JSON path
    /// of the first mismatch (for example `$.items[2].price: expected a
    /// number`). The path quality matters: it is fed back verbatim to the
    /// model as repair guidance.
    pub fn validate_value(&self, value: &Value) -> Result<(), String> {
        validate_value_at(self, value, "$")
    }

    /// Renders this schema as a plain JSON Schema fragment. Objects are
    /// closed (`additionalProperties: false`) and required fields listed,
    /// matching what the validator actually enforces.
    pub fn to_json_schema(&self) -> Value {
        match self.kind {
            StructuredValueKind::Any => serde_json::json!({}),
            StructuredValueKind::String => serde_json::json!({"type": "string"}),
            StructuredValueKind::Number => serde_json::json!({"type": "number"}),
            StructuredValueKind::Boolean => serde_json::json!({"type": "boolean"}),
            StructuredValueKind::Object => {
                let mut properties = serde_json::Map::new();
                let mut required = Vec::new();
                for (name, field_schema) in &self.fields {
                    properties.insert(name.clone(), field_schema.to_json_schema());
                    required.push(Value::String(name.clone()));
                }
                for (name, field_schema) in &self.optional_fields {
                    properties.insert(name.clone(), field_schema.to_json_schema());
                }
                serde_json::json!({
                    "type": "object",
                    "properties": properties,
                    "required": required,
                    "additionalProperties": false,
                })
            }
            StructuredValueKind::Array => serde_json::json!({
                "type": "array",
                "items": self
                    .items
                    .as_ref()
                    .map(|items| items.to_json_schema())
                    .unwrap_or_else(|| serde_json::json!({})),
            }),
        }
    }

    /// Parses a strict JSON Schema subset into the internal schema.
    ///
    /// Every unsupported keyword is collected and reported with its path —
    /// never silently dropped: a contract must not accept constraints it
    /// cannot enforce. (The lenient converter in `kheish-mcp` exists for
    /// tool schemas, where falling back to unstructured handling is safe;
    /// for contracts, only the strict form is acceptable.)
    pub fn from_json_schema(schema: &Value) -> Result<Self, String> {
        let mut issues = Vec::new();
        let converted = convert_json_schema_node(schema, "$", &mut issues);
        if issues.is_empty() {
            Ok(converted)
        } else {
            Err(issues.join("; "))
        }
    }
}

/// Extracts the JSON candidate from a model answer: trims whitespace and
/// strips one surrounding Markdown fence pair. Extraction is lenient so a
/// well-formed payload inside a fence is not rejected for cosmetics —
/// validation stays strict.
pub fn extract_json_text(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let Some(newline) = rest.find('\n') else {
        return trimmed;
    };
    let body = &rest[newline + 1..];
    match body.rfind("```") {
        Some(end) => body[..end].trim(),
        None => trimmed,
    }
}

fn validate_value_at(
    schema: &StructuredFieldSchema,
    value: &Value,
    path: &str,
) -> Result<(), String> {
    match schema.kind {
        StructuredValueKind::Any => Ok(()),
        StructuredValueKind::String if value.is_string() => Ok(()),
        StructuredValueKind::Number if value.is_number() => Ok(()),
        StructuredValueKind::Boolean if value.is_boolean() => Ok(()),
        StructuredValueKind::Object => {
            let object = value
                .as_object()
                .ok_or_else(|| format!("{path}: expected an object, got {}", value_kind(value)))?;
            for name in object.keys() {
                if !schema.fields.contains_key(name) && !schema.optional_fields.contains_key(name) {
                    return Err(format!("{path}: unknown field `{name}`"));
                }
            }
            for (name, field_schema) in &schema.fields {
                let field_value = object
                    .get(name)
                    .ok_or_else(|| format!("{path}: missing required field `{name}`"))?;
                validate_value_at(field_schema, field_value, &format!("{path}.{name}"))?;
            }
            for (name, field_schema) in &schema.optional_fields {
                if let Some(field_value) = object.get(name).filter(|value| !value.is_null()) {
                    validate_value_at(field_schema, field_value, &format!("{path}.{name}"))?;
                }
            }
            Ok(())
        }
        StructuredValueKind::Array => {
            let items = value
                .as_array()
                .ok_or_else(|| format!("{path}: expected an array, got {}", value_kind(value)))?;
            if let Some(item_schema) = &schema.items {
                for (index, item) in items.iter().enumerate() {
                    validate_value_at(item_schema, item, &format!("{path}[{index}]"))?;
                }
            }
            Ok(())
        }
        StructuredValueKind::String
        | StructuredValueKind::Number
        | StructuredValueKind::Boolean => Err(format!(
            "{path}: expected a {}, got {}",
            kind_label(schema.kind),
            value_kind(value)
        )),
    }
}

fn kind_label(kind: StructuredValueKind) -> &'static str {
    match kind {
        StructuredValueKind::Any => "value",
        StructuredValueKind::String => "string",
        StructuredValueKind::Number => "number",
        StructuredValueKind::Boolean => "boolean",
        StructuredValueKind::Object => "object",
        StructuredValueKind::Array => "array",
    }
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

const SUPPORTED_JSON_SCHEMA_KEYWORDS: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "description",
];

fn convert_json_schema_node(
    schema: &Value,
    path: &str,
    issues: &mut Vec<String>,
) -> StructuredFieldSchema {
    let Some(object) = schema.as_object() else {
        issues.push(format!("{path}: schema node must be a JSON object"));
        return StructuredFieldSchema::new(StructuredValueKind::Any);
    };
    for key in object.keys() {
        let root_only = path == "$" && (key == "$schema" || key == "title");
        if !SUPPORTED_JSON_SCHEMA_KEYWORDS.contains(&key.as_str()) && !root_only {
            issues.push(format!("{path}: unsupported JSON Schema keyword `{key}`"));
        }
    }
    let kind = match object.get("type") {
        Some(Value::String(kind)) => kind.as_str(),
        Some(_) => {
            issues.push(format!("{path}: `type` must be a single string"));
            return StructuredFieldSchema::new(StructuredValueKind::Any);
        }
        None => {
            if object.contains_key("properties") || object.contains_key("items") {
                issues.push(format!("{path}: missing `type` next to properties/items"));
            }
            return StructuredFieldSchema::new(StructuredValueKind::Any);
        }
    };
    match kind {
        "string" => StructuredFieldSchema::new(StructuredValueKind::String),
        "number" | "integer" => StructuredFieldSchema::new(StructuredValueKind::Number),
        "boolean" => StructuredFieldSchema::new(StructuredValueKind::Boolean),
        "array" => {
            let mut array = StructuredFieldSchema::new(StructuredValueKind::Array);
            if let Some(items) = object.get("items") {
                array.items = Some(Box::new(convert_json_schema_node(
                    items,
                    &format!("{path}.items"),
                    issues,
                )));
            }
            array
        }
        "object" => {
            if object.get("additionalProperties") != Some(&Value::Bool(false)) {
                issues.push(format!(
                    "{path}: objects must set `additionalProperties: false` (the contract rejects unknown fields)"
                ));
            }
            let required: std::collections::BTreeSet<&str> = object
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let properties = object.get("properties").and_then(Value::as_object);
            let mut fields = BTreeMap::new();
            let mut optional_fields = BTreeMap::new();
            if let Some(properties) = properties {
                for (name, field_schema) in properties {
                    let child = convert_json_schema_node(
                        field_schema,
                        &format!("{path}.properties.{name}"),
                        issues,
                    );
                    if required.contains(name.as_str()) {
                        fields.insert(name.clone(), child);
                    } else {
                        optional_fields.insert(name.clone(), child);
                    }
                }
            }
            for name in &required {
                if properties.is_none_or(|entries| !entries.contains_key(*name)) {
                    issues.push(format!(
                        "{path}: required field `{name}` is not declared in properties"
                    ));
                }
            }
            StructuredFieldSchema {
                kind: StructuredValueKind::Object,
                fields,
                optional_fields,
                items: None,
            }
        }
        other => {
            issues.push(format!("{path}: unsupported `type` value `{other}`"));
            StructuredFieldSchema::new(StructuredValueKind::Any)
        }
    }
}

/// A structured output contract: when set on a session, the final answer of
/// every run must be a single JSON value matching the schema. The engine
/// validates at the completion boundary and repairs with bounded corrective
/// turns; the validated payload becomes the delivered output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredOutputContract {
    /// The schema the final answer must match.
    pub schema: StructuredFieldSchema,
    /// The maximum number of corrective turns after a failed validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_repair_attempts: Option<u8>,
}

/// The default number of corrective turns granted to a contract.
pub const DEFAULT_OUTPUT_CONTRACT_REPAIR_ATTEMPTS: u8 = 3;
/// The hard ceiling on corrective turns, whatever the contract asks for.
pub const MAX_OUTPUT_CONTRACT_REPAIR_ATTEMPTS: u8 = 5;

impl StructuredOutputContract {
    /// Returns the effective repair budget: the configured value clamped to
    /// the hard ceiling, or the default when unset.
    pub fn effective_max_repair_attempts(&self) -> u8 {
        self.max_repair_attempts
            .unwrap_or(DEFAULT_OUTPUT_CONTRACT_REPAIR_ATTEMPTS)
            .min(MAX_OUTPUT_CONTRACT_REPAIR_ATTEMPTS)
    }
}

/// Reads the structured output contract from run metadata, if any.
pub fn structured_output_contract_from_metadata(
    metadata: &Value,
) -> serde_json::Result<Option<StructuredOutputContract>> {
    metadata
        .get(STRUCTURED_OUTPUT_CONTRACT_METADATA_KEY)
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
}

/// Returns metadata with the structured output contract merged in under the
/// stable key.
pub fn metadata_with_structured_output_contract(
    metadata: Value,
    contract: &StructuredOutputContract,
) -> serde_json::Result<Value> {
    let mut object = match metadata {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("user_metadata".to_string(), other);
            map
        }
    };
    object.insert(
        STRUCTURED_OUTPUT_CONTRACT_METADATA_KEY.to_string(),
        serde_json::to_value(contract)?,
    );
    Ok(Value::Object(object))
}

/// Describes the requested response format for one model turn.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    #[default]
    Text,
    StructuredJson {
        schema: StructuredFieldSchema,
    },
}

/// Provider-neutral reasoning effort levels exposed by supported model adapters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

/// Provider-neutral reasoning summary preference for providers that expose one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummary {
    Auto,
    Concise,
    Detailed,
    /// Explicitly disables provider reasoning summaries where the wire API
    /// represents that by omitting the summary parameter.
    None,
}

impl ReasoningSummary {
    pub fn as_provider_str(self) -> Option<&'static str> {
        match self {
            Self::Auto => Some("auto"),
            Self::Concise => Some("concise"),
            Self::Detailed => Some("detailed"),
            Self::None => None,
        }
    }
}

/// Provider-neutral reasoning configuration for one model turn.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<ReasoningSummary>,
    /// Anthropic-style explicit thinking budget. OpenAI routes reject this so
    /// provider-specific knobs do not get silently ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
    /// Enables provider-specific interleaved thinking when the adapter supports
    /// it. Unsupported adapters reject it explicitly.
    #[serde(default, skip_serializing_if = "is_false")]
    pub interleaved: bool,
}

impl ReasoningConfig {
    pub fn is_empty(&self) -> bool {
        self.effort.is_none()
            && self.summary.is_none()
            && self.budget_tokens.is_none()
            && !self.interleaved
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn reasoning_option_is_empty(value: &Option<ReasoningConfig>) -> bool {
    value
        .as_ref()
        .map(ReasoningConfig::is_empty)
        .unwrap_or(true)
}

fn default_true() -> bool {
    true
}

/// Groups provider-neutral generation options for one model turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelGenerationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    #[serde(default)]
    pub tool_choice: ToolChoice,
    #[serde(default = "default_true")]
    pub allow_parallel_tool_calls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "reasoning_option_is_empty")]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(default)]
    pub response_format: ResponseFormat,
}

impl ModelGenerationConfig {
    /// Merges an explicit generation override onto a default/base generation.
    ///
    /// Fields that still equal the type default are treated as unspecified so
    /// callers can send sparse JSON generation objects from UI/API clients.
    pub fn merge_override(base: Option<Self>, override_generation: Option<Self>) -> Option<Self> {
        let default_generation = Self::default();
        match (base, override_generation) {
            (None, None) => None,
            (Some(base), None) => Some(base),
            (None, Some(override_generation)) => Some(override_generation),
            (Some(base), Some(override_generation)) => Some(Self {
                model: override_generation.model.or(base.model),
                fallback_model: override_generation.fallback_model.or(base.fallback_model),
                tool_choice: if override_generation.tool_choice == default_generation.tool_choice {
                    base.tool_choice
                } else {
                    override_generation.tool_choice
                },
                allow_parallel_tool_calls: if override_generation.allow_parallel_tool_calls
                    == default_generation.allow_parallel_tool_calls
                {
                    base.allow_parallel_tool_calls
                } else {
                    override_generation.allow_parallel_tool_calls
                },
                max_output_tokens: override_generation
                    .max_output_tokens
                    .or(base.max_output_tokens),
                temperature: override_generation.temperature.or(base.temperature),
                reasoning: merge_reasoning_override(base.reasoning, override_generation.reasoning),
                response_format: if override_generation.response_format
                    == default_generation.response_format
                {
                    base.response_format
                } else {
                    override_generation.response_format
                },
            }),
        }
    }

    pub fn merge_defaults(defaults: &Self, request: &Self) -> Self {
        Self::merge_override(Some(defaults.clone()), Some(request.clone())).unwrap_or_default()
    }
}

fn merge_reasoning_override(
    base: Option<ReasoningConfig>,
    override_reasoning: Option<ReasoningConfig>,
) -> Option<ReasoningConfig> {
    match (base, override_reasoning) {
        (None, None) => None,
        (Some(base), None) => (!base.is_empty()).then_some(base),
        (None, Some(override_reasoning)) => {
            (!override_reasoning.is_empty()).then_some(override_reasoning)
        }
        (Some(base), Some(override_reasoning)) => {
            let merged = ReasoningConfig {
                effort: override_reasoning.effort.or(base.effort),
                summary: override_reasoning.summary.or(base.summary),
                budget_tokens: override_reasoning.budget_tokens.or(base.budget_tokens),
                interleaved: override_reasoning.interleaved || base.interleaved,
            };
            (!merged.is_empty()).then_some(merged)
        }
    }
}

impl Default for ModelGenerationConfig {
    fn default() -> Self {
        Self {
            model: None,
            fallback_model: None,
            tool_choice: ToolChoice::Auto,
            allow_parallel_tool_calls: true,
            max_output_tokens: None,
            temperature: None,
            reasoning: None,
            response_format: ResponseFormat::Text,
        }
    }
}

/// Describes one concrete completion requirement that must be satisfied before a run is done.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompletionRequirement {
    /// Requires the run to create or update one file inside the workspace.
    WorkspaceFile {
        /// Optional expected workspace-relative path.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
}

impl CompletionRequirement {
    /// Returns prompt-ready guidance for this requirement.
    pub fn prompt_instruction(&self) -> String {
        match self {
            Self::WorkspaceFile { path: Some(path) } => format!(
                "- The task is not complete until the file `{path}` has been created or updated in the workspace."
            ),
            Self::WorkspaceFile { path: None } => {
                "- The task is not complete until you have created or updated a file in the workspace containing the requested result.".to_string()
            }
        }
    }
}

/// Extracts completion requirements from normalized input metadata.
pub fn completion_requirements_from_metadata(
    metadata: &Value,
) -> serde_json::Result<Vec<CompletionRequirement>> {
    metadata
        .get(COMPLETION_REQUIREMENTS_METADATA_KEY)
        .cloned()
        .map(serde_json::from_value)
        .unwrap_or_else(|| Ok(Vec::new()))
}

/// Returns metadata with completion requirements merged in under the stable key.
pub fn metadata_with_completion_requirements(
    metadata: Value,
    requirements: &[CompletionRequirement],
) -> serde_json::Result<Value> {
    if requirements.is_empty() {
        return Ok(metadata);
    }

    let mut object = match metadata {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("user_metadata".to_string(), other);
            map
        }
    };
    object.insert(
        COMPLETION_REQUIREMENTS_METADATA_KEY.to_string(),
        serde_json::to_value(requirements)?,
    );
    Ok(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        ModelGenerationConfig, ReasoningConfig, ReasoningEffort, ReasoningSummary,
        StructuredFieldSchema, extract_json_text, model_context_window, model_max_output_tokens,
    };

    fn order_schema() -> StructuredFieldSchema {
        StructuredFieldSchema::from_json_schema(&json!({
            "type": "object",
            "properties": {
                "status": {"type": "string"},
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"price": {"type": "number"}},
                        "required": ["price"],
                        "additionalProperties": false,
                    },
                },
                "note": {"type": "string"},
            },
            "required": ["status", "items"],
            "additionalProperties": false,
        }))
        .expect("supported schema subset")
    }

    #[test]
    fn strict_json_schema_conversion_supports_the_documented_subset() {
        let schema = order_schema();
        assert!(schema.fields.contains_key("status"));
        assert!(schema.fields.contains_key("items"));
        assert!(schema.optional_fields.contains_key("note"));
    }

    #[test]
    fn strict_json_schema_conversion_rejects_unsupported_keywords_with_paths() {
        let error = StructuredFieldSchema::from_json_schema(&json!({
            "type": "object",
            "properties": {
                "status": {"type": "string", "enum": ["open", "closed"]},
                "kind": {"oneOf": [{"type": "string"}]},
            },
            "required": ["status"],
            "additionalProperties": false,
        }))
        .expect_err("enum and oneOf are unsupported");
        assert!(error.contains("$.properties.status: unsupported JSON Schema keyword `enum`"));
        assert!(error.contains("$.properties.kind: unsupported JSON Schema keyword `oneOf`"));
    }

    #[test]
    fn strict_json_schema_conversion_requires_closed_objects() {
        let error = StructuredFieldSchema::from_json_schema(&json!({
            "type": "object",
            "properties": {"status": {"type": "string"}},
            "required": ["status", "missing"],
        }))
        .expect_err("open object and undeclared required field");
        assert!(error.contains("additionalProperties: false"));
        assert!(error.contains("required field `missing` is not declared"));
    }

    #[test]
    fn validate_value_reports_json_paths() {
        let schema = order_schema();
        let error = schema
            .validate_value(&json!({
                "status": "open",
                "items": [{"price": 10}, {"price": "free"}],
            }))
            .expect_err("string price must fail");
        assert_eq!(error, "$.items[1].price: expected a number, got a string");

        let error = schema
            .validate_value(&json!({"status": "open", "items": [], "extra": 1}))
            .expect_err("unknown field must fail");
        assert_eq!(error, "$: unknown field `extra`");

        schema
            .validate_value(&json!({"status": "open", "items": [{"price": 3.5}]}))
            .expect("conformant payload validates");
    }

    #[test]
    fn extract_json_text_strips_a_single_fence_pair() {
        assert_eq!(extract_json_text("  {\"a\": 1} "), "{\"a\": 1}");
        assert_eq!(extract_json_text("```json\n{\"a\": 1}\n```"), "{\"a\": 1}");
        assert_eq!(extract_json_text("```\n[1, 2]\n```"), "[1, 2]");
        assert_eq!(extract_json_text("``` not a fence"), "``` not a fence");
    }

    #[test]
    fn to_json_schema_round_trips_through_strict_conversion() {
        let schema = order_schema();
        let rendered = schema.to_json_schema();
        let reparsed =
            StructuredFieldSchema::from_json_schema(&rendered).expect("canonical render reparses");
        assert_eq!(schema, reparsed);
    }

    #[test]
    fn gemini_models_use_known_context_windows() {
        assert_eq!(model_context_window("gemini-2.5-flash"), Some(1_048_576));
        assert_eq!(
            model_context_window("gemini-3-pro-image-preview"),
            Some(65_536)
        );
    }

    #[test]
    fn gemini_models_use_native_output_token_defaults() {
        let flash = model_max_output_tokens("gemini-2.5-flash");
        assert_eq!(flash.default, 65_536);
        assert_eq!(flash.upper_limit, 65_536);

        let image = model_max_output_tokens("gemini-3-pro-image-preview");
        assert_eq!(image.default, 32_768);
        assert_eq!(image.upper_limit, 32_768);
    }

    #[test]
    fn generation_reasoning_roundtrips_and_omits_empty_reasoning() {
        let generation = ModelGenerationConfig {
            model: Some("gpt-5.4".to_string()),
            reasoning: Some(ReasoningConfig {
                effort: Some(ReasoningEffort::Xhigh),
                summary: Some(ReasoningSummary::Auto),
                budget_tokens: None,
                interleaved: false,
            }),
            ..ModelGenerationConfig::default()
        };

        let value = serde_json::to_value(&generation).expect("generation serializes");
        assert_eq!(value["reasoning"]["effort"], "xhigh");
        assert_eq!(value["reasoning"]["summary"], "auto");
        let roundtrip: ModelGenerationConfig =
            serde_json::from_value(value).expect("generation deserializes");
        assert_eq!(roundtrip, generation);

        let empty_reasoning = serde_json::to_value(ModelGenerationConfig {
            reasoning: Some(ReasoningConfig::default()),
            ..ModelGenerationConfig::default()
        })
        .expect("generation serializes");
        assert!(empty_reasoning.get("reasoning").is_none());
    }

    #[test]
    fn generation_merge_preserves_and_overrides_reasoning_fields() {
        let merged = ModelGenerationConfig::merge_override(
            Some(ModelGenerationConfig {
                reasoning: Some(ReasoningConfig {
                    effort: Some(ReasoningEffort::High),
                    summary: Some(ReasoningSummary::Concise),
                    budget_tokens: None,
                    interleaved: false,
                }),
                ..ModelGenerationConfig::default()
            }),
            Some(ModelGenerationConfig {
                reasoning: Some(ReasoningConfig {
                    effort: Some(ReasoningEffort::Xhigh),
                    summary: None,
                    budget_tokens: Some(32_768),
                    interleaved: true,
                }),
                ..ModelGenerationConfig::default()
            }),
        )
        .expect("merged generation should be present");

        assert_eq!(
            merged.reasoning,
            Some(ReasoningConfig {
                effort: Some(ReasoningEffort::Xhigh),
                summary: Some(ReasoningSummary::Concise),
                budget_tokens: Some(32_768),
                interleaved: true,
            })
        );
        assert_eq!(
            serde_json::from_value::<ModelGenerationConfig>(json!({
                "reasoning": {"effort": "low"}
            }))
            .expect("sparse generation deserializes")
            .reasoning
            .and_then(|reasoning| reasoning.effort),
            Some(ReasoningEffort::Low)
        );
    }
}
