use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolInputKind,
    ToolSchemaField, current_cancellation_token, interrupted_error,
};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{self, Duration};

use crate::shared::{
    CodingToolConfig, SharedConfig, WorkspaceView, optional_string_field, string_field,
    tool_schema, truncate_text,
};

/// Keeps a Unix directory fd alive until a configured shell command has spawned.
pub struct BashCommandWorkdirGuard {
    #[cfg(unix)]
    _dir: std::fs::File,
}

/// Resolves the effective bash working directory inside the configured workspace.
pub fn resolve_bash_workdir(
    config: &CodingToolConfig,
    ctx: &ToolContext,
    workdir: Option<&str>,
) -> Result<std::path::PathBuf> {
    bash_workspace(config, ctx).resolve_base_dir(workdir)
}

/// Configures a shell command to start from a workspace directory without
/// reopening the directory path after validation on Unix.
pub fn configure_bash_command_workdir(
    config: &CodingToolConfig,
    ctx: &ToolContext,
    command: &mut Command,
    workdir: Option<&str>,
) -> Result<(std::path::PathBuf, BashCommandWorkdirGuard)> {
    let resolved = bash_workspace(config, ctx).resolve_base_dir_no_follow(workdir)?;
    configure_prepared_workdir(command, resolved)
}

/// Configures a shell command for an already-resolved workspace directory.
pub fn configure_resolved_bash_command_workdir(
    command: &mut Command,
    workspace_root: &std::path::Path,
    workdir: &std::path::Path,
) -> Result<BashCommandWorkdirGuard> {
    let workdir = workdir
        .to_str()
        .ok_or_else(|| anyhow!("bash workdir is not valid UTF-8: {}", workdir.display()))?;
    let resolved = WorkspaceView::from_root(workspace_root.to_path_buf())
        .resolve_base_dir_no_follow(Some(workdir))?;
    Ok(configure_prepared_workdir(command, resolved)?.1)
}

fn bash_workspace<'a>(config: &'a CodingToolConfig, ctx: &ToolContext) -> WorkspaceView<'a> {
    WorkspaceView::from_root(
        ctx.metadata
            .get("workspace_root")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| config.workspace_root.clone()),
    )
}

fn configure_prepared_workdir(
    command: &mut Command,
    resolved: crate::shared::ResolvedWorkspaceDir,
) -> Result<(std::path::PathBuf, BashCommandWorkdirGuard)> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        let fd = resolved.dir.as_raw_fd();
        unsafe {
            command.pre_exec(move || {
                if libc::fchdir(fd) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        Ok((
            resolved.path,
            BashCommandWorkdirGuard { _dir: resolved.dir },
        ))
    }

    #[cfg(not(unix))]
    {
        command.current_dir(&resolved.path);
        Ok((resolved.path, BashCommandWorkdirGuard {}))
    }
}

/// Executes one bash command in the foreground using the default coding-tool semantics.
pub async fn execute_bash_foreground(
    config: &CodingToolConfig,
    ctx: ToolContext,
    command: &str,
    workdir: Option<&str>,
) -> Result<ToolExecutionOutput> {
    let mut process = Command::new(&config.shell);
    process
        .arg("-lc")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let (workdir, _workdir_guard) =
        configure_bash_command_workdir(config, &ctx, &mut process, workdir)?;
    #[cfg(unix)]
    {
        process.process_group(0);
    }
    let mut child = process
        .spawn()
        .with_context(|| format!("failed to execute bash in {}", workdir.display()))?;
    let child_pid = child.id();
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("bash stdout pipe was not available"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("bash stderr pipe was not available"))?;
    let stdout_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        stdout.read_to_end(&mut buffer).await?;
        Ok::<Vec<u8>, std::io::Error>(buffer)
    });
    let stderr_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        stderr.read_to_end(&mut buffer).await?;
        Ok::<Vec<u8>, std::io::Error>(buffer)
    });
    let status = tokio::select! {
        status = child.wait() => {
            status.with_context(|| format!("failed to execute bash in {}", workdir.display()))?
        }
        _ = async {
            if let Some(cancellation) = current_cancellation_token() {
                cancellation.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => {
            shutdown_bash_process(child_pid, &mut child).await;
            return Err(interrupted_error());
        }
    };
    shutdown_bash_process_group(child_pid).await;
    let stdout = stdout_task
        .await
        .context("bash stdout reader task panicked")?
        .context("failed to collect bash stdout")?;
    let stderr = stderr_task
        .await
        .context("bash stderr reader task panicked")?
        .context("failed to collect bash stderr")?;
    let stdout = String::from_utf8_lossy(&stdout).to_string();
    let stderr = String::from_utf8_lossy(&stderr).to_string();
    Ok(ToolExecutionOutput::json(json!({
        "command": command,
        "workdir": workdir.display().to_string(),
        "exit_code": status.code(),
        "success": status.success(),
        "stdout": truncate_text(&stdout, 64 * 1024).0,
        "stderr": truncate_text(&stderr, 64 * 1024).0,
    })))
}

pub(crate) struct BashTool {
    shared: Arc<SharedConfig>,
}

impl BashTool {
    pub(crate) fn new(shared: Arc<SharedConfig>) -> Self {
        Self { shared }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "bash".to_string(),
            description: "Executes one shell command inside the workspace root.".to_string(),
            schema: tool_schema(vec![
                ToolSchemaField {
                    name: "command".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: true,
                    description: Some("Shell command to execute.".to_string()),
                },
                ToolSchemaField {
                    name: "workdir".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some(
                        "Optional working directory inside the workspace.".to_string(),
                    ),
                },
            ]),
            timeout_ms: 20_000,
            sandbox: SandboxProfile::WorkspaceWrite,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let command = string_field(&input, "command")?;
        execute_bash_foreground(
            &CodingToolConfig {
                workspace_root: self.shared.root.clone(),
                shell: self.shared.shell.clone(),
                user_agent: "kheish/0.1".to_string(),
                max_read_bytes: self.shared.max_read_bytes,
                max_results: self.shared.max_results,
                max_edit_bytes: self.shared.max_edit_bytes,
            },
            ctx,
            &command,
            optional_string_field(&input, "workdir").as_deref(),
        )
        .await
    }
}

async fn shutdown_bash_process(child_pid: Option<u32>, child: &mut tokio::process::Child) {
    shutdown_bash_process_group(child_pid).await;
    let _ = child.kill().await;
}

#[cfg(unix)]
async fn shutdown_bash_process_group(child_pid: Option<u32>) {
    let Some(child_pid) = child_pid else {
        return;
    };
    if !process_group_exists(child_pid) {
        return;
    }
    let _ = signal_process_group(child_pid, libc::SIGTERM);
    for _ in 0..10 {
        if !process_group_exists(child_pid) {
            return;
        }
        time::sleep(Duration::from_millis(50)).await;
    }
    let _ = signal_process_group(child_pid, libc::SIGKILL);
}

#[cfg(not(unix))]
async fn shutdown_bash_process_group(_child_pid: Option<u32>) {}

#[cfg(unix)]
fn process_group_exists(group_id: u32) -> bool {
    let result = unsafe { libc::kill(-(group_id as i32), 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn signal_process_group(group_id: u32, signal: i32) -> std::io::Result<()> {
    let result = unsafe { libc::kill(-(group_id as i32), signal) };
    if result == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}
