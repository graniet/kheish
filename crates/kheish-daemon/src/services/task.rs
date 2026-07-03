use parking_lot::Mutex as SyncMutex;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Notify};

use kheish_types::{SessionControlState, TaskRecord, TaskStatus};

use crate::DaemonTaskStatusSummaryView;
use crate::shell_tasks::{
    BackgroundShellShutdownOutcome, BackgroundShellTaskMetadata, BackgroundShellTaskRequest,
    TaskOutputView, background_shell_metadata, build_background_shell_task_record,
    build_task_output_view, recover_shell_task_output_stats,
};

/// Tracks live daemon-managed background shell task handles.
pub(crate) struct TaskService {
    background_shell_tasks: Mutex<BTreeMap<(String, String), BackgroundShellTaskHandle>>,
}

/// Mutable runtime handle for one background shell task owned by the daemon.
#[derive(Clone)]
pub(crate) struct BackgroundShellTaskHandle {
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) wake: Arc<Notify>,
    pub(crate) stop_reason: Arc<SyncMutex<Option<String>>>,
}

/// Describes the terminal state observed by one background shell task runner.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BackgroundShellTaskFinalState {
    pub(crate) exit_code: Option<i32>,
    pub(crate) output_size_bytes: u64,
    pub(crate) output_total_bytes: u64,
    pub(crate) output_rotated: bool,
    pub(crate) output_rotation_count: u64,
    pub(crate) cancelled: bool,
    pub(crate) killed_for_size: bool,
    pub(crate) interactive_prompt_detected: bool,
    pub(crate) stop_reason: Option<String>,
    pub(crate) shutdown_outcome: Option<BackgroundShellShutdownOutcome>,
    pub(crate) output_capture_error: Option<String>,
}

/// Captures one finalized background shell task after its state was persisted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FinalizedBackgroundShellTask {
    pub(crate) task: TaskRecord,
    pub(crate) metadata: BackgroundShellTaskMetadata,
    pub(crate) notify_on_completion: bool,
}

impl BackgroundShellTaskHandle {
    /// Creates one fresh background shell task handle.
    pub(crate) fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(Notify::new()),
            stop_reason: Arc::new(SyncMutex::new(None)),
        }
    }
}

impl TaskService {
    /// Creates a new task service with an empty runtime handle registry.
    pub(crate) fn new() -> Self {
        Self {
            background_shell_tasks: Mutex::new(BTreeMap::new()),
        }
    }

    /// Returns one unique background shell task identifier for the current
    /// session, avoiding live and archived task ids alike.
    pub(crate) fn next_background_shell_task_id(
        session_id: &str,
        tasks: &[kheish_types::TaskRecord],
        archived_task_ids: &std::collections::BTreeSet<String>,
        now: u64,
    ) -> String {
        let is_free = |candidate: &str| {
            tasks.iter().all(|task| task.id != candidate) && !archived_task_ids.contains(candidate)
        };
        let base = format!("shell-task-{}-{now}", shell_task_session_prefix(session_id));
        if is_free(&base) {
            return base;
        }
        let mut suffix = 1u32;
        loop {
            let candidate = format!("{base}-{suffix}");
            if is_free(&candidate) {
                return candidate;
            }
            suffix = suffix.saturating_add(1);
        }
    }

    /// Appends one managed background shell task to the current session control state.
    pub(crate) fn insert_managed_shell_task(
        &self,
        state: &mut SessionControlState,
        task_id: String,
        owner_agent_id: &str,
        request: &BackgroundShellTaskRequest,
        output_path: &Path,
        now_ms: u64,
    ) -> TaskRecord {
        let task = build_background_shell_task_record(
            task_id,
            owner_agent_id.to_string(),
            request,
            output_path.display().to_string(),
            now_ms,
        );
        state.tasks.push(task.clone());
        task
    }

    /// Marks one live managed shell task as detached from the invoking run.
    pub(crate) fn detach_shell_task(
        &self,
        state: &mut SessionControlState,
        task_id: &str,
        updated_at_ms: u64,
    ) -> Result<bool> {
        let Some(task) = state.tasks.iter_mut().find(|task| task.id == task_id) else {
            return Ok(false);
        };
        let Some(mut metadata) = background_shell_metadata(task) else {
            return Ok(false);
        };
        if matches!(
            task.status,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            return Ok(false);
        };
        metadata.started_in_background = true;
        task.metadata = serde_json::to_value(&metadata)?;
        task.updated_at_ms = updated_at_ms;
        Ok(true)
    }

    /// Updates the persisted metadata for one managed shell task when it is still present.
    pub(crate) fn update_background_shell_metadata<F>(
        &self,
        state: &mut SessionControlState,
        task_id: &str,
        updated_at_ms: u64,
        update: F,
    ) -> Result<bool>
    where
        F: FnOnce(&mut BackgroundShellTaskMetadata),
    {
        let Some(task) = state.tasks.iter_mut().find(|task| task.id == task_id) else {
            return Ok(false);
        };
        let mut metadata = background_shell_metadata(task)
            .ok_or_else(|| anyhow!("task {task_id} is not a background shell task"))?;
        update(&mut metadata);
        task.metadata = serde_json::to_value(&metadata)?;
        task.updated_at_ms = updated_at_ms;
        Ok(true)
    }

    /// Applies one background shell task completion outcome to the current session state.
    pub(crate) fn finalize_background_shell_task(
        &self,
        state: &mut SessionControlState,
        task_id: &str,
        outcome: std::result::Result<BackgroundShellTaskFinalState, String>,
        output_size_bytes: Option<u64>,
        updated_at_ms: u64,
        _preserve_existing_terminal_state: bool,
    ) -> Result<Option<FinalizedBackgroundShellTask>> {
        let Some(task) = state.tasks.iter_mut().find(|task| task.id == task_id) else {
            return Ok(None);
        };
        if matches!(
            task.status,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            return Ok(None);
        }
        let mut metadata = background_shell_metadata(task).unwrap_or_default();
        let notify_on_completion = metadata.started_in_background;
        match outcome {
            Ok(final_state) => {
                metadata.exit_code = final_state.exit_code;
                metadata.terminal_reason = None;
                metadata.recovered_on_boot = false;
                metadata.output_size_bytes = Some(final_state.output_size_bytes);
                metadata.output_total_bytes = Some(final_state.output_total_bytes);
                metadata.output_rotated = final_state.output_rotated;
                metadata.output_rotation_count = final_state.output_rotation_count;
                metadata.cancelled = final_state.cancelled;
                metadata.killed_for_size = final_state.killed_for_size;
                metadata.interactive_prompt_detected = final_state.interactive_prompt_detected;
                metadata.stop_requested = final_state.cancelled;
                metadata.stop_reason = final_state.stop_reason.clone();
                if let Some(outcome) = final_state.shutdown_outcome.as_ref() {
                    apply_background_shell_shutdown_outcome(&mut metadata, outcome);
                }
                if let Some(error) = final_state.output_capture_error {
                    metadata.terminal_reason = Some("output_capture_failed".to_string());
                    metadata.output_capture_error = Some(error.clone());
                    task.status = TaskStatus::Failed;
                    task.output = Some(format!(
                        "Background shell task failed while capturing output: {error}. Inspect partial output with task_output on {task_id}."
                    ));
                } else if final_state.killed_for_size {
                    task.status = TaskStatus::Failed;
                    task.output = Some(format!(
                        "Background shell task exceeded the output limit and was terminated. Use task_output on {task_id} to inspect the captured output."
                    ));
                } else if final_state.cancelled {
                    task.status = TaskStatus::Cancelled;
                    task.output = Some(
                        final_state
                            .stop_reason
                            .unwrap_or_else(|| "Background shell task was cancelled.".to_string()),
                    );
                } else if final_state.exit_code.unwrap_or(1) == 0 {
                    task.status = TaskStatus::Completed;
                    task.output = Some(format!(
                        "Background shell task completed successfully. Use task_output on {task_id} to inspect the output."
                    ));
                } else {
                    task.status = TaskStatus::Failed;
                    task.output = Some(format!(
                        "Background shell task failed with exit code {}. Use task_output on {task_id} to inspect the output.",
                        final_state.exit_code.unwrap_or(-1)
                    ));
                }
            }
            Err(error) => {
                task.status = TaskStatus::Failed;
                metadata.terminal_reason = None;
                metadata.recovered_on_boot = false;
                metadata.output_size_bytes = output_size_bytes;
                metadata.output_total_bytes = output_size_bytes;
                task.output = Some(format!(
                    "Background shell task could not be started: {error}. Use task_output on {task_id} for any partial output."
                ));
            }
        };
        task.metadata = serde_json::to_value(&metadata)?;
        task.updated_at_ms = updated_at_ms;
        Ok(Some(FinalizedBackgroundShellTask {
            task: task.clone(),
            metadata,
            notify_on_completion,
        }))
    }

    /// Cancels one persisted task when no live runtime handle can be reached.
    pub(crate) fn cancel_persisted_task(
        &self,
        state: &mut SessionControlState,
        task_id: &str,
        reason: Option<String>,
        updated_at_ms: u64,
    ) -> Result<TaskRecord> {
        let task = state
            .tasks
            .iter_mut()
            .find(|task| task.id == task_id)
            .ok_or_else(|| anyhow!("unknown task {task_id}"))?;
        if matches!(
            task.status,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            return Ok(task.clone());
        }
        if let Some(mut metadata) = background_shell_metadata(task) {
            metadata.cancelled = true;
            metadata.stop_requested = true;
            metadata.stop_requested_at_ms = Some(updated_at_ms);
            metadata.stop_reason = reason.clone();
            task.metadata = serde_json::to_value(&metadata)?;
        }
        task.status = TaskStatus::Cancelled;
        task.output = Some(reason.unwrap_or_else(|| "Task cancelled.".to_string()));
        task.updated_at_ms = updated_at_ms;
        Ok(task.clone())
    }

    /// Marks interrupted background shell tasks as failed after a daemon restart.
    pub(crate) fn restore_background_shell_tasks_on_boot(
        &self,
        state: &mut SessionControlState,
        updated_at_ms: u64,
    ) -> Result<Vec<FinalizedBackgroundShellTask>> {
        let mut recovered = Vec::new();
        for task in &mut state.tasks {
            let Some(mut metadata) = background_shell_metadata(task) else {
                continue;
            };
            if !matches!(task.status, TaskStatus::Pending | TaskStatus::InProgress) {
                if background_shell_terminal_task_needs_shutdown_retry(task, &metadata) {
                    recovered.push(FinalizedBackgroundShellTask {
                        task: task.clone(),
                        metadata: metadata.clone(),
                        notify_on_completion: false,
                    });
                }
                continue;
            }
            metadata.cancelled = false;
            metadata.terminal_reason = Some("daemon_restarted".to_string());
            metadata.recovered_on_boot = true;
            if let Some(output_stats) = recover_shell_task_output_stats(&metadata.output_path()) {
                metadata.output_size_bytes = Some(output_stats.retained_size_bytes);
                metadata.output_total_bytes = Some(
                    metadata
                        .output_total_bytes
                        .unwrap_or_default()
                        .max(output_stats.total_size_bytes),
                );
                metadata.output_rotated = metadata.output_rotated || output_stats.rotated;
                metadata.output_rotation_count = metadata
                    .output_rotation_count
                    .max(output_stats.rotation_count);
            }
            task.status = TaskStatus::Failed;
            task.output = Some("Background shell task did not complete because the daemon restarted. The previous process was terminated best-effort. Inspect partial output with task_output and retry only if still needed.".to_string());
            task.updated_at_ms = updated_at_ms;
            task.metadata = serde_json::to_value(&metadata)?;
            recovered.push(FinalizedBackgroundShellTask {
                task: task.clone(),
                metadata: metadata.clone(),
                notify_on_completion: metadata.started_in_background,
            });
        }
        Ok(recovered)
    }

    /// Waits for one task to settle, returning the latest persisted snapshot.
    pub(crate) async fn wait_for_task_settle<Load, Fut>(
        &self,
        mut load_state: Load,
        task_id: &str,
        timeout: Duration,
    ) -> Result<TaskRecord>
    where
        Load: FnMut() -> Fut,
        Fut: Future<Output = Result<SessionControlState>>,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let state = load_state().await?;
            let task = state
                .tasks
                .into_iter()
                .find(|task| task.id == task_id)
                .ok_or_else(|| anyhow!("unknown task {task_id}"))?;
            if !matches!(task.status, TaskStatus::Pending | TaskStatus::InProgress) {
                return Ok(task);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(task);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Builds one task output view, optionally waiting for the task to settle first.
    pub(crate) async fn task_output_view<Load, Fut>(
        &self,
        mut load_state: Load,
        task_id: &str,
        wait: bool,
        timeout: Duration,
        tail_bytes: usize,
        include_full_output: bool,
    ) -> Result<TaskOutputView>
    where
        Load: FnMut() -> Fut,
        Fut: Future<Output = Result<SessionControlState>>,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let state = load_state().await?;
            let Some(task) = state.tasks.into_iter().find(|task| task.id == task_id) else {
                anyhow::bail!("unknown task {task_id}");
            };
            let terminal = !matches!(task.status, TaskStatus::Pending | TaskStatus::InProgress);
            if terminal {
                return Ok(build_task_output_view(
                    "success",
                    task,
                    tail_bytes,
                    include_full_output,
                )
                .await);
            }
            if !wait {
                return Ok(build_task_output_view(
                    "not_ready",
                    task,
                    tail_bytes,
                    include_full_output,
                )
                .await);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(build_task_output_view(
                    "timeout",
                    task,
                    tail_bytes,
                    include_full_output,
                )
                .await);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Registers a live background shell task handle.
    pub(crate) async fn register_background_shell_task(
        &self,
        session_id: String,
        task_id: String,
        handle: BackgroundShellTaskHandle,
    ) {
        self.background_shell_tasks
            .lock()
            .await
            .insert((session_id, task_id), handle);
    }

    /// Removes one background shell task handle when its runner settles.
    pub(crate) async fn remove_background_shell_task(&self, session_id: &str, task_id: &str) {
        self.background_shell_tasks
            .lock()
            .await
            .remove(&(session_id.to_string(), task_id.to_string()));
    }

    /// Returns a cheap point-in-time summary for live daemon-managed tasks.
    pub(crate) async fn status_snapshot(&self) -> DaemonTaskStatusSummaryView {
        DaemonTaskStatusSummaryView {
            live_background_shell_task_count: self.background_shell_tasks.lock().await.len(),
            ..Default::default()
        }
    }

    /// Requests cancellation for one live background shell task when present.
    pub(crate) async fn request_background_shell_stop(
        &self,
        session_id: &str,
        task_id: &str,
        reason: Option<String>,
    ) -> bool {
        let handle = self
            .background_shell_tasks
            .lock()
            .await
            .get(&(session_id.to_string(), task_id.to_string()))
            .cloned();
        let Some(handle) = handle else {
            return false;
        };
        let mut stop_reason = handle.stop_reason.lock();
        if !handle.cancelled.load(Ordering::Acquire) {
            *stop_reason = reason;
            handle.cancelled.store(true, Ordering::Release);
        }
        drop(stop_reason);
        handle.wake.notify_waiters();
        true
    }
}

/// Returns whether one terminal shell task still needs a boot-time shutdown
/// retry and must therefore stay in the hot control state instead of being
/// archived: the archive is immutable, so archiving it would drop the retry.
pub(crate) fn background_shell_task_shutdown_unsettled(task: &TaskRecord) -> bool {
    background_shell_metadata(task)
        .map(|metadata| background_shell_terminal_task_needs_shutdown_retry(task, &metadata))
        .unwrap_or(false)
}

fn background_shell_terminal_task_needs_shutdown_retry(
    task: &TaskRecord,
    metadata: &BackgroundShellTaskMetadata,
) -> bool {
    if !matches!(
        task.status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    ) {
        return false;
    }
    if metadata.process_tree_shutdown_confirmed == Some(true) {
        return false;
    }
    if metadata.process_group_id.is_none()
        && metadata.pid.is_none()
        && metadata.identity_token.is_none()
    {
        return false;
    }
    metadata.stop_requested
        || metadata.recovered_on_boot
        || metadata.terminal_reason.as_deref() == Some("daemon_restarted")
}

pub(crate) fn apply_background_shell_shutdown_outcome(
    metadata: &mut BackgroundShellTaskMetadata,
    outcome: &BackgroundShellShutdownOutcome,
) {
    metadata.process_tree_shutdown_confirmed = Some(outcome.confirmed);
    metadata.process_tree_shutdown_matched_target = Some(outcome.matched_target);
    metadata.process_tree_shutdown_signal_sent = Some(outcome.signal_sent);
    metadata.process_tree_shutdown_timed_out = outcome.timed_out;
    metadata.process_tree_shutdown_supported = Some(outcome.supported);
    metadata.process_tree_remaining_pids = outcome.remaining_pids.clone();
}

fn shell_task_session_prefix(session_id: &str) -> String {
    let digest = Sha256::digest(session_id.as_bytes());
    let mut prefix = String::with_capacity(12);
    for byte in digest.iter().take(6) {
        use std::fmt::Write as _;
        let _ = write!(&mut prefix, "{byte:02x}");
    }
    prefix
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use super::{
        BackgroundShellTaskFinalState, BackgroundShellTaskHandle, TaskService,
        shell_task_session_prefix,
    };
    use crate::shell_tasks::{BackgroundShellTaskRequest, background_shell_metadata};
    use kheish_types::{SessionControlState, TaskStatus};
    use tempfile::tempdir;

    fn background_request() -> BackgroundShellTaskRequest {
        BackgroundShellTaskRequest {
            command: "printf test".to_string(),
            shell: "bash".to_string(),
            workdir: Path::new("/tmp").to_path_buf(),
            description: "test task".to_string(),
            tool_call_id: "call-1".to_string(),
            created_by_run_id: Some("run-1".to_string()),
            started_in_background: true,
            reply_targets: Vec::new(),
        }
    }

    #[test]
    fn next_background_shell_task_id_avoids_collisions() {
        let session_id = "session-1";
        let prefix = shell_task_session_prefix(session_id);
        let tasks = vec![
            kheish_types::TaskRecord {
                id: format!("shell-task-{prefix}-42"),
                title: "one".to_string(),
                description: "one".to_string(),
                status: kheish_types::TaskStatus::Pending,
                owner_agent_id: None,
                blocked_by: Vec::new(),
                blocks: Vec::new(),
                created_at_ms: 0,
                updated_at_ms: 0,
                output: None,
                metadata: serde_json::Value::Null,
            },
            kheish_types::TaskRecord {
                id: format!("shell-task-{prefix}-42-1"),
                title: "two".to_string(),
                description: "two".to_string(),
                status: kheish_types::TaskStatus::Pending,
                owner_agent_id: None,
                blocked_by: Vec::new(),
                blocks: Vec::new(),
                created_at_ms: 0,
                updated_at_ms: 0,
                output: None,
                metadata: serde_json::Value::Null,
            },
        ];

        assert_eq!(
            TaskService::next_background_shell_task_id(session_id, &tasks, &Default::default(), 42),
            format!("shell-task-{prefix}-42-2")
        );
    }

    #[tokio::test]
    async fn task_service_registers_and_stops_background_tasks() {
        let service = TaskService::new();
        let handle = BackgroundShellTaskHandle::new();
        service
            .register_background_shell_task(
                "session-1".to_string(),
                "task-1".to_string(),
                handle.clone(),
            )
            .await;
        assert_eq!(
            service
                .status_snapshot()
                .await
                .live_background_shell_task_count,
            1
        );

        assert!(
            service
                .request_background_shell_stop("session-1", "task-1", Some("cancelled".to_string()))
                .await
        );
        assert!(handle.cancelled.load(Ordering::Relaxed));
        assert_eq!(handle.stop_reason.lock().as_deref(), Some("cancelled"));
        assert!(
            service
                .request_background_shell_stop("session-1", "task-1", Some("duplicate".to_string()))
                .await
        );
        assert_eq!(handle.stop_reason.lock().as_deref(), Some("cancelled"));

        service
            .remove_background_shell_task("session-1", "task-1")
            .await;
        assert_eq!(
            service
                .status_snapshot()
                .await
                .live_background_shell_task_count,
            0
        );
        assert!(
            !service
                .request_background_shell_stop("session-1", "task-1", Some("again".to_string()))
                .await
        );
    }

    #[tokio::test]
    async fn task_service_stop_is_scoped_by_session() {
        let service = TaskService::new();
        let handle = BackgroundShellTaskHandle::new();
        service
            .register_background_shell_task(
                "session-1".to_string(),
                "task-1".to_string(),
                handle.clone(),
            )
            .await;

        assert!(
            !service
                .request_background_shell_stop("session-2", "task-1", Some("wrong".to_string()))
                .await
        );
        assert!(!handle.cancelled.load(Ordering::Relaxed));

        service
            .remove_background_shell_task("session-1", "task-1")
            .await;
    }

    #[tokio::test]
    async fn task_service_first_stop_request_wins_even_without_reason() {
        let service = TaskService::new();
        let handle = BackgroundShellTaskHandle::new();
        service
            .register_background_shell_task(
                "session-1".to_string(),
                "task-1".to_string(),
                handle.clone(),
            )
            .await;

        assert!(
            service
                .request_background_shell_stop("session-1", "task-1", None)
                .await
        );
        assert!(
            service
                .request_background_shell_stop(
                    "session-1",
                    "task-1",
                    Some("late reason".to_string())
                )
                .await
        );
        assert!(handle.cancelled.load(Ordering::Relaxed));
        assert!(handle.stop_reason.lock().is_none());

        service
            .remove_background_shell_task("session-1", "task-1")
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_service_stop_reason_is_visible_when_cancelled_is_observed() {
        for index in 0..100 {
            let service = Arc::new(TaskService::new());
            let handle = BackgroundShellTaskHandle::new();
            let task_id = format!("task-{index}");
            service
                .register_background_shell_task(
                    "session-1".to_string(),
                    task_id.clone(),
                    handle.clone(),
                )
                .await;

            let observer_handle = handle.clone();
            let observer = tokio::spawn(async move {
                loop {
                    if observer_handle.cancelled.load(Ordering::Acquire) {
                        return observer_handle.stop_reason.lock().clone();
                    }
                    tokio::task::yield_now().await;
                }
            });
            let stopper_service = Arc::clone(&service);
            let stopper = tokio::spawn(async move {
                stopper_service
                    .request_background_shell_stop(
                        "session-1",
                        &task_id,
                        Some("operator stop".to_string()),
                    )
                    .await
            });

            assert!(stopper.await.expect("stopper task should not panic"));
            assert_eq!(
                observer
                    .await
                    .expect("observer task should not panic")
                    .as_deref(),
                Some("operator stop")
            );
        }
    }

    #[test]
    fn task_service_inserts_and_detaches_background_shell_tasks() {
        let service = TaskService::new();
        let mut state = SessionControlState::default();
        let task = service.insert_managed_shell_task(
            &mut state,
            "shell-task-1".to_string(),
            "agent-1",
            &background_request(),
            Path::new("/tmp/shell-task-1.log"),
            42,
        );

        assert_eq!(state.tasks.len(), 1);
        assert_eq!(task.id, "shell-task-1");
        assert!(
            service
                .detach_shell_task(&mut state, "shell-task-1", 84)
                .expect("detach task")
        );
        let metadata = background_shell_metadata(&state.tasks[0]).expect("shell task metadata");
        assert!(metadata.started_in_background);
        assert_eq!(state.tasks[0].updated_at_ms, 84);
    }

    #[test]
    fn task_service_finalization_is_terminal_idempotent() {
        let service = TaskService::new();
        let mut state = SessionControlState::default();
        service.insert_managed_shell_task(
            &mut state,
            "shell-task-1".to_string(),
            "agent-1",
            &background_request(),
            Path::new("/tmp/shell-task-1.log"),
            42,
        );

        service
            .finalize_background_shell_task(
                &mut state,
                "shell-task-1",
                Ok(BackgroundShellTaskFinalState {
                    exit_code: Some(0),
                    output_size_bytes: 7,
                    output_total_bytes: 7,
                    output_rotated: false,
                    output_rotation_count: 0,
                    cancelled: false,
                    killed_for_size: false,
                    interactive_prompt_detected: false,
                    stop_reason: None,
                    ..Default::default()
                }),
                None,
                84,
                false,
            )
            .expect("first finalize")
            .expect("first finalized");

        let duplicate = service
            .finalize_background_shell_task(
                &mut state,
                "shell-task-1",
                Ok(BackgroundShellTaskFinalState {
                    exit_code: Some(1),
                    output_size_bytes: 11,
                    output_total_bytes: 11,
                    output_rotated: true,
                    output_rotation_count: 1,
                    cancelled: false,
                    killed_for_size: false,
                    interactive_prompt_detected: false,
                    stop_reason: None,
                    ..Default::default()
                }),
                None,
                100,
                false,
            )
            .expect("duplicate finalize");

        assert!(duplicate.is_none());
        assert_eq!(state.tasks[0].status, TaskStatus::Completed);
        assert_eq!(state.tasks[0].updated_at_ms, 84);
        let metadata = background_shell_metadata(&state.tasks[0]).expect("shell task metadata");
        assert_eq!(metadata.exit_code, Some(0));
        assert!(!metadata.output_rotated);
    }

    #[test]
    fn task_service_finalizes_background_shell_tasks() {
        let service = TaskService::new();
        let mut state = SessionControlState::default();
        service.insert_managed_shell_task(
            &mut state,
            "shell-task-1".to_string(),
            "agent-1",
            &background_request(),
            Path::new("/tmp/shell-task-1.log"),
            42,
        );

        let finalized = service
            .finalize_background_shell_task(
                &mut state,
                "shell-task-1",
                Ok(BackgroundShellTaskFinalState {
                    exit_code: Some(0),
                    output_size_bytes: 7,
                    output_total_bytes: 7,
                    output_rotated: false,
                    output_rotation_count: 0,
                    cancelled: false,
                    killed_for_size: false,
                    interactive_prompt_detected: false,
                    stop_reason: None,
                    ..Default::default()
                }),
                None,
                84,
                false,
            )
            .expect("finalize task")
            .expect("finalized task");

        assert!(finalized.notify_on_completion);
        assert_eq!(finalized.task.status, TaskStatus::Completed);
        assert_eq!(finalized.metadata.exit_code, Some(0));
        assert_eq!(finalized.metadata.output_size_bytes, Some(7));
        assert_eq!(state.tasks[0].updated_at_ms, 84);
    }

    #[test]
    fn task_service_cancel_persisted_task_marks_shell_metadata_cancelled() {
        let service = TaskService::new();
        let mut state = SessionControlState::default();
        service.insert_managed_shell_task(
            &mut state,
            "shell-task-1".to_string(),
            "agent-1",
            &background_request(),
            Path::new("/tmp/shell-task-1.log"),
            42,
        );

        let updated = service
            .cancel_persisted_task(&mut state, "shell-task-1", Some("stop".to_string()), 84)
            .expect("cancel task");

        assert_eq!(updated.status, TaskStatus::Cancelled);
        let metadata = background_shell_metadata(&updated).expect("shell task metadata");
        assert!(metadata.cancelled);
    }

    #[test]
    fn task_service_retries_terminal_shell_shutdown_after_crash_window() {
        let service = TaskService::new();
        let mut state = SessionControlState::default();
        service.insert_managed_shell_task(
            &mut state,
            "shell-task-1".to_string(),
            "agent-1",
            &background_request(),
            Path::new("/tmp/shell-task-1.log"),
            42,
        );
        service
            .update_background_shell_metadata(&mut state, "shell-task-1", 43, |metadata| {
                metadata.pid = Some(1234);
                metadata.process_group_id = Some(1234);
                metadata.identity_token = Some("shell-task-1".to_string());
            })
            .expect("update metadata");
        service
            .cancel_persisted_task(&mut state, "shell-task-1", Some("stop".to_string()), 84)
            .expect("cancel task");

        let recovered = service
            .restore_background_shell_tasks_on_boot(&mut state, 126)
            .expect("restore tasks");

        assert_eq!(recovered.len(), 1);
        assert!(!recovered[0].notify_on_completion);
        assert_eq!(recovered[0].task.status, TaskStatus::Cancelled);
        assert_eq!(state.tasks[0].status, TaskStatus::Cancelled);
    }

    #[test]
    fn task_service_does_not_retry_completed_shell_shutdown_without_stop_or_restart() {
        let service = TaskService::new();
        let mut state = SessionControlState::default();
        service.insert_managed_shell_task(
            &mut state,
            "shell-task-1".to_string(),
            "agent-1",
            &background_request(),
            Path::new("/tmp/shell-task-1.log"),
            42,
        );
        service
            .finalize_background_shell_task(
                &mut state,
                "shell-task-1",
                Ok(BackgroundShellTaskFinalState {
                    exit_code: Some(0),
                    ..Default::default()
                }),
                None,
                84,
                false,
            )
            .expect("finalize task");

        let recovered = service
            .restore_background_shell_tasks_on_boot(&mut state, 126)
            .expect("restore tasks");

        assert!(recovered.is_empty());
        assert_eq!(state.tasks[0].status, TaskStatus::Completed);
    }

    #[test]
    fn task_service_restores_interrupted_background_shell_tasks_on_boot() {
        let service = TaskService::new();
        let temp = tempdir().expect("tempdir");
        let output_path = temp.path().join("shell-task-1.log");
        std::fs::write(
            &output_path,
            "\n[daemon output rotated: retained latest output after 9000000 total bytes; rotation=2]\nTAIL",
        )
        .expect("write output");
        let mut state = SessionControlState::default();
        service.insert_managed_shell_task(
            &mut state,
            "shell-task-1".to_string(),
            "agent-1",
            &background_request(),
            &output_path,
            42,
        );

        let recovered = service
            .restore_background_shell_tasks_on_boot(&mut state, 84)
            .expect("restore tasks");
        assert_eq!(recovered.len(), 1);
        assert!(recovered[0].notify_on_completion);
        assert_eq!(state.tasks[0].status, TaskStatus::Failed);
        let metadata = background_shell_metadata(&state.tasks[0]).expect("shell task metadata");
        assert!(!metadata.cancelled);
        assert_eq!(
            metadata.terminal_reason.as_deref(),
            Some("daemon_restarted")
        );
        assert!(metadata.recovered_on_boot);
        assert_eq!(
            metadata.output_size_bytes,
            Some(std::fs::metadata(&output_path).expect("metadata").len())
        );
        assert_eq!(metadata.output_total_bytes, Some(9_000_000));
        assert!(metadata.output_rotated);
        assert_eq!(metadata.output_rotation_count, 2);
        assert!(
            state.tasks[0]
                .output
                .as_deref()
                .unwrap_or_default()
                .contains("did not complete because the daemon restarted")
        );
        assert_eq!(state.tasks[0].updated_at_ms, 84);
    }

    #[test]
    fn next_background_shell_task_id_is_unique_across_sessions() {
        let tasks = Vec::new();

        let left = TaskService::next_background_shell_task_id(
            "session-1",
            &tasks,
            &Default::default(),
            42,
        );
        let right = TaskService::next_background_shell_task_id(
            "session-2",
            &tasks,
            &Default::default(),
            42,
        );

        assert_ne!(left, right);
    }

    #[tokio::test]
    async fn task_service_task_output_view_reports_not_ready_for_live_tasks() {
        let service = TaskService::new();
        let mut state = SessionControlState::default();
        service.insert_managed_shell_task(
            &mut state,
            "shell-task-1".to_string(),
            "agent-1",
            &background_request(),
            Path::new("/tmp/shell-task-1.log"),
            42,
        );

        let view = service
            .task_output_view(
                || async { Ok(state.clone()) },
                "shell-task-1",
                false,
                Duration::from_millis(0),
                1024,
                false,
            )
            .await
            .expect("task output view");

        assert_eq!(view.retrieval_status, "not_ready");
        assert_eq!(view.task.status, TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn task_service_task_output_success_can_wrap_failed_task_with_partial_output() {
        let temp = tempdir().expect("tempdir");
        let output_path = temp.path().join("shell-task-1.log");
        std::fs::write(&output_path, "partial-before-restart\n").expect("write output");
        let service = TaskService::new();
        let mut state = SessionControlState::default();
        service.insert_managed_shell_task(
            &mut state,
            "shell-task-1".to_string(),
            "agent-1",
            &background_request(),
            &output_path,
            42,
        );
        service
            .restore_background_shell_tasks_on_boot(&mut state, 84)
            .expect("restore tasks");

        let view = service
            .task_output_view(
                || async { Ok(state.clone()) },
                "shell-task-1",
                true,
                Duration::from_millis(0),
                1024,
                true,
            )
            .await
            .expect("task output view");

        assert_eq!(view.retrieval_status, "success");
        assert_eq!(view.task.status, TaskStatus::Failed);
        assert_eq!(
            view.task.metadata["terminal_reason"],
            serde_json::json!("daemon_restarted")
        );
        assert!(
            view.output_text
                .as_deref()
                .unwrap_or_default()
                .contains("partial-before-restart")
        );
    }

    #[tokio::test]
    async fn task_service_wait_for_task_settle_returns_latest_snapshot_on_timeout() {
        let service = TaskService::new();
        let mut state = SessionControlState::default();
        service.insert_managed_shell_task(
            &mut state,
            "shell-task-1".to_string(),
            "agent-1",
            &background_request(),
            Path::new("/tmp/shell-task-1.log"),
            42,
        );

        let task = service
            .wait_for_task_settle(
                || async { Ok(state.clone()) },
                "shell-task-1",
                Duration::from_millis(0),
            )
            .await
            .expect("wait for task settle");

        assert_eq!(task.status, TaskStatus::InProgress);
    }
}
