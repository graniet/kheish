use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};
use tokio::sync::{Mutex, Notify};

use crate::DaemonScheduleStatusSummaryView;
use crate::runs::{DaemonRunStatus, RunRecord, now_ms};
use crate::scheduler::{
    DEFAULT_SCHEDULE_RECENT_EXECUTION_LIMIT, FileScheduleStore, ScheduleExecutionRecord,
    ScheduleExecutionStatus, ScheduleRecord, ScheduleStatus, ScheduleView, SchedulerPolicyConfig,
    due_schedule_plan, resume_schedule_next_fire_at_ms,
};

/// One scheduler loop snapshot collected in a single pass over the schedule map.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SchedulerSnapshot {
    /// The active schedules that should be processed immediately.
    pub(crate) due_schedule_ids: Vec<String>,
    /// The next wake-up boundary across all active schedules.
    pub(crate) next_due_at_ms: Option<u64>,
}

/// One mark-only action for a scheduled fire that already owns a live run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScheduleDispatchMark {
    /// The fire timestamp acknowledged by the scheduler.
    pub(crate) fire_at_ms: u64,
    /// The existing run identifier to associate with the fire when needed.
    pub(crate) run_id: Option<String>,
    /// Indicates whether the queued-fire slot should be cleared.
    pub(crate) clear_queued_fire: bool,
}

/// One scheduled fire that must be dispatched by the daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScheduleDispatchRequest {
    /// The fire timestamp to dispatch.
    pub(crate) fire_at_ms: u64,
    /// Indicates whether the fire came from the queued-fire slot.
    pub(crate) from_queued_fire: bool,
}

/// One post-dispatch mutation that should be applied after processing due fires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScheduleDueCompletion {
    /// Clears a queued fire that overlap policy skipped.
    SkipQueuedFire {
        /// The queued fire timestamp to clear.
        fire_at_ms: u64,
        /// Human-readable reason recorded in the bounded execution history.
        reason: String,
    },
    /// Advances the next fire boundary without dispatching new work.
    AdvanceWithoutDispatch {
        /// The next fire boundary to persist.
        next_fire_at_ms: Option<u64>,
    },
    /// Persists one queued follow-up fire while another execution is still active.
    QueueFire {
        /// The next cadence boundary to persist.
        next_fire_at_ms: Option<u64>,
        /// The fire timestamp that should remain queued.
        queued_fire_at_ms: u64,
    },
    /// Finalizes the next fire boundary after dispatching the current work.
    FinalizeNextFire {
        /// The next fire boundary to persist.
        next_fire_at_ms: Option<u64>,
    },
}

/// One scheduler decision for a single due schedule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScheduleDueDecision {
    /// The schedule update watermark observed while planning this decision.
    pub(crate) planned_updated_at_ms: u64,
    /// Existing fires that should only be marked as dispatched.
    pub(crate) marks: Vec<ScheduleDispatchMark>,
    /// New fires that should be dispatched through `DaemonState`.
    pub(crate) dispatches: Vec<ScheduleDispatchRequest>,
    /// The follow-up persistence step after the dispatch loop finishes.
    pub(crate) completion: Option<ScheduleDueCompletion>,
}

/// Owns durable daemon schedules, their in-memory index, and scheduler wakeups.
pub(crate) struct ScheduleService {
    schedule_store: FileScheduleStore,
    schedules: Mutex<BTreeMap<String, ScheduleRecord>>,
    notify: Notify,
    next_schedule_id: AtomicU64,
    retry_policy: SchedulerPolicyConfig,
}

impl ScheduleService {
    /// Creates a new schedule service backed by the persisted schedule store.
    pub(crate) fn new(
        schedule_store: FileScheduleStore,
        schedules: BTreeMap<String, ScheduleRecord>,
        next_schedule_id: AtomicU64,
        retry_policy: SchedulerPolicyConfig,
    ) -> Self {
        Self {
            schedule_store,
            schedules: Mutex::new(schedules),
            notify: Notify::new(),
            next_schedule_id,
            retry_policy,
        }
    }

    /// Returns one fresh daemon-managed schedule identifier.
    pub(crate) fn next_schedule_id(&self) -> String {
        format!(
            "schedule-{}",
            self.next_schedule_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Returns the scheduler wakeup notifier owned by the service.
    pub(crate) fn notify(&self) -> &Notify {
        &self.notify
    }

    /// Returns the daemon-global scheduler retry policy used for new retry decisions.
    pub(crate) fn scheduler_policy(&self) -> SchedulerPolicyConfig {
        self.retry_policy.clone()
    }

    async fn update_schedule_record<T>(
        &self,
        schedule_id: &str,
        notify_waiters: bool,
        update: impl FnOnce(&mut ScheduleRecord) -> Result<(T, bool)>,
    ) -> Result<T> {
        let (result, _changed) = {
            let mut schedules = self.schedules.lock().await;
            let record = schedules
                .get_mut(schedule_id)
                .ok_or_else(|| anyhow!("unknown schedule {schedule_id}"))?;
            let previous = record.clone();
            let (result, changed) = update(record)?;
            if changed {
                if let Err(error) = self.schedule_store.save_schedule(record) {
                    *record = previous;
                    return Err(error);
                }
            }
            (result, changed)
        };
        if notify_waiters {
            self.notify.notify_waiters();
        }
        Ok(result)
    }

    /// Persists and registers one new schedule record.
    pub(crate) async fn create_schedule(&self, record: ScheduleRecord) -> Result<ScheduleView> {
        let view = record.view.clone();
        let mut schedules = self.schedules.lock().await;
        self.schedule_store.save_schedule(&record)?;
        schedules.insert(view.schedule_id.clone(), record);
        drop(schedules);
        self.notify.notify_waiters();
        Ok(view)
    }

    /// Persists and registers one new schedule only when its public name is unused.
    pub(crate) async fn create_schedule_if_name_absent(
        &self,
        record: ScheduleRecord,
    ) -> Result<ScheduleView> {
        let view = record.view.clone();
        let mut schedules = self.schedules.lock().await;
        if schedules
            .values()
            .any(|candidate| candidate.view.name == view.name)
        {
            bail!("schedule {} already exists", view.name);
        }
        self.schedule_store.save_schedule(&record)?;
        schedules.insert(view.schedule_id.clone(), record);
        drop(schedules);
        self.notify.notify_waiters();
        Ok(view)
    }

    /// Lists schedules, optionally scoped to one session identifier.
    pub(crate) async fn list_schedules(&self, session_id: Option<&str>) -> Vec<ScheduleView> {
        let schedules = self.schedules.lock().await;
        let mut views = schedules
            .values()
            .filter(|record| {
                session_id.is_none_or(|session_id| {
                    record.view.target_session_id == session_id
                        || record.view.owner_session_id.as_deref() == Some(session_id)
                })
            })
            .map(|record| record.view.clone())
            .collect::<Vec<_>>();
        views.sort_by(|left, right| left.schedule_id.cmp(&right.schedule_id));
        views
    }

    /// Returns one schedule view by identifier.
    pub(crate) async fn get_schedule(&self, schedule_id: &str) -> Result<ScheduleView> {
        self.schedules
            .lock()
            .await
            .get(schedule_id)
            .map(|record| record.view.clone())
            .ok_or_else(|| anyhow!("unknown schedule {schedule_id}"))
    }

    /// Counts active schedules owned by one session.
    pub(crate) async fn active_owner_schedule_count(&self, owner_session_id: &str) -> usize {
        self.schedules
            .lock()
            .await
            .values()
            .filter(|record| {
                record.view.owner_session_id.as_deref() == Some(owner_session_id)
                    && !record.view.status.is_terminal()
            })
            .count()
    }

    /// Returns one cloned schedule record when present.
    pub(crate) async fn schedule_record(&self, schedule_id: &str) -> Option<ScheduleRecord> {
        self.schedules.lock().await.get(schedule_id).cloned()
    }

    /// Returns every persisted schedule record in stable schedule-id order.
    pub(crate) async fn schedule_records(&self) -> Vec<ScheduleRecord> {
        self.schedules.lock().await.values().cloned().collect()
    }

    /// Returns a cheap point-in-time status summary for all daemon schedules.
    pub(crate) async fn status_snapshot(&self, now: u64) -> DaemonScheduleStatusSummaryView {
        let schedules = self.schedules.lock().await;
        let mut snapshot = DaemonScheduleStatusSummaryView::default();

        for record in schedules.values() {
            snapshot.total += 1;
            match record.view.status {
                ScheduleStatus::Active => snapshot.active += 1,
                ScheduleStatus::Paused => snapshot.paused += 1,
                ScheduleStatus::Completed => snapshot.completed += 1,
                ScheduleStatus::Canceled => snapshot.canceled += 1,
            }

            if record.view.in_flight_run_id.is_some() {
                snapshot.in_flight_count += 1;
            }
            if record.view.queued_fire_at_ms.is_some() {
                snapshot.queued_fire_count += 1;
            }

            if record.view.status != ScheduleStatus::Active {
                continue;
            }

            let Some((earliest_fire_at_ms, backoff_applied)) =
                effective_schedule_fire_at_ms(record, now)
            else {
                continue;
            };
            if backoff_applied {
                snapshot.backoff_count += 1;
            }
            if earliest_fire_at_ms <= now {
                let lag_ms = now.saturating_sub(earliest_fire_at_ms);
                if snapshot
                    .oldest_due_schedule_lag_ms
                    .is_none_or(|current| lag_ms > current)
                {
                    snapshot.oldest_due_schedule_lag_ms = Some(lag_ms);
                    snapshot.oldest_due_schedule_id = Some(record.view.schedule_id.clone());
                }
            }
            snapshot.next_due_at_ms = Some(
                snapshot
                    .next_due_at_ms
                    .map(|current| current.min(earliest_fire_at_ms))
                    .unwrap_or(earliest_fire_at_ms),
            );
            if earliest_fire_at_ms <= now {
                snapshot.due_count += 1;
            }
        }

        snapshot
    }

    /// Returns the scheduler work queue and next wakeup using one schedule-map scan.
    pub(crate) async fn scheduler_snapshot(&self, now: u64) -> SchedulerSnapshot {
        let schedules = self.schedules.lock().await;
        let mut snapshot = SchedulerSnapshot::default();
        for record in schedules.values() {
            if record.view.status != ScheduleStatus::Active {
                continue;
            }
            let Some((earliest_fire_at_ms, _)) = effective_schedule_fire_at_ms(record, now) else {
                continue;
            };
            snapshot.next_due_at_ms = Some(
                snapshot
                    .next_due_at_ms
                    .map(|current| current.min(earliest_fire_at_ms))
                    .unwrap_or(earliest_fire_at_ms),
            );
            if earliest_fire_at_ms <= now {
                snapshot
                    .due_schedule_ids
                    .push(record.view.schedule_id.clone());
            }
        }
        snapshot
    }

    /// Clears the retry backoff for one schedule after a successful pass or admin change.
    pub(crate) async fn clear_schedule_retry_backoff(&self, schedule_id: &str) -> Result<()> {
        self.update_schedule_record(schedule_id, true, |record| {
            if record.view.scheduler_retry_after_ms.is_none()
                && record.view.scheduler_retry_attempt == 0
                && record.view.last_scheduler_error.is_none()
            {
                return Ok(((), false));
            }
            record.view.scheduler_retry_after_ms = None;
            record.view.scheduler_retry_attempt = 0;
            record.view.last_scheduler_error = None;
            record.view.updated_at_ms = now_ms();
            Ok(((), true))
        })
        .await
    }

    /// Defers one broken schedule without slowing down unrelated wakeups.
    pub(crate) async fn defer_schedule_retry(
        &self,
        schedule_id: &str,
        now: u64,
        error: &anyhow::Error,
    ) -> Result<()> {
        let policy = self.retry_policy.clone();
        self.update_schedule_record(schedule_id, true, |record| {
            if record.view.status != ScheduleStatus::Active {
                return Ok(((), false));
            }
            let attempt = record.view.scheduler_retry_attempt.saturating_add(1);
            let message = error.to_string();
            record.view.scheduler_retry_attempt = attempt;
            record.view.last_scheduler_error = Some(message.clone());
            let exhausted = policy.retry_max_attempts > 0 && attempt >= policy.retry_max_attempts;
            if exhausted {
                record.view.scheduler_retry_after_ms = None;
                record.view.status = ScheduleStatus::Paused;
                record.view.paused_remaining_ms = record
                    .view
                    .next_fire_at_ms
                    .or(record.view.queued_fire_at_ms)
                    .map(|fire_at_ms| fire_at_ms.saturating_sub(now));
            } else {
                let delay = retry_delay_ms(&policy, &record.view.schedule_id, attempt);
                let retry_after_ms = now.saturating_add(delay);
                let fire_at_ms = retry_fire_at_ms(record);
                record.view.scheduler_retry_after_ms = Some(retry_after_ms);
                record_recent_execution(
                    record,
                    fire_at_ms,
                    None,
                    ScheduleExecutionStatus::Retrying,
                    false,
                    now,
                    Some(retry_after_ms),
                    Some(message.clone()),
                );
            }
            record.view.updated_at_ms = now;
            Ok(((), true))
        })
        .await
    }

    /// Updates the status of one schedule.
    pub(crate) async fn update_schedule_status(
        &self,
        schedule_id: &str,
        status: ScheduleStatus,
    ) -> Result<ScheduleView> {
        self.update_schedule_record(schedule_id, true, |record| {
            anyhow::ensure!(
                !record.view.status.is_terminal(),
                "schedule {schedule_id} is already terminal"
            );
            record.view.status = status;
            if record.view.status.is_terminal() {
                record.view.next_fire_at_ms = None;
                record.view.queued_fire_at_ms = None;
            }
            record.view.updated_at_ms = now_ms();
            Ok((record.view.clone(), true))
        })
        .await
    }

    /// Pauses one active schedule and persists the updated remaining delay.
    pub(crate) async fn pause_schedule(&self, schedule_id: &str) -> Result<ScheduleView> {
        self.update_schedule_record(schedule_id, true, |record| {
            anyhow::ensure!(
                record.view.status == ScheduleStatus::Active,
                "schedule {schedule_id} is not active"
            );
            let now = now_ms();
            record.view.status = ScheduleStatus::Paused;
            record.view.paused_remaining_ms =
                record.view.queued_fire_at_ms.map(|_| 0).or_else(|| {
                    record
                        .view
                        .next_fire_at_ms
                        .map(|fire_at_ms| fire_at_ms.saturating_sub(now))
                });
            record.view.next_fire_at_ms = None;
            record.view.queued_fire_at_ms = None;
            record.view.updated_at_ms = now;
            Ok((record.view.clone(), true))
        })
        .await
    }

    /// Resumes one paused schedule and restores its next fire time.
    pub(crate) async fn resume_schedule(&self, schedule_id: &str) -> Result<ScheduleView> {
        self.update_schedule_record(schedule_id, true, |record| {
            anyhow::ensure!(
                record.view.status == ScheduleStatus::Paused,
                "schedule {schedule_id} is not paused"
            );
            let now = now_ms();
            record.view.status = ScheduleStatus::Active;
            record.view.next_fire_at_ms = resume_schedule_next_fire_at_ms(&record.view, now)?;
            record.view.queued_fire_at_ms = None;
            record.view.paused_remaining_ms = None;
            record.view.updated_at_ms = now;
            record.view.last_error = None;
            record.view.last_scheduler_error = None;
            record.view.scheduler_retry_after_ms = None;
            record.view.scheduler_retry_attempt = 0;
            Ok((record.view.clone(), true))
        })
        .await
    }

    /// Queues one immediate fire for an active schedule.
    pub(crate) async fn trigger_schedule_now(&self, schedule_id: &str) -> Result<ScheduleView> {
        self.update_schedule_record(schedule_id, true, |record| {
            anyhow::ensure!(
                record.view.status == ScheduleStatus::Active,
                "schedule {schedule_id} is not active"
            );
            let now = now_ms();
            if matches!(record.view.cadence, crate::ScheduleCadence::Once { .. }) {
                record.view.next_fire_at_ms = None;
            }
            record.view.queued_fire_at_ms = Some(
                record
                    .view
                    .queued_fire_at_ms
                    .map(|value| value.min(now))
                    .unwrap_or(now),
            );
            record.view.paused_remaining_ms = None;
            record.view.last_scheduler_error = None;
            record.view.scheduler_retry_after_ms = None;
            record.view.scheduler_retry_attempt = 0;
            record.view.updated_at_ms = now;
            Ok((record.view.clone(), true))
        })
        .await
    }

    /// Updates the recorded target agent for one schedule.
    pub(crate) async fn set_target_agent(
        &self,
        schedule_id: &str,
        target_agent_id: String,
    ) -> Result<()> {
        self.update_schedule_record(schedule_id, false, |record| {
            record.view.target_agent_id = Some(target_agent_id);
            Ok(((), true))
        })
        .await
    }

    /// Marks one fire as dispatched and optionally clears the queued-fire slot.
    pub(crate) async fn mark_schedule_dispatched(
        &self,
        schedule_id: &str,
        fire_at_ms: u64,
        run_id: Option<String>,
        clear_queued_fire: bool,
        expected_updated_at_ms: u64,
    ) -> Result<bool> {
        self.update_schedule_record(schedule_id, true, |record| {
            if record.view.status != ScheduleStatus::Active
                || record.view.updated_at_ms != expected_updated_at_ms
            {
                return Ok((false, false));
            }
            record.view.in_flight_fire_at_ms = Some(fire_at_ms);
            record.view.in_flight_run_id = run_id
                .clone()
                .or_else(|| record.view.in_flight_run_id.clone());
            record.view.last_dispatched_run_id =
                run_id.or_else(|| record.view.last_dispatched_run_id.clone());
            if clear_queued_fire {
                record.view.queued_fire_at_ms = None;
            }
            let now = now_ms();
            record_recent_execution(
                record,
                fire_at_ms,
                record.view.in_flight_run_id.clone(),
                ScheduleExecutionStatus::Claimed,
                clear_queued_fire,
                now,
                None,
                None,
            );
            Ok((true, true))
        })
        .await
    }

    /// Marks a previously claimed scheduled run as durably persisted.
    pub(crate) async fn mark_schedule_run_persisted(
        &self,
        schedule_id: &str,
        fire_at_ms: u64,
        run_id: &str,
    ) -> Result<()> {
        self.update_schedule_record(schedule_id, true, |record| {
            if record.view.in_flight_run_id.as_deref() != Some(run_id)
                || record.view.in_flight_fire_at_ms != Some(fire_at_ms)
            {
                return Ok(((), false));
            }
            record_recent_execution(
                record,
                fire_at_ms,
                Some(run_id.to_string()),
                ScheduleExecutionStatus::Dispatched,
                false,
                now_ms(),
                None,
                None,
            );
            Ok(((), true))
        })
        .await
    }

    /// Rolls back one claimed dispatch when run submission fails.
    pub(crate) async fn rollback_schedule_dispatch(
        &self,
        schedule_id: &str,
        fire_at_ms: u64,
        run_id: &str,
        restore_queued_fire: bool,
        error: &anyhow::Error,
    ) -> Result<()> {
        self.update_schedule_record(schedule_id, true, |record| {
            let matches_in_flight = record.view.in_flight_fire_at_ms == Some(fire_at_ms)
                || record.view.in_flight_run_id.as_deref() == Some(run_id);
            if !matches_in_flight {
                return Ok(((), false));
            }
            if record.view.in_flight_fire_at_ms == Some(fire_at_ms) {
                record.view.in_flight_fire_at_ms = None;
            }
            if record.view.in_flight_run_id.as_deref() == Some(run_id) {
                record.view.in_flight_run_id = None;
            }
            if record.view.last_dispatched_run_id.as_deref() == Some(run_id) {
                record.view.last_dispatched_run_id = None;
            }
            if restore_queued_fire {
                record.view.queued_fire_at_ms = Some(
                    record
                        .view
                        .queued_fire_at_ms
                        .map(|value| value.min(fire_at_ms))
                        .unwrap_or(fire_at_ms),
                );
            }
            let now = now_ms();
            record_recent_execution(
                record,
                fire_at_ms,
                Some(run_id.to_string()),
                ScheduleExecutionStatus::RolledBack,
                restore_queued_fire,
                now,
                None,
                Some(error.to_string()),
            );
            record.view.updated_at_ms = now;
            Ok(((), true))
        })
        .await
    }

    /// Clears one queued fire because overlap policy skipped it.
    pub(crate) async fn skip_queued_fire(
        &self,
        schedule_id: &str,
        fire_at_ms: u64,
        expected_updated_at_ms: u64,
        reason: &str,
    ) -> Result<bool> {
        self.update_schedule_record(schedule_id, true, |record| {
            if record.view.status != ScheduleStatus::Active
                || record.view.updated_at_ms != expected_updated_at_ms
                || record.view.queued_fire_at_ms != Some(fire_at_ms)
            {
                return Ok((false, false));
            }
            record.view.queued_fire_at_ms = None;
            let now = now_ms();
            record_recent_execution(
                record,
                fire_at_ms,
                None,
                ScheduleExecutionStatus::Skipped,
                true,
                now,
                None,
                Some(reason.to_string()),
            );
            record.view.updated_at_ms = now;
            Ok((true, true))
        })
        .await
    }

    /// Clears an in-flight dispatch marker whose run never became durable.
    pub(crate) async fn clear_stale_in_flight_run(
        &self,
        schedule_id: &str,
        run_id: &str,
        reason: &str,
    ) -> Result<Option<ScheduleView>> {
        self.update_schedule_record(schedule_id, true, |record| {
            if record.view.in_flight_run_id.as_deref() != Some(run_id) {
                return Ok((None, false));
            }
            let fire_at_ms = record.view.in_flight_fire_at_ms;
            record.view.in_flight_fire_at_ms = None;
            record.view.in_flight_run_id = None;
            if record.view.last_dispatched_run_id.as_deref() == Some(run_id) {
                record.view.last_dispatched_run_id = None;
            }
            record.view.last_scheduler_error = Some(reason.to_string());
            let now = now_ms();
            if let Some(fire_at_ms) = fire_at_ms {
                if should_restore_stale_queued_fire(record, fire_at_ms, run_id) {
                    record.view.queued_fire_at_ms = Some(
                        record
                            .view
                            .queued_fire_at_ms
                            .map(|value| value.min(fire_at_ms))
                            .unwrap_or(fire_at_ms),
                    );
                }
                record_recent_execution(
                    record,
                    fire_at_ms,
                    Some(run_id.to_string()),
                    ScheduleExecutionStatus::RolledBack,
                    false,
                    now,
                    None,
                    Some(reason.to_string()),
                );
            }
            record.view.updated_at_ms = now;
            Ok((Some(record.view.clone()), true))
        })
        .await
    }

    /// Persists the next fire boundary after dispatch or skipping.
    pub(crate) async fn finalize_schedule_next_fire(
        &self,
        schedule_id: &str,
        next_fire_at_ms: Option<u64>,
        expected_updated_at_ms: u64,
    ) -> Result<bool> {
        self.update_schedule_record(schedule_id, true, |record| {
            if record.view.status != ScheduleStatus::Active
                || record.view.updated_at_ms != expected_updated_at_ms
            {
                return Ok((false, false));
            }
            record.view.next_fire_at_ms = next_fire_at_ms;
            if next_fire_at_ms.is_none() && record.view.in_flight_fire_at_ms.is_none() {
                record.view.status = ScheduleStatus::Completed;
            }
            record.view.updated_at_ms = now_ms();
            Ok((true, true))
        })
        .await
    }

    /// Advances one schedule without dispatching new work.
    pub(crate) async fn advance_schedule_without_dispatch(
        &self,
        schedule_id: &str,
        next_fire_at_ms: Option<u64>,
        expected_updated_at_ms: u64,
    ) -> Result<bool> {
        self.update_schedule_record(schedule_id, true, |record| {
            if record.view.status != ScheduleStatus::Active
                || record.view.updated_at_ms != expected_updated_at_ms
            {
                return Ok((false, false));
            }
            record.view.next_fire_at_ms = next_fire_at_ms;
            if next_fire_at_ms.is_none() && record.view.in_flight_fire_at_ms.is_none() {
                record.view.status = ScheduleStatus::Completed;
            }
            record.view.updated_at_ms = now_ms();
            Ok((true, true))
        })
        .await
    }

    /// Queues one future fire while leaving an active execution in flight.
    pub(crate) async fn queue_schedule_fire(
        &self,
        schedule_id: &str,
        next_fire_at_ms: Option<u64>,
        queued_fire_at_ms: u64,
        expected_updated_at_ms: u64,
    ) -> Result<bool> {
        self.update_schedule_record(schedule_id, true, |record| {
            if record.view.status != ScheduleStatus::Active
                || record.view.updated_at_ms != expected_updated_at_ms
            {
                return Ok((false, false));
            }
            record.view.next_fire_at_ms = next_fire_at_ms;
            record.view.queued_fire_at_ms = Some(
                record
                    .view
                    .queued_fire_at_ms
                    .map(|value| value.min(queued_fire_at_ms))
                    .unwrap_or(queued_fire_at_ms),
            );
            record.view.updated_at_ms = now_ms();
            Ok((true, true))
        })
        .await
    }

    /// Plans the next scheduler actions for one schedule using the current run snapshot.
    pub(crate) async fn plan_due_schedule(
        &self,
        schedule_id: &str,
        now: u64,
        existing_runs_by_fire_at_ms: &BTreeMap<u64, String>,
    ) -> Result<Option<ScheduleDueDecision>> {
        let Some(record) = self.schedule_record(schedule_id).await else {
            return Ok(None);
        };
        if record.view.status != ScheduleStatus::Active {
            return Ok(None);
        }

        let mut remaining_slots =
            remaining_dispatch_slots(&record, existing_runs_by_fire_at_ms.len());
        let mut decision = ScheduleDueDecision {
            planned_updated_at_ms: record.view.updated_at_ms,
            ..ScheduleDueDecision::default()
        };
        if let Some(queued_fire_at_ms) = record.view.queued_fire_at_ms.filter(|value| *value <= now)
        {
            if let Some(existing_run_id) = existing_runs_by_fire_at_ms.get(&queued_fire_at_ms) {
                decision.marks.push(ScheduleDispatchMark {
                    fire_at_ms: queued_fire_at_ms,
                    run_id: Some(existing_run_id.clone()),
                    clear_queued_fire: true,
                });
            } else {
                if remaining_slots == 0 {
                    decision.completion = Some(ScheduleDueCompletion::SkipQueuedFire {
                        fire_at_ms: queued_fire_at_ms,
                        reason: "queued fire skipped because max_executions is already reserved"
                            .to_string(),
                    });
                } else if existing_runs_by_fire_at_ms.is_empty()
                    || record.view.overlap_policy == crate::ScheduleOverlapPolicy::Parallel
                {
                    decision.dispatches.push(ScheduleDispatchRequest {
                        fire_at_ms: queued_fire_at_ms,
                        from_queued_fire: true,
                    });
                } else {
                    match record.view.overlap_policy {
                        crate::ScheduleOverlapPolicy::Skip => {
                            decision.completion = Some(ScheduleDueCompletion::SkipQueuedFire {
                                fire_at_ms: queued_fire_at_ms,
                                reason: "queued fire skipped because another scheduled execution is active"
                                    .to_string(),
                            });
                        }
                        crate::ScheduleOverlapPolicy::QueueOne => {}
                        crate::ScheduleOverlapPolicy::Parallel => unreachable!(),
                    }
                }
            }
            return Ok(Some(decision));
        }

        let Some(plan) = due_schedule_plan(&record.view, now)? else {
            return Ok(None);
        };
        if plan.fire_times_ms.is_empty() {
            decision.completion = Some(ScheduleDueCompletion::AdvanceWithoutDispatch {
                next_fire_at_ms: plan.next_fire_at_ms,
            });
            return Ok(Some(decision));
        }

        if !existing_runs_by_fire_at_ms.is_empty() {
            match record.view.overlap_policy {
                crate::ScheduleOverlapPolicy::Skip => {
                    decision.completion = Some(ScheduleDueCompletion::AdvanceWithoutDispatch {
                        next_fire_at_ms: plan.next_fire_at_ms,
                    });
                }
                crate::ScheduleOverlapPolicy::QueueOne => {
                    if remaining_slots > 0 {
                        decision.completion = Some(ScheduleDueCompletion::QueueFire {
                            next_fire_at_ms: plan.next_fire_at_ms,
                            queued_fire_at_ms: *plan
                                .fire_times_ms
                                .last()
                                .expect("non-empty fire list"),
                        });
                    } else {
                        decision.completion = Some(ScheduleDueCompletion::AdvanceWithoutDispatch {
                            next_fire_at_ms: plan.next_fire_at_ms,
                        });
                    }
                }
                crate::ScheduleOverlapPolicy::Parallel => {
                    for fire_at_ms in plan.fire_times_ms {
                        if let Some(existing_run_id) = existing_runs_by_fire_at_ms.get(&fire_at_ms)
                        {
                            decision.marks.push(ScheduleDispatchMark {
                                fire_at_ms,
                                run_id: Some(existing_run_id.clone()),
                                clear_queued_fire: false,
                            });
                        } else if remaining_slots > 0 {
                            decision.dispatches.push(ScheduleDispatchRequest {
                                fire_at_ms,
                                from_queued_fire: false,
                            });
                            remaining_slots = remaining_slots.saturating_sub(1);
                        }
                    }
                    decision.completion = Some(ScheduleDueCompletion::FinalizeNextFire {
                        next_fire_at_ms: plan.next_fire_at_ms,
                    });
                }
            }
            return Ok(Some(decision));
        }

        match record.view.overlap_policy {
            crate::ScheduleOverlapPolicy::Parallel => {
                for fire_at_ms in plan.fire_times_ms {
                    if let Some(existing_run_id) = existing_runs_by_fire_at_ms.get(&fire_at_ms) {
                        decision.marks.push(ScheduleDispatchMark {
                            fire_at_ms,
                            run_id: Some(existing_run_id.clone()),
                            clear_queued_fire: false,
                        });
                    } else if remaining_slots > 0 {
                        decision.dispatches.push(ScheduleDispatchRequest {
                            fire_at_ms,
                            from_queued_fire: false,
                        });
                        remaining_slots = remaining_slots.saturating_sub(1);
                    }
                }
            }
            crate::ScheduleOverlapPolicy::Skip | crate::ScheduleOverlapPolicy::QueueOne => {
                let fire_at_ms = *plan.fire_times_ms.last().expect("non-empty fire list");
                if let Some(existing_run_id) = existing_runs_by_fire_at_ms.get(&fire_at_ms) {
                    decision.marks.push(ScheduleDispatchMark {
                        fire_at_ms,
                        run_id: Some(existing_run_id.clone()),
                        clear_queued_fire: false,
                    });
                } else if remaining_slots > 0 {
                    decision.dispatches.push(ScheduleDispatchRequest {
                        fire_at_ms,
                        from_queued_fire: false,
                    });
                }
            }
        }
        decision.completion = Some(ScheduleDueCompletion::FinalizeNextFire {
            next_fire_at_ms: plan.next_fire_at_ms,
        });
        Ok(Some(decision))
    }

    /// Reconciles one settled scheduled run back into its owning schedule.
    pub(crate) async fn settle_scheduled_run(
        &self,
        record: &RunRecord,
    ) -> Result<Option<ScheduleView>> {
        let Some(origin) = crate::services::run::scheduled_run_origin(&record.payload) else {
            return Ok(None);
        };
        let schedule_id = origin.schedule_id;
        let fire_at_ms = origin.fire_at_ms;
        self.update_schedule_record(&schedule_id, true, |schedule| {
            let matches_in_flight = schedule.view.in_flight_run_id.as_deref()
                == Some(&record.view.run_id)
                || schedule.view.in_flight_fire_at_ms == Some(fire_at_ms);
            let matches_last_dispatch =
                schedule.view.last_dispatched_run_id.as_deref() == Some(&record.view.run_id);
            if !matches_in_flight {
                if matches_last_dispatch && schedule.view.last_fire_at_ms == Some(fire_at_ms) {
                    return Ok((None, false));
                }
                if !matches_last_dispatch
                    && schedule
                        .view
                        .last_fire_at_ms
                        .is_some_and(|last_fire_at_ms| last_fire_at_ms >= fire_at_ms)
                {
                    return Ok((None, false));
                }
            }
            if schedule.view.in_flight_run_id.as_deref() == Some(&record.view.run_id) {
                schedule.view.in_flight_run_id = None;
            }
            if schedule.view.in_flight_fire_at_ms == Some(fire_at_ms) {
                schedule.view.in_flight_fire_at_ms = None;
            }
            schedule.view.last_fire_at_ms = Some(fire_at_ms);
            schedule.view.last_dispatched_run_id = Some(record.view.run_id.clone());
            schedule.view.execution_count = schedule.view.execution_count.saturating_add(1);
            let now = now_ms();
            schedule.view.updated_at_ms = now;
            if record.view.status == DaemonRunStatus::Completed {
                schedule.view.consecutive_failures = 0;
                schedule.view.last_error = None;
            } else {
                schedule.view.consecutive_failures =
                    schedule.view.consecutive_failures.saturating_add(1);
                schedule.view.last_error = record.view.error.clone().or_else(|| {
                    Some(format!(
                        "scheduled run {} ended with status {}",
                        record.view.run_id,
                        serde_json::to_string(&record.view.status)
                            .unwrap_or_else(|_| "unknown".to_string())
                    ))
                });
            }
            let settled_error = schedule.view.last_error.clone();
            record_recent_execution(
                schedule,
                fire_at_ms,
                Some(record.view.run_id.clone()),
                ScheduleExecutionStatus::Settled,
                false,
                now,
                None,
                settled_error,
            );
            if schedule.view.status == ScheduleStatus::Active
                && schedule
                    .view
                    .next_fire_at_ms
                    .is_some_and(|next_fire_at_ms| next_fire_at_ms <= fire_at_ms)
            {
                if let Some(plan) = due_schedule_plan(&schedule.view, fire_at_ms)? {
                    schedule.view.next_fire_at_ms = plan.next_fire_at_ms;
                }
            }
            if schedule
                .view
                .max_executions
                .is_some_and(|limit| schedule.view.execution_count >= limit)
            {
                schedule.view.status = ScheduleStatus::Completed;
                schedule.view.next_fire_at_ms = None;
                schedule.view.queued_fire_at_ms = None;
                schedule.view.paused_remaining_ms = None;
            } else if schedule.view.status == ScheduleStatus::Active
                && schedule.view.next_fire_at_ms.is_none()
                && schedule.view.queued_fire_at_ms.is_none()
            {
                schedule.view.status = ScheduleStatus::Completed;
            }
            Ok((Some(schedule.view.clone()), true))
        })
        .await
    }
}

fn effective_schedule_fire_at_ms(record: &ScheduleRecord, now: u64) -> Option<(u64, bool)> {
    let mut earliest_fire_at_ms = if record.view.overlap_policy
        == crate::ScheduleOverlapPolicy::QueueOne
        && record.view.in_flight_run_id.is_some()
    {
        if record.view.queued_fire_at_ms.is_some() {
            return None;
        }
        record.view.next_fire_at_ms?
    } else {
        [record.view.queued_fire_at_ms, record.view.next_fire_at_ms]
            .into_iter()
            .flatten()
            .min()?
    };
    let backoff_until_ms = record
        .view
        .scheduler_retry_after_ms
        .filter(|backoff_until_ms| *backoff_until_ms > now);
    if earliest_fire_at_ms <= now {
        if let Some(backoff_until_ms) = backoff_until_ms {
            earliest_fire_at_ms = backoff_until_ms;
            return Some((earliest_fire_at_ms, true));
        }
    }
    Some((earliest_fire_at_ms, false))
}

fn should_restore_stale_queued_fire(
    record: &ScheduleRecord,
    fire_at_ms: u64,
    run_id: &str,
) -> bool {
    record.view.recent_executions.iter().rev().any(|entry| {
        entry.fire_at_ms == fire_at_ms
            && entry.run_id.as_deref() == Some(run_id)
            && entry.from_queued_fire
            && matches!(
                entry.status,
                ScheduleExecutionStatus::Claimed | ScheduleExecutionStatus::Dispatched
            )
    })
}

fn remaining_dispatch_slots(record: &ScheduleRecord, non_terminal_run_count: usize) -> usize {
    let Some(max_executions) = record.view.max_executions else {
        return usize::MAX;
    };
    max_executions
        .saturating_sub(record.view.execution_count)
        .saturating_sub(non_terminal_run_count as u64)
        .try_into()
        .unwrap_or(usize::MAX)
}

fn retry_fire_at_ms(record: &ScheduleRecord) -> u64 {
    record
        .view
        .queued_fire_at_ms
        .or(record.view.next_fire_at_ms)
        .or(record.view.in_flight_fire_at_ms)
        .or(record.view.last_fire_at_ms)
        .unwrap_or_else(now_ms)
}

fn retry_delay_ms(policy: &SchedulerPolicyConfig, schedule_id: &str, attempt: u32) -> u64 {
    let base = policy.retry_base_delay_ms.max(1);
    let exponent = attempt.saturating_sub(1).min(20);
    let exponential = base.saturating_mul(1u64 << exponent);
    let capped = exponential.min(policy.retry_max_delay_ms.max(base));
    capped
        .saturating_add(deterministic_retry_jitter_ms(
            schedule_id,
            attempt,
            policy.retry_jitter_ms,
        ))
        .min(policy.retry_max_delay_ms.max(base))
}

fn deterministic_retry_jitter_ms(schedule_id: &str, attempt: u32, jitter_ms: u64) -> u64 {
    if jitter_ms == 0 {
        return 0;
    }
    let mut hasher = DefaultHasher::new();
    schedule_id.hash(&mut hasher);
    attempt.hash(&mut hasher);
    hasher.finish() % jitter_ms.saturating_add(1)
}

fn record_recent_execution(
    record: &mut ScheduleRecord,
    fire_at_ms: u64,
    run_id: Option<String>,
    status: ScheduleExecutionStatus,
    from_queued_fire: bool,
    now: u64,
    retry_after_ms: Option<u64>,
    error: Option<String>,
) {
    let candidate = record
        .view
        .recent_executions
        .iter_mut()
        .rev()
        .find(|entry| {
            entry.fire_at_ms == fire_at_ms
                && match (&entry.run_id, &run_id) {
                    (Some(left), Some(right)) => left == right,
                    (None, None) => true,
                    (Some(_), None) | (None, Some(_)) => false,
                }
        });
    let entry = if let Some(entry) = candidate {
        entry
    } else {
        record.view.recent_executions.push(ScheduleExecutionRecord {
            fire_at_ms,
            run_id: run_id.clone(),
            status: ScheduleExecutionStatus::Claimed,
            attempt: 0,
            from_queued_fire,
            claimed_at_ms: None,
            dispatched_at_ms: None,
            settled_at_ms: None,
            retry_after_ms: None,
            error: None,
        });
        record
            .view
            .recent_executions
            .last_mut()
            .expect("just pushed schedule execution")
    };
    entry.run_id = run_id.or_else(|| entry.run_id.clone());
    entry.status = status;
    entry.attempt = record.view.scheduler_retry_attempt;
    entry.from_queued_fire |= from_queued_fire;
    entry.retry_after_ms = retry_after_ms;
    entry.error = error.or_else(|| entry.error.clone());
    match entry.status {
        ScheduleExecutionStatus::Claimed => {
            entry.claimed_at_ms.get_or_insert(now);
        }
        ScheduleExecutionStatus::Dispatched => {
            entry.claimed_at_ms.get_or_insert(now);
            entry.dispatched_at_ms = Some(now);
        }
        ScheduleExecutionStatus::Retrying => {
            entry.claimed_at_ms.get_or_insert(now);
        }
        ScheduleExecutionStatus::Skipped | ScheduleExecutionStatus::RolledBack => {
            entry.settled_at_ms = Some(now);
        }
        ScheduleExecutionStatus::Settled => {
            entry.settled_at_ms = Some(now);
        }
    }
    let overflow = record
        .view
        .recent_executions
        .len()
        .saturating_sub(DEFAULT_SCHEDULE_RECENT_EXECUTION_LIMIT);
    if overflow > 0 {
        record.view.recent_executions.drain(0..overflow);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::atomic::AtomicU64;

    use anyhow::{Result, anyhow};
    use tempfile::tempdir;

    use super::{
        ScheduleDispatchRequest, ScheduleDueCompletion, ScheduleService, SchedulerSnapshot,
    };
    use crate::runs::{
        DaemonRunKind, DaemonRunStatus, RunRecord, RunRequestPayload, RunRequestSummary, RunView,
        ScheduledRunOrigin,
    };
    use crate::scheduler::{
        FileScheduleStore, ScheduleCadence, ScheduleExecutionRecord, ScheduleExecutionStatus,
        ScheduleMisfirePolicy, ScheduleOverlapPolicy, ScheduleRecord, ScheduleStatus, ScheduleView,
        SchedulerPolicyConfig,
    };
    use crate::{ResolveApprovalsRequest, SubmitInputRequest};

    fn sample_submit_request() -> SubmitInputRequest {
        SubmitInputRequest {
            provider: Some("openai".to_string()),
            source_plugin: None,
            source_kind: None,
            actor_id: None,
            content: "hello".to_string(),
            input_items: Vec::new(),
            attachments: Vec::new(),
            generation: Some(kheish_runtime::ModelGenerationConfig {
                model: Some("gpt-5.4".to_string()),
                ..Default::default()
            }),
            completion_requirements: None,
            metadata: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
            reply_address: None,
        }
    }

    fn sample_schedule(schedule_id: &str, status: ScheduleStatus) -> ScheduleRecord {
        ScheduleRecord {
            view: ScheduleView {
                schedule_id: schedule_id.to_string(),
                name: "test".to_string(),
                target_session_id: "session-1".to_string(),
                target_agent_id: Some("agent-1".to_string()),
                owner_session_id: Some("session-1".to_string()),
                owner_agent_id: Some("agent-1".to_string()),
                created_by_run_id: None,
                status,
                cadence: ScheduleCadence::Once { fire_at_ms: 10_000 },
                overlap_policy: ScheduleOverlapPolicy::Skip,
                misfire_policy: ScheduleMisfirePolicy::CoalesceOnce,
                max_executions: None,
                created_at_ms: 1,
                updated_at_ms: 1,
                next_fire_at_ms: Some(10_000),
                queued_fire_at_ms: None,
                in_flight_fire_at_ms: None,
                in_flight_run_id: None,
                paused_remaining_ms: None,
                last_fire_at_ms: None,
                last_dispatched_run_id: None,
                last_error: None,
                last_scheduler_error: None,
                scheduler_retry_after_ms: None,
                scheduler_retry_attempt: 0,
                execution_count: 0,
                consecutive_failures: 0,
                recent_executions: Vec::new(),
                request: RunRequestSummary {
                    source_plugin: "daemon".to_string(),
                    source_kind: "api".to_string(),
                    actor_id: "api-user".to_string(),
                    text_preview: Some("hello".to_string()),
                    provider: Some("openai".to_string()),
                    model: Some("gpt-5.4".to_string()),
                    approval_count: None,
                    question_count: None,
                },
            },
            request: Some(sample_submit_request()),
            observation_materialization: None,
        }
    }

    fn scheduled_run(run_id: &str, status: DaemonRunStatus) -> RunRecord {
        RunRecord {
            view: RunView {
                run_id: run_id.to_string(),
                session_id: "session-1".to_string(),
                agent_id: "agent-1".to_string(),
                kind: DaemonRunKind::ScheduledInput,
                status,
                submitted_at_ms: 1,
                updated_at_ms: 2,
                started_at_ms: Some(1),
                finished_at_ms: Some(2),
                queued_position: None,
                request: RunRequestSummary {
                    source_plugin: "daemon".to_string(),
                    source_kind: "scheduler".to_string(),
                    actor_id: "scheduler".to_string(),
                    text_preview: Some("hello".to_string()),
                    provider: Some("openai".to_string()),
                    model: Some("gpt-5.4".to_string()),
                    approval_count: None,
                    question_count: None,
                },
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
            reply_targets: Vec::new(),
            payload: RunRequestPayload::ScheduledInput {
                schedule_id: "schedule-1".to_string(),
                fire_at_ms: 10_000,
                request: sample_submit_request(),
            },
        }
    }

    fn scheduled_run_for_fire(run_id: &str, status: DaemonRunStatus, fire_at_ms: u64) -> RunRecord {
        let mut record = scheduled_run(run_id, status);
        match &mut record.payload {
            RunRequestPayload::ScheduledInput {
                fire_at_ms: origin_fire_at_ms,
                ..
            } => *origin_fire_at_ms = fire_at_ms,
            payload => panic!("unexpected scheduled test payload: {payload:?}"),
        }
        record
    }

    fn resumed_scheduled_run(run_id: &str, status: DaemonRunStatus) -> RunRecord {
        RunRecord {
            view: RunView {
                run_id: run_id.to_string(),
                session_id: "session-1".to_string(),
                agent_id: "agent-1".to_string(),
                kind: DaemonRunKind::ScheduledInput,
                status,
                submitted_at_ms: 1,
                updated_at_ms: 2,
                started_at_ms: Some(1),
                finished_at_ms: Some(2),
                queued_position: None,
                request: RunRequestSummary {
                    source_plugin: "daemon".to_string(),
                    source_kind: "scheduler".to_string(),
                    actor_id: "scheduler".to_string(),
                    text_preview: Some("hello".to_string()),
                    provider: Some("openai".to_string()),
                    model: Some("gpt-5.4".to_string()),
                    approval_count: None,
                    question_count: None,
                },
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
            reply_targets: Vec::new(),
            payload: RunRequestPayload::ApprovalResume {
                request: ResolveApprovalsRequest {
                    idempotency_key: None,
                    resolutions: Vec::new(),
                },
                original_request: Some(sample_submit_request()),
                scheduled_origin: Some(ScheduledRunOrigin {
                    schedule_id: "schedule-1".to_string(),
                    fire_at_ms: 10_000,
                }),
                channel_delivery: None,
            },
        }
    }

    #[tokio::test]
    async fn schedule_service_creates_and_lists_schedules() -> Result<()> {
        let temp = tempdir()?;
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::new(),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let view = service
            .create_schedule(sample_schedule("schedule-1", ScheduleStatus::Active))
            .await?;

        assert_eq!(view.schedule_id, "schedule-1");
        assert_eq!(service.list_schedules(Some("session-1")).await.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_pauses_resumes_and_triggers() -> Result<()> {
        let temp = tempdir()?;
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([(
                "schedule-1".to_string(),
                sample_schedule("schedule-1", ScheduleStatus::Active),
            )]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let paused = service.pause_schedule("schedule-1").await?;
        assert_eq!(paused.status, ScheduleStatus::Paused);
        let resumed = service.resume_schedule("schedule-1").await?;
        assert_eq!(resumed.status, ScheduleStatus::Active);
        let triggered = service.trigger_schedule_now("schedule-1").await?;
        assert!(triggered.queued_fire_at_ms.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_marks_and_settles_scheduled_runs() -> Result<()> {
        let temp = tempdir()?;
        let initial = sample_schedule("schedule-1", ScheduleStatus::Active);
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([("schedule-1".to_string(), initial)]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        service
            .mark_schedule_dispatched("schedule-1", 10_000, Some("run-1".to_string()), false, 1)
            .await?;
        service
            .mark_schedule_run_persisted("schedule-1", 10_000, "run-1")
            .await?;
        let settled = service
            .settle_scheduled_run(&scheduled_run("run-1", DaemonRunStatus::Completed))
            .await?
            .expect("scheduled run should settle the schedule");

        assert_eq!(settled.execution_count, 1);
        assert_eq!(settled.consecutive_failures, 0);
        let execution = settled
            .recent_executions
            .last()
            .expect("settled run should record execution history");
        assert_eq!(execution.status, ScheduleExecutionStatus::Settled);
        assert_eq!(execution.run_id.as_deref(), Some("run-1"));
        assert!(execution.claimed_at_ms.is_some());
        assert!(execution.dispatched_at_ms.is_some());
        assert!(execution.settled_at_ms.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_settle_advances_stale_due_boundary() -> Result<()> {
        let temp = tempdir()?;
        let mut initial = sample_schedule("schedule-1", ScheduleStatus::Active);
        initial.view.cadence = ScheduleCadence::Interval { every_seconds: 5 };
        initial.view.next_fire_at_ms = Some(10_000);
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([("schedule-1".to_string(), initial)]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        service
            .mark_schedule_dispatched("schedule-1", 10_000, Some("run-1".to_string()), false, 1)
            .await?;
        service
            .mark_schedule_run_persisted("schedule-1", 10_000, "run-1")
            .await?;
        let settled = service
            .settle_scheduled_run(&scheduled_run("run-1", DaemonRunStatus::Completed))
            .await?
            .expect("scheduled run should settle the schedule");

        assert_eq!(settled.execution_count, 1);
        assert_eq!(settled.next_fire_at_ms, Some(15_000));
        assert_eq!(settled.status, ScheduleStatus::Active);
        assert!(
            service
                .plan_due_schedule("schedule-1", 10_500, &BTreeMap::new())
                .await?
                .is_none(),
            "settle should advance the stale boundary before the worker can duplicate dispatch"
        );
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_settles_resumed_scheduled_runs() -> Result<()> {
        let temp = tempdir()?;
        let initial = sample_schedule("schedule-1", ScheduleStatus::Active);
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([("schedule-1".to_string(), initial)]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        service
            .mark_schedule_dispatched("schedule-1", 10_000, Some("run-1".to_string()), false, 1)
            .await?;
        let settled = service
            .settle_scheduled_run(&resumed_scheduled_run("run-1", DaemonRunStatus::Completed))
            .await?
            .expect("resumed scheduled run should still settle the schedule");

        assert_eq!(settled.execution_count, 1);
        assert!(settled.in_flight_run_id.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_scheduler_snapshot_reports_due_ids_and_next_wakeup() -> Result<()> {
        let temp = tempdir()?;
        let mut active = sample_schedule("schedule-1", ScheduleStatus::Active);
        active.view.next_fire_at_ms = Some(15_000);
        let mut queued = sample_schedule("schedule-2", ScheduleStatus::Active);
        queued.view.next_fire_at_ms = Some(30_000);
        queued.view.queued_fire_at_ms = Some(12_000);
        let paused = sample_schedule("schedule-3", ScheduleStatus::Paused);
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([
                ("schedule-1".to_string(), active),
                ("schedule-2".to_string(), queued),
                ("schedule-3".to_string(), paused),
            ]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let snapshot = service.scheduler_snapshot(16_000).await;
        assert_eq!(
            snapshot,
            SchedulerSnapshot {
                due_schedule_ids: vec!["schedule-1".to_string(), "schedule-2".to_string()],
                next_due_at_ms: Some(12_000),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_status_snapshot_reports_operator_counts() -> Result<()> {
        let temp = tempdir()?;
        let mut due_backoff = sample_schedule("schedule-1", ScheduleStatus::Active);
        due_backoff.view.next_fire_at_ms = Some(500);
        due_backoff.view.scheduler_retry_after_ms = Some(2_000);
        due_backoff.view.scheduler_retry_attempt = 1;
        let mut queued = sample_schedule("schedule-2", ScheduleStatus::Active);
        queued.view.next_fire_at_ms = Some(5_000);
        queued.view.queued_fire_at_ms = Some(600);
        queued.view.in_flight_run_id = Some("run-1".to_string());
        let paused = sample_schedule("schedule-3", ScheduleStatus::Paused);
        let completed = sample_schedule("schedule-4", ScheduleStatus::Completed);
        let mut future_backoff = sample_schedule("schedule-5", ScheduleStatus::Active);
        future_backoff.view.next_fire_at_ms = Some(3_000);
        future_backoff.view.scheduler_retry_after_ms = Some(2_000);
        future_backoff.view.scheduler_retry_attempt = 1;
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([
                ("schedule-1".to_string(), due_backoff),
                ("schedule-2".to_string(), queued),
                ("schedule-3".to_string(), paused),
                ("schedule-4".to_string(), completed),
                ("schedule-5".to_string(), future_backoff),
            ]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let snapshot = service.status_snapshot(1_000).await;
        assert_eq!(snapshot.total, 5);
        assert_eq!(snapshot.active, 3);
        assert_eq!(snapshot.paused, 1);
        assert_eq!(snapshot.completed, 1);
        assert_eq!(snapshot.due_count, 1);
        assert_eq!(snapshot.backoff_count, 1);
        assert_eq!(snapshot.in_flight_count, 1);
        assert_eq!(snapshot.queued_fire_count, 1);
        assert_eq!(snapshot.next_due_at_ms, Some(600));
        assert_eq!(
            snapshot.oldest_due_schedule_id.as_deref(),
            Some("schedule-2")
        );
        assert_eq!(snapshot.oldest_due_schedule_lag_ms, Some(400));
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_scheduler_snapshot_defers_retry_only_for_the_broken_schedule()
    -> Result<()> {
        let temp = tempdir()?;
        let mut broken = sample_schedule("schedule-1", ScheduleStatus::Active);
        broken.view.next_fire_at_ms = Some(10_000);
        let mut healthy = sample_schedule("schedule-2", ScheduleStatus::Active);
        healthy.view.next_fire_at_ms = Some(10_100);
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([
                ("schedule-1".to_string(), broken),
                ("schedule-2".to_string(), healthy),
            ]),
            AtomicU64::new(0),
            SchedulerPolicyConfig {
                retry_base_delay_ms: 500,
                retry_max_delay_ms: 500,
                retry_jitter_ms: 0,
                retry_max_attempts: 0,
            },
        );

        service
            .defer_schedule_retry("schedule-1", 10_000, &anyhow!("missing session"))
            .await?;

        assert_eq!(
            service.scheduler_snapshot(10_000).await,
            SchedulerSnapshot {
                due_schedule_ids: Vec::new(),
                next_due_at_ms: Some(10_100),
            }
        );
        assert_eq!(
            service.scheduler_snapshot(10_150).await,
            SchedulerSnapshot {
                due_schedule_ids: vec!["schedule-2".to_string()],
                next_due_at_ms: Some(10_100),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_plans_queue_one_overlap_when_active_run_exists() -> Result<()> {
        let temp = tempdir()?;
        let mut initial = sample_schedule("schedule-1", ScheduleStatus::Active);
        initial.view.cadence = ScheduleCadence::Interval { every_seconds: 5 };
        initial.view.next_fire_at_ms = Some(10_000);
        initial.view.overlap_policy = ScheduleOverlapPolicy::QueueOne;
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([("schedule-1".to_string(), initial)]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let decision = service
            .plan_due_schedule(
                "schedule-1",
                10_000,
                &BTreeMap::from([(10_000, "run-1".to_string())]),
            )
            .await?
            .expect("active schedule should produce one decision");

        assert!(decision.marks.is_empty());
        assert!(decision.dispatches.is_empty());
        assert_eq!(
            decision.completion,
            Some(ScheduleDueCompletion::QueueFire {
                next_fire_at_ms: Some(15_000),
                queued_fire_at_ms: 10_000,
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_queue_one_inflight_wakes_once_to_persist_queued_fire() -> Result<()> {
        let temp = tempdir()?;
        let mut initial = sample_schedule("schedule-1", ScheduleStatus::Active);
        initial.view.cadence = ScheduleCadence::Interval { every_seconds: 5 };
        initial.view.next_fire_at_ms = Some(10_000);
        initial.view.in_flight_run_id = Some("run-active".to_string());
        initial.view.in_flight_fire_at_ms = Some(5_000);
        initial.view.overlap_policy = ScheduleOverlapPolicy::QueueOne;
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([("schedule-1".to_string(), initial)]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        assert_eq!(
            service.scheduler_snapshot(10_000).await,
            SchedulerSnapshot {
                due_schedule_ids: vec!["schedule-1".to_string()],
                next_due_at_ms: Some(10_000),
            }
        );
        let decision = service
            .plan_due_schedule(
                "schedule-1",
                10_000,
                &BTreeMap::from([(5_000, "run-active".to_string())]),
            )
            .await?
            .expect("queue_one should persist a queued fire while active");
        assert_eq!(
            decision.completion,
            Some(ScheduleDueCompletion::QueueFire {
                next_fire_at_ms: Some(15_000),
                queued_fire_at_ms: 10_000,
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_plans_due_queued_fire_before_recomputing_next_fire() -> Result<()> {
        let temp = tempdir()?;
        let mut initial = sample_schedule("schedule-1", ScheduleStatus::Active);
        initial.view.cadence = ScheduleCadence::Interval { every_seconds: 5 };
        initial.view.next_fire_at_ms = Some(15_000);
        initial.view.queued_fire_at_ms = Some(12_000);
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([("schedule-1".to_string(), initial)]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let decision = service
            .plan_due_schedule("schedule-1", 16_000, &BTreeMap::new())
            .await?
            .expect("queued fire should take precedence");

        assert!(decision.marks.is_empty());
        assert_eq!(
            decision.dispatches,
            vec![ScheduleDispatchRequest {
                fire_at_ms: 12_000,
                from_queued_fire: true,
            }]
        );
        assert!(decision.completion.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_skip_overlap_clears_due_queued_fire_without_spin() -> Result<()> {
        let temp = tempdir()?;
        let mut initial = sample_schedule("schedule-1", ScheduleStatus::Active);
        initial.view.cadence = ScheduleCadence::Interval { every_seconds: 5 };
        initial.view.next_fire_at_ms = Some(20_000);
        initial.view.queued_fire_at_ms = Some(12_000);
        initial.view.in_flight_run_id = Some("run-active".to_string());
        initial.view.in_flight_fire_at_ms = Some(10_000);
        initial.view.overlap_policy = ScheduleOverlapPolicy::Skip;
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([("schedule-1".to_string(), initial)]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let decision = service
            .plan_due_schedule(
                "schedule-1",
                16_000,
                &BTreeMap::from([(10_000, "run-active".to_string())]),
            )
            .await?
            .expect("queued fire should produce a decision");
        assert_eq!(
            decision.completion,
            Some(ScheduleDueCompletion::SkipQueuedFire {
                fire_at_ms: 12_000,
                reason: "queued fire skipped because another scheduled execution is active"
                    .to_string(),
            })
        );
        let skipped = service
            .skip_queued_fire(
                "schedule-1",
                12_000,
                decision.planned_updated_at_ms,
                "queued fire skipped because another scheduled execution is active",
            )
            .await?;
        assert!(skipped);
        let schedule = service.get_schedule("schedule-1").await?;
        assert!(schedule.queued_fire_at_ms.is_none());
        assert_eq!(
            schedule.recent_executions.last().map(|entry| &entry.status),
            Some(&ScheduleExecutionStatus::Skipped)
        );
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_clears_stale_queued_claim_without_losing_fire() -> Result<()> {
        let temp = tempdir()?;
        let mut initial = sample_schedule("schedule-1", ScheduleStatus::Active);
        initial.view.next_fire_at_ms = None;
        initial.view.queued_fire_at_ms = None;
        initial.view.in_flight_fire_at_ms = Some(10_000);
        initial.view.in_flight_run_id = Some("run-1".to_string());
        initial.view.last_dispatched_run_id = Some("run-1".to_string());
        initial.view.recent_executions = vec![ScheduleExecutionRecord {
            fire_at_ms: 10_000,
            run_id: Some("run-1".to_string()),
            status: ScheduleExecutionStatus::Claimed,
            attempt: 0,
            from_queued_fire: true,
            claimed_at_ms: Some(1),
            dispatched_at_ms: None,
            settled_at_ms: None,
            retry_after_ms: None,
            error: None,
        }];
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([("schedule-1".to_string(), initial)]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let restored = service
            .clear_stale_in_flight_run("schedule-1", "run-1", "missing run")
            .await?
            .expect("stale claim should be cleared");

        assert_eq!(restored.queued_fire_at_ms, Some(10_000));
        assert!(restored.in_flight_run_id.is_none());
        assert!(restored.in_flight_fire_at_ms.is_none());
        assert_eq!(
            restored.recent_executions.last().map(|entry| &entry.status),
            Some(&ScheduleExecutionStatus::RolledBack)
        );
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_queue_one_queued_fire_waits_for_inflight_without_due_spin()
    -> Result<()> {
        let temp = tempdir()?;
        let mut initial = sample_schedule("schedule-1", ScheduleStatus::Active);
        initial.view.cadence = ScheduleCadence::Interval { every_seconds: 5 };
        initial.view.next_fire_at_ms = Some(10_000);
        initial.view.queued_fire_at_ms = Some(12_000);
        initial.view.in_flight_run_id = Some("run-active".to_string());
        initial.view.in_flight_fire_at_ms = Some(10_000);
        initial.view.overlap_policy = ScheduleOverlapPolicy::QueueOne;
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([("schedule-1".to_string(), initial)]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let snapshot = service.scheduler_snapshot(16_000).await;
        assert!(snapshot.due_schedule_ids.is_empty());
        assert_eq!(snapshot.next_due_at_ms, None);
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_parallel_dispatches_due_queued_fire_while_inflight() -> Result<()> {
        let temp = tempdir()?;
        let mut initial = sample_schedule("schedule-1", ScheduleStatus::Active);
        initial.view.cadence = ScheduleCadence::Interval { every_seconds: 5 };
        initial.view.next_fire_at_ms = Some(20_000);
        initial.view.queued_fire_at_ms = Some(12_000);
        initial.view.in_flight_run_id = Some("run-active".to_string());
        initial.view.in_flight_fire_at_ms = Some(10_000);
        initial.view.overlap_policy = ScheduleOverlapPolicy::Parallel;
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([("schedule-1".to_string(), initial)]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let decision = service
            .plan_due_schedule(
                "schedule-1",
                16_000,
                &BTreeMap::from([(10_000, "run-active".to_string())]),
            )
            .await?
            .expect("queued fire should produce a decision");
        assert_eq!(
            decision.dispatches,
            vec![ScheduleDispatchRequest {
                fire_at_ms: 12_000,
                from_queued_fire: true,
            }]
        );
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_parallel_settles_older_run_without_clearing_newer_inflight()
    -> Result<()> {
        let temp = tempdir()?;
        let mut initial = sample_schedule("schedule-1", ScheduleStatus::Active);
        initial.view.cadence = ScheduleCadence::Interval { every_seconds: 5 };
        initial.view.next_fire_at_ms = Some(20_000);
        initial.view.overlap_policy = ScheduleOverlapPolicy::Parallel;
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([("schedule-1".to_string(), initial)]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        service
            .mark_schedule_dispatched("schedule-1", 10_000, Some("run-1".to_string()), false, 1)
            .await?;
        service
            .mark_schedule_run_persisted("schedule-1", 10_000, "run-1")
            .await?;
        service
            .mark_schedule_dispatched("schedule-1", 15_000, Some("run-2".to_string()), false, 1)
            .await?;
        service
            .mark_schedule_run_persisted("schedule-1", 15_000, "run-2")
            .await?;

        let first = service
            .settle_scheduled_run(&scheduled_run_for_fire(
                "run-1",
                DaemonRunStatus::Completed,
                10_000,
            ))
            .await?
            .expect("older run should settle");
        assert_eq!(first.execution_count, 1);
        assert_eq!(first.in_flight_run_id.as_deref(), Some("run-2"));
        assert_eq!(first.in_flight_fire_at_ms, Some(15_000));

        let second = service
            .settle_scheduled_run(&scheduled_run_for_fire(
                "run-2",
                DaemonRunStatus::Completed,
                15_000,
            ))
            .await?
            .expect("newer run should settle");
        assert_eq!(second.execution_count, 2);
        assert!(second.in_flight_run_id.is_none());
        assert!(second.in_flight_fire_at_ms.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_max_executions_counts_non_terminal_runs_before_dispatch() -> Result<()>
    {
        let temp = tempdir()?;
        let mut initial = sample_schedule("schedule-1", ScheduleStatus::Active);
        initial.view.cadence = ScheduleCadence::Interval { every_seconds: 5 };
        initial.view.next_fire_at_ms = Some(15_000);
        initial.view.overlap_policy = ScheduleOverlapPolicy::Parallel;
        initial.view.max_executions = Some(1);
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([("schedule-1".to_string(), initial)]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let decision = service
            .plan_due_schedule(
                "schedule-1",
                15_000,
                &BTreeMap::from([(10_000, "run-active".to_string())]),
            )
            .await?
            .expect("schedule should still advance its boundary");

        assert!(decision.dispatches.is_empty());
        assert!(decision.marks.is_empty());
        assert_eq!(
            decision.completion,
            Some(ScheduleDueCompletion::FinalizeNextFire {
                next_fire_at_ms: Some(20_000),
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_persists_retry_backoff_with_deterministic_jitter() -> Result<()> {
        let temp = tempdir()?;
        let initial = sample_schedule("schedule-1", ScheduleStatus::Active);
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([("schedule-1".to_string(), initial)]),
            AtomicU64::new(0),
            SchedulerPolicyConfig {
                retry_base_delay_ms: 500,
                retry_max_delay_ms: 2_000,
                retry_jitter_ms: 250,
                retry_max_attempts: 0,
            },
        );

        service
            .defer_schedule_retry("schedule-1", 1_000, &anyhow!("missing session"))
            .await?;
        let first = service.get_schedule("schedule-1").await?;
        assert_eq!(first.scheduler_retry_attempt, 1);
        assert!(
            first
                .scheduler_retry_after_ms
                .is_some_and(|retry| { (1_500..=1_750).contains(&retry) })
        );
        assert_eq!(
            first
                .recent_executions
                .last()
                .map(|entry| (&entry.status, entry.error.as_deref())),
            Some((&ScheduleExecutionStatus::Retrying, Some("missing session")))
        );

        service.clear_schedule_retry_backoff("schedule-1").await?;
        let cleared = service.get_schedule("schedule-1").await?;
        assert_eq!(cleared.scheduler_retry_attempt, 0);
        assert!(cleared.scheduler_retry_after_ms.is_none());
        assert!(cleared.last_scheduler_error.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_tracks_failure_state_and_resets_on_success() -> Result<()> {
        let temp = tempdir()?;
        let initial = sample_schedule("schedule-1", ScheduleStatus::Active);
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([("schedule-1".to_string(), initial)]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let first_updated_at_ms = service
            .schedule_record("schedule-1")
            .await
            .expect("schedule should exist")
            .view
            .updated_at_ms;
        service
            .mark_schedule_dispatched(
                "schedule-1",
                10_000,
                Some("run-1".to_string()),
                false,
                first_updated_at_ms,
            )
            .await?;
        let failed = service
            .settle_scheduled_run(&scheduled_run("run-1", DaemonRunStatus::Failed))
            .await?
            .expect("failed run should settle the schedule");
        assert_eq!(failed.execution_count, 1);
        assert_eq!(failed.consecutive_failures, 1);
        assert!(failed.last_error.is_some());

        let second_updated_at_ms = service
            .schedule_record("schedule-1")
            .await
            .expect("schedule should exist")
            .view
            .updated_at_ms;
        service
            .mark_schedule_dispatched(
                "schedule-1",
                15_000,
                Some("run-2".to_string()),
                false,
                second_updated_at_ms,
            )
            .await?;
        let recovered = service
            .settle_scheduled_run(&scheduled_run_for_fire(
                "run-2",
                DaemonRunStatus::Completed,
                15_000,
            ))
            .await?
            .expect("successful run should settle the schedule");
        assert_eq!(recovered.execution_count, 2);
        assert_eq!(recovered.consecutive_failures, 0);
        assert!(recovered.last_error.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_settles_unmarked_dispatch_once() -> Result<()> {
        let temp = tempdir()?;
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([(
                "schedule-1".to_string(),
                sample_schedule("schedule-1", ScheduleStatus::Active),
            )]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let first = service
            .settle_scheduled_run(&scheduled_run("run-1", DaemonRunStatus::Completed))
            .await?
            .expect("unmarked dispatch should still settle once");
        let second = service
            .settle_scheduled_run(&scheduled_run("run-1", DaemonRunStatus::Completed))
            .await?;

        assert_eq!(first.execution_count, 1);
        assert!(second.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_rejects_stale_dispatch_claims_after_admin_mutation() -> Result<()> {
        let temp = tempdir()?;
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([(
                "schedule-1".to_string(),
                sample_schedule("schedule-1", ScheduleStatus::Active),
            )]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let decision = service
            .plan_due_schedule("schedule-1", 10_000, &BTreeMap::new())
            .await?
            .expect("schedule should be due");
        service.pause_schedule("schedule-1").await?;

        let claimed = service
            .mark_schedule_dispatched(
                "schedule-1",
                10_000,
                Some("run-1".to_string()),
                false,
                decision.planned_updated_at_ms,
            )
            .await?;
        assert!(!claimed, "stale plan should not claim a paused schedule");

        let schedule = service.get_schedule("schedule-1").await?;
        assert_eq!(schedule.status, ScheduleStatus::Paused);
        assert!(schedule.in_flight_fire_at_ms.is_none());
        assert!(schedule.in_flight_run_id.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_rejects_stale_completion_after_trigger_now() -> Result<()> {
        let temp = tempdir()?;
        let service = ScheduleService::new(
            FileScheduleStore::new(temp.path()),
            BTreeMap::from([(
                "schedule-1".to_string(),
                sample_schedule("schedule-1", ScheduleStatus::Active),
            )]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let decision = service
            .plan_due_schedule("schedule-1", 10_000, &BTreeMap::new())
            .await?
            .expect("schedule should be due");
        service.trigger_schedule_now("schedule-1").await?;

        let applied = service
            .finalize_schedule_next_fire("schedule-1", Some(15_000), decision.planned_updated_at_ms)
            .await?;
        assert!(!applied, "stale plan should not overwrite a fresh trigger");

        let schedule = service.get_schedule("schedule-1").await?;
        assert!(schedule.queued_fire_at_ms.is_some());
        assert_eq!(schedule.next_fire_at_ms, None);
        Ok(())
    }

    #[tokio::test]
    async fn schedule_service_rolls_back_in_memory_state_when_save_fails() -> Result<()> {
        let temp = tempdir()?;
        let store = FileScheduleStore::new(temp.path());
        let initial = sample_schedule("schedule-1", ScheduleStatus::Active);
        store.save_schedule(&initial)?;
        let sabotaged_path = temp.path().join("schedules").join("schedule-1.json");
        fs::remove_file(&sabotaged_path)?;
        fs::create_dir_all(&sabotaged_path)?;
        let service = ScheduleService::new(
            store,
            BTreeMap::from([("schedule-1".to_string(), initial.clone())]),
            AtomicU64::new(0),
            SchedulerPolicyConfig::default(),
        );

        let error = service
            .pause_schedule("schedule-1")
            .await
            .expect_err("schedule persistence should fail");
        assert!(
            error.to_string().contains("failed to replace"),
            "unexpected error: {error}"
        );
        assert_eq!(
            service.get_schedule("schedule-1").await?.status,
            ScheduleStatus::Active
        );
        Ok(())
    }
}
