use serde::{Deserialize, Serialize};
use serde_json::Value;

const ASSET_URI_PREFIX: &str = "asset://";
/// Default character budget used when rendering document attachments into prompt-ready text.
pub const DEFAULT_DOCUMENT_ATTACHMENT_TEXT_CHAR_LIMIT: usize = 12_000;

/// Identifies the input plugin and source kind that produced a normalized input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    /// The ingress plugin name.
    pub plugin: String,
    /// The ingress source kind inside that plugin.
    pub kind: String,
}

/// Identifies the actor responsible for a normalized input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorRef {
    /// The stable actor identifier.
    pub id: String,
    /// The optional human-readable actor label.
    pub display_name: Option<String>,
}

/// Identifies one conversation session and optional provider thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationKey {
    /// The daemon session identifier.
    pub session_id: String,
    /// The optional provider-side thread identifier.
    pub thread_id: Option<String>,
}

/// Points at a binary or file attachment associated with an input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentRef {
    /// The stable daemon-owned asset identifier.
    pub id: String,
    /// The normalized MIME type used to validate and render the attachment.
    pub media_type: String,
    /// The daemon-managed storage URI for the raw file payload.
    pub uri: String,
    /// The original file name when one was provided by the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The normalized SHA-256 digest of the raw file payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// The raw file size in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_length: Option<u64>,
    /// The daemon-managed storage URI for derived plain text when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_uri: Option<String>,
    /// The normalized SHA-256 digest of the derived plain text payload when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_sha256: Option<String>,
    /// The derived plain text payload size in bytes when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_byte_length: Option<u64>,
    /// The daemon-managed storage URI for one derived visual preview when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_image_uri: Option<String>,
    /// The normalized MIME type for the derived visual preview when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_image_media_type: Option<String>,
    /// The normalized SHA-256 digest of the derived visual preview payload when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_image_sha256: Option<String>,
    /// The derived visual preview payload size in bytes when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_image_byte_length: Option<u64>,
}

/// Describes one ordered content part inside an input or output payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// One plain-text fragment.
    Text { text: String },
    /// One daemon-owned attachment referenced in sequence with the text.
    Attachment { attachment: AttachmentRef },
}

/// Backward-compatible alias for the canonical input content-part type.
pub type InputContentPart = ContentPart;

/// One canonical rich output payload emitted by the daemon.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichOutput {
    /// Plain-text fallback and preview for text-only surfaces.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    /// Ordered visible output parts rendered by rich-capable surfaces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<ContentPart>,
    /// Additional daemon-owned assets produced by the run but not shown inline.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<AttachmentRef>,
}

impl RichOutput {
    /// Returns one plain-text rich output with no inline attachments.
    #[must_use]
    pub fn text(content: impl Into<String>) -> Self {
        let content = content.into();
        Self {
            content: content.clone(),
            parts: (!content.is_empty())
                .then_some(ContentPart::Text { text: content })
                .into_iter()
                .collect(),
            artifacts: Vec::new(),
        }
    }

    /// Returns this output with a stable fallback string and visible text part when needed.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        if !self.content.is_empty()
            && !self
                .parts
                .iter()
                .any(|part| matches!(part, ContentPart::Text { .. }))
        {
            self.parts.insert(
                0,
                ContentPart::Text {
                    text: self.content.clone(),
                },
            );
        }
        if self.parts.is_empty() && self.content.is_empty() && !self.artifacts.is_empty() {
            self.parts.extend(
                self.artifacts
                    .iter()
                    .cloned()
                    .map(|attachment| ContentPart::Attachment { attachment }),
            );
        }
        if !self.parts.is_empty() {
            self.content = render_content_parts_text(&self.parts);
        } else if self.content.is_empty() {
            self.content.clear();
        }
        self
    }
}

/// Builds one opaque daemon storage URI for an asset payload.
pub fn asset_storage_uri(kind: &str, relative_path: &str) -> String {
    format!("{ASSET_URI_PREFIX}{kind}/{relative_path}")
}

/// Parses one daemon storage URI into its kind and relative path.
pub fn parse_asset_storage_uri(uri: &str) -> Option<(&str, &str)> {
    let remainder = uri.strip_prefix(ASSET_URI_PREFIX)?;
    let (kind, relative_path) = remainder.split_once('/')?;
    if kind.is_empty() || relative_path.is_empty() {
        return None;
    }
    Some((kind, relative_path))
}

/// Renders one document attachment into the stable prompt-ready text block used across the daemon.
pub fn render_document_attachment_text(
    display_name: &str,
    media_type: &str,
    text: &str,
    char_limit: usize,
) -> String {
    let (snippet, truncated) = truncate_chars(text, char_limit);
    let mut lines = vec![format!(
        "Document attachment: {display_name} ({media_type})"
    )];
    if !snippet.is_empty() {
        lines.push(snippet);
    }
    if truncated {
        lines.push(format!(
            "[Truncated document text after {char_limit} characters.]"
        ));
    }
    lines.join("\n")
}

/// Renders ordered content parts into a compact plain-text fallback.
#[must_use]
pub fn render_content_parts_text(parts: &[ContentPart]) -> String {
    let rendered = parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => {
                let trimmed = text.trim();
                (!trimmed.is_empty()).then_some(trimmed.to_string())
            }
            ContentPart::Attachment { attachment } => {
                let display_name = attachment
                    .file_name
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(&attachment.id);
                Some(format!(
                    "Attached asset: {display_name} ({})",
                    attachment.media_type
                ))
            }
        })
        .collect::<Vec<_>>();
    rendered.join("\n")
}

fn truncate_chars(value: &str, limit: usize) -> (String, bool) {
    if value.chars().count() <= limit {
        return (value.to_string(), false);
    }
    (value.chars().take(limit).collect::<String>(), true)
}

/// Routes a finalized response back to a specific output plugin and destination.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReplyHandle {
    pub plugin: String,
    pub address: String,
}

/// Returns a deduplicated list of reply targets, preserving the declared primary reply first
/// whenever one is provided.
pub fn normalize_reply_targets(
    reply: Option<ReplyHandle>,
    reply_targets: Vec<ReplyHandle>,
) -> Vec<ReplyHandle> {
    let mut normalized = reply.into_iter().collect::<Vec<_>>();
    normalized.extend(reply_targets);
    let mut seen = std::collections::BTreeSet::new();
    normalized.retain(|target| seen.insert(target.clone()));
    normalized
}

/// Describes one normalized input payload forwarded to the agent runtime.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputPayload {
    /// A plain text-only input.
    Text { content: String },
    /// A multimodal input with ordered content parts plus a canonical rendered transcript.
    Rich {
        /// The canonical transcript used by text-only providers and history rendering.
        rendered_content: String,
        /// The ordered content parts preserved for provider-specific multimodal encoding.
        items: Vec<InputContentPart>,
    },
    /// A structured JSON input.
    Json { value: Value },
    /// A named event input emitted by the daemon or a connector.
    Event { name: String, value: Value },
    /// A command-style input carrying structured arguments.
    Command { name: String, arguments: Value },
}

/// Wraps one normalized input with routing, actor, and metadata information.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputEnvelope {
    /// The ingress source that produced the normalized input.
    pub source: SourceRef,
    /// The conversation key that owns the input.
    pub conversation: ConversationKey,
    /// The actor responsible for the input.
    pub actor: ActorRef,
    /// The normalized payload presented to the runtime.
    pub payload: InputPayload,
    /// The flattened attachment list retained for compatibility and indexing.
    pub attachments: Vec<AttachmentRef>,
    /// Arbitrary caller metadata attached to the input.
    #[serde(default)]
    pub metadata: Value,
    /// The full set of output targets that should receive replies for this input.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reply_targets: Vec<ReplyHandle>,
    /// The primary reply target for compatibility with older flows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<ReplyHandle>,
}

impl InputEnvelope {
    pub fn text(
        plugin: impl Into<String>,
        kind: impl Into<String>,
        session_id: impl Into<String>,
        actor_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            source: SourceRef {
                plugin: plugin.into(),
                kind: kind.into(),
            },
            conversation: ConversationKey {
                session_id: session_id.into(),
                thread_id: None,
            },
            actor: ActorRef {
                id: actor_id.into(),
                display_name: None,
            },
            payload: InputPayload::Text {
                content: content.into(),
            },
            attachments: Vec::new(),
            metadata: Value::Null,
            reply_targets: Vec::new(),
            reply: None,
        }
    }
}
