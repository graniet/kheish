use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolInputKind,
    ToolSchemaField,
};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::shared::{
    SharedConfig, clamp_limit, optional_string_field, optional_usize_field, string_field,
    tool_schema,
};

pub(crate) struct ListFilesTool {
    shared: Arc<SharedConfig>,
}

impl ListFilesTool {
    pub(crate) fn new(shared: Arc<SharedConfig>) -> Self {
        Self { shared }
    }
}

#[async_trait]
impl Tool for ListFilesTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "list_files".to_string(),
            description: "Lists files under a workspace directory.".to_string(),
            schema: tool_schema(vec![
                ToolSchemaField {
                    name: "base".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some("Optional base directory.".to_string()),
                },
                ToolSchemaField {
                    name: "limit".to_string(),
                    kind: ToolInputKind::Number,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some("Maximum number of paths to return.".to_string()),
                },
            ]),
            timeout_ms: 10_000,
            sandbox: SandboxProfile::ReadOnly,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let workspace = self.shared.workspace(&ctx);
        let base = workspace.resolve_base_dir(optional_string_field(&input, "base").as_deref())?;
        let limit = clamp_limit(
            optional_usize_field(&input, "limit"),
            self.shared.max_results,
        );
        let files = list_files(&base, limit).await?;
        Ok(ToolExecutionOutput::json(json!({
            "base": base.display().to_string(),
            "paths": files,
        })))
    }
}

pub(crate) struct GlobSearchTool {
    shared: Arc<SharedConfig>,
}

impl GlobSearchTool {
    pub(crate) fn new(shared: Arc<SharedConfig>) -> Self {
        Self { shared }
    }
}

#[async_trait]
impl Tool for GlobSearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "glob_search".to_string(),
            description: "Finds files whose relative paths match a glob pattern.".to_string(),
            schema: tool_schema(vec![
                ToolSchemaField {
                    name: "pattern".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: true,
                    description: Some("Glob pattern, for example `src/**/*.rs`.".to_string()),
                },
                ToolSchemaField {
                    name: "base".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some("Optional base directory.".to_string()),
                },
                ToolSchemaField {
                    name: "limit".to_string(),
                    kind: ToolInputKind::Number,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some("Maximum number of paths to return.".to_string()),
                },
            ]),
            timeout_ms: 10_000,
            sandbox: SandboxProfile::ReadOnly,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let workspace = self.shared.workspace(&ctx);
        let base = workspace.resolve_base_dir(optional_string_field(&input, "base").as_deref())?;
        let pattern = string_field(&input, "pattern")?;
        let limit = clamp_limit(
            optional_usize_field(&input, "limit"),
            self.shared.max_results,
        );
        let paths = glob_search(&base, &pattern, limit).await?;
        Ok(ToolExecutionOutput::json(json!({
            "base": base.display().to_string(),
            "pattern": pattern,
            "paths": paths,
        })))
    }
}

pub(crate) struct GrepSearchTool {
    shared: Arc<SharedConfig>,
}

impl GrepSearchTool {
    pub(crate) fn new(shared: Arc<SharedConfig>) -> Self {
        Self { shared }
    }
}

#[async_trait]
impl Tool for GrepSearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "grep_search".to_string(),
            description:
                "Searches text across workspace files using ripgrep semantics when available."
                    .to_string(),
            schema: tool_schema(vec![
                ToolSchemaField {
                    name: "pattern".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: true,
                    description: Some("Search pattern.".to_string()),
                },
                ToolSchemaField {
                    name: "base".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some("Optional base directory.".to_string()),
                },
                ToolSchemaField {
                    name: "limit".to_string(),
                    kind: ToolInputKind::Number,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some("Maximum number of matches to return.".to_string()),
                },
            ]),
            timeout_ms: 10_000,
            sandbox: SandboxProfile::ReadOnly,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let workspace = self.shared.workspace(&ctx);
        let base = workspace.resolve_base_dir(optional_string_field(&input, "base").as_deref())?;
        let pattern = string_field(&input, "pattern")?;
        let limit = clamp_limit(
            optional_usize_field(&input, "limit"),
            self.shared.max_results,
        );
        let matches = grep_search(&base, &pattern, limit).await?;
        Ok(ToolExecutionOutput::json(json!({
            "base": base.display().to_string(),
            "pattern": pattern,
            "matches": matches,
        })))
    }
}

async fn list_files(base: &Path, limit: usize) -> Result<Vec<String>> {
    if let Ok(paths) = run_rg_lines(base, &["--files"]).await {
        return Ok(paths.into_iter().take(limit).collect());
    }
    let files = tokio::task::spawn_blocking({
        let base = base.to_path_buf();
        move || collect_files(&base)
    })
    .await??;
    Ok(files.into_iter().take(limit).collect())
}

async fn glob_search(base: &Path, pattern: &str, limit: usize) -> Result<Vec<String>> {
    if let Ok(paths) = run_rg_lines(base, &["--files", "-g", pattern]).await {
        return Ok(paths.into_iter().take(limit).collect());
    }
    let files = tokio::task::spawn_blocking({
        let base = base.to_path_buf();
        let pattern = pattern.to_string();
        move || {
            let files = collect_files(&base)?;
            Ok::<Vec<String>, anyhow::Error>(
                files
                    .into_iter()
                    .filter(|path| glob_matches(pattern.as_str(), path))
                    .take(limit)
                    .collect(),
            )
        }
    })
    .await??;
    Ok(files)
}

async fn grep_search(base: &Path, pattern: &str, limit: usize) -> Result<Vec<Value>> {
    if let Ok(lines) = run_rg_lines(base, &["-n", "--no-heading", pattern]).await {
        return Ok(lines
            .into_iter()
            .filter_map(|line| parse_grep_line(&line))
            .take(limit)
            .collect());
    }
    let base = base.to_path_buf();
    let pattern = pattern.to_string();
    tokio::task::spawn_blocking(move || {
        let files = collect_files(&base)?;
        let mut matches = Vec::new();
        for file in files {
            if matches.len() >= limit {
                break;
            }
            let absolute = base.join(&file);
            let content = std::fs::read_to_string(&absolute).unwrap_or_default();
            for (line_index, line) in content.lines().enumerate() {
                if line.contains(&pattern) {
                    matches.push(json!({
                        "path": file,
                        "line": line_index + 1,
                        "text": line,
                    }));
                    if matches.len() >= limit {
                        break;
                    }
                }
            }
        }
        Ok(matches)
    })
    .await?
}

fn collect_files(base: &Path) -> Result<Vec<String>> {
    let mut queue = VecDeque::from([base.to_path_buf()]);
    let mut files = Vec::new();
    while let Some(dir) = queue.pop_front() {
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("failed to list {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                queue.push_back(path);
                continue;
            }
            if file_type.is_file() {
                files.push(
                    path.strip_prefix(base)
                        .unwrap_or(&path)
                        .display()
                        .to_string(),
                );
            }
        }
    }
    files.sort();
    Ok(files)
}

fn glob_matches(pattern: &str, candidate: &str) -> bool {
    glob_match_recursive(pattern.as_bytes(), candidate.as_bytes())
}

fn glob_match_recursive(pattern: &[u8], candidate: &[u8]) -> bool {
    if pattern.is_empty() {
        return candidate.is_empty();
    }
    match pattern[0] {
        b'*' => {
            glob_match_recursive(&pattern[1..], candidate)
                || (!candidate.is_empty() && glob_match_recursive(pattern, &candidate[1..]))
        }
        b'?' => !candidate.is_empty() && glob_match_recursive(&pattern[1..], &candidate[1..]),
        byte => {
            !candidate.is_empty()
                && byte == candidate[0]
                && glob_match_recursive(&pattern[1..], &candidate[1..])
        }
    }
}

fn parse_grep_line(line: &str) -> Option<Value> {
    let mut parts = line.splitn(3, ':');
    let path = parts.next()?;
    let line_number = parts.next()?.parse::<usize>().ok()?;
    let text = parts.next().unwrap_or_default();
    Some(json!({
        "path": path,
        "line": line_number,
        "text": text,
    }))
}

async fn run_rg_lines(base: &Path, args: &[&str]) -> Result<Vec<String>> {
    let output = Command::new("rg")
        .args(args)
        .current_dir(base)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;
    let output = match output {
        Ok(output) if output.status.success() || output.status.code() == Some(1) => output,
        Ok(output) => bail!("rg exited with status {}", output.status),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("rg is not installed")
        }
        Err(error) => return Err(error.into()),
    };
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(ToString::to_string)
        .collect())
}
