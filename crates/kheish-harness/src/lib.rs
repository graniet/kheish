use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use kheish_codec::{digest_json_value, digest_text};
use kheish_core::{
    AgentEngine, LoopPolicy, ModelDriver, ModelRequest, ModelTurn, PermissionGate, RunOutcome,
    ToolCatalog, ToolExecutor,
};
use kheish_types::{
    CanonicalStateSnapshot, InputEnvelope, MessageRecord, ModelFinishReason, PermissionDecision,
    Role, RunSnapshot, ToolCallRecord, ToolDefinition, ToolResultRecord,
};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct FixtureLoopPolicy {
    pub max_turns: usize,
    #[serde(default)]
    pub compact_after_chars: Option<usize>,
    pub keep_last_messages: usize,
    #[serde(default)]
    pub snip_token_budget: Option<usize>,
    #[serde(default)]
    pub snip_keep_minimum_messages: Option<usize>,
    #[serde(default)]
    pub microcompact_keep_recent: Option<usize>,
    #[serde(default)]
    pub microcompact_idle_timeout_ms: Option<u64>,
    #[serde(default)]
    pub autocompact_threshold_tokens: Option<usize>,
    #[serde(default)]
    pub autocompact_buffer_tokens: Option<usize>,
    #[serde(default)]
    pub session_memory_min_tokens: Option<usize>,
    #[serde(default)]
    pub session_memory_max_tokens: Option<usize>,
}

impl From<FixtureLoopPolicy> for LoopPolicy {
    fn from(value: FixtureLoopPolicy) -> Self {
        let mut policy = LoopPolicy {
            max_turns: value.max_turns,
            keep_last_messages: value.keep_last_messages,
            ..LoopPolicy::default()
        };
        if let Some(value) = value.snip_token_budget {
            policy.snip_token_budget = value;
        }
        if let Some(value) = value.snip_keep_minimum_messages {
            policy.snip_keep_minimum = value;
        }
        if let Some(value) = value.microcompact_keep_recent {
            policy.microcompact_keep_recent = value;
        }
        if let Some(value) = value.microcompact_idle_timeout_ms {
            policy.microcompact_stale_after_ms = Some(value);
        }
        if let Some(value) = value.autocompact_threshold_tokens {
            policy.autocompact_threshold_tokens = value;
        } else if let Some(value) = value.compact_after_chars {
            policy.autocompact_threshold_tokens = (value / 4).max(1);
        }
        if let Some(value) = value.autocompact_buffer_tokens {
            policy.autocompact_buffer_tokens = value;
        }
        if let Some(value) = value.session_memory_min_tokens {
            policy.session_memory_min_tokens = value;
        }
        if let Some(value) = value.session_memory_max_tokens {
            policy.session_memory_max_tokens = value;
        }
        policy
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallFixture {
    pub id: String,
    pub name: String,
    pub input: Value,
}

impl From<ToolCallFixture> for ToolCallRecord {
    fn from(value: ToolCallFixture) -> Self {
        Self {
            id: value.id,
            name: value.name,
            input: value.input,
            assistant_message_id: None,
            assistant_provider_response_id: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScriptedTurnFixture {
    pub assistant_message_id: String,
    pub assistant_content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallFixture>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolOutputFixture {
    pub call_id: String,
    pub output: Value,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeniedToolFixture {
    pub tool_name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioExpectation {
    pub final_message_id: String,
    pub turns: usize,
    pub min_checkpoints: usize,
    #[serde(default)]
    pub summary_turns: Vec<usize>,
    pub tool_result_errors: usize,
    #[serde(default)]
    pub denied_calls: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioFixture {
    pub name: String,
    pub policy: FixtureLoopPolicy,
    pub input: InputEnvelope,
    pub scripted_turns: Vec<ScriptedTurnFixture>,
    #[serde(default)]
    pub tool_outputs: Vec<ToolOutputFixture>,
    #[serde(default)]
    pub denied_tools: Vec<DeniedToolFixture>,
    pub expect: ScenarioExpectation,
}

#[derive(Debug, Clone)]
pub struct ScenarioReport {
    pub fixture_name: String,
    pub outcome: RunOutcome,
    pub journal_state: CanonicalStateSnapshot,
    pub checkpoint_state_matches: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeComparableMessage {
    pub uuid: String,
    pub role: Role,
    pub text: String,
    pub content_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeToolCallBaseline {
    pub call_id: String,
    pub tool_name: String,
    pub input_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeToolResultBaseline {
    pub call_id: String,
    pub result_digest: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeCompactBoundaryBaseline {
    pub uuid: String,
    pub trigger: String,
    pub pre_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeToolUseSummaryBaseline {
    pub summary: String,
    pub preceding_tool_use_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeActiveChainNode {
    pub uuid: String,
    pub type_name: String,
    pub parent_uuid: Option<String>,
    pub logical_parent_uuid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeBaseline {
    pub session_id: Option<String>,
    pub timeline_messages: Vec<ClaudeCodeComparableMessage>,
    pub active_chain: Vec<ClaudeCodeActiveChainNode>,
    pub active_chain_messages: Vec<ClaudeCodeComparableMessage>,
    pub tool_calls: Vec<ClaudeCodeToolCallBaseline>,
    pub tool_results: Vec<ClaudeCodeToolResultBaseline>,
    pub compact_boundaries: Vec<ClaudeCodeCompactBoundaryBaseline>,
    pub session_state_changes: Vec<String>,
    pub tool_use_summaries: Vec<ClaudeCodeToolUseSummaryBaseline>,
    pub result_stop_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityDiff {
    pub mismatches: Vec<String>,
}

impl ParityDiff {
    pub fn is_match(&self) -> bool {
        self.mismatches.is_empty()
    }
}

#[derive(Debug, Clone)]
struct ClaudeTranscriptMessageEntry {
    uuid: String,
    type_name: String,
    parent_uuid: Option<String>,
    logical_parent_uuid: Option<String>,
    is_sidechain: bool,
    raw: Value,
}

pub fn load_fixture(path: impl AsRef<Path>) -> Result<ScenarioFixture> {
    let path = path.as_ref();
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read fixture {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse fixture {}", path.display()))
}

pub fn load_snapshot(path: impl AsRef<Path>) -> Result<RunSnapshot> {
    let path = path.as_ref();
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read snapshot {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse snapshot {}", path.display()))
}

pub fn load_claude_code_baseline(path: impl AsRef<Path>) -> Result<ClaudeCodeBaseline> {
    let path = path.as_ref();
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read transcript baseline {}", path.display()))?;
    import_claude_code_baseline(&raw)
        .with_context(|| format!("failed to import transcript baseline {}", path.display()))
}

pub fn import_claude_code_baseline(raw: &str) -> Result<ClaudeCodeBaseline> {
    let mut session_id = None;
    let mut transcript_messages = Vec::new();
    let mut timeline_messages = Vec::new();
    let mut tool_calls = Vec::new();
    let mut tool_results = Vec::new();
    let mut compact_boundaries = Vec::new();
    let mut session_state_changes = Vec::new();
    let mut tool_use_summaries = Vec::new();
    let mut result_stop_reason = None;

    for (index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let entry: Value = serde_json::from_str(line)
            .with_context(|| format!("invalid JSONL line {}", index + 1))?;
        if session_id.is_none() {
            session_id =
                string_field(&entry, "sessionId").or_else(|| string_field(&entry, "session_id"));
        }

        let type_name = string_field(&entry, "type").unwrap_or_else(|| "unknown".to_string());
        if matches!(
            type_name.as_str(),
            "user" | "assistant" | "system" | "attachment"
        ) {
            if let Some(message) = parse_transcript_message(&entry) {
                if !message.is_sidechain {
                    if let Some(comparable) = comparable_message_from_entry(&message.raw)? {
                        timeline_messages.push(comparable);
                    }
                    tool_calls.extend(extract_tool_calls(&message.raw)?);
                    tool_results.extend(extract_tool_results(&message.raw)?);
                    if let Some(boundary) = extract_compact_boundary(&message.raw)? {
                        compact_boundaries.push(boundary);
                    }
                    if let Some(state) = extract_session_state_changed(&message.raw) {
                        session_state_changes.push(state);
                    }
                }
                transcript_messages.push(message);
            }
            continue;
        }

        if type_name == "tool_use_summary" {
            tool_use_summaries.push(ClaudeCodeToolUseSummaryBaseline {
                summary: string_field(&entry, "summary").unwrap_or_default(),
                preceding_tool_use_ids: string_array_field(&entry, "preceding_tool_use_ids"),
            });
            continue;
        }

        if type_name == "result" {
            result_stop_reason = string_field(&entry, "stop_reason");
        }
    }

    let active_chain_entries = build_active_chain(&transcript_messages);
    let active_chain = active_chain_entries
        .iter()
        .map(|entry| ClaudeCodeActiveChainNode {
            uuid: entry.uuid.clone(),
            type_name: entry.type_name.clone(),
            parent_uuid: entry.parent_uuid.clone(),
            logical_parent_uuid: entry.logical_parent_uuid.clone(),
        })
        .collect::<Vec<_>>();
    let active_chain_messages = active_chain_entries
        .iter()
        .filter(|entry| !entry.is_sidechain)
        .map(|entry| comparable_message_from_entry(&entry.raw))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();

    Ok(ClaudeCodeBaseline {
        session_id,
        timeline_messages,
        active_chain,
        active_chain_messages,
        tool_calls,
        tool_results,
        compact_boundaries,
        session_state_changes,
        tool_use_summaries,
        result_stop_reason,
    })
}

pub async fn run_fixture(fixture: ScenarioFixture) -> Result<ScenarioReport> {
    let policy: LoopPolicy = fixture.policy.clone().into();
    let mut engine = AgentEngine::new(fixture.input.conversation.clone(), policy);
    let model = FixtureModel::new(
        fixture
            .scripted_turns
            .into_iter()
            .map(|turn| ModelTurn {
                finish_reason: if turn.tool_calls.is_empty() {
                    ModelFinishReason::Completed
                } else {
                    ModelFinishReason::ToolCalls
                },
                assistant_message: MessageRecord::new(
                    turn.assistant_message_id,
                    kheish_types::Role::Assistant,
                    turn.assistant_content,
                ),
                tool_calls: turn.tool_calls.into_iter().map(Into::into).collect(),
                usage: None,
            })
            .collect(),
    );
    let tools = FixtureToolExecutor::new(fixture.tool_outputs);
    let permissions = FixturePermissionGate::new(fixture.denied_tools);

    let outcome = engine
        .run_input_with_permissions(fixture.input, &model, &tools, &permissions)
        .await?;

    assert_eq!(
        outcome.final_message_id, fixture.expect.final_message_id,
        "fixture {}",
        fixture.name
    );
    assert_eq!(
        outcome.turns, fixture.expect.turns,
        "fixture {}",
        fixture.name
    );
    assert!(
        outcome.checkpoints_created >= fixture.expect.min_checkpoints,
        "fixture {} expected at least {} checkpoints, got {}",
        fixture.name,
        fixture.expect.min_checkpoints,
        outcome.checkpoints_created
    );

    for turn in &fixture.expect.summary_turns {
        let trace =
            outcome.trace.turns.get(turn - 1).ok_or_else(|| {
                anyhow!("fixture {} missing trace for turn {}", fixture.name, turn)
            })?;
        assert!(
            trace.prompt.has_summary,
            "fixture {} expected a summary on turn {}",
            fixture.name, turn
        );
    }

    let tool_result_errors = outcome
        .trace
        .turns
        .iter()
        .flat_map(|turn| turn.tool_executions.iter())
        .filter(|execution| execution.result_is_error)
        .count();
    assert_eq!(
        tool_result_errors, fixture.expect.tool_result_errors,
        "fixture {}",
        fixture.name
    );

    let denied_calls = outcome
        .trace
        .turns
        .iter()
        .flat_map(|turn| turn.tool_executions.iter())
        .filter(|execution| matches!(execution.decision, PermissionDecision::Deny { .. }))
        .map(|execution| execution.call_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        denied_calls, fixture.expect.denied_calls,
        "fixture {}",
        fixture.name
    );

    let journal_state = engine.replay_from_journal();
    let checkpoint_state_matches = engine
        .replay_from_latest_checkpoint()
        .map(|checkpoint| checkpoint == journal_state)
        .unwrap_or(true);
    assert!(
        checkpoint_state_matches,
        "fixture {} should replay identically from latest checkpoint",
        fixture.name
    );

    Ok(ScenarioReport {
        fixture_name: fixture.name,
        outcome,
        journal_state,
        checkpoint_state_matches,
    })
}

pub fn assert_snapshot_matches(report: &ScenarioReport, expected: &RunSnapshot) -> Result<()> {
    if &report.outcome.snapshot != expected {
        return Err(anyhow!(
            "snapshot mismatch for fixture {}\nactual: {}\nexpected: {}",
            report.fixture_name,
            serde_json::to_string_pretty(&report.outcome.snapshot)?,
            serde_json::to_string_pretty(expected)?,
        ));
    }
    Ok(())
}

pub fn compare_claude_code_baseline(
    baseline: &ClaudeCodeBaseline,
    snapshot: &RunSnapshot,
) -> ParityDiff {
    let mut mismatches = Vec::new();

    if let Some(expected_session_id) = &baseline.session_id {
        if expected_session_id != &snapshot.run_meta.session_id {
            mismatches.push(format!(
                "session_id mismatch: baseline={} snapshot={}",
                expected_session_id, snapshot.run_meta.session_id
            ));
        }
    }

    let snapshot_messages = snapshot
        .final_state
        .messages
        .iter()
        .filter(|message| matches!(message.role, Role::User | Role::Assistant))
        .collect::<Vec<_>>();

    if baseline.timeline_messages.len() != snapshot_messages.len() {
        mismatches.push(format!(
            "message count mismatch: baseline={} snapshot={}",
            baseline.timeline_messages.len(),
            snapshot_messages.len()
        ));
    }

    for (index, (expected, actual)) in baseline
        .timeline_messages
        .iter()
        .zip(snapshot_messages.iter())
        .enumerate()
    {
        if expected.role != actual.role {
            mismatches.push(format!(
                "message[{index}] role mismatch: baseline={:?} snapshot={:?}",
                expected.role, actual.role
            ));
        }
        if expected.content_digest != actual.content_digest {
            mismatches.push(format!(
                "message[{index}] content digest mismatch: baseline={} snapshot={}",
                expected.content_digest, actual.content_digest
            ));
        }
    }

    let snapshot_tool_calls = snapshot
        .turns
        .iter()
        .flat_map(|turn| turn.tool_calls.iter())
        .collect::<Vec<_>>();
    if baseline.tool_calls.len() != snapshot_tool_calls.len() {
        mismatches.push(format!(
            "tool call count mismatch: baseline={} snapshot={}",
            baseline.tool_calls.len(),
            snapshot_tool_calls.len()
        ));
    }

    for (index, (expected, actual)) in baseline
        .tool_calls
        .iter()
        .zip(snapshot_tool_calls.iter())
        .enumerate()
    {
        if expected.call_id != actual.call_id {
            mismatches.push(format!(
                "tool_call[{index}] call_id mismatch: baseline={} snapshot={}",
                expected.call_id, actual.call_id
            ));
        }
        if expected.tool_name != actual.tool_name {
            mismatches.push(format!(
                "tool_call[{index}] tool_name mismatch: baseline={} snapshot={}",
                expected.tool_name, actual.tool_name
            ));
        }
        if expected.input_digest != actual.input_digest {
            mismatches.push(format!(
                "tool_call[{index}] input digest mismatch: baseline={} snapshot={}",
                expected.input_digest, actual.input_digest
            ));
        }
    }

    let snapshot_tool_results = snapshot
        .turns
        .iter()
        .flat_map(|turn| turn.tool_results.iter())
        .collect::<Vec<_>>();
    if baseline.tool_results.len() != snapshot_tool_results.len() {
        mismatches.push(format!(
            "tool result count mismatch: baseline={} snapshot={}",
            baseline.tool_results.len(),
            snapshot_tool_results.len()
        ));
    }

    for (index, (expected, actual)) in baseline
        .tool_results
        .iter()
        .zip(snapshot_tool_results.iter())
        .enumerate()
    {
        if expected.call_id != actual.call_id {
            mismatches.push(format!(
                "tool_result[{index}] call_id mismatch: baseline={} snapshot={}",
                expected.call_id, actual.call_id
            ));
        }
        if expected.is_error != actual.result_is_error {
            mismatches.push(format!(
                "tool_result[{index}] is_error mismatch: baseline={} snapshot={}",
                expected.is_error, actual.result_is_error
            ));
        }
        if expected.result_digest != actual.result_digest {
            mismatches.push(format!(
                "tool_result[{index}] result digest mismatch: baseline={} snapshot={}",
                expected.result_digest, actual.result_digest
            ));
        }
    }

    if baseline.compact_boundaries.len() != snapshot.checkpoints.len() {
        mismatches.push(format!(
            "checkpoint count mismatch: baseline={} snapshot={}",
            baseline.compact_boundaries.len(),
            snapshot.checkpoints.len()
        ));
    }

    let snapshot_tool_batches = snapshot
        .turns
        .iter()
        .filter(|turn| !turn.tool_calls.is_empty())
        .count();
    if baseline.tool_use_summaries.len() != snapshot_tool_batches {
        mismatches.push(format!(
            "tool batch count mismatch: baseline={} snapshot={}",
            baseline.tool_use_summaries.len(),
            snapshot_tool_batches
        ));
    }

    if let Some(last_state) = baseline.session_state_changes.last() {
        if last_state != "idle" {
            mismatches.push(format!(
                "final session state mismatch: baseline={} snapshot=completed",
                last_state
            ));
        }
    }

    if let Some(expected_stop_reason) = baseline
        .result_stop_reason
        .as_deref()
        .and_then(map_claude_code_stop_reason)
    {
        let actual_stop_reason = snapshot
            .turns
            .last()
            .map(|turn| turn.stop_reason.as_str())
            .unwrap_or("unknown");
        if expected_stop_reason != actual_stop_reason {
            mismatches.push(format!(
                "stop_reason mismatch: baseline={} snapshot={}",
                expected_stop_reason, actual_stop_reason
            ));
        }
    }

    ParityDiff { mismatches }
}

pub fn assert_parity_matches(baseline: &ClaudeCodeBaseline, snapshot: &RunSnapshot) -> Result<()> {
    let diff = compare_claude_code_baseline(baseline, snapshot);
    if diff.is_match() {
        return Ok(());
    }

    Err(anyhow!(
        "parity mismatch\nbaseline: {}\nsnapshot: {}\nmismatches:\n- {}",
        format_claude_code_baseline(baseline)?,
        serde_json::to_string_pretty(snapshot)?,
        diff.mismatches.join("\n- "),
    ))
}

fn format_claude_code_baseline(baseline: &ClaudeCodeBaseline) -> Result<String> {
    let value = serde_json::json!({
        "session_id": baseline.session_id,
        "timeline_messages": baseline.timeline_messages.iter().map(|message| serde_json::json!({
            "uuid": message.uuid,
            "role": message.role,
            "content_digest": message.content_digest,
            "text": message.text,
        })).collect::<Vec<_>>(),
        "active_chain": baseline.active_chain.iter().map(|entry| serde_json::json!({
            "uuid": entry.uuid,
            "type_name": entry.type_name,
            "parent_uuid": entry.parent_uuid,
            "logical_parent_uuid": entry.logical_parent_uuid,
        })).collect::<Vec<_>>(),
        "tool_calls": baseline.tool_calls.iter().map(|call| serde_json::json!({
            "call_id": call.call_id,
            "tool_name": call.tool_name,
            "input_digest": call.input_digest,
        })).collect::<Vec<_>>(),
        "tool_results": baseline.tool_results.iter().map(|result| serde_json::json!({
            "call_id": result.call_id,
            "result_digest": result.result_digest,
            "is_error": result.is_error,
        })).collect::<Vec<_>>(),
        "compact_boundaries": baseline.compact_boundaries.iter().map(|boundary| serde_json::json!({
            "uuid": boundary.uuid,
            "trigger": boundary.trigger,
            "pre_tokens": boundary.pre_tokens,
        })).collect::<Vec<_>>(),
        "session_state_changes": baseline.session_state_changes,
        "tool_use_summaries": baseline.tool_use_summaries.iter().map(|summary| serde_json::json!({
            "summary": summary.summary,
            "preceding_tool_use_ids": summary.preceding_tool_use_ids,
        })).collect::<Vec<_>>(),
        "result_stop_reason": baseline.result_stop_reason,
    });
    serde_json::to_string_pretty(&value).map_err(Into::into)
}

fn parse_transcript_message(entry: &Value) -> Option<ClaudeTranscriptMessageEntry> {
    let uuid = string_field(entry, "uuid")?;
    let type_name = string_field(entry, "type").unwrap_or_else(|| "unknown".to_string());
    let parent_uuid = optional_string_field(entry, "parentUuid");
    let logical_parent_uuid = optional_string_field(entry, "logicalParentUuid");
    let is_sidechain = entry
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(ClaudeTranscriptMessageEntry {
        uuid,
        type_name,
        parent_uuid,
        logical_parent_uuid,
        is_sidechain,
        raw: entry.clone(),
    })
}

fn comparable_message_from_entry(entry: &Value) -> Result<Option<ClaudeCodeComparableMessage>> {
    let role = match string_field(entry, "type").as_deref() {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return Ok(None),
    };

    let text = extract_visible_message_text(entry)?;
    let Some(text) = text else {
        return Ok(None);
    };

    Ok(Some(ClaudeCodeComparableMessage {
        uuid: string_field(entry, "uuid").unwrap_or_default(),
        role,
        content_digest: digest_text(&text),
        text,
    }))
}

fn extract_visible_message_text(entry: &Value) -> Result<Option<String>> {
    let Some(content) = entry
        .get("message")
        .and_then(|message| message.get("content"))
    else {
        return Ok(None);
    };

    match content {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Value::Array(blocks) => {
            let text = blocks
                .iter()
                .filter_map(|block| match string_field(block, "type").as_deref() {
                    Some("text") => string_field(block, "text"),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Value::Null => Ok(None),
        other => Ok(Some(serde_json::to_string(other)?)),
    }
}

fn extract_tool_calls(entry: &Value) -> Result<Vec<ClaudeCodeToolCallBaseline>> {
    if string_field(entry, "type").as_deref() != Some("assistant") {
        return Ok(Vec::new());
    }

    let Some(blocks) = entry
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };

    blocks
        .iter()
        .filter(|block| string_field(block, "type").as_deref() == Some("tool_use"))
        .map(|block| {
            Ok(ClaudeCodeToolCallBaseline {
                call_id: string_field(block, "id").unwrap_or_default(),
                tool_name: string_field(block, "name").unwrap_or_default(),
                input_digest: digest_json_value(block.get("input").unwrap_or(&Value::Null))?,
            })
        })
        .collect()
}

fn extract_tool_results(entry: &Value) -> Result<Vec<ClaudeCodeToolResultBaseline>> {
    if string_field(entry, "type").as_deref() != Some("user") {
        return Ok(Vec::new());
    }

    let Some(blocks) = entry
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };

    let tool_result_blocks = blocks
        .iter()
        .filter(|block| string_field(block, "type").as_deref() == Some("tool_result"))
        .collect::<Vec<_>>();

    tool_result_blocks
        .iter()
        .map(|block| {
            let result_value = if tool_result_blocks.len() == 1 {
                entry
                    .get("tool_use_result")
                    .unwrap_or_else(|| block.get("content").unwrap_or(&Value::Null))
            } else {
                block.get("content").unwrap_or(&Value::Null)
            };

            Ok(ClaudeCodeToolResultBaseline {
                call_id: string_field(block, "tool_use_id").unwrap_or_default(),
                result_digest: digest_json_value(result_value)?,
                is_error: block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

fn extract_compact_boundary(entry: &Value) -> Result<Option<ClaudeCodeCompactBoundaryBaseline>> {
    if string_field(entry, "type").as_deref() != Some("system")
        || string_field(entry, "subtype").as_deref() != Some("compact_boundary")
    {
        return Ok(None);
    }

    let compact_metadata = entry
        .get("compact_metadata")
        .ok_or_else(|| anyhow!("compact_boundary missing compact_metadata"))?;
    Ok(Some(ClaudeCodeCompactBoundaryBaseline {
        uuid: string_field(entry, "uuid").unwrap_or_default(),
        trigger: string_field(compact_metadata, "trigger").unwrap_or_else(|| "unknown".to_string()),
        pre_tokens: compact_metadata
            .get("pre_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
    }))
}

fn extract_session_state_changed(entry: &Value) -> Option<String> {
    if string_field(entry, "type").as_deref() != Some("system")
        || string_field(entry, "subtype").as_deref() != Some("session_state_changed")
    {
        return None;
    }
    string_field(entry, "state")
}

fn build_active_chain(
    transcript_messages: &[ClaudeTranscriptMessageEntry],
) -> Vec<ClaudeTranscriptMessageEntry> {
    let parent_uuids = transcript_messages
        .iter()
        .filter_map(effective_parent_uuid_owned)
        .collect::<std::collections::BTreeSet<_>>();
    let entries_by_uuid = transcript_messages
        .iter()
        .map(|entry| (entry.uuid.clone(), entry.clone()))
        .collect::<BTreeMap<_, _>>();

    let leaf = transcript_messages
        .iter()
        .filter(|entry| !parent_uuids.contains(&entry.uuid))
        .rev()
        .find_map(|terminal| nearest_user_or_assistant(terminal, &entries_by_uuid));

    let Some(leaf) = leaf else {
        return Vec::new();
    };

    let mut seen = std::collections::BTreeSet::new();
    let mut chain = Vec::new();
    let mut current = Some(leaf);
    while let Some(entry) = current {
        if !seen.insert(entry.uuid.clone()) {
            break;
        }
        chain.push(entry.clone());
        current = entry
            .parent_uuid
            .as_ref()
            .or(entry.logical_parent_uuid.as_ref())
            .and_then(|parent_uuid| entries_by_uuid.get(parent_uuid))
            .cloned();
    }
    chain.reverse();
    chain
}

fn nearest_user_or_assistant(
    terminal: &ClaudeTranscriptMessageEntry,
    entries_by_uuid: &BTreeMap<String, ClaudeTranscriptMessageEntry>,
) -> Option<ClaudeTranscriptMessageEntry> {
    let mut current = Some(terminal.clone());
    let mut seen = std::collections::BTreeSet::new();
    while let Some(entry) = current {
        if !seen.insert(entry.uuid.clone()) {
            return None;
        }
        if matches!(entry.type_name.as_str(), "user" | "assistant") && !entry.is_sidechain {
            return Some(entry);
        }
        current = entry
            .parent_uuid
            .as_ref()
            .or(entry.logical_parent_uuid.as_ref())
            .and_then(|parent_uuid| entries_by_uuid.get(parent_uuid))
            .cloned();
    }
    None
}

fn effective_parent_uuid_owned(entry: &ClaudeTranscriptMessageEntry) -> Option<String> {
    entry
        .parent_uuid
        .clone()
        .or_else(|| entry.logical_parent_uuid.clone())
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn optional_string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(|field| match field {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        _ => None,
    })
}

fn string_array_field(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect()
}

fn map_claude_code_stop_reason(stop_reason: &str) -> Option<&'static str> {
    match stop_reason {
        "end_turn" => Some("completed"),
        "tool_use" => Some("tool_calls"),
        _ => None,
    }
}

struct FixtureModel {
    turns: Mutex<VecDeque<ModelTurn>>,
}

impl FixtureModel {
    fn new(turns: Vec<ModelTurn>) -> Self {
        Self {
            turns: Mutex::new(VecDeque::from(turns)),
        }
    }
}

#[async_trait]
impl ModelDriver for FixtureModel {
    async fn next_turn(&self, _request: ModelRequest) -> Result<ModelTurn> {
        self.turns
            .lock()
            .pop_front()
            .ok_or_else(|| anyhow!("no scripted model turn remaining"))
    }
}

struct FixtureToolExecutor {
    outputs: BTreeMap<String, ToolOutputFixture>,
}

impl FixtureToolExecutor {
    fn new(outputs: Vec<ToolOutputFixture>) -> Self {
        Self {
            outputs: outputs
                .into_iter()
                .map(|fixture| (fixture.call_id.clone(), fixture))
                .collect(),
        }
    }
}

impl ToolCatalog for FixtureToolExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }
}

#[async_trait]
impl ToolExecutor for FixtureToolExecutor {
    async fn execute(&self, call: &ToolCallRecord) -> Result<ToolResultRecord> {
        let fixture = self
            .outputs
            .get(&call.id)
            .ok_or_else(|| anyhow!("missing tool output for call {}", call.id))?;
        Ok(ToolResultRecord {
            call_id: fixture.call_id.clone(),
            output: fixture.output.clone(),
            is_error: fixture.is_error,
            tool_name: Some(call.name.clone()),
            offset: None,
            timestamp_ms: None,
            context_updates: Vec::new(),
            hook_contexts: Vec::new(),
        })
    }
}

struct FixturePermissionGate {
    rules: BTreeMap<String, String>,
}

impl FixturePermissionGate {
    fn new(denials: Vec<DeniedToolFixture>) -> Self {
        Self {
            rules: denials
                .into_iter()
                .map(|denial| (denial.tool_name, denial.reason))
                .collect(),
        }
    }
}

#[async_trait]
impl PermissionGate for FixturePermissionGate {
    async fn check(&self, call: &ToolCallRecord) -> Result<PermissionDecision> {
        match self.rules.get(&call.name) {
            Some(reason) => Ok(PermissionDecision::Deny {
                reason: reason.clone(),
            }),
            None => Ok(PermissionDecision::Allow),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        assert_parity_matches, assert_snapshot_matches, load_claude_code_baseline, load_fixture,
        load_snapshot, run_fixture,
    };
    use anyhow::Result;
    use std::path::PathBuf;

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(name)
    }

    fn golden_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("goldens")
            .join(name)
    }

    #[tokio::test]
    async fn compaction_tool_loop_fixture_matches_expectations() -> Result<()> {
        let fixture = load_fixture(fixture_path("compaction_tool_loop.json"))?;
        let report = run_fixture(fixture).await?;
        let expected = load_snapshot(golden_path("compaction_tool_loop.snapshot.json"))?;
        assert!(report.checkpoint_state_matches);
        assert!(report.outcome.trace.turns.len() >= 2);
        assert_snapshot_matches(&report, &expected)?;
        Ok(())
    }

    #[tokio::test]
    async fn permission_denial_fixture_matches_expectations() -> Result<()> {
        let fixture = load_fixture(fixture_path("permission_denied.json"))?;
        let report = run_fixture(fixture).await?;
        let expected = load_snapshot(golden_path("permission_denied.snapshot.json"))?;
        assert!(report.checkpoint_state_matches);
        assert_eq!(report.journal_state.completed_tool_results.len(), 1);
        assert_snapshot_matches(&report, &expected)?;
        Ok(())
    }

    #[test]
    fn claude_code_transcript_import_preserves_markers_and_roles() -> Result<()> {
        let baseline =
            load_claude_code_baseline(fixture_path("compaction_tool_loop.claude.jsonl"))?;

        assert_eq!(baseline.session_id.as_deref(), Some("fixture-session-1"));
        assert_eq!(baseline.timeline_messages.len(), 3);
        assert_eq!(baseline.timeline_messages[0].role, kheish_types::Role::User);
        assert_eq!(
            baseline.timeline_messages[1].role,
            kheish_types::Role::Assistant
        );
        assert_eq!(
            baseline.timeline_messages[2].role,
            kheish_types::Role::Assistant
        );
        assert_eq!(baseline.tool_calls.len(), 1);
        assert_eq!(baseline.tool_results.len(), 1);
        assert_eq!(
            baseline.active_chain.last().map(|node| node.uuid.as_str()),
            Some("20000000-0000-4000-8000-000000000007")
        );
        assert_eq!(
            baseline.session_state_changes,
            vec!["running".to_string(), "idle".to_string()]
        );
        assert_eq!(baseline.compact_boundaries.len(), 1);
        assert_eq!(baseline.tool_use_summaries.len(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn compaction_fixture_matches_claude_code_baseline() -> Result<()> {
        let fixture = load_fixture(fixture_path("compaction_tool_loop.json"))?;
        let report = run_fixture(fixture).await?;
        let baseline =
            load_claude_code_baseline(fixture_path("compaction_tool_loop.claude.jsonl"))?;
        assert_parity_matches(&baseline, &report.outcome.snapshot)?;
        Ok(())
    }

    #[tokio::test]
    async fn permission_fixture_matches_claude_code_baseline() -> Result<()> {
        let fixture = load_fixture(fixture_path("permission_denied.json"))?;
        let report = run_fixture(fixture).await?;
        let baseline = load_claude_code_baseline(fixture_path("permission_denied.claude.jsonl"))?;
        assert_eq!(baseline.timeline_messages.len(), 3);
        assert_eq!(baseline.tool_calls.len(), 1);
        assert_eq!(baseline.tool_results.len(), 1);
        assert_eq!(baseline.compact_boundaries.len(), 0);
        assert_eq!(
            baseline.active_chain.last().map(|node| node.uuid.as_str()),
            Some("10000000-0000-4000-8000-000000000006")
        );
        assert_eq!(
            baseline.session_state_changes,
            vec!["running".to_string(), "idle".to_string()]
        );
        assert_eq!(baseline.tool_use_summaries.len(), 1);
        assert_parity_matches(&baseline, &report.outcome.snapshot)?;
        Ok(())
    }
}
