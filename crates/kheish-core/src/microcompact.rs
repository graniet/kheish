//! Prompt-view compaction for older tool results.

use std::collections::BTreeSet;

use kheish_types::{MessageRecord, Role, ToolResultRecord};
use serde_json::Value;

use crate::tokens::rough_token_estimate_value;

/// Placeholder used when an old tool result is cleared from the prompt view.
pub const CLEARED_TOOL_RESULT_MESSAGE: &str = "[Tool result cleared by microcompact]";

/// Tool results that are safe to clear aggressively from prompt projections.
pub const COMPACTABLE_TOOLS: &[&str] = &[
    "read_file",
    "bash",
    "grep_search",
    "glob_search",
    "web_fetch",
    "web_search",
];

/// The result of one microcompact pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MicrocompactResult {
    /// The tool call identifiers whose result content was cleared.
    pub cleared_tool_ids: Vec<String>,
    /// The rough number of tokens freed by clearing those results.
    pub tokens_freed: usize,
}

/// Clears the oldest compactable tool results while keeping the most recent ones intact.
pub fn microcompact_tool_results(
    messages: &mut [MessageRecord],
    tool_results: &[ToolResultRecord],
    keep_recent: usize,
) -> MicrocompactResult {
    let compactable = tool_results
        .iter()
        .filter(|result| {
            result
                .tool_name
                .as_deref()
                .is_some_and(|name| COMPACTABLE_TOOLS.contains(&name) && !result.output.is_null())
        })
        .collect::<Vec<_>>();

    if compactable.len() <= keep_recent {
        return MicrocompactResult::default();
    }

    let to_clear = compactable.len() - keep_recent;
    let mut tokens_freed = 0usize;
    let mut cleared_tool_ids = Vec::with_capacity(to_clear);

    for result in compactable.into_iter().take(to_clear) {
        tokens_freed = tokens_freed.saturating_add(rough_token_estimate_value(&result.output));
        cleared_tool_ids.push(result.call_id.clone());
    }

    let cleared = cleared_tool_ids.iter().cloned().collect::<BTreeSet<_>>();
    let replacement =
        serde_json::to_string(&Value::String(CLEARED_TOOL_RESULT_MESSAGE.to_string()))
            .unwrap_or_else(|_| "\"[Tool result cleared by microcompact]\"".to_string());

    for message in messages.iter_mut() {
        if message.role != Role::Tool {
            continue;
        }
        let Some(call_id) = message.id.strip_prefix("tool-message-") else {
            continue;
        };
        if cleared.contains(call_id) {
            message.content = replacement.clone();
        }
    }

    MicrocompactResult {
        cleared_tool_ids,
        tokens_freed,
    }
}

#[cfg(test)]
mod tests {
    use kheish_types::{MessageRecord, Role, ToolResultRecord};
    use serde_json::json;

    use super::{CLEARED_TOOL_RESULT_MESSAGE, microcompact_tool_results};

    #[test]
    fn microcompact_clears_old_compactable_results() {
        let mut messages = vec![
            MessageRecord::new("tool-message-call-1", Role::Tool, "{\"big\":true}"),
            MessageRecord::new("tool-message-call-2", Role::Tool, "{\"big\":true}"),
        ];
        let results = vec![
            ToolResultRecord {
                call_id: "call-1".to_string(),
                output: json!({"payload":"very large"}),
                is_error: false,
                tool_name: Some("read_file".to_string()),
                offset: None,
                timestamp_ms: None,
                context_updates: Vec::new(),
                hook_contexts: Vec::new(),
            },
            ToolResultRecord {
                call_id: "call-2".to_string(),
                output: json!({"payload":"very large"}),
                is_error: false,
                tool_name: Some("bash".to_string()),
                offset: None,
                timestamp_ms: None,
                context_updates: Vec::new(),
                hook_contexts: Vec::new(),
            },
        ];
        let result = microcompact_tool_results(&mut messages, &results, 1);
        assert_eq!(result.cleared_tool_ids, vec!["call-1".to_string()]);
        assert!(messages[0].content.contains(CLEARED_TOOL_RESULT_MESSAGE));
        assert!(!messages[1].content.contains(CLEARED_TOOL_RESULT_MESSAGE));
    }
}
