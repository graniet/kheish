//! Durable daemon-owned schedules and wakeups.

mod store;

use std::str::FromStr;

use anyhow::{Result, anyhow, bail};
use chrono::TimeZone;
use chrono_tz::Tz;
use cron::Schedule;
use serde::{Deserialize, Serialize};

use crate::observations::summarize_observation_materialization_request;
use crate::{
    ObservationMaterializationRequest, RunRequestSummary, SubmitInputRequest,
    summarize_input_request,
};

pub(crate) use store::FileScheduleStore;

pub(crate) const DEFAULT_SCHEDULE_MIN_INTERVAL_SECONDS: u64 = 5;
pub(crate) const DEFAULT_MAX_OWNER_SCHEDULES: usize = 128;
pub(crate) const DEFAULT_SCHEDULE_RECENT_EXECUTION_LIMIT: usize = 50;

/// Global scheduler retry/backoff settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerPolicyConfig {
    /// Base retry delay after scheduler-owned dispatch failures.
    pub retry_base_delay_ms: u64,
    /// Maximum retry delay after exponential backoff and jitter.
    pub retry_max_delay_ms: u64,
    /// Deterministic jitter range added to retry delays.
    pub retry_jitter_ms: u64,
    /// Maximum consecutive scheduler dispatch attempts before pausing the schedule.
    /// `0` means unlimited retries.
    pub retry_max_attempts: u32,
}

impl Default for SchedulerPolicyConfig {
    fn default() -> Self {
        Self {
            retry_base_delay_ms: 500,
            retry_max_delay_ms: 30_000,
            retry_jitter_ms: 250,
            retry_max_attempts: 0,
        }
    }
}
/// The durable lifecycle of one daemon schedule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleStatus {
    Active,
    Paused,
    Completed,
    Canceled,
}

impl ScheduleStatus {
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Canceled)
    }
}

/// How a recurring schedule behaves when an earlier execution is still running.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleOverlapPolicy {
    #[default]
    Skip,
    QueueOne,
    Parallel,
}

/// How a recurring schedule behaves after downtime caused one or more missed fires.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScheduleMisfirePolicy {
    #[default]
    CoalesceOnce,
    SkipMissed,
}

/// Durable status for one recent scheduler fire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleExecutionStatus {
    Claimed,
    Dispatched,
    Retrying,
    Skipped,
    RolledBack,
    Settled,
}

/// Bounded execution history retained directly on the schedule record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleExecutionRecord {
    pub fire_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub status: ScheduleExecutionStatus,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub from_queued_fire: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatched_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The cadence used by one schedule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScheduleCadence {
    Once {
        fire_at_ms: u64,
    },
    Interval {
        every_seconds: u64,
    },
    Cron {
        expression: String,
        #[serde(default = "default_schedule_timezone")]
        timezone: String,
    },
}

/// The externally visible schedule view.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScheduleView {
    pub schedule_id: String,
    pub name: String,
    pub target_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_run_id: Option<String>,
    pub status: ScheduleStatus,
    pub cadence: ScheduleCadence,
    pub overlap_policy: ScheduleOverlapPolicy,
    pub misfire_policy: ScheduleMisfirePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_executions: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fire_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_fire_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_flight_fire_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_flight_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_remaining_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fire_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_dispatched_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scheduler_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler_retry_after_ms: Option<u64>,
    #[serde(default)]
    pub scheduler_retry_attempt: u32,
    #[serde(default)]
    pub execution_count: u64,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_executions: Vec<ScheduleExecutionRecord>,
    pub request: RunRequestSummary,
}

/// One persisted schedule record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScheduleRecord {
    pub view: ScheduleView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<SubmitInputRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_materialization: Option<ObservationMaterializationRequest>,
}

/// The API payload used to create one schedule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScheduleCreateRequest {
    pub name: String,
    pub target_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_run_id: Option<String>,
    pub cadence: ScheduleCadence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_executions: Option<u64>,
    #[serde(default)]
    pub overlap_policy: ScheduleOverlapPolicy,
    #[serde(default)]
    pub misfire_policy: ScheduleMisfirePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<SubmitInputRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_materialization: Option<ObservationMaterializationRequest>,
}

/// One summary used to describe how the worker should treat a due schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DueSchedulePlan {
    pub(crate) fire_times_ms: Vec<u64>,
    pub(crate) next_fire_at_ms: Option<u64>,
}

pub(crate) fn default_schedule_timezone() -> String {
    "UTC".to_string()
}

pub(crate) fn validate_schedule_create_request(request: &ScheduleCreateRequest) -> Result<()> {
    let name = request.name.trim();
    anyhow::ensure!(!name.is_empty(), "schedule name is required");
    anyhow::ensure!(
        !request.target_session_id.trim().is_empty(),
        "target_session_id is required"
    );
    validate_schedule_cadence(&request.cadence)?;
    if let Some(max_executions) = request.max_executions {
        anyhow::ensure!(
            max_executions > 0,
            "max_executions must be greater than zero"
        );
    }
    match (
        request.request.as_ref(),
        request.observation_materialization.as_ref(),
    ) {
        (Some(_), None) => {}
        (None, Some(observation_materialization)) => {
            observation_materialization.validate()?;
            anyhow::ensure!(
                observation_materialization.target_session_id == request.target_session_id,
                "observation_materialization.target_session_id must match target_session_id"
            );
        }
        (Some(_), Some(_)) => {
            bail!("schedule request must define exactly one payload")
        }
        (None, None) => bail!("schedule request must define exactly one payload"),
    }
    Ok(())
}

pub(crate) fn summarize_schedule_create_request(
    request: &ScheduleCreateRequest,
) -> RunRequestSummary {
    match (
        request.request.as_ref(),
        request.observation_materialization.as_ref(),
    ) {
        (Some(input), None) => summarize_input_request(input),
        (None, Some(observation_materialization)) => {
            summarize_observation_materialization_request(observation_materialization)
        }
        _ => unreachable!("schedule requests are validated before they are summarized"),
    }
}

pub(crate) fn validate_schedule_cadence(cadence: &ScheduleCadence) -> Result<()> {
    match cadence {
        ScheduleCadence::Once { fire_at_ms } => {
            anyhow::ensure!(*fire_at_ms > 0, "fire_at_ms must be greater than zero");
        }
        ScheduleCadence::Interval { every_seconds } => {
            anyhow::ensure!(
                *every_seconds >= DEFAULT_SCHEDULE_MIN_INTERVAL_SECONDS,
                "every_seconds must be at least {DEFAULT_SCHEDULE_MIN_INTERVAL_SECONDS}"
            );
        }
        ScheduleCadence::Cron {
            expression,
            timezone,
        } => {
            let schedule = Schedule::from_str(expression)
                .map_err(|error| anyhow!("invalid cron expression: {error}"))?;
            let tz = parse_timezone(timezone)?;
            let first = schedule
                .upcoming(tz)
                .next()
                .ok_or_else(|| anyhow!("cron expression never fires"))?;
            let second = schedule
                .after(&first)
                .next()
                .ok_or_else(|| anyhow!("cron expression fires only once"))?;
            let delta = second.timestamp_millis() - first.timestamp_millis();
            anyhow::ensure!(
                delta >= (DEFAULT_SCHEDULE_MIN_INTERVAL_SECONDS as i64) * 1000,
                "cron interval must be at least {DEFAULT_SCHEDULE_MIN_INTERVAL_SECONDS} seconds"
            );
        }
    }
    Ok(())
}

pub(crate) fn initial_next_fire_at_ms(
    cadence: &ScheduleCadence,
    now_ms: u64,
) -> Result<Option<u64>> {
    match cadence {
        ScheduleCadence::Once { fire_at_ms } => Ok(Some(*fire_at_ms)),
        ScheduleCadence::Interval { every_seconds } => Ok(Some(
            now_ms.saturating_add(every_seconds.saturating_mul(1000)),
        )),
        ScheduleCadence::Cron {
            expression,
            timezone,
        } => {
            let schedule = Schedule::from_str(expression)
                .map_err(|error| anyhow!("invalid cron expression: {error}"))?;
            let tz = parse_timezone(timezone)?;
            let next = schedule
                .after(
                    &tz.timestamp_millis_opt(now_ms as i64)
                        .single()
                        .ok_or_else(|| anyhow!("invalid schedule timestamp"))?,
                )
                .next()
                .ok_or_else(|| anyhow!("cron expression never fires"))?;
            Ok(Some(next.timestamp_millis() as u64))
        }
    }
}

pub(crate) fn build_schedule_record(
    schedule_id: String,
    now_ms: u64,
    request: ScheduleCreateRequest,
) -> Result<ScheduleRecord> {
    let next_fire_at_ms = initial_next_fire_at_ms(&request.cadence, now_ms)?;
    let request_summary = summarize_schedule_create_request(&request);
    Ok(ScheduleRecord {
        view: ScheduleView {
            schedule_id,
            name: request.name,
            target_session_id: request.target_session_id,
            target_agent_id: request.target_agent_id,
            owner_session_id: request.owner_session_id,
            owner_agent_id: request.owner_agent_id,
            created_by_run_id: request.created_by_run_id,
            status: ScheduleStatus::Active,
            cadence: request.cadence,
            overlap_policy: request.overlap_policy,
            misfire_policy: request.misfire_policy,
            max_executions: request.max_executions,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            next_fire_at_ms,
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
            request: request_summary,
        },
        request: request.request,
        observation_materialization: request.observation_materialization,
    })
}

pub(crate) fn parse_timezone(value: &str) -> Result<Tz> {
    value
        .parse::<Tz>()
        .map_err(|error| anyhow!("invalid timezone {value:?}: {error}"))
}

pub(crate) fn due_schedule_plan(
    view: &ScheduleView,
    now_ms: u64,
) -> Result<Option<DueSchedulePlan>> {
    let Some(next_fire_at_ms) = view.next_fire_at_ms else {
        return Ok(None);
    };
    if next_fire_at_ms > now_ms {
        return Ok(None);
    }
    let plan = match &view.cadence {
        ScheduleCadence::Once { .. } => DueSchedulePlan {
            fire_times_ms: vec![next_fire_at_ms],
            next_fire_at_ms: None,
        },
        ScheduleCadence::Interval { every_seconds } => interval_due_plan(
            next_fire_at_ms,
            *every_seconds,
            &view.misfire_policy,
            now_ms,
        ),
        ScheduleCadence::Cron {
            expression,
            timezone,
        } => cron_due_plan(
            next_fire_at_ms,
            expression,
            timezone,
            &view.misfire_policy,
            now_ms,
        )?,
    };
    Ok(Some(plan))
}

fn interval_due_plan(
    next_fire_at_ms: u64,
    every_seconds: u64,
    misfire_policy: &ScheduleMisfirePolicy,
    now_ms: u64,
) -> DueSchedulePlan {
    let step_ms = every_seconds.saturating_mul(1000);
    let skipped = ((now_ms.saturating_sub(next_fire_at_ms)) / step_ms) as u32;
    let due_count = skipped.saturating_add(1);
    let fire_count = match misfire_policy {
        ScheduleMisfirePolicy::CoalesceOnce => 1,
        ScheduleMisfirePolicy::SkipMissed => 0,
    };
    let mut fire_times_ms = Vec::new();
    if fire_count > 0 {
        let start_index = due_count.saturating_sub(fire_count);
        for offset in 0..fire_count {
            fire_times_ms.push(
                next_fire_at_ms
                    .saturating_add((start_index as u64 + offset as u64).saturating_mul(step_ms)),
            );
        }
    }
    DueSchedulePlan {
        fire_times_ms,
        next_fire_at_ms: Some(
            next_fire_at_ms.saturating_add((due_count as u64).saturating_mul(step_ms)),
        ),
    }
}

fn cron_due_plan(
    next_fire_at_ms: u64,
    expression: &str,
    timezone: &str,
    misfire_policy: &ScheduleMisfirePolicy,
    now_ms: u64,
) -> Result<DueSchedulePlan> {
    let schedule = Schedule::from_str(expression)
        .map_err(|error| anyhow!("invalid cron expression: {error}"))?;
    let tz = parse_timezone(timezone)?;
    let now = tz
        .timestamp_millis_opt(now_ms as i64)
        .single()
        .ok_or_else(|| anyhow!("invalid schedule timestamp"))?;
    let next_future_ms = schedule
        .after(&now)
        .next()
        .ok_or_else(|| anyhow!("cron expression never produces a future fire"))?
        .timestamp_millis() as u64;
    let last_due_ms = schedule
        .after(&now)
        .next_back()
        .map(|fire| fire.timestamp_millis() as u64)
        .filter(|fire_at_ms| *fire_at_ms >= next_fire_at_ms)
        .unwrap_or(next_fire_at_ms);
    let fire_times_ms = match misfire_policy {
        ScheduleMisfirePolicy::CoalesceOnce => vec![last_due_ms],
        ScheduleMisfirePolicy::SkipMissed => Vec::new(),
    };
    Ok(DueSchedulePlan {
        fire_times_ms,
        next_fire_at_ms: Some(next_future_ms),
    })
}

pub(crate) fn resume_schedule_next_fire_at_ms(
    view: &ScheduleView,
    now_ms: u64,
) -> Result<Option<u64>> {
    let remaining_ms = view.paused_remaining_ms.unwrap_or(0);
    match &view.cadence {
        ScheduleCadence::Once { .. } | ScheduleCadence::Interval { .. } => {
            Ok(Some(now_ms.saturating_add(remaining_ms)))
        }
        ScheduleCadence::Cron { .. } => initial_next_fire_at_ms(&view.cadence, now_ms),
    }
}

pub(crate) fn resolved_request_for_schedule(
    schedule_id: &str,
    mut request: SubmitInputRequest,
    fire_at_ms: u64,
) -> SubmitInputRequest {
    if request
        .source_plugin
        .as_deref()
        .is_none_or(|value| value.is_empty())
    {
        request.source_plugin = Some("scheduler".to_string());
    }
    if request
        .source_kind
        .as_deref()
        .is_none_or(|value| value.is_empty())
    {
        request.source_kind = Some("scheduled_input".to_string());
    }
    if request
        .actor_id
        .as_deref()
        .is_none_or(|value| value.is_empty())
    {
        request.actor_id = Some(schedule_id.to_string());
    }
    let mut metadata = request.metadata.unwrap_or_default();
    if !metadata.is_object() {
        metadata = serde_json::json!({});
    }
    if let Some(object) = metadata.as_object_mut() {
        object.insert("schedule_id".to_string(), serde_json::json!(schedule_id));
        object.insert(
            "scheduled_for_ms".to_string(),
            serde_json::json!(fire_at_ms),
        );
    }
    request.metadata = Some(metadata);
    request
}

pub(crate) fn resolved_observation_materialization_request_for_schedule(
    schedule_id: &str,
    mut request: ObservationMaterializationRequest,
    fire_at_ms: u64,
) -> ObservationMaterializationRequest {
    let had_source_kind = request
        .request
        .source_kind
        .as_deref()
        .filter(|value| !value.is_empty())
        .is_some();
    request.request = resolved_request_for_schedule(schedule_id, request.request, fire_at_ms);
    if !had_source_kind {
        request.request.source_kind = Some("scheduled_observation_materialization".to_string());
    }
    request
}

#[cfg(test)]
mod tests {
    use chrono::{Datelike, Timelike};

    use super::*;

    #[test]
    fn interval_due_plan_coalesces_to_one_fire() {
        let view = ScheduleView {
            schedule_id: "schedule-1".to_string(),
            name: "demo".to_string(),
            target_session_id: "session-1".to_string(),
            target_agent_id: None,
            owner_session_id: Some("session-1".to_string()),
            owner_agent_id: Some("agent-1".to_string()),
            created_by_run_id: None,
            status: ScheduleStatus::Active,
            cadence: ScheduleCadence::Interval { every_seconds: 5 },
            overlap_policy: ScheduleOverlapPolicy::Skip,
            misfire_policy: ScheduleMisfirePolicy::CoalesceOnce,
            max_executions: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            next_fire_at_ms: Some(5_000),
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
                source_plugin: "scheduler".to_string(),
                source_kind: "scheduled_input".to_string(),
                actor_id: "schedule-1".to_string(),
                text_preview: Some("hello".to_string()),
                provider: None,
                model: None,
                approval_count: None,
                question_count: None,
            },
        };
        let plan = due_schedule_plan(&view, 16_000)
            .expect("plan should compute")
            .expect("schedule should be due");
        assert_eq!(plan.fire_times_ms, vec![15_000]);
        assert_eq!(plan.next_fire_at_ms, Some(20_000));
    }

    #[test]
    fn interval_due_plan_can_skip_missed_occurrences() {
        let view = ScheduleView {
            misfire_policy: ScheduleMisfirePolicy::SkipMissed,
            cadence: ScheduleCadence::Interval { every_seconds: 5 },
            schedule_id: "schedule-1".to_string(),
            name: "demo".to_string(),
            target_session_id: "session-1".to_string(),
            target_agent_id: None,
            owner_session_id: None,
            owner_agent_id: None,
            created_by_run_id: None,
            status: ScheduleStatus::Active,
            overlap_policy: ScheduleOverlapPolicy::Skip,
            max_executions: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            next_fire_at_ms: Some(5_000),
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
                source_plugin: "scheduler".to_string(),
                source_kind: "scheduled_input".to_string(),
                actor_id: "schedule-1".to_string(),
                text_preview: Some("hello".to_string()),
                provider: None,
                model: None,
                approval_count: None,
                question_count: None,
            },
        };
        let plan = due_schedule_plan(&view, 16_000)
            .expect("plan should compute")
            .expect("schedule should be due");
        assert!(plan.fire_times_ms.is_empty());
        assert_eq!(plan.next_fire_at_ms, Some(20_000));
    }

    #[test]
    fn cron_initial_fire_skips_nonexistent_dst_time() {
        let cadence = ScheduleCadence::Cron {
            expression: "0 30 2 * * *".to_string(),
            timezone: "Europe/Paris".to_string(),
        };
        let tz = parse_timezone("Europe/Paris").expect("timezone should parse");
        let before_gap = tz
            .with_ymd_and_hms(2026, 3, 29, 0, 0, 0)
            .single()
            .expect("pre-DST timestamp should be valid")
            .timestamp_millis() as u64;

        let next = initial_next_fire_at_ms(&cadence, before_gap)
            .expect("cron should compute")
            .expect("cron should have next fire");
        let local = tz
            .timestamp_millis_opt(next as i64)
            .single()
            .expect("next fire should be an unambiguous instant");

        assert_eq!((local.year(), local.month(), local.day()), (2026, 3, 30));
        assert_eq!((local.hour(), local.minute()), (2, 30));
    }

    #[test]
    fn cron_fall_back_coalesces_ambiguous_hour_to_one_due_fire() {
        let cadence = ScheduleCadence::Cron {
            expression: "0 30 2 * * *".to_string(),
            timezone: "Europe/Paris".to_string(),
        };
        let tz = parse_timezone("Europe/Paris").expect("timezone should parse");
        let before_overlap = tz
            .with_ymd_and_hms(2026, 10, 25, 0, 0, 0)
            .single()
            .expect("pre-DST timestamp should be valid")
            .timestamp_millis() as u64;
        let first = initial_next_fire_at_ms(&cadence, before_overlap)
            .expect("cron should compute")
            .expect("cron should have next fire");
        let now_after_overlap = tz
            .with_ymd_and_hms(2026, 10, 25, 4, 0, 0)
            .single()
            .expect("post-DST timestamp should be valid")
            .timestamp_millis() as u64;
        let view = ScheduleView {
            schedule_id: "schedule-1".to_string(),
            name: "dst".to_string(),
            target_session_id: "session-1".to_string(),
            target_agent_id: None,
            owner_session_id: None,
            owner_agent_id: None,
            created_by_run_id: None,
            status: ScheduleStatus::Active,
            cadence,
            overlap_policy: ScheduleOverlapPolicy::Skip,
            misfire_policy: ScheduleMisfirePolicy::CoalesceOnce,
            max_executions: None,
            created_at_ms: before_overlap,
            updated_at_ms: before_overlap,
            next_fire_at_ms: Some(first),
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
                source_plugin: "scheduler".to_string(),
                source_kind: "scheduled_input".to_string(),
                actor_id: "schedule-1".to_string(),
                text_preview: Some("hello".to_string()),
                provider: None,
                model: None,
                approval_count: None,
                question_count: None,
            },
        };

        let plan = due_schedule_plan(&view, now_after_overlap)
            .expect("cron should compute")
            .expect("cron should be due");
        assert_eq!(plan.fire_times_ms.len(), 1);
        assert!(
            plan.next_fire_at_ms
                .is_some_and(|next| next > now_after_overlap)
        );
    }

    #[test]
    fn cron_due_plan_bounds_large_catch_up() {
        let cadence = ScheduleCadence::Cron {
            expression: "*/5 * * * * *".to_string(),
            timezone: "UTC".to_string(),
        };
        let view = ScheduleView {
            schedule_id: "schedule-1".to_string(),
            name: "catch-up".to_string(),
            target_session_id: "session-1".to_string(),
            target_agent_id: None,
            owner_session_id: None,
            owner_agent_id: None,
            created_by_run_id: None,
            status: ScheduleStatus::Active,
            cadence,
            overlap_policy: ScheduleOverlapPolicy::Skip,
            misfire_policy: ScheduleMisfirePolicy::CoalesceOnce,
            max_executions: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            next_fire_at_ms: Some(5_000),
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
                source_plugin: "scheduler".to_string(),
                source_kind: "scheduled_input".to_string(),
                actor_id: "schedule-1".to_string(),
                text_preview: Some("hello".to_string()),
                provider: None,
                model: None,
                approval_count: None,
                question_count: None,
            },
        };

        let plan = due_schedule_plan(&view, 90 * 24 * 60 * 60 * 1000)
            .expect("cron should compute")
            .expect("cron should be due");
        assert_eq!(plan.fire_times_ms.len(), 1);
        assert!(
            plan.fire_times_ms[0] >= 90 * 24 * 60 * 60 * 1000 - 10_000,
            "coalesced fire should be the latest missed cron fire, not the bounded scan cutoff"
        );
        assert!(
            plan.next_fire_at_ms
                .is_some_and(|next| next > 90 * 24 * 60 * 60 * 1000)
        );
    }
}
