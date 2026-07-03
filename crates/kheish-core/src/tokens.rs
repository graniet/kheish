//! Token estimation helpers used by the core compaction pipeline.

use kheish_types::{
    ApiUsage, MessageRecord, PostCompactRestoration, SummaryBlock, SystemPromptSection,
    ToolCallRecord,
};
use serde_json::Value;

/// Claude Code-style rough estimate: approximately 0.285 tokens per character.
pub fn rough_token_estimate(text: &str) -> usize {
    ((text.len() as f64) * 0.285).ceil() as usize
}

/// Estimates tokens for a JSON value by serializing it to a compact string first.
pub fn rough_token_estimate_value(value: &Value) -> usize {
    rough_token_estimate(&serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()))
}

/// Estimates tokens for one canonical message.
pub fn rough_token_estimate_message(message: &MessageRecord) -> usize {
    rough_token_estimate(&message.content)
}

/// Estimates tokens for an ordered message slice with conservative padding.
pub fn rough_token_estimate_all(messages: &[MessageRecord]) -> usize {
    let total = messages
        .iter()
        .map(rough_token_estimate_message)
        .sum::<usize>();
    if total == 0 { 0 } else { (total * 4) / 3 }
}

/// Returns the latest measured API usage embedded in the provided messages.
pub fn latest_api_usage(messages: &[MessageRecord]) -> Option<&ApiUsage> {
    messages
        .iter()
        .rev()
        .find_map(|message| message.api_usage.as_ref())
}

/// Estimates context tokens using measured API usage when available and rough estimation otherwise.
pub fn calibrated_token_count(
    messages: &[MessageRecord],
    last_api_usage: Option<&ApiUsage>,
) -> usize {
    match last_api_usage {
        Some(usage) => {
            let measured = usage.input_tokens.saturating_add(usage.output_tokens) as usize;
            let tail_estimate = messages
                .iter()
                .rev()
                .take_while(|message| message.api_usage.is_none())
                .map(rough_token_estimate_message)
                .sum::<usize>();
            measured.saturating_add(tail_estimate)
        }
        None => rough_token_estimate_all(messages),
    }
}

/// Estimates prompt tokens from visible prompt components sent to the provider.
///
/// When the window carries a measured provider usage, that measurement already
/// covers the system sections, summary block, restoration payload, and every
/// message present in that request — adding component estimates on top would
/// double-count them, and an inflated estimate can wedge autocompact
/// permanently above its threshold (compaction cannot shrink components that
/// are re-attached after every compaction). Only messages appended since the
/// measured call still need estimating.
pub fn calibrated_prompt_token_count(
    summary: Option<&SummaryBlock>,
    system_sections: &[SystemPromptSection],
    messages: &[MessageRecord],
    open_tool_calls: &[ToolCallRecord],
    restoration: Option<&PostCompactRestoration>,
) -> usize {
    let last_api_usage = latest_api_usage(messages);
    let message_tokens = calibrated_token_count(messages, last_api_usage);
    if last_api_usage.is_some() {
        return message_tokens;
    }
    let summary_tokens = summary
        .map(|summary| rough_token_estimate(&summary.content))
        .unwrap_or_default();
    let system_section_tokens = if system_sections.is_empty() {
        0
    } else {
        rough_token_estimate(
            &system_sections
                .iter()
                .map(|section| section.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    };
    let open_tool_call_tokens = open_tool_calls
        .iter()
        .map(|call| {
            rough_token_estimate(&call.name).saturating_add(rough_token_estimate_value(&call.input))
        })
        .sum::<usize>();
    // The restoration payload reaches the provider as rendered text, not as
    // its raw serialized form; estimate what is actually sent.
    let restoration_tokens = restoration
        .map(|restoration| {
            rough_token_estimate(&crate::restoration::render_post_compact_restoration(
                restoration,
            ))
        })
        .unwrap_or_default();
    message_tokens
        .saturating_add(summary_tokens)
        .saturating_add(system_section_tokens)
        .saturating_add(open_tool_call_tokens)
        .saturating_add(restoration_tokens)
}

#[cfg(test)]
mod tests {
    use kheish_types::{
        MessageRecord, ModelUsage, PostCompactRestoration, Role, SessionControlState, SummaryBlock,
        SystemPromptSection, ToolCallRecord, WorkspaceSnapshot,
    };
    use serde_json::json;

    use super::{
        calibrated_prompt_token_count, calibrated_token_count, latest_api_usage,
        rough_token_estimate, rough_token_estimate_all,
    };

    #[test]
    fn rough_estimate_uses_claude_code_ratio() {
        assert_eq!(rough_token_estimate(""), 0);
        assert_eq!(rough_token_estimate("abcd"), 2);
    }

    #[test]
    fn calibrated_count_uses_latest_measured_usage_plus_tail() {
        let messages = vec![
            MessageRecord::new("u1", Role::User, "hello there"),
            MessageRecord::new("a1", Role::Assistant, "measured").with_api_usage(ModelUsage {
                input_tokens: 20,
                output_tokens: 5,
                cost_usd: 0.0,
            }),
            MessageRecord::new("u2", Role::User, "tail message"),
        ];
        assert_eq!(latest_api_usage(&messages).unwrap().input_tokens, 20);
        assert_eq!(
            calibrated_token_count(&messages, latest_api_usage(&messages)),
            25 + rough_token_estimate("tail message")
        );
    }

    #[test]
    fn calibrated_count_falls_back_to_padded_rough_estimate() {
        let messages = vec![
            MessageRecord::new("u1", Role::User, "abcd"),
            MessageRecord::new("a1", Role::Assistant, "efgh"),
        ];
        assert_eq!(
            calibrated_token_count(&messages, None),
            rough_token_estimate_all(&messages)
        );
    }

    #[test]
    fn prompt_token_count_includes_summary_system_sections_restoration_and_open_tool_calls() {
        let messages = vec![MessageRecord::new("u1", Role::User, "hello")];
        let summary = SummaryBlock {
            title: "summary".to_string(),
            content: "older context".to_string(),
        };
        let system_sections = vec![SystemPromptSection {
            name: "recovered_memory".to_string(),
            content: "Recovered run memory".to_string(),
        }];
        let open_tool_calls = vec![ToolCallRecord {
            id: "call-1".to_string(),
            name: "read_file".to_string(),
            input: json!({"path":"src/main.rs"}),
            assistant_message_id: None,
            assistant_provider_response_id: None,
        }];
        let restoration = PostCompactRestoration {
            modified_files: Vec::new(),
            active_tools: Vec::new(),
            active_skills: Vec::new(),
            active_plugins: Vec::new(),
            active_mcp_tools: Vec::new(),
            mcp_server_instructions: Vec::new(),
            workspace_state: WorkspaceSnapshot {
                workspace_root: Some("/tmp".to_string()),
                git_branch: None,
                recent_read_files: Vec::new(),
                recent_modified_files: Vec::new(),
            },
            retained_user_inputs: Vec::new(),
            session_control: SessionControlState::default(),
        };
        let count = calibrated_prompt_token_count(
            Some(&summary),
            &system_sections,
            &messages,
            &open_tool_calls,
            Some(&restoration),
        );
        assert!(count > rough_token_estimate("hello"));
        assert!(count > rough_token_estimate("Recovered run memory"));
    }

    fn fat_restoration() -> PostCompactRestoration {
        // A task archive worth of metadata: the rendered prompt line only
        // carries id, status, owner, and title, so descriptions, outputs, and
        // metadata must not weigh on the estimate.
        let tasks = (0..100)
            .map(|index| kheish_types::TaskRecord {
                id: format!("task-{index}"),
                title: format!("Task {index}"),
                description: "d".repeat(2_000),
                status: kheish_types::TaskStatus::Completed,
                owner_agent_id: None,
                blocked_by: Vec::new(),
                blocks: Vec::new(),
                output: Some("o".repeat(2_000)),
                metadata: json!({ "padding": "m".repeat(1_000) }),
                created_at_ms: 1,
                updated_at_ms: 2,
            })
            .collect();
        PostCompactRestoration {
            modified_files: Vec::new(),
            active_tools: Vec::new(),
            active_skills: Vec::new(),
            active_plugins: Vec::new(),
            active_mcp_tools: Vec::new(),
            mcp_server_instructions: Vec::new(),
            workspace_state: WorkspaceSnapshot::default(),
            retained_user_inputs: Vec::new(),
            session_control: SessionControlState {
                tasks,
                ..SessionControlState::default()
            },
        }
    }

    #[test]
    fn measured_usage_anchors_the_estimate_without_double_counting() {
        // The provider already measured a prompt that contained the summary,
        // system sections, and restoration payload; re-adding component
        // estimates would keep the count above the autocompact threshold
        // forever (compaction re-attaches those components), wedging the
        // engine in a compaction loop.
        let messages = vec![
            MessageRecord::new("u1", Role::User, "hello"),
            MessageRecord::new("a1", Role::Assistant, "done").with_api_usage(ModelUsage {
                input_tokens: 150_000,
                output_tokens: 200,
                cost_usd: 0.0,
            }),
            MessageRecord::new("u2", Role::User, "tail"),
        ];
        let summary = SummaryBlock {
            title: "summary".to_string(),
            content: "s".repeat(10_000),
        };
        let count = calibrated_prompt_token_count(
            Some(&summary),
            &[],
            &messages,
            &[],
            Some(&fat_restoration()),
        );
        assert_eq!(count, 150_200 + rough_token_estimate("tail"));
    }

    #[test]
    fn restoration_is_estimated_as_rendered_text_not_raw_json() {
        let restoration = fat_restoration();
        let messages = vec![MessageRecord::new("u1", Role::User, "hello")];
        let count = calibrated_prompt_token_count(None, &[], &messages, &[], Some(&restoration));
        let raw_json_tokens = rough_token_estimate(&serde_json::to_string(&restoration).unwrap());
        // 100 tasks × ~5 KB of non-rendered fields: the raw payload estimate
        // is hundreds of thousands of tokens, the rendered form a few
        // thousand.
        assert!(raw_json_tokens > 100_000);
        assert!(
            count < raw_json_tokens / 10,
            "estimate {count} should reflect the rendered text, not the {raw_json_tokens}-token raw payload"
        );
    }
}
