use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable metadata key used to carry run completion requirements.
pub const COMPLETION_REQUIREMENTS_METADATA_KEY: &str = "completion_requirements";
/// Claude Code-style capped default output token ceiling.
pub const CAPPED_DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8_000;
/// Claude Code-style escalated output token ceiling used for recovery.
pub const ESCALATED_MAX_OUTPUT_TOKENS: u32 = 64_000;

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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    pub fields: BTreeMap<String, StructuredFieldSchema>,
    /// Optional object fields keyed by property name.
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
        model_context_window, model_max_output_tokens,
    };

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
