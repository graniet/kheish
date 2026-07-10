//! Durable daemon-owned shared channels, messages, reactions, and turn leases.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kheish_session::{
    append_json_line_sync, prepare_storage_path_for_write, resolve_storage_path_for_read,
    write_json_pretty_atomically,
};
use kheish_types::{ActorRef, RichOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state_files::read_json_or_quarantine;

fn default_next_channel_id() -> u64 {
    1
}

fn default_next_message_id() -> u64 {
    1
}

fn default_next_turn_id() -> u64 {
    1
}

fn default_next_stimulus_id() -> u64 {
    1
}

/// Durable pacing state for the autonomous channel heartbeat worker.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelHeartbeatStateView {
    /// The owning channel identifier.
    pub channel_id: String,
    /// True while a granted autonomous turn is still awaiting its next-poll outcome.
    #[serde(default)]
    pub pending_grant: bool,
    /// Latest channel message id observed by the heartbeat scorer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_message_id: Option<String>,
    /// Consecutive autonomous grants that produced no new channel activity.
    #[serde(default)]
    pub consecutive_silent: u32,
    /// When the heartbeat last granted an autonomous turn.
    #[serde(default)]
    pub last_turn_at_ms: u64,
    /// Monotonic per-channel turn phase used to space fresh topics.
    #[serde(default)]
    pub turn_counter: u64,
    /// Per-session timestamp of the last autonomous grant for fair rotation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub last_granted: BTreeMap<String, u64>,
    /// Whether the heartbeat has put this channel to sleep until new activity arrives.
    #[serde(default)]
    pub dormant: bool,
    /// Latest heartbeat state update timestamp.
    #[serde(default)]
    pub updated_at_ms: u64,
}

/// The durable participation mode for one channel member.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelParticipationMode {
    /// The member never receives daemon-selected autonomous turns.
    ManualOnly,
    /// The member participates only when explicitly selected by channel arbitration.
    #[default]
    SelectedOnly,
    /// The member is preferred when already active in the thread.
    PreferSelected,
    /// The member is always eligible for daemon arbitration.
    AlwaysListen,
}

/// The durable kind of one channel member.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelMemberKind {
    /// One human actor identified only by daemon-stable actor metadata.
    HumanActor,
    /// One agent-backed daemon session.
    Session,
}

/// The durable arbitration policy for one channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelAutonomyPolicy {
    /// The maximum number of parallel public speakers allowed per thread.
    pub max_parallel_public_speakers: u32,
    /// The maximum number of autonomous agent replies allowed after one human message.
    pub max_agent_replies_per_human_message: u32,
    /// The member cooldown in milliseconds before the same session may speak again.
    pub member_cooldown_ms: u64,
    /// The public turn lease duration in milliseconds.
    pub lease_timeout_ms: u64,
    /// The maximum number of pending or claimed autonomous stimuli retained for one channel.
    pub max_pending_stimuli: u32,
    /// The maximum number of autonomous root posts allowed inside one rolling hour window.
    pub max_autonomous_root_posts_per_hour: u32,
    /// The maximum number of autonomous root threads that may remain active at the same time.
    pub max_active_autonomous_roots: u32,
    /// The quiet-period after one autonomous root post before another root may open.
    pub quiet_period_ms_after_root_post: u64,
}

impl Default for ChannelAutonomyPolicy {
    fn default() -> Self {
        Self {
            max_parallel_public_speakers: 1,
            max_agent_replies_per_human_message: 3,
            member_cooldown_ms: 15_000,
            lease_timeout_ms: 60_000,
            max_pending_stimuli: 32,
            max_autonomous_root_posts_per_hour: 4,
            max_active_autonomous_roots: 1,
            quiet_period_ms_after_root_post: 300_000,
        }
    }
}

/// Computed moderation and anti-storm counters for one channel.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelModerationMetricsView {
    /// Active turn leases currently controlling public speakers.
    #[serde(default)]
    pub active_turn_lease_count: u64,
    /// Turn leases that already have a live public delivery run.
    #[serde(default)]
    pub active_public_speaker_count: u64,
    /// Pending autonomous stimuli waiting for the worker.
    #[serde(default)]
    pub pending_stimulus_count: u64,
    /// Claimed autonomous stimuli that will be retried or dispatched by the worker.
    #[serde(default)]
    pub claimed_stimulus_count: u64,
    /// Canonical thread-work items that are not terminal.
    #[serde(default)]
    pub active_thread_work_count: u64,
    /// Autonomous root posts materialized in the last rolling hour.
    #[serde(default)]
    pub recent_autonomous_root_posts_1h: u64,
    /// Stimuli suppressed by coalescing, supersession, cancellation, or budgets.
    #[serde(default)]
    pub suppressed_stimulus_count: u64,
}

/// The durable scope used by one autonomous channel stimulus.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelStimulusScope {
    /// The stimulus may open one new top-level subject in the main channel feed.
    #[default]
    Channel,
    /// The stimulus may only continue one existing canonical thread.
    Thread,
}

/// The caller hint that influences where the daemon should surface one stimulus.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelStimulusVisibilityHint {
    /// Let the daemon choose the smallest valid surface based on the scope and kind.
    #[default]
    Auto,
    /// Publish the canonical public marker in the main feed.
    Main,
    /// Publish the canonical public marker inside one existing thread.
    Thread,
}

/// The durable lifecycle state of one queued channel stimulus.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelStimulusState {
    /// The stimulus is pending and may still be claimed by the worker.
    Pending,
    /// The worker has claimed the stimulus and is deciding whether to publish it.
    Claimed,
    /// The stimulus already materialized into the public channel timeline.
    Dispatched,
    /// The stimulus was intentionally merged into a newer equivalent request.
    Coalesced,
    /// The stimulus was replaced by a newer equivalent request before dispatch.
    Superseded,
    /// The stimulus was explicitly canceled or expired.
    Cancelled,
}

impl ChannelStimulusState {
    /// Returns true when the stimulus no longer requires worker attention.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Dispatched | Self::Coalesced | Self::Superseded | Self::Cancelled
        )
    }
}

/// The durable semantic kind of one autonomous channel stimulus.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelStimulusKind {
    /// One agent proposed a new idea or line of work.
    #[default]
    AgentIdea,
    /// One schedule fired and should wake the bound public thread.
    ScheduleFire,
    /// One scheduled run settled and should publish progress into the bound thread.
    ScheduleResult,
    /// One background task completed and should publish progress.
    TaskCompleted,
    /// One reviewer run produced new findings.
    ReviewCompleted,
    /// One sidechain reached a durable milestone.
    SidechainMilestone,
    /// One observation stream produced a new materialized record.
    ObservationMaterialized,
    /// One thread stayed idle long enough to justify a follow-up wake-up.
    ThreadIdleFollowUp,
    /// One canonical result summary should be promoted into the main feed.
    ResultSummary,
    /// One stale progress marker was superseded by a newer update.
    SupersessionNotice,
}

/// The durable role of one canonical root thread.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelThreadTopicKind {
    /// The root thread was opened by a human-authored message.
    #[default]
    Human,
    /// The root thread was opened autonomously by the daemon.
    AutonomousRoot,
    /// The root thread acts as one summary or result announcement.
    Summary,
}

/// The durable lifecycle state of one canonical root thread.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelThreadWorkStatus {
    /// The work item was proposed but not yet actively pursued.
    #[default]
    Proposed,
    /// The thread currently represents active work.
    Active,
    /// The thread currently waits on a review or validation step.
    Reviewing,
    /// The thread reached a completed state.
    Completed,
    /// The thread was superseded by a newer canonical subject.
    Superseded,
}

/// The durable kind used when binding daemon work to one canonical root thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelWorkBindingKind {
    /// The binding tracks one daemon schedule.
    Schedule,
    /// The binding tracks one daemon run identifier.
    Run,
    /// The binding tracks one spawned sidechain agent.
    SidechainAgent,
    /// The binding tracks one daemon task identifier.
    Task,
    /// The binding tracks one observation record or source.
    Observation,
}

/// One durable queued stimulus that can wake a channel without new human input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelStimulusView {
    /// The stable daemon-owned stimulus identifier.
    pub stimulus_id: String,
    /// The owning channel identifier.
    pub channel_id: String,
    /// The scope that constrains where the daemon may surface the stimulus.
    pub scope: ChannelStimulusScope,
    /// The canonical thread targeted by the stimulus when it is thread-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_root_message_id: Option<String>,
    /// The durable worker state.
    pub state: ChannelStimulusState,
    /// The semantic kind used for dedupe, policy, and rendering.
    pub kind: ChannelStimulusKind,
    /// The preferred presentation surface for the public marker.
    #[serde(default)]
    pub visibility_hint: ChannelStimulusVisibilityHint,
    /// The public text that should be materialized before channel arbitration continues.
    pub content: String,
    /// The members explicitly preferred when the stimulus opens a new social turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addressed_member_ids: Vec<String>,
    /// The optional session that requested the stimulus and should appear as the public sender.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_session_id: Option<String>,
    /// The optional actor identifier used when the stimulus does not originate from a session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_actor_id: Option<String>,
    /// The optional display name override used for non-session stimulus senders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_display_name: Option<String>,
    /// The caller-reported source kind such as `schedule`, `sidechain`, or `agent_idea`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    /// The stable source reference such as a schedule id or run id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    /// One optional dedupe key used to coalesce equivalent pending stimuli.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    /// One optional progress key used for superseding stale progress updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_key: Option<String>,
    /// The creation timestamp in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// The earliest timestamp when the worker may process the stimulus.
    pub available_at_ms: u64,
    /// The expiration timestamp after which the stimulus should be canceled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// The latest worker claim timestamp when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at_ms: Option<u64>,
    /// The dispatch timestamp when the stimulus materialized publicly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatched_at_ms: Option<u64>,
    /// The latest operational error observed while processing this stimulus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// One durable binding from daemon work to a canonical public root thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelWorkBindingView {
    /// The binding kind.
    pub binding_kind: ChannelWorkBindingKind,
    /// The stable identifier of the bound daemon work item.
    pub binding_ref: String,
    /// The timestamp when the binding was recorded.
    pub bound_at_ms: u64,
    /// The latest stimulus derived from the bound work item when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_stimulus_id: Option<String>,
}

/// One durable supersedable progress snapshot for a canonical root thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelProgressSnapshotView {
    /// The stable progress key used for supersession.
    pub progress_key: String,
    /// The source kind associated with this progress stream.
    pub source_kind: String,
    /// The stable source reference such as a schedule id or run id.
    pub source_ref: String,
    /// The latest public message identifier that materialized this progress marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_message_id: Option<String>,
    /// The latest update timestamp in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
    /// One optional summary digest used to suppress duplicates across retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_digest: Option<String>,
}

/// One durable operational state record attached to a canonical public root thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelThreadWorkStateView {
    /// The owning channel identifier.
    pub channel_id: String,
    /// The canonical root thread identifier.
    pub thread_root_message_id: String,
    /// The durable topic role of the root thread.
    #[serde(default)]
    pub topic_kind: ChannelThreadTopicKind,
    /// The durable work status of the thread.
    #[serde(default)]
    pub status: ChannelThreadWorkStatus,
    /// The current owner session when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    /// The stable initiative key used to dedupe autonomous roots when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiative_key: Option<String>,
    /// The high-level source kind that opened the canonical root thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    /// The stable source reference that opened the canonical root thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    /// The latest stimulus that touched this thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_stimulus_id: Option<String>,
    /// The latest timestamp when the thread received a durable signal.
    pub last_signal_at_ms: u64,
    /// The latest timestamp when the thread promoted one autonomous summary to the main feed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_main_promotion_at_ms: Option<u64>,
    /// The bound daemon work items that should continue to report into this thread.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<ChannelWorkBindingView>,
    /// The latest progress snapshots keyed by progress stream.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub progress_snapshots: Vec<ChannelProgressSnapshotView>,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// One durable channel member record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelMemberView {
    /// The stable member identifier inside the channel namespace.
    pub member_id: String,
    /// The durable member kind.
    pub member_kind: ChannelMemberKind,
    /// How this member's display name should evolve over time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name_mode: Option<ChannelMemberDisplayNameMode>,
    /// The human-readable member display name.
    pub display_name: String,
    /// The bound daemon session identifier when this member is session-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The bound daemon actor identifier when this member is a human actor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    /// The optional role label used by channel arbitration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The optional expertise tags used by channel arbitration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expertise_tags: Vec<String>,
    /// The durable participation mode for this member.
    #[serde(default)]
    pub participation_mode: ChannelParticipationMode,
    /// Whether the member is muted from autonomous speaking.
    #[serde(default)]
    pub muted: bool,
    /// The channel join timestamp in milliseconds since the Unix epoch.
    pub joined_at_ms: u64,
}

/// How one channel member display name should be managed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelMemberDisplayNameMode {
    /// Keep the channel-local label exactly as stored.
    #[default]
    Manual,
    /// Follow the effective visible name of the bound agent session.
    FollowAgent,
}

/// One aggregated public reaction summary attached to a channel message.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelReactionView {
    /// The reaction emoji or token.
    pub emoji: String,
    /// The actor identifiers that currently hold this reaction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actor_ids: Vec<String>,
    /// The current stable reaction count.
    pub count: u64,
}

/// One durable public channel message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelMessageView {
    /// The stable daemon-owned message identifier.
    pub message_id: String,
    /// The owning channel identifier.
    pub channel_id: String,
    /// The thread root identifier when this message belongs to one thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_root_message_id: Option<String>,
    /// The direct parent message when this message is a public reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
    /// The durable sender identity.
    pub sender: ActorRef,
    /// The bound daemon session identifier when the sender is session-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_session_id: Option<String>,
    /// The ordered addressed members explicitly targeted by the message author.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addressed_member_ids: Vec<String>,
    /// The durable public content payload.
    pub output: RichOutput,
    /// The creation timestamp in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// The current aggregated reactions attached to this message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<ChannelReactionView>,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// One compact channel summary returned by list APIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelSummaryView {
    /// The stable daemon-owned channel identifier.
    pub channel_id: String,
    /// The user-visible channel title.
    pub title: String,
    /// The optional short description shown in room lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The optional longer purpose text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// The current channel member count.
    #[serde(default)]
    pub member_count: u64,
    /// The current channel message count.
    #[serde(default)]
    pub message_count: u64,
    /// The latest message identifier when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_id: Option<String>,
    /// The latest message preview when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_preview: Option<String>,
    /// The creation timestamp in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// The last update timestamp in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
    /// Whether autonomous speaking is paused.
    #[serde(default)]
    pub paused: bool,
}

/// One full channel record returned by detail APIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelView {
    /// The compact externally visible channel summary.
    #[serde(flatten)]
    pub summary: ChannelSummaryView,
    /// The durable channel members.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ChannelMemberView>,
    /// The daemon-owned pinned assets shared in the channel header.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_asset_ids: Vec<String>,
    /// The stable creator actor identifier.
    pub created_by: String,
    /// The durable channel autonomy policy.
    #[serde(default)]
    pub autonomy_policy: ChannelAutonomyPolicy,
    /// The default member participation mode for new members.
    #[serde(default)]
    pub default_participation_mode: ChannelParticipationMode,
    /// Computed operational counters used to audit moderation and anti-storm behavior.
    #[serde(default)]
    pub moderation_metrics: ChannelModerationMetricsView,
    /// Internal autonomous heartbeat pacing state when this channel has been observed by the worker.
    #[serde(default, skip_serializing)]
    pub heartbeat_state: Option<ChannelHeartbeatStateView>,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// One durable public turn lease used to prevent reply storms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelTurnLeaseView {
    /// The stable daemon-owned turn identifier.
    pub turn_id: String,
    /// The owning channel identifier.
    pub channel_id: String,
    /// The thread root this lease controls.
    pub thread_root_message_id: String,
    /// The triggering public message identifier.
    pub origin_message_id: String,
    /// The current holder session identifier.
    pub holder_session_id: String,
    /// The public run identifier when the holder is already scheduled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run_id: Option<String>,
    /// The timed-out run this lease explicitly replaced during a handoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_run_id: Option<String>,
    /// The timed-out turn this lease explicitly replaced during a handoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_turn_id: Option<String>,
    /// The remaining autonomous reply budget for the current human turn.
    pub remaining_reply_budget: u32,
    /// Whether this lease was granted by daemon autonomous channel pacing.
    #[serde(default)]
    pub autonomous: bool,
    /// Whether this autonomous lease should post a brand-new top-level root.
    #[serde(default)]
    pub autonomous_new_topic: bool,
    /// The lease expiration timestamp in milliseconds since the Unix epoch.
    pub expires_at_ms: u64,
    /// The candidate sessions considered next when the holder abstains or times out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_candidate_session_ids: Vec<String>,
    /// The current human-origin message that anchors the active public turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_human_origin_message_id: Option<String>,
    /// The last human message that refreshed the current thread budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_human_message_id: Option<String>,
    /// The latest preferred session order derived from the last human message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_human_priority_session_ids: Vec<String>,
    /// The current autonomous reply count since the last human message.
    #[serde(default)]
    pub agent_reply_count_since_last_human: u32,
}

/// One compact channel index entry used for listing and repair.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelIndexEntry {
    /// The stable daemon-owned channel identifier.
    pub channel_id: String,
    /// The user-visible channel title.
    pub title: String,
    /// The current update timestamp.
    pub updated_at_ms: u64,
    /// The current message count.
    pub message_count: u64,
}

impl From<&ChannelView> for ChannelIndexEntry {
    fn from(value: &ChannelView) -> Self {
        Self {
            channel_id: value.summary.channel_id.clone(),
            title: value.summary.title.clone(),
            updated_at_ms: value.summary.updated_at_ms,
            message_count: value.summary.message_count,
        }
    }
}

/// The durable channel index stored beside daemon state files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelIndex {
    /// The next numeric channel identifier seed.
    #[serde(default = "default_next_channel_id")]
    pub next_channel_id: u64,
    /// The next numeric message identifier seed.
    #[serde(default = "default_next_message_id")]
    pub next_message_id: u64,
    /// The next numeric turn identifier seed.
    #[serde(default = "default_next_turn_id")]
    pub next_turn_id: u64,
    /// The next numeric stimulus identifier seed.
    #[serde(default = "default_next_stimulus_id")]
    pub next_stimulus_id: u64,
    /// The compact channel summaries keyed by identifier.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub channels: BTreeMap<String, ChannelIndexEntry>,
}

impl Default for ChannelIndex {
    fn default() -> Self {
        Self {
            next_channel_id: default_next_channel_id(),
            next_message_id: default_next_message_id(),
            next_turn_id: default_next_turn_id(),
            next_stimulus_id: default_next_stimulus_id(),
            channels: BTreeMap::new(),
        }
    }
}

/// One append-only channel event entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelEventEntry {
    /// The owning channel identifier.
    pub channel_id: String,
    /// The durable event timestamp in milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// The typed event payload.
    pub event: ChannelEvent,
}

/// One durable public channel event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChannelEvent {
    /// One public message was posted.
    MessagePosted { message: ChannelMessageView },
    /// One reaction was added to a message.
    ReactionSet {
        message_id: String,
        actor_id: String,
        emoji: String,
    },
    /// One reaction was removed from a message.
    ReactionUnset {
        message_id: String,
        actor_id: String,
        emoji: String,
    },
    /// One channel member joined.
    MemberJoined { member: ChannelMemberView },
    /// One channel member left.
    MemberLeft { member_id: String },
}

/// Filesystem-backed channel storage rooted under one daemon state directory.
#[derive(Clone, Debug)]
pub(crate) struct FileChannelStore {
    root: PathBuf,
}

impl FileChannelStore {
    /// Creates a new channel store rooted under one daemon state directory.
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("channel-index.json")
    }

    fn channels_root(&self) -> PathBuf {
        self.root.join("channels")
    }

    fn events_root(&self) -> PathBuf {
        self.root.join("channel-events")
    }

    fn leases_root(&self) -> PathBuf {
        self.root.join("channel-leases")
    }

    fn stimuli_root(&self) -> PathBuf {
        self.root.join("channel-stimuli")
    }

    fn thread_state_root(&self) -> PathBuf {
        self.root.join("channel-thread-state")
    }

    fn heartbeat_state_root(&self) -> PathBuf {
        self.root.join("channel-heartbeats")
    }

    fn channel_path(&self, channel_id: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.channels_root(), channel_id, "json")
    }

    fn events_path(&self, channel_id: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.events_root(), channel_id, "jsonl")
    }

    fn lease_path(&self, channel_id: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.leases_root(), channel_id, "json")
    }

    fn stimuli_path(&self, channel_id: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.stimuli_root(), channel_id, "json")
    }

    fn thread_state_path(&self, channel_id: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.thread_state_root(), channel_id, "json")
    }

    fn heartbeat_state_path(&self, channel_id: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.heartbeat_state_root(), channel_id, "json")
    }

    /// Loads the persisted channel index, tolerating corruption by quarantining broken files.
    pub(crate) fn load_index(&self) -> Result<ChannelIndex> {
        Ok(read_json_or_quarantine(&self.index_path(), "channel index")?.unwrap_or_default())
    }

    /// Persists the current channel index atomically.
    pub(crate) fn save_index(&self, index: &ChannelIndex) -> Result<()> {
        write_json_pretty_atomically(&self.index_path(), index)
    }

    /// Loads every persisted channel record, quarantining corrupted files.
    pub(crate) fn load_channels(&self) -> Result<BTreeMap<String, ChannelView>> {
        load_json_records::<ChannelView>(&self.channels_root(), "channel record").map(|records| {
            let mut map = BTreeMap::new();
            for record in records {
                map.insert(record.summary.channel_id.clone(), record);
            }
            map
        })
    }

    /// Loads every persisted channel turn lease snapshot, quarantining corrupted files.
    pub(crate) fn load_leases(&self) -> Result<BTreeMap<String, Vec<ChannelTurnLeaseView>>> {
        load_json_records::<Vec<ChannelTurnLeaseView>>(&self.leases_root(), "channel lease").map(
            |records| {
                let mut map = BTreeMap::new();
                for leases in records {
                    if let Some(channel_id) = leases.first().map(|lease| lease.channel_id.clone()) {
                        map.insert(channel_id, leases);
                    }
                }
                map
            },
        )
    }

    /// Loads every persisted channel stimulus snapshot, quarantining corrupted files.
    pub(crate) fn load_stimuli(&self) -> Result<BTreeMap<String, Vec<ChannelStimulusView>>> {
        load_json_records::<Vec<ChannelStimulusView>>(&self.stimuli_root(), "channel stimulus").map(
            |records| {
                let mut map = BTreeMap::new();
                for stimuli in records {
                    if let Some(channel_id) =
                        stimuli.first().map(|stimulus| stimulus.channel_id.clone())
                    {
                        map.insert(channel_id, stimuli);
                    }
                }
                map
            },
        )
    }

    /// Loads every persisted channel thread-work snapshot, quarantining corrupted files.
    pub(crate) fn load_thread_states(
        &self,
    ) -> Result<BTreeMap<String, Vec<ChannelThreadWorkStateView>>> {
        load_json_records::<Vec<ChannelThreadWorkStateView>>(
            &self.thread_state_root(),
            "channel thread work state",
        )
        .map(|records| {
            let mut map = BTreeMap::new();
            for states in records {
                if let Some(channel_id) = states.first().map(|state| state.channel_id.clone()) {
                    map.insert(channel_id, states);
                }
            }
            map
        })
    }

    /// Loads every persisted channel heartbeat snapshot, quarantining corrupted files.
    pub(crate) fn load_heartbeat_states(
        &self,
    ) -> Result<BTreeMap<String, ChannelHeartbeatStateView>> {
        load_json_records::<ChannelHeartbeatStateView>(
            &self.heartbeat_state_root(),
            "channel heartbeat state",
        )
        .map(|records| {
            let mut map = BTreeMap::new();
            for state in records {
                if !state.channel_id.trim().is_empty() {
                    map.insert(state.channel_id.clone(), state);
                }
            }
            map
        })
    }

    /// Loads the append-only event log for one channel, quarantining corrupted files.
    pub(crate) fn load_events(&self, channel_id: &str) -> Result<Vec<ChannelEventEntry>> {
        read_jsonl_records(&self.events_path(channel_id), "channel event")
    }

    /// Persists one channel record atomically.
    pub(crate) fn save_channel(&self, channel: &ChannelView) -> Result<()> {
        let path = prepare_storage_path_for_write(
            &self.channels_root(),
            &channel.summary.channel_id,
            "json",
        )?;
        write_json_pretty_atomically(&path, channel)
    }

    /// Appends one channel event to the durable event log.
    pub(crate) fn append_event(&self, entry: &ChannelEventEntry) -> Result<()> {
        let path = prepare_storage_path_for_write(&self.events_root(), &entry.channel_id, "jsonl")?;
        append_json_line_sync(&path, entry)
    }

    /// Persists the current turn leases for one channel atomically.
    pub(crate) fn save_leases(
        &self,
        channel_id: &str,
        leases: &[ChannelTurnLeaseView],
    ) -> Result<()> {
        let path = prepare_storage_path_for_write(&self.leases_root(), channel_id, "json")?;
        write_json_pretty_atomically(&path, &leases.to_vec())
    }

    /// Deletes one persisted channel turn lease snapshot when it exists.
    pub(crate) fn delete_leases(&self, channel_id: &str) -> Result<()> {
        delete_if_exists(self.lease_path(channel_id), "channel leases")
    }

    /// Persists the current channel stimuli for one channel atomically.
    pub(crate) fn save_stimuli(
        &self,
        channel_id: &str,
        stimuli: &[ChannelStimulusView],
    ) -> Result<()> {
        let path = prepare_storage_path_for_write(&self.stimuli_root(), channel_id, "json")?;
        write_json_pretty_atomically(&path, &stimuli.to_vec())
    }

    /// Deletes one persisted channel stimulus snapshot when it exists.
    pub(crate) fn delete_stimuli(&self, channel_id: &str) -> Result<()> {
        delete_if_exists(self.stimuli_path(channel_id), "channel stimuli")
    }

    /// Persists the current thread-work state for one channel atomically.
    pub(crate) fn save_thread_states(
        &self,
        channel_id: &str,
        states: &[ChannelThreadWorkStateView],
    ) -> Result<()> {
        let path = prepare_storage_path_for_write(&self.thread_state_root(), channel_id, "json")?;
        write_json_pretty_atomically(&path, &states.to_vec())
    }

    /// Persists one channel heartbeat snapshot atomically.
    pub(crate) fn save_heartbeat_state(
        &self,
        channel_id: &str,
        state: &ChannelHeartbeatStateView,
    ) -> Result<()> {
        let path =
            prepare_storage_path_for_write(&self.heartbeat_state_root(), channel_id, "json")?;
        let mut state = state.clone();
        state.channel_id = channel_id.to_string();
        state
            .last_granted
            .retain(|session_id, _| !session_id.trim().is_empty());
        write_json_pretty_atomically(&path, &state)
    }

    /// Deletes one persisted channel heartbeat snapshot when it exists.
    pub(crate) fn delete_heartbeat_state(&self, channel_id: &str) -> Result<()> {
        delete_if_exists(
            self.heartbeat_state_path(channel_id),
            "channel heartbeat state",
        )
    }

    /// Deletes one persisted channel thread-work snapshot when it exists.
    pub(crate) fn delete_thread_states(&self, channel_id: &str) -> Result<()> {
        delete_if_exists(
            self.thread_state_path(channel_id),
            "channel thread work state",
        )
    }

    /// Deletes one persisted channel record and its sidecar files when they exist.
    pub(crate) fn delete_channel(&self, channel_id: &str) -> Result<()> {
        delete_if_exists(self.channel_path(channel_id), "channel record")?;
        delete_if_exists(self.events_path(channel_id), "channel events")?;
        delete_if_exists(self.lease_path(channel_id), "channel leases")?;
        delete_if_exists(self.stimuli_path(channel_id), "channel stimuli")?;
        delete_if_exists(
            self.thread_state_path(channel_id),
            "channel thread work state",
        )?;
        delete_if_exists(
            self.heartbeat_state_path(channel_id),
            "channel heartbeat state",
        )
    }
}

fn delete_if_exists(path: PathBuf, label: &str) -> Result<()> {
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to delete {label} {}", path.display()))
        }
    }
}

fn load_json_records<T>(root: &Path, label: &'static str) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for scan_root in storage_scan_roots(root) {
        if !scan_root.exists() {
            continue;
        }
        for entry in fs::read_dir(scan_root)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(record) = read_json_or_quarantine::<T>(&path, label)? {
                records.push(record);
            }
        }
    }
    Ok(records)
}

fn read_jsonl_records<T>(path: &Path, label: &'static str) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read {label} {}", path.display()));
        }
    };
    let mut records = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<T>(line) {
            Ok(record) => records.push(record),
            Err(error) => {
                let quarantine_path = quarantine_path_for(path, index as u64);
                fs::rename(path, &quarantine_path).with_context(|| {
                    format!(
                        "failed to quarantine corrupt {label} {} after decode error: {error}",
                        path.display()
                    )
                })?;
                return Ok(records);
            }
        }
    }
    Ok(records)
}

/// Returns whether one top-level message opened an autonomous root: a stimulus promotion
/// or an autonomous new-topic delivery. Shared by the moderation metrics and the
/// root-budget counters so both sides always agree on what counts as an autonomous root.
pub(crate) fn channel_message_is_autonomous_root(message: &ChannelMessageView) -> bool {
    match message.metadata.get("source_kind").and_then(Value::as_str) {
        Some("channel_stimulus") => true,
        Some("channel_delivery") => message
            .metadata
            .get("autonomous_new_topic")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        _ => false,
    }
}

fn quarantine_path_for(path: &Path, suffix: u64) -> PathBuf {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let mut file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("corrupt")
        .to_string();
    if !extension.is_empty() {
        file_name.push_str(&format!(".corrupt-{suffix}"));
    }
    path.with_file_name(file_name)
}

fn storage_scan_roots(root: &Path) -> [PathBuf; 2] {
    [root.to_path_buf(), root.join("__safe")]
}
