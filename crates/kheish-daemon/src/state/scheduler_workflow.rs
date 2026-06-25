//! Scheduler workflow methods implemented on `DaemonState`.

use super::*;

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(crate) fn spawn_schedule_worker(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            state.scheduler_worker_loop().await;
        })
    }

    pub(crate) async fn restore_schedule_worker_on_boot(self: &Arc<Self>) -> Result<()> {
        self.reconcile_schedules_on_boot().await?;
        self.schedule_service.notify().notify_waiters();
        Ok(())
    }

    pub(crate) async fn create_schedule(
        self: &Arc<Self>,
        mut request: ScheduleCreateRequest,
    ) -> Result<ScheduleView> {
        validate_schedule_create_request(&request)?;
        let target_agent = self
            .agent_id_for_session(&request.target_session_id)
            .await?;
        if let Some(input_request) = request.request.as_mut() {
            if contains_flow_metadata(&input_request.metadata) {
                anyhow::bail!("metadata key `{KHEISH_FLOW_METADATA_KEY}` is daemon-owned");
            }
            self.ensure_submit_input_request_has_payload(input_request)?;
            self.validate_submit_input_request(&request.target_session_id, input_request)
                .await?;
            self.normalize_submit_input_request_for_schedule(
                &request.target_session_id,
                input_request,
            )
            .await?;
        }
        if let Some(observation_materialization) = request.observation_materialization.as_mut() {
            if contains_flow_metadata(&observation_materialization.request.metadata) {
                anyhow::bail!("metadata key `{KHEISH_FLOW_METADATA_KEY}` is daemon-owned");
            }
            anyhow::ensure!(
                observation_materialization.target_session_id == request.target_session_id,
                "observation_materialization.target_session_id must match target_session_id"
            );
            self.observation_service
                .selection_sources(observation_materialization)
                .await?;
            self.validate_submit_input_request(
                &request.target_session_id,
                &observation_materialization.request,
            )
            .await?;
            self.normalize_submit_input_request_for_schedule(
                &request.target_session_id,
                &mut observation_materialization.request,
            )
            .await?;
        }
        if let Some(target_agent_id) = request.target_agent_id.as_deref() {
            anyhow::ensure!(
                target_agent_id == target_agent.0,
                "target_agent_id {target_agent_id} does not own session {}",
                request.target_session_id
            );
        }
        request.target_agent_id = Some(target_agent.0.clone());
        if let Some(owner_session_id) = request.owner_session_id.as_deref() {
            let active_count = self
                .schedule_service
                .active_owner_schedule_count(owner_session_id)
                .await;
            anyhow::ensure!(
                active_count < DEFAULT_MAX_OWNER_SCHEDULES,
                "owner session already has {DEFAULT_MAX_OWNER_SCHEDULES} active schedules"
            );
        }
        let now = now_ms();
        let record = build_schedule_record(self.schedule_service.next_schedule_id(), now, request)?;
        let view = self.schedule_service.create_schedule(record).await?;
        info!(
            schedule_id = %view.schedule_id,
            target_session_id = %view.target_session_id,
            target_agent_id = view.target_agent_id.as_deref(),
            owner_session_id = view.owner_session_id.as_deref(),
            cadence = ?view.cadence,
            overlap_policy = ?view.overlap_policy,
            "created schedule"
        );
        if let Some(created_by_run_id) = view.created_by_run_id.as_deref() {
            self.bind_schedule_to_channel_thread_from_run(&view.schedule_id, created_by_run_id)
                .await?;
        }
        Ok(view)
    }

    pub(crate) async fn list_schedules(
        &self,
        session_id: Option<&str>,
    ) -> Result<Vec<ScheduleView>> {
        Ok(self.schedule_service.list_schedules(session_id).await)
    }

    pub(crate) async fn get_schedule(&self, schedule_id: &str) -> Result<ScheduleView> {
        self.schedule_service.get_schedule(schedule_id).await
    }

    pub(crate) async fn cancel_schedule(&self, schedule_id: &str) -> Result<ScheduleView> {
        let view = self
            .update_schedule_status(schedule_id, ScheduleStatus::Canceled)
            .await?;
        info!(schedule_id = %view.schedule_id, "canceled schedule");
        Ok(view)
    }

    pub(crate) async fn pause_schedule(&self, schedule_id: &str) -> Result<ScheduleView> {
        let view = self.schedule_service.pause_schedule(schedule_id).await?;
        info!(schedule_id = %view.schedule_id, "paused schedule");
        Ok(view)
    }

    pub(crate) async fn resume_schedule(&self, schedule_id: &str) -> Result<ScheduleView> {
        let view = self.schedule_service.resume_schedule(schedule_id).await?;
        info!(
            schedule_id = %view.schedule_id,
            next_fire_at_ms = view.next_fire_at_ms,
            "resumed schedule"
        );
        Ok(view)
    }

    pub(crate) async fn trigger_schedule_now(&self, schedule_id: &str) -> Result<ScheduleView> {
        let view = self
            .schedule_service
            .trigger_schedule_now(schedule_id)
            .await?;
        info!(
            schedule_id = %view.schedule_id,
            queued_fire_at_ms = view.queued_fire_at_ms,
            "triggered schedule immediately"
        );
        Ok(view)
    }

    async fn update_schedule_status(
        &self,
        schedule_id: &str,
        status: ScheduleStatus,
    ) -> Result<ScheduleView> {
        self.schedule_service
            .update_schedule_status(schedule_id, status)
            .await
    }

    async fn scheduler_worker_loop(self: Arc<Self>) {
        loop {
            match self.scheduler_step().await {
                Ok(Some(deadline_ms)) => {
                    let wait = deadline_ms.saturating_sub(now_ms()).max(1);
                    tokio::select! {
                        _ = self.schedule_service.notify().notified() => {}
                        _ = sleep_until(Instant::now() + Duration::from_millis(wait)) => {}
                    }
                }
                Ok(None) => {
                    self.schedule_service.notify().notified().await;
                }
                Err(error) => {
                    error!(error = ?error, "scheduler worker error");
                    tokio::select! {
                        _ = self.schedule_service.notify().notified() => {}
                        _ = sleep_until(Instant::now() + Duration::from_millis(500)) => {}
                    }
                }
            }
        }
    }

    async fn scheduler_step(self: &Arc<Self>) -> Result<Option<u64>> {
        let SchedulerSnapshot {
            due_schedule_ids, ..
        } = self.schedule_service.scheduler_snapshot(now_ms()).await;
        for schedule_id in due_schedule_ids {
            if let Err(error) = self.process_due_schedule(&schedule_id).await {
                error!(
                    schedule_id = %schedule_id,
                    error = ?error,
                    "failed to process due schedule"
                );
                self.schedule_service
                    .defer_schedule_retry(&schedule_id, now_ms(), &error)
                    .await?;
            } else {
                self.schedule_service
                    .clear_schedule_retry_backoff(&schedule_id)
                    .await?;
            }
        }
        Ok(self
            .schedule_service
            .scheduler_snapshot(now_ms())
            .await
            .next_due_at_ms)
    }

    async fn process_due_schedule(self: &Arc<Self>, schedule_id: &str) -> Result<()> {
        let existing_runs_by_fire_at_ms = self
            .run_service
            .scheduled_execution_snapshot(schedule_id)
            .run_ids_by_fire_at_ms;
        let Some(ScheduleDueDecision {
            planned_updated_at_ms,
            marks,
            dispatches,
            completion,
        }) = self
            .schedule_service
            .plan_due_schedule(schedule_id, now_ms(), &existing_runs_by_fire_at_ms)
            .await?
        else {
            return Ok(());
        };

        for mark in marks {
            let applied = self
                .schedule_service
                .mark_schedule_dispatched(
                    schedule_id,
                    mark.fire_at_ms,
                    mark.run_id,
                    mark.clear_queued_fire,
                    planned_updated_at_ms,
                )
                .await?;
            if !applied {
                debug!(
                    schedule_id = %schedule_id,
                    fire_at_ms = mark.fire_at_ms,
                    "skipping stale schedule mark"
                );
                return Ok(());
            }
        }
        for dispatch in dispatches {
            let dispatched = self
                .dispatch_scheduled_work(
                    schedule_id,
                    planned_updated_at_ms,
                    dispatch.fire_at_ms,
                    dispatch.from_queued_fire,
                )
                .await?;
            if !dispatched {
                debug!(
                    schedule_id = %schedule_id,
                    fire_at_ms = dispatch.fire_at_ms,
                    "skipping stale scheduled input dispatch"
                );
                return Ok(());
            }
        }
        if let Some(completion) = completion {
            match completion {
                ScheduleDueCompletion::SkipQueuedFire { fire_at_ms, reason } => {
                    let applied = self
                        .schedule_service
                        .skip_queued_fire(schedule_id, fire_at_ms, planned_updated_at_ms, &reason)
                        .await?;
                    if !applied {
                        debug!(
                            schedule_id = %schedule_id,
                            fire_at_ms,
                            "skipping stale queued-fire clear"
                        );
                    }
                }
                ScheduleDueCompletion::AdvanceWithoutDispatch { next_fire_at_ms } => {
                    let applied = self
                        .schedule_service
                        .advance_schedule_without_dispatch(
                            schedule_id,
                            next_fire_at_ms,
                            planned_updated_at_ms,
                        )
                        .await?;
                    if !applied {
                        debug!(
                            schedule_id = %schedule_id,
                            "skipping stale advance-without-dispatch completion"
                        );
                    }
                }
                ScheduleDueCompletion::QueueFire {
                    next_fire_at_ms,
                    queued_fire_at_ms,
                } => {
                    let applied = self
                        .schedule_service
                        .queue_schedule_fire(
                            schedule_id,
                            next_fire_at_ms,
                            queued_fire_at_ms,
                            planned_updated_at_ms,
                        )
                        .await?;
                    if !applied {
                        debug!(
                            schedule_id = %schedule_id,
                            queued_fire_at_ms,
                            "skipping stale queue-fire completion"
                        );
                    }
                }
                ScheduleDueCompletion::FinalizeNextFire { next_fire_at_ms } => {
                    let applied = self
                        .schedule_service
                        .finalize_schedule_next_fire(
                            schedule_id,
                            next_fire_at_ms,
                            planned_updated_at_ms,
                        )
                        .await?;
                    if !applied {
                        debug!(
                            schedule_id = %schedule_id,
                            "skipping stale finalize-next-fire completion"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    async fn dispatch_scheduled_work(
        self: &Arc<Self>,
        schedule_id: &str,
        expected_updated_at_ms: u64,
        fire_at_ms: u64,
        from_queued_fire: bool,
    ) -> Result<bool> {
        let record = self
            .schedule_service
            .schedule_record(schedule_id)
            .await
            .ok_or_else(|| anyhow!("unknown schedule {schedule_id}"))?;
        if record.view.status != ScheduleStatus::Active
            || record.view.updated_at_ms != expected_updated_at_ms
        {
            debug!(
                schedule_id = %schedule_id,
                status = ?record.view.status,
                current_updated_at_ms = record.view.updated_at_ms,
                expected_updated_at_ms,
                "skipping scheduled input dispatch because the schedule is no longer current"
            );
            return Ok(false);
        }
        let run_id = self.next_run_id();
        let now = now_ms();
        let target_agent = self
            .agent_id_for_session(&record.view.target_session_id)
            .await?;
        let run_record = if let Some(request) = record.request.clone() {
            let mut request = resolved_request_for_schedule(schedule_id, request, fire_at_ms);
            self.normalize_submit_input_request(&record.view.target_session_id, &mut request)
                .await?;
            let (resolved_provider, resolved_generation) = {
                let _runtime_config_snapshot = self.runtime_config_service.snapshot_guard().await;
                self.resolve_generation_route_for_session(
                    &record.view.target_session_id,
                    request.provider.take(),
                    request.generation.take(),
                )
                .await?
            };
            request.provider = resolved_provider;
            request.generation = resolved_generation;
            let input_attachments = self
                .input_attachment_refs_for_request(&record.view.target_session_id, &request)
                .await?;
            let input_metadata = request.metadata.clone();
            let reply_targets = self
                .resolve_run_reply_targets(&record.view.target_session_id, &request)
                .await?;
            RunRecord {
                view: RunView {
                    run_id: run_id.clone(),
                    session_id: record.view.target_session_id.clone(),
                    agent_id: target_agent.0.clone(),
                    kind: DaemonRunKind::ScheduledInput,
                    status: DaemonRunStatus::Queued,
                    submitted_at_ms: now,
                    updated_at_ms: now,
                    started_at_ms: None,
                    finished_at_ms: None,
                    queued_position: None,
                    request: summarize_input_request(&request),
                    input_attachments,
                    input_metadata,
                    pending_approval_ids: Vec::new(),
                    pending_approvals: Vec::new(),
                    pending_question_ids: Vec::new(),
                    pending_questions: Vec::new(),
                    outputs: Vec::new(),
                    deliveries: Vec::new(),
                    error: None,
                },
                reply_targets,
                payload: RunRequestPayload::ScheduledInput {
                    schedule_id: schedule_id.to_string(),
                    fire_at_ms,
                    request,
                },
            }
        } else if let Some(request) = record.observation_materialization.clone() {
            let request = self
                .prepare_observation_materialization_request(
                    resolved_observation_materialization_request_for_schedule(
                        schedule_id,
                        request,
                        fire_at_ms,
                    ),
                )
                .await?;
            let reply_targets = self
                .resolve_run_reply_targets(&record.view.target_session_id, &request.request)
                .await?;
            RunRecord {
                view: RunView {
                    run_id: run_id.clone(),
                    session_id: record.view.target_session_id.clone(),
                    agent_id: target_agent.0.clone(),
                    kind: DaemonRunKind::ScheduledObservationMaterialization,
                    status: DaemonRunStatus::Queued,
                    submitted_at_ms: now,
                    updated_at_ms: now,
                    started_at_ms: None,
                    finished_at_ms: None,
                    queued_position: None,
                    request: summarize_observation_materialization_request(&request),
                    input_attachments: Vec::new(),
                    input_metadata: None,
                    pending_approval_ids: Vec::new(),
                    pending_approvals: Vec::new(),
                    pending_question_ids: Vec::new(),
                    pending_questions: Vec::new(),
                    outputs: Vec::new(),
                    deliveries: Vec::new(),
                    error: None,
                },
                reply_targets,
                payload: RunRequestPayload::ScheduledObservationMaterialization {
                    schedule_id: schedule_id.to_string(),
                    fire_at_ms,
                    request,
                },
            }
        } else {
            anyhow::bail!("schedule {schedule_id} does not contain a payload");
        };
        let claimed = self
            .schedule_service
            .mark_schedule_dispatched(
                schedule_id,
                fire_at_ms,
                Some(run_id.clone()),
                from_queued_fire,
                expected_updated_at_ms,
            )
            .await?;
        if !claimed {
            debug!(
                schedule_id = %schedule_id,
                fire_at_ms,
                expected_updated_at_ms,
                "skipping scheduled input dispatch because the schedule changed before dispatch claim"
            );
            return Ok(false);
        }
        let view = match self.schedule_run(run_record).await {
            Ok(view) => view,
            Err(error) => {
                if let Err(rollback_error) = self
                    .schedule_service
                    .rollback_schedule_dispatch(
                        schedule_id,
                        fire_at_ms,
                        &run_id,
                        from_queued_fire,
                        &error,
                    )
                    .await
                {
                    return Err(anyhow!(
                        "failed to dispatch scheduled input: {error}; rollback also failed: {rollback_error}"
                    ));
                }
                return Err(error);
            }
        };
        self.schedule_service
            .mark_schedule_run_persisted(schedule_id, fire_at_ms, &run_id)
            .await?;
        info!(
            schedule_id = %schedule_id,
            run_id = %view.run_id,
            target_session_id = %view.session_id,
            target_agent_id = %view.agent_id,
            fire_at_ms,
            from_queued_fire,
            "dispatched scheduled work"
        );
        self.schedule_service
            .set_target_agent(schedule_id, target_agent.0.clone())
            .await?;
        Ok(true)
    }

    pub(super) async fn settle_scheduled_run(self: &Arc<Self>, run_id: &str) -> Result<()> {
        let record = self.run_record(run_id).await?;
        let Some(view) = self.schedule_service.settle_scheduled_run(&record).await? else {
            return Ok(());
        };
        info!(
            schedule_id = %view.schedule_id,
            run_id = %run_id,
            status = ?record.view.status,
            execution_count = view.execution_count,
            consecutive_failures = view.consecutive_failures,
            "settled scheduled run"
        );
        if let Some((channel_id, thread_root_message_id)) = self
            .channel_service
            .thread_for_binding(
                crate::channels::ChannelWorkBindingKind::Schedule,
                &view.schedule_id,
            )
            .await
        {
            let latest_output = record
                .view
                .outputs
                .iter()
                .rev()
                .find(|output| {
                    output.source_kind == Some(crate::DaemonOutputSourceKind::EmitOutput)
                        && (!output.content.trim().is_empty()
                            || !output.parts.is_empty()
                            || !output.artifacts.is_empty())
                })
                .map(|output| output.content.trim().to_string())
                .filter(|content| !content.is_empty());
            let content = latest_output.unwrap_or_else(|| {
                format!(
                    "Scheduled work `{}` finished with status `{}`.",
                    view.name,
                    serde_json::to_string(&record.view.status)
                        .unwrap_or_else(|_| "\"completed\"".to_string())
                        .trim_matches('"')
                )
            });
            let _ = self
                .enqueue_channel_stimulus(
                    &channel_id,
                    Some(&thread_root_message_id),
                    crate::channels::ChannelStimulusKind::ScheduleResult,
                    content,
                    Some("schedule".to_string()),
                    Some(view.schedule_id.clone()),
                    Some(format!("schedule:{}", view.schedule_id)),
                    Some(format!("schedule:{}:latest", view.schedule_id)),
                    Vec::new(),
                    crate::channels::ChannelStimulusVisibilityHint::Thread,
                    None,
                    json!({
                        "schedule_id": view.schedule_id,
                        "run_id": run_id,
                        "status": record.view.status,
                    }),
                )
                .await?;
        }
        Ok(())
    }

    async fn reconcile_schedules_on_boot(self: &Arc<Self>) -> Result<()> {
        let terminal_scheduled_runs = self.run_service.terminal_scheduled_runs().await;
        for record in terminal_scheduled_runs {
            if let Err(error) = self.settle_scheduled_run(&record.view.run_id).await {
                warn!(
                    run_id = %record.view.run_id,
                    error = ?error,
                    "failed to reconcile terminal scheduled run on boot"
                );
            }
        }

        for schedule in self.schedule_service.schedule_records().await {
            let Some(run_id) = schedule.view.in_flight_run_id.clone() else {
                continue;
            };
            if !self.run_service.run_exists(&run_id).await {
                let reason = format!(
                    "cleared stale scheduler claim for missing run {run_id} during daemon boot"
                );
                self.schedule_service
                    .clear_stale_in_flight_run(&schedule.view.schedule_id, &run_id, &reason)
                    .await?;
            }
        }
        Ok(())
    }
}
