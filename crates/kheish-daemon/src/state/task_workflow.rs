//! Managed shell-task workflow methods implemented on [`DaemonState`].

use super::*;
use std::fs;

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(super) async fn start_background_shell_task(
        self: &Arc<Self>,
        session_id: &str,
        owner_agent_id: &str,
        request: BackgroundShellTaskRequest,
    ) -> Result<kheish_types::TaskRecord> {
        self.start_managed_shell_task(session_id, owner_agent_id, request)
            .await
    }

    async fn start_managed_shell_task(
        self: &Arc<Self>,
        session_id: &str,
        owner_agent_id: &str,
        mut request: BackgroundShellTaskRequest,
    ) -> Result<kheish_types::TaskRecord> {
        self.agent_id_for_session(session_id).await?;
        if request.reply_targets.is_empty() {
            request.reply_targets = self.session_reply_targets(session_id).await;
        }
        let mut state = self.load_session_control_state(session_id).await?;
        let now = now_ms();
        let archived = self.archived_session_task_index(session_id).await?;
        let task_id = TaskService::next_background_shell_task_id(
            session_id,
            &state.tasks,
            &archived.ids,
            now,
        );
        let output_path = self.store.shell_task_output_path(&task_id);
        self.store.ensure_parent_dir(&output_path)?;
        self.task_service.insert_managed_shell_task(
            &mut state,
            task_id.clone(),
            owner_agent_id,
            &request,
            &output_path,
            now,
        );
        self.task_service.update_background_shell_metadata(
            &mut state,
            &task_id,
            now,
            |metadata| {
                metadata.identity_token = Some(task_id.clone());
            },
        )?;
        self.save_session_control_state(session_id, state).await?;

        let child = match self
            .spawn_background_shell_child(&request, &output_path, &task_id)
            .await
        {
            Ok(child) => child,
            Err(error) => {
                let error_text = error.to_string();
                if let Err(finalize_error) = self
                    .finish_background_shell_task(
                        session_id,
                        owner_agent_id,
                        &task_id,
                        Err(error_text),
                        Some(output_path.clone()),
                        false,
                    )
                    .await
                {
                    tracing::warn!(
                        session_id = %session_id,
                        task_id = %task_id,
                        error = ?finalize_error,
                        "failed to persist failed background shell launch"
                    );
                }
                return Err(error);
            }
        };
        let pid = child.id();
        let process_group_id = pid.and_then(background_shell_process_group_id);
        let process_started_at = pid.and_then(background_shell_process_started_at);
        let mut state = self.load_session_control_state(session_id).await?;
        let task_status_after_spawn = state
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .map(|task| task.status.clone());
        if !matches!(
            task_status_after_spawn,
            Some(kheish_types::TaskStatus::Pending | kheish_types::TaskStatus::InProgress)
        ) {
            #[cfg(unix)]
            let shutdown_outcome = shutdown_background_shell_processes(
                process_group_id,
                pid,
                BackgroundShellShutdownGuard {
                    expected_command: Some(request.command.as_str()),
                    expected_process_started_at: process_started_at.as_deref(),
                    expected_task_id: Some(&task_id),
                },
            )
            .await;
            #[cfg(not(unix))]
            {
                let mut child = child;
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
            #[cfg(unix)]
            self.task_service.update_background_shell_metadata(
                &mut state,
                &task_id,
                now_ms(),
                |metadata| {
                    metadata.pid = pid;
                    metadata.process_group_id = process_group_id;
                    metadata.process_started_at = process_started_at.clone();
                    metadata.identity_token = Some(task_id.clone());
                    apply_background_shell_shutdown_outcome(metadata, &shutdown_outcome);
                },
            )?;
            let saved = self.save_session_control_state(session_id, state).await?;
            return saved
                .tasks
                .into_iter()
                .find(|task| task.id == task_id)
                .ok_or_else(|| anyhow!("unknown task {task_id} after stopped launch"));
        }
        if let Err(error) = self.task_service.update_background_shell_metadata(
            &mut state,
            &task_id,
            now_ms(),
            |metadata| {
                metadata.pid = pid;
                metadata.process_group_id = process_group_id;
                metadata.process_started_at = process_started_at.clone();
                metadata.identity_token = Some(task_id.clone());
            },
        ) {
            #[cfg(unix)]
            let _ = shutdown_background_shell_processes(
                process_group_id,
                pid,
                BackgroundShellShutdownGuard {
                    expected_command: Some(request.command.as_str()),
                    expected_process_started_at: process_started_at.as_deref(),
                    expected_task_id: Some(&task_id),
                },
            )
            .await;
            #[cfg(not(unix))]
            {
                let mut child = child;
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
            return Err(error);
        }
        let saved = match self.save_session_control_state(session_id, state).await {
            Ok(saved) => saved,
            Err(error) => {
                #[cfg(unix)]
                let _ = shutdown_background_shell_processes(
                    process_group_id,
                    pid,
                    BackgroundShellShutdownGuard {
                        expected_command: Some(request.command.as_str()),
                        expected_process_started_at: process_started_at.as_deref(),
                        expected_task_id: Some(&task_id),
                    },
                )
                .await;
                #[cfg(not(unix))]
                {
                    let mut child = child;
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
                return Err(error);
            }
        };
        let task = saved
            .tasks
            .into_iter()
            .find(|task| task.id == task_id)
            .ok_or_else(|| anyhow!("unknown task {task_id} after launch"))?;
        if !matches!(
            task.status,
            kheish_types::TaskStatus::Pending | kheish_types::TaskStatus::InProgress
        ) {
            #[cfg(unix)]
            let shutdown_outcome = shutdown_background_shell_processes(
                process_group_id,
                pid,
                BackgroundShellShutdownGuard {
                    expected_command: Some(request.command.as_str()),
                    expected_process_started_at: process_started_at.as_deref(),
                    expected_task_id: Some(&task_id),
                },
            )
            .await;
            #[cfg(not(unix))]
            {
                let mut child = child;
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
            #[cfg(unix)]
            {
                let mut state = self.load_session_control_state(session_id).await?;
                self.task_service.update_background_shell_metadata(
                    &mut state,
                    &task_id,
                    now_ms(),
                    |metadata| apply_background_shell_shutdown_outcome(metadata, &shutdown_outcome),
                )?;
                let saved = self.save_session_control_state(session_id, state).await?;
                if let Some(task) = saved.tasks.into_iter().find(|task| task.id == task_id) {
                    return Ok(task);
                }
            }
            return Ok(task);
        }

        let handle = BackgroundShellTaskHandle::new();
        self.task_service
            .register_background_shell_task(session_id.to_string(), task_id.clone(), handle.clone())
            .await;
        self.spawn_background_shell_task_runner(
            session_id.to_string(),
            owner_agent_id.to_string(),
            task_id,
            output_path,
            child,
            request.command.clone(),
            handle,
        );
        Ok(task)
    }

    pub(super) async fn run_foreground_shell_task(
        self: &Arc<Self>,
        session_id: &str,
        owner_agent_id: &str,
        request: BackgroundShellTaskRequest,
    ) -> Result<ToolExecutionOutput> {
        let task = self
            .start_managed_shell_task(session_id, owner_agent_id, request.clone())
            .await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60 * 60);
        loop {
            let view = self
                .task_output_view(
                    session_id,
                    &task.id,
                    false,
                    Duration::from_millis(0),
                    64 * 1024,
                    true,
                )
                .await?;
            if !matches!(
                view.task.status,
                kheish_types::TaskStatus::Pending | kheish_types::TaskStatus::InProgress
            ) {
                let metadata = background_shell_metadata(&view.task)
                    .ok_or_else(|| anyhow!("task {} is not a managed shell task", task.id))?;
                let output = view.output_text.or(view.output_excerpt).unwrap_or_default();
                let success = matches!(view.task.status, kheish_types::TaskStatus::Completed);
                return Ok(ToolExecutionOutput::json(json!({
                    "command": request.command,
                    "workdir": request.workdir.display().to_string(),
                    "exit_code": metadata.exit_code,
                    "success": success,
                    "stdout": output,
                    "stderr": "",
                    "task_id": task.id,
                    "output_truncated": view.output_truncated,
                })));
            }
            if let Some(cancellation) = current_cancellation_token() {
                if cancellation.is_cancelled()
                    && self.detach_shell_task(session_id, &task.id).await?
                {
                    return Err(interrupted_error());
                }
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for managed shell task {}",
                task.id
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn detach_shell_task(&self, session_id: &str, task_id: &str) -> Result<bool> {
        let mut state = self.load_session_control_state(session_id).await?;
        let detached = self
            .task_service
            .detach_shell_task(&mut state, task_id, now_ms())?;
        if !detached {
            return Ok(false);
        }
        self.save_session_control_state(session_id, state).await?;
        Ok(true)
    }

    fn spawn_background_shell_task_runner(
        self: &Arc<Self>,
        session_id: String,
        owner_agent_id: String,
        task_id: String,
        output_path: PathBuf,
        child: tokio::process::Child,
        expected_command: String,
        handle: BackgroundShellTaskHandle,
    ) {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let result = state
                .run_background_shell_task(
                    session_id.clone(),
                    owner_agent_id.clone(),
                    task_id.clone(),
                    output_path,
                    child,
                    expected_command,
                    handle.clone(),
                )
                .await;
            state
                .task_service
                .remove_background_shell_task(&session_id, &task_id)
                .await;
            if let Err(error) = result {
                let _ = state
                    .finish_background_shell_task(
                        &session_id,
                        &owner_agent_id,
                        &task_id,
                        Err(error.to_string()),
                        None,
                        true,
                    )
                    .await;
            }
        });
    }

    async fn run_background_shell_task(
        self: &Arc<Self>,
        session_id: String,
        owner_agent_id: String,
        task_id: String,
        output_path: PathBuf,
        mut child: tokio::process::Child,
        expected_command: String,
        handle: BackgroundShellTaskHandle,
    ) -> Result<()> {
        let output_file = Arc::new(tokio::sync::Mutex::new(ShellTaskOutputWriter::new(
            output_path.clone(),
        )));
        let pid = child.id();
        let process_group_id = pid.and_then(background_shell_process_group_id);
        let process_started_at = pid.and_then(background_shell_process_started_at);

        let (collector_error_tx, mut collector_error_rx) =
            tokio::sync::mpsc::unbounded_channel::<String>();
        let stdout_task = spawn_pipe_collector(
            child.stdout.take(),
            output_file.clone(),
            collector_error_tx.clone(),
        );
        let stderr_task =
            spawn_pipe_collector(child.stderr.take(), output_file.clone(), collector_error_tx);

        let mut last_growth_size = 0u64;
        let mut last_growth_at = tokio::time::Instant::now();
        let mut interactive_prompt_detected = false;
        let mut cancelled = false;
        let mut shutdown_requested = false;
        let mut child_exit_code = None;
        let mut last_persisted_output_stats = ShellTaskOutputStats::default();
        let mut shutdown_outcome = None;
        let mut output_capture_error = None;
        let exit_code = loop {
            if output_capture_error.is_none()
                && let Ok(error) = collector_error_rx.try_recv()
            {
                output_capture_error = Some(error);
                if !shutdown_requested {
                    shutdown_requested = true;
                    #[cfg(unix)]
                    {
                        shutdown_outcome = Some(
                            shutdown_background_shell_processes(
                                process_group_id,
                                pid,
                                BackgroundShellShutdownGuard {
                                    expected_command: Some(expected_command.as_str()),
                                    expected_process_started_at: process_started_at.as_deref(),
                                    expected_task_id: Some(&task_id),
                                },
                            )
                            .await,
                        );
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = child.kill().await;
                    }
                }
            }
            if child_exit_code.is_none()
                && let Some(status) = child.try_wait()?
            {
                child_exit_code = Some(status.code());
                if !shutdown_requested {
                    #[cfg(unix)]
                    {
                        let outcome = shutdown_background_shell_processes(
                            process_group_id,
                            pid,
                            BackgroundShellShutdownGuard {
                                expected_command: Some(expected_command.as_str()),
                                expected_process_started_at: process_started_at.as_deref(),
                                expected_task_id: Some(&task_id),
                            },
                        )
                        .await;
                        if outcome.matched_target || outcome.signal_sent {
                            shutdown_requested = true;
                            shutdown_outcome = Some(outcome);
                        }
                    }
                }
            }
            if child_exit_code.is_some() && stdout_task.is_finished() && stderr_task.is_finished() {
                break child_exit_code.flatten();
            }
            if handle.cancelled.load(Ordering::Acquire) {
                cancelled = true;
                if !shutdown_requested {
                    shutdown_requested = true;
                    #[cfg(unix)]
                    {
                        shutdown_outcome = Some(
                            shutdown_background_shell_processes(
                                process_group_id,
                                pid,
                                BackgroundShellShutdownGuard {
                                    expected_command: Some(expected_command.as_str()),
                                    expected_process_started_at: process_started_at.as_deref(),
                                    expected_task_id: Some(&task_id),
                                },
                            )
                            .await,
                        );
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = child.kill().await;
                    }
                }
            }

            let progress = read_task_output_progress(&output_path)
                .await
                .unwrap_or_default();
            if let Ok(output_stats) = output_file.lock().await.stats().await {
                if output_stats != last_persisted_output_stats {
                    if let Err(error) = self
                        .persist_background_shell_output_stats(&session_id, &task_id, &output_stats)
                        .await
                    {
                        tracing::warn!(
                            session_id = %session_id,
                            task_id = %task_id,
                            error = ?error,
                            "failed to persist background shell output stats"
                        );
                    } else {
                        last_persisted_output_stats = output_stats;
                    }
                }
            }
            if progress.size_bytes != last_growth_size {
                last_growth_size = progress.size_bytes;
                last_growth_at = tokio::time::Instant::now();
            } else if !interactive_prompt_detected
                && last_growth_at.elapsed() >= BACKGROUND_SHELL_STALL_THRESHOLD
                && looks_like_interactive_prompt(&progress.tail_excerpt)
            {
                interactive_prompt_detected = true;
                let _ = self
                    .notify_background_shell_task(
                        &session_id,
                        &owner_agent_id,
                        &task_id,
                        "background_shell_stalled",
                        format!(
                            "Background shell task {task_id} appears to be waiting for interactive input. Inspect it with task_output before retrying."
                        ),
                        json!({
                            "task_id": task_id,
                            "state": "interactive_prompt_detected",
                            "output_file_path": output_path.display().to_string(),
                        }),
                    )
                    .await;
            }
            if shutdown_requested {
                tokio::time::sleep(Duration::from_millis(50)).await;
            } else {
                tokio::select! {
                    _ = tokio::time::sleep(BACKGROUND_SHELL_WATCHDOG_INTERVAL) => {}
                    _ = handle.wake.notified() => {}
                }
            }
        };

        if let Err(error) = await_pipe_collector(stdout_task).await
            && output_capture_error.is_none()
        {
            output_capture_error = Some(error.to_string());
        }
        if let Err(error) = await_pipe_collector(stderr_task).await
            && output_capture_error.is_none()
        {
            output_capture_error = Some(error.to_string());
        }

        #[cfg(unix)]
        if !shutdown_requested
            && background_shell_shutdown_targets_visible(process_group_id, pid, Some(&task_id))
        {
            shutdown_outcome = Some(
                shutdown_background_shell_processes(
                    process_group_id,
                    pid,
                    BackgroundShellShutdownGuard {
                        expected_command: Some(expected_command.as_str()),
                        expected_process_started_at: process_started_at.as_deref(),
                        expected_task_id: Some(&task_id),
                    },
                )
                .await,
            );
        }

        let output_stats = output_file.lock().await.stats().await.unwrap_or_default();
        let stop_reason = handle.stop_reason.lock().clone();
        self.finish_background_shell_task(
            &session_id,
            &owner_agent_id,
            &task_id,
            Ok(BackgroundShellTaskFinalState {
                exit_code,
                output_size_bytes: output_stats.retained_size_bytes,
                output_total_bytes: output_stats.total_size_bytes,
                output_rotated: output_stats.rotated,
                output_rotation_count: output_stats.rotation_count,
                cancelled,
                killed_for_size: false,
                interactive_prompt_detected,
                stop_reason,
                shutdown_outcome,
                output_capture_error,
            }),
            Some(output_path),
            false,
        )
        .await
    }

    async fn spawn_background_shell_child(
        &self,
        request: &BackgroundShellTaskRequest,
        output_path: &Path,
        task_id: &str,
    ) -> Result<tokio::process::Child> {
        self.store.ensure_parent_dir(output_path)?;
        let mut command = Command::new(&request.shell);
        command
            .arg("-lc")
            .arg(&request.command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env(BACKGROUND_SHELL_TASK_ID_ENV, task_id);
        let _workdir_guard = configure_resolved_bash_command_workdir(
            &mut command,
            &self.workspace_root,
            &request.workdir,
        )?;
        configure_background_shell_command(&mut command);
        command.spawn().with_context(|| {
            format!(
                "failed to execute background shell in {}",
                request.workdir.display()
            )
        })
    }

    async fn finish_background_shell_task(
        self: &Arc<Self>,
        session_id: &str,
        owner_agent_id: &str,
        task_id: &str,
        outcome: std::result::Result<BackgroundShellTaskFinalState, String>,
        output_path: Option<PathBuf>,
        preserve_existing_terminal_state: bool,
    ) -> Result<()> {
        let mut state = self.load_session_control_state(session_id).await?;
        let finalized = self.task_service.finalize_background_shell_task(
            &mut state,
            task_id,
            outcome,
            output_path
                .as_ref()
                .and_then(|path| fs::metadata(path).ok())
                .map(|metadata| metadata.len()),
            now_ms(),
            preserve_existing_terminal_state,
        )?;
        let Some(FinalizedBackgroundShellTask {
            task: completed,
            metadata,
            notify_on_completion,
        }) = finalized
        else {
            return Ok(());
        };
        self.save_session_control_state(session_id, state).await?;
        if notify_on_completion {
            let summary = completed.output.clone().unwrap_or_default();
            self.emit_session_output(
                session_id,
                None,
                RichOutput::text(format!("Task {}: {}", completed.id, summary)),
                Some(metadata.reply_targets.clone()),
            )
            .await?;
            self.notify_background_shell_task(
                session_id,
                owner_agent_id,
                &completed.id,
                "background_shell_completed",
                format!(
                    "Background shell task {} is now {}.\n{}",
                    completed.id,
                    render_task_status(&completed.status),
                    summary
                ),
                json!({
                    "task": completed,
                    "output_file_path": output_path.map(|path| path.display().to_string()),
                }),
            )
            .await?;
        }
        Ok(())
    }

    async fn notify_background_shell_task(
        self: &Arc<Self>,
        session_id: &str,
        owner_agent_id: &str,
        task_id: &str,
        subject: &str,
        message: String,
        payload: Value,
    ) -> Result<()> {
        let _ = self
            .post_mailbox(PostMailboxRequest {
                message_id: None,
                from_agent_id: "supervisor".to_string(),
                to_agent_id: owner_agent_id.to_string(),
                subject: subject.to_string(),
                ttl_ms: None,
                payload: json!({
                    "task_id": task_id,
                    "message": message,
                    "session_id": session_id,
                    "payload": payload,
                }),
            })
            .await;
        Ok(())
    }

    async fn persist_background_shell_output_stats(
        &self,
        session_id: &str,
        task_id: &str,
        output_stats: &ShellTaskOutputStats,
    ) -> Result<()> {
        let mut state = self.load_session_control_state(session_id).await?;
        let updated = self.task_service.update_background_shell_metadata(
            &mut state,
            task_id,
            now_ms(),
            |metadata| {
                metadata.output_size_bytes = Some(output_stats.retained_size_bytes);
                metadata.output_total_bytes = Some(output_stats.total_size_bytes);
                metadata.output_rotated = output_stats.rotated;
                metadata.output_rotation_count = output_stats.rotation_count;
            },
        )?;
        if updated {
            self.save_session_control_state(session_id, state).await?;
        }
        Ok(())
    }

    pub(crate) async fn task_output_view(
        &self,
        session_id: &str,
        task_id: &str,
        wait: bool,
        timeout: Duration,
        tail_bytes: usize,
        include_full_output: bool,
    ) -> Result<TaskOutputView> {
        self.task_service
            .task_output_view(
                || self.load_session_control_state_resolving_task(session_id, task_id),
                task_id,
                wait,
                timeout,
                tail_bytes,
                include_full_output,
            )
            .await
    }

    /// Loads the hot control state, resolving the given task from the archive
    /// when it is no longer live. Settle-waits and output views keep working
    /// after a terminal task leaves the hot state; the archive is only read
    /// on a miss.
    async fn load_session_control_state_resolving_task(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<SessionControlState> {
        let mut state = self.load_session_control_state(session_id).await?;
        if !state.tasks.iter().any(|task| task.id == task_id)
            && let Some(task) = self.find_archived_session_task(session_id, task_id).await?
        {
            state.tasks.push(task);
        }
        Ok(state)
    }

    pub(crate) async fn stop_session_task(
        self: &Arc<Self>,
        session_id: &str,
        task_id: &str,
        reason: Option<String>,
        actor_agent_id: Option<String>,
    ) -> Result<kheish_types::TaskRecord> {
        if self
            .task_service
            .request_background_shell_stop(session_id, task_id, reason.clone())
            .await
        {
            let task = self
                .task_service
                .wait_for_task_settle(
                    || self.load_session_control_state_resolving_task(session_id, task_id),
                    task_id,
                    Duration::from_secs(5),
                )
                .await?;
            if matches!(
                task.status,
                kheish_types::TaskStatus::Pending | kheish_types::TaskStatus::InProgress
            ) && background_shell_metadata(&task).is_some()
            {
                let mut state = self.load_session_control_state(session_id).await?;
                if let Some(current) = state.tasks.iter().find(|task| task.id == task_id)
                    && !matches!(
                        current.status,
                        kheish_types::TaskStatus::Pending | kheish_types::TaskStatus::InProgress
                    )
                {
                    return Ok(current.clone());
                }
                self.task_service.update_background_shell_metadata(
                    &mut state,
                    task_id,
                    now_ms(),
                    |metadata| {
                        metadata.stop_requested = true;
                        metadata.stop_requested_at_ms = Some(now_ms());
                        metadata.stop_reason = reason.clone();
                    },
                )?;
                if let Some(task) = state.tasks.iter_mut().find(|task| task.id == task_id) {
                    task.output = Some(
                        "Stop requested; background shell task is still shutting down. Use task_output with wait=true to observe the terminal state."
                            .to_string(),
                    );
                }
                let saved = self.save_session_control_state(session_id, state).await?;
                return saved
                    .tasks
                    .into_iter()
                    .find(|task| task.id == task_id)
                    .ok_or_else(|| anyhow!("unknown task {task_id}"));
            }
            return Ok(task);
        }

        let mut state = self.load_session_control_state(session_id).await?;
        let Some(task_before) = state.tasks.iter().find(|task| task.id == task_id).cloned() else {
            // An archived task is already terminal; stopping it is a no-op
            // that returns the frozen snapshot.
            return self
                .find_archived_session_task(session_id, task_id)
                .await?
                .ok_or_else(|| anyhow!("unknown task {task_id}"));
        };
        let was_terminal = matches!(
            task_before.status,
            kheish_types::TaskStatus::Completed
                | kheish_types::TaskStatus::Failed
                | kheish_types::TaskStatus::Cancelled
        );
        let shell_metadata_before = background_shell_metadata(&task_before);
        let updated = self.task_service.cancel_persisted_task(
            &mut state,
            task_id,
            reason.clone(),
            now_ms(),
        )?;
        self.save_session_control_state(session_id, state).await?;
        let mut updated = updated;
        if !was_terminal && let Some(metadata) = shell_metadata_before.as_ref() {
            let shutdown_outcome = shutdown_background_shell_processes(
                metadata.process_group_id,
                metadata.pid,
                BackgroundShellShutdownGuard {
                    expected_command: Some(metadata.command.as_str()),
                    expected_process_started_at: metadata.process_started_at.as_deref(),
                    expected_task_id: metadata.identity_token.as_deref(),
                },
            )
            .await;
            let mut state = self.load_session_control_state(session_id).await?;
            self.task_service.update_background_shell_metadata(
                &mut state,
                task_id,
                now_ms(),
                |metadata| apply_background_shell_shutdown_outcome(metadata, &shutdown_outcome),
            )?;
            let saved = self.save_session_control_state(session_id, state).await?;
            if let Some(saved_task) = saved.tasks.into_iter().find(|task| task.id == task_id) {
                updated = saved_task;
            }
        }
        if let Some(owner_agent_id) = updated.owner_agent_id.as_ref() {
            if actor_agent_id.as_deref() != Some(owner_agent_id.as_str()) {
                let _ = self
                    .post_mailbox(PostMailboxRequest {
                        message_id: None,
                        from_agent_id: actor_agent_id.unwrap_or_else(|| "supervisor".to_string()),
                        to_agent_id: owner_agent_id.clone(),
                        subject: "task_completed".to_string(),
                        ttl_ms: None,
                        payload: json!({
                            "task_id": updated.id,
                            "message": updated.output.clone(),
                            "session_id": session_id,
                        }),
                    })
                    .await;
            }
        }
        Ok(updated)
    }

    pub(crate) async fn restore_background_shell_tasks_on_boot(self: &Arc<Self>) -> Result<()> {
        let session_ids: Vec<String> = self
            .session_service
            .index()
            .lock()
            .await
            .sessions
            .keys()
            .cloned()
            .collect();
        for session_id in session_ids {
            let mut state = self.load_session_control_state(&session_id).await?;
            let recovered = self
                .task_service
                .restore_background_shell_tasks_on_boot(&mut state, now_ms())?;
            if recovered.is_empty() {
                continue;
            }
            self.save_session_control_state(&session_id, state).await?;
            for mut finalized in recovered {
                let shutdown_outcome = shutdown_background_shell_processes(
                    finalized.metadata.process_group_id,
                    finalized.metadata.pid,
                    BackgroundShellShutdownGuard {
                        expected_command: Some(finalized.metadata.command.as_str()),
                        expected_process_started_at: finalized
                            .metadata
                            .process_started_at
                            .as_deref(),
                        expected_task_id: finalized.metadata.identity_token.as_deref(),
                    },
                )
                .await;
                let mut state = self.load_session_control_state(&session_id).await?;
                self.task_service.update_background_shell_metadata(
                    &mut state,
                    &finalized.task.id,
                    now_ms(),
                    |metadata| apply_background_shell_shutdown_outcome(metadata, &shutdown_outcome),
                )?;
                self.save_session_control_state(&session_id, state).await?;
                apply_background_shell_shutdown_outcome(&mut finalized.metadata, &shutdown_outcome);
                finalized.task.metadata = serde_json::to_value(&finalized.metadata)?;
                if !finalized.notify_on_completion {
                    continue;
                }
                let summary = finalized.task.output.clone().unwrap_or_default();
                self.emit_session_output(
                    &session_id,
                    None,
                    RichOutput::text(format!("Task {}: {}", finalized.task.id, summary)),
                    Some(finalized.metadata.reply_targets.clone()),
                )
                .await?;
                if let Some(owner_agent_id) = finalized.task.owner_agent_id.as_deref() {
                    self.notify_background_shell_task(
                        &session_id,
                        owner_agent_id,
                        &finalized.task.id,
                        "background_shell_interrupted",
                        format!(
                            "Background shell task {} is now {}.\n{}",
                            finalized.task.id,
                            render_task_status(&finalized.task.status),
                            summary
                        ),
                        json!({ "task": finalized.task }),
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }
}
