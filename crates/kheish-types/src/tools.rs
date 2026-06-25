use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

use crate::model::ApiUsage;

/// Internal allow-list sentinel that expands to the daemon's currently visible MCP surface.
///
/// Built-in agent profiles use this marker so their default allow-lists can include dynamic MCP
/// tools without hard-coding provider-specific names. Explicit allow-lists should normally omit it
/// when they intend to restrict MCP visibility to exact tool names.
pub const DYNAMIC_MCP_TOOL_ALLOWLIST_SENTINEL: &str = "__dynamic_mcp_tools__";

/// Declares the logical role of a message in canonical session history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
    Summary,
}

/// Stores one canonical message in session history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageRecord {
    pub id: String,
    pub role: Role,
    pub content: String,
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_usage: Option<ApiUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<u64>,
    /// Provider-native context that must be replayed with this message but is
    /// not part of the user-visible transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_context: Option<Value>,
}

impl MessageRecord {
    pub fn new(id: impl Into<String>, role: Role, content: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            role,
            content: content.into(),
            pinned: false,
            provider_response_id: None,
            api_usage: None,
            offset: None,
            timestamp_ms: None,
            provider_context: None,
        }
    }

    pub fn pinned(mut self) -> Self {
        self.pinned = true;
        self
    }

    pub fn with_api_usage(mut self, usage: ApiUsage) -> Self {
        self.api_usage = Some(usage);
        self
    }

    pub fn with_provider_response_id(mut self, provider_response_id: impl Into<String>) -> Self {
        self.provider_response_id = Some(provider_response_id.into());
        self
    }

    pub fn with_offset(mut self, offset: u64) -> Self {
        self.offset = Some(offset);
        self
    }

    pub fn with_timestamp_ms(mut self, timestamp_ms: u64) -> Self {
        self.timestamp_ms = Some(timestamp_ms);
        self
    }

    pub fn with_provider_context(mut self, provider_context: Value) -> Self {
        self.provider_context = Some(provider_context);
        self
    }
}

/// Stores one tool call emitted by the assistant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub id: String,
    pub name: String,
    pub input: Value,
    /// The assistant message that emitted this call when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_message_id: Option<String>,
    /// The provider-specific response identifier associated with the assistant turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_provider_response_id: Option<String>,
}

/// Describes one provider-facing tool that may be advertised to a model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub allows_parallel: bool,
}

/// Declares one allow/deny filter applied to an agent-specific tool surface.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSurfaceFilter {
    /// Optional allow-list. When non-empty, only these tools remain visible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowlist: Vec<String>,
    /// Optional deny-list applied after the allow-list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denylist: Vec<String>,
}

impl ToolSurfaceFilter {
    /// Returns a filter that exposes no tools.
    pub fn deny_all() -> Self {
        Self {
            allowlist: vec![DENY_ALL_ALLOWLIST_SENTINEL.to_string()],
            denylist: Vec::new(),
        }
    }

    /// Returns true when the filter explicitly denies every tool.
    pub fn is_deny_all(&self) -> bool {
        self.normalized()
            .allowlist
            .iter()
            .any(|entry| entry == DENY_ALL_ALLOWLIST_SENTINEL)
    }

    /// Returns true when the filter does not constrain the visible tool surface.
    pub fn is_empty(&self) -> bool {
        self.allowlist.is_empty() && self.denylist.is_empty()
    }

    /// Returns a normalized copy with trimmed, sorted, deduplicated entries.
    pub fn normalized(&self) -> Self {
        Self {
            allowlist: normalize_entries(&self.allowlist),
            denylist: normalize_entries(&self.denylist),
        }
    }

    /// Returns one filter that is no wider than either input filter.
    pub fn restrict_with(&self, narrower: &Self) -> Self {
        let base = self.normalized();
        let narrower = narrower.normalized();
        if base.is_deny_all() || narrower.is_deny_all() {
            return Self::deny_all();
        }
        Self {
            allowlist: intersect_allow_lists(&base.allowlist, &narrower.allowlist),
            denylist: union_lists(&base.denylist, &narrower.denylist),
        }
    }

    /// Returns whether the named tool remains visible after applying the filter.
    pub fn allows(&self, tool_name: &str) -> bool {
        let normalized = self.normalized();
        if normalized.is_deny_all() {
            return false;
        }
        let allowed = normalized.allowlist.is_empty()
            || normalized.allowlist.iter().any(|entry| entry == tool_name);
        allowed && !normalized.denylist.iter().any(|entry| entry == tool_name)
    }
}

const DENY_ALL_ALLOWLIST_SENTINEL: &str = "\0kheish_tool_surface_deny_all\0";

/// Returns whether one allow-list entry represents a concrete or helper MCP tool.
pub fn is_dynamic_mcp_tool_entry(entry: &str) -> bool {
    entry.starts_with("mcp__")
        || matches!(
            entry,
            "list_mcp_resources" | "list_mcp_resource_templates" | "read_mcp_resource"
        )
}

fn normalize_entries(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn intersect_allow_lists(left: &[String], right: &[String]) -> Vec<String> {
    if left.is_empty() {
        return right.to_vec();
    }
    if right.is_empty() {
        return left.to_vec();
    }
    let left_has_dynamic = left
        .iter()
        .any(|entry| entry == DYNAMIC_MCP_TOOL_ALLOWLIST_SENTINEL);
    let right_has_dynamic = right
        .iter()
        .any(|entry| entry == DYNAMIC_MCP_TOOL_ALLOWLIST_SENTINEL);
    let left = left.iter().collect::<BTreeSet<_>>();
    let right = right.iter().collect::<BTreeSet<_>>();
    let mut result = left
        .intersection(&right)
        .map(|entry| (*entry).clone())
        .collect::<BTreeSet<_>>();
    if left_has_dynamic {
        result.extend(
            right
                .iter()
                .filter(|entry| is_dynamic_mcp_tool_entry(entry))
                .map(|entry| (*entry).clone()),
        );
    }
    if right_has_dynamic {
        result.extend(
            left.iter()
                .filter(|entry| is_dynamic_mcp_tool_entry(entry))
                .map(|entry| (*entry).clone()),
        );
    }
    result.into_iter().collect()
}

fn union_lists(left: &[String], right: &[String]) -> Vec<String> {
    left.iter()
        .chain(right.iter())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Describes one normalized workspace-side effect emitted by a tool execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextUpdate {
    /// One file was read from the workspace.
    FileRead { path: String },
    /// One file was created or modified in the workspace.
    FileModified { path: String },
    /// The effective workspace root changed for the current session.
    WorkspaceRootChanged { path: String },
    /// One web resource was fetched or inspected.
    WebResourceVisited { uri: String },
}

/// Stores one tool result returned to the model after execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultRecord {
    pub call_id: String,
    pub output: Value,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_updates: Vec<ContextUpdate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hook_contexts: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::{DYNAMIC_MCP_TOOL_ALLOWLIST_SENTINEL, ToolSurfaceFilter};

    #[test]
    fn tool_surface_filter_restricts_by_intersection_and_union() {
        let base = ToolSurfaceFilter {
            allowlist: vec!["read_file".to_string(), "bash".to_string()],
            denylist: vec!["edit_file".to_string()],
        };
        let narrower = ToolSurfaceFilter {
            allowlist: vec!["bash".to_string(), "web_search".to_string()],
            denylist: vec!["write_file".to_string()],
        };

        let restricted = base.restrict_with(&narrower);
        assert_eq!(restricted.allowlist, vec!["bash".to_string()]);
        assert_eq!(
            restricted.denylist,
            vec!["edit_file".to_string(), "write_file".to_string()]
        );
    }

    #[test]
    fn tool_surface_filter_allows_when_not_denied() {
        let filter = ToolSurfaceFilter {
            allowlist: vec!["read_file".to_string(), "bash".to_string()],
            denylist: vec!["bash".to_string()],
        };

        assert!(filter.allows("read_file"));
        assert!(!filter.allows("bash"));
        assert!(!filter.allows("edit_file"));
    }

    #[test]
    fn tool_surface_filter_can_explicitly_deny_all_tools() {
        let deny_all = ToolSurfaceFilter::deny_all();

        assert!(deny_all.is_deny_all());
        assert!(!deny_all.is_empty());
        assert!(!deny_all.allows("read_file"));
        assert!(!deny_all.allows("bash"));

        let restricted = ToolSurfaceFilter::default().restrict_with(&deny_all);
        assert!(restricted.is_deny_all());
        assert!(!restricted.allows("read_file"));
    }

    #[test]
    fn tool_surface_filter_keeps_explicit_mcp_tools_when_parent_uses_dynamic_sentinel() {
        let parent = ToolSurfaceFilter {
            allowlist: vec![
                DYNAMIC_MCP_TOOL_ALLOWLIST_SENTINEL.to_string(),
                "list_mcp_resources".to_string(),
            ],
            denylist: vec!["bash".to_string()],
        };
        let child = ToolSurfaceFilter {
            allowlist: vec![
                "mcp__github__search_code".to_string(),
                "list_mcp_resources".to_string(),
            ],
            denylist: Vec::new(),
        };

        let restricted = parent.restrict_with(&child);
        assert_eq!(
            restricted.allowlist,
            vec![
                "list_mcp_resources".to_string(),
                "mcp__github__search_code".to_string(),
            ]
        );
        assert_eq!(restricted.denylist, vec!["bash".to_string()]);
    }
}
