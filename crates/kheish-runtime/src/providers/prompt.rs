use std::collections::{BTreeMap, BTreeSet};

use kheish_core::{build_compaction_resume_message, render_post_compact_restoration};
use serde_json::json;

use kheish_types::{
    AttachmentRef, InputContentPart, ProviderInputItem, ProviderPrompt, Role, ToolCallRecord,
    ToolResultRecord,
};

/// Provider-neutral prompt instructions and conversation items with repaired tool pairing.
#[derive(Clone, Debug, PartialEq)]
pub struct NormalizedProviderPrompt {
    /// System-level instructions that should be prepended outside the conversation turns.
    pub instructions: Vec<String>,
    /// Ordered conversation items safe to encode for provider APIs.
    pub conversation: Vec<NormalizedConversationItem>,
}

/// One normalized conversation item ready for provider-specific encoding.
#[derive(Clone, Debug, PartialEq)]
pub enum NormalizedConversationItem {
    /// A user-authored text message.
    UserMessage {
        id: String,
        content: String,
        content_parts: Vec<InputContentPart>,
        attachments: Vec<AttachmentRef>,
    },
    /// An assistant-authored text message.
    AssistantMessage {
        id: String,
        provider_response_id: Option<String>,
        provider_context: Option<serde_json::Value>,
        content: String,
    },
    /// One or more tool calls emitted by the same assistant message.
    AssistantToolCalls {
        assistant_message_id: Option<String>,
        assistant_provider_response_id: Option<String>,
        calls: Vec<ToolCallRecord>,
    },
    /// One or more tool results that belong to previously emitted tool calls.
    ToolResults { results: Vec<ToolResultRecord> },
}

/// Normalizes provider prompt items into stable conversation boundaries and repaired tool pairs.
pub(crate) fn normalize_provider_prompt(prompt: &ProviderPrompt) -> NormalizedProviderPrompt {
    let mut instructions = prompt.instructions.clone();
    let mut conversation = Vec::new();
    let mut active_assistant_message_id: Option<String> = None;
    let mut seen_tool_calls = BTreeSet::new();
    let mut seen_tool_results = BTreeSet::new();
    let mut open_tool_calls = BTreeMap::new();
    let mut tool_call_order = Vec::new();

    for item in &prompt.items {
        match item {
            ProviderInputItem::Summary { summary } => {
                append_missing_tool_results(
                    &mut conversation,
                    &mut open_tool_calls,
                    &mut tool_call_order,
                );
                if !summary.content.trim().is_empty() {
                    conversation.push(NormalizedConversationItem::UserMessage {
                        id: format!("summary-{}", summary.title),
                        content: build_compaction_resume_message(&summary.content, true),
                        content_parts: Vec::new(),
                        attachments: Vec::new(),
                    });
                }
            }
            ProviderInputItem::Restoration { restoration } => {
                append_missing_tool_results(
                    &mut conversation,
                    &mut open_tool_calls,
                    &mut tool_call_order,
                );
                let content = render_post_compact_restoration(restoration);
                if !content.trim().is_empty() {
                    conversation.push(NormalizedConversationItem::UserMessage {
                        id: "post-compact-restoration".to_string(),
                        content,
                        content_parts: Vec::new(),
                        attachments: Vec::new(),
                    });
                }
                for retained in &restoration.retained_user_inputs {
                    if retained.content.trim().is_empty() && retained.content_parts.is_empty() {
                        continue;
                    }
                    conversation.push(NormalizedConversationItem::UserMessage {
                        id: format!("retained-{}", retained.message_id),
                        content: retained.content.clone(),
                        content_parts: retained.content_parts.clone(),
                        attachments: retained
                            .content_parts
                            .iter()
                            .filter_map(|part| match part {
                                InputContentPart::Attachment { attachment } => {
                                    Some(attachment.clone())
                                }
                                InputContentPart::Text { .. } => None,
                            })
                            .collect(),
                    });
                }
            }
            ProviderInputItem::Message {
                id,
                role,
                content,
                content_parts,
                attachments,
                provider_response_id,
                provider_context,
            } => match role {
                Role::System | Role::Summary => {
                    append_missing_tool_results(
                        &mut conversation,
                        &mut open_tool_calls,
                        &mut tool_call_order,
                    );
                    if !content.trim().is_empty() {
                        instructions.push(content.clone());
                    }
                }
                Role::User => {
                    append_missing_tool_results(
                        &mut conversation,
                        &mut open_tool_calls,
                        &mut tool_call_order,
                    );
                    active_assistant_message_id = None;
                    if !content.trim().is_empty() {
                        conversation.push(NormalizedConversationItem::UserMessage {
                            id: id.clone(),
                            content: content.clone(),
                            content_parts: content_parts.clone(),
                            attachments: attachments.clone(),
                        });
                    } else if !content_parts.is_empty() || !attachments.is_empty() {
                        conversation.push(NormalizedConversationItem::UserMessage {
                            id: id.clone(),
                            content: content.clone(),
                            content_parts: content_parts.clone(),
                            attachments: attachments.clone(),
                        });
                    }
                }
                Role::Assistant => {
                    append_missing_tool_results(
                        &mut conversation,
                        &mut open_tool_calls,
                        &mut tool_call_order,
                    );
                    active_assistant_message_id = Some(id.clone());
                    if !content.trim().is_empty() || provider_context.is_some() {
                        conversation.push(NormalizedConversationItem::AssistantMessage {
                            id: id.clone(),
                            provider_response_id: provider_response_id.clone(),
                            provider_context: provider_context.clone(),
                            content: content.clone(),
                        });
                    }
                }
                Role::Tool => {}
            },
            ProviderInputItem::ToolCall {
                assistant_message_id,
                call,
            } => {
                if !seen_tool_calls.insert(call.id.clone()) {
                    continue;
                }
                let assistant_message_id = assistant_message_id
                    .clone()
                    .or_else(|| call.assistant_message_id.clone())
                    .or_else(|| active_assistant_message_id.clone());
                let assistant_provider_response_id = call.assistant_provider_response_id.clone();
                let mut call = call.clone();
                call.assistant_message_id = assistant_message_id.clone();
                open_tool_calls.insert(call.id.clone(), call.clone());
                tool_call_order.push(call.id.clone());
                match conversation.last_mut() {
                    Some(NormalizedConversationItem::AssistantToolCalls {
                        assistant_message_id: current_assistant_message_id,
                        assistant_provider_response_id: current_provider_response_id,
                        calls,
                    }) if *current_assistant_message_id == assistant_message_id
                        && *current_provider_response_id == assistant_provider_response_id =>
                    {
                        calls.push(call)
                    }
                    _ => conversation.push(NormalizedConversationItem::AssistantToolCalls {
                        assistant_message_id,
                        assistant_provider_response_id,
                        calls: vec![call],
                    }),
                }
            }
            ProviderInputItem::ToolResult { result } => {
                if !seen_tool_results.insert(result.call_id.clone()) {
                    continue;
                }
                if open_tool_calls.remove(&result.call_id).is_none() {
                    continue;
                }
                append_tool_result(&mut conversation, result.clone());
                active_assistant_message_id = None;
            }
        }
    }

    append_missing_tool_results(
        &mut conversation,
        &mut open_tool_calls,
        &mut tool_call_order,
    );

    if prompt.force_synthetic_user_prefix {
        prepend_synthetic_user_entrypoint(&mut conversation);
    }

    NormalizedProviderPrompt {
        instructions,
        conversation,
    }
}

fn append_missing_tool_results(
    conversation: &mut Vec<NormalizedConversationItem>,
    open_tool_calls: &mut BTreeMap<String, ToolCallRecord>,
    tool_call_order: &mut Vec<String>,
) {
    for call_id in std::mem::take(tool_call_order) {
        let Some(call) = open_tool_calls.remove(&call_id) else {
            continue;
        };
        append_tool_result(
            conversation,
            ToolResultRecord {
                call_id: call.id,
                output: json!({
                    "error": "tool result missing during provider prompt normalization",
                    "tool": call.name,
                }),
                is_error: true,
                tool_name: Some(call.name),
                offset: None,
                timestamp_ms: None,
                context_updates: Vec::new(),
                hook_contexts: Vec::new(),
            },
        );
    }
}

fn append_tool_result(
    conversation: &mut Vec<NormalizedConversationItem>,
    result: ToolResultRecord,
) {
    match conversation.last_mut() {
        Some(NormalizedConversationItem::ToolResults { results }) => results.push(result),
        _ => conversation.push(NormalizedConversationItem::ToolResults {
            results: vec![result],
        }),
    }
}

fn prepend_synthetic_user_entrypoint(conversation: &mut Vec<NormalizedConversationItem>) {
    let Some(first) = conversation.first() else {
        return;
    };
    if normalized_item_side(first) == ConversationSide::Assistant {
        conversation.insert(
            0,
            NormalizedConversationItem::UserMessage {
                id: "synthetic-user-continue".to_string(),
                content: "Continue from where you left off.".to_string(),
                content_parts: Vec::new(),
                attachments: Vec::new(),
            },
        );
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ConversationSide {
    User,
    Assistant,
}

fn normalized_item_side(item: &NormalizedConversationItem) -> ConversationSide {
    match item {
        NormalizedConversationItem::UserMessage { .. }
        | NormalizedConversationItem::ToolResults { .. } => ConversationSide::User,
        NormalizedConversationItem::AssistantMessage { .. }
        | NormalizedConversationItem::AssistantToolCalls { .. } => ConversationSide::Assistant,
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serde_json::json;

    use super::{NormalizedConversationItem, normalize_provider_prompt};
    use kheish_types::{
        ProviderInputItem, ProviderPrompt, Role, SummaryBlock, ToolCallRecord, ToolResultRecord,
    };

    #[test]
    fn normalization_drops_orphan_results_and_synthesizes_missing_results() -> Result<()> {
        let prompt = ProviderPrompt {
            instructions: Vec::new(),
            force_synthetic_user_prefix: false,
            items: vec![
                ProviderInputItem::Summary {
                    summary: SummaryBlock {
                        title: "resume".to_string(),
                        content: "Earlier context".to_string(),
                    },
                },
                ProviderInputItem::Message {
                    id: "assistant-1".to_string(),
                    role: Role::Assistant,
                    content: "Working on it.".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: Some("resp-1".to_string()),
                    provider_context: None,
                },
                ProviderInputItem::ToolCall {
                    assistant_message_id: Some("assistant-1".to_string()),
                    call: ToolCallRecord {
                        id: "call-1".to_string(),
                        name: "echo".to_string(),
                        input: json!({"text": "hi"}),
                        assistant_message_id: None,
                        assistant_provider_response_id: None,
                    },
                },
                ProviderInputItem::ToolResult {
                    result: ToolResultRecord {
                        call_id: "call-missing".to_string(),
                        output: json!({"ignored": true}),
                        is_error: false,
                        tool_name: Some("echo".to_string()),
                        offset: None,
                        timestamp_ms: None,
                        context_updates: Vec::new(),
                        hook_contexts: Vec::new(),
                    },
                },
            ],
        };

        let normalized = normalize_provider_prompt(&prompt);
        assert!(normalized.instructions.is_empty());
        assert_eq!(normalized.conversation.len(), 4);
        assert!(matches!(
            &normalized.conversation[0],
            NormalizedConversationItem::UserMessage { content, .. }
            if content.contains("continued from an earlier conversation")
        ));
        assert!(matches!(
            &normalized.conversation[3],
            NormalizedConversationItem::ToolResults { results }
            if results.len() == 1
                && results[0].call_id == "call-1"
                && results[0].is_error
        ));
        Ok(())
    }

    #[test]
    fn normalization_flushes_missing_tool_results_before_next_user_boundary() -> Result<()> {
        let prompt = ProviderPrompt {
            instructions: Vec::new(),
            force_synthetic_user_prefix: false,
            items: vec![
                ProviderInputItem::Message {
                    id: "assistant-1".to_string(),
                    role: Role::Assistant,
                    content: "Starting tool.".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: Some("resp-1".to_string()),
                    provider_context: None,
                },
                ProviderInputItem::ToolCall {
                    assistant_message_id: Some("assistant-1".to_string()),
                    call: ToolCallRecord {
                        id: "call-1".to_string(),
                        name: "bash".to_string(),
                        input: json!({"cmd": "echo hi"}),
                        assistant_message_id: None,
                        assistant_provider_response_id: Some("resp-1".to_string()),
                    },
                },
                ProviderInputItem::Message {
                    id: "user-2".to_string(),
                    role: Role::User,
                    content: "Continue.".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: None,
                    provider_context: None,
                },
            ],
        };

        let normalized = normalize_provider_prompt(&prompt);
        assert_eq!(normalized.conversation.len(), 4);
        assert!(matches!(
            &normalized.conversation[1],
            NormalizedConversationItem::AssistantToolCalls { calls, .. }
                if calls[0].id == "call-1"
        ));
        assert!(matches!(
            &normalized.conversation[2],
            NormalizedConversationItem::ToolResults { results }
                if results[0].call_id == "call-1" && results[0].is_error
        ));
        assert!(matches!(
            &normalized.conversation[3],
            NormalizedConversationItem::UserMessage { content, .. } if content == "Continue."
        ));
        Ok(())
    }

    #[test]
    fn normalization_keeps_user_text_and_tool_results_in_distinct_items() -> Result<()> {
        let prompt = ProviderPrompt {
            instructions: Vec::new(),
            force_synthetic_user_prefix: false,
            items: vec![
                ProviderInputItem::Message {
                    id: "user-1".to_string(),
                    role: Role::User,
                    content: "Run the tool.".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: None,
                    provider_context: None,
                },
                ProviderInputItem::Message {
                    id: "assistant-1".to_string(),
                    role: Role::Assistant,
                    content: "Calling tools.".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: Some("resp-1".to_string()),
                    provider_context: None,
                },
                ProviderInputItem::ToolCall {
                    assistant_message_id: Some("assistant-1".to_string()),
                    call: ToolCallRecord {
                        id: "call-1".to_string(),
                        name: "echo".to_string(),
                        input: json!({"text": "hi"}),
                        assistant_message_id: None,
                        assistant_provider_response_id: None,
                    },
                },
                ProviderInputItem::ToolResult {
                    result: ToolResultRecord {
                        call_id: "call-1".to_string(),
                        output: json!({"echo": "hi"}),
                        is_error: false,
                        tool_name: Some("echo".to_string()),
                        offset: None,
                        timestamp_ms: None,
                        context_updates: Vec::new(),
                        hook_contexts: Vec::new(),
                    },
                },
                ProviderInputItem::Message {
                    id: "user-2".to_string(),
                    role: Role::User,
                    content: "Continue.".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: None,
                    provider_context: None,
                },
            ],
        };

        let normalized = normalize_provider_prompt(&prompt);
        assert!(matches!(
            &normalized.conversation[2],
            NormalizedConversationItem::AssistantToolCalls { calls, .. } if calls.len() == 1
        ));
        assert!(matches!(
            &normalized.conversation[3],
            NormalizedConversationItem::ToolResults { results } if results.len() == 1
        ));
        assert!(matches!(
            &normalized.conversation[4],
            NormalizedConversationItem::UserMessage { content, .. } if content == "Continue."
        ));
        Ok(())
    }

    #[test]
    fn normalization_prepends_user_entry_when_conversation_starts_with_assistant() -> Result<()> {
        let prompt = ProviderPrompt {
            instructions: Vec::new(),
            force_synthetic_user_prefix: true,
            items: vec![ProviderInputItem::Message {
                id: "assistant-1".to_string(),
                role: Role::Assistant,
                content: "Recovered assistant message.".to_string(),
                content_parts: Vec::new(),
                attachments: Vec::new(),
                provider_response_id: Some("resp-1".to_string()),
                provider_context: None,
            }],
        };

        let normalized = normalize_provider_prompt(&prompt);
        assert!(matches!(
            &normalized.conversation[0],
            NormalizedConversationItem::UserMessage { content, .. }
                if content == "Continue from where you left off."
        ));
        assert!(matches!(
            &normalized.conversation[1],
            NormalizedConversationItem::AssistantMessage { content, .. }
                if content == "Recovered assistant message."
        ));
        Ok(())
    }
}
