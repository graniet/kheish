use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Result, anyhow};
use kheish_runtime::{RuntimeObserver, TraceEvent, TraceEventKind};
use kheish_types::ConversationKey;
use tracing::{debug, info, warn};

use crate::types::{
    AgentId, AgentRecord, AgentStatus, AgentSupervisorAuditEntry, AgentSupervisorSnapshot,
    AgentSupervisorStatusSnapshot, ChildRetentionPolicy, ForkContext, MailboxMessage,
    ManagedAgentSnapshot, SubtaskSpec,
};

const AGENT_SUPERVISOR_AUDIT_LIMIT: usize = 2_048;
const AGENT_NICKNAMES: &[&str] = &[
    "Atlas", "Aurora", "Cinder", "Ion", "Juniper", "Lyra", "Nova", "Orion", "Sage", "Vega",
];

/// Optional durable sink for supervisor lifecycle audit entries.
pub trait AgentSupervisorAuditSink: Send + Sync {
    fn append_supervisor_audit(&self, entry: &AgentSupervisorAuditEntry) -> Result<()>;
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn slugify_agent_name(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_separator = false;
    for character in value.chars() {
        let normalized = match character {
            'A'..='Z' => Some(character.to_ascii_lowercase()),
            'a'..='z' | '0'..='9' => Some(character),
            _ => None,
        };
        match normalized {
            Some(character) => {
                slug.push(character);
                last_was_separator = false;
            }
            None if !slug.is_empty() && !last_was_separator => {
                slug.push('_');
                last_was_separator = true;
            }
            None => {}
        }
    }
    let slug = slug.trim_matches('_').to_string();
    if slug.is_empty() {
        "agent".to_string()
    } else {
        slug
    }
}

/// A lightweight in-memory multi-agent supervisor.
pub struct AgentSupervisor {
    next_id: Mutex<u64>,
    next_audit_id: Mutex<u64>,
    agents: Mutex<BTreeMap<AgentId, AgentRecord>>,
    terminal_snapshots: Mutex<BTreeMap<AgentId, ManagedAgentSnapshot>>,
    mailboxes: Mutex<BTreeMap<AgentId, Vec<MailboxMessage>>>,
    mailbox_dead_letters: Mutex<BTreeMap<AgentId, Vec<MailboxMessage>>>,
    audit_log: Mutex<Vec<AgentSupervisorAuditEntry>>,
    audit_sink: Mutex<Option<Arc<dyn AgentSupervisorAuditSink>>>,
    audit_sink_error_count: AtomicU64,
    audit_sink_last_error: Mutex<Option<String>>,
    observer: Arc<dyn RuntimeObserver>,
}

impl AgentSupervisor {
    /// Creates a new supervisor.
    pub fn new(observer: Arc<dyn RuntimeObserver>) -> Self {
        Self {
            next_id: Mutex::new(0),
            next_audit_id: Mutex::new(0),
            agents: Mutex::new(BTreeMap::new()),
            terminal_snapshots: Mutex::new(BTreeMap::new()),
            mailboxes: Mutex::new(BTreeMap::new()),
            mailbox_dead_letters: Mutex::new(BTreeMap::new()),
            audit_log: Mutex::new(Vec::new()),
            audit_sink: Mutex::new(None),
            audit_sink_error_count: AtomicU64::new(0),
            audit_sink_last_error: Mutex::new(None),
            observer,
        }
    }

    /// Installs an append-only audit sink used for daemon-backed durable audit recovery.
    pub fn set_audit_sink(&self, sink: Arc<dyn AgentSupervisorAuditSink>) {
        *self.audit_sink.lock().expect("audit sink mutex poisoned") = Some(sink);
    }

    /// Restores a supervisor from a serializable snapshot.
    pub fn restore(snapshot: AgentSupervisorSnapshot, observer: Arc<dyn RuntimeObserver>) -> Self {
        Self::try_restore(snapshot, observer).expect("repaired supervisor snapshot should be valid")
    }

    /// Restores a supervisor from a snapshot and reports invalid topology instead of panicking.
    pub fn try_restore(
        snapshot: AgentSupervisorSnapshot,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self> {
        let snapshot = repair_snapshot(snapshot);
        validate_snapshot(&snapshot)?;
        Ok(Self {
            next_id: Mutex::new(snapshot.next_id),
            next_audit_id: Mutex::new(snapshot.next_audit_id),
            agents: Mutex::new(snapshot.agents),
            terminal_snapshots: Mutex::new(snapshot.terminal_snapshots),
            mailboxes: Mutex::new(snapshot.mailboxes),
            mailbox_dead_letters: Mutex::new(snapshot.mailbox_dead_letters),
            audit_log: Mutex::new(trim_audit_log(snapshot.audit_log)),
            audit_sink: Mutex::new(None),
            audit_sink_error_count: AtomicU64::new(0),
            audit_sink_last_error: Mutex::new(None),
            observer,
        })
    }

    /// Validates one serializable supervisor snapshot without mutating it.
    pub fn validate_snapshot(snapshot: &AgentSupervisorSnapshot) -> Result<()> {
        validate_snapshot(snapshot)
    }

    /// Validates the current topology invariants.
    pub fn validate_topology(&self) -> Result<()> {
        let agents = self.agents.lock().expect("agents mutex poisoned");
        validate_agents(&agents)?;
        let terminal_snapshots = self
            .terminal_snapshots
            .lock()
            .expect("terminal snapshot mutex poisoned");
        validate_terminal_snapshots(&agents, &terminal_snapshots)?;
        let mailboxes = self.mailboxes.lock().expect("mailboxes mutex poisoned");
        for agent_id in mailboxes.keys() {
            anyhow::ensure!(
                agents.contains_key(agent_id),
                "mailbox references unknown agent {}",
                agent_id.0
            );
        }
        let mailbox_dead_letters = self
            .mailbox_dead_letters
            .lock()
            .expect("mailbox dead letter mutex poisoned");
        for agent_id in mailbox_dead_letters.keys() {
            anyhow::ensure!(
                agents.contains_key(agent_id),
                "mailbox dead letter queue references unknown agent {}",
                agent_id.0
            );
        }
        let next_id = *self.next_id.lock().expect("next_id mutex poisoned");
        validate_next_id(next_id, &agents)?;
        Ok(())
    }

    /// Spawns a new agent record and returns it.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        &self,
        parent: Option<AgentId>,
        conversation: ConversationKey,
        requested_name: Option<&str>,
        requested_nickname: Option<String>,
        retention: ChildRetentionPolicy,
        spawned_by_run_id: Option<String>,
        spawn_request_id: Option<String>,
    ) -> Result<AgentRecord> {
        let mut next_id = self.next_id.lock().expect("next_id mutex poisoned");
        *next_id += 1;
        let numeric_id = *next_id;
        let id = AgentId(format!("agent-{}", numeric_id));
        let mut agents = self.agents.lock().expect("agents mutex poisoned");
        if let Some(existing) = agents
            .values()
            .find(|record| record.conversation.session_id == conversation.session_id)
        {
            return Err(anyhow!(
                "session {} is already owned by agent {}",
                conversation.session_id,
                existing.id.0
            ));
        }
        let parent_path = match parent.as_ref() {
            Some(parent_id) => Some(
                agents
                    .get(parent_id)
                    .ok_or_else(|| anyhow!("unknown agent {}", parent_id.0))?
                    .path
                    .clone()
                    .unwrap_or_else(|| parent_id.0.clone()),
            ),
            None => None,
        };
        let requested_name = requested_name
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&conversation.session_id);
        let base_name = slugify_agent_name(requested_name);
        let sibling_names = agents
            .values()
            .filter(|record| record.parent == parent)
            .filter_map(|record| record.name.clone())
            .collect::<Vec<_>>();
        let name = allocate_unique_name(&sibling_names, &base_name);
        let path = parent_path
            .map(|parent_path| format!("{parent_path}/{name}"))
            .unwrap_or_else(|| name.clone());
        let nickname = allocate_nickname(
            agents
                .values()
                .filter_map(|record| record.nickname.as_deref()),
            requested_nickname.as_deref(),
            numeric_id,
        );
        let record = AgentRecord {
            id: id.clone(),
            parent,
            name: Some(name),
            path: Some(path),
            nickname,
            conversation,
            status: AgentStatus::Idle,
            retention,
            spawned_by_run_id,
            spawn_request_id,
            spawned_at_ms: now_ms(),
            settled_at_ms: None,
            closed_at_ms: None,
            subtasks: Vec::new(),
            sidechain_session_id: None,
            fork_context: None,
        };
        agents.insert(id.clone(), record.clone());
        drop(agents);
        info!(
            agent_id = %record.id.0,
            parent_agent_id = record.parent.as_ref().map(|parent| parent.0.as_str()),
            session_id = %record.conversation.session_id,
            thread_id = record.conversation.thread_id.as_deref(),
            retention = ?record.retention,
            name = record.name.as_deref(),
            path = record.path.as_deref(),
            nickname = record.nickname.as_deref(),
            spawn_request_id = record.spawn_request_id.as_deref(),
            spawned_by_run_id = record.spawned_by_run_id.as_deref(),
            "spawned agent record"
        );
        self.observer
            .record(TraceEvent::new(TraceEventKind::AgentSpawned {
                agent_id: id.0.clone(),
            }));
        self.record_audit("spawned", &record, None, Some(record.status.clone()), None);
        Ok(record)
    }

    /// Forks a sub-agent from an existing parent and captures the fork context.
    #[allow(clippy::too_many_arguments)]
    pub fn fork(
        &self,
        parent: AgentId,
        conversation: ConversationKey,
        fork_context: ForkContext,
        requested_name: Option<&str>,
        requested_nickname: Option<String>,
        retention: ChildRetentionPolicy,
        spawned_by_run_id: Option<String>,
        spawn_request_id: Option<String>,
    ) -> Result<AgentRecord> {
        let mut record = self.spawn(
            Some(parent),
            conversation,
            requested_name,
            requested_nickname,
            retention,
            spawned_by_run_id,
            spawn_request_id,
        )?;
        record.sidechain_session_id = Some(record.conversation.session_id.clone());
        record.fork_context = Some(fork_context);
        self.agents
            .lock()
            .expect("agents mutex poisoned")
            .insert(record.id.clone(), record.clone());
        self.record_audit("forked", &record, None, Some(record.status.clone()), None);
        info!(
            agent_id = %record.id.0,
            parent_agent_id = record.parent.as_ref().map(|parent| parent.0.as_str()),
            session_id = %record.conversation.session_id,
            sidechain_session_id = record.sidechain_session_id.as_deref(),
            worktree_path = record
                .fork_context
                .as_ref()
                .and_then(|fork| fork.worktree_path.as_deref()),
            team_name = record
                .fork_context
                .as_ref()
                .and_then(|fork| fork.team_name.as_deref()),
            "forked sidechain agent record"
        );
        Ok(record)
    }

    /// Returns a copy of an existing agent record.
    pub fn get(&self, id: &AgentId) -> Option<AgentRecord> {
        self.agents
            .lock()
            .expect("agents mutex poisoned")
            .get(id)
            .cloned()
    }

    /// Returns a copy of every known agent record.
    pub fn list(&self) -> Vec<AgentRecord> {
        self.agents
            .lock()
            .expect("agents mutex poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Returns cheap aggregate counts without cloning mailbox payloads or terminal snapshots.
    pub fn status_snapshot(&self) -> AgentSupervisorStatusSnapshot {
        let agents = self.agents.lock().expect("agents mutex poisoned");
        let mut snapshot = AgentSupervisorStatusSnapshot {
            total: agents.len(),
            ..Default::default()
        };

        for agent in agents.values() {
            if agent.parent.is_some() {
                snapshot.sidechain_count += 1;
            }
            if agent.closed_at_ms.is_some() {
                snapshot.closed_count += 1;
            }
            match agent.status {
                AgentStatus::Idle => snapshot.idle += 1,
                AgentStatus::Running => snapshot.running += 1,
                AgentStatus::WaitingForApproval => snapshot.waiting_for_approval += 1,
                AgentStatus::WaitingForUserInput => snapshot.waiting_for_user_input += 1,
                AgentStatus::Failed => snapshot.failed += 1,
                AgentStatus::Completed => snapshot.completed += 1,
            }
        }

        snapshot.terminal_snapshot_count = self
            .terminal_snapshots
            .lock()
            .expect("terminal snapshot mutex poisoned")
            .len();
        snapshot.mailbox_message_count = self
            .mailboxes
            .lock()
            .expect("mailboxes mutex poisoned")
            .values()
            .map(Vec::len)
            .sum();
        snapshot.audit_sink_error_count = self.audit_sink_error_count.load(Ordering::Relaxed);
        snapshot.last_audit_sink_error = self
            .audit_sink_last_error
            .lock()
            .expect("audit sink last error mutex poisoned")
            .clone();
        snapshot
    }

    /// Updates an agent status.
    pub fn set_status(&self, id: &AgentId, status: AgentStatus) -> Result<()> {
        let mut agents = self.agents.lock().expect("agents mutex poisoned");
        let Some(agent) = agents.get_mut(id) else {
            return Err(anyhow!("unknown agent {}", id.0));
        };
        let previous = agent.status.clone();
        agent.status = status;
        if matches!(
            agent.status,
            AgentStatus::Running
                | AgentStatus::WaitingForApproval
                | AgentStatus::WaitingForUserInput
        ) {
            agent.settled_at_ms = None;
            agent.closed_at_ms = None;
        }
        if previous != agent.status {
            debug!(
                agent_id = %agent.id.0,
                session_id = %agent.conversation.session_id,
                previous_status = ?previous,
                status = ?agent.status,
                retention = ?agent.retention,
                "updated agent status"
            );
        }
        let record = agent.clone();
        drop(agents);
        if previous != record.status {
            self.record_audit(
                "status_changed",
                &record,
                Some(previous),
                Some(record.status.clone()),
                None,
            );
        }
        Ok(())
    }

    /// Resumes a paused agent and returns the updated record.
    pub fn resume(&self, id: &AgentId) -> Result<AgentRecord> {
        self.set_status(id, AgentStatus::Running)?;
        self.get(id)
            .ok_or_else(|| anyhow!("unknown agent {}", id.0))
    }

    /// Appends a subtask to an agent.
    pub fn assign_subtask(&self, id: &AgentId, subtask: SubtaskSpec) -> Result<()> {
        let mut agents = self.agents.lock().expect("agents mutex poisoned");
        let Some(agent) = agents.get_mut(id) else {
            return Err(anyhow!("unknown agent {}", id.0));
        };
        debug!(
            agent_id = %agent.id.0,
            session_id = %agent.conversation.session_id,
            subtask_name = %subtask.name,
            "assigned subtask to agent"
        );
        agent.subtasks.push(subtask);
        Ok(())
    }

    /// Posts a mailbox message to the destination agent.
    pub fn post(&self, message: MailboxMessage) {
        debug!(
            from_agent_id = %message.from.0,
            to_agent_id = %message.to.0,
            subject = %message.subject,
            "queued mailbox message"
        );
        self.mailboxes
            .lock()
            .expect("mailboxes mutex poisoned")
            .entry(message.to.clone())
            .or_default()
            .push(message);
    }

    /// Posts a mailbox message only when an equivalent pending message is absent.
    pub fn post_if_absent(&self, message: MailboxMessage) -> bool {
        let mut mailboxes = self.mailboxes.lock().expect("mailboxes mutex poisoned");
        let messages = mailboxes.entry(message.to.clone()).or_default();
        if messages
            .iter()
            .any(|existing| existing.matches_delivery_payload(&message))
        {
            return false;
        }
        debug!(
            from_agent_id = %message.from.0,
            to_agent_id = %message.to.0,
            subject = %message.subject,
            message_id = %message.id,
            "queued mailbox message"
        );
        messages.push(message);
        true
    }

    /// Drains all pending mailbox messages for an agent.
    pub fn drain_mailbox(&self, id: &AgentId) -> Vec<MailboxMessage> {
        self.mailboxes
            .lock()
            .expect("mailboxes mutex poisoned")
            .remove(id)
            .unwrap_or_default()
    }

    /// Returns a copy of the pending mailbox messages for an agent.
    pub fn peek_mailbox(&self, id: &AgentId) -> Vec<MailboxMessage> {
        self.mailboxes
            .lock()
            .expect("mailboxes mutex poisoned")
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// Returns the number of pending mailbox messages for an agent.
    pub fn mailbox_len(&self, id: &AgentId) -> usize {
        self.mailboxes
            .lock()
            .expect("mailboxes mutex poisoned")
            .get(id)
            .map(Vec::len)
            .unwrap_or(0)
    }

    /// Returns pending mailbox message counts keyed by destination agent.
    pub fn mailbox_counts(&self) -> BTreeMap<AgentId, usize> {
        self.mailboxes
            .lock()
            .expect("mailboxes mutex poisoned")
            .iter()
            .map(|(agent_id, messages)| (agent_id.clone(), messages.len()))
            .collect()
    }

    /// Removes a prefix of pending mailbox messages after they have been durably scheduled.
    pub fn ack_mailbox_prefix(&self, id: &AgentId, count: usize) {
        let mut mailboxes = self.mailboxes.lock().expect("mailboxes mutex poisoned");
        let Some(messages) = mailboxes.get_mut(id) else {
            return;
        };
        let drain = count.min(messages.len());
        messages.drain(..drain);
        if messages.is_empty() {
            mailboxes.remove(id);
        }
    }

    /// Removes pending mailbox messages matching the provided message ids.
    pub fn ack_mailbox_message_ids(&self, id: &AgentId, message_ids: &[String]) -> usize {
        let mut mailboxes = self.mailboxes.lock().expect("mailboxes mutex poisoned");
        let Some(messages) = mailboxes.get_mut(id) else {
            return 0;
        };
        let original_len = messages.len();
        messages.retain(|message| {
            message.id.is_empty()
                || !message_ids
                    .iter()
                    .any(|message_id| message_id == &message.id)
        });
        let removed = original_len.saturating_sub(messages.len());
        if messages.is_empty() {
            mailboxes.remove(id);
        }
        removed
    }

    /// Removes one matching pending mailbox message after the same payload is already durable.
    pub fn ack_mailbox_message(&self, id: &AgentId, expected: &MailboxMessage) -> bool {
        let mut mailboxes = self.mailboxes.lock().expect("mailboxes mutex poisoned");
        let Some(messages) = mailboxes.get_mut(id) else {
            return false;
        };
        let Some(index) = messages
            .iter()
            .position(|message| message.matches_delivery_payload(expected))
        else {
            return false;
        };
        messages.remove(index);
        if messages.is_empty() {
            mailboxes.remove(id);
        }
        true
    }

    /// Removes all pending mailbox messages for one agent and returns them.
    pub fn clear_mailbox(&self, id: &AgentId) -> Vec<MailboxMessage> {
        self.mailboxes
            .lock()
            .expect("mailboxes mutex poisoned")
            .remove(id)
            .unwrap_or_default()
    }

    /// Moves pending mailbox messages into the dead-letter queue.
    pub fn dead_letter_mailbox_messages(
        &self,
        id: &AgentId,
        messages: Vec<MailboxMessage>,
        reason: &str,
    ) -> usize {
        let dead_letters = messages
            .into_iter()
            .map(|message| message.dead_lettered(reason))
            .collect::<Vec<_>>();
        let count = dead_letters.len();
        if count == 0 {
            return 0;
        }
        self.mailbox_dead_letters
            .lock()
            .expect("mailbox dead letter mutex poisoned")
            .entry(id.clone())
            .or_default()
            .extend(dead_letters);
        count
    }

    /// Moves all pending mailbox messages for one agent into the dead-letter queue.
    pub fn dead_letter_mailbox(&self, id: &AgentId, reason: &str) -> usize {
        let messages = self.clear_mailbox(id);
        self.dead_letter_mailbox_messages(id, messages, reason)
    }

    /// Returns dead-lettered mailbox messages for an agent.
    pub fn mailbox_dead_letters(&self, id: &AgentId) -> Vec<MailboxMessage> {
        self.mailbox_dead_letters
            .lock()
            .expect("mailbox dead letter mutex poisoned")
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// Updates one agent record in-place and returns the latest copy.
    pub fn update_record<F>(&self, id: &AgentId, update: F) -> Result<AgentRecord>
    where
        F: FnOnce(&mut AgentRecord),
    {
        let mut agents = self.agents.lock().expect("agents mutex poisoned");
        let Some(agent) = agents.get_mut(id) else {
            return Err(anyhow!("unknown agent {}", id.0));
        };
        update(agent);
        Ok(agent.clone())
    }

    /// Sets or clears the human-readable nickname for one agent.
    ///
    /// When a nickname is requested, the supervisor preserves global uniqueness by
    /// allocating a numeric suffix when needed. Clearing the nickname falls back to
    /// the stable agent name or identifier in downstream views.
    pub fn set_nickname(
        &self,
        id: &AgentId,
        requested_nickname: Option<&str>,
    ) -> Result<AgentRecord> {
        let updated = {
            let mut agents = self.agents.lock().expect("agents mutex poisoned");
            let Some(current) = agents.get(id) else {
                return Err(anyhow!("unknown agent {}", id.0));
            };
            let numeric_id = current
                .id
                .0
                .strip_prefix("agent-")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(1);
            let nickname = requested_nickname
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|requested| {
                    allocate_nickname(
                        agents
                            .iter()
                            .filter(|(agent_id, _)| *agent_id != id)
                            .filter_map(|(_, record)| record.nickname.as_deref()),
                        Some(requested),
                        numeric_id,
                    )
                    .expect("requested nickname should always allocate")
                });
            let agent = agents
                .get_mut(id)
                .expect("agent should remain present during nickname update");
            agent.nickname = nickname;
            agent.clone()
        };
        self.record_audit(
            "nickname_updated",
            &updated,
            None,
            Some(updated.status.clone()),
            None,
        );

        let mut terminal_snapshots = self
            .terminal_snapshots
            .lock()
            .expect("terminal snapshot mutex poisoned");
        if let Some(snapshot) = terminal_snapshots.get_mut(id) {
            snapshot.agent.nickname = updated.nickname.clone();
        }
        Ok(updated)
    }

    /// Returns the direct children of one agent.
    pub fn children_of(&self, parent: &AgentId) -> Vec<AgentRecord> {
        self.agents
            .lock()
            .expect("agents mutex poisoned")
            .values()
            .filter(|record| record.parent.as_ref() == Some(parent))
            .cloned()
            .collect()
    }

    /// Returns all descendants of one agent in stable order.
    pub fn descendants_of(&self, parent: &AgentId) -> Vec<AgentRecord> {
        let agents = self.agents.lock().expect("agents mutex poisoned");
        collect_descendants(&agents, parent)
    }

    /// Returns the depth of one agent within the parent/child tree.
    pub fn depth_of(&self, id: &AgentId) -> Option<usize> {
        let agents = self.agents.lock().expect("agents mutex poisoned");
        let mut depth = 0usize;
        let mut current = agents.get(id)?;
        while let Some(parent) = current.parent.as_ref() {
            current = agents.get(parent)?;
            depth += 1;
        }
        Some(depth)
    }

    /// Returns the root ancestor for one agent.
    pub fn root_of(&self, id: &AgentId) -> Option<AgentRecord> {
        let agents = self.agents.lock().expect("agents mutex poisoned");
        let mut current = agents.get(id)?.clone();
        while let Some(parent) = current.parent.as_ref() {
            current = agents.get(parent)?.clone();
        }
        Some(current)
    }

    /// Returns whether two agent records belong to the same root tree.
    ///
    /// A missing left-hand agent is treated as an error because callers usually
    /// use it as the authenticated actor. A missing right-hand agent returns
    /// `false`, which preserves visibility checks for unknown targets.
    pub fn shares_root_with(&self, left: &AgentId, right: &AgentId) -> Result<bool> {
        let agents = self.agents.lock().expect("agents mutex poisoned");
        let left_root = root_id_from_agents(&agents, left)?
            .ok_or_else(|| anyhow!("unknown agent {}", left.0))?;
        let Some(right_root) = root_id_from_agents(&agents, right)? else {
            return Ok(false);
        };
        Ok(left_root == right_root)
    }

    /// Returns every record in the same root tree as the requested agent.
    ///
    /// The returned order is stable: root first, then depth-first descendants
    /// ordered by agent identifier at each sibling level.
    pub fn root_tree_records(&self, id: &AgentId) -> Result<Vec<AgentRecord>> {
        let agents = self.agents.lock().expect("agents mutex poisoned");
        let root_id =
            root_id_from_agents(&agents, id)?.ok_or_else(|| anyhow!("unknown agent {}", id.0))?;
        let root = agents
            .get(&root_id)
            .ok_or_else(|| anyhow!("unknown agent {}", root_id.0))?
            .clone();
        let mut records = vec![root];
        records.extend(collect_descendants(&agents, &root_id));
        Ok(records)
    }

    /// Stores the latest terminal snapshot for a closed agent.
    pub fn record_terminal_snapshot(&self, snapshot: ManagedAgentSnapshot) {
        let agent_id = snapshot.agent.id.clone();
        self.terminal_snapshots
            .lock()
            .expect("terminal snapshot mutex poisoned")
            .insert(agent_id.clone(), snapshot);
        if let Some(record) = self.get(&agent_id) {
            self.record_audit(
                "terminal_snapshot_recorded",
                &record,
                None,
                Some(record.status.clone()),
                None,
            );
        }
    }

    /// Returns the latest terminal snapshot for a closed agent.
    pub fn terminal_snapshot(&self, id: &AgentId) -> Option<ManagedAgentSnapshot> {
        self.terminal_snapshots
            .lock()
            .expect("terminal snapshot mutex poisoned")
            .get(id)
            .cloned()
    }

    /// Removes one persisted terminal snapshot after the runtime is reopened.
    pub fn clear_terminal_snapshot(&self, id: &AgentId) {
        self.terminal_snapshots
            .lock()
            .expect("terminal snapshot mutex poisoned")
            .remove(id);
    }

    /// Removes one agent record together with any cached mailbox or terminal state.
    ///
    /// This is intended for rollback paths that must discard an agent before it
    /// ever becomes part of durable topology.
    pub fn remove_agent(&self, id: &AgentId) -> Option<AgentRecord> {
        self.mailboxes
            .lock()
            .expect("mailboxes mutex poisoned")
            .remove(id);
        self.mailbox_dead_letters
            .lock()
            .expect("mailbox dead letter mutex poisoned")
            .remove(id);
        self.terminal_snapshots
            .lock()
            .expect("terminal snapshot mutex poisoned")
            .remove(id);
        let removed = self
            .agents
            .lock()
            .expect("agents mutex poisoned")
            .remove(id);
        if let Some(record) = removed.as_ref() {
            self.record_audit("removed", record, None, Some(record.status.clone()), None);
        }
        removed
    }

    /// Records an operator-visible supervisor lifecycle event.
    pub fn record_lifecycle_event(&self, event: &str, record: &AgentRecord, reason: Option<&str>) {
        self.record_audit(
            event,
            record,
            None,
            Some(record.status.clone()),
            reason.map(str::to_string),
        );
    }

    /// Returns the bounded lifecycle audit log, optionally scoped to one agent.
    pub fn audit_log(&self, agent_id: Option<&AgentId>) -> Vec<AgentSupervisorAuditEntry> {
        self.audit_log
            .lock()
            .expect("audit log mutex poisoned")
            .iter()
            .filter(|entry| agent_id.is_none_or(|agent_id| &entry.agent_id == agent_id))
            .cloned()
            .collect()
    }

    fn record_audit(
        &self,
        event: &str,
        record: &AgentRecord,
        previous_status: Option<AgentStatus>,
        status: Option<AgentStatus>,
        reason: Option<String>,
    ) {
        let mut next_audit_id = self
            .next_audit_id
            .lock()
            .expect("next audit id mutex poisoned");
        *next_audit_id += 1;
        let entry = AgentSupervisorAuditEntry {
            audit_id: *next_audit_id,
            timestamp_ms: now_ms(),
            event: event.to_string(),
            agent_id: record.id.clone(),
            parent_agent_id: record.parent.clone(),
            session_id: Some(record.conversation.session_id.clone()),
            previous_status,
            status,
            reason,
        };
        let mut audit_log = self.audit_log.lock().expect("audit log mutex poisoned");
        audit_log.push(entry.clone());
        let overflow = audit_log.len().saturating_sub(AGENT_SUPERVISOR_AUDIT_LIMIT);
        if overflow > 0 {
            audit_log.drain(..overflow);
        }
        drop(audit_log);
        let sink = self
            .audit_sink
            .lock()
            .expect("audit sink mutex poisoned")
            .clone();
        if let Some(sink) = sink
            && let Err(error) = sink.append_supervisor_audit(&entry)
        {
            self.audit_sink_error_count.fetch_add(1, Ordering::Relaxed);
            *self
                .audit_sink_last_error
                .lock()
                .expect("audit sink last error mutex poisoned") = Some(error.to_string());
            warn!(
                audit_id = entry.audit_id,
                agent_id = %entry.agent_id.0,
                event = %entry.event,
                error = %error,
                "failed to append supervisor audit entry"
            );
        }
    }

    /// Returns a serializable snapshot of the supervisor state.
    pub fn snapshot(&self) -> AgentSupervisorSnapshot {
        AgentSupervisorSnapshot {
            next_id: *self.next_id.lock().expect("next_id mutex poisoned"),
            next_audit_id: *self
                .next_audit_id
                .lock()
                .expect("next audit id mutex poisoned"),
            agents: self.agents.lock().expect("agents mutex poisoned").clone(),
            terminal_snapshots: self
                .terminal_snapshots
                .lock()
                .expect("terminal snapshot mutex poisoned")
                .clone(),
            mailboxes: self
                .mailboxes
                .lock()
                .expect("mailboxes mutex poisoned")
                .clone(),
            mailbox_dead_letters: self
                .mailbox_dead_letters
                .lock()
                .expect("mailbox dead letter mutex poisoned")
                .clone(),
            audit_log: self
                .audit_log
                .lock()
                .expect("audit log mutex poisoned")
                .clone(),
        }
    }
}

fn allocate_unique_name(existing_names: &[String], base_name: &str) -> String {
    if !existing_names.iter().any(|name| name == base_name) {
        return base_name.to_string();
    }
    let mut ordinal = 2usize;
    loop {
        let candidate = format!("{base_name}_{ordinal}");
        if !existing_names.iter().any(|name| name == &candidate) {
            return candidate;
        }
        ordinal += 1;
    }
}

fn allocate_nickname<'a>(
    existing_nicknames: impl Iterator<Item = &'a str>,
    requested_nickname: Option<&str>,
    numeric_id: u64,
) -> Option<String> {
    let base = requested_nickname
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            AGENT_NICKNAMES[((numeric_id.saturating_sub(1)) as usize) % AGENT_NICKNAMES.len()]
                .to_string()
        });
    let existing = existing_nicknames.collect::<Vec<_>>();
    if !existing.iter().any(|nickname| *nickname == base) {
        return Some(base);
    }
    let mut ordinal = 2usize;
    loop {
        let candidate = format!("{base} {ordinal}");
        if !existing.iter().any(|nickname| *nickname == candidate) {
            return Some(candidate);
        }
        ordinal += 1;
    }
}

fn root_id_from_agents(
    agents: &BTreeMap<AgentId, AgentRecord>,
    id: &AgentId,
) -> Result<Option<AgentId>> {
    let Some(mut current) = agents.get(id) else {
        return Ok(None);
    };
    let mut visited = BTreeSet::new();
    loop {
        if !visited.insert(current.id.clone()) {
            return Err(anyhow!(
                "agent tree contains a parent cycle at {}",
                current.id.0
            ));
        }
        let Some(parent_id) = current.parent.as_ref() else {
            return Ok(Some(current.id.clone()));
        };
        current = agents.get(parent_id).ok_or_else(|| {
            anyhow!(
                "agent {} references missing parent {}",
                current.id.0,
                parent_id.0
            )
        })?;
    }
}

fn validate_snapshot(snapshot: &AgentSupervisorSnapshot) -> Result<()> {
    validate_agents(&snapshot.agents)?;
    validate_terminal_snapshots(&snapshot.agents, &snapshot.terminal_snapshots)?;
    for agent_id in snapshot.mailboxes.keys() {
        anyhow::ensure!(
            snapshot.agents.contains_key(agent_id),
            "mailbox references unknown agent {}",
            agent_id.0
        );
    }
    for agent_id in snapshot.mailbox_dead_letters.keys() {
        anyhow::ensure!(
            snapshot.agents.contains_key(agent_id),
            "mailbox dead letter queue references unknown agent {}",
            agent_id.0
        );
    }
    validate_next_id(snapshot.next_id, &snapshot.agents)?;
    validate_audit_log(snapshot.next_audit_id, &snapshot.audit_log)?;
    Ok(())
}

fn repair_snapshot(mut snapshot: AgentSupervisorSnapshot) -> AgentSupervisorSnapshot {
    let max_agent_numeric_id = snapshot
        .agents
        .keys()
        .filter_map(numeric_agent_id)
        .max()
        .unwrap_or(0);
    snapshot.next_id = snapshot.next_id.max(max_agent_numeric_id);
    let max_audit_id = snapshot
        .audit_log
        .iter()
        .map(|entry| entry.audit_id)
        .max()
        .unwrap_or(0);
    snapshot.next_audit_id = snapshot.next_audit_id.max(max_audit_id);
    snapshot.audit_log = trim_audit_log(snapshot.audit_log);

    snapshot.mailboxes.retain(|agent_id, _| {
        let keep = snapshot.agents.contains_key(agent_id);
        if !keep {
            debug!(agent_id = %agent_id.0, "dropping mailbox for unknown restored agent");
        }
        keep
    });
    for (agent_id, messages) in snapshot.mailboxes.iter_mut() {
        repair_mailbox_messages(agent_id, messages, "pending");
    }
    snapshot.mailbox_dead_letters.retain(|agent_id, _| {
        let keep = snapshot.agents.contains_key(agent_id);
        if !keep {
            debug!(agent_id = %agent_id.0, "dropping dead-letter mailbox for unknown restored agent");
        }
        keep
    });
    for (agent_id, messages) in snapshot.mailbox_dead_letters.iter_mut() {
        repair_mailbox_messages(agent_id, messages, "dead");
    }
    snapshot.terminal_snapshots.retain(|agent_id, terminal| {
        let keep = snapshot.agents.contains_key(agent_id) && terminal.agent.id == *agent_id;
        if !keep {
            debug!(
                agent_id = %agent_id.0,
                terminal_agent_id = %terminal.agent.id.0,
                "dropping terminal snapshot with invalid restored agent reference"
            );
        }
        keep
    });
    for (agent_id, terminal) in snapshot.terminal_snapshots.iter_mut() {
        if let Some(record) = snapshot.agents.get(agent_id) {
            terminal.agent = record.clone();
        }
    }
    snapshot
}

fn repair_mailbox_messages(agent_id: &AgentId, messages: &mut [MailboxMessage], prefix: &str) {
    for (index, message) in messages.iter_mut().enumerate() {
        if message.schema_version == 0 {
            message.schema_version = MailboxMessage::SCHEMA_VERSION;
        }
        if message.id.trim().is_empty() {
            message.id = format!("legacy-mailbox-{}-{prefix}-{index}", agent_id.0);
        }
    }
}

fn validate_agents(agents: &BTreeMap<AgentId, AgentRecord>) -> Result<()> {
    let mut sibling_names: BTreeMap<Option<AgentId>, BTreeSet<String>> = BTreeMap::new();
    let mut paths = BTreeSet::new();
    let mut session_ids: BTreeMap<String, AgentId> = BTreeMap::new();
    for (agent_id, record) in agents {
        anyhow::ensure!(
            agent_id == &record.id,
            "agent map key {} does not match record id {}",
            agent_id.0,
            record.id.0
        );
        if let Some(previous_agent_id) =
            session_ids.insert(record.conversation.session_id.clone(), agent_id.clone())
        {
            anyhow::ensure!(
                previous_agent_id == *agent_id,
                "agent {} shares session {} with agent {}",
                agent_id.0,
                record.conversation.session_id,
                previous_agent_id.0
            );
        }
        if let Some(parent_id) = record.parent.as_ref() {
            anyhow::ensure!(
                parent_id != &record.id,
                "agent {} cannot be its own parent",
                record.id.0
            );
            anyhow::ensure!(
                agents.contains_key(parent_id),
                "agent {} references missing parent {}",
                record.id.0,
                parent_id.0
            );
        }
        if let Some(sidechain_session_id) = record.sidechain_session_id.as_ref() {
            anyhow::ensure!(
                record.parent.is_some(),
                "root agent {} cannot have sidechain session {}",
                agent_id.0,
                sidechain_session_id
            );
            anyhow::ensure!(
                sidechain_session_id == &record.conversation.session_id,
                "agent {} sidechain session {} does not match conversation session {}",
                agent_id.0,
                sidechain_session_id,
                record.conversation.session_id
            );
        }
        if record.fork_context.is_some() {
            anyhow::ensure!(
                record.sidechain_session_id.as_deref()
                    == Some(record.conversation.session_id.as_str()),
                "forked agent {} must bind sidechain session {}",
                agent_id.0,
                record.conversation.session_id
            );
        }
        if let Some(name) = record.name.as_ref() {
            anyhow::ensure!(
                !name.trim().is_empty(),
                "agent {} has an empty name",
                record.id.0
            );
            let inserted = sibling_names
                .entry(record.parent.clone())
                .or_default()
                .insert(name.clone());
            anyhow::ensure!(
                inserted,
                "agent {} has duplicate sibling name {}",
                record.id.0,
                name
            );
        }
        if let Some(path) = record.path.as_ref() {
            anyhow::ensure!(
                !path.trim().is_empty(),
                "agent {} has an empty path",
                record.id.0
            );
            let inserted = paths.insert(path.clone());
            anyhow::ensure!(
                inserted,
                "agent {} has duplicate path {}",
                record.id.0,
                path
            );
            if let Some(name) = record.name.as_ref() {
                match record.parent.as_ref() {
                    Some(parent_id) => {
                        if let Some(parent_path) = agents
                            .get(parent_id)
                            .and_then(|parent| parent.path.as_ref())
                        {
                            let expected = format!("{parent_path}/{name}");
                            anyhow::ensure!(
                                path == &expected,
                                "agent {} path {} does not match expected {}",
                                record.id.0,
                                path,
                                expected
                            );
                        }
                    }
                    None => anyhow::ensure!(
                        path == name,
                        "root agent {} path {} does not match name {}",
                        record.id.0,
                        path,
                        name
                    ),
                }
            }
        }
    }

    let mut covered = BTreeSet::new();
    for record in agents.values() {
        root_id_from_agents(agents, &record.id)?;
        if record.parent.is_none() {
            covered.insert(record.id.clone());
            for descendant in collect_descendants(agents, &record.id) {
                anyhow::ensure!(
                    covered.insert(descendant.id.clone()),
                    "agent {} appears more than once in root traversal",
                    descendant.id.0
                );
            }
        }
    }
    anyhow::ensure!(
        covered.len() == agents.len(),
        "agent topology traversal covered {} of {} agents",
        covered.len(),
        agents.len()
    );
    Ok(())
}

fn validate_terminal_snapshots(
    agents: &BTreeMap<AgentId, AgentRecord>,
    terminal_snapshots: &BTreeMap<AgentId, ManagedAgentSnapshot>,
) -> Result<()> {
    for (agent_id, snapshot) in terminal_snapshots {
        anyhow::ensure!(
            agents.contains_key(agent_id),
            "terminal snapshot references unknown agent {}",
            agent_id.0
        );
        anyhow::ensure!(
            snapshot.agent.id == *agent_id,
            "terminal snapshot key {} does not match agent id {}",
            agent_id.0,
            snapshot.agent.id.0
        );
    }
    Ok(())
}

fn validate_next_id(next_id: u64, agents: &BTreeMap<AgentId, AgentRecord>) -> Result<()> {
    let max_agent_numeric_id = agents
        .keys()
        .filter_map(numeric_agent_id)
        .max()
        .unwrap_or(0);
    anyhow::ensure!(
        next_id >= max_agent_numeric_id,
        "next agent id {} is behind existing agent id {}",
        next_id,
        max_agent_numeric_id
    );
    Ok(())
}

fn validate_audit_log(next_audit_id: u64, audit_log: &[AgentSupervisorAuditEntry]) -> Result<()> {
    anyhow::ensure!(
        audit_log.len() <= AGENT_SUPERVISOR_AUDIT_LIMIT,
        "supervisor audit log has {} entries, limit is {}",
        audit_log.len(),
        AGENT_SUPERVISOR_AUDIT_LIMIT
    );
    let mut previous_id = 0u64;
    for entry in audit_log {
        anyhow::ensure!(entry.audit_id > 0, "audit entry id must be positive");
        anyhow::ensure!(
            entry.audit_id > previous_id,
            "audit entry id {} is not strictly greater than {}",
            entry.audit_id,
            previous_id
        );
        anyhow::ensure!(
            !entry.event.trim().is_empty(),
            "audit entry {} has an empty event",
            entry.audit_id
        );
        previous_id = entry.audit_id;
    }
    anyhow::ensure!(
        next_audit_id >= previous_id,
        "next audit id {} is behind existing audit id {}",
        next_audit_id,
        previous_id
    );
    Ok(())
}

fn trim_audit_log(mut audit_log: Vec<AgentSupervisorAuditEntry>) -> Vec<AgentSupervisorAuditEntry> {
    let overflow = audit_log.len().saturating_sub(AGENT_SUPERVISOR_AUDIT_LIMIT);
    if overflow > 0 {
        audit_log.drain(..overflow);
    }
    audit_log
}

fn numeric_agent_id(id: &AgentId) -> Option<u64> {
    id.0.strip_prefix("agent-")?.parse::<u64>().ok()
}

fn collect_descendants(
    agents: &BTreeMap<AgentId, AgentRecord>,
    parent: &AgentId,
) -> Vec<AgentRecord> {
    let mut children_by_parent: BTreeMap<AgentId, Vec<AgentRecord>> = BTreeMap::new();
    for record in agents.values() {
        if let Some(parent_id) = record.parent.as_ref() {
            children_by_parent
                .entry(parent_id.clone())
                .or_default()
                .push(record.clone());
        }
    }

    let mut descendants = Vec::new();
    let mut stack = children_by_parent.remove(parent).unwrap_or_default();
    stack.reverse();
    while let Some(record) = stack.pop() {
        let record_id = record.id.clone();
        descendants.push(record);
        if let Some(mut children) = children_by_parent.remove(&record_id) {
            children.reverse();
            stack.extend(children);
        }
    }
    descendants
}
