//! Shared CLI input decoding helpers.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, prelude::BASE64_STANDARD};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Reads required text from either an inline argument, a file, or standard input.
pub(crate) async fn read_text_input(
    inline: Option<String>,
    file: Option<&Path>,
    stdin: bool,
) -> Result<String> {
    let sources = inline.is_some() as u8 + file.is_some() as u8 + stdin as u8;
    if sources != 1 {
        bail!("provide exactly one of inline content, --content-file, or --stdin");
    }

    if let Some(inline) = inline {
        return Ok(inline);
    }
    if let Some(file) = file {
        return tokio::fs::read_to_string(file)
            .await
            .with_context(|| format!("failed to read {}", file.display()));
    }

    tokio::task::spawn_blocking(|| {
        let mut content = String::new();
        std::io::stdin()
            .read_to_string(&mut content)
            .context("failed to read stdin")?;
        Ok::<String, anyhow::Error>(content)
    })
    .await
    .context("stdin reader task panicked")?
}

/// Reads required JSON from either an inline argument, a file, or standard input.
pub(crate) async fn read_json_input<T>(
    inline: Option<String>,
    file: Option<&Path>,
    stdin: bool,
) -> Result<T>
where
    T: DeserializeOwned,
{
    let payload = read_text_input(inline, file, stdin).await?;
    serde_json::from_str(&payload).context("failed to parse JSON input")
}

/// Reads optional text from either an inline argument, a file, or standard input.
pub(crate) async fn read_optional_text_input(
    inline: Option<String>,
    file: Option<&Path>,
    stdin: bool,
) -> Result<Option<String>> {
    let sources = usize::from(inline.is_some()) + usize::from(file.is_some()) + usize::from(stdin);
    if sources == 0 {
        return Ok(None);
    }
    read_text_input(inline, file, stdin).await.map(Some)
}

/// Reads optional JSON from either an inline argument or a file.
pub(crate) async fn read_optional_json_input(
    inline: Option<&str>,
    file: Option<&Path>,
) -> Result<Option<Value>> {
    match (inline, file) {
        (Some(_), Some(_)) => bail!("provide either inline JSON or a JSON file, not both"),
        (Some(inline), None) => serde_json::from_str(inline)
            .with_context(|| "failed to parse inline JSON".to_string())
            .map(Some),
        (None, Some(file)) => {
            let raw = tokio::fs::read_to_string(file)
                .await
                .with_context(|| format!("failed to read {}", file.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse JSON from {}", file.display()))
                .map(Some)
        }
        (None, None) => Ok(None),
    }
}

/// Reads optional typed JSON from either an inline argument or a file.
pub(crate) async fn read_optional_typed_json_input<T>(
    inline: Option<&str>,
    file: Option<&Path>,
) -> Result<Option<T>>
where
    T: DeserializeOwned,
{
    read_optional_json_input(inline, file)
        .await?
        .map(serde_json::from_value)
        .transpose()
        .context("failed to parse typed JSON input")
}

/// Reads required typed JSON from either an inline argument or a file.
pub(crate) async fn read_required_typed_json_input<T>(
    inline: Option<&str>,
    file: Option<&Path>,
    label: &str,
) -> Result<T>
where
    T: DeserializeOwned,
{
    read_optional_typed_json_input(inline, file)
        .await?
        .ok_or_else(|| anyhow!("provide {label} via --json or --file"))
}

/// Reads one file and encodes it as an inline asset upload payload.
pub(crate) async fn inline_asset_upload_from_path(
    path: &Path,
    media_type: Option<&str>,
) -> Result<kheish_daemon::InlineAssetUpload> {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("asset path must include a file name"))?
        .to_string();
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    Ok(kheish_daemon::InlineAssetUpload {
        file_name,
        media_type: media_type.map(str::to_string),
        content_base64: BASE64_STANDARD.encode(bytes),
    })
}

/// Builds input attachments from file paths and existing asset references.
pub(crate) async fn build_session_input_attachments(
    files: &[PathBuf],
    asset_ids: &[String],
) -> Result<Vec<kheish_daemon::InputAttachmentRequest>> {
    let mut attachments = Vec::with_capacity(files.len() + asset_ids.len());
    for file in files {
        attachments.push(kheish_daemon::InputAttachmentRequest::InlineAsset(
            inline_asset_upload_from_path(file, None).await?,
        ));
    }
    attachments.extend(
        asset_ids
            .iter()
            .cloned()
            .map(|asset_id| kheish_daemon::InputAttachmentRequest::AssetReference { asset_id }),
    );
    Ok(attachments)
}

/// URL-encodes one query component without allocating a custom encoder.
pub(crate) fn url_encode_component(value: &str) -> String {
    let mut url = reqwest::Url::parse("http://localhost").expect("static URL should parse");
    url.query_pairs_mut().append_pair("value", value);
    url.query()
        .and_then(|query| query.strip_prefix("value="))
        .unwrap_or_default()
        .to_string()
}

/// Percent-encodes one URI path segment.
pub(crate) fn url_encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'~' => {
                encoded.push(*byte as char)
            }
            byte => {
                encoded.push('%');
                encoded.push(HEX[(byte >> 4) as usize] as char);
                encoded.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    encoded
}

impl crate::ApprovalArgs {
    /// Builds one approval-resolution payload from CLI flags.
    pub(crate) async fn into_resolution(
        self,
        behavior: kheish_types::ApprovalResolutionBehavior,
    ) -> Result<crate::ApprovalResolutionRequest> {
        let updated_input = read_optional_json_input(
            self.updated_input_json.as_deref(),
            self.updated_input_file.as_deref(),
        )
        .await?;
        Ok(crate::ApprovalResolutionRequest {
            session_id: self.session_id,
            run_id: None,
            idempotency_key: self.idempotency_key,
            resolution: kheish_types::ApprovalResolution {
                request_id: self.request_id,
                behavior,
                updated_input,
                justification: self.justification,
                reason: None,
            },
        })
    }
}

impl crate::DenyApprovalArgs {
    /// Builds one deny-resolution payload from CLI flags.
    pub(crate) async fn into_resolution(self) -> Result<crate::ApprovalResolutionRequest> {
        Ok(crate::ApprovalResolutionRequest {
            session_id: self.session_id,
            run_id: None,
            idempotency_key: self.idempotency_key,
            resolution: kheish_types::ApprovalResolution {
                request_id: self.request_id,
                behavior: kheish_types::ApprovalResolutionBehavior::Deny,
                updated_input: None,
                justification: self.justification,
                reason: self.reason,
            },
        })
    }
}

impl crate::GenerationArgs {
    /// Builds one generation config from CLI flags when any override was provided.
    pub(crate) async fn build(&self) -> Result<Option<kheish_runtime::ModelGenerationConfig>> {
        if !self.is_set() {
            return Ok(None);
        }

        let tool_choice = match self.tool_choice.unwrap_or(crate::ToolChoiceArg::Auto) {
            crate::ToolChoiceArg::Auto => kheish_types::ToolChoice::Auto,
            crate::ToolChoiceArg::None => kheish_types::ToolChoice::None,
            crate::ToolChoiceArg::Required => kheish_types::ToolChoice::Required,
            crate::ToolChoiceArg::Specific => kheish_types::ToolChoice::Specific {
                name: self
                    .tool_name
                    .clone()
                    .context("--tool-name is required when --tool-choice=specific")?,
            },
        };
        if !matches!(
            self.tool_choice,
            Some(crate::ToolChoiceArg::Specific) | None
        ) && self.tool_name.is_some()
        {
            bail!("--tool-name requires --tool-choice=specific");
        }

        let response_format = match self
            .response_format
            .unwrap_or(crate::ResponseFormatArg::Text)
        {
            crate::ResponseFormatArg::Text => {
                if self.response_schema_json.is_some() || self.response_schema_file.is_some() {
                    bail!("response schema requires --response-format=structured-json");
                }
                kheish_runtime::ResponseFormat::Text
            }
            crate::ResponseFormatArg::StructuredJson => {
                let schema = read_optional_schema(
                    self.response_schema_json.as_deref(),
                    self.response_schema_file.as_deref(),
                )
                .await?
                .context("structured JSON response format requires a schema")?;
                kheish_runtime::ResponseFormat::StructuredJson { schema }
            }
        };

        Ok(Some(kheish_runtime::ModelGenerationConfig {
            model: self.model.clone(),
            fallback_model: self.fallback_model.clone(),
            tool_choice,
            allow_parallel_tool_calls: !self.serial_tools,
            max_output_tokens: self.max_output_tokens,
            temperature: self.temperature,
            reasoning: self.reasoning_config(),
            response_format,
        }))
    }

    fn reasoning_config(&self) -> Option<kheish_runtime::ReasoningConfig> {
        let config = kheish_runtime::ReasoningConfig {
            effort: self.reasoning_effort.map(Into::into),
            summary: self.reasoning_summary.map(Into::into),
            budget_tokens: self.reasoning_budget_tokens,
            interleaved: self.reasoning_interleaved,
        };
        (!config.is_empty()).then_some(config)
    }

    /// Returns whether any generation override flag was set.
    pub(crate) fn is_set(&self) -> bool {
        self.model.is_some()
            || self.fallback_model.is_some()
            || self.temperature.is_some()
            || self.max_output_tokens.is_some()
            || self.reasoning_effort.is_some()
            || self.reasoning_summary.is_some()
            || self.reasoning_budget_tokens.is_some()
            || self.reasoning_interleaved
            || self.tool_choice.is_some()
            || self.tool_name.is_some()
            || self.serial_tools
            || self.response_format.is_some()
            || self.response_schema_json.is_some()
            || self.response_schema_file.is_some()
    }
}

impl From<crate::ReasoningEffortArg> for kheish_runtime::ReasoningEffort {
    fn from(value: crate::ReasoningEffortArg) -> Self {
        match value {
            crate::ReasoningEffortArg::None => Self::None,
            crate::ReasoningEffortArg::Minimal => Self::Minimal,
            crate::ReasoningEffortArg::Low => Self::Low,
            crate::ReasoningEffortArg::Medium => Self::Medium,
            crate::ReasoningEffortArg::High => Self::High,
            crate::ReasoningEffortArg::Xhigh => Self::Xhigh,
        }
    }
}

impl From<crate::ReasoningSummaryArg> for kheish_runtime::ReasoningSummary {
    fn from(value: crate::ReasoningSummaryArg) -> Self {
        match value {
            crate::ReasoningSummaryArg::Auto => Self::Auto,
            crate::ReasoningSummaryArg::Concise => Self::Concise,
            crate::ReasoningSummaryArg::Detailed => Self::Detailed,
            crate::ReasoningSummaryArg::None => Self::None,
        }
    }
}

impl From<crate::PermissionModeArg> for kheish_runtime::PermissionMode {
    fn from(value: crate::PermissionModeArg) -> Self {
        match value {
            crate::PermissionModeArg::Default => Self::Default,
            crate::PermissionModeArg::AcceptEdits => Self::AcceptEdits,
            crate::PermissionModeArg::BypassPermissions => Self::BypassPermissions,
            crate::PermissionModeArg::Plan => Self::Plan,
            crate::PermissionModeArg::DontAsk => Self::DontAsk,
        }
    }
}

impl From<crate::DebugLevelArg> for kheish_runtime::DebugCaptureLevel {
    fn from(value: crate::DebugLevelArg) -> Self {
        match value {
            crate::DebugLevelArg::Off => Self::Off,
            crate::DebugLevelArg::On => Self::On,
            crate::DebugLevelArg::Redacted => Self::Redacted,
            crate::DebugLevelArg::Full => Self::Full,
        }
    }
}

impl From<crate::ScheduleOverlapPolicyArg> for kheish_daemon::ScheduleOverlapPolicy {
    fn from(value: crate::ScheduleOverlapPolicyArg) -> Self {
        match value {
            crate::ScheduleOverlapPolicyArg::Skip => Self::Skip,
            crate::ScheduleOverlapPolicyArg::QueueOne => Self::QueueOne,
            crate::ScheduleOverlapPolicyArg::Parallel => Self::Parallel,
        }
    }
}

impl From<crate::ScheduleMisfirePolicyArg> for kheish_daemon::ScheduleMisfirePolicy {
    fn from(value: crate::ScheduleMisfirePolicyArg) -> Self {
        match value {
            crate::ScheduleMisfirePolicyArg::CoalesceOnce => Self::CoalesceOnce,
            crate::ScheduleMisfirePolicyArg::SkipMissed => Self::SkipMissed,
        }
    }
}

async fn read_optional_schema(
    inline: Option<&str>,
    file: Option<&Path>,
) -> Result<Option<kheish_types::StructuredFieldSchema>> {
    match (inline, file) {
        (Some(_), Some(_)) => {
            bail!("provide either --response-schema-json or --response-schema-file, not both")
        }
        (Some(inline), None) => serde_json::from_str(inline)
            .with_context(|| "failed to parse structured schema JSON".to_string())
            .map(Some),
        (None, Some(file)) => {
            let raw = tokio::fs::read_to_string(file)
                .await
                .with_context(|| format!("failed to read {}", file.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| {
                    format!("failed to parse structured schema from {}", file.display())
                })
                .map(Some)
        }
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encode_component_escapes_reserved_characters() {
        assert_eq!(url_encode_component("a b/c?d"), "a+b%2Fc%3Fd");
    }

    #[test]
    fn url_encode_path_segment_escapes_slashes_and_spaces() {
        assert_eq!(url_encode_path_segment("a b/c?d"), "a%20b%2Fc%3Fd");
        assert_eq!(url_encode_path_segment("."), "%2E");
        assert_eq!(url_encode_path_segment(".."), "%2E%2E");
    }

    #[tokio::test]
    async fn read_optional_typed_json_input_reads_one_file() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("payload.json");
        tokio::fs::write(&path, "{\"value\":3}").await?;
        let payload =
            read_optional_typed_json_input::<serde_json::Value>(None, Some(&path)).await?;
        assert_eq!(payload, Some(serde_json::json!({"value": 3})));
        Ok(())
    }
}
