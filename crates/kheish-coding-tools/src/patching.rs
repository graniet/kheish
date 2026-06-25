use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use kheish_runtime::{
    SandboxProfile, StructuredFieldSchema, StructuredValueKind, Tool, ToolContext, ToolDescriptor,
    ToolExecutionOutput, ToolInputKind, ToolSchemaField,
};
use kheish_types::ContextUpdate;
use serde_json::{Value, json};

use crate::shared::{
    PreparedAtomicWrite, SharedConfig, abort_prepared_atomic_write, assert_expected_sha256,
    commit_prepared_atomic_write, optional_bool_field, optional_string_field,
    prepare_atomic_write_file, read_text_file_for_edit, sha256_hex, string_field, tool_schema,
};

pub(crate) struct ApplyPatchTool {
    shared: Arc<SharedConfig>,
}

impl ApplyPatchTool {
    pub(crate) fn new(shared: Arc<SharedConfig>) -> Self {
        Self { shared }
    }
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "apply_patch".to_string(),
            description:
                "Applies one or more exact text replacement hunks atomically per file after validating every hunk."
                    .to_string(),
            schema: tool_schema(vec![ToolSchemaField {
                name: "hunks".to_string(),
                kind: ToolInputKind::Array,
                item_kind: None,
                structured_schema: Some(hunks_schema()),
                required: true,
                description: Some(
                    "Ordered hunks with path, old_text, new_text, and optional replace_all."
                        .to_string(),
                ),
            }]),
            timeout_ms: 10_000,
            sandbox: SandboxProfile::WorkspaceWrite,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let workspace = self.shared.workspace(&ctx);
        let hunks = parse_hunks(&input)?;
        if hunks.is_empty() {
            bail!("apply_patch requires at least one hunk");
        }
        let hunks = hunks
            .into_iter()
            .map(|hunk| {
                if hunk.old_text.is_empty() {
                    bail!("old_text must not be empty for {}", hunk.path);
                }
                let path = workspace.resolve_write_path(&hunk.path)?;
                let relative_path = workspace.workspace_relative_string(&path);
                Ok(ResolvedPatchHunk {
                    path,
                    relative_path,
                    old_text: hunk.old_text,
                    new_text: hunk.new_text,
                    replace_all: hunk.replace_all,
                    expected_sha256: hunk.expected_sha256,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut locked_paths = BTreeSet::new();
        for hunk in &hunks {
            locked_paths.insert(hunk.path.clone());
        }
        let mut guards = Vec::new();
        for path in &locked_paths {
            guards.push(self.shared.lock_path(path).await?);
        }

        let workspace_root = workspace.root()?;
        let mut files = BTreeMap::<PathBuf, PendingPatchFile>::new();
        for hunk in hunks {
            let entry = match files.entry(hunk.path.clone()) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    assert_expected_sha256(
                        workspace_root.clone(),
                        hunk.path.clone(),
                        hunk.expected_sha256.as_deref(),
                    )
                    .await?;
                    let original = read_text_file_for_edit(
                        workspace_root.clone(),
                        hunk.path.clone(),
                        self.shared.max_edit_bytes,
                    )
                    .await
                    .with_context(|| format!("failed to prepare {}", hunk.path.display()))?;
                    entry.insert(PendingPatchFile {
                        relative_path: hunk.relative_path.clone(),
                        expected_sha256: hunk.expected_sha256.clone(),
                        original: original.clone(),
                        updated: original,
                        hunk_count: 0,
                        replacements: 0,
                    })
                }
            };
            if let Some(expected_sha256) = &hunk.expected_sha256 {
                match &entry.expected_sha256 {
                    Some(existing) if existing != expected_sha256 => bail!(
                        "conflicting expected_sha256 values for {}",
                        hunk.path.display()
                    ),
                    Some(_) => {}
                    None => entry.expected_sha256 = Some(expected_sha256.clone()),
                }
            }

            let matches = entry.updated.matches(&hunk.old_text).count();
            if matches == 0 {
                bail!(
                    "patch hunk target not found in {} after applying previous hunks",
                    hunk.path.display()
                );
            }
            let applied_replacements = if hunk.replace_all { matches } else { 1 };
            entry.updated = if hunk.replace_all {
                entry.updated.replace(&hunk.old_text, &hunk.new_text)
            } else {
                entry.updated.replacen(&hunk.old_text, &hunk.new_text, 1)
            };
            entry.hunk_count += 1;
            entry.replacements += applied_replacements;
        }

        let mut staged_updates = BTreeMap::<PathBuf, PreparedAtomicWrite>::new();
        let mut staged_rollbacks = BTreeMap::<PathBuf, PreparedAtomicWrite>::new();
        for (path, file) in &files {
            match prepare_atomic_write_file(
                workspace_root.clone(),
                path.clone(),
                file.updated.clone().into_bytes(),
            )
            .await
            {
                Ok(prepared) => {
                    staged_updates.insert(path.clone(), prepared);
                }
                Err(error) => {
                    abort_prepared_writes(staged_updates).await;
                    return Err(error)
                        .with_context(|| format!("failed to stage write for {}", path.display()));
                }
            }
            match prepare_atomic_write_file(
                workspace_root.clone(),
                path.clone(),
                file.original.clone().into_bytes(),
            )
            .await
            {
                Ok(prepared) => {
                    staged_rollbacks.insert(path.clone(), prepared);
                }
                Err(error) => {
                    abort_prepared_writes(staged_updates).await;
                    abort_prepared_writes(staged_rollbacks).await;
                    return Err(error).with_context(|| {
                        format!("failed to stage rollback for {}", path.display())
                    });
                }
            }
        }

        let mut committed = BTreeSet::<PathBuf>::new();
        for path in files.keys() {
            let prepared = staged_updates
                .remove(path)
                .ok_or_else(|| anyhow!("missing staged write for {}", path.display()))?;
            if let Err(error) = commit_prepared_atomic_write(prepared).await {
                rollback_committed_writes(&mut staged_rollbacks, &committed).await;
                abort_prepared_writes(staged_updates).await;
                abort_prepared_writes(staged_rollbacks).await;
                return Err(error).with_context(|| format!("failed to write {}", path.display()));
            }
            committed.insert(path.clone());
        }
        abort_prepared_writes(staged_rollbacks).await;

        let changed_files = files
            .iter()
            .map(|(_, file)| {
                json!({
                    "path": file.relative_path,
                    "hunks": file.hunk_count,
                    "replacements": file.replacements,
                    "bytes_written": file.updated.len(),
                    "sha256": sha256_hex(file.updated.as_bytes()),
                })
            })
            .collect::<Vec<_>>();
        let context_updates = files
            .values()
            .map(|file| ContextUpdate::FileModified {
                path: file.relative_path.clone(),
            })
            .collect::<Vec<_>>();

        Ok(ToolExecutionOutput::with_updates(
            json!({
                "changed_files": changed_files,
                "file_count": files.len(),
            }),
            context_updates,
        ))
    }
}

#[derive(Clone, Debug)]
struct PatchHunk {
    path: String,
    old_text: String,
    new_text: String,
    replace_all: bool,
    expected_sha256: Option<String>,
}

#[derive(Clone, Debug)]
struct ResolvedPatchHunk {
    path: PathBuf,
    relative_path: String,
    old_text: String,
    new_text: String,
    replace_all: bool,
    expected_sha256: Option<String>,
}

#[derive(Clone, Debug)]
struct PendingPatchFile {
    relative_path: String,
    expected_sha256: Option<String>,
    original: String,
    updated: String,
    hunk_count: usize,
    replacements: usize,
}

fn parse_hunks(input: &Value) -> Result<Vec<PatchHunk>> {
    let hunks = input
        .get("hunks")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing array field `hunks`"))?;
    hunks
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let object = value
                .as_object()
                .ok_or_else(|| anyhow!("hunks[{index}] must be an object"))?;
            let hunk_value = Value::Object(object.clone());
            Ok(PatchHunk {
                path: string_field(&hunk_value, "path")?,
                old_text: string_field(&hunk_value, "old_text")?,
                new_text: string_field(&hunk_value, "new_text")?,
                replace_all: optional_bool_field(&hunk_value, "replace_all").unwrap_or(false),
                expected_sha256: optional_string_field(&hunk_value, "expected_sha256"),
            })
        })
        .collect()
}

async fn rollback_committed_writes(
    staged_rollbacks: &mut BTreeMap<PathBuf, PreparedAtomicWrite>,
    committed: &BTreeSet<PathBuf>,
) {
    for path in committed {
        if let Some(prepared) = staged_rollbacks.remove(path) {
            let _ = commit_prepared_atomic_write(prepared).await;
        }
    }
}

async fn abort_prepared_writes(writes: BTreeMap<PathBuf, PreparedAtomicWrite>) {
    for (_, prepared) in writes {
        abort_prepared_atomic_write(prepared).await;
    }
}

fn hunks_schema() -> StructuredFieldSchema {
    let mut hunk_schema = StructuredFieldSchema::new(StructuredValueKind::Object);
    hunk_schema.fields.insert(
        "path".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    hunk_schema.fields.insert(
        "old_text".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    hunk_schema.fields.insert(
        "new_text".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    hunk_schema.optional_fields.insert(
        "replace_all".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::Boolean),
    );
    hunk_schema.optional_fields.insert(
        "expected_sha256".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );

    let mut schema = StructuredFieldSchema::new(StructuredValueKind::Array);
    schema.items = Some(Box::new(hunk_schema));
    schema
}
