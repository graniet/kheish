//! Helpers for daemon-managed shell tasks.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow};
use kheish_session::atomic_write;
use kheish_types::{ReplyHandle, TaskRecord, TaskStatus};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Stable task metadata kind used for daemon-managed shell tasks.
pub const BACKGROUND_SHELL_TASK_KIND: &str = "background_shell";
/// Stable prefix for daemon-created shell task identifiers.
pub const BACKGROUND_SHELL_TASK_ID_PREFIX: &str = "shell-task-";
/// Environment marker inherited by daemon-owned shell-task processes.
pub const BACKGROUND_SHELL_TASK_ID_ENV: &str = "KHEISH_SHELL_TASK_ID";
/// Default tail size returned by operator and tool output views.
pub const DEFAULT_TASK_OUTPUT_TAIL_BYTES: usize = 8 * 1024;
/// Maximum tail size accepted by task-output API/tool calls.
pub const MAX_TASK_OUTPUT_TAIL_BYTES: usize = 256 * 1024;
/// Maximum allowed on-disk output size for one background shell task.
pub const MAX_BACKGROUND_SHELL_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
/// Number of bytes retained after rotating one oversized shell-task output file.
pub const BACKGROUND_SHELL_OUTPUT_ROTATE_KEEP_BYTES: u64 = 6 * 1024 * 1024;
/// How often the daemon inspects one running background shell task.
pub const BACKGROUND_SHELL_WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);
/// How long output may stay flat before checking for interactive prompts.
pub const BACKGROUND_SHELL_STALL_THRESHOLD: Duration = Duration::from_secs(45);
/// Number of bytes read from the output tail for prompt detection.
pub const BACKGROUND_SHELL_STALL_TAIL_BYTES: usize = 1024;
const OUTPUT_ROTATION_MARKER: &str = "[daemon output rotated:";

/// One request to launch one daemon-managed shell task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundShellTaskRequest {
    /// The shell command to execute.
    pub command: String,
    /// The shell binary used to execute the command.
    pub shell: String,
    /// The resolved working directory.
    pub workdir: PathBuf,
    /// The operator-facing description of the task.
    pub description: String,
    /// The originating tool call identifier.
    pub tool_call_id: String,
    /// The daemon run that created this task, when launched from a run-scoped tool.
    pub created_by_run_id: Option<String>,
    /// Whether the task starts detached from the invoking run.
    pub started_in_background: bool,
    /// External reply targets captured when the task was created.
    pub reply_targets: Vec<ReplyHandle>,
}

/// One persisted metadata payload for a background shell task.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundShellTaskMetadata {
    /// Stable metadata kind identifier.
    pub kind: String,
    /// The shell command executed by the task.
    pub command: String,
    /// The resolved working directory.
    pub workdir: String,
    /// The output file written by the task.
    pub output_file_path: String,
    /// The originating tool call identifier.
    pub tool_call_id: String,
    /// The daemon run that created this task, when launched from a run-scoped tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_run_id: Option<String>,
    /// Whether the task started detached or was detached later.
    #[serde(default)]
    pub started_in_background: bool,
    /// External reply targets captured when the task was created.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reply_targets: Vec<ReplyHandle>,
    /// The child process identifier when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The owning process group identifier when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_group_id: Option<u32>,
    /// Observed process start timestamp from the OS process table when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_started_at: Option<String>,
    /// Daemon-owned identity token expected in inherited child process environments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_token: Option<String>,
    /// The terminal exit code when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Machine-readable terminal reason when the daemon settles the task without a process exit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    /// Whether this terminal state was recovered during daemon boot.
    #[serde(default)]
    pub recovered_on_boot: bool,
    /// The latest output size on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_size_bytes: Option<u64>,
    /// The total number of bytes written by the task before retention rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_total_bytes: Option<u64>,
    /// Whether the task output exceeded the retention window and was rotated.
    #[serde(default)]
    pub output_rotated: bool,
    /// Number of output retention rotations applied.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub output_rotation_count: u64,
    /// Whether the task was cancelled by the daemon or operator.
    #[serde(default)]
    pub cancelled: bool,
    /// Whether the output watchdog terminated the task.
    #[serde(default)]
    pub killed_for_size: bool,
    /// Whether the interactive-prompt stall notification already fired.
    #[serde(default)]
    pub interactive_prompt_detected: bool,
    /// Whether an operator or daemon stop was requested.
    #[serde(default)]
    pub stop_requested: bool,
    /// Stop request timestamp when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_requested_at_ms: Option<u64>,
    /// Human-facing stop reason when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Whether the latest process-tree shutdown attempt was confirmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_tree_shutdown_confirmed: Option<bool>,
    /// Whether the latest process-tree shutdown attempt matched the expected target identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_tree_shutdown_matched_target: Option<bool>,
    /// Whether at least one signal was sent during the latest shutdown attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_tree_shutdown_signal_sent: Option<bool>,
    /// Whether the latest process-tree shutdown attempt timed out with survivors.
    #[serde(default)]
    pub process_tree_shutdown_timed_out: bool,
    /// Whether process-tree shutdown is implemented on the current platform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_tree_shutdown_supported: Option<bool>,
    /// Process ids still visible after the latest process-tree shutdown attempt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub process_tree_remaining_pids: Vec<u32>,
    /// Machine-readable terminal reason for output capture failures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_capture_error: Option<String>,
}

impl BackgroundShellTaskMetadata {
    /// Builds one metadata payload for a fresh running task.
    pub fn new(request: &BackgroundShellTaskRequest, output_file_path: impl Into<String>) -> Self {
        Self {
            kind: BACKGROUND_SHELL_TASK_KIND.to_string(),
            command: request.command.clone(),
            workdir: request.workdir.display().to_string(),
            output_file_path: output_file_path.into(),
            tool_call_id: request.tool_call_id.clone(),
            created_by_run_id: request.created_by_run_id.clone(),
            started_in_background: request.started_in_background,
            reply_targets: request.reply_targets.clone(),
            pid: None,
            process_group_id: None,
            process_started_at: None,
            identity_token: None,
            exit_code: None,
            terminal_reason: None,
            recovered_on_boot: false,
            output_size_bytes: None,
            output_total_bytes: None,
            output_rotated: false,
            output_rotation_count: 0,
            cancelled: false,
            killed_for_size: false,
            interactive_prompt_detected: false,
            stop_requested: false,
            stop_requested_at_ms: None,
            stop_reason: None,
            process_tree_shutdown_confirmed: None,
            process_tree_shutdown_matched_target: None,
            process_tree_shutdown_signal_sent: None,
            process_tree_shutdown_timed_out: false,
            process_tree_shutdown_supported: None,
            process_tree_remaining_pids: Vec::new(),
            output_capture_error: None,
        }
    }

    /// Returns the output file path as a filesystem path.
    pub fn output_path(&self) -> PathBuf {
        PathBuf::from(&self.output_file_path)
    }
}

/// Identity checks used before signalling persisted shell-task processes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackgroundShellShutdownGuard<'a> {
    /// Expected command line fragment for the shell task.
    pub expected_command: Option<&'a str>,
    /// Expected process start timestamp for the original child pid.
    pub expected_process_started_at: Option<&'a str>,
    /// Expected daemon shell-task identity token inherited through the environment.
    pub expected_task_id: Option<&'a str>,
}

/// Result of one process-tree shutdown attempt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundShellShutdownOutcome {
    /// Whether process-tree shutdown is implemented on the current platform.
    pub supported: bool,
    /// Whether the persisted target matched the expected identity guard.
    pub matched_target: bool,
    /// Whether at least one signal/kill request was attempted.
    pub signal_sent: bool,
    /// Whether no tracked process remained at the end of the attempt.
    pub confirmed: bool,
    /// Whether the attempt reached its bounded escalation window with survivors.
    pub timed_out: bool,
    /// Remaining visible process ids after the attempt.
    pub remaining_pids: Vec<u32>,
}

/// One operator-facing view over task output retrieval.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskOutputView {
    /// The retrieval state for the request.
    pub retrieval_status: String,
    /// The current task snapshot.
    pub task: TaskRecord,
    /// The output file path when the task owns one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_file_path: Option<String>,
    /// The latest output excerpt when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_excerpt: Option<String>,
    /// The full output text when the caller requested it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_text: Option<String>,
    /// Whether the excerpt was truncated.
    #[serde(default)]
    pub output_truncated: bool,
    /// The total output size on disk when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_size_bytes: Option<u64>,
    /// Total bytes written by the task before retention rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_total_bytes: Option<u64>,
    /// Whether the output file is a retained window rather than the complete stream.
    #[serde(default)]
    pub output_rotated: bool,
    /// Number of retention rotations applied to the task output.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub output_rotation_count: u64,
}

/// One lightweight snapshot of output progress used by the watchdog.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskOutputProgress {
    /// The current output size in bytes.
    pub size_bytes: u64,
    /// The latest output tail excerpt.
    pub tail_excerpt: String,
}

/// One snapshot of a shell-task output retention writer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellTaskOutputStats {
    /// Bytes retained in the output file currently on disk.
    pub retained_size_bytes: u64,
    /// Total bytes accepted from stdout/stderr before retention rotation.
    pub total_size_bytes: u64,
    /// Whether the output was rotated at least once.
    pub rotated: bool,
    /// Number of retention rotations applied.
    pub rotation_count: u64,
}

/// Reconstructs retained output stats from the on-disk file after daemon recovery.
pub fn recover_shell_task_output_stats(path: &Path) -> Option<ShellTaskOutputStats> {
    let metadata = std::fs::metadata(path).ok()?;
    let retained_size_bytes = metadata.len();
    let marker = read_output_rotation_marker(path)
        .and_then(|marker| parse_output_rotation_marker(&marker))
        .unwrap_or_default();
    let marker_total_bytes = marker.total_size_bytes(retained_size_bytes);
    let marker_rotation_count = marker.rotation_count;
    let rotated =
        marker_rotation_count > 0 || retained_size_bytes > MAX_BACKGROUND_SHELL_OUTPUT_BYTES;
    Some(ShellTaskOutputStats {
        retained_size_bytes,
        total_size_bytes: marker_total_bytes.max(retained_size_bytes),
        rotated,
        rotation_count: marker_rotation_count,
    })
}

/// Bounded writer for daemon-managed shell-task output.
#[derive(Debug)]
pub struct ShellTaskOutputWriter {
    path: PathBuf,
    total_size_bytes: u64,
    rotation_count: u64,
    max_retained_bytes: u64,
    rotate_keep_bytes: u64,
}

impl ShellTaskOutputWriter {
    /// Creates one writer with the production retention policy.
    pub fn new(path: PathBuf) -> Self {
        Self::new_with_limits(
            path,
            MAX_BACKGROUND_SHELL_OUTPUT_BYTES,
            BACKGROUND_SHELL_OUTPUT_ROTATE_KEEP_BYTES,
        )
    }

    fn new_with_limits(path: PathBuf, max_retained_bytes: u64, rotate_keep_bytes: u64) -> Self {
        Self {
            path,
            total_size_bytes: 0,
            rotation_count: 0,
            max_retained_bytes,
            rotate_keep_bytes,
        }
    }

    /// Appends one stdout/stderr chunk and rotates the retained file when needed.
    pub async fn append(&mut self, chunk: &[u8]) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        file.write_all(chunk).await?;
        file.flush().await?;
        drop(file);

        self.total_size_bytes = self.total_size_bytes.saturating_add(chunk.len() as u64);
        let retained_size = tokio::fs::metadata(&self.path).await?.len();
        if retained_size > self.max_retained_bytes {
            self.rotate_retained_output().await?;
        }
        Ok(())
    }

    /// Returns the latest retention statistics.
    pub async fn stats(&self) -> Result<ShellTaskOutputStats> {
        let retained_size_bytes = tokio::fs::metadata(&self.path)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        Ok(ShellTaskOutputStats {
            retained_size_bytes,
            total_size_bytes: self.total_size_bytes,
            rotated: self.rotation_count > 0,
            rotation_count: self.rotation_count,
        })
    }

    async fn rotate_retained_output(&mut self) -> Result<()> {
        self.rotation_count = self.rotation_count.saturating_add(1);
        let tail = read_task_output_tail_bytes(&self.path, self.rotate_keep_bytes as usize).await?;
        let mut marker = format!(
            "\n{OUTPUT_ROTATION_MARKER} retained latest {} bytes after {} total bytes; rotation={}]\n",
            tail.len(),
            self.total_size_bytes,
            self.rotation_count
        );
        let mut marker_len = marker.as_bytes().len() as u64;
        let keep_budget = self
            .max_retained_bytes
            .saturating_sub(marker_len)
            .min(self.rotate_keep_bytes);
        let tail = if tail.len() as u64 > keep_budget {
            let clipped = tail[tail.len().saturating_sub(keep_budget as usize)..].to_vec();
            marker = format!(
                "\n{OUTPUT_ROTATION_MARKER} retained latest {} bytes after {} total bytes; rotation={}]\n",
                clipped.len(),
                self.total_size_bytes,
                self.rotation_count
            );
            marker_len = marker.as_bytes().len() as u64;
            if marker_len.saturating_add(clipped.len() as u64) > self.max_retained_bytes {
                Vec::new()
            } else {
                clipped
            }
        } else {
            tail
        };
        let marker_bytes = marker.as_bytes();
        let mut retained = Vec::with_capacity(marker_bytes.len() + tail.len());
        retained.extend_from_slice(marker_bytes);
        retained.extend_from_slice(&tail);
        write_rotated_output_atomically(self.path.clone(), retained).await
    }
}

/// Returns the shell-task metadata when one task belongs to the background shell subsystem.
pub fn background_shell_metadata(task: &TaskRecord) -> Option<BackgroundShellTaskMetadata> {
    if !task.id.starts_with(BACKGROUND_SHELL_TASK_ID_PREFIX) {
        return None;
    }
    serde_json::from_value::<BackgroundShellTaskMetadata>(task.metadata.clone())
        .ok()
        .filter(|metadata| metadata.kind == BACKGROUND_SHELL_TASK_KIND)
}

/// Reads the latest output excerpt for one task when available.
pub async fn read_task_output_excerpt(
    path: &Path,
    tail_bytes: usize,
) -> Result<(String, bool, u64)> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = File::open(&path)?;
        let size = file.metadata()?.len();
        let read_len = tail_bytes.min(size as usize);
        let start = size.saturating_sub(read_len as u64);
        file.seek(SeekFrom::Start(start))?;
        let mut buffer = vec![0; read_len];
        file.read_exact(&mut buffer)?;
        let excerpt = String::from_utf8_lossy(&buffer).to_string();
        let rotation_truncated = excerpt.contains(OUTPUT_ROTATION_MARKER);
        Ok::<_, anyhow::Error>((excerpt, size > read_len as u64 || rotation_truncated, size))
    })
    .await
    .map_err(|error| anyhow!("task output reader panicked: {error}"))?
}

async fn read_task_output_tail_bytes(path: &Path, tail_bytes: usize) -> Result<Vec<u8>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = File::open(&path)?;
        let size = file.metadata()?.len();
        let read_len = tail_bytes.min(size as usize);
        let start = size.saturating_sub(read_len as u64);
        file.seek(SeekFrom::Start(start))?;
        let mut buffer = vec![0; read_len];
        file.read_exact(&mut buffer)?;
        Ok::<_, anyhow::Error>(buffer)
    })
    .await
    .map_err(|error| anyhow!("task output tail reader panicked: {error}"))?
}

/// Reads one small progress snapshot for the watchdog loop.
pub async fn read_task_output_progress(path: &Path) -> Result<TaskOutputProgress> {
    let (tail_excerpt, _, size_bytes) =
        read_task_output_excerpt(path, BACKGROUND_SHELL_STALL_TAIL_BYTES).await?;
    Ok(TaskOutputProgress {
        size_bytes,
        tail_excerpt,
    })
}

/// Reads the full output body for one task.
pub async fn read_task_output_text(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = File::open(&path)?;
        let size = file.metadata()?.len();
        let mut prefixed = None;
        if size > MAX_BACKGROUND_SHELL_OUTPUT_BYTES {
            let marker = format!(
                "\n{OUTPUT_ROTATION_MARKER} retained latest {} bytes from oversized legacy output]\n",
                MAX_BACKGROUND_SHELL_OUTPUT_BYTES
            );
            file.seek(SeekFrom::Start(
                size.saturating_sub(MAX_BACKGROUND_SHELL_OUTPUT_BYTES),
            ))?;
            prefixed = Some(marker);
        }
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;
        let body = String::from_utf8_lossy(&buffer).to_string();
        Ok::<_, anyhow::Error>(match prefixed {
            Some(marker) => format!("{marker}{body}"),
            None => body,
        })
    })
    .await
    .map_err(|error| anyhow!("task output reader panicked: {error}"))?
}

/// Builds one output view from the current task state and optional shell metadata.
pub async fn build_task_output_view(
    retrieval_status: impl Into<String>,
    task: TaskRecord,
    tail_bytes: usize,
    include_full_output: bool,
) -> TaskOutputView {
    let mut view = TaskOutputView {
        retrieval_status: retrieval_status.into(),
        task: task.clone(),
        output_file_path: None,
        output_excerpt: None,
        output_text: None,
        output_truncated: false,
        output_size_bytes: None,
        output_total_bytes: None,
        output_rotated: false,
        output_rotation_count: 0,
    };
    if let Some(metadata) = background_shell_metadata(&task) {
        view.output_file_path = Some(metadata.output_file_path.clone());
        let output_path = metadata.output_path();
        let recovered_stats = recover_shell_task_output_stats(&output_path);
        let persisted_total_bytes = metadata.output_total_bytes.or(metadata.output_size_bytes);
        let recovered_total_bytes = recovered_stats.as_ref().map(|stats| stats.total_size_bytes);
        view.output_total_bytes = match (persisted_total_bytes, recovered_total_bytes) {
            (Some(persisted), Some(recovered)) => Some(persisted.max(recovered)),
            (Some(persisted), None) => Some(persisted),
            (None, Some(recovered)) => Some(recovered),
            (None, None) => None,
        };
        view.output_rotated = metadata.output_rotated
            || recovered_stats
                .as_ref()
                .map(|stats| stats.rotated)
                .unwrap_or(false);
        view.output_rotation_count = metadata.output_rotation_count.max(
            recovered_stats
                .as_ref()
                .map(|stats| stats.rotation_count)
                .unwrap_or(0),
        );
        if output_path.exists() {
            if let Ok((excerpt, truncated, size)) =
                read_task_output_excerpt(&output_path, tail_bytes).await
            {
                view.output_excerpt = Some(excerpt);
                view.output_truncated = truncated || view.output_rotated;
                view.output_size_bytes = Some(size);
            }
            if include_full_output {
                if let Ok(output_text) = read_task_output_text(&output_path).await {
                    view.output_text = Some(output_text);
                }
            }
        } else {
            view.output_size_bytes = metadata.output_size_bytes;
        }
    }
    view
}

async fn write_rotated_output_atomically(path: PathBuf, retained: Vec<u8>) -> Result<()> {
    tokio::task::spawn_blocking(move || atomic_write(&path, &retained))
        .await
        .map_err(|error| anyhow!("task output rotation writer panicked: {error}"))?
}

fn read_output_rotation_marker(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut buffer = vec![0; 512];
    let read = file.read(&mut buffer).ok()?;
    if read == 0 {
        return None;
    }
    let head = String::from_utf8_lossy(&buffer[..read]);
    let start = head.find(OUTPUT_ROTATION_MARKER)?;
    let rest = &head[start..];
    let end = rest.find(']')?;
    Some(rest[..=end].to_string())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct OutputRotationMarker {
    total_bytes_at_rotation: u64,
    rotation_count: u64,
    retained_bytes_at_rotation: Option<u64>,
    marker_envelope_bytes: u64,
}

impl OutputRotationMarker {
    fn total_size_bytes(self, retained_size_bytes: u64) -> u64 {
        let appended_after_rotation = self
            .retained_bytes_at_rotation
            .map(|retained| {
                retained_size_bytes
                    .saturating_sub(self.marker_envelope_bytes.saturating_add(retained))
            })
            .unwrap_or(0);
        self.total_bytes_at_rotation
            .saturating_add(appended_after_rotation)
            .max(retained_size_bytes)
    }
}

fn parse_output_rotation_marker(marker: &str) -> Option<OutputRotationMarker> {
    let marker_envelope_bytes = marker.len().saturating_add(2) as u64;
    let (_, after_prefix) = marker.split_once("retained latest ")?;
    if let Some((retained, after_retained)) = after_prefix.split_once(" bytes after ") {
        let (total, after_total) = after_retained.split_once(" total bytes; rotation=")?;
        let rotation = after_total.trim_end_matches(']');
        return Some(OutputRotationMarker {
            total_bytes_at_rotation: total.parse().ok()?,
            rotation_count: rotation.parse().ok()?,
            retained_bytes_at_rotation: Some(retained.parse().ok()?),
            marker_envelope_bytes,
        });
    }
    let (_, after_legacy) = marker.split_once("output after ")?;
    let (total, after_total) = after_legacy.split_once(" total bytes; rotation=")?;
    let rotation = after_total.trim_end_matches(']');
    Some(OutputRotationMarker {
        total_bytes_at_rotation: total.parse().ok()?,
        rotation_count: rotation.parse().ok()?,
        retained_bytes_at_rotation: None,
        marker_envelope_bytes,
    })
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// Returns true when the output tail looks like an interactive prompt.
pub fn looks_like_interactive_prompt(tail: &str) -> bool {
    let last_line = tail.trim_end().lines().last().unwrap_or_default();
    [
        "(y/n)",
        "[y/n]",
        "(yes/no)",
        "Press Enter",
        "Press any key",
        "Continue?",
        "Overwrite?",
    ]
    .iter()
    .any(|pattern| {
        last_line
            .to_ascii_lowercase()
            .contains(&pattern.to_ascii_lowercase())
    }) || (last_line.ends_with('?')
        && ["do you", "would you", "shall i", "are you sure", "ready to"]
            .iter()
            .any(|pattern| last_line.to_ascii_lowercase().contains(pattern)))
}

/// Configures one shell command so its descendants run in a dedicated process group.
pub fn configure_background_shell_command(command: &mut Command) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }
}

/// Returns the dedicated process-group id for one spawned shell task when available.
pub fn background_shell_process_group_id(child_pid: u32) -> Option<u32> {
    #[cfg(unix)]
    {
        Some(child_pid)
    }
    #[cfg(not(unix))]
    {
        let _ = child_pid;
        None
    }
}

/// Returns the OS-reported process start timestamp for stale pid reuse checks.
#[cfg(unix)]
pub fn background_shell_process_started_at(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let started_at = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if started_at.is_empty() {
        None
    } else {
        Some(started_at)
    }
}

/// Returns the OS-reported process start timestamp for stale pid reuse checks.
#[cfg(not(unix))]
pub fn background_shell_process_started_at(_pid: u32) -> Option<String> {
    None
}

/// Terminates the dedicated process group or falls back to the child pid when needed.
pub async fn shutdown_background_shell_processes(
    process_group_id: Option<u32>,
    pid: Option<u32>,
    guard: BackgroundShellShutdownGuard<'_>,
) -> BackgroundShellShutdownOutcome {
    #[cfg(unix)]
    {
        let expected_task_id = guard.expected_task_id;
        if let Some(group_id) = process_group_id {
            if !background_shell_process_group_target_matches(group_id, guard) {
                let identity_pids = background_shell_identity_pids(expected_task_id);
                if identity_pids.is_empty() {
                    return BackgroundShellShutdownOutcome {
                        supported: true,
                        matched_target: false,
                        confirmed: false,
                        ..Default::default()
                    };
                }
                return shutdown_background_shell_tracked_processes(
                    identity_pids,
                    expected_task_id,
                )
                .await;
            }
            let mut signal_sent = false;
            let mut descendants = background_shell_process_group_descendant_pids(group_id);
            merge_process_ids(
                &mut descendants,
                background_shell_identity_pids(expected_task_id),
            );
            for _ in 0..5 {
                if !background_shell_process_group_exists(group_id) {
                    merge_process_ids(
                        &mut descendants,
                        background_shell_identity_pids(expected_task_id),
                    );
                    signal_sent |= signal_background_shell_processes(&descendants, libc::SIGTERM);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    merge_process_ids(
                        &mut descendants,
                        background_shell_process_group_descendant_pids(group_id),
                    );
                    merge_process_ids(
                        &mut descendants,
                        background_shell_identity_pids(expected_task_id),
                    );
                    signal_sent |= signal_background_shell_processes(&descendants, libc::SIGKILL);
                    let remaining_pids = wait_for_background_shell_remaining_pids(
                        process_group_id,
                        pid,
                        &descendants,
                        expected_task_id,
                    )
                    .await;
                    return BackgroundShellShutdownOutcome {
                        supported: true,
                        matched_target: true,
                        signal_sent,
                        confirmed: remaining_pids.is_empty(),
                        timed_out: false,
                        remaining_pids,
                    };
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            merge_process_ids(
                &mut descendants,
                background_shell_process_group_descendant_pids(group_id),
            );
            merge_process_ids(
                &mut descendants,
                background_shell_identity_pids(expected_task_id),
            );
            signal_sent |= signal_background_shell_processes(&descendants, libc::SIGTERM);
            signal_sent |= signal_background_shell_process_group(group_id, libc::SIGTERM).is_ok();
            for _ in 0..10 {
                if !background_shell_process_group_exists(group_id) {
                    merge_process_ids(
                        &mut descendants,
                        background_shell_process_group_descendant_pids(group_id),
                    );
                    merge_process_ids(
                        &mut descendants,
                        background_shell_identity_pids(expected_task_id),
                    );
                    signal_sent |= signal_background_shell_processes(&descendants, libc::SIGKILL);
                    let remaining_pids = wait_for_background_shell_remaining_pids(
                        process_group_id,
                        pid,
                        &descendants,
                        expected_task_id,
                    )
                    .await;
                    return BackgroundShellShutdownOutcome {
                        supported: true,
                        matched_target: true,
                        signal_sent,
                        confirmed: remaining_pids.is_empty(),
                        timed_out: false,
                        remaining_pids,
                    };
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            merge_process_ids(
                &mut descendants,
                background_shell_process_group_descendant_pids(group_id),
            );
            merge_process_ids(
                &mut descendants,
                background_shell_identity_pids(expected_task_id),
            );
            signal_sent |= signal_background_shell_processes(&descendants, libc::SIGKILL);
            signal_sent |= signal_background_shell_process_group(group_id, libc::SIGKILL).is_ok();
            let remaining_pids = wait_for_background_shell_remaining_pids(
                process_group_id,
                pid,
                &descendants,
                expected_task_id,
            )
            .await;
            return BackgroundShellShutdownOutcome {
                supported: true,
                matched_target: true,
                signal_sent,
                confirmed: remaining_pids.is_empty(),
                timed_out: !remaining_pids.is_empty(),
                remaining_pids,
            };
        }
        let Some(pid) = pid else {
            let identity_pids = background_shell_identity_pids(expected_task_id);
            if !identity_pids.is_empty() {
                return shutdown_background_shell_tracked_processes(
                    identity_pids,
                    expected_task_id,
                )
                .await;
            }
            return BackgroundShellShutdownOutcome {
                supported: true,
                matched_target: false,
                confirmed: true,
                ..Default::default()
            };
        };
        if !background_shell_persisted_target_matches(pid, None, guard) {
            let identity_pids = background_shell_identity_pids(expected_task_id);
            if !identity_pids.is_empty() {
                return shutdown_background_shell_tracked_processes(
                    identity_pids,
                    expected_task_id,
                )
                .await;
            }
            return BackgroundShellShutdownOutcome {
                supported: true,
                matched_target: false,
                confirmed: false,
                ..Default::default()
            };
        }
        let mut signal_sent = false;
        let mut descendants = background_shell_descendant_pids(pid);
        merge_process_ids(
            &mut descendants,
            background_shell_identity_pids(expected_task_id),
        );
        for _ in 0..5 {
            if !background_shell_process_exists(pid) {
                merge_process_ids(
                    &mut descendants,
                    background_shell_identity_pids(expected_task_id),
                );
                signal_sent |= signal_background_shell_processes(&descendants, libc::SIGTERM);
                tokio::time::sleep(Duration::from_millis(50)).await;
                merge_process_ids(&mut descendants, background_shell_descendant_pids(pid));
                merge_process_ids(
                    &mut descendants,
                    background_shell_identity_pids(expected_task_id),
                );
                signal_sent |= signal_background_shell_processes(&descendants, libc::SIGKILL);
                let remaining_pids = wait_for_background_shell_remaining_pids(
                    process_group_id,
                    Some(pid),
                    &descendants,
                    expected_task_id,
                )
                .await;
                return BackgroundShellShutdownOutcome {
                    supported: true,
                    matched_target: true,
                    signal_sent,
                    confirmed: remaining_pids.is_empty(),
                    timed_out: false,
                    remaining_pids,
                };
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        merge_process_ids(&mut descendants, background_shell_descendant_pids(pid));
        merge_process_ids(
            &mut descendants,
            background_shell_identity_pids(expected_task_id),
        );
        signal_sent |= signal_background_shell_processes(&descendants, libc::SIGTERM);
        signal_sent |= signal_background_shell_process(pid, libc::SIGTERM).is_ok();
        for _ in 0..10 {
            if !background_shell_process_exists(pid) {
                merge_process_ids(&mut descendants, background_shell_descendant_pids(pid));
                merge_process_ids(
                    &mut descendants,
                    background_shell_identity_pids(expected_task_id),
                );
                signal_sent |= signal_background_shell_processes(&descendants, libc::SIGKILL);
                let remaining_pids = wait_for_background_shell_remaining_pids(
                    process_group_id,
                    Some(pid),
                    &descendants,
                    expected_task_id,
                )
                .await;
                return BackgroundShellShutdownOutcome {
                    supported: true,
                    matched_target: true,
                    signal_sent,
                    confirmed: remaining_pids.is_empty(),
                    timed_out: false,
                    remaining_pids,
                };
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        merge_process_ids(&mut descendants, background_shell_descendant_pids(pid));
        merge_process_ids(
            &mut descendants,
            background_shell_identity_pids(expected_task_id),
        );
        signal_sent |= signal_background_shell_processes(&descendants, libc::SIGKILL);
        signal_sent |= signal_background_shell_process(pid, libc::SIGKILL).is_ok();
        let remaining_pids = wait_for_background_shell_remaining_pids(
            process_group_id,
            Some(pid),
            &descendants,
            expected_task_id,
        )
        .await;
        BackgroundShellShutdownOutcome {
            supported: true,
            matched_target: true,
            signal_sent,
            confirmed: remaining_pids.is_empty(),
            timed_out: !remaining_pids.is_empty(),
            remaining_pids,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = process_group_id;
        let _ = pid;
        let _ = guard;
        BackgroundShellShutdownOutcome {
            supported: false,
            matched_target: false,
            signal_sent: false,
            confirmed: false,
            timed_out: false,
            remaining_pids: Vec::new(),
        }
    }
}

/// Returns whether a shell-task shutdown target is still visible to the daemon.
pub fn background_shell_shutdown_targets_visible(
    process_group_id: Option<u32>,
    pid: Option<u32>,
    expected_task_id: Option<&str>,
) -> bool {
    #[cfg(unix)]
    {
        if expected_task_id
            .is_some_and(|task_id| !background_shell_identity_pids(Some(task_id)).is_empty())
        {
            return true;
        }
        if process_group_id.is_some_and(background_shell_process_group_exists) {
            return true;
        }
        pid.is_some_and(background_shell_process_exists)
    }
    #[cfg(not(unix))]
    {
        let _ = process_group_id;
        let _ = pid;
        let _ = expected_task_id;
        false
    }
}

#[cfg(unix)]
async fn shutdown_background_shell_tracked_processes(
    mut tracked_pids: Vec<u32>,
    expected_task_id: Option<&str>,
) -> BackgroundShellShutdownOutcome {
    merge_process_ids(
        &mut tracked_pids,
        background_shell_identity_pids(expected_task_id),
    );
    tracked_pids = background_shell_tracked_process_tree_pids(&tracked_pids);
    if tracked_pids.is_empty() {
        return BackgroundShellShutdownOutcome {
            supported: true,
            matched_target: false,
            confirmed: true,
            ..Default::default()
        };
    }
    let mut signal_sent = signal_background_shell_processes(&tracked_pids, libc::SIGTERM);
    let mut remaining_pids =
        wait_for_background_shell_remaining_pids(None, None, &tracked_pids, expected_task_id).await;
    if remaining_pids.is_empty() {
        return BackgroundShellShutdownOutcome {
            supported: true,
            matched_target: true,
            signal_sent,
            confirmed: true,
            timed_out: false,
            remaining_pids,
        };
    }
    merge_process_ids(
        &mut tracked_pids,
        background_shell_identity_pids(expected_task_id),
    );
    tracked_pids = background_shell_tracked_process_tree_pids(&tracked_pids);
    signal_sent |= signal_background_shell_processes(&tracked_pids, libc::SIGKILL);
    remaining_pids =
        wait_for_background_shell_remaining_pids(None, None, &tracked_pids, expected_task_id).await;
    BackgroundShellShutdownOutcome {
        supported: true,
        matched_target: true,
        signal_sent,
        confirmed: remaining_pids.is_empty(),
        timed_out: !remaining_pids.is_empty(),
        remaining_pids,
    }
}

#[cfg(unix)]
fn background_shell_tracked_process_tree_pids(tracked_pids: &[u32]) -> Vec<u32> {
    let mut pids = Vec::new();
    for pid in tracked_pids {
        merge_process_ids(&mut pids, vec![*pid]);
        merge_process_ids(&mut pids, background_shell_descendant_pids(*pid));
    }
    pids
}

#[cfg(unix)]
fn background_shell_process_group_target_matches(
    group_id: u32,
    guard: BackgroundShellShutdownGuard<'_>,
) -> bool {
    if background_shell_process_exists(group_id) {
        let identity_guard = BackgroundShellShutdownGuard {
            expected_command: None,
            ..guard
        };
        if !background_shell_persisted_target_matches(group_id, Some(group_id), identity_guard) {
            return false;
        }
        let env_identity_verified = guard.expected_task_id.is_some_and(|expected_task_id| {
            background_shell_process_has_task_identity(group_id, expected_task_id) == Some(true)
        });
        if guard.expected_process_started_at.is_some() || env_identity_verified {
            return true;
        }
    }
    let Some(expected_command) = guard.expected_command else {
        return true;
    };
    let members = background_shell_process_group_members(group_id);
    if members.is_empty() {
        return background_shell_persisted_target_matches(group_id, Some(group_id), guard);
    }
    members
        .iter()
        .any(|member| background_shell_command_matches(&member.command, expected_command))
        || members.iter().any(|member| {
            guard.expected_task_id.is_some_and(|expected_task_id| {
                background_shell_process_has_task_identity(member.pid, expected_task_id)
                    == Some(true)
            })
        })
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct BackgroundShellProcessSnapshot {
    pid: u32,
    parent_pid: u32,
    group_id: u32,
    command: String,
}

#[cfg(unix)]
fn background_shell_process_group_members(group_id: u32) -> Vec<BackgroundShellProcessSnapshot> {
    background_shell_process_snapshots()
        .into_iter()
        .filter(|snapshot| snapshot.group_id == group_id)
        .collect()
}

#[cfg(unix)]
fn background_shell_process_group_descendant_pids(group_id: u32) -> Vec<u32> {
    let mut roots = BTreeSet::new();
    roots.insert(group_id);
    for member in background_shell_process_group_members(group_id) {
        roots.insert(member.pid);
    }
    background_shell_descendant_pids_from_roots(roots)
}

#[cfg(unix)]
fn background_shell_process_snapshots() -> Vec<BackgroundShellProcessSnapshot> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,command="])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_background_shell_process_snapshot)
        .collect()
}

#[cfg(all(unix, test))]
pub(crate) fn background_shell_process_group_member_pids_for_test(group_id: u32) -> Vec<u32> {
    background_shell_process_group_members(group_id)
        .into_iter()
        .map(|snapshot| snapshot.pid)
        .filter(|pid| !background_shell_process_is_zombie(*pid))
        .collect()
}

#[cfg(unix)]
fn parse_background_shell_process_snapshot(line: &str) -> Option<BackgroundShellProcessSnapshot> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let parent_pid = parts.next()?.parse::<u32>().ok()?;
    let group_id = parts.next()?.parse::<u32>().ok()?;
    let command = parts.collect::<Vec<_>>().join(" ");
    Some(BackgroundShellProcessSnapshot {
        pid,
        parent_pid,
        group_id,
        command,
    })
}

#[cfg(unix)]
fn background_shell_descendant_pids(root_pid: u32) -> Vec<u32> {
    let mut roots = BTreeSet::new();
    roots.insert(root_pid);
    background_shell_descendant_pids_from_roots(roots)
}

#[cfg(unix)]
fn background_shell_descendant_pids_from_roots(mut frontier: BTreeSet<u32>) -> Vec<u32> {
    let snapshots = background_shell_process_snapshots();
    let mut seen = BTreeSet::new();
    while !frontier.is_empty() {
        let mut next = BTreeSet::new();
        for snapshot in &snapshots {
            if frontier.contains(&snapshot.parent_pid) && seen.insert(snapshot.pid) {
                next.insert(snapshot.pid);
            }
        }
        frontier = next;
    }
    seen.into_iter().collect()
}

#[cfg(unix)]
fn background_shell_process_group_exists(group_id: u32) -> bool {
    let result = unsafe { libc::kill(-(group_id as i32), 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn signal_background_shell_process_group(group_id: u32, signal: i32) -> std::io::Result<()> {
    let result = unsafe { libc::kill(-(group_id as i32), signal) };
    if result == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}

#[cfg(unix)]
fn background_shell_persisted_target_matches(
    pid: u32,
    expected_group_id: Option<u32>,
    guard: BackgroundShellShutdownGuard<'_>,
) -> bool {
    let needs_validation = expected_group_id.is_some()
        || guard.expected_command.is_some()
        || guard.expected_process_started_at.is_some()
        || guard.expected_task_id.is_some();
    if !needs_validation {
        return true;
    }
    if let Some(expected_started_at) = guard.expected_process_started_at
        && background_shell_process_started_at(pid).as_deref() != Some(expected_started_at)
    {
        return false;
    }
    if let Some(expected_task_id) = guard.expected_task_id
        && matches!(
            background_shell_process_has_task_identity(pid, expected_task_id),
            Some(false)
        )
    {
        return false;
    }
    let Ok(output) = std::process::Command::new("ps")
        .args(["-o", "pid=,pgid=,command=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if line.is_empty() {
        return false;
    }
    let mut parts = line.split_whitespace();
    let Some(actual_pid) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
        return false;
    };
    let Some(actual_group_id) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
        return false;
    };
    let command = parts.collect::<Vec<_>>().join(" ");
    if actual_pid != pid {
        return false;
    }
    if let Some(expected_group_id) = expected_group_id
        && actual_group_id != expected_group_id
    {
        return false;
    }
    if let Some(expected_command) = guard.expected_command {
        if !background_shell_command_matches(&command, expected_command) {
            return false;
        }
    }
    true
}

#[cfg(target_os = "linux")]
fn background_shell_process_has_task_identity(pid: u32, expected_task_id: &str) -> Option<bool> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let expected = format!("{BACKGROUND_SHELL_TASK_ID_ENV}={expected_task_id}");
    Some(
        raw.split(|byte| *byte == 0)
            .any(|entry| entry == expected.as_bytes()),
    )
}

#[cfg(target_os = "macos")]
fn background_shell_process_has_task_identity(pid: u32, expected_task_id: &str) -> Option<bool> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid.try_into().ok()?];
    let arg_max = unsafe { libc::sysconf(libc::_SC_ARG_MAX) };
    let mut size = if arg_max > 0 {
        arg_max as libc::size_t
    } else {
        1024 * 1024
    };
    if size == 0 || size > 4 * 1024 * 1024 {
        size = 1024 * 1024;
    }
    let mut raw = vec![0u8; size];
    let read_result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            raw.as_mut_ptr().cast::<libc::c_void>(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read_result != 0 {
        return None;
    }
    raw.truncate(size);
    let expected = format!("{BACKGROUND_SHELL_TASK_ID_ENV}={expected_task_id}");
    let env_entries = macos_background_shell_process_env_entries(&raw)?;
    Some(
        env_entries
            .into_iter()
            .any(|entry| entry == expected.as_bytes()),
    )
}

#[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
fn background_shell_process_has_task_identity(_pid: u32, _expected_task_id: &str) -> Option<bool> {
    None
}

#[cfg(target_os = "macos")]
fn macos_background_shell_process_env_entries(raw: &[u8]) -> Option<Vec<&[u8]>> {
    let argc = i32::from_ne_bytes(raw.get(..4)?.try_into().ok()?);
    if !(0..=4096).contains(&argc) {
        return None;
    }
    let mut offset = 4usize;
    skip_macos_procargs_string(raw, &mut offset)?;
    skip_macos_procargs_padding(raw, &mut offset);
    for _ in 0..argc {
        skip_macos_procargs_string(raw, &mut offset)?;
        skip_macos_procargs_padding(raw, &mut offset);
    }
    let entries = raw[offset..]
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty() && entry.contains(&b'='))
        .collect::<Vec<_>>();
    if entries.is_empty() {
        None
    } else {
        Some(entries)
    }
}

#[cfg(target_os = "macos")]
fn skip_macos_procargs_string(raw: &[u8], offset: &mut usize) -> Option<()> {
    while *offset < raw.len() && raw[*offset] != 0 {
        *offset += 1;
    }
    if *offset >= raw.len() {
        return None;
    }
    Some(())
}

#[cfg(target_os = "macos")]
fn skip_macos_procargs_padding(raw: &[u8], offset: &mut usize) {
    while *offset < raw.len() && raw[*offset] == 0 {
        *offset += 1;
    }
}

#[cfg(target_os = "linux")]
fn background_shell_processes_with_task_identity(expected_task_id: &str) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            entry
                .ok()?
                .file_name()
                .to_string_lossy()
                .parse::<u32>()
                .ok()
        })
        .filter(|pid| {
            matches!(
                background_shell_process_has_task_identity(*pid, expected_task_id),
                Some(true)
            )
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn background_shell_processes_with_task_identity(expected_task_id: &str) -> Vec<u32> {
    background_shell_process_snapshots()
        .into_iter()
        .map(|snapshot| snapshot.pid)
        .filter(|pid| {
            matches!(
                background_shell_process_has_task_identity(*pid, expected_task_id),
                Some(true)
            )
        })
        .collect()
}

#[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
fn background_shell_processes_with_task_identity(_expected_task_id: &str) -> Vec<u32> {
    Vec::new()
}

#[cfg(unix)]
fn background_shell_identity_pids(expected_task_id: Option<&str>) -> Vec<u32> {
    expected_task_id
        .map(background_shell_processes_with_task_identity)
        .unwrap_or_default()
        .into_iter()
        .filter(|pid| {
            background_shell_process_exists(*pid) && !background_shell_process_is_zombie(*pid)
        })
        .collect()
}

#[cfg(all(unix, test))]
pub(crate) fn background_shell_task_identity_pids_for_test(task_id: &str) -> Vec<u32> {
    background_shell_identity_pids(Some(task_id))
}

#[cfg(unix)]
fn background_shell_command_matches(actual: &str, expected: &str) -> bool {
    let actual = normalized_background_shell_command(actual);
    let expected = normalized_background_shell_expected_command(expected);
    if actual.is_empty() || expected.is_empty() {
        return false;
    }
    if actual.contains(&expected) || expected.contains(&actual) {
        return true;
    }
    let actual_args = normalized_background_shell_command_args(&actual);
    let expected_args = normalized_background_shell_command_args(&expected);
    !actual_args.is_empty()
        && !expected_args.is_empty()
        && (actual_args.contains(&expected_args) || expected_args.contains(&actual_args))
}

#[cfg(unix)]
fn normalized_background_shell_command(command: &str) -> String {
    command
        .chars()
        .filter(|ch| !matches!(ch, '"' | '\''))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(unix)]
fn normalized_background_shell_expected_command(command: &str) -> String {
    let command = normalized_background_shell_command(command);
    command
        .strip_prefix("exec ")
        .unwrap_or(&command)
        .trim_end_matches('&')
        .trim()
        .to_string()
}

#[cfg(unix)]
fn normalized_background_shell_command_args(command: &str) -> String {
    command
        .split_whitespace()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(unix)]
fn background_shell_process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn signal_background_shell_process(pid: u32, signal: i32) -> std::io::Result<()> {
    let result = unsafe { libc::kill(pid as i32, signal) };
    if result == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}

#[cfg(unix)]
fn background_shell_process_is_zombie(pid: u32) -> bool {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim_start()
        .starts_with('Z')
}

#[cfg(unix)]
fn signal_background_shell_processes(pids: &[u32], signal: i32) -> bool {
    let mut sent = false;
    for pid in pids {
        sent |= signal_background_shell_process(*pid, signal).is_ok();
    }
    sent
}

#[cfg(unix)]
fn merge_process_ids(existing: &mut Vec<u32>, additional: Vec<u32>) {
    let mut seen: BTreeSet<u32> = existing.iter().copied().collect();
    for pid in additional {
        if seen.insert(pid) {
            existing.push(pid);
        }
    }
}

#[cfg(unix)]
fn background_shell_remaining_target_pids(
    process_group_id: Option<u32>,
    pid: Option<u32>,
    tracked_pids: &[u32],
    expected_task_id: Option<&str>,
) -> Vec<u32> {
    let mut remaining = Vec::new();
    if let Some(group_id) = process_group_id {
        remaining.extend(
            background_shell_process_group_members(group_id)
                .into_iter()
                .filter(|snapshot| !background_shell_process_is_zombie(snapshot.pid))
                .map(|snapshot| snapshot.pid),
        );
        merge_process_ids(
            &mut remaining,
            background_shell_process_group_descendant_pids(group_id)
                .into_iter()
                .filter(|pid| !background_shell_process_is_zombie(*pid))
                .collect(),
        );
    }
    if let Some(pid) = pid
        && background_shell_process_exists(pid)
        && !background_shell_process_is_zombie(pid)
    {
        merge_process_ids(&mut remaining, vec![pid]);
        merge_process_ids(
            &mut remaining,
            background_shell_descendant_pids(pid)
                .into_iter()
                .filter(|pid| !background_shell_process_is_zombie(*pid))
                .collect(),
        );
    }
    merge_process_ids(
        &mut remaining,
        tracked_pids
            .iter()
            .copied()
            .filter(|pid| {
                background_shell_process_exists(*pid) && !background_shell_process_is_zombie(*pid)
            })
            .collect(),
    );
    for tracked_pid in tracked_pids {
        merge_process_ids(
            &mut remaining,
            background_shell_descendant_pids(*tracked_pid)
                .into_iter()
                .filter(|pid| {
                    background_shell_process_exists(*pid)
                        && !background_shell_process_is_zombie(*pid)
                })
                .collect(),
        );
    }
    merge_process_ids(
        &mut remaining,
        background_shell_identity_pids(expected_task_id)
            .into_iter()
            .filter(|pid| !background_shell_process_is_zombie(*pid))
            .collect(),
    );
    remaining
}

#[cfg(unix)]
async fn wait_for_background_shell_remaining_pids(
    process_group_id: Option<u32>,
    pid: Option<u32>,
    tracked_pids: &[u32],
    expected_task_id: Option<&str>,
) -> Vec<u32> {
    let mut remaining = background_shell_remaining_target_pids(
        process_group_id,
        pid,
        tracked_pids,
        expected_task_id,
    );
    for _ in 0..50 {
        if remaining.is_empty() {
            return remaining;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        remaining = background_shell_remaining_target_pids(
            process_group_id,
            pid,
            tracked_pids,
            expected_task_id,
        );
    }
    remaining
}

/// Builds one fresh shell-task record ready to enter the session control state.
pub fn build_background_shell_task_record(
    task_id: String,
    owner_agent_id: String,
    request: &BackgroundShellTaskRequest,
    output_file_path: impl Into<String>,
    now_ms: u64,
) -> TaskRecord {
    TaskRecord {
        id: task_id,
        title: request.description.clone(),
        description: format!(
            "{} shell command: {}",
            if request.started_in_background {
                "Background"
            } else {
                "Managed"
            },
            request.command
        ),
        status: TaskStatus::InProgress,
        owner_agent_id: Some(owner_agent_id),
        blocked_by: Vec::new(),
        blocks: Vec::new(),
        output: None,
        metadata: json!(BackgroundShellTaskMetadata::new(request, output_file_path)),
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use anyhow::Result;

    use super::*;

    #[test]
    fn detects_prompt_like_output() {
        assert!(looks_like_interactive_prompt("Continue?"));
        assert!(looks_like_interactive_prompt(
            "Are you sure you want to continue?"
        ));
        assert!(!looks_like_interactive_prompt("all good\nstill running"));
    }

    #[tokio::test]
    async fn reads_full_output_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("output.log");
        let mut file = File::create(&path).expect("create log");
        file.write_all(b"alpha\nbeta\n").expect("write log");
        drop(file);

        let body = read_task_output_text(&path)
            .await
            .expect("read full output");
        assert_eq!(body, "alpha\nbeta\n");
    }

    #[tokio::test]
    async fn output_writer_rotates_retained_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("output.log");
        let mut writer = ShellTaskOutputWriter::new_with_limits(path.clone(), 256, 64);
        writer
            .append(&vec![b'a'; 300])
            .await
            .expect("append output");
        writer.append(b"tail-line\n").await.expect("append tail");

        let stats = writer.stats().await.expect("writer stats");
        assert!(stats.rotated);
        assert!(stats.rotation_count >= 1);
        assert_eq!(stats.total_size_bytes, 310);
        assert!(stats.retained_size_bytes <= 256);

        let retained = read_task_output_text(&path)
            .await
            .expect("read retained output");
        assert!(retained.contains(OUTPUT_ROTATION_MARKER));
        assert!(retained.contains("tail-line"));
    }

    #[tokio::test]
    async fn recovers_output_rotation_stats_from_retained_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("output.log");
        let mut writer = ShellTaskOutputWriter::new_with_limits(path.clone(), 256, 64);
        writer
            .append(&vec![b'a'; 300])
            .await
            .expect("append output");
        writer.append(b"tail-line\n").await.expect("append tail");

        let stats = recover_shell_task_output_stats(&path).expect("recovered stats");
        assert!(stats.rotated);
        assert_eq!(stats.rotation_count, 1);
        assert_eq!(stats.total_size_bytes, 310);
        assert!(stats.retained_size_bytes <= 256);
    }

    #[tokio::test]
    async fn output_view_ignores_forged_background_shell_metadata_on_regular_tasks() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let secret = dir.path().join("secret.log");
        tokio::fs::write(&secret, "DO_NOT_EXPOSE").await?;
        let task = TaskRecord {
            id: "task-1".to_string(),
            title: "forged".to_string(),
            description: String::new(),
            status: TaskStatus::InProgress,
            owner_agent_id: None,
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            output: None,
            metadata: json!(BackgroundShellTaskMetadata {
                kind: BACKGROUND_SHELL_TASK_KIND.to_string(),
                output_file_path: secret.display().to_string(),
                ..BackgroundShellTaskMetadata::default()
            }),
            created_at_ms: 1,
            updated_at_ms: 1,
        };

        assert!(background_shell_metadata(&task).is_none());
        let view = build_task_output_view("success", task, 1024, true).await;
        assert!(view.output_text.is_none());
        assert!(view.output_excerpt.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn output_view_detects_rotation_from_retained_file_for_live_task() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("output.log");
        let mut writer = ShellTaskOutputWriter::new_with_limits(path.clone(), 256, 64);
        writer.append(&vec![b'a'; 300]).await?;
        writer.append(b"tail-line\n").await?;
        let task = TaskRecord {
            id: "shell-task-1".to_string(),
            title: "rotated".to_string(),
            description: String::new(),
            status: TaskStatus::InProgress,
            owner_agent_id: None,
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            output: None,
            metadata: json!(BackgroundShellTaskMetadata {
                kind: BACKGROUND_SHELL_TASK_KIND.to_string(),
                output_file_path: path.display().to_string(),
                ..BackgroundShellTaskMetadata::default()
            }),
            created_at_ms: 1,
            updated_at_ms: 1,
        };

        let view = build_task_output_view("not_ready", task, 16, false).await;
        assert!(view.output_rotated);
        assert!(view.output_truncated);
        assert_eq!(view.output_rotation_count, 1);
        assert_eq!(view.output_total_bytes, Some(310));
        assert!(view.output_text.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn output_view_uses_current_file_size_over_stale_live_metadata() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("output.log");
        tokio::fs::write(&path, "current output body").await?;
        let task = TaskRecord {
            id: "shell-task-1".to_string(),
            title: "live".to_string(),
            description: String::new(),
            status: TaskStatus::InProgress,
            owner_agent_id: None,
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            output: None,
            metadata: json!(BackgroundShellTaskMetadata {
                kind: BACKGROUND_SHELL_TASK_KIND.to_string(),
                output_file_path: path.display().to_string(),
                output_size_bytes: Some(1),
                output_total_bytes: Some(1),
                ..BackgroundShellTaskMetadata::default()
            }),
            created_at_ms: 1,
            updated_at_ms: 1,
        };

        let view = build_task_output_view("not_ready", task, 1024, false).await;
        assert_eq!(view.output_size_bytes, Some(19));
        assert_eq!(view.output_total_bytes, Some(19));
        assert_eq!(view.output_excerpt.as_deref(), Some("current output body"));
        Ok(())
    }

    #[cfg(unix)]
    fn process_group_id(pid: u32) -> Result<u32> {
        let output = std::process::Command::new("ps")
            .args(["-o", "pgid=", "-p", &pid.to_string()])
            .output()?;
        anyhow::ensure!(output.status.success(), "ps failed for pid {pid}");
        Ok(String::from_utf8_lossy(&output.stdout).trim().parse()?)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_shell_command_uses_a_distinct_process_group() -> Result<()> {
        let mut command = Command::new("bash");
        command.arg("-lc").arg("sleep 5");
        configure_background_shell_command(&mut command);
        let mut child = command.spawn()?;
        let child_pid = child.id().ok_or_else(|| anyhow!("missing child pid"))?;
        let parent_pgid = process_group_id(std::process::id())?;
        let child_pgid = process_group_id(child_pid)?;
        let outcome = shutdown_background_shell_processes(
            Some(child_pid),
            Some(child_pid),
            BackgroundShellShutdownGuard::default(),
        )
        .await;
        let _ = child.wait().await;
        assert!(outcome.matched_target);
        assert_ne!(child_pgid, parent_pgid);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persisted_target_validation_matches_expected_command() -> Result<()> {
        let mut command = Command::new("bash");
        command.arg("-lc").arg("sleep 5");
        configure_background_shell_command(&mut command);
        let mut child = command.spawn()?;
        let child_pid = child.id().ok_or_else(|| anyhow!("missing child pid"))?;
        assert!(background_shell_persisted_target_matches(
            child_pid,
            Some(child_pid),
            BackgroundShellShutdownGuard {
                expected_command: Some("sleep 5"),
                ..Default::default()
            },
        ));
        assert!(!background_shell_persisted_target_matches(
            child_pid,
            Some(child_pid),
            BackgroundShellShutdownGuard {
                expected_command: Some("definitely-not-the-running-command"),
                ..Default::default()
            },
        ));
        shutdown_background_shell_processes(
            Some(child_pid),
            Some(child_pid),
            BackgroundShellShutdownGuard::default(),
        )
        .await;
        let _ = child.wait().await;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persisted_target_validation_accepts_exec_replaced_leader() -> Result<()> {
        let mut command = Command::new("bash");
        command.arg("-lc").arg("exec sleep 30");
        configure_background_shell_command(&mut command);
        let mut child = command.spawn()?;
        let child_pid = child.id().ok_or_else(|| anyhow!("missing child pid"))?;
        assert!(background_shell_persisted_target_matches(
            child_pid,
            Some(child_pid),
            BackgroundShellShutdownGuard {
                expected_command: Some("exec sleep 30"),
                ..Default::default()
            },
        ));
        shutdown_background_shell_processes(
            Some(child_pid),
            Some(child_pid),
            BackgroundShellShutdownGuard {
                expected_command: Some("exec sleep 30"),
                ..Default::default()
            },
        )
        .await;
        let _ = child.wait().await;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_validation_does_not_trust_unverified_task_identity() -> Result<()> {
        let mut command = Command::new("bash");
        command.arg("-lc").arg("sleep 30");
        configure_background_shell_command(&mut command);
        let mut child = command.spawn()?;
        let child_pid = child.id().ok_or_else(|| anyhow!("missing child pid"))?;

        assert!(!background_shell_process_group_target_matches(
            child_pid,
            BackgroundShellShutdownGuard {
                expected_command: Some("definitely-not-the-running-command"),
                expected_task_id: Some("unverified-shell-task-id"),
                ..Default::default()
            },
        ));
        shutdown_background_shell_processes(
            Some(child_pid),
            Some(child_pid),
            BackgroundShellShutdownGuard::default(),
        )
        .await;
        let _ = child.wait().await;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_identity_check_distinguishes_present_and_absent_tokens() -> Result<()> {
        let python_ready = std::process::Command::new("python3")
            .arg("-c")
            .arg("import time")
            .status();
        if !python_ready.map(|status| status.success()).unwrap_or(false) {
            return Ok(());
        }

        let token = format!("shell-task-identity-token-{}", std::process::id());
        let mut with_identity = Command::new("python3");
        with_identity
            .arg("-c")
            .arg("import time; time.sleep(30)")
            .env(BACKGROUND_SHELL_TASK_ID_ENV, &token);
        configure_background_shell_command(&mut with_identity);
        let mut with_identity = with_identity.spawn()?;
        let with_identity_pid = with_identity
            .id()
            .ok_or_else(|| anyhow!("missing identity child pid"))?;

        let mut without_identity = Command::new("python3");
        without_identity
            .arg("-c")
            .arg("import time; time.sleep(30)")
            .env_remove(BACKGROUND_SHELL_TASK_ID_ENV);
        configure_background_shell_command(&mut without_identity);
        let mut without_identity = without_identity.spawn()?;
        let without_identity_pid = without_identity
            .id()
            .ok_or_else(|| anyhow!("missing non-identity child pid"))?;

        for _ in 0..50 {
            if background_shell_process_has_task_identity(with_identity_pid, &token) == Some(true)
                && background_shell_process_has_task_identity(without_identity_pid, &token)
                    == Some(false)
            {
                shutdown_background_shell_processes(
                    Some(with_identity_pid),
                    Some(with_identity_pid),
                    BackgroundShellShutdownGuard::default(),
                )
                .await;
                shutdown_background_shell_processes(
                    Some(without_identity_pid),
                    Some(without_identity_pid),
                    BackgroundShellShutdownGuard::default(),
                )
                .await;
                let _ = with_identity.wait().await;
                let _ = without_identity.wait().await;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let present = background_shell_process_has_task_identity(with_identity_pid, &token);
        let absent = background_shell_process_has_task_identity(without_identity_pid, &token);
        shutdown_background_shell_processes(
            Some(with_identity_pid),
            Some(with_identity_pid),
            BackgroundShellShutdownGuard::default(),
        )
        .await;
        shutdown_background_shell_processes(
            Some(without_identity_pid),
            Some(without_identity_pid),
            BackgroundShellShutdownGuard::default(),
        )
        .await;
        let _ = with_identity.wait().await;
        let _ = without_identity.wait().await;
        anyhow::bail!("unexpected identity checks: present={present:?} absent={absent:?}")
    }

    #[cfg(unix)]
    fn process_snapshot(pid: u32) -> Option<BackgroundShellProcessSnapshot> {
        background_shell_process_snapshots()
            .into_iter()
            .find(|snapshot| snapshot.pid == pid)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_kills_descendant_that_escaped_process_group() -> Result<()> {
        let python_ready = std::process::Command::new("python3")
            .arg("-c")
            .arg("import os, signal, subprocess")
            .status();
        if !python_ready.map(|status| status.success()).unwrap_or(false) {
            return Ok(());
        }

        let script = r#"
import os, signal, subprocess, sys, time
child = subprocess.Popen(["sleep", "30"], preexec_fn=os.setsid)

def stop(signum, frame):
    try:
        child.terminate()
    except Exception:
        pass
    try:
        child.wait(timeout=5)
    except Exception:
        try:
            child.kill()
        except Exception:
            pass
        try:
            child.wait(timeout=5)
        except Exception:
            pass
    sys.exit(0)

signal.signal(signal.SIGTERM, stop)
while True:
    time.sleep(1)
"#;
        let mut command = Command::new("python3");
        command.arg("-c").arg(script);
        configure_background_shell_command(&mut command);
        let mut child = command.spawn()?;
        let child_pid = child.id().ok_or_else(|| anyhow!("missing child pid"))?;

        let mut escaped_pid = None;
        for _ in 0..50 {
            for pid in background_shell_descendant_pids(child_pid) {
                if process_snapshot(pid)
                    .map(|snapshot| snapshot.group_id != child_pid)
                    .unwrap_or(false)
                {
                    escaped_pid = Some(pid);
                    break;
                }
            }
            if escaped_pid.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let escaped_pid =
            escaped_pid.ok_or_else(|| anyhow!("expected a descendant outside process group"))?;

        let outcome = shutdown_background_shell_processes(
            Some(child_pid),
            Some(child_pid),
            BackgroundShellShutdownGuard::default(),
        )
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        assert!(outcome.matched_target);

        for _ in 0..50 {
            if process_snapshot(escaped_pid).is_none() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let snapshot = process_snapshot(escaped_pid);
        let _ = signal_background_shell_process(escaped_pid, libc::SIGKILL);
        anyhow::bail!(
            "escaped descendant pid {escaped_pid} survived shutdown; snapshot={snapshot:?}"
        )
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_kills_process_group_after_leader_exits() -> Result<()> {
        let mut command = Command::new("bash");
        command.arg("-lc").arg("sleep 30 & exit 0");
        configure_background_shell_command(&mut command);
        let mut child = command.spawn()?;
        let child_pid = child.id().ok_or_else(|| anyhow!("missing child pid"))?;
        tokio::time::timeout(Duration::from_secs(3), child.wait()).await??;

        for _ in 0..20 {
            if !background_shell_process_group_members(child_pid).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !background_shell_process_group_members(child_pid).is_empty(),
            "expected one descendant to remain in process group {child_pid}"
        );

        shutdown_background_shell_processes(
            Some(child_pid),
            Some(child_pid),
            BackgroundShellShutdownGuard {
                expected_command: Some("sleep 30 & exit 0"),
                ..Default::default()
            },
        )
        .await;
        for _ in 0..20 {
            if background_shell_process_group_members(child_pid).is_empty() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("process group {child_pid} still had members after shutdown")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_kills_identity_marked_process_without_recorded_pid() -> Result<()> {
        let python_ready = std::process::Command::new("python3")
            .arg("-c")
            .arg("import os, subprocess")
            .status();
        if !python_ready.map(|status| status.success()).unwrap_or(false) {
            return Ok(());
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let task_id = format!("shell-task-identity-test-{}-{now}", std::process::id());
        let script = r#"
import os, subprocess, sys
subprocess.Popen(
    ["python3", "-c", "import time; time.sleep(30)"],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    preexec_fn=os.setsid,
)
sys.exit(0)
"#;
        let mut command = Command::new("sh");
        command
            .arg("-lc")
            .arg(format!("python3 -c {}", shell_quote_for_test(script)))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .env(BACKGROUND_SHELL_TASK_ID_ENV, &task_id);
        configure_background_shell_command(&mut command);
        let mut child = command.spawn()?;
        let leader_pid = child.id().ok_or_else(|| anyhow!("missing child pid"))?;
        tokio::time::timeout(Duration::from_secs(3), child.wait()).await??;

        for _ in 0..50 {
            let identity_pids = background_shell_identity_pids(Some(&task_id));
            if identity_pids.iter().any(|pid| *pid != leader_pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let identity_pids = background_shell_identity_pids(Some(&task_id));
        assert!(
            !identity_pids.is_empty(),
            "expected detached process carrying shell task identity {task_id}"
        );

        let outcome = shutdown_background_shell_processes(
            None,
            None,
            BackgroundShellShutdownGuard {
                expected_task_id: Some(&task_id),
                ..Default::default()
            },
        )
        .await;
        assert!(outcome.matched_target);
        assert!(
            outcome.confirmed,
            "identity shutdown should be confirmed: {outcome:?}"
        );

        for _ in 0..50 {
            if background_shell_identity_pids(Some(&task_id)).is_empty() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let remaining = background_shell_identity_pids(Some(&task_id));
        for pid in &remaining {
            let _ = signal_background_shell_process(*pid, libc::SIGKILL);
        }
        anyhow::bail!("identity-marked pids survived shutdown: {remaining:?}")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn identity_shutdown_does_not_kill_different_identity() -> Result<()> {
        let python_ready = std::process::Command::new("python3")
            .arg("-c")
            .arg("import time")
            .status();
        if !python_ready.map(|status| status.success()).unwrap_or(false) {
            return Ok(());
        }

        let token_a = format!("shell-task-token-a-{}", std::process::id());
        let token_b = format!("shell-task-token-b-{}", std::process::id());
        let mut command_a = Command::new("python3");
        command_a
            .arg("-c")
            .arg("import time; time.sleep(30)")
            .env(BACKGROUND_SHELL_TASK_ID_ENV, &token_a);
        configure_background_shell_command(&mut command_a);
        let mut child_a = command_a.spawn()?;

        let mut command_b = Command::new("python3");
        command_b
            .arg("-c")
            .arg("import time; time.sleep(30)")
            .env(BACKGROUND_SHELL_TASK_ID_ENV, &token_b);
        configure_background_shell_command(&mut command_b);
        let mut child_b = command_b.spawn()?;
        let pid_b = child_b.id().ok_or_else(|| anyhow!("missing child b pid"))?;

        for _ in 0..50 {
            if !background_shell_identity_pids(Some(&token_a)).is_empty()
                && !background_shell_identity_pids(Some(&token_b)).is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let outcome = shutdown_background_shell_processes(
            None,
            None,
            BackgroundShellShutdownGuard {
                expected_task_id: Some(&token_a),
                ..Default::default()
            },
        )
        .await;
        assert!(outcome.matched_target);
        let _ = child_a.wait().await;

        assert!(
            background_shell_process_exists(pid_b) && !background_shell_process_is_zombie(pid_b),
            "different identity process should survive token-scoped shutdown"
        );
        shutdown_background_shell_processes(
            Some(pid_b),
            Some(pid_b),
            BackgroundShellShutdownGuard::default(),
        )
        .await;
        let _ = child_b.wait().await;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_start_time_without_identity_does_not_signal_target() -> Result<()> {
        let mut command = Command::new("bash");
        command.arg("-lc").arg("sleep 30");
        configure_background_shell_command(&mut command);
        let mut child = command.spawn()?;
        let child_pid = child.id().ok_or_else(|| anyhow!("missing child pid"))?;
        let outcome = shutdown_background_shell_processes(
            Some(child_pid),
            Some(child_pid),
            BackgroundShellShutdownGuard {
                expected_command: Some("sleep 30"),
                expected_process_started_at: Some("not the real process start time"),
                expected_task_id: None,
            },
        )
        .await;
        assert!(!outcome.matched_target);
        assert!(!outcome.signal_sent);
        assert!(
            background_shell_process_exists(child_pid)
                && !background_shell_process_is_zombie(child_pid)
        );
        shutdown_background_shell_processes(
            Some(child_pid),
            Some(child_pid),
            BackgroundShellShutdownGuard::default(),
        )
        .await;
        let _ = child.wait().await;
        Ok(())
    }

    #[cfg(unix)]
    fn shell_quote_for_test(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_kills_descendant_spawned_during_sigterm() -> Result<()> {
        let python_ready = std::process::Command::new("python3")
            .arg("-c")
            .arg("import os, signal, subprocess")
            .status();
        if !python_ready.map(|status| status.success()).unwrap_or(false) {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let pid_file = dir.path().join("escaped.pid");
        let script = r#"
import os, signal, subprocess, sys, time
pid_file = sys.argv[1]
spawned = None

def stop(signum, frame):
    global spawned
    if spawned is None:
        spawned = subprocess.Popen(["sleep", "30"], preexec_fn=os.setsid)
        with open(pid_file, "w", encoding="utf-8") as handle:
            handle.write(str(spawned.pid))
    time.sleep(10)
    sys.exit(0)

signal.signal(signal.SIGTERM, stop)
while True:
    time.sleep(1)
"#;
        let mut command = Command::new("python3");
        command.arg("-c").arg(script).arg(&pid_file);
        configure_background_shell_command(&mut command);
        let mut child = command.spawn()?;
        let child_pid = child.id().ok_or_else(|| anyhow!("missing child pid"))?;

        let outcome = shutdown_background_shell_processes(
            Some(child_pid),
            Some(child_pid),
            BackgroundShellShutdownGuard::default(),
        )
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        assert!(outcome.matched_target);
        let escaped_pid = std::fs::read_to_string(&pid_file)?.trim().parse::<u32>()?;
        for _ in 0..50 {
            if process_snapshot(escaped_pid).is_none() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let snapshot = process_snapshot(escaped_pid);
        let _ = signal_background_shell_process(escaped_pid, libc::SIGKILL);
        anyhow::bail!(
            "late escaped descendant pid {escaped_pid} survived shutdown; snapshot={snapshot:?}"
        )
    }
}
