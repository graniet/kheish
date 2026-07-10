use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};
use tokio::sync::{Mutex, Notify};

use crate::channels::{
    ChannelAutonomyPolicy, ChannelEvent, ChannelEventEntry, ChannelHeartbeatStateView,
    ChannelIndex, ChannelIndexEntry, ChannelMemberView, ChannelMessageView,
    ChannelParticipationMode, ChannelProgressSnapshotView, ChannelReactionView,
    ChannelStimulusState, ChannelStimulusView, ChannelSummaryView, ChannelThreadWorkStateView,
    ChannelTurnLeaseView, ChannelView, ChannelWorkBindingKind, ChannelWorkBindingView,
    FileChannelStore,
};
use crate::now_ms;
use kheish_types::{ActorRef, RichOutput};
use serde_json::Value;

#[derive(Clone, Debug, Default)]
struct ChannelState {
    channels: BTreeMap<String, ChannelView>,
    messages: BTreeMap<String, BTreeMap<String, ChannelMessageView>>,
    ordered_message_ids: BTreeMap<String, Vec<String>>,
    leases: BTreeMap<String, BTreeMap<String, ChannelTurnLeaseView>>,
    stimuli: BTreeMap<String, BTreeMap<String, ChannelStimulusView>>,
    thread_states: BTreeMap<String, BTreeMap<String, ChannelThreadWorkStateView>>,
    heartbeat_states: BTreeMap<String, ChannelHeartbeatStateView>,
}

impl ChannelState {
    fn new(
        channels: BTreeMap<String, ChannelView>,
        messages: BTreeMap<String, BTreeMap<String, ChannelMessageView>>,
        leases: BTreeMap<String, BTreeMap<String, ChannelTurnLeaseView>>,
        stimuli: BTreeMap<String, BTreeMap<String, ChannelStimulusView>>,
        thread_states: BTreeMap<String, BTreeMap<String, ChannelThreadWorkStateView>>,
        heartbeat_states: BTreeMap<String, ChannelHeartbeatStateView>,
    ) -> Self {
        let mut ordered_message_ids = BTreeMap::<String, Vec<String>>::new();
        for (channel_id, messages) in &messages {
            let mut ordered = messages.values().cloned().collect::<Vec<_>>();
            ordered.sort_by(|left, right| {
                left.created_at_ms
                    .cmp(&right.created_at_ms)
                    .then_with(|| left.message_id.cmp(&right.message_id))
            });
            ordered_message_ids.insert(
                channel_id.clone(),
                ordered
                    .into_iter()
                    .map(|message| message.message_id)
                    .collect(),
            );
        }
        Self {
            channels,
            messages,
            ordered_message_ids,
            leases,
            stimuli,
            thread_states,
            heartbeat_states,
        }
    }
}

/// One durable request used to create a channel.
#[derive(Clone, Debug)]
pub(crate) struct CreateChannelRecord {
    pub(crate) channel_id: String,
    pub(crate) title: String,
    pub(crate) description: Option<String>,
    pub(crate) purpose: Option<String>,
    pub(crate) pinned_asset_ids: Vec<String>,
    pub(crate) created_by: String,
    pub(crate) created_at_ms: u64,
    pub(crate) members: Vec<ChannelMemberView>,
    pub(crate) autonomy_policy: ChannelAutonomyPolicy,
    pub(crate) default_participation_mode: ChannelParticipationMode,
    pub(crate) metadata: Value,
}

/// One durable request used to create a public channel message.
#[derive(Clone, Debug)]
pub(crate) struct CreateChannelMessageRecord {
    pub(crate) channel_id: String,
    pub(crate) message_id: String,
    pub(crate) sender: ActorRef,
    pub(crate) sender_session_id: Option<String>,
    pub(crate) addressed_member_ids: Vec<String>,
    pub(crate) reply_to_message_id: Option<String>,
    pub(crate) requested_thread_root_message_id: Option<String>,
    pub(crate) output: RichOutput,
    pub(crate) created_at_ms: u64,
    pub(crate) metadata: Value,
}

/// One durable request used to change a reaction on a public channel message.
#[derive(Clone, Debug)]
pub(crate) struct ChannelReactionMutation {
    pub(crate) channel_id: String,
    pub(crate) message_id: String,
    pub(crate) actor_id: String,
    pub(crate) emoji: String,
    pub(crate) timestamp_ms: u64,
}

/// One due stimulus candidate discovered by the stimulus worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DueChannelStimulus {
    pub(crate) channel_id: String,
    pub(crate) stimulus_id: String,
}

/// One compact scheduler snapshot for queued channel stimuli.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChannelStimulusSchedulerSnapshot {
    pub(crate) due: Vec<DueChannelStimulus>,
    pub(crate) next_due_at_ms: Option<u64>,
}

/// One expired lease candidate discovered by the lease worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExpiredChannelLease {
    pub(crate) channel_id: String,
    pub(crate) turn_id: String,
    pub(crate) thread_root_message_id: String,
}

/// One compact scheduler snapshot for channel turn leases.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChannelLeaseSchedulerSnapshot {
    pub(crate) expired: Vec<ExpiredChannelLease>,
    pub(crate) next_expiry_ms: Option<u64>,
}

/// Owns durable channels, public message logs, reactions, and turn leases.
pub(crate) struct ChannelService {
    store: FileChannelStore,
    state: Mutex<ChannelState>,
    index: Mutex<ChannelIndex>,
    notify: Notify,
    next_channel_id: AtomicU64,
    next_message_id: AtomicU64,
    next_turn_id: AtomicU64,
    next_stimulus_id: AtomicU64,
}

impl ChannelService {
    /// Creates a new channel service backed by persisted daemon state.
    pub(crate) fn new(
        store: FileChannelStore,
        index: ChannelIndex,
        channels: BTreeMap<String, ChannelView>,
        messages: BTreeMap<String, BTreeMap<String, ChannelMessageView>>,
        leases: BTreeMap<String, BTreeMap<String, ChannelTurnLeaseView>>,
        stimuli: BTreeMap<String, BTreeMap<String, ChannelStimulusView>>,
        thread_states: BTreeMap<String, BTreeMap<String, ChannelThreadWorkStateView>>,
        heartbeat_states: BTreeMap<String, ChannelHeartbeatStateView>,
        next_channel_id: AtomicU64,
        next_message_id: AtomicU64,
        next_turn_id: AtomicU64,
        next_stimulus_id: AtomicU64,
    ) -> Self {
        Self {
            store,
            state: Mutex::new(ChannelState::new(
                channels,
                messages,
                leases,
                stimuli,
                thread_states,
                heartbeat_states,
            )),
            notify: Notify::new(),
            next_channel_id,
            next_message_id,
            next_turn_id,
            next_stimulus_id,
            index: Mutex::new(index),
        }
    }

    /// Returns one fresh daemon-managed channel identifier.
    pub(crate) fn next_channel_id(&self) -> String {
        format!(
            "channel-{}",
            self.next_channel_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Returns one fresh daemon-managed channel message identifier.
    pub(crate) fn next_message_id(&self) -> String {
        format!(
            "channel-message-{}",
            self.next_message_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Returns one fresh daemon-managed turn identifier.
    pub(crate) fn next_turn_id(&self) -> String {
        format!(
            "channel-turn-{}",
            self.next_turn_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Returns one fresh daemon-managed channel stimulus identifier.
    pub(crate) fn next_stimulus_id(&self) -> String {
        format!(
            "channel-stimulus-{}",
            self.next_stimulus_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Returns the worker notification primitive used for lease scheduling.
    pub(crate) fn notify(&self) -> &Notify {
        &self.notify
    }

    /// Lists channels filtered by optional query.
    pub(crate) async fn list_channels(&self, query: Option<&str>) -> Vec<ChannelView> {
        let query = query
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        let state = self.state.lock().await;
        let mut channels = state
            .channels
            .values()
            .filter(|channel| {
                query.as_ref().is_none_or(|query| {
                    channel
                        .summary
                        .channel_id
                        .to_ascii_lowercase()
                        .contains(query)
                        || channel.summary.title.to_ascii_lowercase().contains(query)
                        || channel
                            .summary
                            .description
                            .as_deref()
                            .unwrap_or_default()
                            .to_ascii_lowercase()
                            .contains(query)
                })
            })
            .map(|channel| channel_with_metrics(&state, channel, now_ms()))
            .collect::<Vec<_>>();
        channels.sort_by(|left, right| left.summary.channel_id.cmp(&right.summary.channel_id));
        channels
    }

    /// Lists channels without computed metrics for internal workers that only need policy/member data.
    pub(crate) async fn list_channels_lightweight(&self, query: Option<&str>) -> Vec<ChannelView> {
        let query = query
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        let state = self.state.lock().await;
        let mut channels = state
            .channels
            .values()
            .filter(|channel| {
                query.as_ref().is_none_or(|query| {
                    channel
                        .summary
                        .channel_id
                        .to_ascii_lowercase()
                        .contains(query)
                        || channel.summary.title.to_ascii_lowercase().contains(query)
                        || channel
                            .summary
                            .description
                            .as_deref()
                            .unwrap_or_default()
                            .to_ascii_lowercase()
                            .contains(query)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        channels.sort_by(|left, right| left.summary.channel_id.cmp(&right.summary.channel_id));
        channels
    }

    /// Returns one channel by identifier.
    pub(crate) async fn get_channel(&self, channel_id: &str) -> Result<ChannelView> {
        let state = self.state.lock().await;
        state
            .channels
            .get(channel_id)
            .map(|channel| channel_with_metrics(&state, channel, now_ms()))
            .ok_or_else(|| anyhow!("unknown channel {channel_id}"))
    }

    /// Returns the ordered public messages for one channel, optionally restricted to one thread.
    pub(crate) async fn list_messages(
        &self,
        channel_id: &str,
        thread_root_message_id: Option<&str>,
    ) -> Result<Vec<ChannelMessageView>> {
        let state = self.state.lock().await;
        let Some(channel) = state.channels.get(channel_id) else {
            bail!("unknown channel {channel_id}");
        };
        let mut messages = state
            .ordered_message_ids
            .get(&channel.summary.channel_id)
            .into_iter()
            .flat_map(|message_ids| message_ids.iter())
            .filter_map(|message_id| state.messages.get(channel_id)?.get(message_id))
            .filter(|message| {
                thread_root_message_id.is_none_or(|thread_root_message_id| {
                    message.thread_root_message_id.as_deref() == Some(thread_root_message_id)
                        || message.message_id == thread_root_message_id
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        messages.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.message_id.cmp(&right.message_id))
        });
        Ok(messages)
    }

    /// Returns the channel's most recent message, using the same ordering as
    /// `list_messages` (created-at, then message id). One lock-scoped scan and a
    /// single clone, so hot pollers (the heartbeat worker) never copy a channel's
    /// full history just to observe its tail.
    pub(crate) async fn latest_message(
        &self,
        channel_id: &str,
    ) -> Result<Option<ChannelMessageView>> {
        let state = self.state.lock().await;
        anyhow::ensure!(
            state.channels.contains_key(channel_id),
            "unknown channel {channel_id}"
        );
        Ok(state
            .messages
            .get(channel_id)
            .and_then(|messages| {
                messages.values().max_by(|left, right| {
                    left.created_at_ms
                        .cmp(&right.created_at_ms)
                        .then_with(|| left.message_id.cmp(&right.message_id))
                })
            })
            .cloned())
    }

    /// Returns one durable public channel message.
    pub(crate) async fn get_message(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<ChannelMessageView> {
        let state = self.state.lock().await;
        state
            .messages
            .get(channel_id)
            .and_then(|messages| messages.get(message_id))
            .cloned()
            .ok_or_else(|| anyhow!("unknown channel message {message_id}"))
    }

    /// Returns the active turn leases for one channel.
    pub(crate) async fn list_leases(&self, channel_id: &str) -> Result<Vec<ChannelTurnLeaseView>> {
        let state = self.state.lock().await;
        let Some(channel) = state.channels.get(channel_id) else {
            bail!("unknown channel {channel_id}");
        };
        let mut leases = state
            .leases
            .get(&channel.summary.channel_id)
            .into_iter()
            .flat_map(|leases| leases.values())
            .cloned()
            .collect::<Vec<_>>();
        leases.sort_by(|left, right| {
            left.expires_at_ms
                .cmp(&right.expires_at_ms)
                .then_with(|| left.turn_id.cmp(&right.turn_id))
        });
        Ok(leases)
    }

    /// Returns the current expired and next-due lease state for the worker loop.
    pub(crate) async fn lease_scheduler_snapshot(
        &self,
        now_ms: u64,
    ) -> ChannelLeaseSchedulerSnapshot {
        let state = self.state.lock().await;
        let mut expired = Vec::new();
        let mut next_expiry_ms = None;
        for (channel_id, leases) in &state.leases {
            for lease in leases.values() {
                if lease.expires_at_ms <= now_ms {
                    expired.push(ExpiredChannelLease {
                        channel_id: channel_id.clone(),
                        turn_id: lease.turn_id.clone(),
                        thread_root_message_id: lease.thread_root_message_id.clone(),
                    });
                } else {
                    next_expiry_ms = Some(
                        next_expiry_ms
                            .map(|current: u64| current.min(lease.expires_at_ms))
                            .unwrap_or(lease.expires_at_ms),
                    );
                }
            }
        }
        ChannelLeaseSchedulerSnapshot {
            expired,
            next_expiry_ms,
        }
    }

    /// Returns the current due and next-due stimulus state for the worker loop.
    pub(crate) async fn stimulus_scheduler_snapshot(
        &self,
        now_ms: u64,
    ) -> ChannelStimulusSchedulerSnapshot {
        let state = self.state.lock().await;
        let mut due = Vec::new();
        let mut next_due_at_ms = None;
        for (channel_id, stimuli) in &state.stimuli {
            for stimulus in stimuli.values() {
                if stimulus.state != ChannelStimulusState::Pending {
                    continue;
                }
                if stimulus
                    .expires_at_ms
                    .is_some_and(|expires_at_ms| expires_at_ms <= now_ms)
                {
                    due.push(DueChannelStimulus {
                        channel_id: channel_id.clone(),
                        stimulus_id: stimulus.stimulus_id.clone(),
                    });
                    continue;
                }
                if stimulus.available_at_ms <= now_ms {
                    due.push(DueChannelStimulus {
                        channel_id: channel_id.clone(),
                        stimulus_id: stimulus.stimulus_id.clone(),
                    });
                } else {
                    next_due_at_ms = Some(
                        next_due_at_ms
                            .map(|current: u64| current.min(stimulus.available_at_ms))
                            .unwrap_or(stimulus.available_at_ms),
                    );
                }
            }
        }
        ChannelStimulusSchedulerSnapshot {
            due,
            next_due_at_ms,
        }
    }

    /// Lists the persisted autonomous stimuli for one channel.
    pub(crate) async fn list_stimuli(
        &self,
        channel_id: &str,
        thread_root_message_id: Option<&str>,
        state_filter: Option<ChannelStimulusState>,
    ) -> Result<Vec<ChannelStimulusView>> {
        let state = self.state.lock().await;
        let Some(channel) = state.channels.get(channel_id) else {
            bail!("unknown channel {channel_id}");
        };
        let mut stimuli = state
            .stimuli
            .get(&channel.summary.channel_id)
            .into_iter()
            .flat_map(|stimuli| stimuli.values())
            .filter(|stimulus| {
                thread_root_message_id.is_none_or(|thread_root_message_id| {
                    stimulus.thread_root_message_id.as_deref() == Some(thread_root_message_id)
                })
            })
            .filter(|stimulus| {
                state_filter
                    .as_ref()
                    .is_none_or(|state_filter| &stimulus.state == state_filter)
            })
            .cloned()
            .collect::<Vec<_>>();
        stimuli.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.stimulus_id.cmp(&right.stimulus_id))
        });
        Ok(stimuli)
    }

    /// Returns one persisted channel stimulus by identifier.
    pub(crate) async fn get_stimulus(
        &self,
        channel_id: &str,
        stimulus_id: &str,
    ) -> Result<ChannelStimulusView> {
        let state = self.state.lock().await;
        state
            .stimuli
            .get(channel_id)
            .and_then(|stimuli| stimuli.get(stimulus_id))
            .cloned()
            .ok_or_else(|| anyhow!("unknown channel stimulus {stimulus_id}"))
    }

    /// Persists one new channel stimulus after coalescing equivalent pending entries.
    pub(crate) async fn create_stimulus(
        &self,
        mut stimulus: ChannelStimulusView,
    ) -> Result<ChannelStimulusView> {
        let mut state = self.state.lock().await;
        let Some(channel) = state.channels.get(&stimulus.channel_id) else {
            bail!("unknown channel {}", stimulus.channel_id);
        };
        let max_pending_stimuli = channel.autonomy_policy.max_pending_stimuli as usize;
        if let Some(thread_root_message_id) = stimulus.thread_root_message_id.as_deref() {
            anyhow::ensure!(
                state
                    .messages
                    .get(&stimulus.channel_id)
                    .and_then(|messages| messages.get(thread_root_message_id))
                    .is_some_and(|message| message.thread_root_message_id.is_none()),
                "thread_root_message_id {thread_root_message_id} must point at an existing thread root"
            );
        }
        let now = now_ms();
        let stimuli = state
            .stimuli
            .entry(stimulus.channel_id.clone())
            .or_default();
        let superseded_ids = stimuli
            .values()
            .filter(|existing| {
                matches!(
                    existing.state,
                    ChannelStimulusState::Pending | ChannelStimulusState::Claimed
                ) && stimuli_equivalent(existing, &stimulus)
            })
            .map(|existing| existing.stimulus_id.clone())
            .collect::<Vec<_>>();
        for stimulus_id in superseded_ids {
            if let Some(existing) = stimuli.get_mut(&stimulus_id) {
                existing.state = ChannelStimulusState::Superseded;
                existing.dispatched_at_ms = Some(now);
                existing.last_error = None;
            }
        }
        let pending_count = stimuli
            .values()
            .filter(|candidate| {
                matches!(
                    candidate.state,
                    ChannelStimulusState::Pending | ChannelStimulusState::Claimed
                )
            })
            .count();
        anyhow::ensure!(
            pending_count < max_pending_stimuli,
            "channel {} already has {} pending stimuli",
            stimulus.channel_id,
            max_pending_stimuli
        );

        stimulus.available_at_ms = stimulus.available_at_ms.max(stimulus.created_at_ms);
        stimuli.insert(stimulus.stimulus_id.clone(), stimulus.clone());
        self.persist_stimuli_locked(&state, &stimulus.channel_id)?;
        let mut index = self.index.lock().await;
        index.next_stimulus_id = self.next_stimulus_id.load(Ordering::Relaxed);
        self.store.save_index(&index)?;
        self.notify.notify_waiters();
        Ok(stimulus)
    }

    /// Claims one pending stimulus for worker processing.
    pub(crate) async fn claim_stimulus(
        &self,
        channel_id: &str,
        stimulus_id: &str,
        now_ms: u64,
    ) -> Result<Option<ChannelStimulusView>> {
        let mut state = self.state.lock().await;
        let Some(stimulus) = state
            .stimuli
            .get_mut(channel_id)
            .and_then(|stimuli| stimuli.get_mut(stimulus_id))
        else {
            return Ok(None);
        };
        if stimulus.state != ChannelStimulusState::Pending {
            return Ok(None);
        }
        stimulus.state = ChannelStimulusState::Claimed;
        stimulus.claimed_at_ms = Some(now_ms);
        let claimed = stimulus.clone();
        self.persist_stimuli_locked(&state, channel_id)?;
        self.notify.notify_waiters();
        Ok(Some(claimed))
    }

    /// Replaces the state of one already persisted stimulus.
    pub(crate) async fn set_stimulus_state(
        &self,
        channel_id: &str,
        stimulus_id: &str,
        next_state: ChannelStimulusState,
        now_ms: u64,
        last_error: Option<String>,
    ) -> Result<Option<ChannelStimulusView>> {
        let mut state = self.state.lock().await;
        let Some(stimulus) = state
            .stimuli
            .get_mut(channel_id)
            .and_then(|stimuli| stimuli.get_mut(stimulus_id))
        else {
            return Ok(None);
        };
        stimulus.state = next_state;
        stimulus.last_error = last_error;
        if stimulus.state.is_terminal() {
            stimulus.dispatched_at_ms = Some(now_ms);
        }
        let snapshot = stimulus.clone();
        self.persist_stimuli_locked(&state, channel_id)?;
        self.notify.notify_waiters();
        Ok(Some(snapshot))
    }

    /// Updates one persisted stimulus in place.
    pub(crate) async fn update_stimulus(
        &self,
        channel_id: &str,
        stimulus_id: &str,
        update: impl FnOnce(&mut ChannelStimulusView) -> Result<bool>,
    ) -> Result<Option<ChannelStimulusView>> {
        let mut state = self.state.lock().await;
        let previous = {
            let Some(stimulus) = state
                .stimuli
                .get_mut(channel_id)
                .and_then(|stimuli| stimuli.get_mut(stimulus_id))
            else {
                return Ok(None);
            };
            let previous = stimulus.clone();
            if !update(stimulus)? {
                return Ok(Some(previous));
            }
            previous
        };
        if let Err(error) = self.persist_stimuli_locked(&state, channel_id) {
            if let Some(stimulus) = state
                .stimuli
                .get_mut(channel_id)
                .and_then(|stimuli| stimuli.get_mut(stimulus_id))
            {
                *stimulus = previous;
            }
            return Err(error);
        }
        let Some(snapshot) = state
            .stimuli
            .get(channel_id)
            .and_then(|stimuli| stimuli.get(stimulus_id))
            .cloned()
        else {
            return Ok(None);
        };
        self.notify.notify_waiters();
        Ok(Some(snapshot))
    }

    /// Lists the current canonical thread-work state for one channel.
    pub(crate) async fn list_thread_states(
        &self,
        channel_id: &str,
    ) -> Result<Vec<ChannelThreadWorkStateView>> {
        let state = self.state.lock().await;
        let Some(channel) = state.channels.get(channel_id) else {
            bail!("unknown channel {channel_id}");
        };
        let mut states = state
            .thread_states
            .get(&channel.summary.channel_id)
            .into_iter()
            .flat_map(|states| states.values())
            .cloned()
            .collect::<Vec<_>>();
        states.sort_by(|left, right| {
            left.thread_root_message_id
                .cmp(&right.thread_root_message_id)
        });
        Ok(states)
    }

    /// Returns the canonical work state for one root thread when it exists.
    pub(crate) async fn get_thread_state(
        &self,
        channel_id: &str,
        thread_root_message_id: &str,
    ) -> Result<Option<ChannelThreadWorkStateView>> {
        let state = self.state.lock().await;
        Ok(state
            .thread_states
            .get(channel_id)
            .and_then(|states| states.get(thread_root_message_id))
            .cloned())
    }

    /// Returns the durable heartbeat pacing state for one channel when it exists.
    pub(crate) async fn get_heartbeat_state(
        &self,
        channel_id: &str,
    ) -> Result<Option<ChannelHeartbeatStateView>> {
        let state = self.state.lock().await;
        anyhow::ensure!(
            state.channels.contains_key(channel_id),
            "unknown channel {channel_id}"
        );
        Ok(state.heartbeat_states.get(channel_id).cloned())
    }

    /// Creates or updates the durable heartbeat pacing state for one channel.
    pub(crate) async fn set_heartbeat_state(
        &self,
        channel_id: &str,
        mut heartbeat_state: ChannelHeartbeatStateView,
    ) -> Result<ChannelHeartbeatStateView> {
        {
            let state = self.state.lock().await;
            anyhow::ensure!(
                state.channels.contains_key(channel_id),
                "unknown channel {channel_id}"
            );
        }
        heartbeat_state.channel_id = channel_id.to_string();
        self.store
            .save_heartbeat_state(channel_id, &heartbeat_state)?;
        let mut state = self.state.lock().await;
        if !state.channels.contains_key(channel_id) {
            self.store.delete_heartbeat_state(channel_id)?;
            bail!("unknown channel {channel_id}");
        }
        state
            .heartbeat_states
            .insert(channel_id.to_string(), heartbeat_state.clone());
        Ok(heartbeat_state)
    }

    /// Deletes the heartbeat pacing state for one channel when it exists.
    pub(crate) async fn delete_heartbeat_state(&self, channel_id: &str) -> Result<()> {
        {
            let state = self.state.lock().await;
            anyhow::ensure!(
                state.channels.contains_key(channel_id),
                "unknown channel {channel_id}"
            );
        }
        self.store.delete_heartbeat_state(channel_id)?;
        let mut state = self.state.lock().await;
        state.heartbeat_states.remove(channel_id);
        Ok(())
    }

    /// Creates or updates the canonical work state for one root thread.
    pub(crate) async fn upsert_thread_state(
        &self,
        channel_id: &str,
        thread_root_message_id: &str,
        build: impl FnOnce(Option<ChannelThreadWorkStateView>) -> Result<ChannelThreadWorkStateView>,
    ) -> Result<ChannelThreadWorkStateView> {
        let mut state = self.state.lock().await;
        anyhow::ensure!(
            state.channels.contains_key(channel_id),
            "unknown channel {channel_id}"
        );
        anyhow::ensure!(
            state
                .messages
                .get(channel_id)
                .and_then(|messages| messages.get(thread_root_message_id))
                .is_some_and(|message| message.thread_root_message_id.is_none()),
            "thread_root_message_id {thread_root_message_id} must point at an existing thread root"
        );
        let current = state
            .thread_states
            .get(channel_id)
            .and_then(|states| states.get(thread_root_message_id))
            .cloned();
        let next = build(current)?;
        anyhow::ensure!(
            next.channel_id == channel_id,
            "channel thread state ownership mismatch"
        );
        anyhow::ensure!(
            next.thread_root_message_id == thread_root_message_id,
            "thread_root_message_id ownership mismatch"
        );
        state
            .thread_states
            .entry(channel_id.to_string())
            .or_default()
            .insert(thread_root_message_id.to_string(), next.clone());
        self.persist_thread_states_locked(&state, channel_id)?;
        Ok(next)
    }

    /// Replaces the persisted thread-work projection for one channel atomically.
    pub(crate) async fn replace_thread_states(
        &self,
        channel_id: &str,
        states: Vec<ChannelThreadWorkStateView>,
    ) -> Result<Vec<ChannelThreadWorkStateView>> {
        let mut state = self.state.lock().await;
        anyhow::ensure!(
            state.channels.contains_key(channel_id),
            "unknown channel {channel_id}"
        );
        anyhow::ensure!(
            states
                .iter()
                .all(|thread_state| thread_state.channel_id == channel_id),
            "channel thread state ownership mismatch"
        );
        anyhow::ensure!(
            states.iter().all(|thread_state| {
                state
                    .messages
                    .get(channel_id)
                    .and_then(|messages| messages.get(&thread_state.thread_root_message_id))
                    .is_some_and(|message| message.thread_root_message_id.is_none())
            }),
            "thread_root_message_id must point at an existing thread root"
        );
        let mut ordered = states;
        ordered.sort_by(|left, right| {
            left.thread_root_message_id
                .cmp(&right.thread_root_message_id)
        });
        state.thread_states.insert(
            channel_id.to_string(),
            ordered
                .iter()
                .cloned()
                .map(|thread_state| (thread_state.thread_root_message_id.clone(), thread_state))
                .collect(),
        );
        self.persist_thread_states_locked(&state, channel_id)?;
        Ok(ordered)
    }

    /// Adds or updates one durable work binding on the canonical root thread.
    pub(crate) async fn bind_thread_work(
        &self,
        channel_id: &str,
        thread_root_message_id: &str,
        binding: ChannelWorkBindingView,
    ) -> Result<ChannelThreadWorkStateView> {
        self.upsert_thread_state(channel_id, thread_root_message_id, |existing| {
            let mut state = existing.unwrap_or(ChannelThreadWorkStateView {
                channel_id: channel_id.to_string(),
                thread_root_message_id: thread_root_message_id.to_string(),
                topic_kind: Default::default(),
                status: Default::default(),
                owner_session_id: None,
                initiative_key: None,
                source_kind: None,
                source_ref: None,
                last_stimulus_id: None,
                last_signal_at_ms: binding.bound_at_ms,
                last_main_promotion_at_ms: None,
                bindings: Vec::new(),
                progress_snapshots: Vec::new(),
                metadata: Value::Null,
            });
            if let Some(existing_binding) = state.bindings.iter_mut().find(|existing_binding| {
                existing_binding.binding_kind == binding.binding_kind
                    && existing_binding.binding_ref == binding.binding_ref
            }) {
                *existing_binding = binding.clone();
            } else {
                state.bindings.push(binding.clone());
                state.bindings.sort_by(|left, right| {
                    format!("{:?}:{}", left.binding_kind, left.binding_ref)
                        .cmp(&format!("{:?}:{}", right.binding_kind, right.binding_ref))
                });
            }
            state.last_signal_at_ms = state.last_signal_at_ms.max(binding.bound_at_ms);
            Ok(state)
        })
        .await
    }

    /// Records the latest supersedable progress snapshot for one root thread.
    pub(crate) async fn upsert_progress_snapshot(
        &self,
        channel_id: &str,
        thread_root_message_id: &str,
        snapshot: ChannelProgressSnapshotView,
    ) -> Result<ChannelThreadWorkStateView> {
        self.upsert_thread_state(channel_id, thread_root_message_id, |existing| {
            let mut state = existing.unwrap_or(ChannelThreadWorkStateView {
                channel_id: channel_id.to_string(),
                thread_root_message_id: thread_root_message_id.to_string(),
                topic_kind: Default::default(),
                status: Default::default(),
                owner_session_id: None,
                initiative_key: None,
                source_kind: None,
                source_ref: None,
                last_stimulus_id: None,
                last_signal_at_ms: snapshot.updated_at_ms,
                last_main_promotion_at_ms: None,
                bindings: Vec::new(),
                progress_snapshots: Vec::new(),
                metadata: Value::Null,
            });
            if let Some(existing_snapshot) = state
                .progress_snapshots
                .iter_mut()
                .find(|existing_snapshot| existing_snapshot.progress_key == snapshot.progress_key)
            {
                *existing_snapshot = snapshot.clone();
            } else {
                state.progress_snapshots.push(snapshot.clone());
                state
                    .progress_snapshots
                    .sort_by(|left, right| left.progress_key.cmp(&right.progress_key));
            }
            state.last_signal_at_ms = state.last_signal_at_ms.max(snapshot.updated_at_ms);
            Ok(state)
        })
        .await
    }

    /// Resolves the canonical root thread that currently owns one bound work item.
    pub(crate) async fn thread_for_binding(
        &self,
        binding_kind: ChannelWorkBindingKind,
        binding_ref: &str,
    ) -> Option<(String, String)> {
        let state = self.state.lock().await;
        state.thread_states.iter().find_map(|(channel_id, states)| {
            states.values().find_map(|thread_state| {
                thread_state
                    .bindings
                    .iter()
                    .find(|binding| {
                        binding.binding_kind == binding_kind && binding.binding_ref == binding_ref
                    })
                    .map(|_| {
                        (
                            channel_id.clone(),
                            thread_state.thread_root_message_id.clone(),
                        )
                    })
            })
        })
    }

    /// Persists one new channel.
    pub(crate) async fn create_channel(&self, request: CreateChannelRecord) -> Result<ChannelView> {
        let mut state = self.state.lock().await;
        if state.channels.contains_key(&request.channel_id) {
            bail!("channel {} already exists", request.channel_id);
        }
        let channel = ChannelView {
            summary: ChannelSummaryView {
                channel_id: request.channel_id.clone(),
                title: request.title,
                description: request.description,
                purpose: request.purpose,
                member_count: request.members.len() as u64,
                message_count: 0,
                last_message_id: None,
                last_message_preview: None,
                created_at_ms: request.created_at_ms,
                updated_at_ms: request.created_at_ms,
                paused: false,
            },
            members: request.members,
            pinned_asset_ids: request.pinned_asset_ids,
            created_by: request.created_by,
            autonomy_policy: request.autonomy_policy,
            default_participation_mode: request.default_participation_mode,
            moderation_metrics: Default::default(),
            heartbeat_state: None,
            metadata: request.metadata,
        };
        self.store.save_channel(&channel)?;
        let mut index = self.index.lock().await;
        index.channels.insert(
            channel.summary.channel_id.clone(),
            ChannelIndexEntry::from(&channel),
        );
        index.next_channel_id = self.next_channel_id.load(Ordering::Relaxed);
        self.store.save_index(&index)?;
        state
            .messages
            .insert(channel.summary.channel_id.clone(), BTreeMap::new());
        state
            .ordered_message_ids
            .insert(channel.summary.channel_id.clone(), Vec::new());
        state
            .stimuli
            .insert(channel.summary.channel_id.clone(), BTreeMap::new());
        state
            .thread_states
            .insert(channel.summary.channel_id.clone(), BTreeMap::new());
        state.heartbeat_states.remove(&channel.summary.channel_id);
        state
            .channels
            .insert(channel.summary.channel_id.clone(), channel.clone());
        Ok(channel_with_metrics(&state, &channel, now_ms()))
    }

    /// Updates one persisted channel in place.
    pub(crate) async fn update_channel(
        &self,
        channel_id: &str,
        update: impl FnOnce(&mut ChannelView) -> Result<bool>,
    ) -> Result<ChannelView> {
        let mut state = self.state.lock().await;
        let mut changed = false;
        let channel_snapshot = {
            let channel = state
                .channels
                .get_mut(channel_id)
                .ok_or_else(|| anyhow!("unknown channel {channel_id}"))?;
            let previous = channel.clone();
            if !update(channel)? {
                previous
            } else {
                if let Err(error) = self.store.save_channel(channel) {
                    *channel = previous;
                    return Err(error);
                }
                changed = true;
                channel.clone()
            }
        };
        if !changed {
            return Ok(channel_with_metrics(&state, &channel_snapshot, now_ms()));
        }
        let mut index = self.index.lock().await;
        index.channels.insert(
            channel_id.to_string(),
            ChannelIndexEntry::from(&channel_snapshot),
        );
        self.store.save_index(&index)?;
        Ok(channel_with_metrics(&state, &channel_snapshot, now_ms()))
    }

    /// Deletes one persisted channel and its sidecar state.
    pub(crate) async fn delete_channel(&self, channel_id: &str) -> Result<()> {
        let mut state = self.state.lock().await;
        if !state.channels.contains_key(channel_id) {
            bail!("unknown channel {channel_id}");
        }
        self.store.delete_channel(channel_id)?;
        state.channels.remove(channel_id);
        state.messages.remove(channel_id);
        state.ordered_message_ids.remove(channel_id);
        state.leases.remove(channel_id);
        state.stimuli.remove(channel_id);
        state.thread_states.remove(channel_id);
        state.heartbeat_states.remove(channel_id);
        let mut index = self.index.lock().await;
        index.channels.remove(channel_id);
        self.store.save_index(&index)
    }

    /// Adds or replaces one channel member.
    pub(crate) async fn upsert_member(
        &self,
        channel_id: &str,
        member: ChannelMemberView,
    ) -> Result<ChannelView> {
        self.update_channel(channel_id, |channel| {
            let previous_len = channel.members.len();
            let mut changed = false;
            if let Some(existing) = channel
                .members
                .iter_mut()
                .find(|existing| existing.member_id == member.member_id)
            {
                if *existing != member {
                    *existing = member.clone();
                    changed = true;
                }
            } else {
                channel.members.push(member.clone());
                changed = true;
            }
            channel
                .members
                .sort_by(|left, right| left.member_id.cmp(&right.member_id));
            channel.summary.member_count = channel.members.len() as u64;
            if changed {
                channel.summary.updated_at_ms = now_ms();
            }
            Ok(changed || previous_len != channel.members.len())
        })
        .await
    }

    /// Removes one channel member.
    pub(crate) async fn remove_member(
        &self,
        channel_id: &str,
        member_id: &str,
    ) -> Result<ChannelView> {
        let mut state = self.state.lock().await;
        let (removed_member, updated_at_ms, channel_snapshot) = {
            let channel = state
                .channels
                .get_mut(channel_id)
                .ok_or_else(|| anyhow!("unknown channel {channel_id}"))?;
            let previous_len = channel.members.len();
            let removed_member = channel
                .members
                .iter()
                .find(|member| member.member_id == member_id)
                .cloned()
                .ok_or_else(|| anyhow!("unknown member {member_id}"))?;
            channel
                .members
                .retain(|member| member.member_id != member_id);
            anyhow::ensure!(
                previous_len != channel.members.len(),
                "unknown member {member_id}"
            );
            channel.summary.member_count = channel.members.len() as u64;
            channel.summary.updated_at_ms = now_ms();
            self.store.save_channel(channel)?;
            (
                removed_member,
                channel.summary.updated_at_ms,
                channel.clone(),
            )
        };

        let removed_session_id = removed_member.session_id.as_deref();
        if let Some(leases) = state.leases.get_mut(channel_id) {
            leases.retain(|_, lease| {
                let holder_removed = removed_session_id == Some(lease.holder_session_id.as_str());
                if holder_removed {
                    return false;
                }
                if let Some(removed_session_id) = removed_session_id {
                    lease
                        .queued_candidate_session_ids
                        .retain(|session_id| session_id != removed_session_id);
                    lease
                        .last_human_priority_session_ids
                        .retain(|session_id| session_id != removed_session_id);
                }
                true
            });
            if leases.is_empty() {
                self.store.delete_leases(channel_id)?;
                state.leases.remove(channel_id);
            } else {
                let persisted = leases.values().cloned().collect::<Vec<_>>();
                self.store.save_leases(channel_id, &persisted)?;
            }
        }

        self.store.append_event(&ChannelEventEntry {
            channel_id: channel_id.to_string(),
            timestamp_ms: updated_at_ms,
            event: ChannelEvent::MemberLeft {
                member_id: member_id.to_string(),
            },
        })?;
        let mut index = self.index.lock().await;
        index.channels.insert(
            channel_id.to_string(),
            ChannelIndexEntry::from(&channel_snapshot),
        );
        self.store.save_index(&index)?;
        self.notify.notify_waiters();
        Ok(channel_snapshot)
    }

    /// Returns whether one actor identifier currently belongs to the channel.
    pub(crate) async fn channel_has_actor(&self, channel_id: &str, actor_id: &str) -> bool {
        self.state
            .lock()
            .await
            .channels
            .get(channel_id)
            .is_some_and(|channel| {
                channel.members.iter().any(|member| {
                    member.actor_id.as_deref() == Some(actor_id) || member.member_id == actor_id
                })
            })
    }

    /// Returns whether one session currently belongs to the channel.
    pub(crate) async fn channel_has_session(&self, channel_id: &str, session_id: &str) -> bool {
        self.state
            .lock()
            .await
            .channels
            .get(channel_id)
            .is_some_and(|channel| {
                channel
                    .members
                    .iter()
                    .any(|member| member.session_id.as_deref() == Some(session_id))
            })
    }

    /// Persists one public channel message and updates the in-memory projection.
    pub(crate) async fn post_message(
        &self,
        request: CreateChannelMessageRecord,
    ) -> Result<ChannelMessageView> {
        let mut state = self.state.lock().await;
        anyhow::ensure!(
            state.channels.contains_key(&request.channel_id),
            "unknown channel {}",
            request.channel_id
        );
        if state
            .messages
            .get(&request.channel_id)
            .is_some_and(|messages| messages.contains_key(&request.message_id))
        {
            bail!("channel message {} already exists", request.message_id);
        }
        let thread_root_message_id = match request.reply_to_message_id.as_deref() {
            Some(parent_message_id) => {
                let parent = state
                    .messages
                    .get(&request.channel_id)
                    .and_then(|messages| messages.get(parent_message_id))
                    .ok_or_else(|| anyhow!("unknown parent channel message {parent_message_id}"))?;
                let derived = parent
                    .thread_root_message_id
                    .clone()
                    .unwrap_or_else(|| parent.message_id.clone());
                if let Some(requested) = request.requested_thread_root_message_id.as_deref() {
                    anyhow::ensure!(
                        requested == derived,
                        "reply thread_root_message_id {} does not match parent thread root {}",
                        requested,
                        derived
                    );
                }
                Some(derived)
            }
            None => request.requested_thread_root_message_id,
        };
        let message = ChannelMessageView {
            message_id: request.message_id.clone(),
            channel_id: request.channel_id.clone(),
            thread_root_message_id,
            reply_to_message_id: request.reply_to_message_id,
            sender: request.sender,
            sender_session_id: request.sender_session_id,
            addressed_member_ids: request.addressed_member_ids,
            output: request.output.normalized(),
            created_at_ms: request.created_at_ms,
            reactions: Vec::new(),
            metadata: request.metadata,
        };
        self.store.append_event(&ChannelEventEntry {
            channel_id: request.channel_id.clone(),
            timestamp_ms: message.created_at_ms,
            event: ChannelEvent::MessagePosted {
                message: message.clone(),
            },
        })?;
        let messages = state
            .messages
            .entry(request.channel_id.clone())
            .or_default();
        messages.insert(message.message_id.clone(), message.clone());
        state
            .ordered_message_ids
            .entry(request.channel_id.clone())
            .or_default()
            .push(message.message_id.clone());
        let channel = state
            .channels
            .get_mut(&request.channel_id)
            .ok_or_else(|| anyhow!("unknown channel {}", request.channel_id))?;
        channel.summary.message_count = channel.summary.message_count.saturating_add(1);
        channel.summary.last_message_id = Some(message.message_id.clone());
        channel.summary.last_message_preview = Some(message.output.content.clone());
        channel.summary.updated_at_ms = message.created_at_ms.max(channel.summary.updated_at_ms);
        self.store.save_channel(channel)?;
        let mut index = self.index.lock().await;
        index
            .channels
            .insert(request.channel_id, ChannelIndexEntry::from(&*channel));
        index.next_message_id = self.next_message_id.load(Ordering::Relaxed);
        self.store.save_index(&index)?;
        self.notify.notify_waiters();
        Ok(message)
    }

    /// Adds one reaction when it is not already present.
    pub(crate) async fn set_reaction(
        &self,
        request: ChannelReactionMutation,
    ) -> Result<ChannelMessageView> {
        self.mutate_reaction(request, true).await
    }

    /// Removes one reaction when it exists.
    pub(crate) async fn unset_reaction(
        &self,
        request: ChannelReactionMutation,
    ) -> Result<ChannelMessageView> {
        self.mutate_reaction(request, false).await
    }

    async fn mutate_reaction(
        &self,
        request: ChannelReactionMutation,
        present: bool,
    ) -> Result<ChannelMessageView> {
        let mut state = self.state.lock().await;
        let _channel = state
            .channels
            .get(&request.channel_id)
            .ok_or_else(|| anyhow!("unknown channel {}", request.channel_id))?;
        let message = state
            .messages
            .get_mut(&request.channel_id)
            .and_then(|messages| messages.get_mut(&request.message_id))
            .ok_or_else(|| anyhow!("unknown channel message {}", request.message_id))?;
        let channel_id = message.channel_id.clone();
        let mut reactions = message
            .reactions
            .iter()
            .map(|reaction| {
                (
                    reaction.emoji.clone(),
                    reaction.actor_ids.iter().cloned().collect::<BTreeSet<_>>(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let actors = reactions.entry(request.emoji.clone()).or_default();
        let changed = if present {
            actors.insert(request.actor_id.clone())
        } else {
            actors.remove(&request.actor_id)
        };
        if !changed {
            return Ok(message.clone());
        }
        if actors.is_empty() {
            reactions.remove(&request.emoji);
        }
        message.reactions = reactions
            .into_iter()
            .map(|(emoji, actor_ids)| ChannelReactionView {
                count: actor_ids.len() as u64,
                actor_ids: actor_ids.into_iter().collect(),
                emoji,
            })
            .collect();
        self.store.append_event(&ChannelEventEntry {
            channel_id: request.channel_id,
            timestamp_ms: request.timestamp_ms,
            event: if present {
                ChannelEvent::ReactionSet {
                    message_id: request.message_id,
                    actor_id: request.actor_id,
                    emoji: request.emoji,
                }
            } else {
                ChannelEvent::ReactionUnset {
                    message_id: request.message_id,
                    actor_id: request.actor_id,
                    emoji: request.emoji,
                }
            },
        })?;
        let message = message.clone();
        if let Some(channel) = state.channels.get_mut(&channel_id) {
            channel.summary.updated_at_ms = request.timestamp_ms.max(channel.summary.updated_at_ms);
            self.store.save_channel(channel)?;
            let mut index = self.index.lock().await;
            index
                .channels
                .insert(channel_id, ChannelIndexEntry::from(&*channel));
            self.store.save_index(&index)?;
        }
        self.notify.notify_waiters();
        Ok(message)
    }

    /// Persists the current turn leases for one channel.
    pub(crate) async fn replace_leases(
        &self,
        channel_id: &str,
        leases: Vec<ChannelTurnLeaseView>,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        let channel = state
            .channels
            .get(channel_id)
            .ok_or_else(|| anyhow!("unknown channel {channel_id}"))?;
        if leases.is_empty() {
            self.store.delete_leases(channel_id)?;
            state.leases.remove(channel_id);
            self.notify.notify_waiters();
            return Ok(());
        }
        anyhow::ensure!(
            leases
                .iter()
                .all(|lease| lease.channel_id == channel.summary.channel_id),
            "channel lease ownership mismatch"
        );
        self.store.save_leases(channel_id, &leases)?;
        state.leases.insert(
            channel_id.to_string(),
            leases
                .into_iter()
                .map(|lease| (lease.turn_id.clone(), lease))
                .collect(),
        );
        let mut index = self.index.lock().await;
        index.next_turn_id = self.next_turn_id.load(Ordering::Relaxed);
        self.store.save_index(&index)?;
        self.notify.notify_waiters();
        Ok(())
    }

    fn persist_stimuli_locked(&self, state: &ChannelState, channel_id: &str) -> Result<()> {
        let stimuli = state
            .stimuli
            .get(channel_id)
            .map(|stimuli| {
                let mut ordered = stimuli.values().cloned().collect::<Vec<_>>();
                ordered.sort_by(|left, right| {
                    left.created_at_ms
                        .cmp(&right.created_at_ms)
                        .then_with(|| left.stimulus_id.cmp(&right.stimulus_id))
                });
                ordered
            })
            .unwrap_or_default();
        if stimuli.is_empty() {
            self.store.delete_stimuli(channel_id)
        } else {
            self.store.save_stimuli(channel_id, &stimuli)
        }
    }

    fn persist_thread_states_locked(&self, state: &ChannelState, channel_id: &str) -> Result<()> {
        let states = state
            .thread_states
            .get(channel_id)
            .map(|states| {
                let mut ordered = states.values().cloned().collect::<Vec<_>>();
                ordered.sort_by(|left, right| {
                    left.thread_root_message_id
                        .cmp(&right.thread_root_message_id)
                });
                ordered
            })
            .unwrap_or_default();
        if states.is_empty() {
            self.store.delete_thread_states(channel_id)
        } else {
            self.store.save_thread_states(channel_id, &states)
        }
    }
}

fn channel_with_metrics(state: &ChannelState, channel: &ChannelView, now_ms: u64) -> ChannelView {
    let mut channel = channel.clone();
    channel.moderation_metrics =
        channel_moderation_metrics(state, &channel.summary.channel_id, now_ms);
    channel.heartbeat_state = state
        .heartbeat_states
        .get(&channel.summary.channel_id)
        .cloned();
    channel
}

fn channel_moderation_metrics(
    state: &ChannelState,
    channel_id: &str,
    now_ms: u64,
) -> crate::channels::ChannelModerationMetricsView {
    let leases = state.leases.get(channel_id);
    let stimuli = state.stimuli.get(channel_id);
    let thread_states = state.thread_states.get(channel_id);
    let messages = state.messages.get(channel_id);
    crate::channels::ChannelModerationMetricsView {
        active_turn_lease_count: leases.map_or(0, |leases| leases.len() as u64),
        active_public_speaker_count: leases
            .map(|leases| {
                leases
                    .values()
                    .filter(|lease| lease.active_run_id.is_some())
                    .count() as u64
            })
            .unwrap_or(0),
        pending_stimulus_count: stimuli
            .map(|stimuli| {
                stimuli
                    .values()
                    .filter(|stimulus| stimulus.state == ChannelStimulusState::Pending)
                    .count() as u64
            })
            .unwrap_or(0),
        claimed_stimulus_count: stimuli
            .map(|stimuli| {
                stimuli
                    .values()
                    .filter(|stimulus| stimulus.state == ChannelStimulusState::Claimed)
                    .count() as u64
            })
            .unwrap_or(0),
        active_thread_work_count: thread_states
            .map(|states| {
                states
                    .values()
                    .filter(|state| {
                        !matches!(
                            state.status,
                            crate::channels::ChannelThreadWorkStatus::Completed
                                | crate::channels::ChannelThreadWorkStatus::Superseded
                        )
                    })
                    .count() as u64
            })
            .unwrap_or(0),
        recent_autonomous_root_posts_1h: messages
            .map(|messages| {
                messages
                    .values()
                    .filter(|message| {
                        message.thread_root_message_id.is_none()
                            && message.created_at_ms >= now_ms.saturating_sub(3_600_000)
                            && crate::channels::channel_message_is_autonomous_root(message)
                    })
                    .count() as u64
            })
            .unwrap_or(0),
        suppressed_stimulus_count: stimuli
            .map(|stimuli| {
                stimuli
                    .values()
                    .filter(|stimulus| {
                        matches!(
                            stimulus.state,
                            ChannelStimulusState::Cancelled
                                | ChannelStimulusState::Coalesced
                                | ChannelStimulusState::Superseded
                        )
                    })
                    .count() as u64
            })
            .unwrap_or(0),
    }
}

pub(crate) fn stimuli_equivalent(left: &ChannelStimulusView, right: &ChannelStimulusView) -> bool {
    if let (Some(left_progress_key), Some(right_progress_key)) =
        (left.progress_key.as_deref(), right.progress_key.as_deref())
    {
        return left.channel_id == right.channel_id
            && left.scope == right.scope
            && left.thread_root_message_id == right.thread_root_message_id
            && left_progress_key == right_progress_key;
    }
    if let (Some(left_dedupe_key), Some(right_dedupe_key)) =
        (left.dedupe_key.as_deref(), right.dedupe_key.as_deref())
    {
        return left.channel_id == right.channel_id
            && left.scope == right.scope
            && left.thread_root_message_id == right.thread_root_message_id
            && left_dedupe_key == right_dedupe_key;
    }
    left.channel_id == right.channel_id
        && left.scope == right.scope
        && left.thread_root_message_id == right.thread_root_message_id
        && left.kind == right.kind
        && left.source_kind == right.source_kind
        && left.source_ref == right.source_ref
        && left.content == right.content
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicU64;

    use anyhow::Result;
    use kheish_types::{ActorRef, RichOutput};
    use serde_json::{Value, json};
    use tempfile::tempdir;

    use super::{ChannelService, CreateChannelMessageRecord, CreateChannelRecord};
    use crate::channels::{
        ChannelAutonomyPolicy, ChannelHeartbeatStateView, ChannelIndex, ChannelParticipationMode,
        FileChannelStore,
    };
    use crate::now_ms;

    fn test_service(root: &std::path::Path) -> ChannelService {
        ChannelService::new(
            FileChannelStore::new(root),
            ChannelIndex::default(),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            AtomicU64::new(1),
            AtomicU64::new(1),
            AtomicU64::new(1),
            AtomicU64::new(1),
        )
    }

    async fn create_empty_channel(service: &ChannelService, channel_id: &str) -> Result<()> {
        service
            .create_channel(CreateChannelRecord {
                channel_id: channel_id.to_string(),
                title: "Room".to_string(),
                description: None,
                purpose: None,
                pinned_asset_ids: Vec::new(),
                created_by: "test".to_string(),
                created_at_ms: 1,
                members: Vec::new(),
                autonomy_policy: ChannelAutonomyPolicy::default(),
                default_participation_mode: ChannelParticipationMode::SelectedOnly,
                metadata: Value::Null,
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn heartbeat_state_persists_internally_without_public_serialization() -> Result<()> {
        let temp = tempdir()?;
        let service = test_service(temp.path());
        create_empty_channel(&service, "channel-heartbeat").await?;

        let heartbeat = ChannelHeartbeatStateView {
            channel_id: "channel-heartbeat".to_string(),
            pending_grant: true,
            last_seen_message_id: Some("channel-message-1".to_string()),
            consecutive_silent: 3,
            last_turn_at_ms: 42,
            turn_counter: 7,
            last_granted: BTreeMap::from([("session-1".to_string(), 40)]),
            dormant: true,
            updated_at_ms: 99,
        };
        service
            .set_heartbeat_state("channel-heartbeat", heartbeat.clone())
            .await?;

        let view = service.get_channel("channel-heartbeat").await?;
        assert_eq!(view.heartbeat_state, Some(heartbeat.clone()));
        let public_json = serde_json::to_value(&view)?;
        assert!(public_json.get("heartbeat_state").is_none());

        let persisted = FileChannelStore::new(temp.path()).load_heartbeat_states()?;
        assert_eq!(persisted.get("channel-heartbeat"), Some(&heartbeat));

        service.delete_heartbeat_state("channel-heartbeat").await?;
        assert!(
            service
                .get_channel("channel-heartbeat")
                .await?
                .heartbeat_state
                .is_none()
        );
        assert!(
            FileChannelStore::new(temp.path())
                .load_heartbeat_states()?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn moderation_metrics_count_autonomous_channel_delivery_roots() -> Result<()> {
        let temp = tempdir()?;
        let service = test_service(temp.path());
        create_empty_channel(&service, "channel-metrics").await?;
        let created_at_ms = now_ms();

        let sender = ActorRef {
            id: "session-1".to_string(),
            display_name: Some("Agent".to_string()),
        };
        for (message_id, metadata) in [
            (
                "channel-message-1",
                json!({"source_kind": "channel_delivery", "autonomous_new_topic": true}),
            ),
            (
                "channel-message-2",
                json!({"source_kind": "channel_delivery", "autonomous_new_topic": false}),
            ),
            (
                "channel-message-3",
                json!({"source_kind": "channel_stimulus"}),
            ),
        ] {
            service
                .post_message(CreateChannelMessageRecord {
                    channel_id: "channel-metrics".to_string(),
                    message_id: message_id.to_string(),
                    sender: sender.clone(),
                    sender_session_id: Some("session-1".to_string()),
                    addressed_member_ids: Vec::new(),
                    reply_to_message_id: None,
                    requested_thread_root_message_id: None,
                    output: RichOutput {
                        content: message_id.to_string(),
                        parts: Vec::new(),
                        artifacts: Vec::new(),
                    },
                    created_at_ms,
                    metadata,
                })
                .await?;
        }

        let view = service.get_channel("channel-metrics").await?;
        assert_eq!(view.moderation_metrics.recent_autonomous_root_posts_1h, 2);
        Ok(())
    }
}
