use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolInputKind,
    ToolSchemaField,
};
use kheish_types::ContextUpdate;
use serde_json::{Value, json};

use crate::shared::{
    SharedConfig, assert_expected_sha256, atomic_write_file, optional_bool_field,
    optional_string_field, optional_usize_field, read_file_bytes_limited, sha256_hex, string_field,
    tool_schema, truncate_text,
};

pub(crate) struct ReadFileTool {
    shared: Arc<SharedConfig>,
}

impl ReadFileTool {
    pub(crate) fn new(shared: Arc<SharedConfig>) -> Self {
        Self { shared }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "read_file".to_string(),
            description: "Reads one file from the workspace with optional line slicing."
                .to_string(),
            schema: tool_schema(vec![
                ToolSchemaField {
                    name: "path".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: true,
                    description: Some("Workspace-relative or absolute file path.".to_string()),
                },
                ToolSchemaField {
                    name: "start_line".to_string(),
                    kind: ToolInputKind::Number,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some("Optional 1-based start line.".to_string()),
                },
                ToolSchemaField {
                    name: "line_count".to_string(),
                    kind: ToolInputKind::Number,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some("Optional number of lines to return.".to_string()),
                },
                ToolSchemaField {
                    name: "include_base64".to_string(),
                    kind: ToolInputKind::Boolean,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some(
                        "Include base64 content for binary or exact byte reads.".to_string(),
                    ),
                },
            ]),
            timeout_ms: 5_000,
            sandbox: SandboxProfile::ReadOnly,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let workspace = self.shared.workspace(&ctx);
        let path = workspace.resolve_existing_path(&string_field(&input, "path")?)?;
        let workspace_root = workspace.root()?;
        let (bytes, truncated_by_bytes) =
            read_file_bytes_limited(workspace_root, path.clone(), self.shared.max_read_bytes)
                .await?;
        let digest = sha256_hex(&bytes);
        let include_base64 = optional_bool_field(&input, "include_base64").unwrap_or(false);
        let content = match std::str::from_utf8(&bytes) {
            Ok(content) => content,
            Err(_) => {
                let mut output = json!({
                    "path": path.display().to_string(),
                    "start_line": 1,
                    "line_count": 0,
                    "content": "",
                    "bytes_read": bytes.len(),
                    "sha256": digest,
                    "encoding": "binary",
                    "binary": true,
                    "truncated": truncated_by_bytes,
                });
                if include_base64 {
                    output["content_base64"] = Value::String(BASE64.encode(&bytes));
                }
                return Ok(ToolExecutionOutput::with_updates(
                    output,
                    vec![ContextUpdate::FileRead {
                        path: workspace.workspace_relative_string(&path),
                    }],
                ));
            }
        };
        let start_line = optional_usize_field(&input, "start_line")
            .unwrap_or(1)
            .max(1);
        let line_count = optional_usize_field(&input, "line_count");
        let lines = content.lines().collect::<Vec<_>>();
        let start_index = start_line.saturating_sub(1).min(lines.len());
        let end_index = line_count
            .map(|count| start_index.saturating_add(count).min(lines.len()))
            .unwrap_or(lines.len());
        let slice = lines[start_index..end_index].join("\n");
        let (content, truncated_by_slice) = truncate_text(&slice, self.shared.max_read_bytes);
        let mut output = json!({
                "path": path.display().to_string(),
                "start_line": start_line,
                "line_count": end_index.saturating_sub(start_index),
                "content": content,
                "bytes_read": bytes.len(),
                "sha256": digest,
                "encoding": "utf-8",
                "binary": false,
                "truncated": truncated_by_bytes || truncated_by_slice,
        });
        if include_base64 {
            output["content_base64"] = Value::String(BASE64.encode(&bytes));
        }
        Ok(ToolExecutionOutput::with_updates(
            output,
            vec![ContextUpdate::FileRead {
                path: workspace.workspace_relative_string(&path),
            }],
        ))
    }
}

pub(crate) struct WriteFileTool {
    shared: Arc<SharedConfig>,
}

impl WriteFileTool {
    pub(crate) fn new(shared: Arc<SharedConfig>) -> Self {
        Self { shared }
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "write_file".to_string(),
            description:
                "Writes one file inside the workspace, creating parent directories when needed."
                    .to_string(),
            schema: tool_schema(vec![
                ToolSchemaField {
                    name: "path".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: true,
                    description: Some("Workspace-relative or absolute file path.".to_string()),
                },
                ToolSchemaField {
                    name: "content".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some("Full file contents.".to_string()),
                },
                ToolSchemaField {
                    name: "content_base64".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some(
                        "Base64-encoded bytes to write instead of UTF-8 content.".to_string(),
                    ),
                },
                ToolSchemaField {
                    name: "expected_sha256".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some(
                        "Optional SHA-256 digest that the existing file must match before writing."
                            .to_string(),
                    ),
                },
            ]),
            timeout_ms: 5_000,
            sandbox: SandboxProfile::WorkspaceWrite,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let workspace = self.shared.workspace(&ctx);
        let path = workspace.resolve_write_path(&string_field(&input, "path")?)?;
        let content = parse_write_content(&input)?;
        let expected_sha256 = optional_string_field(&input, "expected_sha256");
        let _guard = self.shared.lock_path(&path).await?;
        let workspace_root = workspace.root()?;
        assert_expected_sha256(
            workspace_root.clone(),
            path.clone(),
            expected_sha256.as_deref(),
        )
        .await?;
        atomic_write_file(workspace_root, path.clone(), content.clone()).await?;
        Ok(ToolExecutionOutput::with_updates(
            json!({
                "path": path.display().to_string(),
                "bytes_written": content.len(),
                "sha256": sha256_hex(&content),
            }),
            vec![ContextUpdate::FileModified {
                path: workspace.workspace_relative_string(&path),
            }],
        ))
    }
}

fn parse_write_content(input: &Value) -> Result<Vec<u8>> {
    match (
        optional_string_field(input, "content"),
        optional_string_field(input, "content_base64"),
    ) {
        (Some(_), Some(_)) => bail!("provide exactly one of content or content_base64"),
        (Some(content), None) => Ok(content.into_bytes()),
        (None, Some(content_base64)) => BASE64
            .decode(content_base64.as_bytes())
            .map_err(|error| anyhow!("content_base64 is not valid base64: {error}")),
        (None, None) => bail!("missing one of content or content_base64"),
    }
}
