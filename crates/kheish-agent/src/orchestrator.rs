use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use kheish_core::{LoopPolicy, ModelDriver, RunOutcome};
use kheish_runtime::{
    AgentRuntime, AgentRuntimeDependencies, AgentRuntimeRestore, ExecutionScope,
    bounded_workspace_root, interrupted_error, is_interrupted_error, scope_execution,
};
use kheish_types::{
    ApprovalResolution, ConversationKey, InputEnvelope, ModelGenerationConfig,
    UserQuestionResolution,
};
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::snapshot::{
    agent_default_generation, agent_prompt_override, agent_tool_surface, agent_workspace_root,
    build_snapshot,
};
use crate::supervisor::AgentSupervisor;
use crate::types::{
    AgentId, AgentRecord, AgentStatus, ChildRetentionPolicy, DaemonOwnedWorktree, ForkContext,
    InterruptResult, ManagedAgentSnapshot, SubtaskSpec,
};

fn filter_resolved_approvals_from_snapshot(
    snapshot: &mut ManagedAgentSnapshot,
    resolutions: &[ApprovalResolution],
) {
    if resolutions.is_empty() || snapshot.pending_approvals.is_empty() {
        return;
    }
    let resolved = resolutions
        .iter()
        .map(|resolution| resolution.request_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    snapshot
        .pending_approvals
        .retain(|request| !resolved.contains(request.id.as_str()));
}

enum SessionCommand {
    SubmitInput {
        input: InputEnvelope,
        generation: ModelGenerationConfig,
        run_id: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        respond_to: oneshot::Sender<Result<ManagedAgentSnapshot>>,
    },
    ResumeApprovals {
        resolutions: Vec<ApprovalResolution>,
        run_id: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        respond_to: oneshot::Sender<Result<ManagedAgentSnapshot>>,
    },
    ResumeUserQuestion {
        resolution: UserQuestionResolution,
        run_id: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        respond_to: oneshot::Sender<Result<ManagedAgentSnapshot>>,
    },
    Snapshot {
        respond_to: oneshot::Sender<Result<ManagedAgentSnapshot>>,
    },
    Interrupt {
        respond_to: oneshot::Sender<Result<InterruptResult>>,
    },
}

#[derive(Clone)]
struct SessionHandle {
    sender: mpsc::UnboundedSender<SessionCommand>,
}

struct InFlightRun {
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<Result<RunOutcome>>,
    respond_to: oneshot::Sender<Result<ManagedAgentSnapshot>>,
    interrupt_waiters: Vec<oneshot::Sender<Result<InterruptResult>>>,
}

/// Coordinates long-lived runtimes behind the multi-agent supervisor.
pub struct AgentOrchestrator<M> {
    deps: AgentRuntimeDependencies<M>,
    policy: LoopPolicy,
    supervisor: Arc<AgentSupervisor>,
    sessions: Mutex<BTreeMap<AgentId, SessionHandle>>,
}

impl<M> AgentOrchestrator<M>
where
    M: ModelDriver + Send + Sync + 'static,
{
    fn validated_workspace_root_override(
        &self,
        fork_context: Option<&ForkContext>,
    ) -> Result<Option<std::path::PathBuf>> {
        agent_workspace_root(fork_context)
            .map(|path| {
                bounded_workspace_root(&self.deps.system_prompt.environment().workspace_root, &path)
            })
            .transpose()
    }

    /// Creates a new orchestrator backed by the provided runtime dependencies.
    pub fn new(
        policy: LoopPolicy,
        deps: AgentRuntimeDependencies<M>,
        supervisor: Arc<AgentSupervisor>,
    ) -> Self {
        Self {
            deps,
            policy,
            supervisor,
            sessions: Mutex::new(BTreeMap::new()),
        }
    }

    /// Returns the underlying supervisor.
    pub fn supervisor(&self) -> Arc<AgentSupervisor> {
        self.supervisor.clone()
    }

    /// Restores runtime actors for every record already present in the supervisor.
    pub async fn restore_registered_agents(&self) -> Result<()> {
        let mut restored = 0usize;
        for record in self.supervisor.list() {
            if record.closed_at_ms.is_some() {
                continue;
            }
            if self.sessions.lock().contains_key(&record.id) {
                continue;
            }
            let runtime = AgentRuntime::restore(
                AgentRuntimeRestore {
                    conversation: record.conversation.clone(),
                    policy: self.policy.clone(),
                    agent_prompt: agent_prompt_override(record.fork_context.as_ref()),
                    default_generation: agent_default_generation(record.fork_context.as_ref()),
                    tool_surface: agent_tool_surface(record.fork_context.as_ref()),
                    workspace_root_override: self
                        .validated_workspace_root_override(record.fork_context.as_ref())?,
                },
                self.deps.clone(),
            )
            .await?;
            self.register_runtime(record, runtime)?;
            restored += 1;
        }
        info!(
            restored_agents = restored,
            "restored registered agent runtimes"
        );
        Ok(())
    }

    /// Spawns a root agent and starts its session actor.
    pub async fn spawn_root(&self, conversation: ConversationKey) -> Result<ManagedAgentSnapshot> {
        let record = self.supervisor.spawn(
            None,
            conversation,
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        self.register_runtime(
            record.clone(),
            AgentRuntime::new(
                record.conversation.clone(),
                self.policy.clone(),
                self.deps.clone(),
                None,
                ModelGenerationConfig::default(),
                kheish_types::ToolSurfaceFilter::default(),
                None,
            ),
        )?;
        info!(
            agent_id = %record.id.0,
            session_id = %record.conversation.session_id,
            thread_id = record.conversation.thread_id.as_deref(),
            "spawned root agent runtime"
        );
        self.snapshot(&record.id).await
    }

    /// Spawns a sidechain agent, optionally assigning an initial subtask.
    pub async fn spawn_sidechain(
        &self,
        parent: &AgentId,
        conversation: ConversationKey,
        fork_context: ForkContext,
        requested_name: Option<String>,
        requested_nickname: Option<String>,
        retention: ChildRetentionPolicy,
        spawned_by_run_id: Option<String>,
        spawn_request_id: Option<String>,
        daemon_owned_worktree: Option<DaemonOwnedWorktree>,
        subtask: Option<SubtaskSpec>,
    ) -> Result<ManagedAgentSnapshot> {
        let record = self.supervisor.fork(
            parent.clone(),
            conversation,
            fork_context,
            requested_name.as_deref(),
            requested_nickname,
            retention,
            spawned_by_run_id,
            spawn_request_id,
            daemon_owned_worktree,
        )?;
        if let Some(subtask) = subtask {
            if let Err(error) = self.supervisor.assign_subtask(&record.id, subtask) {
                self.supervisor.remove_agent(&record.id);
                return Err(error);
            }
        }
        let workspace_root_override =
            match self.validated_workspace_root_override(record.fork_context.as_ref()) {
                Ok(workspace_root_override) => workspace_root_override,
                Err(error) => {
                    self.supervisor.remove_agent(&record.id);
                    return Err(error);
                }
            };
        if let Err(error) = self.register_runtime(
            record.clone(),
            AgentRuntime::new(
                record.conversation.clone(),
                self.policy.clone(),
                self.deps.clone(),
                agent_prompt_override(record.fork_context.as_ref()),
                agent_default_generation(record.fork_context.as_ref()),
                agent_tool_surface(record.fork_context.as_ref()),
                workspace_root_override,
            ),
        ) {
            let _ = self.close_runtime(&record.id);
            self.supervisor.remove_agent(&record.id);
            return Err(error);
        }
        info!(
            agent_id = %record.id.0,
            parent_agent_id = record.parent.as_ref().map(|parent| parent.0.as_str()),
            session_id = %record.conversation.session_id,
            sidechain_session_id = record.sidechain_session_id.as_deref(),
            retention = ?record.retention,
            path = record.path.as_deref(),
            spawned_by_run_id = record.spawned_by_run_id.as_deref(),
            "spawned sidechain agent runtime"
        );
        match self.snapshot(&record.id).await {
            Ok(snapshot) => Ok(snapshot),
            Err(error) => {
                let _ = self.close_runtime(&record.id);
                self.supervisor.remove_agent(&record.id);
                Err(error)
            }
        }
    }

    /// Submits one normalized input to the target agent.
    pub async fn submit_input(
        &self,
        agent_id: &AgentId,
        input: InputEnvelope,
        generation: ModelGenerationConfig,
    ) -> Result<ManagedAgentSnapshot> {
        let model = generation.model.clone();
        self.submit_input_for_run(agent_id, input, generation, None, None, model)
            .await
    }

    /// Submits one normalized input to the target agent with optional run correlation.
    pub async fn submit_input_for_run(
        &self,
        agent_id: &AgentId,
        input: InputEnvelope,
        generation: ModelGenerationConfig,
        run_id: Option<String>,
        provider: Option<String>,
        model: Option<String>,
    ) -> Result<ManagedAgentSnapshot> {
        debug!(
            agent_id = %agent_id.0,
            session_id = %input.conversation.session_id,
            run_id = run_id.as_deref(),
            source_plugin = %input.source.plugin,
            source_kind = %input.source.kind,
            actor_id = %input.actor.id,
            provider = provider.as_deref(),
            model = model.as_deref().or(generation.model.as_deref()),
            "dispatching input to agent runtime"
        );
        let handle = self.session_handle(agent_id)?;
        let (respond_to, receive_from) = oneshot::channel();
        handle
            .sender
            .send(SessionCommand::SubmitInput {
                input,
                generation,
                run_id,
                provider,
                model,
                respond_to,
            })
            .map_err(|_| anyhow!("agent {} session is unavailable", agent_id.0))?;
        receive_from
            .await
            .map_err(|_| anyhow!("agent session dropped"))?
    }

    /// Resolves pending approvals for the target agent.
    pub async fn resume_approvals(
        &self,
        agent_id: &AgentId,
        resolutions: Vec<ApprovalResolution>,
    ) -> Result<ManagedAgentSnapshot> {
        self.resume_approvals_for_run(agent_id, resolutions, None, None, None)
            .await
    }

    /// Resolves pending approvals for the target agent with optional run correlation.
    pub async fn resume_approvals_for_run(
        &self,
        agent_id: &AgentId,
        resolutions: Vec<ApprovalResolution>,
        run_id: Option<String>,
        provider: Option<String>,
        model: Option<String>,
    ) -> Result<ManagedAgentSnapshot> {
        debug!(
            agent_id = %agent_id.0,
            run_id = run_id.as_deref(),
            provider = provider.as_deref(),
            model = model.as_deref(),
            approval_count = resolutions.len(),
            "resuming agent runtime after approvals"
        );
        let handle = self.session_handle(agent_id)?;
        let (respond_to, receive_from) = oneshot::channel();
        handle
            .sender
            .send(SessionCommand::ResumeApprovals {
                resolutions,
                run_id,
                provider,
                model,
                respond_to,
            })
            .map_err(|_| anyhow!("agent {} session is unavailable", agent_id.0))?;
        receive_from
            .await
            .map_err(|_| anyhow!("agent session dropped"))?
    }

    /// Resolves one pending structured user-question request with optional run correlation.
    pub async fn resume_user_question_for_run(
        &self,
        agent_id: &AgentId,
        resolution: UserQuestionResolution,
        run_id: Option<String>,
        provider: Option<String>,
        model: Option<String>,
    ) -> Result<ManagedAgentSnapshot> {
        debug!(
            agent_id = %agent_id.0,
            run_id = run_id.as_deref(),
            provider = provider.as_deref(),
            model = model.as_deref(),
            declined = resolution.declined,
            "resuming agent runtime after user question"
        );
        let handle = self.session_handle(agent_id)?;
        let (respond_to, receive_from) = oneshot::channel();
        handle
            .sender
            .send(SessionCommand::ResumeUserQuestion {
                resolution,
                run_id,
                provider,
                model,
                respond_to,
            })
            .map_err(|_| anyhow!("agent {} session is unavailable", agent_id.0))?;
        receive_from
            .await
            .map_err(|_| anyhow!("agent session dropped"))?
    }

    /// Returns the latest managed snapshot for one agent.
    pub async fn snapshot(&self, agent_id: &AgentId) -> Result<ManagedAgentSnapshot> {
        let handle = self.session_handle(agent_id)?;
        let (respond_to, receive_from) = oneshot::channel();
        handle
            .sender
            .send(SessionCommand::Snapshot { respond_to })
            .map_err(|_| anyhow!("agent {} session is unavailable", agent_id.0))?;
        receive_from
            .await
            .map_err(|_| anyhow!("agent session dropped"))?
    }

    /// Interrupts one in-flight run when the target agent is currently busy.
    pub async fn interrupt(&self, agent_id: &AgentId) -> Result<InterruptResult> {
        debug!(agent_id = %agent_id.0, "interrupt requested for agent runtime");
        let handle = self.session_handle(agent_id)?;
        let (respond_to, receive_from) = oneshot::channel();
        handle
            .sender
            .send(SessionCommand::Interrupt { respond_to })
            .map_err(|_| anyhow!("agent {} session is unavailable", agent_id.0))?;
        receive_from
            .await
            .map_err(|_| anyhow!("agent session dropped"))?
    }

    fn register_runtime(&self, record: AgentRecord, runtime: AgentRuntime<M>) -> Result<()> {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let supervisor = self.supervisor.clone();
        let agent_id = record.id.clone();
        let conversation = record.conversation.clone();
        self.supervisor.clear_terminal_snapshot(&agent_id);
        let workspace_root_override =
            self.validated_workspace_root_override(record.fork_context.as_ref())?;
        let restore = AgentRuntimeRestore {
            conversation: conversation.clone(),
            policy: self.policy.clone(),
            agent_prompt: agent_prompt_override(record.fork_context.as_ref()),
            default_generation: agent_default_generation(record.fork_context.as_ref()),
            tool_surface: agent_tool_surface(record.fork_context.as_ref()),
            workspace_root_override,
        };
        let deps = self.deps.clone();
        let scoped_workspace_root = restore
            .workspace_root_override
            .as_ref()
            .map(|path| path.display().to_string());
        debug!(
            agent_id = %agent_id.0,
            session_id = %conversation.session_id,
            parent_agent_id = record.parent.as_ref().map(|parent| parent.0.as_str()),
            workspace_root = scoped_workspace_root.as_deref(),
            "registering agent runtime"
        );
        tokio::spawn(async move {
            let runtime = Arc::new(tokio::sync::Mutex::new(runtime));
            let mut last_error = None;
            let mut cached_snapshot = {
                let runtime = runtime.lock().await;
                build_snapshot(&supervisor, &agent_id, &runtime, None)
                    .expect("initial snapshot should build")
            };
            let mut inflight: Option<InFlightRun> = None;

            loop {
                if let Some(active) = inflight.as_mut() {
                    tokio::select! {
                        biased;
                        maybe_command = receiver.recv() => {
                            let Some(command) = maybe_command else {
                                active.cancellation.cancel();
                                break;
                            };
                            match command {
                                SessionCommand::Snapshot { respond_to } => {
                                    let _ = respond_to.send(Ok(cached_snapshot.clone()));
                                }
                                SessionCommand::Interrupt { respond_to } => {
                                    active.cancellation.cancel();
                                    active.interrupt_waiters.push(respond_to);
                                }
                                SessionCommand::SubmitInput { respond_to, .. }
                                | SessionCommand::ResumeApprovals { respond_to, .. }
                                | SessionCommand::ResumeUserQuestion { respond_to, .. } => {
                                    let _ = respond_to.send(Err(anyhow!("agent session is already running")));
                                }
                            }
                        }
                        completed = &mut active.task => {
                            let InFlightRun { task: _, respond_to, interrupt_waiters, .. } =
                                inflight.take().expect("inflight run should exist");
                            let completion = completed.map_err(|error| anyhow!(error)).and_then(|result| result);
                            match completion {
                                Ok(outcome) => {
                                    last_error = None;
                                    let status = match outcome.status {
                                        kheish_types::RunStatus::Completed => {
                                            if supervisor
                                                .get(&agent_id)
                                                .map(|record| {
                                                    record.retention
                                                        == ChildRetentionPolicy::CloseOnSettle
                                                })
                                                .unwrap_or(false)
                                            {
                                                AgentStatus::Completed
                                            } else {
                                                AgentStatus::Idle
                                            }
                                        }
                                        kheish_types::RunStatus::WaitingForApproval { .. } => AgentStatus::WaitingForApproval,
                                        kheish_types::RunStatus::WaitingForUserQuestion { .. } => AgentStatus::WaitingForUserInput,
                                    };
                                    let settled_at_ms = matches!(status, AgentStatus::Completed)
                                        .then(crate::supervisor::now_ms);
                                    let _ = supervisor.update_record(&agent_id, |agent| {
                                        agent.status = status.clone();
                                        agent.settled_at_ms = settled_at_ms;
                                        if !matches!(status, AgentStatus::Completed) {
                                            agent.closed_at_ms = None;
                                        }
                                    });
                                    let snapshot = {
                                        let runtime = runtime.lock().await;
                                        build_snapshot(&supervisor, &agent_id, &runtime, None)
                                    };
                                    match snapshot {
                                        Ok(snapshot) => {
                                            info!(
                                                agent_id = %agent_id.0,
                                                session_id = %conversation.session_id,
                                                status = ?status,
                                                turns = outcome.turns,
                                                checkpoints_created = outcome.checkpoints_created,
                                                pending_approvals = snapshot.pending_approvals.len(),
                                                pending_questions = snapshot.pending_questions.len(),
                                                "agent runtime settled"
                                            );
                                            cached_snapshot = snapshot.clone();
                                            let _ = respond_to.send(Ok(snapshot.clone()));
                                            for waiter in interrupt_waiters {
                                                let _ = waiter.send(Ok(InterruptResult {
                                                    interrupted: false,
                                                    snapshot: snapshot.clone(),
                                                }));
                                            }
                                        }
                                        Err(error) => {
                                            error!(
                                                agent_id = %agent_id.0,
                                                session_id = %conversation.session_id,
                                                error = %error,
                                                "failed to build managed agent snapshot after run"
                                            );
                                            last_error = Some(error.to_string());
                                            let _ = supervisor.update_record(&agent_id, |agent| {
                                                agent.status = AgentStatus::Failed;
                                                agent.settled_at_ms =
                                                    Some(crate::supervisor::now_ms());
                                                agent.closed_at_ms = None;
                                            });
                                            let _ = respond_to.send(Err(anyhow!(error.to_string())));
                                            for waiter in interrupt_waiters {
                                                let _ = waiter.send(Err(anyhow!(error.to_string())));
                                            }
                                        }
                                    }
                                }
                                Err(error) if is_interrupted_error(&error) => {
                                    info!(
                                        agent_id = %agent_id.0,
                                        session_id = %conversation.session_id,
                                        "agent runtime interrupted"
                                    );
                                    let _ = supervisor.update_record(&agent_id, |agent| {
                                        agent.status = AgentStatus::Idle;
                                        agent.settled_at_ms = None;
                                        agent.closed_at_ms = None;
                                    });
                                    match AgentRuntime::restore(restore.clone(), deps.clone()).await {
                                        Ok(restored) => {
                                            *runtime.lock().await = restored;
                                            last_error = None;
                                            match {
                                                let runtime = runtime.lock().await;
                                                build_snapshot(&supervisor, &agent_id, &runtime, None)
                                            } {
                                                Ok(snapshot) => {
                                                    debug!(
                                                        agent_id = %agent_id.0,
                                                        session_id = %conversation.session_id,
                                                        "restored agent runtime after interrupt"
                                                    );
                                                    cached_snapshot = snapshot.clone();
                                                    let _ = respond_to.send(Err(interrupted_error()));
                                                    for waiter in interrupt_waiters {
                                                        let _ = waiter.send(Ok(InterruptResult {
                                                            interrupted: true,
                                                            snapshot: snapshot.clone(),
                                                        }));
                                                    }
                                                }
                                                Err(snapshot_error) => {
                                                    error!(
                                                        agent_id = %agent_id.0,
                                                        session_id = %conversation.session_id,
                                                        error = %snapshot_error,
                                                        "failed to rebuild snapshot after interrupt restore"
                                                    );
                                                    let _ = respond_to.send(Err(anyhow!(snapshot_error.to_string())));
                                                    for waiter in interrupt_waiters {
                                                        let _ = waiter.send(Err(anyhow!(snapshot_error.to_string())));
                                                    }
                                                }
                                            }
                                        }
                                        Err(restore_error) => {
                                            error!(
                                                agent_id = %agent_id.0,
                                                session_id = %conversation.session_id,
                                                error = %restore_error,
                                                "failed to restore agent runtime after interrupt"
                                            );
                                            last_error = Some(restore_error.to_string());
                                            let _ = supervisor.update_record(&agent_id, |agent| {
                                                agent.status = AgentStatus::Failed;
                                                agent.settled_at_ms =
                                                    Some(crate::supervisor::now_ms());
                                                agent.closed_at_ms = None;
                                            });
                                            let _ = respond_to.send(Err(interrupted_error()));
                                            for waiter in interrupt_waiters {
                                                let _ = waiter.send(Err(anyhow!(restore_error.to_string())));
                                            }
                                        }
                                    }
                                }
                                Err(error) => {
                                    error!(
                                        agent_id = %agent_id.0,
                                        session_id = %conversation.session_id,
                                        error = %error,
                                        "agent runtime failed"
                                    );
                                    last_error = Some(error.to_string());
                                    let _ = supervisor.update_record(&agent_id, |agent| {
                                        agent.status = AgentStatus::Failed;
                                        agent.settled_at_ms =
                                            Some(crate::supervisor::now_ms());
                                        agent.closed_at_ms = None;
                                    });
                                    let snapshot = {
                                        let runtime = runtime.lock().await;
                                        build_snapshot(&supervisor, &agent_id, &runtime, last_error.clone())
                                    };
                                    if let Ok(snapshot) = snapshot {
                                        cached_snapshot = snapshot;
                                    }
                                    let message = error.to_string();
                                    let _ = respond_to.send(Err(anyhow!(message.clone())));
                                    for waiter in interrupt_waiters {
                                        let _ = waiter.send(Ok(InterruptResult {
                                            interrupted: false,
                                            snapshot: cached_snapshot.clone(),
                                        }));
                                    }
                                }
                            }
                        }
                    }
                    continue;
                }

                let Some(command) = receiver.recv().await else {
                    break;
                };
                match command {
                    SessionCommand::SubmitInput {
                        input,
                        generation,
                        run_id,
                        provider,
                        model,
                        respond_to,
                    } => {
                        debug!(
                            agent_id = %agent_id.0,
                            session_id = %conversation.session_id,
                            run_id = run_id.as_deref(),
                            source_plugin = %input.source.plugin,
                            source_kind = %input.source.kind,
                            actor_id = %input.actor.id,
                            provider = provider.as_deref(),
                            model = model.as_deref().or(generation.model.as_deref()),
                            "starting agent runtime input command"
                        );
                        let _ = supervisor.update_record(&agent_id, |agent| {
                            agent.status = AgentStatus::Running;
                            agent.settled_at_ms = None;
                            agent.closed_at_ms = None;
                        });
                        cached_snapshot.agent.status = AgentStatus::Running;
                        cached_snapshot.last_error = None;
                        let cancellation = CancellationToken::new();
                        let run_cancellation = cancellation.clone();
                        let runtime_handle = runtime.clone();
                        let agent_scope = ExecutionScope {
                            session_id: conversation.session_id.clone(),
                            agent_id: Some(agent_id.0.clone()),
                            run_id: run_id.clone(),
                            principal_id: Some(format!("agent:{}", agent_id.0)),
                            parent_principal_id: None,
                            delegation_id: None,
                            grant_id: None,
                            tool_call_id: None,
                            provider,
                            model,
                            credential_scope: Default::default(),
                            workspace_root: scoped_workspace_root.clone(),
                            visible_skills: Vec::new(),
                            visible_mcp_servers: Vec::new(),
                            visible_mcp_tools: Vec::new(),
                        };
                        let task = tokio::spawn(async move {
                            let mut runtime = runtime_handle.lock().await;
                            scope_execution(
                                agent_scope,
                                run_cancellation,
                                runtime.process_input_with_generation_for_run(
                                    input,
                                    generation,
                                    run_id.as_deref(),
                                ),
                            )
                            .await
                        });
                        inflight = Some(InFlightRun {
                            cancellation,
                            task,
                            respond_to,
                            interrupt_waiters: Vec::new(),
                        });
                    }
                    SessionCommand::ResumeApprovals {
                        resolutions,
                        run_id,
                        provider,
                        model,
                        respond_to,
                    } => {
                        debug!(
                            agent_id = %agent_id.0,
                            session_id = %conversation.session_id,
                            run_id = run_id.as_deref(),
                            provider = provider.as_deref(),
                            model = model.as_deref(),
                            approval_count = resolutions.len(),
                            "starting agent runtime approval resume command"
                        );
                        let _ = supervisor.update_record(&agent_id, |agent| {
                            agent.status = AgentStatus::Running;
                            agent.settled_at_ms = None;
                            agent.closed_at_ms = None;
                        });
                        filter_resolved_approvals_from_snapshot(&mut cached_snapshot, &resolutions);
                        cached_snapshot.agent.status =
                            if cached_snapshot.pending_approvals.is_empty() {
                                AgentStatus::Running
                            } else {
                                AgentStatus::WaitingForApproval
                            };
                        cached_snapshot.last_error = None;
                        let cancellation = CancellationToken::new();
                        let run_cancellation = cancellation.clone();
                        let runtime_handle = runtime.clone();
                        let agent_scope = ExecutionScope {
                            session_id: conversation.session_id.clone(),
                            agent_id: Some(agent_id.0.clone()),
                            run_id: run_id.clone(),
                            principal_id: Some(format!("agent:{}", agent_id.0)),
                            parent_principal_id: None,
                            delegation_id: None,
                            grant_id: None,
                            tool_call_id: None,
                            provider,
                            model,
                            credential_scope: Default::default(),
                            workspace_root: scoped_workspace_root.clone(),
                            visible_skills: Vec::new(),
                            visible_mcp_servers: Vec::new(),
                            visible_mcp_tools: Vec::new(),
                        };
                        let task = tokio::spawn(async move {
                            let mut runtime = runtime_handle.lock().await;
                            scope_execution(
                                agent_scope,
                                run_cancellation,
                                runtime.resume_approvals_for_run(&resolutions, run_id.as_deref()),
                            )
                            .await
                        });
                        inflight = Some(InFlightRun {
                            cancellation,
                            task,
                            respond_to,
                            interrupt_waiters: Vec::new(),
                        });
                    }
                    SessionCommand::ResumeUserQuestion {
                        resolution,
                        run_id,
                        provider,
                        model,
                        respond_to,
                    } => {
                        debug!(
                            agent_id = %agent_id.0,
                            session_id = %conversation.session_id,
                            run_id = run_id.as_deref(),
                            provider = provider.as_deref(),
                            model = model.as_deref(),
                            declined = resolution.declined,
                            "starting agent runtime user-question resume command"
                        );
                        let _ = supervisor.update_record(&agent_id, |agent| {
                            agent.status = AgentStatus::Running;
                            agent.settled_at_ms = None;
                            agent.closed_at_ms = None;
                        });
                        cached_snapshot.agent.status = AgentStatus::Running;
                        cached_snapshot.last_error = None;
                        let cancellation = CancellationToken::new();
                        let run_cancellation = cancellation.clone();
                        let runtime_handle = runtime.clone();
                        let agent_scope = ExecutionScope {
                            session_id: conversation.session_id.clone(),
                            agent_id: Some(agent_id.0.clone()),
                            run_id: run_id.clone(),
                            principal_id: Some(format!("agent:{}", agent_id.0)),
                            parent_principal_id: None,
                            delegation_id: None,
                            grant_id: None,
                            tool_call_id: None,
                            provider,
                            model,
                            credential_scope: Default::default(),
                            workspace_root: scoped_workspace_root.clone(),
                            visible_skills: Vec::new(),
                            visible_mcp_servers: Vec::new(),
                            visible_mcp_tools: Vec::new(),
                        };
                        let task = tokio::spawn(async move {
                            let mut runtime = runtime_handle.lock().await;
                            scope_execution(
                                agent_scope,
                                run_cancellation,
                                runtime
                                    .resume_user_question_for_run(&resolution, run_id.as_deref()),
                            )
                            .await
                        });
                        inflight = Some(InFlightRun {
                            cancellation,
                            task,
                            respond_to,
                            interrupt_waiters: Vec::new(),
                        });
                    }
                    SessionCommand::Snapshot { respond_to } => {
                        let snapshot = {
                            let runtime = runtime.lock().await;
                            build_snapshot(&supervisor, &agent_id, &runtime, last_error.clone())
                        };
                        if let Ok(snapshot) = &snapshot {
                            cached_snapshot = snapshot.clone();
                        }
                        let _ = respond_to.send(snapshot);
                    }
                    SessionCommand::Interrupt { respond_to } => {
                        let interrupted = match {
                            let mut runtime = runtime.lock().await;
                            runtime.discard_pending_batch().await
                        } {
                            Ok(interrupted) => interrupted,
                            Err(error) => {
                                error!(
                                    agent_id = %agent_id.0,
                                    session_id = %conversation.session_id,
                                    error = %error,
                                    "failed to discard pending batch during interrupt"
                                );
                                last_error = Some(error.to_string());
                                let _ = supervisor.update_record(&agent_id, |agent| {
                                    agent.status = AgentStatus::Failed;
                                    agent.settled_at_ms = Some(crate::supervisor::now_ms());
                                    agent.closed_at_ms = None;
                                });
                                let _ = respond_to.send(Err(error));
                                continue;
                            }
                        };
                        if interrupted {
                            info!(
                                agent_id = %agent_id.0,
                                session_id = %conversation.session_id,
                                "cleared pending approval batch without active run"
                            );
                            let _ = supervisor.update_record(&agent_id, |agent| {
                                agent.status = AgentStatus::Idle;
                                agent.settled_at_ms = None;
                                agent.closed_at_ms = None;
                            });
                            let snapshot = match {
                                let runtime = runtime.lock().await;
                                build_snapshot(&supervisor, &agent_id, &runtime, None)
                            } {
                                Ok(snapshot) => snapshot,
                                Err(error) => {
                                    error!(
                                        agent_id = %agent_id.0,
                                        session_id = %conversation.session_id,
                                        error = %error,
                                        "failed to rebuild snapshot after clearing pending batch"
                                    );
                                    last_error = Some(error.to_string());
                                    let _ = supervisor.update_record(&agent_id, |agent| {
                                        agent.status = AgentStatus::Failed;
                                        agent.settled_at_ms = Some(crate::supervisor::now_ms());
                                        agent.closed_at_ms = None;
                                    });
                                    let _ = respond_to.send(Err(error));
                                    continue;
                                }
                            };
                            cached_snapshot = snapshot.clone();
                            last_error = None;
                            let _ = respond_to.send(Ok(InterruptResult {
                                interrupted: true,
                                snapshot,
                            }));
                        } else {
                            debug!(
                                agent_id = %agent_id.0,
                                session_id = %conversation.session_id,
                                "interrupt requested but no in-flight or pending work was cleared"
                            );
                            let _ = respond_to.send(Ok(InterruptResult {
                                interrupted: false,
                                snapshot: cached_snapshot.clone(),
                            }));
                        }
                    }
                }
            }
        });
        self.sessions
            .lock()
            .insert(record.id, SessionHandle { sender });
        Ok(())
    }

    /// Returns true when the agent still has an active runtime handle.
    pub fn has_runtime(&self, agent_id: &AgentId) -> bool {
        self.sessions.lock().contains_key(agent_id)
    }

    /// Returns the number of live agent runtime handles currently registered.
    pub fn runtime_count(&self) -> usize {
        self.sessions.lock().len()
    }

    /// Returns the agent identifiers with live runtime handles.
    pub fn runtime_ids(&self) -> BTreeSet<AgentId> {
        self.sessions.lock().keys().cloned().collect()
    }

    /// Closes the runtime handle for one settled agent.
    pub fn close_runtime(&self, agent_id: &AgentId) -> bool {
        let closed = self.sessions.lock().remove(agent_id).is_some();
        if closed {
            info!(agent_id = %agent_id.0, "closed agent runtime handle");
        }
        closed
    }

    fn session_handle(&self, agent_id: &AgentId) -> Result<SessionHandle> {
        self.sessions
            .lock()
            .get(agent_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown agent {}", agent_id.0))
    }
}
