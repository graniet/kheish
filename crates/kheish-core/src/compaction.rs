//! Model-driven compaction prompts and resume wrappers.

const NO_TOOLS_PREAMBLE: &str = r#"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.

- Do NOT use Read, Bash, Grep, Glob, Edit, Write, or any other tool.
- You already have all the context you need in the conversation above.
- Tool calls will be rejected and will waste your only turn.
- Your full response must be plain text: an <analysis> block followed by a <summary> block.
"#;

const NO_TOOLS_TRAILER: &str = r#"

REMINDER: Do NOT call any tools. Respond with plain text only: an <analysis> block followed by a <summary> block. Tool calls will be rejected.
"#;

const BASE_ANALYSIS_INSTRUCTION: &str = r#"Before writing the final summary, use an <analysis> block as a drafting scratchpad. In that analysis:

1. Review the conversation chronologically.
2. Capture the user's explicit requests, constraints, and feedback.
3. Capture the work completed so far, including important technical decisions.
4. Preserve critical details: file paths, commands, APIs, code edits, error messages, and fixes.
5. Identify the most recent unfinished work and the next concrete step.
6. Double-check the summary for technical accuracy and completeness.
"#;

const PARTIAL_ANALYSIS_INSTRUCTION: &str = r#"Before writing the final summary, use an <analysis> block as a drafting scratchpad. In that analysis:

1. Review the recent messages chronologically.
2. Capture the user's explicit requests, constraints, and feedback from that recent portion.
3. Capture the work completed in the recent portion, including important technical decisions.
4. Preserve critical details: file paths, commands, APIs, code edits, error messages, and fixes.
5. Identify the most recent unfinished work and the next concrete step.
6. Double-check the summary for technical accuracy and completeness.
"#;

const BASE_COMPACT_PROMPT: &str = r#"Your task is to create a detailed summary of the conversation so far so the agent can continue working without losing context.

The summary must preserve the user's intent, the technical state, and the exact place where work stopped.

Your <summary> must contain these sections:

1. Primary Request and Intent
2. Key Technical Concepts
3. Files, Commands, and Tools Used
4. Errors and Fixes
5. Problem Solving
6. All User Messages
7. Pending Tasks
8. Current Work
9. Optional Next Step
"#;

const PARTIAL_COMPACT_PROMPT: &str = r#"Your task is to create a detailed summary of the recent portion of the conversation only.

Earlier retained context will remain available after compaction and does not need to be re-summarized. Focus on what happened in the recent messages and where the current work stopped.

Your <summary> must contain these sections:

1. Primary Request and Intent
2. Key Technical Concepts
3. Files, Commands, and Tools Used
4. Errors and Fixes
5. Problem Solving
6. All User Messages
7. Pending Tasks
8. Current Work
9. Optional Next Step
"#;

/// Returns the dedicated system prompt used for model-driven compaction.
pub fn build_compaction_system_prompt() -> String {
    "You are Kheish's compaction engine. Produce faithful resume summaries. Respond with text only and never call tools."
        .to_string()
}

/// Returns the dedicated user instruction for one model-driven compaction turn.
pub fn build_compaction_user_prompt(has_retained_context: bool) -> String {
    let mut prompt = String::from(NO_TOOLS_PREAMBLE);
    if has_retained_context {
        prompt.push_str(PARTIAL_COMPACT_PROMPT);
        prompt.push_str("\n\n");
        prompt.push_str(PARTIAL_ANALYSIS_INSTRUCTION);
    } else {
        prompt.push_str(BASE_COMPACT_PROMPT);
        prompt.push_str("\n\n");
        prompt.push_str(BASE_ANALYSIS_INSTRUCTION);
    }
    prompt.push_str(NO_TOOLS_TRAILER);
    prompt
}

/// Formats one raw compaction response into the stored summary text.
pub fn format_compaction_summary(raw: &str) -> String {
    let mut formatted = raw.trim().to_string();
    if let Some((_, summary)) = extract_tag_content(&formatted, "summary") {
        formatted = format!("Summary:\n{}", summary.trim());
    }
    strip_analysis_block(&formatted).trim().to_string()
}

/// Renders a compacted summary as a resume message for subsequent turns.
pub fn build_compaction_resume_message(summary: &str, recent_messages_preserved: bool) -> String {
    let mut rendered = format!(
        "This session is being continued from an earlier conversation that ran out of context.\n\n{}",
        format_compaction_summary(summary)
    );
    if recent_messages_preserved {
        rendered.push_str("\n\nRecent messages are preserved verbatim.");
    }
    rendered.push_str(
        "\n\nContinue the conversation from where it left off without asking the user to repeat context. Resume directly, do not acknowledge the summary, and do not narrate that you are resuming.",
    );
    rendered
}

fn strip_analysis_block(value: &str) -> String {
    if let Some(start) = value.find("<analysis>") {
        if let Some(end_relative) = value[start..].find("</analysis>") {
            let end = start + end_relative + "</analysis>".len();
            let mut rendered = String::with_capacity(value.len());
            rendered.push_str(value[..start].trim_end());
            if !rendered.is_empty() && !value[end..].trim().is_empty() {
                rendered.push_str("\n\n");
            }
            rendered.push_str(value[end..].trim_start());
            return rendered;
        }
    }
    value.to_string()
}

fn extract_tag_content<'a>(value: &'a str, tag: &str) -> Option<(&'a str, &'a str)> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = value.find(&open)?;
    let after_open = start + open.len();
    let end_relative = value[after_open..].find(&close)?;
    let end = after_open + end_relative;
    Some((&value[..start], &value[after_open..end]))
}

#[cfg(test)]
mod tests {
    use super::{
        build_compaction_resume_message, build_compaction_system_prompt,
        build_compaction_user_prompt, format_compaction_summary,
    };

    #[test]
    fn compaction_prompt_disallows_tools() {
        let system = build_compaction_system_prompt();
        let user = build_compaction_user_prompt(false);
        assert!(system.contains("never call tools"));
        assert!(user.contains("Do NOT call any tools"));
        assert!(user.contains("<summary>"));
        assert!(user.contains("Pending Tasks"));
    }

    #[test]
    fn partial_compaction_prompt_mentions_recent_messages() {
        let prompt = build_compaction_user_prompt(true);
        assert!(prompt.contains("recent portion of the conversation"));
        assert!(prompt.contains("recent messages"));
    }

    #[test]
    fn compaction_summary_strips_analysis_and_summary_tags() {
        let raw = r#"<analysis>
draft
</analysis>

<summary>
1. Primary Request and Intent
- Do the work
</summary>"#;
        let formatted = format_compaction_summary(raw);
        assert!(!formatted.contains("<analysis>"));
        assert!(!formatted.contains("<summary>"));
        assert!(formatted.contains("Summary:"));
        assert!(formatted.contains("Primary Request and Intent"));
    }

    #[test]
    fn compaction_resume_message_instructs_direct_resume() {
        let rendered = build_compaction_resume_message(
            "<summary>\n1. Current Work\n- Finish the patch\n</summary>",
            true,
        );
        assert!(rendered.contains("Recent messages are preserved verbatim."));
        assert!(rendered.contains("Continue the conversation from where it left off"));
        assert!(rendered.contains("Finish the patch"));
    }
}
