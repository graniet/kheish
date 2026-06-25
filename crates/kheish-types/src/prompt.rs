use serde::{Deserialize, Serialize};

use crate::compaction::{PostCompactRestoration, SummaryBlock};
use crate::routing::AttachmentRef;
use crate::routing::InputContentPart;
use crate::tools::{MessageRecord, Role, ToolCallRecord, ToolResultRecord};

/// Stores one named system-prompt section before provider-specific encoding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemPromptSection {
    /// The stable section identifier.
    pub name: String,
    /// The section content appended to the effective system prompt.
    pub content: String,
}

/// Describes the prompt slice currently visible to the agent loop.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptProjection {
    /// The optional compacted summary block currently in force.
    pub summary: Option<SummaryBlock>,
    /// The named system prompt sections composing the current instructions.
    pub system_sections: Vec<SystemPromptSection>,
    /// The canonical conversation messages visible to the next turn.
    pub messages: Vec<MessageRecord>,
    /// Tool calls that are still open in the visible prompt slice.
    pub open_tool_calls: Vec<ToolCallRecord>,
    /// The optional post-compaction restoration payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restoration: Option<PostCompactRestoration>,
}

/// Describes one provider-facing prompt item built from canonical session state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderInputItem {
    Summary {
        /// The canonical summary block retained after compaction.
        summary: SummaryBlock,
    },
    Restoration {
        /// The restoration payload used to recover compacted context.
        restoration: PostCompactRestoration,
    },
    Message {
        /// The canonical message identifier.
        id: String,
        /// The speaker role for this message.
        role: Role,
        /// The canonical text transcript for this message.
        content: String,
        /// The ordered multimodal content parts when the message came from a rich user input.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content_parts: Vec<InputContentPart>,
        /// The flattened attachment list retained for compatibility and fallback rendering.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<AttachmentRef>,
        /// The optional provider-native response identifier used for incremental replay.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_response_id: Option<String>,
        /// Provider-native context that must be replayed with this message but
        /// is not part of the user-visible transcript.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_context: Option<serde_json::Value>,
    },
    ToolCall {
        /// The assistant message that originally emitted the tool call, when known.
        assistant_message_id: Option<String>,
        /// The canonical tool call record.
        call: ToolCallRecord,
    },
    ToolResult {
        /// The canonical tool result record.
        result: ToolResultRecord,
    },
}

/// Provider-neutral prompt representation used at the model boundary.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderPrompt {
    /// The system instructions forwarded to the provider adapter.
    #[serde(default)]
    pub instructions: Vec<String>,
    /// Whether the adapter should synthesize a leading user turn when required.
    #[serde(default)]
    pub force_synthetic_user_prefix: bool,
    /// The ordered provider-neutral prompt items.
    pub items: Vec<ProviderInputItem>,
}
