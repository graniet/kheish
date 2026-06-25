use serde::{Deserialize, Serialize};

use crate::routing::InputContentPart;
use crate::session::SessionControlState;
use crate::skills::ActiveSkillSnapshot;
use crate::tools::ToolDefinition;

/// Stores one compacted history summary block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryBlock {
    pub title: String,
    pub content: String,
}

/// The trigger that caused a compaction event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    /// A manual compaction initiated by a caller.
    Manual,
    /// An automatic compaction initiated by the runtime.
    #[default]
    Auto,
}

/// A segment preserved across compaction boundaries for faithful resume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreservedSegment {
    /// The first preserved message identifier.
    pub head_uuid: String,
    /// The anchor identifier used to splice preserved content after compaction.
    pub anchor_uuid: String,
    /// The last preserved message identifier.
    pub tail_uuid: String,
}

/// Compaction metadata stored alongside one compacted checkpoint or boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactBoundaryMetadata {
    /// The trigger that caused compaction.
    pub trigger: CompactionTrigger,
    /// The rough token count observed before compaction.
    pub pre_tokens: usize,
    /// The rough token count observed after compaction.
    #[serde(default)]
    pub post_tokens: usize,
    /// The strategy used to obtain the compacted prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<CompactionStrategy>,
    /// The optional preserved tail splice information.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserved_segment: Option<PreservedSegment>,
}

/// The strategy used by one autocompact decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStrategy {
    SessionMemory,
    ModelDriven,
}

/// One non-destructive compaction boundary persisted in the session journal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompactionBoundary {
    Snip {
        turn: usize,
        messages_removed: usize,
        tokens_freed: usize,
        #[serde(default)]
        new_head_offset: u64,
    },
    Microcompact {
        turn: usize,
        cleared_tool_ids: Vec<String>,
        tokens_freed: usize,
    },
    Autocompact {
        turn: usize,
        trigger: CompactionTrigger,
        pre_tokens: usize,
        post_tokens: usize,
        strategy: CompactionStrategy,
    },
}

/// Captures one current file snapshot re-injected after compaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSnapshot {
    /// The workspace-relative path.
    pub path: String,
    /// The current file content.
    pub content: String,
}

/// Summarizes the current workspace state after one compaction boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    /// The current workspace root if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    /// The current git branch if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// The most recent workspace-relative files read by the agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_read_files: Vec<String>,
    /// The most recent workspace-relative files modified by the agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_modified_files: Vec<String>,
}

/// Carries concrete workspace state back into the prompt after compaction.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostCompactRestoration {
    /// The most relevant modified files restored verbatim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modified_files: Vec<FileSnapshot>,
    /// The currently active tools reminded to the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_tools: Vec<ToolDefinition>,
    /// The currently active reusable skills reminded to the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_skills: Vec<ActiveSkillSnapshot>,
    /// The currently active plugins reminded to the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_plugins: Vec<String>,
    /// The currently active MCP tools reminded to the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_mcp_tools: Vec<String>,
    /// MCP server instruction blocks restored after compaction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_server_instructions: Vec<String>,
    /// The current workspace snapshot.
    #[serde(default)]
    pub workspace_state: WorkspaceSnapshot,
    /// Recent user attachment-bearing inputs restored after compaction so providers can replay them faithfully.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retained_user_inputs: Vec<RetainedUserInput>,
    /// The current session control state.
    #[serde(default)]
    pub session_control: SessionControlState,
}

/// Restores one user input that still needs concrete attachment semantics after compaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedUserInput {
    /// The stable user-message identifier.
    pub message_id: String,
    /// The rendered canonical text content associated with the user message.
    pub content: String,
    /// The ordered content parts that should remain visible to the provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_parts: Vec<InputContentPart>,
}
