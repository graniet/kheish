use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolInputKind,
    ToolSchemaField,
};
use kheish_types::ContextUpdate;
use serde_json::{Value, json};

use crate::shared::{
    SharedConfig, assert_expected_sha256, atomic_write_file, optional_string_field,
    read_text_file_for_edit, sha256_hex, string_field, tool_schema,
};

pub(crate) struct EditFileTool {
    shared: Arc<SharedConfig>,
}

impl EditFileTool {
    pub(crate) fn new(shared: Arc<SharedConfig>) -> Self {
        Self { shared }
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "edit_file".to_string(),
            description: "Applies an in-place string replacement to an existing file.".to_string(),
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
                    name: "old_text".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: true,
                    description: Some("Existing text to replace.".to_string()),
                },
                ToolSchemaField {
                    name: "new_text".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: true,
                    description: Some("Replacement text.".to_string()),
                },
                ToolSchemaField {
                    name: "replace_all".to_string(),
                    kind: ToolInputKind::Boolean,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some(
                        "Replace every occurrence instead of only the first.".to_string(),
                    ),
                },
                ToolSchemaField {
                    name: "expected_sha256".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some(
                        "Optional SHA-256 digest that the existing file must match before editing."
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
        let old_text = string_field(&input, "old_text")?;
        let new_text = string_field(&input, "new_text")?;
        let expected_sha256 = optional_string_field(&input, "expected_sha256");
        let replace_all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let _guard = self.shared.lock_path(&path).await?;
        let workspace_root = workspace.root()?;
        assert_expected_sha256(
            workspace_root.clone(),
            path.clone(),
            expected_sha256.as_deref(),
        )
        .await?;
        let original = read_text_file_for_edit(
            workspace_root.clone(),
            path.clone(),
            self.shared.max_edit_bytes,
        )
        .await?;
        let replacements = original.matches(&old_text).count();
        if replacements == 0 {
            bail!("edit target not found in {}", path.display());
        }
        let updated = if replace_all {
            original.replace(&old_text, &new_text)
        } else {
            original.replacen(&old_text, &new_text, 1)
        };
        let updated_bytes = updated.into_bytes();
        atomic_write_file(workspace_root, path.clone(), updated_bytes.clone()).await?;
        Ok(ToolExecutionOutput::with_updates(
            json!({
                "path": path.display().to_string(),
                "replacements": if replace_all { replacements } else { 1 },
                "sha256": sha256_hex(&updated_bytes),
            }),
            vec![ContextUpdate::FileModified {
                path: workspace.workspace_relative_string(&path),
            }],
        ))
    }
}
