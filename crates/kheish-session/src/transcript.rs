use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{CompactBoundaryMetadata, CompactionTrigger, PreservedSegment};

/// A normalized tool call extracted from a transcript message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedToolCall {
    /// The tool call identifier.
    pub id: String,
    /// The tool name.
    pub name: String,
    /// The canonical input payload.
    pub input: Value,
}

/// A normalized tool result extracted from a transcript message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedToolResult {
    /// The corresponding tool call identifier.
    pub tool_use_id: String,
    /// Whether the result is marked as an error.
    pub is_error: bool,
    /// The normalized result payload.
    pub content: Value,
}

/// A single transcript node parsed from JSONL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptNode {
    /// The transcript UUID.
    pub uuid: String,
    /// The raw transcript type.
    pub type_name: String,
    /// The optional system subtype.
    pub subtype: Option<String>,
    /// The direct parent UUID.
    pub parent_uuid: Option<String>,
    /// The logical parent UUID used for resume relinking.
    pub logical_parent_uuid: Option<String>,
    /// The optional originating assistant UUID for tool-result relinking.
    pub source_tool_assistant_uuid: Option<String>,
    /// UUIDs removed by a snip event.
    pub removed_uuids: Vec<String>,
    /// The compact-boundary metadata when the node represents a boundary.
    pub compact_metadata: Option<CompactBoundaryMetadata>,
    /// The optional assistant message identifier used for split message merging.
    pub message_id: Option<String>,
    /// Whether the node belongs to a sidechain transcript.
    pub is_sidechain: bool,
    /// The extracted visible text.
    pub text: Option<String>,
    /// The tool calls extracted from the node.
    pub tool_calls: Vec<ParsedToolCall>,
    /// The tool results extracted from the node.
    pub tool_results: Vec<ParsedToolResult>,
}

/// A normalized transcript message merged across split assistant blocks when possible.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedTranscriptMessage {
    /// The first UUID contributing to this normalized message.
    pub uuid: String,
    /// The normalized role.
    pub role: String,
    /// The merged text payload.
    pub text: String,
    /// The merged tool calls.
    pub tool_calls: Vec<ParsedToolCall>,
    /// The merged tool results.
    pub tool_results: Vec<ParsedToolResult>,
}

/// A parsed transcript graph with parent and logical-parent relationships.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TranscriptGraph {
    /// The ordered node UUIDs as they appear in the JSONL file.
    pub order: Vec<String>,
    /// The nodes keyed by UUID.
    pub nodes: BTreeMap<String, TranscriptNode>,
}

impl TranscriptGraph {
    /// Parses a Claude Code style JSONL transcript into a normalized graph.
    pub fn from_jsonl(raw: &str) -> Result<Self> {
        let mut graph = Self::default();
        for line in raw.lines().filter(|line| !line.trim().is_empty()) {
            let value = serde_json::from_str::<Value>(line)?;
            let Some(type_name) = string_field(&value, "type") else {
                continue;
            };
            if !matches!(
                type_name.as_str(),
                "user" | "assistant" | "system" | "attachment" | "progress" | "tool_use_summary"
            ) {
                continue;
            }
            let Some(uuid) = string_field(&value, "uuid") else {
                continue;
            };

            let node = TranscriptNode {
                uuid: uuid.clone(),
                type_name,
                subtype: string_field(&value, "subtype"),
                parent_uuid: optional_string_field(&value, "parentUuid"),
                logical_parent_uuid: optional_string_field(&value, "logicalParentUuid"),
                source_tool_assistant_uuid: optional_string_field(
                    &value,
                    "sourceToolAssistantUUID",
                )
                .or_else(|| optional_string_field(&value, "sourceToolAssistantUuid")),
                removed_uuids: string_array_field(&value, "removedUuids")
                    .or_else(|| string_array_field(&value, "removed_uuids"))
                    .unwrap_or_default(),
                compact_metadata: parse_compact_metadata(&value),
                message_id: value
                    .get("message")
                    .and_then(|message| string_field(message, "id")),
                is_sidechain: value
                    .get("isSidechain")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                text: extract_visible_text(&value),
                tool_calls: extract_tool_calls(&value),
                tool_results: extract_tool_results(&value),
            };
            graph.order.push(uuid.clone());
            graph.nodes.insert(uuid, node);
        }
        Ok(graph)
    }

    /// Returns a best-effort normalized timeline for visible user and assistant messages.
    pub fn normalized_timeline(&self) -> Vec<NormalizedTranscriptMessage> {
        self.normalize_visible_nodes(self.visible_nodes_in_order())
    }

    /// Returns a visible timeline repaired for split assistants and parallel tool DAGs.
    pub fn recovered_timeline(&self) -> Vec<NormalizedTranscriptMessage> {
        let selected = self.recovered_visible_uuid_set(None);
        let nodes = self
            .order
            .iter()
            .filter(|uuid| selected.contains(*uuid))
            .filter_map(|uuid| self.nodes.get(uuid).cloned())
            .collect();
        self.normalize_visible_nodes(nodes)
    }

    /// Returns a sanitized resume timeline suitable for rehydrating a model prompt.
    pub fn sanitized_resume_timeline(&self) -> Vec<NormalizedTranscriptMessage> {
        let boundary_scope = self.resume_window_uuids();
        let mut timeline = self.normalize_visible_nodes(
            self.order
                .iter()
                .filter(|uuid| {
                    boundary_scope
                        .as_ref()
                        .map(|selected| selected.contains(*uuid))
                        .unwrap_or(true)
                })
                .filter_map(|uuid| self.nodes.get(uuid).cloned())
                .collect(),
        );
        timeline = self.repair_parallel_assistant_dag(timeline);
        sanitize_timeline(timeline)
    }

    /// Returns the active conversation chain using parent and logical-parent links.
    pub fn active_chain(&self) -> Vec<TranscriptNode> {
        let leaves = self.leaf_nodes();
        let Some(leaf) = leaves
            .into_iter()
            .rev()
            .find(|node| is_chain_candidate(node))
        else {
            return Vec::new();
        };

        let mut chain = Vec::new();
        let mut seen = BTreeSet::new();
        let mut current = Some(leaf);
        while let Some(node) = current {
            if !seen.insert(node.uuid.clone()) {
                break;
            }
            if is_chain_visible(&node) {
                chain.push(node.clone());
            }
            current = self.resolve_parent_node(&node);
        }
        chain.reverse();
        chain
    }

    fn visible_nodes_in_order(&self) -> Vec<TranscriptNode> {
        self.order
            .iter()
            .filter_map(|uuid| self.nodes.get(uuid))
            .filter(|node| !node.is_sidechain && is_visible_message(node))
            .cloned()
            .collect()
    }

    fn normalize_visible_nodes(
        &self,
        nodes: Vec<TranscriptNode>,
    ) -> Vec<NormalizedTranscriptMessage> {
        let mut merged: Vec<NormalizedTranscriptMessage> = Vec::new();
        for node in nodes {
            if node.is_sidechain || !is_visible_message(&node) {
                continue;
            }
            let role = node.type_name.clone();
            let text = node.text.clone().unwrap_or_default();

            if let Some(last) = merged.last_mut() {
                let can_merge_assistant = role == "assistant"
                    && last.role == "assistant"
                    && node.message_id.is_some()
                    && self
                        .nodes
                        .get(&last.uuid)
                        .and_then(|message| message.message_id.as_ref())
                        == node.message_id.as_ref();
                if can_merge_assistant {
                    merge_message(last, &node);
                    continue;
                }
            }

            merged.push(NormalizedTranscriptMessage {
                uuid: node.uuid.clone(),
                role,
                text,
                tool_calls: node.tool_calls.clone(),
                tool_results: node.tool_results.clone(),
            });
        }
        merged
    }

    fn repair_parallel_assistant_dag(
        &self,
        mut timeline: Vec<NormalizedTranscriptMessage>,
    ) -> Vec<NormalizedTranscriptMessage> {
        let selected = self.recovered_visible_uuid_set(self.resume_window_uuids());
        let recovered = self.normalize_visible_nodes(
            self.order
                .iter()
                .filter(|uuid| selected.contains(*uuid))
                .filter_map(|uuid| self.nodes.get(uuid).cloned())
                .collect(),
        );
        if recovered.len() > timeline.len() {
            timeline = recovered;
        }
        timeline
    }

    fn recovered_visible_uuid_set(
        &self,
        initial_scope: Option<BTreeSet<String>>,
    ) -> BTreeSet<String> {
        let mut selected = self
            .active_chain()
            .into_iter()
            .filter(|node| is_visible_message(node))
            .map(|node| node.uuid)
            .collect::<BTreeSet<_>>();
        if let Some(ref scope) = initial_scope {
            selected.retain(|uuid| scope.contains(uuid));
        }

        let mut assistant_message_ids = selected
            .iter()
            .filter_map(|uuid| self.nodes.get(uuid))
            .filter_map(|node| node.message_id.clone())
            .collect::<BTreeSet<_>>();
        let mut changed = true;
        while changed {
            changed = false;
            for uuid in &self.order {
                let Some(node) = self.nodes.get(uuid) else {
                    continue;
                };
                if node.is_sidechain {
                    continue;
                }
                if initial_scope
                    .as_ref()
                    .map(|scope| !scope.contains(uuid))
                    .unwrap_or(false)
                {
                    continue;
                }

                let split_assistant = node.type_name == "assistant"
                    && node
                        .message_id
                        .as_ref()
                        .map(|message_id| assistant_message_ids.contains(message_id))
                        .unwrap_or(false);
                let related_tool_result = !node.tool_results.is_empty()
                    && node
                        .source_tool_assistant_uuid
                        .as_ref()
                        .map(|assistant_uuid| selected.contains(assistant_uuid))
                        .or_else(|| {
                            node.parent_uuid
                                .as_ref()
                                .map(|parent_uuid| selected.contains(parent_uuid))
                        })
                        .unwrap_or(false);

                if (split_assistant || related_tool_result) && selected.insert(node.uuid.clone()) {
                    changed = true;
                    if let Some(message_id) = &node.message_id {
                        assistant_message_ids.insert(message_id.clone());
                    }
                }
            }
        }

        selected
    }

    fn resolve_parent_node(&self, node: &TranscriptNode) -> Option<TranscriptNode> {
        let relinks = self.removed_uuid_relinks();
        let mut parent_uuid = effective_parent_uuid(node);
        while let Some(uuid) = parent_uuid {
            let Some(parent) = self.nodes.get(&uuid).cloned().or_else(|| {
                relinks
                    .get(&uuid)
                    .and_then(|replacement| replacement.as_ref())
                    .and_then(|replacement| self.nodes.get(replacement))
                    .cloned()
            }) else {
                return None;
            };
            if parent.type_name == "progress" {
                parent_uuid = effective_parent_uuid(&parent);
                continue;
            }
            return Some(parent);
        }
        None
    }

    fn removed_uuid_relinks(&self) -> BTreeMap<String, Option<String>> {
        let mut relinks = BTreeMap::new();
        for node in self.nodes.values() {
            if node.subtype.as_deref() != Some("snip") {
                continue;
            }
            let replacement = node
                .logical_parent_uuid
                .clone()
                .or_else(|| node.parent_uuid.clone());
            for removed_uuid in &node.removed_uuids {
                relinks.insert(removed_uuid.clone(), replacement.clone());
            }
        }
        relinks
    }

    fn leaf_nodes(&self) -> Vec<TranscriptNode> {
        let mut parent_uuids = BTreeSet::new();
        for node in self.nodes.values() {
            if let Some(parent_uuid) = effective_parent_uuid(node) {
                parent_uuids.insert(parent_uuid);
            }
        }
        self.order
            .iter()
            .filter_map(|uuid| self.nodes.get(uuid))
            .filter(|node| !node.is_sidechain && !parent_uuids.contains(&node.uuid))
            .cloned()
            .collect()
    }

    fn resume_window_uuids(&self) -> Option<BTreeSet<String>> {
        let last_boundary_index = self.order.iter().rposition(|uuid| {
            self.nodes
                .get(uuid)
                .map(|node| node.subtype.as_deref() == Some("compact_boundary"))
                .unwrap_or(false)
        })?;
        let boundary = self.nodes.get(&self.order[last_boundary_index])?;
        let mut selected = self
            .order
            .iter()
            .skip(last_boundary_index + 1)
            .filter_map(|uuid| self.nodes.get(uuid))
            .filter(|node| !node.is_sidechain && is_visible_message(node))
            .map(|node| node.uuid.clone())
            .collect::<BTreeSet<_>>();

        if let Some(segment) = boundary
            .compact_metadata
            .as_ref()
            .and_then(|metadata| metadata.preserved_segment.as_ref())
        {
            let Some(preserved) = self.preserved_window(segment) else {
                return None;
            };
            selected.extend(preserved);
        }

        Some(selected)
    }

    fn preserved_window(&self, segment: &PreservedSegment) -> Option<Vec<String>> {
        if !self.nodes.contains_key(&segment.head_uuid)
            || !self.nodes.contains_key(&segment.anchor_uuid)
            || !self.nodes.contains_key(&segment.tail_uuid)
        {
            return None;
        }

        if segment.head_uuid != segment.anchor_uuid {
            let head = self.nodes.get(&segment.head_uuid)?;
            let head_parent = self.resolve_parent_node(head)?;
            if head_parent.uuid != segment.anchor_uuid {
                return None;
            }
        }

        let mut chain = Vec::new();
        let mut seen = BTreeSet::new();
        let mut current = self.nodes.get(&segment.tail_uuid).cloned();
        while let Some(node) = current {
            if !seen.insert(node.uuid.clone()) {
                return None;
            }
            if !node.is_sidechain && is_visible_message(&node) {
                chain.push(node.uuid.clone());
            }
            if node.uuid == segment.head_uuid {
                chain.reverse();
                return Some(chain);
            }
            current = self.resolve_parent_node(&node);
        }

        None
    }
}

fn merge_message(target: &mut NormalizedTranscriptMessage, node: &TranscriptNode) {
    if let Some(text) = &node.text {
        if !text.trim().is_empty() {
            if !target.text.is_empty() {
                target.text.push('\n');
            }
            target.text.push_str(text.trim());
        }
    }
    target.tool_calls.extend(node.tool_calls.clone());
    target.tool_results.extend(node.tool_results.clone());
}

fn sanitize_timeline(
    timeline: Vec<NormalizedTranscriptMessage>,
) -> Vec<NormalizedTranscriptMessage> {
    let mut sanitized = Vec::new();
    let mut open_tool_calls = BTreeMap::new();

    for mut message in timeline {
        let has_content = !message.text.trim().is_empty()
            || !message.tool_calls.is_empty()
            || !message.tool_results.is_empty();
        if !has_content {
            continue;
        }

        if message.role == "assistant" {
            message.tool_calls = dedupe_tool_calls(message.tool_calls);
            for call in &message.tool_calls {
                open_tool_calls
                    .entry(call.id.clone())
                    .or_insert_with(|| call.clone());
            }
            sanitized.push(message);
            continue;
        }

        if message.role == "user" {
            message.tool_results = dedupe_tool_results(message.tool_results)
                .into_iter()
                .filter(|result| open_tool_calls.remove(&result.tool_use_id).is_some())
                .collect();
            if !message.text.trim().is_empty() || !message.tool_results.is_empty() {
                sanitized.extend(split_user_message(message));
            }
        }
    }

    if !open_tool_calls.is_empty() {
        sanitized.push(NormalizedTranscriptMessage {
            uuid: "synthetic-missing-tool-results".to_string(),
            role: "user".to_string(),
            text: String::new(),
            tool_calls: Vec::new(),
            tool_results: open_tool_calls
                .into_values()
                .map(|call| ParsedToolResult {
                    tool_use_id: call.id,
                    is_error: true,
                    content: serde_json::json!({
                        "error": "tool result missing during resume",
                        "tool": call.name,
                    }),
                })
                .collect(),
        });
    }

    let mut merged = merge_adjacent_roles(sanitized);
    if merged
        .last()
        .map(|message| message.role.as_str() == "assistant")
        .unwrap_or(false)
    {
        merged.push(NormalizedTranscriptMessage {
            uuid: "synthetic-continue".to_string(),
            role: "user".to_string(),
            text: "Continue from where you left off.".to_string(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        });
    }
    merged
}

fn dedupe_tool_calls(tool_calls: Vec<ParsedToolCall>) -> Vec<ParsedToolCall> {
    let mut seen = BTreeSet::new();
    tool_calls
        .into_iter()
        .filter(|call| seen.insert(call.id.clone()))
        .collect()
}

fn dedupe_tool_results(tool_results: Vec<ParsedToolResult>) -> Vec<ParsedToolResult> {
    let mut seen = BTreeSet::new();
    tool_results
        .into_iter()
        .filter(|result| seen.insert(result.tool_use_id.clone()))
        .collect()
}

fn merge_adjacent_roles(
    messages: Vec<NormalizedTranscriptMessage>,
) -> Vec<NormalizedTranscriptMessage> {
    let mut merged: Vec<NormalizedTranscriptMessage> = Vec::new();
    for message in messages {
        if let Some(previous) = merged.last_mut() {
            if can_merge_adjacent_messages(previous, &message) {
                if !message.text.is_empty() {
                    if !previous.text.is_empty() {
                        previous.text.push('\n');
                    }
                    previous.text.push_str(&message.text);
                }
                previous.tool_calls.extend(message.tool_calls);
                previous.tool_results.extend(message.tool_results);
                continue;
            }
        }
        merged.push(message);
    }
    merged
}

fn split_user_message(message: NormalizedTranscriptMessage) -> Vec<NormalizedTranscriptMessage> {
    if message.role != "user" || message.text.trim().is_empty() || message.tool_results.is_empty() {
        return vec![message];
    }

    vec![
        NormalizedTranscriptMessage {
            uuid: format!("{}-tool-results", message.uuid),
            role: message.role.clone(),
            text: String::new(),
            tool_calls: Vec::new(),
            tool_results: message.tool_results,
        },
        NormalizedTranscriptMessage {
            uuid: format!("{}-text", message.uuid),
            role: message.role,
            text: message.text,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        },
    ]
}

fn can_merge_adjacent_messages(
    previous: &NormalizedTranscriptMessage,
    current: &NormalizedTranscriptMessage,
) -> bool {
    if previous.role != current.role {
        return false;
    }
    if previous.role != "user" {
        return true;
    }

    let previous_is_tool_result_turn =
        previous.text.trim().is_empty() && !previous.tool_results.is_empty();
    let current_is_tool_result_turn =
        current.text.trim().is_empty() && !current.tool_results.is_empty();

    previous_is_tool_result_turn == current_is_tool_result_turn
}

fn parse_compact_metadata(value: &Value) -> Option<CompactBoundaryMetadata> {
    let metadata = value.get("compact_metadata")?;
    let trigger = match string_field(metadata, "trigger").as_deref() {
        Some("manual") => CompactionTrigger::Manual,
        _ => CompactionTrigger::Auto,
    };
    let pre_tokens = metadata
        .get("pre_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default() as usize;
    let post_tokens = metadata
        .get("post_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default() as usize;
    let strategy = metadata
        .get("strategy")
        .and_then(Value::as_str)
        .and_then(|value| match value {
            "session_memory" => Some(kheish_types::CompactionStrategy::SessionMemory),
            "model_driven" => Some(kheish_types::CompactionStrategy::ModelDriven),
            _ => None,
        });
    let preserved_segment = metadata
        .get("preserved_segment")
        .or_else(|| metadata.get("preservedSegment"))
        .and_then(Value::as_object)
        .map(|segment| PreservedSegment {
            head_uuid: string_field(&Value::Object(segment.clone()), "head_uuid")
                .or_else(|| string_field(&Value::Object(segment.clone()), "headUuid"))
                .unwrap_or_default(),
            anchor_uuid: string_field(&Value::Object(segment.clone()), "anchor_uuid")
                .or_else(|| string_field(&Value::Object(segment.clone()), "anchorUuid"))
                .unwrap_or_default(),
            tail_uuid: string_field(&Value::Object(segment.clone()), "tail_uuid")
                .or_else(|| string_field(&Value::Object(segment.clone()), "tailUuid"))
                .unwrap_or_default(),
        })
        .filter(|segment| {
            !segment.head_uuid.is_empty()
                && !segment.anchor_uuid.is_empty()
                && !segment.tail_uuid.is_empty()
        });

    Some(CompactBoundaryMetadata {
        trigger,
        pre_tokens,
        post_tokens,
        strategy,
        preserved_segment,
    })
}

fn effective_parent_uuid(node: &TranscriptNode) -> Option<String> {
    if node.subtype.as_deref() == Some("compact_boundary") {
        return node
            .logical_parent_uuid
            .clone()
            .or_else(|| node.parent_uuid.clone());
    }

    if !node.tool_results.is_empty() {
        return node
            .source_tool_assistant_uuid
            .clone()
            .or_else(|| node.parent_uuid.clone())
            .or_else(|| node.logical_parent_uuid.clone());
    }

    node.parent_uuid
        .clone()
        .or_else(|| node.logical_parent_uuid.clone())
}

fn extract_visible_text(value: &Value) -> Option<String> {
    let content = value
        .get("message")
        .and_then(|message| message.get("content"))?;
    match content {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Array(blocks) => {
            let text = blocks
                .iter()
                .filter_map(|block| match string_field(block, "type").as_deref() {
                    Some("text") => string_field(block, "text"),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            Some(text.trim().to_string()).filter(|text| !text.is_empty())
        }
        _ => None,
    }
}

fn extract_tool_calls(value: &Value) -> Vec<ParsedToolCall> {
    value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| string_field(block, "type").as_deref() == Some("tool_use"))
        .map(|block| ParsedToolCall {
            id: string_field(block, "id").unwrap_or_default(),
            name: string_field(block, "name").unwrap_or_default(),
            input: block.get("input").cloned().unwrap_or(Value::Null),
        })
        .collect()
}

fn extract_tool_results(value: &Value) -> Vec<ParsedToolResult> {
    let Some(blocks) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    blocks
        .iter()
        .filter(|block| string_field(block, "type").as_deref() == Some("tool_result"))
        .map(|block| ParsedToolResult {
            tool_use_id: string_field(block, "tool_use_id").unwrap_or_default(),
            is_error: block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            content: value
                .get("tool_use_result")
                .cloned()
                .or_else(|| {
                    block.get("content").and_then(|content| match content {
                        Value::String(text) => serde_json::from_str(text)
                            .ok()
                            .or_else(|| Some(Value::String(text.clone()))),
                        other => Some(other.clone()),
                    })
                })
                .unwrap_or(Value::Null),
        })
        .collect()
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn string_array_field(value: &Value, key: &str) -> Option<Vec<String>> {
    value.get(key).and_then(Value::as_array).map(|values| {
        values
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect()
    })
}

fn optional_string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(|field| match field {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        _ => None,
    })
}

fn is_visible_message(node: &TranscriptNode) -> bool {
    matches!(node.type_name.as_str(), "user" | "assistant")
}

fn is_chain_candidate(node: &TranscriptNode) -> bool {
    matches!(
        node.type_name.as_str(),
        "user" | "assistant" | "system" | "progress"
    )
}

fn is_chain_visible(node: &TranscriptNode) -> bool {
    matches!(node.type_name.as_str(), "user" | "assistant" | "system")
        && node.subtype.as_deref() != Some("snip")
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::TranscriptGraph;
    use crate::PreservedSegment;

    #[test]
    fn transcript_merges_split_assistant_blocks() -> Result<()> {
        let raw = r#"{"parentUuid":null,"isSidechain":false,"type":"user","uuid":"u1","message":{"content":[{"type":"text","text":"hello"}]}}
{"parentUuid":"u1","isSidechain":false,"type":"assistant","uuid":"a1","message":{"id":"m1","content":[{"type":"text","text":"part one"}]}}
{"parentUuid":"a1","isSidechain":false,"type":"assistant","uuid":"a2","message":{"id":"m1","content":[{"type":"text","text":"part two"},{"type":"tool_use","id":"call-1","name":"read","input":{"path":"a"}}]}}"#;

        let graph = TranscriptGraph::from_jsonl(raw)?;
        let timeline = graph.normalized_timeline();
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline[1].text, "part one\npart two");
        assert_eq!(timeline[1].tool_calls.len(), 1);
        Ok(())
    }

    #[test]
    fn transcript_active_chain_uses_logical_parent_for_resume() -> Result<()> {
        let raw = r#"{"parentUuid":null,"isSidechain":false,"type":"user","uuid":"u1","message":{"content":[{"type":"text","text":"hello"}]}}
{"parentUuid":null,"logicalParentUuid":"u1","isSidechain":false,"type":"system","subtype":"compact_boundary","uuid":"b1"}
{"parentUuid":"b1","isSidechain":false,"type":"assistant","uuid":"a1","message":{"id":"m1","content":[{"type":"text","text":"done"}]}}"#;

        let graph = TranscriptGraph::from_jsonl(raw)?;
        let chain = graph.active_chain();
        assert_eq!(
            chain
                .iter()
                .map(|node| node.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["u1", "b1", "a1"]
        );
        Ok(())
    }

    #[test]
    fn transcript_ignores_sidechain_leaves_in_main_chain() -> Result<()> {
        let raw = r#"{"parentUuid":null,"isSidechain":false,"type":"user","uuid":"u1","message":{"content":[{"type":"text","text":"main"}]}}
{"parentUuid":"u1","isSidechain":true,"type":"assistant","uuid":"a-side","message":{"id":"m-side","content":[{"type":"text","text":"side"}]}}
{"parentUuid":"u1","isSidechain":false,"type":"assistant","uuid":"a-main","message":{"id":"m-main","content":[{"type":"text","text":"main reply"}]}}"#;

        let graph = TranscriptGraph::from_jsonl(raw)?;
        let chain = graph.active_chain();
        assert_eq!(
            chain
                .iter()
                .map(|node| node.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["u1", "a-main"]
        );
        Ok(())
    }

    #[test]
    fn transcript_bridges_progress_nodes_in_active_chain() -> Result<()> {
        let raw = r#"{"parentUuid":null,"isSidechain":false,"type":"user","uuid":"u1","message":{"content":"hello"}}
{"parentUuid":"u1","isSidechain":false,"type":"progress","uuid":"p1"}
{"parentUuid":"p1","isSidechain":false,"type":"assistant","uuid":"a1","message":{"id":"m1","content":[{"type":"text","text":"done"}]}}"#;

        let graph = TranscriptGraph::from_jsonl(raw)?;
        let chain = graph.active_chain();
        assert_eq!(
            chain
                .iter()
                .map(|node| node.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["u1", "a1"]
        );
        Ok(())
    }

    #[test]
    fn transcript_recovers_parallel_tool_dag() -> Result<()> {
        let raw = r#"{"parentUuid":null,"isSidechain":false,"type":"user","uuid":"u1","message":{"content":"hello"}}
{"parentUuid":"u1","isSidechain":false,"type":"assistant","uuid":"a1","message":{"id":"m1","content":[{"type":"text","text":"chunk one"},{"type":"tool_use","id":"call-1","name":"read","input":{"path":"a"}}]}}
{"parentUuid":"u1","isSidechain":false,"type":"assistant","uuid":"a2","message":{"id":"m1","content":[{"type":"text","text":"chunk two"},{"type":"tool_use","id":"call-2","name":"read","input":{"path":"b"}}]}}
{"parentUuid":"a1","sourceToolAssistantUUID":"a1","isSidechain":false,"type":"user","uuid":"r1","tool_use_result":{"value":"a"},"message":{"content":[{"type":"tool_result","tool_use_id":"call-1","is_error":false,"content":{"value":"a"}}]}}
{"parentUuid":"a2","sourceToolAssistantUUID":"a2","isSidechain":false,"type":"user","uuid":"r2","tool_use_result":{"value":"b"},"message":{"content":[{"type":"tool_result","tool_use_id":"call-2","is_error":false,"content":{"value":"b"}}]}}"#;

        let graph = TranscriptGraph::from_jsonl(raw)?;
        let timeline = graph.recovered_timeline();
        assert_eq!(timeline.len(), 4);
        assert_eq!(timeline[1].tool_calls.len(), 2);
        assert_eq!(timeline[2].tool_results[0].tool_use_id, "call-1");
        assert_eq!(timeline[3].tool_results[0].tool_use_id, "call-2");
        Ok(())
    }

    #[test]
    fn transcript_sanitizes_missing_and_duplicate_tool_pairs() -> Result<()> {
        let raw = r#"{"parentUuid":null,"isSidechain":false,"type":"user","uuid":"u1","message":{"content":"hello"}}
{"parentUuid":"u1","isSidechain":false,"type":"assistant","uuid":"a1","message":{"id":"m1","content":[{"type":"text","text":"thinking"},{"type":"tool_use","id":"call-1","name":"read","input":{"path":"a"}},{"type":"tool_use","id":"call-1","name":"read","input":{"path":"a"}}]}}
{"parentUuid":"a1","isSidechain":false,"type":"user","uuid":"r1","message":{"content":[{"type":"tool_result","tool_use_id":"orphan","is_error":false,"content":"x"},{"type":"tool_result","tool_use_id":"orphan","is_error":false,"content":"x"}]}}"#;

        let graph = TranscriptGraph::from_jsonl(raw)?;
        let timeline = graph.sanitized_resume_timeline();
        assert_eq!(timeline.len(), 3);
        assert_eq!(timeline[1].tool_calls.len(), 1);
        assert_eq!(timeline[2].role, "user");
        assert_eq!(timeline[2].tool_results.len(), 1);
        assert_eq!(timeline[2].tool_results[0].tool_use_id, "call-1");
        assert!(timeline[2].tool_results[0].is_error);
        Ok(())
    }

    #[test]
    fn transcript_relinks_snipped_parent_gaps() -> Result<()> {
        let raw = r#"{"parentUuid":null,"isSidechain":false,"type":"user","uuid":"u1","message":{"content":"hello"}}
{"parentUuid":null,"logicalParentUuid":"u1","isSidechain":false,"type":"system","subtype":"snip","removedUuids":["a1"],"uuid":"s1"}
{"parentUuid":"a1","isSidechain":false,"type":"assistant","uuid":"a2","message":{"id":"m2","content":[{"type":"text","text":"after snip"}]}}"#;

        let graph = TranscriptGraph::from_jsonl(raw)?;
        let chain = graph.active_chain();
        assert_eq!(
            chain
                .iter()
                .map(|node| node.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["u1", "a2"]
        );
        Ok(())
    }

    #[test]
    fn transcript_fails_open_when_preserved_segment_is_broken() -> Result<()> {
        let raw = r#"{"parentUuid":null,"isSidechain":false,"type":"user","uuid":"u1","message":{"content":"one"}}
{"parentUuid":"u1","isSidechain":false,"type":"assistant","uuid":"a1","message":{"id":"m1","content":[{"type":"text","text":"two"}]}}
{"parentUuid":"a1","isSidechain":false,"type":"user","uuid":"u2","message":{"content":"three"}}
{"parentUuid":null,"logicalParentUuid":"u2","isSidechain":false,"type":"system","subtype":"compact_boundary","uuid":"b1","compact_metadata":{"trigger":"auto","pre_tokens":12,"preserved_segment":{"head_uuid":"missing","anchor_uuid":"a1","tail_uuid":"u2"}}}
{"parentUuid":"b1","isSidechain":false,"type":"assistant","uuid":"a2","message":{"id":"m2","content":[{"type":"text","text":"four"}]}}"#;

        let graph = TranscriptGraph::from_jsonl(raw)?;
        let timeline = graph.sanitized_resume_timeline();
        assert!(timeline.iter().any(|message| message.uuid == "u1"));
        assert!(timeline.iter().any(|message| message.uuid == "a2"));
        Ok(())
    }

    #[test]
    fn transcript_preserved_window_follows_parent_chain_not_file_slice() -> Result<()> {
        let raw = r#"{"parentUuid":null,"isSidechain":false,"type":"user","uuid":"u1","message":{"content":"one"}}
{"parentUuid":"u1","isSidechain":false,"type":"assistant","uuid":"a1","message":{"id":"m1","content":[{"type":"text","text":"two"}]}}
{"parentUuid":"a1","isSidechain":false,"type":"user","uuid":"u2","message":{"content":"three"}}
{"parentUuid":"a1","isSidechain":false,"type":"assistant","uuid":"a-side","message":{"id":"m-side","content":[{"type":"text","text":"side branch"}]}}
{"parentUuid":"u2","isSidechain":false,"type":"assistant","uuid":"a2","message":{"id":"m2","content":[{"type":"text","text":"four"}]}}
{"parentUuid":"a2","isSidechain":false,"type":"user","uuid":"u3","message":{"content":"five"}}
{"parentUuid":"u3","isSidechain":false,"type":"assistant","uuid":"a3","message":{"id":"m3","content":[{"type":"text","text":"six"}]}}"#;

        let graph = TranscriptGraph::from_jsonl(raw)?;
        let preserved = graph
            .preserved_window(&PreservedSegment {
                head_uuid: "u2".to_string(),
                anchor_uuid: "a1".to_string(),
                tail_uuid: "a3".to_string(),
            })
            .expect("preserved chain");

        assert_eq!(preserved, vec!["u2", "a2", "u3", "a3"]);
        Ok(())
    }
}
