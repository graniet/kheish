use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use kheish_agent::{ChildRetentionPolicy, ManagedAgentSnapshot};
use kheish_runtime::{
    PermissionMode, PromptMergeMode, ToolContext, ToolInputKind, ToolSchemaField,
    bounded_workspace_root, normalize_tool_input_numbers,
};
use kheish_types::{
    DYNAMIC_MCP_TOOL_ALLOWLIST_SENTINEL, ModelGenerationConfig, StructuredFieldSchema,
    StructuredValueKind, ToolSurfaceFilter, UserQuestion, UserQuestionOption, UserQuestionRequest,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::time::sleep;

use crate::{PostMailboxRequest, SidechainSubtaskRequest, SpawnSidechainRequest};

use super::*;

pub(super) fn summarize_managed_command(
    command: &str,
    workdir: &Path,
    run_in_background: bool,
) -> String {
    let summary = command.trim().replace('\n', " ");
    let summary = if summary.chars().count() > 72 {
        let mut truncated = summary.chars().take(69).collect::<String>();
        truncated.push_str("...");
        truncated
    } else {
        summary
    };
    format!(
        "{} bash in {}: {}",
        if run_in_background {
            "Background"
        } else {
            "Managed"
        },
        workdir.display(),
        summary
    )
}

pub(super) fn execution_session_id(ctx: &ToolContext) -> Result<&str> {
    ctx.metadata
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|session_id| !session_id.is_empty())
        .ok_or_else(|| anyhow!("tool execution missing session_id"))
}

pub(super) fn execution_agent_id(ctx: &ToolContext) -> Result<&str> {
    ctx.metadata
        .get("agent_id")
        .and_then(Value::as_str)
        .filter(|agent_id| !agent_id.is_empty())
        .ok_or_else(|| anyhow!("tool execution missing agent_id"))
}

pub(super) fn execution_run_id(ctx: &ToolContext) -> Option<&str> {
    ctx.metadata
        .get("run_id")
        .and_then(Value::as_str)
        .filter(|run_id| !run_id.is_empty())
}

pub(super) async fn populate_spawn_request_from_context(
    control: &dyn DaemonToolControl,
    ctx: &ToolContext,
    request: &mut SpawnAgentToolRequest,
) -> Result<()> {
    if let Some(cwd) = request.cwd.clone() {
        let Some(base) = ctx
            .metadata
            .get("workspace_root")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            anyhow::bail!("spawn_agent requires workspace_root metadata when cwd is set");
        };
        request.cwd = Some(
            bounded_workspace_root(std::path::Path::new(base), std::path::Path::new(&cwd))?
                .display()
                .to_string(),
        );
    }

    let session_id = execution_session_id(ctx)?;
    if let Some(assistant_message_id) = ctx
        .metadata
        .get("assistant_message_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        request.parent_assistant_message = control
            .load_assistant_message(session_id, assistant_message_id)
            .await?;
    }
    if let Some(tool_call_id) = ctx
        .metadata
        .get("tool_call_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        request.inherited_tool_call_ids = vec![tool_call_id.to_string()];
    }
    request.spawned_by_run_id = ctx
        .metadata
        .get("run_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let spawned_by_run_id = request.spawned_by_run_id.clone();
    request.spawn_request_id = ctx
        .metadata
        .get("tool_call_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|tool_call_id| {
            spawned_by_run_id
                .as_deref()
                .map(|run_id| format!("{run_id}:{tool_call_id}"))
                .unwrap_or_else(|| tool_call_id.to_string())
        });
    Ok(())
}

/// Deserialize one tool request after normalizing integer-like JSON floats.
///
/// Some providers emit tool-call arguments such as `1.0` for fields that are
/// logically integers. Kheish keeps tool schemas strongly typed, so we coerce
/// only lossless integer-like floats before deserialization.
pub(super) fn deserialize_tool_request<T>(input: Value) -> Result<T>
where
    T: DeserializeOwned,
{
    Ok(serde_json::from_value(normalize_tool_input_numbers(input))?)
}

/// Read one optional unsigned integer field while accepting lossless floats.
pub(super) fn optional_u64_field(input: &Value, field: &str) -> Option<u64> {
    input.get(field).and_then(value_as_lossless_u64)
}

/// Read one optional usize field while accepting lossless floats.
pub(super) fn optional_usize_field(input: &Value, field: &str) -> Option<usize> {
    optional_u64_field(input, field).and_then(|value| usize::try_from(value).ok())
}

fn value_as_lossless_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            number.as_f64().and_then(|raw| {
                if raw.is_finite() && raw >= 0.0 && raw.fract() == 0.0 && raw <= u64::MAX as f64 {
                    Some(raw as u64)
                } else {
                    None
                }
            })
        }),
        _ => None,
    }
}

pub(super) fn build_user_question_request(
    ctx: &ToolContext,
    input: &Value,
) -> Result<UserQuestionRequest> {
    let values = if let Some(questions_value) = input.get("questions") {
        let questions = questions_value
            .as_array()
            .cloned()
            .ok_or_else(|| anyhow!("questions must be an array"))?;
        if input.get("question").is_some() || input.get("text").is_some() {
            bail!("questions cannot be combined with single-question shorthand fields");
        }
        questions
    } else {
        input
            .get("options")
            .and_then(Value::as_array)
            .and_then(|options| {
                if options.is_empty() {
                    None
                } else if input.get("question").is_some() || input.get("text").is_some() {
                    Some(vec![input.clone()])
                } else {
                    None
                }
            })
            .ok_or_else(|| anyhow!("questions must be an array"))?
    };
    if !(1..=4).contains(&values.len()) {
        bail!("ask_user_question requires between 1 and 4 questions");
    }

    let mut questions = Vec::with_capacity(values.len());
    let mut seen_question_ids = std::collections::BTreeSet::new();
    let mut seen_question_texts = std::collections::BTreeSet::new();
    for (question_index, value) in values.iter().enumerate() {
        let header = value
            .optional_trimmed_string("header")?
            .unwrap_or_else(|| format!("Question {}", question_index + 1));
        let question = value
            .get("question")
            .or_else(|| value.get("text"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| anyhow!("question text is required"))?;
        if !seen_question_texts.insert(question.to_ascii_lowercase()) {
            bail!("ask_user_question requires unique question text");
        }
        let options_value = value
            .get("options")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("question options must be an array"))?;
        if !(2..=4).contains(&options_value.len()) {
            bail!("each ask_user_question entry requires between 2 and 4 options");
        }
        let mut options = Vec::with_capacity(options_value.len());
        let mut seen_option_ids = std::collections::BTreeSet::new();
        let mut seen_option_labels = std::collections::BTreeSet::new();
        for (option_index, option) in options_value.iter().enumerate() {
            let label = option
                .get("label")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| anyhow!("question option label is required"))?;
            let option_id = option
                .optional_trimmed_string("id")?
                .unwrap_or_else(|| format!("option-{}", option_index + 1));
            if !seen_option_ids.insert(option_id.clone()) {
                bail!("question {} requires unique option ids", question_index + 1);
            }
            if !seen_option_labels.insert(label.to_ascii_lowercase()) {
                bail!(
                    "question {} requires unique option labels",
                    question_index + 1
                );
            }
            options.push(UserQuestionOption {
                id: option_id,
                label: label.to_string(),
                description: option.optional_trimmed_string("description")?,
                preview: option.optional_trimmed_string("preview")?,
            });
        }
        let question_id = value
            .optional_trimmed_string("id")?
            .unwrap_or_else(|| format!("question-{}", question_index + 1));
        if !seen_question_ids.insert(question_id.clone()) {
            bail!("ask_user_question requires unique question ids");
        }
        questions.push(UserQuestion {
            id: question_id,
            header,
            question: question.to_string(),
            options,
            multi_select: value.optional_bool("multi_select")?.unwrap_or(false),
        });
    }

    let now_ms = crate::now_ms();
    let expires_at_ms = parse_user_question_expiration(input, now_ms)?;

    Ok(UserQuestionRequest {
        id: format!("question-request-{}", ctx.call_id),
        tool_call_id: ctx.call_id.clone(),
        questions,
        created_at_ms: now_ms,
        expires_at_ms,
    })
}

fn parse_user_question_expiration(input: &Value, now_ms: u64) -> Result<Option<u64>> {
    let expires_at_ms = optional_strict_u64_field(input, "expires_at_ms")?;
    let expires_after_ms = optional_strict_u64_field(input, "expires_after_ms")?;
    if expires_at_ms.is_some() && expires_after_ms.is_some() {
        bail!("ask_user_question cannot combine expires_at_ms and expires_after_ms");
    }
    if let Some(delta) = expires_after_ms {
        if delta == 0 {
            bail!("expires_after_ms must be greater than zero");
        }
        return now_ms
            .checked_add(delta)
            .map(Some)
            .ok_or_else(|| anyhow!("expires_after_ms overflows timestamp range"));
    }
    Ok(expires_at_ms)
}

fn optional_strict_u64_field(input: &Value, field: &str) -> Result<Option<u64>> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| anyhow!("{field} must be a non-negative integer that fits u64")),
        Some(_) => bail!("{field} must be a non-negative integer"),
    }
}

trait StrictToolValueExt {
    fn optional_trimmed_string(&self, field: &str) -> Result<Option<String>>;
    fn optional_bool(&self, field: &str) -> Result<Option<bool>>;
}

impl StrictToolValueExt for Value {
    fn optional_trimmed_string(&self, field: &str) -> Result<Option<String>> {
        match self.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => {
                let trimmed = value.trim();
                Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
            }
            Some(_) => bail!("{field} must be a string"),
        }
    }

    fn optional_bool(&self, field: &str) -> Result<Option<bool>> {
        match self.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(_) => bail!("{field} must be a boolean"),
        }
    }
}

fn validate_text_or_input_items_with_asset_ids(
    field_name: &str,
    text: &str,
    input_items: &[crate::SubmitInputItemRequest],
    asset_ids: &[String],
) -> Result<()> {
    crate::api::validate_submit_input_items(input_items)?;
    if !input_items.is_empty() && !text.trim().is_empty() {
        bail!("{field_name} cannot be combined with input_items");
    }
    if !input_items.is_empty() && !asset_ids.is_empty() {
        bail!("asset_ids cannot be combined with input_items");
    }
    if input_items.is_empty() && text.trim().is_empty() && asset_ids.is_empty() {
        bail!("{field_name} or asset_ids or input_items is required");
    }
    for asset_id in asset_ids {
        if asset_id.trim().is_empty() {
            bail!("asset_ids entries must not be empty");
        }
    }
    Ok(())
}

fn input_items_from_text_and_asset_ids(
    text: &str,
    asset_ids: &[String],
) -> Vec<crate::SubmitInputItemRequest> {
    let mut items = Vec::with_capacity(asset_ids.len().saturating_add(1));
    if !text.trim().is_empty() {
        items.push(crate::SubmitInputItemRequest::Text {
            text: text.to_string(),
        });
    }
    items.extend(
        asset_ids
            .iter()
            .cloned()
            .map(|asset_id| crate::SubmitInputItemRequest::AssetReference { asset_id }),
    );
    items
}

pub(super) fn build_string_field(name: &str, description: &str, required: bool) -> ToolSchemaField {
    ToolSchemaField {
        name: name.to_string(),
        kind: ToolInputKind::String,
        item_kind: None,
        structured_schema: None,
        required,
        description: Some(description.to_string()),
    }
}

pub(super) fn build_array_field(
    name: &str,
    description: &str,
    required: bool,
    item_kind: ToolInputKind,
) -> ToolSchemaField {
    ToolSchemaField {
        name: name.to_string(),
        kind: ToolInputKind::Array,
        item_kind: Some(item_kind),
        structured_schema: None,
        required,
        description: Some(description.to_string()),
    }
}

fn string_array_schema() -> StructuredFieldSchema {
    let mut schema = StructuredFieldSchema::new(StructuredValueKind::Array);
    schema.items = Some(Box::new(StructuredFieldSchema::new(
        StructuredValueKind::String,
    )));
    schema
}

fn optional_string_array_object_schema(fields: &[&str]) -> StructuredFieldSchema {
    StructuredFieldSchema {
        kind: StructuredValueKind::Object,
        fields: BTreeMap::new(),
        optional_fields: fields
            .iter()
            .map(|field| ((*field).to_string(), string_array_schema()))
            .collect(),
        items: None,
    }
}

pub(super) fn build_capability_scope_field() -> ToolSchemaField {
    ToolSchemaField {
        name: "capability_scope".to_string(),
        kind: ToolInputKind::Object,
        item_kind: None,
        structured_schema: Some(optional_string_array_object_schema(&[
            "skill_allow",
            "skill_deny",
            "mcp_server_allow",
            "mcp_server_deny",
            "mcp_tool_allow",
            "mcp_tool_deny",
        ])),
        required: false,
        description: Some(
            "Optional child capability scope restriction applied on top of the parent session scope."
                .to_string(),
        ),
    }
}

pub(super) fn build_credential_scope_field() -> ToolSchemaField {
    ToolSchemaField {
        name: "credential_scope".to_string(),
        kind: ToolInputKind::Object,
        item_kind: None,
        structured_schema: Some(optional_string_array_object_schema(&[
            "route_allow",
            "route_deny",
            "connector_allow",
            "connector_deny",
            "connector_credential_allow",
            "connector_credential_deny",
            "mcp_server_allow",
            "mcp_server_deny",
        ])),
        required: false,
        description: Some(
            "Optional child credential scope restriction applied on top of the parent session scope."
                .to_string(),
        ),
    }
}

pub(super) const USER_QUESTION_INPUT_EXAMPLE: &str = r#"{"questions":[{"id":"focus","header":"Focus","question":"Which focus should I use?","options":[{"id":"memory","label":"memory"},{"id":"kernel","label":"kernel"}],"multi_select":false}]}"#;

const USER_QUESTION_FIELD_EXAMPLE: &str = r#"[{"id":"focus","header":"Focus","question":"Which focus should I use?","options":[{"id":"memory","label":"memory"},{"id":"kernel","label":"kernel"}],"multi_select":false}]"#;

pub(super) fn build_user_questions_field() -> ToolSchemaField {
    ToolSchemaField {
        name: "questions".to_string(),
        kind: ToolInputKind::Array,
        item_kind: Some(ToolInputKind::Object),
        structured_schema: Some(user_questions_schema()),
        required: true,
        description: Some(format!(
            "Array of 1-4 structured questions. Each question object supports {{id?, header, question, options, multi_select?}}. Each option object supports {{id?, label, description?, preview?}}. Example: {USER_QUESTION_FIELD_EXAMPLE}."
        )),
    }
}

pub(super) fn build_user_question_expiration_fields() -> Vec<ToolSchemaField> {
    vec![
        build_number_field(
            "expires_at_ms",
            "Optional absolute Unix timestamp in milliseconds after which the question request expires.",
            false,
        ),
        build_number_field(
            "expires_after_ms",
            "Optional relative timeout in milliseconds after creation. Do not combine with expires_at_ms.",
            false,
        ),
    ]
}

pub(super) fn build_boolean_field(
    name: &str,
    description: &str,
    required: bool,
) -> ToolSchemaField {
    ToolSchemaField {
        name: name.to_string(),
        kind: ToolInputKind::Boolean,
        item_kind: None,
        structured_schema: None,
        required,
        description: Some(description.to_string()),
    }
}

pub(super) fn build_number_field(name: &str, description: &str, required: bool) -> ToolSchemaField {
    ToolSchemaField {
        name: name.to_string(),
        kind: ToolInputKind::Number,
        item_kind: None,
        structured_schema: None,
        required,
        description: Some(description.to_string()),
    }
}

pub(super) fn string_field(input: &Value, name: &str) -> Result<String> {
    input
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{name} is required"))
}

pub(super) fn optional_string_field(input: &Value, name: &str) -> Option<String> {
    input.get(name).and_then(Value::as_str).map(str::to_string)
}

pub(super) fn build_object_field(name: &str, description: &str, required: bool) -> ToolSchemaField {
    ToolSchemaField {
        name: name.to_string(),
        kind: ToolInputKind::Object,
        item_kind: None,
        structured_schema: None,
        required,
        description: Some(description.to_string()),
    }
}

fn user_questions_schema() -> StructuredFieldSchema {
    let mut option_fields = BTreeMap::new();
    option_fields.insert(
        "label".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    let mut option_optional_fields = BTreeMap::new();
    for name in ["id", "description", "preview"] {
        option_optional_fields.insert(
            name.to_string(),
            StructuredFieldSchema::new(StructuredValueKind::String),
        );
    }
    let option_schema = StructuredFieldSchema {
        kind: StructuredValueKind::Object,
        fields: option_fields,
        optional_fields: option_optional_fields,
        items: None,
    };

    let mut options_schema = StructuredFieldSchema::new(StructuredValueKind::Array);
    options_schema.items = Some(Box::new(option_schema));

    let mut question_fields = BTreeMap::new();
    question_fields.insert(
        "question".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    question_fields.insert("options".to_string(), options_schema);
    let mut question_optional_fields = BTreeMap::new();
    for name in ["id", "header"] {
        question_optional_fields.insert(
            name.to_string(),
            StructuredFieldSchema::new(StructuredValueKind::String),
        );
    }
    question_optional_fields.insert(
        "multi_select".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::Boolean),
    );
    let question_schema = StructuredFieldSchema {
        kind: StructuredValueKind::Object,
        fields: question_fields,
        optional_fields: question_optional_fields,
        items: None,
    };

    let mut questions_schema = StructuredFieldSchema::new(StructuredValueKind::Array);
    questions_schema.items = Some(Box::new(question_schema));
    questions_schema
}

fn normalize_tool_names(values: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() || normalized.iter().any(|entry| entry == trimmed) {
            continue;
        }
        normalized.push(trimmed.to_string());
    }
    normalized
}

fn base_agent_disallowed_tools() -> Vec<String> {
    vec![
        "spawn_agent".to_string(),
        "list_agents".to_string(),
        "list_agent_summaries".to_string(),
        "get_agent".to_string(),
        "message_agent".to_string(),
        "wait_agent".to_string(),
        "enter_plan_mode".to_string(),
        "exit_plan_mode".to_string(),
        "ask_user_question".to_string(),
    ]
}

fn default_async_agent_tools() -> Vec<String> {
    vec![
        DYNAMIC_MCP_TOOL_ALLOWLIST_SENTINEL.to_string(),
        "list_skills".to_string(),
        "use_skill".to_string(),
        "read_file".to_string(),
        "write_file".to_string(),
        "edit_file".to_string(),
        "apply_patch".to_string(),
        "list_files".to_string(),
        "glob_search".to_string(),
        "grep_search".to_string(),
        "bash".to_string(),
        "web_search".to_string(),
        "web_fetch".to_string(),
        "emit_output".to_string(),
        "read_channel_thread".to_string(),
        "set_channel_reaction".to_string(),
        "create_channel_stimulus".to_string(),
        "todo_write".to_string(),
        "request_parent_clarification".to_string(),
        "task_output".to_string(),
        "task_stop".to_string(),
        "wake_after".to_string(),
        "wake_at".to_string(),
        "schedule_create".to_string(),
        "schedule_list".to_string(),
        "schedule_get".to_string(),
        "schedule_cancel".to_string(),
        "schedule_pause".to_string(),
        "schedule_resume".to_string(),
        "schedule_trigger_now".to_string(),
    ]
}

fn coordinator_agent_tools() -> Vec<String> {
    vec![
        DYNAMIC_MCP_TOOL_ALLOWLIST_SENTINEL.to_string(),
        "list_skills".to_string(),
        "use_skill".to_string(),
        "spawn_agent".to_string(),
        "list_agents".to_string(),
        "list_agent_summaries".to_string(),
        "get_agent".to_string(),
        "message_agent".to_string(),
        "wait_agent".to_string(),
        "task_create".to_string(),
        "task_get".to_string(),
        "task_list".to_string(),
        "task_output".to_string(),
        "task_update".to_string(),
        "task_stop".to_string(),
        "task_delete".to_string(),
        "todo_write".to_string(),
        "request_parent_clarification".to_string(),
        "read_file".to_string(),
        "list_files".to_string(),
        "glob_search".to_string(),
        "grep_search".to_string(),
        "bash".to_string(),
        "web_search".to_string(),
        "web_fetch".to_string(),
        "emit_output".to_string(),
        "read_channel_thread".to_string(),
        "set_channel_reaction".to_string(),
        "create_channel_stimulus".to_string(),
        "wake_after".to_string(),
        "wake_at".to_string(),
        "schedule_create".to_string(),
        "schedule_list".to_string(),
        "schedule_get".to_string(),
        "schedule_cancel".to_string(),
        "schedule_pause".to_string(),
        "schedule_resume".to_string(),
        "schedule_trigger_now".to_string(),
    ]
}

pub(super) fn built_in_agent_profile(agent_type: Option<&str>) -> Result<AgentProfileTemplate> {
    match agent_type.unwrap_or("default") {
        "default" => Ok(AgentProfileTemplate {
            tool_surface: ToolSurfaceFilter {
                allowlist: default_async_agent_tools(),
                denylist: base_agent_disallowed_tools(),
            },
            ..AgentProfileTemplate::default()
        }),
        "plan" => Ok(AgentProfileTemplate {
            prompt: Some(
                "You are a planning specialist. Break work into tracked tasks and todos, gather context with read-only tools, and avoid changing files unless explicitly instructed to leave plan mode."
                    .to_string(),
            ),
            prompt_merge_mode: PromptMergeMode::Append,
            tool_surface: ToolSurfaceFilter {
                allowlist: vec![
                    DYNAMIC_MCP_TOOL_ALLOWLIST_SENTINEL.to_string(),
                    "list_skills".to_string(),
                    "read_file".to_string(),
                    "list_files".to_string(),
                    "glob_search".to_string(),
                    "grep_search".to_string(),
                    "web_search".to_string(),
                    "web_fetch".to_string(),
                    "emit_output".to_string(),
                    "read_channel_thread".to_string(),
                    "set_channel_reaction".to_string(),
                    "create_channel_stimulus".to_string(),
                    "request_parent_clarification".to_string(),
                    "task_create".to_string(),
                    "task_get".to_string(),
                    "task_list".to_string(),
                    "task_update".to_string(),
                    "task_stop".to_string(),
                    "task_delete".to_string(),
                    "todo_write".to_string(),
                    "schedule_list".to_string(),
                    "schedule_get".to_string(),
                ],
                denylist: Vec::new(),
            },
            ..AgentProfileTemplate::default()
        }),
        "verification" => Ok(AgentProfileTemplate {
            prompt: Some(
                "You are a verification specialist. Inspect the current state, run safe checks, and report whether the requested work is complete without making unrelated edits."
                    .to_string(),
            ),
            prompt_merge_mode: PromptMergeMode::Append,
            tool_surface: ToolSurfaceFilter {
                allowlist: vec![
                    "list_skills".to_string(),
                    "read_file".to_string(),
                    "list_files".to_string(),
                    "glob_search".to_string(),
                    "grep_search".to_string(),
                    "bash".to_string(),
                    "web_search".to_string(),
                    "web_fetch".to_string(),
                    "emit_output".to_string(),
                    "read_channel_thread".to_string(),
                    "set_channel_reaction".to_string(),
                    "create_channel_stimulus".to_string(),
                    "request_parent_clarification".to_string(),
                    "get_agent".to_string(),
                    "message_agent".to_string(),
                    "wait_agent".to_string(),
                    "schedule_list".to_string(),
                    "schedule_get".to_string(),
                    "task_get".to_string(),
                    "task_list".to_string(),
                    "task_output".to_string(),
                    "task_update".to_string(),
                ],
                denylist: vec![
                    "write_file".to_string(),
                    "edit_file".to_string(),
                    "apply_patch".to_string(),
                ],
            },
            ..AgentProfileTemplate::default()
        }),
        "coordinator" => Ok(AgentProfileTemplate {
            prompt: Some(
                "You are a coordination specialist. Delegate independent work to child agents, track progress with tasks and todos, and use direct execution tools only when coordination alone is insufficient."
                    .to_string(),
            ),
            prompt_merge_mode: PromptMergeMode::Append,
            tool_surface: ToolSurfaceFilter {
                allowlist: coordinator_agent_tools(),
                denylist: Vec::new(),
            },
            ..AgentProfileTemplate::default()
        }),
        other => Err(anyhow!("unknown agent_type {other}")),
    }
}

fn built_in_profile_is_strict(agent_type: Option<&str>) -> bool {
    matches!(agent_type, Some("verification"))
}

pub(crate) fn parse_permission_mode(value: &str) -> Option<PermissionMode> {
    match value {
        "default" => Some(PermissionMode::Default),
        "acceptEdits" => Some(PermissionMode::AcceptEdits),
        "bypassPermissions" => Some(PermissionMode::BypassPermissions),
        "plan" => Some(PermissionMode::Plan),
        "dontAsk" => Some(PermissionMode::DontAsk),
        _ => None,
    }
}

pub(super) fn render_permission_mode(mode: &PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::BypassPermissions => "bypassPermissions",
        PermissionMode::Plan => "plan",
        PermissionMode::DontAsk => "dontAsk",
    }
}

/// Waits for one agent snapshot to settle by polling the daemon control plane.
pub async fn wait_for_agent_snapshot<C>(
    control: &C,
    caller_agent_id: &str,
    agent_id: &str,
    timeout: Duration,
) -> Result<ManagedAgentSnapshot>
where
    C: DaemonToolControl + ?Sized,
{
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = control.get_agent(caller_agent_id, agent_id).await?;
        if snapshot.agent.status != kheish_agent::AgentStatus::Running
            && snapshot.agent.status != kheish_agent::AgentStatus::WaitingForApproval
            && snapshot.agent.status != kheish_agent::AgentStatus::WaitingForUserInput
        {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Ok(snapshot);
        }
        sleep(Duration::from_millis(200)).await;
    }
}

/// Builds the daemon-side subtask request used by `spawn_agent`.
pub fn sidechain_request_from_tool(
    request: &SpawnAgentToolRequest,
) -> Result<SpawnSidechainRequest> {
    validate_text_or_input_items_with_asset_ids(
        "prompt",
        &request.prompt,
        &request.input_items,
        &request.asset_ids,
    )?;
    let profile = built_in_agent_profile(request.agent_type.as_deref())?;
    let mut allowlist = profile.tool_surface.allowlist;
    let normalized_requested_tools = normalize_tool_names(&request.allowed_tools);
    if !normalized_requested_tools.is_empty() {
        allowlist.retain(|tool| tool != DYNAMIC_MCP_TOOL_ALLOWLIST_SENTINEL);
    }
    if built_in_profile_is_strict(request.agent_type.as_deref()) {
        let allowed = allowlist
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let disallowed = normalized_requested_tools
            .iter()
            .filter(|tool| !allowed.contains(*tool))
            .cloned()
            .collect::<Vec<_>>();
        if !disallowed.is_empty() {
            bail!(
                "agent profile {} does not allow requested tools: {}",
                request.agent_type.as_deref().unwrap_or("default"),
                disallowed.join(", ")
            );
        }
    }
    allowlist.extend(normalized_requested_tools);
    allowlist = normalize_tool_names(&allowlist);

    let mut denylist = profile.tool_surface.denylist;
    denylist.extend(normalize_tool_names(&request.blocked_tools));
    denylist = normalize_tool_names(&denylist);

    let explicit_generation = request.generation.clone();
    if let (Some(model), Some(generation_model)) = (
        request.model.as_deref(),
        explicit_generation
            .as_ref()
            .and_then(|generation| generation.model.as_deref()),
    ) {
        anyhow::ensure!(
            model == generation_model,
            "spawn_agent model conflicts with generation.model"
        );
    }
    if let (Some(fallback_model), Some(generation_fallback_model)) = (
        request.fallback_model.as_deref(),
        explicit_generation
            .as_ref()
            .and_then(|generation| generation.fallback_model.as_deref()),
    ) {
        anyhow::ensure!(
            fallback_model == generation_fallback_model,
            "spawn_agent fallback_model conflicts with generation.fallback_model"
        );
    }
    let profile_generation = (profile.generation != ModelGenerationConfig::default())
        .then_some(profile.generation.clone());
    let mut generation =
        ModelGenerationConfig::merge_override(profile_generation, explicit_generation);
    if request.model.is_some() || request.fallback_model.is_some() {
        let config = generation.get_or_insert_with(ModelGenerationConfig::default);
        if let Some(model) = request.model.clone() {
            config.model = Some(model);
        }
        if let Some(fallback_model) = request.fallback_model.clone() {
            config.fallback_model = Some(fallback_model);
        }
    }
    if generation.as_ref() == Some(&ModelGenerationConfig::default()) {
        generation = None;
    }

    let effective_isolation = request.isolation.clone().unwrap_or_default();
    let worktree_path = match effective_isolation {
        SpawnIsolation::Shared => request.cwd.clone(),
        SpawnIsolation::Worktree => request.cwd.clone(),
    };
    let (content, input_items) = if request.input_items.is_empty() && !request.asset_ids.is_empty()
    {
        (
            String::new(),
            input_items_from_text_and_asset_ids(&request.prompt, &request.asset_ids),
        )
    } else {
        (request.prompt.clone(), request.input_items.clone())
    };

    Ok(SpawnSidechainRequest {
        session_id: request.session_id.clone(),
        thread_id: request.thread_id.clone(),
        route_policy: None,
        provider: request.provider.clone(),
        permission_mode: request.mode.clone(),
        retention: Some(
            request
                .retention
                .clone()
                .unwrap_or(ChildRetentionPolicy::Retain),
        ),
        nickname: request.nickname.clone(),
        spawn_request_id: request.spawn_request_id.clone(),
        spawned_by_run_id: request.spawned_by_run_id.clone(),
        fork_context: kheish_agent::ForkContext {
            parent_assistant_message: request.parent_assistant_message.clone().unwrap_or_default(),
            inherited_tool_call_ids: request.inherited_tool_call_ids.clone(),
            team_name: request.team_name.clone(),
            isolation: Some(match effective_isolation {
                SpawnIsolation::Shared => "shared".to_string(),
                SpawnIsolation::Worktree => "worktree".to_string(),
            }),
            system_prompt: request
                .system_prompt
                .clone()
                .or(profile.prompt)
                .unwrap_or_default(),
            prompt_merge_mode: request
                .prompt_merge_mode
                .clone()
                .unwrap_or(profile.prompt_merge_mode),
            provider: request.provider.clone(),
            generation,
            tool_surface: ToolSurfaceFilter {
                allowlist,
                denylist,
            },
            worktree_path,
        },
        generation: None,
        tool_surface: None,
        capability_scope: request.capability_scope.clone(),
        credential_scope: request.credential_scope.clone(),
        subtask: Some(SidechainSubtaskRequest {
            name: request.name.clone(),
            description: request.description.clone(),
            content,
            input_items,
            attachments: Vec::new(),
        }),
    })
}

/// Builds one mailbox request from the `message_agent` tool payload.
pub fn mailbox_request_from_tool(
    from_agent_id: &str,
    request: MessageAgentToolRequest,
) -> Result<PostMailboxRequest> {
    validate_text_or_input_items_with_asset_ids(
        "message",
        &request.message,
        &request.input_items,
        &request.asset_ids,
    )?;
    let input_items = if request.input_items.is_empty() && !request.asset_ids.is_empty() {
        input_items_from_text_and_asset_ids(&request.message, &request.asset_ids)
    } else {
        request.input_items.clone()
    };
    let mut payload = serde_json::Map::new();
    payload.insert(
        "type".to_string(),
        Value::String(
            request
                .message_type
                .unwrap_or_else(|| "message".to_string()),
        ),
    );
    if input_items.is_empty() && !request.message.trim().is_empty() {
        payload.insert("message".to_string(), Value::String(request.message));
    }
    if !input_items.is_empty() {
        payload.insert("input_items".to_string(), json!(input_items));
    }
    Ok(PostMailboxRequest {
        message_id: None,
        from_agent_id: from_agent_id.to_string(),
        to_agent_id: request.agent_id,
        subject: request.subject,
        ttl_ms: None,
        payload: Value::Object(payload),
    })
}
