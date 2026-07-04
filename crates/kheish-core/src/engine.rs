use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use kheish_codec::{canonicalize_json_value, digest_json_value, digest_serialize, digest_text};
#[cfg(test)]
use kheish_types::HookPermissionUpdate;
use kheish_types::{
    ApprovalResolution, CanonicalStateSnapshot, CheckpointSnapshot, CheckpointTrace,
    CompactBoundaryMetadata, CompactionBoundary, CompactionStrategy, CompactionTrigger,
    CompletionRequirement, ConversationKey, FinalStateSnapshot, HookDecision, HookDispatchOutcome,
    HookEventName, HookInvocation, HookPermissionBehavior, InputEnvelope, InputPayload, LogEntry,
    MessageRecord, ModelFinishReason, ModelGenerationConfig, ModelProviderError, PendingToolBatch,
    PendingToolDecision, PendingUserQuestion, PermissionDecision, PostCompactRestoration,
    PreservedSegment, PromptMessageSnapshot, PromptProjection, PromptSnapshot, PromptTrace,
    ProviderErrorKind, ProviderInputItem, ProviderPrompt, Role, RunMetaSnapshot, RunPolicySnapshot,
    RunSnapshot, RunStatus, RunTrace, SessionCheckpoint, SessionEvent, SummaryBlock,
    SystemPromptSection, SystemPromptSectionSnapshot, ToolCallRecord, ToolCallSnapshot, ToolChoice,
    ToolDefinition, ToolExecutionTrace, ToolResultRecord, ToolResultSnapshot, TurnSnapshot,
    TurnTrace, UserQuestionRequest, UserQuestionResolution, model_context_window,
    model_max_output_tokens,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::compaction::{
    build_compaction_system_prompt, build_compaction_user_prompt, format_compaction_summary,
};
use crate::hooks::HookDispatcher;
use crate::microcompact::{CLEARED_TOOL_RESULT_MESSAGE, microcompact_tool_results};
use crate::snip::snip_if_needed;
use crate::tokens::{calibrated_prompt_token_count, rough_token_estimate_value};
use crate::user_questions::render_user_question_resolution;
pub use state::{
    AllowAllPermissions, LoopPolicy, ModelDriver, ModelRequest, ModelRequestKind, ModelTurn,
    PermissionGate, PostCompactRestorationProvider, RunOutcome, ToolCatalog, ToolExecutor,
};
use state::{AutocompactTracking, ExecutedBatchOutcome, PendingQuestionExecution, PromptWorkspace};

mod state;

const MAX_COMPACTION_PTL_RETRIES: usize = 3;
const MAX_OUTPUT_TOKENS_RECOVERY_LIMIT: u8 = 3;
const PROMPT_VISIBLE_TOOL_OUTPUT_TOKEN_LIMIT: usize = 10_000;
const PROMPT_VISIBLE_TOOL_OUTPUT_PREVIEW_CHARS: usize = 32_000;

/// Receives newly appended journal entries at turn boundaries so in-flight
/// work survives a crash instead of only becoming durable at end-of-run.
/// Implementations must make the entries durable before returning; a
/// persistence failure fails the run cleanly rather than continuing with
/// unpersisted state.
#[async_trait]
pub trait JournalSink: Send + Sync {
    async fn persist_entries(
        &self,
        conversation: &ConversationKey,
        entries: &[LogEntry],
    ) -> Result<()>;
}

pub struct AgentEngine {
    conversation: ConversationKey,
    policy: LoopPolicy,
    journal: Vec<LogEntry>,
    checkpoints: Vec<SessionCheckpoint>,
    system_sections: Vec<SystemPromptSection>,
    next_offset: u64,
    autocompact_tracking: AutocompactTracking,
    hook_dispatcher: Option<Arc<dyn HookDispatcher>>,
    journal_sink: Option<Arc<dyn JournalSink>>,
    journal_flushed_len: usize,
}

impl std::fmt::Debug for AgentEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentEngine")
            .field("conversation", &self.conversation)
            .field("policy", &self.policy)
            .field("journal", &self.journal)
            .field("checkpoints", &self.checkpoints)
            .field("system_sections", &self.system_sections)
            .field("next_offset", &self.next_offset)
            .field("autocompact_tracking", &self.autocompact_tracking)
            .field(
                "hook_dispatcher",
                &self.hook_dispatcher.as_ref().map(|_| "<dispatcher>"),
            )
            .field(
                "journal_sink",
                &self.journal_sink.as_ref().map(|_| "<sink>"),
            )
            .field("journal_flushed_len", &self.journal_flushed_len)
            .finish()
    }
}

impl AgentEngine {
    /// Creates a new in-memory agent engine for a conversation.
    pub fn new(conversation: ConversationKey, policy: LoopPolicy) -> Self {
        Self {
            conversation,
            policy,
            journal: Vec::new(),
            checkpoints: Vec::new(),
            system_sections: Vec::new(),
            next_offset: 0,
            autocompact_tracking: AutocompactTracking::default(),
            hook_dispatcher: None,
            journal_sink: None,
            journal_flushed_len: 0,
        }
    }

    /// Restores an engine from persisted journal and checkpoint state.
    pub fn restore(
        conversation: ConversationKey,
        policy: LoopPolicy,
        journal: Vec<LogEntry>,
        checkpoints: Vec<SessionCheckpoint>,
    ) -> Self {
        let checkpoints = Self::sanitize_checkpoints(&journal, checkpoints);
        let next_offset = journal
            .last()
            .map(|entry| entry.offset.saturating_add(1))
            .unwrap_or(0);
        // Restored entries came from the store, so they are already durable.
        let journal_flushed_len = journal.len();
        Self {
            conversation,
            policy,
            journal,
            checkpoints,
            system_sections: Vec::new(),
            next_offset,
            autocompact_tracking: AutocompactTracking::default(),
            hook_dispatcher: None,
            journal_sink: None,
            journal_flushed_len,
        }
    }

    /// Returns the next event offset that will be assigned by the engine.
    pub fn next_offset(&self) -> u64 {
        self.next_offset
    }

    pub fn conversation(&self) -> &ConversationKey {
        &self.conversation
    }

    pub fn journal(&self) -> &[LogEntry] {
        &self.journal
    }

    pub fn checkpoints(&self) -> &[SessionCheckpoint] {
        &self.checkpoints
    }

    /// Replaces the provider-neutral system-prompt sections used for subsequent turns.
    pub fn set_system_sections(&mut self, sections: Vec<SystemPromptSection>) {
        self.system_sections = sections;
    }

    /// Returns the currently active provider-neutral system-prompt sections.
    pub fn current_system_sections(&self) -> &[SystemPromptSection] {
        &self.system_sections
    }

    /// Attaches one shared hook dispatcher used for lifecycle interception.
    pub fn set_hook_dispatcher(&mut self, dispatcher: Option<Arc<dyn HookDispatcher>>) {
        self.hook_dispatcher = dispatcher;
    }

    /// Attaches a sink that receives newly appended journal entries at turn
    /// boundaries, making in-flight work durable before the run completes.
    pub fn set_journal_sink(&mut self, sink: Option<Arc<dyn JournalSink>>) {
        self.journal_sink = sink;
    }

    /// Returns how many journal entries have already been made durable, either
    /// by restoration or through the attached [`JournalSink`]. Persist paths
    /// can skip these entries instead of re-writing them.
    pub fn journal_flushed_len(&self) -> usize {
        self.journal_flushed_len
    }

    async fn flush_journal_to_sink(&mut self) -> Result<()> {
        let Some(sink) = self.journal_sink.clone() else {
            return Ok(());
        };
        if self.journal_flushed_len >= self.journal.len() {
            return Ok(());
        }
        sink.persist_entries(
            &self.conversation,
            &self.journal[self.journal_flushed_len..],
        )
        .await
        .context("failed to persist in-flight journal entries")?;
        self.journal_flushed_len = self.journal.len();
        Ok(())
    }

    /// Appends one synthetic message generated by runtime services.
    pub fn inject_message(&mut self, role: Role, content: impl Into<String>) -> u64 {
        let prefix = match role {
            Role::System => "hook-system",
            Role::User => "hook-user",
            Role::Assistant => "hook-assistant",
            Role::Tool => "hook-tool",
            Role::Summary => "hook-summary",
        };
        self.append_message(MessageRecord::new(
            format!("{prefix}-{}", self.next_offset),
            role,
            content.into(),
        ))
    }

    pub fn replay_from_journal(&self) -> CanonicalStateSnapshot {
        Self::replay_log_entries(self.journal.iter())
    }

    pub fn replay_from_latest_checkpoint(&self) -> Option<CanonicalStateSnapshot> {
        let checkpoint = self.checkpoints.last()?;
        let mut snapshot = checkpoint.canonical.clone();
        for entry in self
            .journal
            .iter()
            .filter(|entry| entry.offset > checkpoint.compacted_until_offset)
        {
            Self::apply_log_entry(&mut snapshot, entry);
        }
        Some(snapshot)
    }

    pub fn current_prompt_projection(&self) -> PromptProjection {
        self.build_prompt_workspace().prompt
    }

    /// Returns the active loop policy used by this engine.
    pub fn policy(&self) -> &LoopPolicy {
        &self.policy
    }

    /// Replaces the restoration block on the latest compaction checkpoint.
    pub fn set_latest_checkpoint_restoration(&mut self, restoration: PostCompactRestoration) {
        if let Some(checkpoint) = self.checkpoints.last_mut() {
            checkpoint.restoration = Some(restoration);
        }
    }

    /// Updates the restoration block on the latest compaction checkpoint when one exists.
    pub fn update_latest_checkpoint_restoration(
        &mut self,
        update: impl FnOnce(&mut PostCompactRestoration),
    ) -> bool {
        let Some(restoration) = self
            .checkpoints
            .last_mut()
            .and_then(|checkpoint| checkpoint.restoration.as_mut())
        else {
            return false;
        };
        update(restoration);
        true
    }

    /// Builds the provider-neutral prompt used by model adapters.
    pub fn current_provider_prompt(&self) -> ProviderPrompt {
        self.build_prompt_workspace().provider_prompt
    }

    fn build_prompt_workspace(&self) -> PromptWorkspace {
        let checkpoint = self.checkpoints.last();
        let compacted_until_offset = checkpoint.map(|value| value.compacted_until_offset);
        let provider_window_started_after_offset =
            self.provider_window_started_after_offset(checkpoint);
        let summary = checkpoint.map(|value| value.prompt_summary.clone());
        let mut messages = Vec::new();
        let mut message_offsets = Vec::new();
        let mut input_content_parts =
            std::collections::BTreeMap::<String, Vec<kheish_types::InputContentPart>>::new();
        let mut open_tool_calls: std::collections::BTreeMap<String, ToolCallRecord> = checkpoint
            .map(|value| {
                value
                    .canonical
                    .open_tool_calls
                    .iter()
                    .map(|(id, call)| (id.clone(), Self::without_provider_resume_tool_call(call)))
                    .collect()
            })
            .unwrap_or_default();
        let mut items = Vec::new();
        let mut item_offsets = Vec::new();
        if let Some(summary) = summary.clone() {
            items.push(ProviderInputItem::Summary { summary });
            item_offsets.push(None);
        }

        let mut active_assistant_message_id: Option<String> = None;
        if let Some(checkpoint) = checkpoint {
            for call in checkpoint.canonical.open_tool_calls.values() {
                let call = Self::without_provider_resume_tool_call(call);
                items.push(ProviderInputItem::ToolCall {
                    assistant_message_id: call.assistant_message_id.clone(),
                    call: call.clone(),
                });
                item_offsets.push(None);
                if active_assistant_message_id.is_none() {
                    active_assistant_message_id = call.assistant_message_id.clone();
                }
            }
        }

        for entry in self.journal.iter().filter(|entry| {
            compacted_until_offset
                .map(|offset| entry.offset > offset)
                .unwrap_or(true)
        }) {
            match &entry.event {
                SessionEvent::InputReceived { input } => {
                    let content_parts = Self::input_content_parts(input);
                    if !content_parts.is_empty() {
                        input_content_parts.insert(
                            Self::user_message_id_for_input_offset(entry.offset),
                            content_parts,
                        );
                    }
                }
                SessionEvent::CompactionBoundary { .. }
                | SessionEvent::UserQuestionRequested { .. }
                | SessionEvent::UserQuestionResolved { .. } => {}
                SessionEvent::MessageAppended { message } => {
                    let mut message = Self::prompt_visible_message(message);
                    if !Self::provider_resume_allowed_at_offset(
                        provider_window_started_after_offset,
                        entry.offset,
                    ) {
                        message.provider_response_id = None;
                        message.provider_context = None;
                    }
                    messages.push(message.clone());
                    message_offsets.push(Some(entry.offset));
                    if message.role == Role::Tool {
                        continue;
                    }
                    if message.role == Role::Assistant {
                        active_assistant_message_id = Some(message.id.clone());
                    }
                    let user_content_parts = if matches!(message.role, Role::User) {
                        input_content_parts.remove(&message.id).unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    items.push(ProviderInputItem::Message {
                        id: message.id.clone(),
                        role: message.role.clone(),
                        content: message.content.clone(),
                        content_parts: user_content_parts.clone(),
                        attachments: Self::attachments_from_content_parts(&user_content_parts),
                        provider_response_id: message.provider_response_id.clone(),
                        provider_context: message.provider_context.clone(),
                    });
                    item_offsets.push(Some(entry.offset));
                }
                SessionEvent::ToolCallStarted { call } => {
                    let mut call = call.clone();
                    if !Self::provider_resume_allowed_at_offset(
                        provider_window_started_after_offset,
                        entry.offset,
                    ) {
                        call.assistant_provider_response_id = None;
                    }
                    open_tool_calls.insert(call.id.clone(), call.clone());
                    let assistant_message_id = call
                        .assistant_message_id
                        .clone()
                        .or_else(|| active_assistant_message_id.clone());
                    items.push(ProviderInputItem::ToolCall {
                        assistant_message_id,
                        call: call.clone(),
                    });
                    item_offsets.push(Some(entry.offset));
                }
                SessionEvent::ToolCallFinished { result } => {
                    let result = Self::prompt_visible_tool_result(result);
                    open_tool_calls.remove(&result.call_id);
                    items.push(ProviderInputItem::ToolResult { result });
                    item_offsets.push(Some(entry.offset));
                }
            }
        }

        let restoration = checkpoint.and_then(|value| value.restoration.clone());
        if let Some(restoration) = restoration.clone() {
            items.push(ProviderInputItem::Restoration { restoration });
            item_offsets.push(None);
        }

        PromptWorkspace {
            prompt: PromptProjection {
                summary,
                system_sections: self.system_sections.clone(),
                messages,
                open_tool_calls: open_tool_calls.into_values().collect(),
                restoration,
            },
            provider_prompt: ProviderPrompt {
                instructions: self
                    .system_sections
                    .iter()
                    .map(|section| section.content.clone())
                    .collect(),
                force_synthetic_user_prefix: false,
                items,
            },
            message_offsets,
            item_offsets,
        }
    }

    pub async fn run_input<M, T>(
        &mut self,
        input: InputEnvelope,
        model: &M,
        tools: &T,
    ) -> Result<RunOutcome>
    where
        M: ModelDriver + ?Sized,
        T: ToolExecutor + ?Sized,
    {
        let permissions = AllowAllPermissions;
        self.run_input_with_permissions(input, model, tools, &permissions)
            .await
    }

    pub async fn run_input_with_permissions<M, T, P>(
        &mut self,
        input: InputEnvelope,
        model: &M,
        tools: &T,
        permissions: &P,
    ) -> Result<RunOutcome>
    where
        M: ModelDriver + ?Sized,
        T: ToolExecutor + ?Sized,
        P: PermissionGate + ?Sized,
    {
        self.run_input_with_generation_and_restoration(
            input,
            ModelGenerationConfig::default(),
            model,
            tools,
            permissions,
            None,
        )
        .await
    }

    /// Runs one normalized input with explicit generation settings.
    pub async fn run_input_with_generation<M, T, P>(
        &mut self,
        input: InputEnvelope,
        generation: ModelGenerationConfig,
        model: &M,
        tools: &T,
        permissions: &P,
    ) -> Result<RunOutcome>
    where
        M: ModelDriver + ?Sized,
        T: ToolExecutor + ?Sized,
        P: PermissionGate + ?Sized,
    {
        self.run_input_with_generation_and_restoration(
            input,
            generation,
            model,
            tools,
            permissions,
            None,
        )
        .await
    }

    /// Runs one normalized input with explicit generation settings and compaction restoration.
    pub async fn run_input_with_generation_and_restoration<M, T, P>(
        &mut self,
        input: InputEnvelope,
        generation: ModelGenerationConfig,
        model: &M,
        tools: &T,
        permissions: &P,
        restoration: Option<&dyn PostCompactRestorationProvider>,
    ) -> Result<RunOutcome>
    where
        M: ModelDriver + ?Sized,
        T: ToolExecutor + ?Sized,
        P: PermissionGate + ?Sized,
    {
        let input_offset = self.accept_input(input.clone())?;
        let run_meta =
            Self::build_run_meta(&self.conversation, &self.policy, &input, input_offset)?;
        self.drive_loop(
            1,
            run_meta,
            generation,
            model,
            tools,
            permissions,
            restoration,
        )
        .await
    }

    /// Resumes a run after one or more pending approvals have been resolved.
    pub async fn resume_with_pending_batch<M, T>(
        &mut self,
        run_meta: RunMetaSnapshot,
        generation: ModelGenerationConfig,
        pending_batch: PendingToolBatch,
        resolutions: &[ApprovalResolution],
        model: &M,
        tools: &T,
        permissions: &impl PermissionGate,
    ) -> Result<RunOutcome>
    where
        M: ModelDriver + ?Sized,
        T: ToolExecutor + ?Sized,
    {
        self.resume_with_pending_batch_and_restoration(
            run_meta,
            generation,
            pending_batch,
            resolutions,
            model,
            tools,
            permissions,
            None,
        )
        .await
    }

    /// Resumes a pending batch and reuses the provided restoration provider.
    pub async fn resume_with_pending_batch_and_restoration<M, T>(
        &mut self,
        run_meta: RunMetaSnapshot,
        generation: ModelGenerationConfig,
        pending_batch: PendingToolBatch,
        resolutions: &[ApprovalResolution],
        model: &M,
        tools: &T,
        permissions: &impl PermissionGate,
        restoration: Option<&dyn PostCompactRestorationProvider>,
    ) -> Result<RunOutcome>
    where
        M: ModelDriver + ?Sized,
        T: ToolExecutor + ?Sized,
    {
        let finalized = crate::apply_approval_resolutions(pending_batch, resolutions)?;
        if let Some(waiting) = Self::collect_waiting_requests(&finalized) {
            return self.build_waiting_outcome(
                finalized.turn,
                finalized.assistant_message_id.clone(),
                run_meta,
                RunTrace::default(),
                Vec::new(),
                Vec::new(),
                Some(finalized),
                waiting,
            );
        }

        let executed = self
            .execute_finalized_batch(tools, &finalized.decisions)
            .await?;
        let mut trace = RunTrace::default();
        let snapshot_turn =
            self.build_turn_snapshot_for_executed_batch(&finalized, &executed.tool_executions)?;
        trace.turns.push(TurnTrace {
            turn: finalized.turn,
            prompt: PromptTrace {
                has_summary: self.checkpoints.last().is_some(),
                system_section_count: self.system_sections.len(),
                message_count: self.current_prompt_projection().messages.len(),
                open_tool_call_count: self.current_prompt_projection().open_tool_calls.len(),
            },
            assistant_message_id: finalized.assistant_message_id.clone(),
            tool_executions: executed.tool_executions,
        });
        self.drive_loop(
            finalized.turn.saturating_add(1),
            run_meta,
            generation,
            model,
            tools,
            permissions,
            restoration,
        )
        .await
        .map(|mut outcome| {
            outcome.trace.turns.insert(0, trace.turns.remove(0));
            outcome.snapshot.turns.insert(0, snapshot_turn);
            outcome.turns = outcome.turns.max(finalized.turn);
            outcome
        })
    }

    /// Resumes a run after the user answered one pending structured question request.
    pub async fn resume_with_pending_user_question_and_restoration<M, T>(
        &mut self,
        run_meta: RunMetaSnapshot,
        generation: ModelGenerationConfig,
        pending_question: PendingUserQuestion,
        resolution: &UserQuestionResolution,
        model: &M,
        _tools: &T,
        permissions: &impl PermissionGate,
        restoration: Option<&dyn PostCompactRestorationProvider>,
    ) -> Result<RunOutcome>
    where
        M: ModelDriver + ?Sized,
        T: ToolExecutor + ?Sized,
    {
        let result = self.user_question_tool_result(&pending_question, resolution)?;
        self.record_event(SessionEvent::UserQuestionResolved {
            resolution: resolution.clone(),
        });
        self.finish_tool_call(result.clone());
        self.append_tool_result_message(&result)?;
        let execution = ToolExecutionTrace {
            call_id: pending_question.call.id.clone(),
            tool_name: pending_question.call.name.clone(),
            decision: PermissionDecision::Allow,
            result_is_error: false,
        };
        let mut trace = RunTrace::default();
        let snapshot_turn = self.build_tool_interaction_snapshot(
            pending_question.turn,
            &pending_question.assistant_message_id,
            &pending_question.call,
            vec![execution.clone()],
        )?;
        trace.turns.push(TurnTrace {
            turn: pending_question.turn,
            prompt: PromptTrace {
                has_summary: self.checkpoints.last().is_some(),
                system_section_count: self.system_sections.len(),
                message_count: self.current_prompt_projection().messages.len(),
                open_tool_call_count: self.current_prompt_projection().open_tool_calls.len(),
            },
            assistant_message_id: pending_question.assistant_message_id.clone(),
            tool_executions: vec![execution],
        });
        self.drive_loop(
            pending_question.turn.saturating_add(1),
            run_meta,
            generation,
            model,
            _tools,
            permissions,
            restoration,
        )
        .await
        .map(|mut outcome| {
            outcome.trace.turns.insert(0, trace.turns.remove(0));
            outcome.snapshot.turns.insert(0, snapshot_turn);
            outcome.turns = outcome.turns.max(pending_question.turn);
            outcome
        })
    }

    async fn drive_loop<M, T, P>(
        &mut self,
        start_turn: usize,
        mut run_meta: RunMetaSnapshot,
        generation: ModelGenerationConfig,
        model: &M,
        tools: &T,
        permissions: &P,
        restoration: Option<&dyn PostCompactRestorationProvider>,
    ) -> Result<RunOutcome>
    where
        M: ModelDriver + ?Sized,
        T: ToolExecutor + ?Sized,
        P: PermissionGate + ?Sized,
    {
        let mut trace = RunTrace::default();
        let mut snapshot_turns = Vec::new();
        let mut snapshot_checkpoints = Vec::new();
        let mut pending_generation = generation.clone();
        // The latest assistant answer text, tracked only under an output
        // contract: the completion boundary validates it against the schema.
        let mut last_assistant_text: Option<String> = None;
        self.autocompact_tracking = AutocompactTracking {
            consecutive_failures: run_meta.autocompact.consecutive_failures,
            last_compacted_turn: run_meta.autocompact.last_compacted_turn,
            turn_counter: run_meta.autocompact.turn_counter,
        };

        let turns: Box<dyn Iterator<Item = usize> + Send> = if self.policy.max_turns == 0 {
            Box::new(std::iter::successors(Some(start_turn), |turn| {
                turn.checked_add(1)
            }))
        } else {
            Box::new(start_turn..=self.policy.max_turns)
        };

        for turn in turns {
            // Durability boundary: the accepted input (first turn) and the
            // previous turn's tail become durable before more model work
            // builds on them.
            self.flush_journal_to_sink().await?;
            self.autocompact_tracking.turn_counter =
                self.autocompact_tracking.turn_counter.saturating_add(1);
            run_meta.autocompact = self.snapshot_autocompact_tracking();

            let mut workspace = self.build_prompt_workspace();
            let pipeline_start_offset = self.next_offset;
            if let Some(checkpoint) = self
                .compact_pipeline(
                    &mut workspace,
                    turn,
                    &pending_generation,
                    model,
                    restoration,
                )
                .await?
            {
                trace.checkpoints.push(CheckpointTrace {
                    compacted_until_offset: checkpoint.compacted_until_offset,
                    journal_digest: checkpoint.journal_digest.clone(),
                    summary_chars: checkpoint.summary_text.len(),
                });
                snapshot_checkpoints.push(checkpoint);
                workspace = self.build_prompt_workspace();
            }
            trace
                .compaction_boundaries
                .extend(self.collect_compaction_boundaries_since(pipeline_start_offset));
            run_meta.autocompact = self.snapshot_autocompact_tracking();

            let prompt_snapshot = self.build_prompt_snapshot(&workspace.prompt)?;
            let prompt_trace = PromptTrace {
                has_summary: workspace.prompt.summary.is_some(),
                system_section_count: workspace.prompt.system_sections.len(),
                message_count: workspace.prompt.messages.len(),
                open_tool_call_count: workspace.prompt.open_tool_calls.len(),
            };
            let request_generation = pending_generation.clone();
            let available_tools = tools.definitions();
            let mut request = ModelRequest {
                kind: ModelRequestKind::MainLoop,
                conversation: self.conversation.clone(),
                turn,
                prompt: workspace.prompt.clone(),
                provider_prompt: workspace.provider_prompt.clone(),
                available_tools: available_tools.clone(),
                generation: request_generation.clone(),
            };
            pending_generation = generation.clone();
            let mut reactive_compacted = false;
            let mut aggressively_snipped = false;
            let mut fallback_model_applied = false;
            let model_turn = loop {
                match model.next_turn(request.clone()).await {
                    Ok(model_turn) => break model_turn,
                    Err(error) if Self::is_prompt_too_long_error(&error) => {
                        if !aggressively_snipped {
                            let snip_start_offset = self.next_offset;
                            if let Some(rebuilt) =
                                self.aggressive_snip_workspace(turn, &request.generation)
                            {
                                trace.compaction_boundaries.extend(
                                    self.collect_compaction_boundaries_since(snip_start_offset),
                                );
                                request.prompt = rebuilt.prompt.clone();
                                request.provider_prompt = rebuilt.provider_prompt.clone();
                                aggressively_snipped = true;
                                continue;
                            }
                        }
                        if !reactive_compacted {
                            let reactive_start_offset = self.next_offset;
                            if let Some(checkpoint) = self
                                .force_reactive_compaction(turn, model, restoration)
                                .await?
                            {
                                trace.checkpoints.push(CheckpointTrace {
                                    compacted_until_offset: checkpoint.compacted_until_offset,
                                    journal_digest: checkpoint.journal_digest.clone(),
                                    summary_chars: checkpoint.summary_text.len(),
                                });
                                snapshot_checkpoints.push(checkpoint);
                                trace.compaction_boundaries.extend(
                                    self.collect_compaction_boundaries_since(reactive_start_offset),
                                );
                                run_meta.autocompact = self.snapshot_autocompact_tracking();
                                let rebuilt = self.build_prompt_workspace();
                                request.prompt = rebuilt.prompt.clone();
                                request.provider_prompt = rebuilt.provider_prompt.clone();
                                reactive_compacted = true;
                                continue;
                            }
                        }
                        if !fallback_model_applied {
                            if let Some(fallback_generation) =
                                Self::fallback_model_generation(&generation, &request_generation)
                            {
                                pending_generation = fallback_generation.clone();
                                request.generation = fallback_generation;
                                fallback_model_applied = true;
                                continue;
                            }
                        }
                        return Err(error);
                    }
                    Err(error)
                        if !fallback_model_applied
                            && (Self::is_rate_limit_error(&error)
                                || Self::is_provider_unavailable_error(&error)) =>
                    {
                        if let Some(fallback_generation) =
                            Self::fallback_model_generation(&generation, &request_generation)
                        {
                            pending_generation = fallback_generation.clone();
                            request.generation = fallback_generation;
                            fallback_model_applied = true;
                            continue;
                        }
                        return Err(error);
                    }
                    Err(error) => return Err(error),
                }
            };
            let final_message_id = model_turn.assistant_message.id.clone();
            let assistant_message_digest = Self::digest_of(&model_turn.assistant_message)?;
            let stop_reason = model_turn.finish_reason.as_str().to_string();
            let assistant_usage = model_turn.usage.clone();
            let model_tool_calls = model_turn
                .tool_calls
                .into_iter()
                .map(|mut call| {
                    if call.assistant_message_id.is_none() {
                        call.assistant_message_id = Some(final_message_id.clone());
                    }
                    if call.assistant_provider_response_id.is_none() {
                        call.assistant_provider_response_id =
                            model_turn.assistant_message.provider_response_id.clone();
                    }
                    call
                })
                .collect::<Vec<_>>();

            let mut assistant_message = model_turn.assistant_message;
            if assistant_message.api_usage.is_none() {
                assistant_message.api_usage = assistant_usage.clone();
            }
            if run_meta.output_contract.is_some() {
                last_assistant_text = Some(assistant_message.content.clone());
            }
            self.append_message(assistant_message);

            let tool_call_snapshots = model_tool_calls
                .iter()
                .map(|call| {
                    self.start_tool_call(call.clone());
                    Ok(ToolCallSnapshot {
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        input_digest: digest_json_value(&call.input)?,
                        input_canonical_json: canonicalize_json_value(&call.input),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let pending_decisions = self
                .evaluate_tool_permissions(permissions, &model_tool_calls)
                .await?;
            for decision in &pending_decisions {
                if !decision.hook_contexts.is_empty() {
                    self.inject_hook_contexts(
                        HookEventName::PermissionRequest,
                        &decision.hook_contexts,
                    );
                }
            }

            if let Some(waiting) = Self::collect_waiting_requests_from_decisions(&pending_decisions)
            {
                trace.turns.push(TurnTrace {
                    turn,
                    prompt: prompt_trace,
                    assistant_message_id: final_message_id.clone(),
                    tool_executions: Vec::new(),
                });
                snapshot_turns.push(TurnSnapshot {
                    turn,
                    prompt: prompt_snapshot,
                    assistant_message_id: final_message_id.clone(),
                    assistant_message_digest,
                    stop_reason,
                    usage: assistant_usage,
                    tool_calls: tool_call_snapshots,
                    tool_results: Vec::new(),
                });
                Self::sync_run_meta_autocompact(&mut run_meta, &self.autocompact_tracking);
                return self.build_waiting_outcome(
                    turn,
                    final_message_id,
                    run_meta,
                    trace,
                    snapshot_turns,
                    snapshot_checkpoints,
                    Some(PendingToolBatch {
                        turn,
                        assistant_message_id: self
                            .current_prompt_projection()
                            .messages
                            .last()
                            .map(|message| message.id.clone())
                            .unwrap_or_else(|| format!("assistant-turn-{turn}")),
                        decisions: pending_decisions,
                    }),
                    waiting,
                );
            }

            // Durability boundary: the assistant message and every
            // ToolCallStarted entry hit disk before tools execute, so a crash
            // mid-tool leaves a record of what was in flight.
            self.flush_journal_to_sink().await?;
            let executed_batch = self
                .execute_finalized_batch(tools, &pending_decisions)
                .await?;
            // Durability boundary: tool results are durable before the next
            // model call consumes them.
            self.flush_journal_to_sink().await?;
            if let Some(pending_question) = executed_batch.pending_question {
                trace.turns.push(TurnTrace {
                    turn,
                    prompt: prompt_trace,
                    assistant_message_id: final_message_id.clone(),
                    tool_executions: Vec::new(),
                });
                snapshot_turns.push(TurnSnapshot {
                    turn,
                    prompt: prompt_snapshot,
                    assistant_message_id: final_message_id.clone(),
                    assistant_message_digest,
                    stop_reason,
                    usage: assistant_usage,
                    tool_calls: tool_call_snapshots,
                    tool_results: Vec::new(),
                });
                Self::sync_run_meta_autocompact(&mut run_meta, &self.autocompact_tracking);
                return self.build_waiting_question_outcome(
                    turn,
                    final_message_id,
                    run_meta,
                    trace,
                    snapshot_turns,
                    snapshot_checkpoints,
                    PendingUserQuestion {
                        turn,
                        assistant_message_id: self
                            .current_prompt_projection()
                            .messages
                            .last()
                            .map(|message| message.id.clone())
                            .unwrap_or_else(|| format!("assistant-turn-{turn}")),
                        call: pending_question.call,
                        request: pending_question.request.clone(),
                    },
                    vec![pending_question.request],
                );
            }
            let tool_executions = executed_batch.tool_executions;
            if tool_executions
                .iter()
                .any(|execution| !execution.result_is_error)
            {
                pending_generation = Self::post_tool_execution_generation(&request_generation);
            }
            let tool_result_snapshots = tool_executions
                .iter()
                .map(|execution| self.build_tool_result_snapshot(execution))
                .collect::<Result<Vec<_>>>()?;

            trace.turns.push(TurnTrace {
                turn,
                prompt: prompt_trace,
                assistant_message_id: final_message_id.clone(),
                tool_executions: tool_executions.clone(),
            });
            snapshot_turns.push(TurnSnapshot {
                turn,
                prompt: prompt_snapshot,
                assistant_message_id: final_message_id.clone(),
                assistant_message_digest,
                stop_reason,
                usage: assistant_usage,
                tool_calls: tool_call_snapshots,
                tool_results: tool_result_snapshots,
            });

            let denied_retries = pending_decisions
                .iter()
                .filter(|decision| {
                    decision.retry && matches!(decision.decision, PermissionDecision::Deny { .. })
                })
                .map(|decision| decision.call.name.clone())
                .collect::<Vec<_>>();
            if !denied_retries.is_empty() {
                if run_meta.permission_denied_retry_count >= 2 {
                    bail!(
                        "permission denial retries exhausted for tools: {}",
                        denied_retries.join(", ")
                    );
                }
                self.append_permission_denied_retry_message(turn, &denied_retries);
                run_meta.permission_denied_retry_count =
                    run_meta.permission_denied_retry_count.saturating_add(1);
                pending_generation = request_generation.clone();
                continue;
            }

            if tool_executions.is_empty()
                && matches!(model_turn.finish_reason, ModelFinishReason::MaxTokens)
            {
                if Self::should_escalate_max_output_tokens(&generation, &request_generation) {
                    pending_generation =
                        Self::max_tokens_recovery_generation(&generation, &request_generation);
                    continue;
                }
                if run_meta.max_output_tokens_recovery_count >= MAX_OUTPUT_TOKENS_RECOVERY_LIMIT {
                    bail!("model output exceeded max tokens after recovery");
                }
                self.append_max_tokens_recovery_message(turn);
                run_meta.max_output_tokens_recovery_count =
                    run_meta.max_output_tokens_recovery_count.saturating_add(1);
                pending_generation =
                    Self::max_tokens_recovery_generation(&generation, &request_generation);
                continue;
            }

            if tool_executions.is_empty() {
                let unsatisfied = self.unsatisfied_completion_requirements(&run_meta)?;
                if !unsatisfied.is_empty() {
                    if run_meta.completion_follow_up_count >= 2 {
                        bail!(
                            "run ended without satisfying completion requirements: {}",
                            Self::completion_requirement_summary(&unsatisfied)
                        );
                    }
                    self.append_completion_follow_up_message(turn, &unsatisfied);
                    run_meta.completion_follow_up_count =
                        run_meta.completion_follow_up_count.saturating_add(1);
                    pending_generation = Self::completion_follow_up_generation(
                        &generation,
                        &unsatisfied,
                        &available_tools,
                    );
                    continue;
                }
                let mut structured_output = None;
                if let Some(contract) = run_meta.output_contract.clone() {
                    let answer = last_assistant_text.clone().unwrap_or_default();
                    match Self::contract_conformant_output(&contract, &answer) {
                        Ok(value) => structured_output = Some(value),
                        Err(error) => {
                            if run_meta.output_contract_repair_count
                                >= contract.effective_max_repair_attempts()
                            {
                                bail!(
                                    "structured output contract unsatisfied after {} repair attempts: {error}",
                                    run_meta.output_contract_repair_count
                                );
                            }
                            self.append_output_contract_repair_message(
                                turn,
                                &error,
                                &contract.schema,
                            );
                            run_meta.output_contract_repair_count =
                                run_meta.output_contract_repair_count.saturating_add(1);
                            pending_generation =
                                Self::output_contract_repair_generation(&generation);
                            continue;
                        }
                    }
                }
                Self::sync_run_meta_autocompact(&mut run_meta, &self.autocompact_tracking);
                return self.build_completed_outcome(
                    turn,
                    final_message_id,
                    run_meta,
                    trace,
                    snapshot_turns,
                    snapshot_checkpoints,
                    structured_output,
                );
            }
        }

        if self.policy.max_turns == 0 {
            bail!("agent loop exhausted the usize turn counter");
        }
        bail!("agent loop exceeded max_turns={}", self.policy.max_turns);
    }

    async fn evaluate_tool_permissions<P>(
        &self,
        permissions: &P,
        calls: &[ToolCallRecord],
    ) -> Result<Vec<PendingToolDecision>>
    where
        P: PermissionGate + ?Sized,
    {
        let mut decisions = Vec::with_capacity(calls.len());
        for call in calls {
            let base_decision = permissions.check(call).await?;
            let mut effective_call = call.clone();
            let mut decision = base_decision.clone();
            let mut hook_contexts = Vec::new();
            let mut retry = false;

            if let PermissionDecision::Ask { request } = &base_decision {
                let hook_outcome = self
                    .dispatch_hook(
                        HookEventName::PermissionRequest,
                        Some(call.name.clone()),
                        json!({
                            "tool_call": call,
                            "approval_request": request,
                        }),
                    )
                    .await?;
                hook_contexts.extend(hook_outcome.additional_contexts.clone());
                if let Some(updated_input) = hook_outcome.updated_input {
                    effective_call.input = updated_input;
                }
                if matches!(hook_outcome.decision, Some(HookDecision::Block))
                    || matches!(hook_outcome.permission, Some(HookPermissionBehavior::Deny))
                    || !hook_outcome.continue_execution
                {
                    decision = PermissionDecision::Deny {
                        reason: hook_outcome
                            .stop_reason
                            .or_else(|| Some("permission denied by hook".to_string()))
                            .unwrap_or_else(|| "permission denied by hook".to_string()),
                    };
                } else if matches!(hook_outcome.permission, Some(HookPermissionBehavior::Ask)) {
                    decision = base_decision.clone();
                } else if matches!(hook_outcome.permission, Some(HookPermissionBehavior::Allow))
                    || matches!(hook_outcome.decision, Some(HookDecision::Approve))
                {
                    if !hook_outcome.updated_permissions.is_empty() {
                        permissions
                            .apply_hook_permission_updates(
                                &self.conversation.session_id,
                                &hook_outcome.updated_permissions,
                            )
                            .await?;
                    }
                    decision = PermissionDecision::Allow;
                }
            }

            if let PermissionDecision::Deny { reason } = &decision {
                let hook_outcome = self
                    .dispatch_hook(
                        HookEventName::PermissionDenied,
                        Some(effective_call.name.clone()),
                        json!({
                            "tool_call": effective_call,
                            "reason": reason,
                        }),
                    )
                    .await?;
                retry = hook_outcome.retry;
                hook_contexts.extend(hook_outcome.additional_contexts);
            }

            let final_reason = match &decision {
                PermissionDecision::Allow
                    if matches!(base_decision, PermissionDecision::Ask { .. }) =>
                {
                    Some("allowed by permission hook".to_string())
                }
                PermissionDecision::Deny { reason } => Some(reason.clone()),
                _ => None,
            };
            permissions
                .record_final_decision(&effective_call, &decision, final_reason)
                .await?;

            decisions.push(PendingToolDecision {
                call: effective_call,
                decision,
                hook_contexts,
                retry,
            });
        }
        Ok(decisions)
    }

    fn collect_waiting_requests_from_decisions(
        decisions: &[PendingToolDecision],
    ) -> Option<Vec<kheish_types::ApprovalRequest>> {
        let requests = crate::pending_approval_requests_from_decisions(decisions);
        if requests.is_empty() {
            None
        } else {
            Some(requests)
        }
    }

    fn collect_waiting_requests(
        batch: &PendingToolBatch,
    ) -> Option<Vec<kheish_types::ApprovalRequest>> {
        Self::collect_waiting_requests_from_decisions(&batch.decisions)
    }

    async fn execute_finalized_batch<T>(
        &mut self,
        tools: &T,
        decisions: &[PendingToolDecision],
    ) -> Result<ExecutedBatchOutcome>
    where
        T: ToolExecutor + ?Sized,
    {
        let allowed_calls = decisions
            .iter()
            .filter_map(|decision| {
                matches!(decision.decision, PermissionDecision::Allow)
                    .then_some(decision.call.clone())
            })
            .collect::<Vec<_>>();
        let allowed_results = tools.execute_batch(&allowed_calls).await?;
        let mut allowed_by_call_id = allowed_results
            .into_iter()
            .map(|result| (result.call_id.clone(), result))
            .collect::<std::collections::BTreeMap<_, _>>();

        let mut executions = Vec::with_capacity(decisions.len());
        let mut pending_question = None;
        for decision in decisions {
            let result = match &decision.decision {
                PermissionDecision::Allow => {
                    let Some(result) = allowed_by_call_id.remove(&decision.call.id) else {
                        bail!("missing tool result for call {}", decision.call.id);
                    };
                    if result.call_id != decision.call.id {
                        bail!(
                            "tool result call_id mismatch: expected {}, got {}",
                            decision.call.id,
                            result.call_id
                        );
                    }
                    let mut result = result;
                    if result.tool_name.is_none() {
                        result.tool_name = Some(decision.call.name.clone());
                    }
                    result
                }
                PermissionDecision::Deny { reason } => Self::denied_tool_result(
                    &decision.call,
                    reason.clone(),
                    decision.hook_contexts.clone(),
                ),
                PermissionDecision::Ask { request } => bail!(
                    "cannot execute tool batch while approval {} is still pending",
                    request.id
                ),
            };
            if let Some(request) = Self::extract_pending_user_question(&result)? {
                if pending_question.is_some() || decisions.len() != 1 {
                    bail!(
                        "ask_user_question or ask_operator must be the only tool call in its turn"
                    );
                }
                self.record_event(SessionEvent::UserQuestionRequested {
                    request: request.clone(),
                });
                pending_question = Some(PendingQuestionExecution {
                    call: decision.call.clone(),
                    request,
                });
                continue;
            }
            self.finish_tool_call(result.clone());
            self.append_tool_result_message(&result)?;
            if !result.hook_contexts.is_empty() {
                self.inject_hook_contexts(HookEventName::PostToolUse, &result.hook_contexts);
            }
            self.emit_file_changed_hook_contexts(&result).await?;
            executions.push(ToolExecutionTrace {
                call_id: decision.call.id.clone(),
                tool_name: decision.call.name.clone(),
                decision: decision.decision.clone(),
                result_is_error: result.is_error,
            });
        }
        Ok(ExecutedBatchOutcome {
            tool_executions: executions,
            pending_question,
        })
    }

    fn build_tool_result_snapshot(
        &self,
        execution: &ToolExecutionTrace,
    ) -> Result<ToolResultSnapshot> {
        let result = self
            .replay_from_journal()
            .completed_tool_results
            .into_iter()
            .find(|result| result.call_id == execution.call_id)
            .ok_or_else(|| anyhow!("missing completed tool result for {}", execution.call_id))?;
        Ok(ToolResultSnapshot {
            call_id: result.call_id.clone(),
            decision: execution.decision.clone(),
            result_is_error: result.is_error,
            result_digest: digest_json_value(&result.output)?,
            result_canonical_json: canonicalize_json_value(&result.output),
        })
    }

    fn build_turn_snapshot_for_executed_batch(
        &self,
        batch: &PendingToolBatch,
        executions: &[ToolExecutionTrace],
    ) -> Result<TurnSnapshot> {
        Ok(TurnSnapshot {
            turn: batch.turn,
            prompt: self.build_prompt_snapshot(&self.current_prompt_projection())?,
            assistant_message_id: batch.assistant_message_id.clone(),
            assistant_message_digest: String::new(),
            stop_reason: ModelFinishReason::ToolCalls.as_str().to_string(),
            usage: None,
            tool_calls: batch
                .decisions
                .iter()
                .map(|decision| {
                    Ok(ToolCallSnapshot {
                        call_id: decision.call.id.clone(),
                        tool_name: decision.call.name.clone(),
                        input_digest: digest_json_value(&decision.call.input)?,
                        input_canonical_json: canonicalize_json_value(&decision.call.input),
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            tool_results: executions
                .iter()
                .map(|execution| self.build_tool_result_snapshot(execution))
                .collect::<Result<Vec<_>>>()?,
        })
    }

    fn build_tool_interaction_snapshot(
        &self,
        turn: usize,
        assistant_message_id: &str,
        call: &ToolCallRecord,
        executions: Vec<ToolExecutionTrace>,
    ) -> Result<TurnSnapshot> {
        Ok(TurnSnapshot {
            turn,
            prompt: self.build_prompt_snapshot(&self.current_prompt_projection())?,
            assistant_message_id: assistant_message_id.to_string(),
            assistant_message_digest: String::new(),
            stop_reason: ModelFinishReason::ToolCalls.as_str().to_string(),
            usage: None,
            tool_calls: vec![ToolCallSnapshot {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                input_digest: digest_json_value(&call.input)?,
                input_canonical_json: canonicalize_json_value(&call.input),
            }],
            tool_results: executions
                .iter()
                .map(|execution| self.build_tool_result_snapshot(execution))
                .collect::<Result<Vec<_>>>()?,
        })
    }

    fn build_completed_outcome(
        &self,
        turns: usize,
        final_message_id: String,
        run_meta: RunMetaSnapshot,
        trace: RunTrace,
        snapshot_turns: Vec<TurnSnapshot>,
        snapshot_checkpoints: Vec<CheckpointSnapshot>,
        structured_output: Option<serde_json::Value>,
    ) -> Result<RunOutcome> {
        let final_state = self.build_final_state_snapshot()?;
        Ok(RunOutcome {
            turns,
            final_message_id,
            checkpoints_created: trace.checkpoints.len(),
            status: RunStatus::Completed,
            pending_batch: None,
            pending_question: None,
            structured_output,
            trace: trace.clone(),
            snapshot: RunSnapshot {
                run_meta,
                turns: snapshot_turns,
                checkpoints: snapshot_checkpoints,
                final_state,
            },
        })
    }

    fn build_waiting_outcome(
        &self,
        turns: usize,
        final_message_id: String,
        run_meta: RunMetaSnapshot,
        trace: RunTrace,
        snapshot_turns: Vec<TurnSnapshot>,
        snapshot_checkpoints: Vec<CheckpointSnapshot>,
        pending_batch: Option<PendingToolBatch>,
        requests: Vec<kheish_types::ApprovalRequest>,
    ) -> Result<RunOutcome> {
        let final_state = self.build_final_state_snapshot()?;
        Ok(RunOutcome {
            turns,
            final_message_id,
            checkpoints_created: trace.checkpoints.len(),
            status: RunStatus::WaitingForApproval { requests },
            pending_batch,
            pending_question: None,
            structured_output: None,
            trace: trace.clone(),
            snapshot: RunSnapshot {
                run_meta,
                turns: snapshot_turns,
                checkpoints: snapshot_checkpoints,
                final_state,
            },
        })
    }

    fn build_waiting_question_outcome(
        &self,
        turns: usize,
        final_message_id: String,
        run_meta: RunMetaSnapshot,
        trace: RunTrace,
        snapshot_turns: Vec<TurnSnapshot>,
        snapshot_checkpoints: Vec<CheckpointSnapshot>,
        pending_question: PendingUserQuestion,
        requests: Vec<UserQuestionRequest>,
    ) -> Result<RunOutcome> {
        let final_state = self.build_final_state_snapshot()?;
        Ok(RunOutcome {
            turns,
            final_message_id,
            checkpoints_created: trace.checkpoints.len(),
            status: RunStatus::WaitingForUserQuestion { requests },
            pending_batch: None,
            pending_question: Some(pending_question),
            structured_output: None,
            trace: trace.clone(),
            snapshot: RunSnapshot {
                run_meta,
                turns: snapshot_turns,
                checkpoints: snapshot_checkpoints,
                final_state,
            },
        })
    }

    fn accept_input(&mut self, input: InputEnvelope) -> Result<u64> {
        let mut persisted_input = input.clone();
        if let serde_json::Value::Object(metadata) = &mut persisted_input.metadata {
            metadata.remove(kheish_types::LEARNED_CONTEXT_METADATA_KEY);
            metadata.remove(kheish_types::RECOVERED_MEMORY_METADATA_KEY);
        }
        let offset = self.record_event(SessionEvent::InputReceived {
            input: persisted_input,
        });
        let content = Self::input_to_content(&input.payload)?;
        self.append_message(MessageRecord::new(
            Self::user_message_id_for_input_offset(offset),
            Role::User,
            content,
        ));
        Ok(offset)
    }

    fn user_message_id_for_input_offset(offset: u64) -> String {
        format!("user-{offset}")
    }

    fn build_run_meta(
        conversation: &ConversationKey,
        policy: &LoopPolicy,
        input: &InputEnvelope,
        input_event_offset: u64,
    ) -> Result<RunMetaSnapshot> {
        let completion_requirements =
            kheish_types::completion_requirements_from_metadata(&input.metadata)?;
        let output_contract =
            kheish_types::structured_output_contract_from_metadata(&input.metadata)?;
        let learned_context = kheish_types::learned_context_from_metadata(&input.metadata)?;
        let recovered_memory = kheish_types::recovered_memory_from_metadata(&input.metadata)?;
        let visible_skills = kheish_types::session_visible_skills_from_metadata(&input.metadata)?;
        Ok(RunMetaSnapshot {
            session_id: conversation.session_id.clone(),
            thread_id: conversation.thread_id.clone(),
            input_source_plugin: input.source.plugin.clone(),
            input_source_kind: input.source.kind.clone(),
            input_event_offset,
            completion_requirements,
            completion_follow_up_count: 0,
            output_contract,
            output_contract_repair_count: 0,
            max_output_tokens_recovery_count: 0,
            permission_denied_retry_count: 0,
            recovered_memory,
            learned_context,
            visible_skills,
            autocompact: kheish_types::AutocompactTracking::default(),
            policy: RunPolicySnapshot {
                max_turns: policy.max_turns,
                keep_last_messages: policy.keep_last_messages,
                snip_token_budget: policy.snip_token_budget,
                snip_keep_minimum_messages: policy.snip_keep_minimum,
                microcompact_keep_recent: policy.microcompact_keep_recent,
                microcompact_idle_timeout_ms: policy.microcompact_stale_after_ms,
                session_memory_min_tokens: policy.session_memory_min_tokens,
                session_memory_max_tokens: policy.session_memory_max_tokens,
                autocompact_threshold_tokens: policy.autocompact_threshold_tokens,
                autocompact_buffer_tokens: policy.autocompact_buffer_tokens,
            },
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
        })
    }

    fn unsatisfied_completion_requirements(
        &self,
        run_meta: &RunMetaSnapshot,
    ) -> Result<Vec<CompletionRequirement>> {
        if run_meta.completion_requirements.is_empty() {
            return Ok(Vec::new());
        }

        let mut calls_by_id = std::collections::BTreeMap::<String, ToolCallRecord>::new();
        let mut satisfied = vec![false; run_meta.completion_requirements.len()];
        for entry in self
            .journal
            .iter()
            .filter(|entry| entry.offset > run_meta.input_event_offset)
        {
            match &entry.event {
                SessionEvent::ToolCallStarted { call } => {
                    calls_by_id.insert(call.id.clone(), call.clone());
                }
                SessionEvent::ToolCallFinished { result } if !result.is_error => {
                    if let Some(call) = calls_by_id.get(&result.call_id) {
                        for (index, requirement) in
                            run_meta.completion_requirements.iter().enumerate()
                        {
                            if !satisfied[index]
                                && Self::completion_requirement_satisfied(requirement, call, result)
                            {
                                satisfied[index] = true;
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(run_meta
            .completion_requirements
            .iter()
            .cloned()
            .zip(satisfied)
            .filter_map(|(requirement, satisfied)| (!satisfied).then_some(requirement))
            .collect())
    }

    fn completion_requirement_satisfied(
        requirement: &CompletionRequirement,
        call: &ToolCallRecord,
        result: &ToolResultRecord,
    ) -> bool {
        match requirement {
            CompletionRequirement::WorkspaceFile { path } => {
                if !matches!(
                    call.name.as_str(),
                    "write_file" | "edit_file" | "apply_patch"
                ) {
                    return false;
                }
                let call_path = call.input.get("path").and_then(|value| value.as_str());
                let result_path = result.output.get("path").and_then(|value| value.as_str());
                path.as_deref()
                    .map(|path| {
                        Self::path_matches_requirement(path, call_path, result_path)
                            || Self::apply_patch_changed_file_matches(path, &result.output)
                    })
                    .unwrap_or(true)
            }
        }
    }

    fn apply_patch_changed_file_matches(required: &str, output: &Value) -> bool {
        output
            .get("changed_files")
            .and_then(Value::as_array)
            .map(|files| {
                files.iter().any(|file| {
                    file.get("path")
                        .and_then(Value::as_str)
                        .map(|path| Self::path_matches_requirement(required, None, Some(path)))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }

    fn path_matches_requirement(
        required: &str,
        call_path: Option<&str>,
        result_path: Option<&str>,
    ) -> bool {
        let required = Self::normalize_path_for_match(required);
        [call_path, result_path]
            .into_iter()
            .flatten()
            .any(|candidate| {
                let candidate = Self::normalize_path_for_match(candidate);
                candidate == required || candidate.ends_with(&format!("/{required}"))
            })
    }

    fn normalize_path_for_match(path: &str) -> String {
        path.trim()
            .replace('\\', "/")
            .trim_start_matches("./")
            .to_string()
    }

    fn append_completion_follow_up_message(
        &mut self,
        turn: usize,
        requirements: &[CompletionRequirement],
    ) {
        let content = Self::completion_follow_up_message(requirements);
        self.append_message(MessageRecord::new(
            format!("user-completion-reminder-{turn}"),
            Role::User,
            content,
        ));
    }

    fn completion_follow_up_message(requirements: &[CompletionRequirement]) -> String {
        let mut lines = vec![
            "Continue from where you left off.".to_string(),
            "The task is not complete yet.".to_string(),
        ];
        for requirement in requirements {
            match requirement {
                CompletionRequirement::WorkspaceFile { path: Some(path) } => lines.push(format!(
                    "You must create or update the workspace file `{path}` before replying with a final answer."
                )),
                CompletionRequirement::WorkspaceFile { path: None } => lines.push(
                    "You must create or update a file in the workspace containing the requested result before replying with a final answer."
                        .to_string(),
                ),
            }
        }
        lines.push("Use a filesystem tool now instead of describing the next step.".to_string());
        lines.join("\n")
    }

    fn append_permission_denied_retry_message(&mut self, turn: usize, tools: &[String]) {
        let tool_list = tools.join(", ");
        self.append_message(MessageRecord::new(
            format!("user-permission-retry-{turn}"),
            Role::User,
            format!(
                "A permission-denied hook requested one retry for the blocked tool call(s): {tool_list}.\n\
                 Continue from where you left off. If you still need the action, retry with safer input or a narrower scope instead of stopping."
            ),
        ));
    }

    fn append_max_tokens_recovery_message(&mut self, turn: usize) {
        self.append_message(MessageRecord::new(
            format!("user-max-tokens-recovery-{turn}"),
            Role::User,
            Self::max_tokens_recovery_message(),
        ));
    }

    fn max_tokens_recovery_message() -> String {
        "Output token limit hit. Resume directly — no apology, no recap of what you were doing. Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces.".to_string()
    }

    fn should_escalate_max_output_tokens(
        base: &ModelGenerationConfig,
        current: &ModelGenerationConfig,
    ) -> bool {
        base.max_output_tokens.is_none()
            && current
                .max_output_tokens
                .unwrap_or_default()
                .lt(&kheish_types::ESCALATED_MAX_OUTPUT_TOKENS)
    }

    fn max_tokens_recovery_generation(
        base: &ModelGenerationConfig,
        current: &ModelGenerationConfig,
    ) -> ModelGenerationConfig {
        if Self::should_escalate_max_output_tokens(base, current) {
            let mut generation = current.clone();
            generation.max_output_tokens = Some(kheish_types::ESCALATED_MAX_OUTPUT_TOKENS);
            return generation;
        }
        base.clone()
    }

    /// Parses and validates the run's final answer against its contract.
    fn contract_conformant_output(
        contract: &kheish_types::StructuredOutputContract,
        answer: &str,
    ) -> std::result::Result<serde_json::Value, String> {
        let candidate = kheish_types::extract_json_text(answer);
        let value: serde_json::Value = serde_json::from_str(candidate)
            .map_err(|error| format!("final answer is not valid JSON: {error}"))?;
        contract.schema.validate_value(&value)?;
        Ok(value)
    }

    fn append_output_contract_repair_message(
        &mut self,
        turn: usize,
        error: &str,
        schema: &kheish_types::StructuredFieldSchema,
    ) {
        let rendered_schema = serde_json::to_string_pretty(&schema.to_json_schema())
            .unwrap_or_else(|_| "{}".to_string());
        let content = format!(
            "Your final answer must be a single JSON value matching the output contract.\n\
             Validation failed: {error}.\n\
             Reply with ONLY the corrected JSON \u{2014} no prose, no Markdown fences.\n\
             Schema:\n{rendered_schema}"
        );
        self.append_message(MessageRecord::new(
            format!("user-output-contract-repair-{turn}"),
            Role::User,
            content,
        ));
    }

    /// Repair turns must answer in text: tools are withheld entirely so the
    /// model cannot wander off instead of correcting its payload.
    fn output_contract_repair_generation(base: &ModelGenerationConfig) -> ModelGenerationConfig {
        let mut generation = base.clone();
        generation.tool_choice = ToolChoice::None;
        generation.allow_parallel_tool_calls = false;
        generation
    }

    fn completion_follow_up_generation(
        base: &ModelGenerationConfig,
        requirements: &[CompletionRequirement],
        available_tools: &[ToolDefinition],
    ) -> ModelGenerationConfig {
        let mut generation = base.clone();
        generation.allow_parallel_tool_calls = false;
        if requirements
            .iter()
            .any(|requirement| matches!(requirement, CompletionRequirement::WorkspaceFile { .. }))
        {
            if available_tools.iter().any(|tool| tool.name == "write_file") {
                generation.tool_choice = ToolChoice::Specific {
                    name: "write_file".to_string(),
                };
            } else if available_tools.iter().any(|tool| tool.name == "edit_file") {
                generation.tool_choice = ToolChoice::Specific {
                    name: "edit_file".to_string(),
                };
            } else {
                generation.tool_choice = ToolChoice::Required;
            }
        }
        generation
    }

    fn post_tool_execution_generation(current: &ModelGenerationConfig) -> ModelGenerationConfig {
        let mut generation = current.clone();
        if matches!(
            generation.tool_choice,
            ToolChoice::Specific { .. } | ToolChoice::Required
        ) {
            generation.tool_choice = ToolChoice::Auto;
        }
        generation
    }

    fn completion_requirement_summary(requirements: &[CompletionRequirement]) -> String {
        requirements
            .iter()
            .map(|requirement| match requirement {
                CompletionRequirement::WorkspaceFile { path: Some(path) } => {
                    format!("workspace file `{path}`")
                }
                CompletionRequirement::WorkspaceFile { path: None } => {
                    "workspace file write".to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn build_prompt_snapshot(&self, prompt: &PromptProjection) -> Result<PromptSnapshot> {
        let checkpoint = self.checkpoints.last();
        Ok(PromptSnapshot {
            checkpoint_offset_used: checkpoint.map(|checkpoint| checkpoint.compacted_until_offset),
            prompt_window_id: checkpoint.map(|checkpoint| checkpoint.prompt_window_id.clone()),
            prompt_window_generation: checkpoint
                .map(|checkpoint| checkpoint.prompt_window_generation)
                .unwrap_or_default(),
            prompt_window_started_after_offset: self
                .provider_window_started_after_offset(checkpoint),
            summary_digest: prompt.summary.as_ref().map(Self::digest_of).transpose()?,
            summary_text: prompt
                .summary
                .as_ref()
                .map(|summary| summary.content.clone()),
            system_sections: prompt
                .system_sections
                .iter()
                .map(Self::build_system_prompt_section_snapshot)
                .collect::<Result<Vec<_>>>()?,
            messages: prompt
                .messages
                .iter()
                .map(Self::build_prompt_message_snapshot)
                .collect::<Result<Vec<_>>>()?,
            open_tool_call_ids: prompt
                .open_tool_calls
                .iter()
                .map(|call| call.id.clone())
                .collect(),
        })
    }

    fn build_final_state_snapshot(&self) -> Result<FinalStateSnapshot> {
        let snapshot = self.replay_from_journal();
        Ok(FinalStateSnapshot {
            messages: snapshot
                .messages
                .iter()
                .map(Self::build_prompt_message_snapshot)
                .collect::<Result<Vec<_>>>()?,
            message_digests: snapshot
                .messages
                .iter()
                .map(Self::digest_message_record)
                .collect::<Result<Vec<_>>>()?,
            message_roles: snapshot
                .messages
                .iter()
                .map(|message| message.role.clone())
                .collect(),
            completed_tool_result_digests: snapshot
                .completed_tool_results
                .iter()
                .map(Self::digest_tool_result_record)
                .collect::<Result<Vec<_>>>()?,
            open_tool_call_ids: snapshot.open_tool_calls.keys().cloned().collect(),
            canonical_state_digest: Self::digest_canonical_state(&snapshot)?,
        })
    }

    fn build_prompt_message_snapshot(message: &MessageRecord) -> Result<PromptMessageSnapshot> {
        Ok(PromptMessageSnapshot {
            id: message.id.clone(),
            role: message.role.clone(),
            digest: Self::digest_message_record(message)?,
            content_digest: Self::digest_message_content(message)?,
        })
    }

    fn build_system_prompt_section_snapshot(
        section: &SystemPromptSection,
    ) -> Result<SystemPromptSectionSnapshot> {
        Ok(SystemPromptSectionSnapshot {
            name: section.name.clone(),
            digest: Self::digest_of(section)?,
            content_digest: digest_text(&section.content),
        })
    }

    fn denied_tool_result(
        call: &ToolCallRecord,
        reason: String,
        hook_contexts: Vec<String>,
    ) -> ToolResultRecord {
        ToolResultRecord {
            call_id: call.id.clone(),
            output: json!({
                "error": reason,
                "tool": call.name,
                "permission_denied": true,
            }),
            is_error: true,
            tool_name: Some(call.name.clone()),
            offset: None,
            timestamp_ms: None,
            context_updates: Vec::new(),
            hook_contexts,
        }
    }

    fn extract_pending_user_question(
        result: &ToolResultRecord,
    ) -> Result<Option<UserQuestionRequest>> {
        if result.is_error
            || !matches!(
                result.tool_name.as_deref(),
                Some("ask_user_question" | "ask_operator")
            )
        {
            return Ok(None);
        }
        let Some(marker) = result.output.get("_kheish_pending_user_question") else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_value(marker.clone())?))
    }

    fn user_question_tool_result(
        &self,
        pending: &PendingUserQuestion,
        resolution: &UserQuestionResolution,
    ) -> Result<ToolResultRecord> {
        let mut output = render_user_question_resolution(&pending.request, resolution)?;
        let audience = if pending.call.name == "ask_operator" {
            "Operator"
        } else {
            "User"
        };
        output["message"] = json!(if resolution.declined {
            format!("{audience} declined to answer your clarification request.")
        } else {
            format!("{audience} answered your clarification request.")
        });

        Ok(ToolResultRecord {
            call_id: pending.call.id.clone(),
            output,
            is_error: false,
            tool_name: Some(pending.call.name.clone()),
            offset: None,
            timestamp_ms: None,
            context_updates: Vec::new(),
            hook_contexts: Vec::new(),
        })
    }

    async fn dispatch_hook(
        &self,
        event: HookEventName,
        subject: Option<String>,
        payload: Value,
    ) -> Result<HookDispatchOutcome> {
        let Some(dispatcher) = self.hook_dispatcher.as_ref() else {
            return Ok(HookDispatchOutcome::default());
        };
        dispatcher
            .dispatch(HookInvocation {
                event,
                subject,
                session_id: Some(self.conversation.session_id.clone()),
                agent_id: None,
                run_id: None,
                payload,
            })
            .await
    }

    fn inject_hook_contexts(&mut self, event: HookEventName, contexts: &[String]) {
        for context in contexts {
            let label = format!("{event:?}");
            self.inject_message(
                Role::System,
                format!("Hook additional context ({label}):\n{context}"),
            );
        }
    }

    async fn emit_file_changed_hook_contexts(&mut self, result: &ToolResultRecord) -> Result<()> {
        for update in &result.context_updates {
            let (event, subject, payload) = match update {
                kheish_types::ContextUpdate::FileModified { path } => (
                    HookEventName::FileChanged,
                    path.clone(),
                    json!({
                        "path": path,
                        "kind": "modified",
                        "tool_result_call_id": result.call_id,
                    }),
                ),
                kheish_types::ContextUpdate::FileRead { path } => (
                    HookEventName::FileChanged,
                    path.clone(),
                    json!({
                        "path": path,
                        "kind": "read",
                        "tool_result_call_id": result.call_id,
                    }),
                ),
                kheish_types::ContextUpdate::WorkspaceRootChanged { path } => (
                    HookEventName::CwdChanged,
                    path.clone(),
                    json!({
                        "path": path,
                        "tool_result_call_id": result.call_id,
                    }),
                ),
                kheish_types::ContextUpdate::WebResourceVisited { .. } => continue,
            };
            let outcome = self
                .dispatch_hook(event.clone(), Some(subject), payload)
                .await?;
            if !outcome.additional_contexts.is_empty() {
                self.inject_hook_contexts(event, &outcome.additional_contexts);
            }
        }
        Ok(())
    }

    fn append_message(&mut self, message: MessageRecord) -> u64 {
        self.record_event(SessionEvent::MessageAppended { message })
    }

    fn append_tool_result_message(&mut self, result: &ToolResultRecord) -> Result<u64> {
        let prompt_visible = Self::prompt_visible_tool_result(result);
        let tool_message = MessageRecord::new(
            format!("tool-message-{}", result.call_id),
            Role::Tool,
            serde_json::to_string(&prompt_visible.output)?,
        );
        Ok(self.append_message(tool_message))
    }

    fn start_tool_call(&mut self, call: ToolCallRecord) -> u64 {
        self.record_event(SessionEvent::ToolCallStarted { call })
    }

    fn finish_tool_call(&mut self, result: ToolResultRecord) -> u64 {
        self.record_event(SessionEvent::ToolCallFinished { result })
    }

    fn record_event(&mut self, event: SessionEvent) -> u64 {
        let offset = self.next_offset;
        self.next_offset += 1;
        let timestamp_ms = Self::current_timestamp_ms();
        let event = match event {
            SessionEvent::MessageAppended { mut message } => {
                if message.offset.is_none() {
                    message.offset = Some(offset);
                }
                if message.timestamp_ms.is_none() {
                    message.timestamp_ms = Some(timestamp_ms);
                }
                SessionEvent::MessageAppended { message }
            }
            SessionEvent::ToolCallFinished { mut result } => {
                if result.offset.is_none() {
                    result.offset = Some(offset);
                }
                if result.timestamp_ms.is_none() {
                    result.timestamp_ms = Some(timestamp_ms);
                }
                SessionEvent::ToolCallFinished { result }
            }
            other => other,
        };
        self.journal.push(LogEntry {
            offset,
            timestamp_ms,
            event,
        });
        offset
    }

    fn input_to_content(payload: &InputPayload) -> Result<String> {
        match payload {
            InputPayload::Text { content } => Ok(content.clone()),
            InputPayload::Rich {
                rendered_content, ..
            } => Ok(rendered_content.clone()),
            InputPayload::Json { value } => serde_json::to_string_pretty(value).map_err(Into::into),
            InputPayload::Event { name, value } => {
                Ok(format!("{name}: {}", serde_json::to_string(value)?))
            }
            InputPayload::Command { name, arguments } => Ok(format!(
                "command {name}: {}",
                serde_json::to_string(arguments)?
            )),
        }
    }

    fn replay_entries<'a>(
        &self,
        entries: impl IntoIterator<Item = &'a LogEntry>,
    ) -> CanonicalStateSnapshot {
        Self::replay_log_entries(entries)
    }

    fn replay_log_entries<'a>(
        entries: impl IntoIterator<Item = &'a LogEntry>,
    ) -> CanonicalStateSnapshot {
        let mut snapshot = CanonicalStateSnapshot::default();
        for entry in entries {
            Self::apply_log_entry(&mut snapshot, entry);
        }
        snapshot
    }

    fn replay_until_offset(&self, offset: u64) -> CanonicalStateSnapshot {
        self.replay_entries(self.journal.iter().filter(|entry| entry.offset <= offset))
    }

    fn sanitize_checkpoints(
        journal: &[LogEntry],
        checkpoints: Vec<SessionCheckpoint>,
    ) -> Vec<SessionCheckpoint> {
        if checkpoints.is_empty() {
            return checkpoints;
        }
        let full_snapshot = Self::replay_log_entries(journal.iter());
        let Ok(full_digest) = Self::digest_canonical_state(&full_snapshot) else {
            return Vec::new();
        };
        for valid_index in (0..checkpoints.len()).rev() {
            match Self::checkpoint_matches_full_state(
                &checkpoints[valid_index],
                journal,
                &full_digest,
            ) {
                Ok(true) => {
                    return checkpoints.into_iter().take(valid_index + 1).collect();
                }
                Ok(false) | Err(_) => {}
            }
        }
        Vec::new()
    }

    fn checkpoint_matches_full_state(
        checkpoint: &SessionCheckpoint,
        journal: &[LogEntry],
        expected_full_digest: &str,
    ) -> Result<bool> {
        let prefix_entries = journal
            .iter()
            .filter(|entry| entry.offset <= checkpoint.compacted_until_offset)
            .collect::<Vec<_>>();
        let prefix_digest = Self::digest_log_entries(prefix_entries.into_iter())?;
        if prefix_digest != checkpoint.journal_digest {
            return Ok(false);
        }

        let mut restored = checkpoint.canonical.clone();
        for entry in journal
            .iter()
            .filter(|entry| entry.offset > checkpoint.compacted_until_offset)
        {
            Self::apply_log_entry(&mut restored, entry);
        }

        Ok(Self::digest_canonical_state(&restored)? == expected_full_digest)
    }

    fn apply_log_entry(snapshot: &mut CanonicalStateSnapshot, entry: &LogEntry) {
        match &entry.event {
            SessionEvent::InputReceived { input } => {
                let content_parts = Self::input_content_parts(input);
                if !content_parts.is_empty() {
                    snapshot.input_content_parts.insert(
                        Self::user_message_id_for_input_offset(entry.offset),
                        content_parts,
                    );
                }
            }
            SessionEvent::CompactionBoundary { .. }
            | SessionEvent::UserQuestionRequested { .. }
            | SessionEvent::UserQuestionResolved { .. } => {}
            SessionEvent::MessageAppended { message } => snapshot.messages.push(message.clone()),
            SessionEvent::ToolCallStarted { call } => {
                snapshot
                    .open_tool_calls
                    .insert(call.id.clone(), call.clone());
            }
            SessionEvent::ToolCallFinished { result } => {
                snapshot.open_tool_calls.remove(&result.call_id);
                snapshot.completed_tool_results.push(result.clone());
            }
        }
    }

    fn current_timestamp_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    fn record_compaction_boundary(&mut self, boundary: CompactionBoundary) {
        self.record_event(SessionEvent::CompactionBoundary { boundary });
    }

    fn provider_window_started_after_offset(
        &self,
        checkpoint: Option<&SessionCheckpoint>,
    ) -> Option<u64> {
        let checkpoint_offset = checkpoint.map(|checkpoint| {
            let effective = checkpoint.effective_prompt_window_started_after_offset();
            if effective == u64::MAX {
                self.first_compaction_boundary_after(checkpoint.compacted_until_offset)
                    .unwrap_or(u64::MAX)
            } else {
                effective
            }
        });
        let boundary_offset = self.latest_compaction_boundary_offset();
        checkpoint_offset.into_iter().chain(boundary_offset).max()
    }

    fn first_compaction_boundary_after(&self, offset: u64) -> Option<u64> {
        self.journal.iter().find_map(|entry| {
            (entry.offset > offset
                && matches!(entry.event, SessionEvent::CompactionBoundary { .. }))
            .then_some(entry.offset)
        })
    }

    fn latest_compaction_boundary_offset(&self) -> Option<u64> {
        self.journal.iter().rev().find_map(|entry| {
            matches!(entry.event, SessionEvent::CompactionBoundary { .. }).then_some(entry.offset)
        })
    }

    fn provider_resume_allowed_at_offset(
        provider_window_started_after_offset: Option<u64>,
        offset: u64,
    ) -> bool {
        provider_window_started_after_offset
            .map(|started_after| offset > started_after)
            .unwrap_or(true)
    }

    fn without_provider_resume_tool_call(call: &ToolCallRecord) -> ToolCallRecord {
        let mut call = call.clone();
        call.assistant_provider_response_id = None;
        call
    }

    fn disable_provider_resume_in_workspace(workspace: &mut PromptWorkspace) {
        for message in &mut workspace.prompt.messages {
            message.provider_response_id = None;
            message.provider_context = None;
        }
        for call in &mut workspace.prompt.open_tool_calls {
            call.assistant_provider_response_id = None;
        }
        for item in &mut workspace.provider_prompt.items {
            match item {
                ProviderInputItem::Message {
                    provider_response_id,
                    provider_context,
                    ..
                } => {
                    *provider_response_id = None;
                    *provider_context = None;
                }
                ProviderInputItem::ToolCall { call, .. } => {
                    call.assistant_provider_response_id = None;
                }
                ProviderInputItem::Summary { .. }
                | ProviderInputItem::Restoration { .. }
                | ProviderInputItem::ToolResult { .. } => {}
            }
        }
    }

    fn prompt_visible_message(message: &MessageRecord) -> MessageRecord {
        if message.role != Role::Tool {
            return message.clone();
        }
        if rough_token_estimate_value(&Value::String(message.content.clone()))
            <= PROMPT_VISIBLE_TOOL_OUTPUT_TOKEN_LIMIT
        {
            return message.clone();
        }

        let output = serde_json::from_str::<Value>(&message.content)
            .unwrap_or_else(|_| Value::String(message.content.clone()));
        let capped = Self::bounded_prompt_tool_output(
            message.id.as_str(),
            None,
            format!("session://messages/{}", message.id),
            &output,
        );
        let mut message = message.clone();
        message.content = serde_json::to_string(&capped)
            .unwrap_or_else(|_| "\"[Tool output truncated for prompt safety]\"".to_string());
        message
    }

    fn prompt_visible_tool_result(result: &ToolResultRecord) -> ToolResultRecord {
        let mut result = result.clone();
        result.output = Self::bounded_prompt_tool_output(
            result.call_id.as_str(),
            result.tool_name.as_deref(),
            format!("session://tool-results/{}", result.call_id),
            &result.output,
        );
        result
    }

    fn bounded_prompt_tool_output(
        source_id: &str,
        tool_name: Option<&str>,
        raw_ref: String,
        output: &Value,
    ) -> Value {
        let estimated_tokens = rough_token_estimate_value(output);
        if estimated_tokens <= PROMPT_VISIBLE_TOOL_OUTPUT_TOKEN_LIMIT {
            return output.clone();
        }

        let raw = serde_json::to_string(output).unwrap_or_else(|_| output.to_string());
        let preview = Self::truncate_prompt_preview(&raw, PROMPT_VISIBLE_TOOL_OUTPUT_PREVIEW_CHARS);
        json!({
            "_kheish_prompt_visible_tool_output": {
                "truncated": true,
                "source_id": source_id,
                "tool_name": tool_name,
                "raw_ref": raw_ref,
                "sha256": digest_text(&raw),
                "bytes": raw.len(),
                "estimated_tokens": estimated_tokens,
                "token_limit": PROMPT_VISIBLE_TOOL_OUTPUT_TOKEN_LIMIT,
                "preview": preview,
                "message": "Tool output was truncated for prompt safety. The raw output is retained in the session audit."
            }
        })
    }

    fn truncate_prompt_preview(value: &str, max_chars: usize) -> String {
        let mut truncated = false;
        let mut end = value.len();
        let mut count = 0usize;
        for (index, _) in value.char_indices() {
            if count == max_chars {
                end = index;
                truncated = true;
                break;
            }
            count += 1;
        }
        if !truncated {
            value.to_string()
        } else {
            value[..end].to_string()
        }
    }

    fn collect_compaction_boundaries_since(&self, offset: u64) -> Vec<CompactionBoundary> {
        self.journal
            .iter()
            .filter(|entry| entry.offset >= offset)
            .filter_map(|entry| match &entry.event {
                SessionEvent::CompactionBoundary { boundary } => Some(boundary.clone()),
                _ => None,
            })
            .collect()
    }

    fn snapshot_autocompact_tracking(&self) -> kheish_types::AutocompactTracking {
        kheish_types::AutocompactTracking {
            consecutive_failures: self.autocompact_tracking.consecutive_failures,
            last_compacted_turn: self.autocompact_tracking.last_compacted_turn,
            turn_counter: self.autocompact_tracking.turn_counter,
        }
    }

    fn sync_run_meta_autocompact(run_meta: &mut RunMetaSnapshot, tracking: &AutocompactTracking) {
        run_meta.autocompact = kheish_types::AutocompactTracking {
            consecutive_failures: tracking.consecutive_failures,
            last_compacted_turn: tracking.last_compacted_turn,
            turn_counter: tracking.turn_counter,
        };
    }

    fn prompt_token_count(prompt: &PromptProjection) -> usize {
        calibrated_prompt_token_count(
            prompt.summary.as_ref(),
            &prompt.system_sections,
            &prompt.messages,
            &prompt.open_tool_calls,
            prompt.restoration.as_ref(),
        )
    }

    fn effective_autocompact_threshold_tokens(&self, generation: &ModelGenerationConfig) -> usize {
        let Some(model) = generation.model.as_deref() else {
            return self.policy.autocompact_threshold_tokens;
        };
        let Some(context_window) = model_context_window(model) else {
            return self.policy.autocompact_threshold_tokens;
        };
        let reserved_output_tokens = generation
            .max_output_tokens
            .map(|value| value as usize)
            .unwrap_or_else(|| model_max_output_tokens(model).default as usize);
        context_window
            .saturating_sub(reserved_output_tokens)
            .saturating_sub(self.policy.autocompact_buffer_tokens)
            .max(1)
            .min(self.policy.autocompact_threshold_tokens)
    }

    fn apply_snip_to_workspace(&self, workspace: &mut PromptWorkspace, new_head_offset: u64) {
        workspace.prompt.messages = workspace
            .prompt
            .messages
            .iter()
            .cloned()
            .zip(workspace.message_offsets.iter().copied())
            .filter_map(|(message, offset)| {
                offset
                    .is_none_or(|offset| offset >= new_head_offset)
                    .then_some(message)
            })
            .collect();
        workspace.message_offsets = workspace
            .message_offsets
            .iter()
            .copied()
            .filter(|offset| offset.is_none_or(|offset| offset >= new_head_offset))
            .collect();
        self.filter_provider_items_from_head_offset(workspace, new_head_offset);
    }

    fn filter_provider_items_from_head_offset(
        &self,
        workspace: &mut PromptWorkspace,
        new_head_offset: u64,
    ) {
        let mut filtered_items = Vec::new();
        let mut filtered_offsets = Vec::new();

        for (item, offset) in workspace
            .provider_prompt
            .items
            .iter()
            .cloned()
            .zip(workspace.item_offsets.iter().copied())
        {
            if offset.is_none() || offset.is_some_and(|offset| offset >= new_head_offset) {
                filtered_items.push(item);
                filtered_offsets.push(offset);
            }
        }

        workspace.provider_prompt.items = filtered_items;
        workspace.item_offsets = filtered_offsets;
        workspace.prompt.open_tool_calls =
            Self::recompute_open_tool_calls(&workspace.provider_prompt.items);
        Self::disable_provider_resume_in_workspace(workspace);
    }

    fn recompute_open_tool_calls(items: &[ProviderInputItem]) -> Vec<ToolCallRecord> {
        let mut open = std::collections::BTreeMap::<String, ToolCallRecord>::new();
        for item in items {
            match item {
                ProviderInputItem::ToolCall { call, .. } => {
                    open.insert(call.id.clone(), call.clone());
                }
                ProviderInputItem::ToolResult { result } => {
                    open.remove(&result.call_id);
                }
                ProviderInputItem::Summary { .. }
                | ProviderInputItem::Message { .. }
                | ProviderInputItem::Restoration { .. } => {}
            }
        }
        open.into_values().collect()
    }

    fn timestamp_for_offset(&self, offset: u64) -> Option<u64> {
        self.journal
            .iter()
            .find(|entry| entry.offset == offset)
            .map(|entry| entry.timestamp_ms)
    }

    fn is_microcompact_stale(&self, workspace: &PromptWorkspace) -> bool {
        let Some(stale_after_ms) = self.policy.microcompact_stale_after_ms else {
            return false;
        };
        let Some(last_offset) = workspace
            .prompt
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .and_then(|message| message.offset)
        else {
            return false;
        };
        let Some(last_timestamp) = self.timestamp_for_offset(last_offset) else {
            return false;
        };
        Self::current_timestamp_ms().saturating_sub(last_timestamp) > stale_after_ms
    }

    fn tool_results_from_provider_items(items: &[ProviderInputItem]) -> Vec<ToolResultRecord> {
        items
            .iter()
            .filter_map(|item| match item {
                ProviderInputItem::ToolResult { result } => Some(result.clone()),
                _ => None,
            })
            .collect()
    }

    fn apply_microcompact_to_workspace(
        &self,
        workspace: &mut PromptWorkspace,
        keep_recent: usize,
        turn: usize,
    ) -> Option<CompactionBoundary> {
        let tool_results = Self::tool_results_from_provider_items(&workspace.provider_prompt.items);
        let result =
            microcompact_tool_results(&mut workspace.prompt.messages, &tool_results, keep_recent);
        if result.cleared_tool_ids.is_empty() {
            return None;
        }
        let cleared = result
            .cleared_tool_ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        for item in &mut workspace.provider_prompt.items {
            let ProviderInputItem::ToolResult { result } = item else {
                continue;
            };
            if cleared.contains(&result.call_id) {
                result.output = serde_json::Value::String(CLEARED_TOOL_RESULT_MESSAGE.to_string());
            }
        }
        for message in &mut workspace.prompt.messages {
            if message.role != Role::Tool {
                continue;
            }
            let Some(call_id) = message.id.strip_prefix("tool-message-") else {
                continue;
            };
            if cleared.contains(call_id) {
                message.content = CLEARED_TOOL_RESULT_MESSAGE.to_string();
            }
        }
        Self::disable_provider_resume_in_workspace(workspace);
        Some(CompactionBoundary::Microcompact {
            turn,
            cleared_tool_ids: result.cleared_tool_ids,
            tokens_freed: result.tokens_freed,
        })
    }

    fn try_session_memory_compact(
        &mut self,
        workspace: &mut PromptWorkspace,
        upcoming_turn: usize,
        pre_tokens: usize,
        threshold_tokens: usize,
    ) -> Result<bool> {
        if self.checkpoints.is_empty() {
            return Ok(false);
        }

        let Some(cutoff) = self.next_offset.checked_sub(1) else {
            return Ok(false);
        };
        let (_, groups) = self.compaction_source_groups(cutoff);
        if groups.len() <= 1 {
            return Ok(false);
        }

        let mut kept_groups = Vec::new();
        let mut kept_tokens = 0usize;
        for group in groups.iter().rev() {
            let group_tokens = Self::estimated_group_tokens(group);
            if !kept_groups.is_empty()
                && kept_tokens.saturating_add(group_tokens) > self.policy.session_memory_max_tokens
            {
                break;
            }
            kept_tokens = kept_tokens.saturating_add(group_tokens);
            kept_groups.push(group);
            if kept_tokens >= self.policy.session_memory_min_tokens {
                break;
            }
        }

        if kept_tokens < self.policy.session_memory_min_tokens || kept_groups.len() == groups.len()
        {
            return Ok(false);
        }

        kept_groups.reverse();
        let Some(new_head_offset) = kept_groups
            .first()
            .and_then(|group| group.first())
            .map(|entry| entry.offset)
        else {
            return Ok(false);
        };

        let mut candidate = workspace.clone();
        self.apply_snip_to_workspace(&mut candidate, new_head_offset);
        let post_tokens = Self::prompt_token_count(&candidate.prompt);
        if post_tokens >= pre_tokens || post_tokens > threshold_tokens {
            return Ok(false);
        }

        *workspace = candidate;
        self.record_compaction_boundary(CompactionBoundary::Autocompact {
            turn: upcoming_turn,
            trigger: CompactionTrigger::Auto,
            pre_tokens,
            post_tokens,
            strategy: CompactionStrategy::SessionMemory,
        });
        Ok(true)
    }

    async fn compact_pipeline<M>(
        &mut self,
        workspace: &mut PromptWorkspace,
        upcoming_turn: usize,
        generation: &ModelGenerationConfig,
        model: &M,
        restoration: Option<&dyn PostCompactRestorationProvider>,
    ) -> Result<Option<CheckpointSnapshot>>
    where
        M: ModelDriver + ?Sized,
    {
        let snip = snip_if_needed(
            &workspace.prompt.messages,
            self.policy.snip_token_budget,
            self.policy.snip_keep_minimum,
        );
        if snip.messages_removed > 0 {
            self.apply_snip_to_workspace(workspace, snip.new_head_offset);
            self.record_compaction_boundary(CompactionBoundary::Snip {
                turn: upcoming_turn,
                messages_removed: snip.messages_removed,
                tokens_freed: snip.tokens_freed,
                new_head_offset: snip.new_head_offset,
            });
        }

        if self.is_microcompact_stale(workspace) {
            let keep_recent = self.policy.microcompact_keep_recent.max(1);
            if let Some(boundary) =
                self.apply_microcompact_to_workspace(workspace, keep_recent, upcoming_turn)
            {
                self.record_compaction_boundary(boundary);
            }
        }

        let post_light_tokens = Self::prompt_token_count(&workspace.prompt);
        let threshold_tokens = self.effective_autocompact_threshold_tokens(generation);
        if post_light_tokens <= threshold_tokens {
            return Ok(None);
        }

        let pre_compact = self
            .dispatch_hook(
                HookEventName::PreCompact,
                None,
                json!({
                    "turn": upcoming_turn,
                    "token_count": post_light_tokens,
                    "threshold_tokens": threshold_tokens,
                }),
            )
            .await?;
        if !pre_compact.additional_contexts.is_empty() {
            self.inject_hook_contexts(HookEventName::PreCompact, &pre_compact.additional_contexts);
        }
        if matches!(pre_compact.decision, Some(HookDecision::Block))
            || !pre_compact.continue_execution
        {
            return Ok(None);
        }

        if self.try_session_memory_compact(
            workspace,
            upcoming_turn,
            post_light_tokens,
            threshold_tokens,
        )? {
            self.autocompact_tracking.record_success(upcoming_turn);
            let post_compact = self
                .dispatch_hook(
                    HookEventName::PostCompact,
                    None,
                    json!({
                        "turn": upcoming_turn,
                        "pre_tokens": post_light_tokens,
                        "post_tokens": Self::prompt_token_count(&workspace.prompt),
                        "strategy": "session_memory",
                    }),
                )
                .await?;
            if !post_compact.additional_contexts.is_empty() {
                self.inject_hook_contexts(
                    HookEventName::PostCompact,
                    &post_compact.additional_contexts,
                );
            }
            return Ok(None);
        }

        if self.autocompact_tracking.should_skip() {
            return Ok(None);
        }

        match self
            .compact_if_needed_with_tokens(
                upcoming_turn,
                post_light_tokens,
                model,
                restoration,
                true,
                CompactionTrigger::Auto,
            )
            .await
        {
            Ok(checkpoint) => {
                if checkpoint.is_some() {
                    self.autocompact_tracking.record_success(upcoming_turn);
                }
                Ok(checkpoint)
            }
            Err(error) => {
                self.autocompact_tracking.record_failure();
                Err(error)
            }
        }
    }

    async fn force_reactive_compaction<M>(
        &mut self,
        upcoming_turn: usize,
        model: &M,
        restoration: Option<&dyn PostCompactRestorationProvider>,
    ) -> Result<Option<CheckpointSnapshot>>
    where
        M: ModelDriver + ?Sized,
    {
        let tokens = Self::prompt_token_count(&self.current_prompt_projection());
        self.compact_if_needed_with_tokens(
            upcoming_turn,
            tokens,
            model,
            restoration,
            true,
            CompactionTrigger::Auto,
        )
        .await
    }

    #[cfg(test)]
    async fn compact_if_needed<M>(
        &mut self,
        upcoming_turn: usize,
        model: &M,
    ) -> Result<Option<CheckpointSnapshot>>
    where
        M: ModelDriver + ?Sized,
    {
        let prompt = self.current_prompt_projection();
        let token_count = Self::prompt_token_count(&prompt);
        self.compact_if_needed_with_tokens(
            upcoming_turn,
            token_count,
            model,
            None,
            false,
            CompactionTrigger::Auto,
        )
        .await
    }

    async fn compact_if_needed_with_tokens<M>(
        &mut self,
        upcoming_turn: usize,
        token_count: usize,
        model: &M,
        restoration: Option<&dyn PostCompactRestorationProvider>,
        force: bool,
        trigger: CompactionTrigger,
    ) -> Result<Option<CheckpointSnapshot>>
    where
        M: ModelDriver + ?Sized,
    {
        if !force && token_count <= self.policy.autocompact_threshold_tokens {
            return Ok(None);
        }

        let snapshot = self.replay_from_journal();
        if snapshot.messages.len() <= self.policy.keep_last_messages {
            return Ok(None);
        }

        let prefix_len = snapshot.messages.len() - self.policy.keep_last_messages;
        let cutoff = self
            .offset_for_message_index(prefix_len - 1)
            .ok_or_else(|| anyhow!("unable to locate message cutoff for compaction"))?;

        if self
            .checkpoints
            .last()
            .map(|checkpoint| checkpoint.compacted_until_offset >= cutoff)
            .unwrap_or(false)
        {
            return Ok(None);
        }

        let Some(cutoff) = self.adjust_compaction_cutoff(cutoff)? else {
            return Ok(None);
        };

        let summary = self
            .generate_compaction_summary(cutoff, upcoming_turn, model)
            .await?;
        let canonical = self.replay_until_offset(cutoff);
        let preserved_segment = self.preserved_segment_after(cutoff);
        let prompt_window_generation = self
            .checkpoints
            .last()
            .map(|checkpoint| checkpoint.prompt_window_generation.max(1).saturating_add(1))
            .unwrap_or(1);
        let prompt_window_started_after_offset = self.next_offset.saturating_sub(1);
        let mut checkpoint = SessionCheckpoint {
            compacted_until_offset: cutoff,
            journal_digest: self.digest_until_offset(cutoff)?,
            prompt_summary: summary,
            prompt_window_id: format!("prompt-window-{prompt_window_generation}"),
            prompt_window_generation,
            prompt_window_started_after_offset,
            prompt_window_created_by: format!("{trigger:?}"),
            compact_metadata: CompactBoundaryMetadata {
                trigger,
                pre_tokens: token_count,
                post_tokens: 0,
                strategy: Some(CompactionStrategy::ModelDriven),
                preserved_segment,
            },
            canonical,
            restoration: None,
        };
        self.checkpoints.push(checkpoint.clone());
        if let Some(provider) = restoration {
            let current_snapshot = self.replay_from_journal();
            if let Some(restored) = provider
                .build_restoration(&self.conversation, &current_snapshot, cutoff)
                .await?
            {
                self.set_latest_checkpoint_restoration(restored.clone());
                if let Some(last) = self.checkpoints.last_mut() {
                    last.restoration = Some(restored.clone());
                    checkpoint = last.clone();
                } else {
                    checkpoint.restoration = Some(restored);
                }
            }
        }
        let post_tokens = Self::prompt_token_count(&self.current_prompt_projection());
        if let Some(last) = self.checkpoints.last_mut() {
            last.compact_metadata.post_tokens = post_tokens;
            checkpoint = last.clone();
        }
        self.record_compaction_boundary(CompactionBoundary::Autocompact {
            turn: upcoming_turn,
            trigger,
            pre_tokens: token_count,
            post_tokens,
            strategy: CompactionStrategy::ModelDriven,
        });
        let post_compact = self
            .dispatch_hook(
                HookEventName::PostCompact,
                None,
                json!({
                    "turn": upcoming_turn,
                    "pre_tokens": token_count,
                    "post_tokens": post_tokens,
                    "strategy": "model_driven",
                    "summary": checkpoint.prompt_summary.content,
                }),
            )
            .await?;
        if !post_compact.additional_contexts.is_empty() {
            self.inject_hook_contexts(
                HookEventName::PostCompact,
                &post_compact.additional_contexts,
            );
        }
        let trace = CheckpointSnapshot {
            compacted_until_offset: checkpoint.compacted_until_offset,
            journal_digest: checkpoint.journal_digest.clone(),
            prompt_window_id: checkpoint.prompt_window_id.clone(),
            prompt_window_generation: checkpoint.prompt_window_generation,
            prompt_window_started_after_offset: checkpoint.prompt_window_started_after_offset,
            summary_digest: Self::digest_of(&checkpoint.prompt_summary)?,
            summary_text: checkpoint.prompt_summary.content.clone(),
            canonical_state_digest: Self::digest_canonical_state(&checkpoint.canonical)?,
        };

        Ok(Some(trace))
    }

    async fn generate_compaction_summary<M>(
        &self,
        cutoff: u64,
        upcoming_turn: usize,
        model: &M,
    ) -> Result<SummaryBlock>
    where
        M: ModelDriver + ?Sized,
    {
        let (summary, mut groups) = self.compaction_source_groups(cutoff);

        for _attempt in 0..=MAX_COMPACTION_PTL_RETRIES {
            let (prompt, provider_prompt) =
                self.compaction_prompt_projection(summary.clone(), &groups, upcoming_turn);
            match model
                .next_turn(ModelRequest {
                    kind: ModelRequestKind::Compaction,
                    conversation: self.conversation.clone(),
                    turn: upcoming_turn,
                    prompt,
                    provider_prompt,
                    available_tools: Vec::new(),
                    generation: Self::compaction_generation(),
                })
                .await
            {
                Ok(model_turn) => {
                    if !model_turn.tool_calls.is_empty() {
                        bail!("compaction model attempted tool use");
                    }
                    let content = format_compaction_summary(&model_turn.assistant_message.content);
                    if content.trim().is_empty() {
                        bail!("compaction model returned an empty summary");
                    }

                    return Ok(SummaryBlock {
                        title: "compacted_history".to_string(),
                        content,
                    });
                }
                Err(error) if Self::is_prompt_too_long_error(&error) => {
                    let gap_hint = Self::prompt_too_long_token_gap(&error);
                    let Some(truncated) = Self::truncate_compaction_groups(&groups, gap_hint)
                    else {
                        return Err(error);
                    };
                    groups = truncated;
                }
                Err(error) => return Err(error),
            }
        }

        bail!("compaction model exhausted prompt-too-long retries")
    }

    fn compaction_source_groups(&self, cutoff: u64) -> (Option<SummaryBlock>, Vec<Vec<LogEntry>>) {
        let checkpoint = self.checkpoints.last();
        let last_compacted_offset = checkpoint.map(|value| value.compacted_until_offset);
        let summary = checkpoint.map(|value| value.prompt_summary.clone());
        let mut groups = Vec::new();
        let mut current_group = Vec::new();
        let mut last_assistant_id: Option<String> = None;

        for entry in self.journal.iter().filter(|entry| {
            entry.offset <= cutoff
                && last_compacted_offset
                    .map(|offset| entry.offset > offset)
                    .unwrap_or(true)
        }) {
            let starts_new_assistant_round = matches!(
                &entry.event,
                SessionEvent::MessageAppended { message }
                    if message.role == Role::Assistant
                        && last_assistant_id.as_ref() != Some(&message.id)
                        && !current_group.is_empty()
            );
            if starts_new_assistant_round {
                groups.push(std::mem::take(&mut current_group));
            }
            if let SessionEvent::MessageAppended { message } = &entry.event {
                if message.role == Role::Assistant {
                    last_assistant_id = Some(message.id.clone());
                }
            }
            if !matches!(
                entry.event,
                SessionEvent::InputReceived { .. } | SessionEvent::CompactionBoundary { .. }
            ) {
                current_group.push(entry.clone());
            }
        }
        if !current_group.is_empty() {
            groups.push(current_group);
        }

        (summary, groups)
    }

    fn compaction_prompt_projection(
        &self,
        summary: Option<SummaryBlock>,
        groups: &[Vec<LogEntry>],
        upcoming_turn: usize,
    ) -> (PromptProjection, ProviderPrompt) {
        let mut messages = Vec::new();
        let mut items = Vec::new();
        let mut input_content_parts =
            std::collections::BTreeMap::<String, Vec<kheish_types::InputContentPart>>::new();

        if let Some(summary) = summary.clone() {
            items.push(ProviderInputItem::Summary { summary });
        }

        let mut active_assistant_message_id = None;
        for entry in groups.iter().flatten() {
            match &entry.event {
                SessionEvent::InputReceived { input } => {
                    let content_parts = Self::input_content_parts(input);
                    if !content_parts.is_empty() {
                        input_content_parts.insert(
                            Self::user_message_id_for_input_offset(entry.offset),
                            content_parts,
                        );
                    }
                }
                SessionEvent::CompactionBoundary { .. }
                | SessionEvent::UserQuestionRequested { .. }
                | SessionEvent::UserQuestionResolved { .. } => {}
                SessionEvent::MessageAppended { message } => {
                    let mut message = Self::prompt_visible_message(message);
                    message.provider_response_id = None;
                    message.provider_context = None;
                    messages.push(message.clone());
                    if message.role == Role::Tool {
                        continue;
                    }
                    if message.role == Role::Assistant {
                        active_assistant_message_id = Some(message.id.clone());
                    }
                    let user_content_parts = if matches!(message.role, Role::User) {
                        input_content_parts.remove(&message.id).unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    items.push(ProviderInputItem::Message {
                        id: message.id.clone(),
                        role: message.role.clone(),
                        content: message.content.clone(),
                        content_parts: user_content_parts.clone(),
                        attachments: Self::attachments_from_content_parts(&user_content_parts),
                        provider_response_id: message.provider_response_id.clone(),
                        provider_context: message.provider_context.clone(),
                    });
                }
                SessionEvent::ToolCallStarted { call } => {
                    let call = Self::without_provider_resume_tool_call(call);
                    items.push(ProviderInputItem::ToolCall {
                        assistant_message_id: call
                            .assistant_message_id
                            .clone()
                            .or_else(|| active_assistant_message_id.clone()),
                        call: call.clone(),
                    });
                }
                SessionEvent::ToolCallFinished { result } => {
                    let result = Self::prompt_visible_tool_result(result);
                    items.push(ProviderInputItem::ToolResult { result });
                }
            }
        }

        let compaction_request = MessageRecord::new(
            format!("compaction-request-{upcoming_turn}"),
            Role::User,
            build_compaction_user_prompt(summary.is_some()),
        );
        messages.push(compaction_request.clone());
        items.push(ProviderInputItem::Message {
            id: compaction_request.id.clone(),
            role: compaction_request.role,
            content: compaction_request.content.clone(),
            content_parts: Vec::new(),
            attachments: Vec::new(),
            provider_response_id: None,
            provider_context: None,
        });

        let system_section = SystemPromptSection {
            name: "compaction".to_string(),
            content: build_compaction_system_prompt(),
        };
        (
            PromptProjection {
                summary,
                system_sections: vec![system_section.clone()],
                messages,
                open_tool_calls: Vec::new(),
                restoration: None,
            },
            ProviderPrompt {
                instructions: vec![system_section.content],
                force_synthetic_user_prefix: false,
                items,
            },
        )
    }

    fn truncate_compaction_groups(
        groups: &[Vec<LogEntry>],
        gap_hint_tokens: Option<usize>,
    ) -> Option<Vec<Vec<LogEntry>>> {
        if groups.len() < 2 {
            return None;
        }
        let drop_count = if let Some(gap_hint_tokens) = gap_hint_tokens {
            let mut dropped_tokens = 0usize;
            let mut groups_to_drop = 0usize;
            for group in groups {
                if groups_to_drop >= groups.len() - 1 {
                    break;
                }
                dropped_tokens = dropped_tokens.saturating_add(Self::estimated_group_tokens(group));
                groups_to_drop += 1;
                if dropped_tokens >= gap_hint_tokens {
                    break;
                }
            }
            groups_to_drop.max(1)
        } else {
            std::cmp::max(1, groups.len() / 5)
        }
        .min(groups.len() - 1);
        Some(groups[drop_count..].to_vec())
    }

    fn is_prompt_too_long_error(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| {
            if let Some(provider_error) = cause.downcast_ref::<ModelProviderError>() {
                return provider_error.kind == ProviderErrorKind::ContextWindowExceeded;
            }
            let message = cause.to_string().to_ascii_lowercase();
            message.contains("context_length_exceeded")
                || message.contains("prompt too long")
                || message.contains("prompt is too long")
                || message.contains("maximum context length")
                || message.contains("context length")
                || message.contains("too many tokens")
                || (message.contains("status 413")
                    && !message.contains("attachment")
                    && !message.contains("image")
                    && !message.contains("pdf")
                    && !message.contains("media"))
                || (message.contains("http error 413")
                    && !message.contains("attachment")
                    && !message.contains("image")
                    && !message.contains("pdf")
                    && !message.contains("media"))
        })
    }

    fn prompt_too_long_token_gap(error: &anyhow::Error) -> Option<usize> {
        error.chain().find_map(|cause| {
            let message = cause.to_string().to_ascii_lowercase();
            if !message.contains("prompt is too long") && !message.contains("prompt too long") {
                return None;
            }
            let numbers = message
                .split(|ch: char| !ch.is_ascii_digit())
                .filter(|part| !part.is_empty())
                .filter_map(|part| part.parse::<usize>().ok())
                .collect::<Vec<_>>();
            if numbers.len() < 2 {
                return None;
            }
            numbers[0].checked_sub(numbers[1]).filter(|gap| *gap > 0)
        })
    }

    fn is_rate_limit_error(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| {
            let message = cause.to_string().to_ascii_lowercase();
            message.contains("rate limit")
                || message.contains("too many requests")
                || message.contains("status 429")
                || message.contains("http error 429")
        })
    }

    fn is_provider_unavailable_error(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| {
            let message = cause.to_string().to_ascii_lowercase();
            message.contains("status 503")
                || message.contains("status 529")
                || message.contains("service unavailable")
                || message.contains("server overloaded")
                || message.contains("temporarily unavailable")
        })
    }

    fn fallback_model_generation(
        base: &ModelGenerationConfig,
        current: &ModelGenerationConfig,
    ) -> Option<ModelGenerationConfig> {
        let fallback_model = base.fallback_model.as_ref()?;
        if current.model.as_deref() == Some(fallback_model.as_str()) {
            return None;
        }
        let mut generation = current.clone();
        generation.model = Some(fallback_model.clone());
        Some(generation)
    }

    fn aggressive_snip_workspace(
        &mut self,
        upcoming_turn: usize,
        generation: &ModelGenerationConfig,
    ) -> Option<PromptWorkspace> {
        let target_budget = self.effective_autocompact_threshold_tokens(generation);
        let mut workspace = self.build_prompt_workspace();
        let result = snip_if_needed(
            &workspace.prompt.messages,
            target_budget,
            self.policy.snip_keep_minimum,
        );
        if result.messages_removed == 0 {
            return None;
        }
        self.apply_snip_to_workspace(&mut workspace, result.new_head_offset);
        self.record_compaction_boundary(CompactionBoundary::Snip {
            turn: upcoming_turn,
            messages_removed: result.messages_removed,
            tokens_freed: result.tokens_freed,
            new_head_offset: result.new_head_offset,
        });
        Some(workspace)
    }

    fn estimated_group_tokens(group: &[LogEntry]) -> usize {
        group
            .iter()
            .map(|entry| match &entry.event {
                SessionEvent::InputReceived { input } => serde_json::to_value(input)
                    .map(|value| rough_token_estimate_value(&value))
                    .unwrap_or_default(),
                SessionEvent::MessageAppended { message } => {
                    crate::tokens::rough_token_estimate_message(message)
                }
                SessionEvent::ToolCallStarted { call } => serde_json::to_value(call)
                    .map(|value| rough_token_estimate_value(&value))
                    .unwrap_or_default(),
                SessionEvent::ToolCallFinished { result } => serde_json::to_value(result)
                    .map(|value| rough_token_estimate_value(&value))
                    .unwrap_or_default(),
                SessionEvent::UserQuestionRequested { request } => serde_json::to_value(request)
                    .map(|value| rough_token_estimate_value(&value))
                    .unwrap_or_default(),
                SessionEvent::UserQuestionResolved { resolution } => {
                    serde_json::to_value(resolution)
                        .map(|value| rough_token_estimate_value(&value))
                        .unwrap_or_default()
                }
                SessionEvent::CompactionBoundary { boundary } => serde_json::to_value(boundary)
                    .map(|value| rough_token_estimate_value(&value))
                    .unwrap_or_default(),
            })
            .sum()
    }

    fn compaction_generation() -> ModelGenerationConfig {
        ModelGenerationConfig {
            model: None,
            fallback_model: None,
            tool_choice: ToolChoice::None,
            allow_parallel_tool_calls: false,
            max_output_tokens: Some(4_096),
            temperature: Some(0.0),
            reasoning: None,
            response_format: kheish_types::ResponseFormat::Text,
        }
    }

    fn offset_for_message_index(&self, index: usize) -> Option<u64> {
        let mut current = 0usize;
        for entry in &self.journal {
            if matches!(entry.event, SessionEvent::MessageAppended { .. }) {
                if current == index {
                    return Some(entry.offset);
                }
                current += 1;
            }
        }
        None
    }

    fn adjust_compaction_cutoff(&self, initial_cutoff: u64) -> Result<Option<u64>> {
        let split_tool_call_ids = self
            .journal
            .iter()
            .filter(|entry| entry.offset > initial_cutoff)
            .filter_map(|entry| match &entry.event {
                SessionEvent::ToolCallFinished { result } => Some(result.call_id.clone()),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        if split_tool_call_ids.is_empty() {
            return Ok(Some(initial_cutoff));
        }

        let assistant_message_offsets = self
            .journal
            .iter()
            .filter_map(|entry| match &entry.event {
                SessionEvent::MessageAppended { message } if message.role == Role::Assistant => {
                    Some((message.id.clone(), entry.offset))
                }
                _ => None,
            })
            .collect::<std::collections::BTreeMap<_, _>>();

        let required_assistant_offset = self
            .journal
            .iter()
            .filter(|entry| entry.offset <= initial_cutoff)
            .filter_map(|entry| match &entry.event {
                SessionEvent::ToolCallStarted { call }
                    if split_tool_call_ids.contains(&call.id) =>
                {
                    call.assistant_message_id
                        .as_ref()
                        .and_then(|message_id| assistant_message_offsets.get(message_id).copied())
                }
                _ => None,
            })
            .min();

        let Some(required_assistant_offset) = required_assistant_offset else {
            return Ok(Some(initial_cutoff));
        };
        if required_assistant_offset == 0 {
            return Ok(None);
        }

        Ok(Some(initial_cutoff.min(required_assistant_offset - 1)))
    }

    fn preserved_segment_after(&self, cutoff: u64) -> Option<PreservedSegment> {
        let preserved_messages = self
            .journal
            .iter()
            .filter(|entry| entry.offset > cutoff)
            .filter_map(|entry| match &entry.event {
                SessionEvent::MessageAppended { message } => Some(message),
                _ => None,
            })
            .collect::<Vec<_>>();
        let head = preserved_messages.first()?;
        let tail = preserved_messages.last()?;
        let anchor_uuid = self
            .journal
            .iter()
            .rev()
            .find_map(|entry| match &entry.event {
                SessionEvent::MessageAppended { message } if entry.offset <= cutoff => {
                    Some(message.id.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| head.id.clone());
        Some(PreservedSegment {
            head_uuid: head.id.clone(),
            anchor_uuid,
            tail_uuid: tail.id.clone(),
        })
    }

    fn digest_until_offset(&self, offset: u64) -> Result<String> {
        Self::digest_log_entries(self.journal.iter().filter(|entry| entry.offset <= offset))
    }

    fn digest_log_entries<'a>(entries: impl IntoIterator<Item = &'a LogEntry>) -> Result<String> {
        let entries = entries
            .into_iter()
            .map(Self::stable_journal_entry_value)
            .collect::<Vec<_>>();
        digest_json_value(&Value::Array(entries))
    }

    fn stable_journal_entry_value(entry: &LogEntry) -> serde_json::Value {
        json!({
            "offset": entry.offset,
            "event": Self::stable_session_event_value(&entry.event),
        })
    }

    fn stable_session_event_value(event: &SessionEvent) -> serde_json::Value {
        match event {
            SessionEvent::InputReceived { input } => json!({
                "type": "input_received",
                "input": input,
            }),
            SessionEvent::MessageAppended { message } => json!({
                "type": "message_appended",
                "message": {
                    "id": message.id,
                    "role": message.role,
                    "content": message.content,
                    "pinned": message.pinned,
                    "api_usage": message.api_usage,
                },
            }),
            SessionEvent::ToolCallStarted { call } => json!({
                "type": "tool_call_started",
                "call": call,
            }),
            SessionEvent::ToolCallFinished { result } => json!({
                "type": "tool_call_finished",
                "result": Self::tool_result_canonical_json(result),
            }),
            SessionEvent::UserQuestionRequested { request } => json!({
                "type": "user_question_requested",
                "request": request,
            }),
            SessionEvent::UserQuestionResolved { resolution } => json!({
                "type": "user_question_resolved",
                "resolution": resolution,
            }),
            SessionEvent::CompactionBoundary { boundary } => json!({
                "type": "compaction_boundary",
                "boundary": boundary,
            }),
        }
    }

    fn digest_of<T: Serialize>(value: &T) -> Result<String> {
        let value = serde_json::to_value(value)?;
        digest_serialize(&value)
    }

    fn digest_message_record(message: &MessageRecord) -> Result<String> {
        Self::digest_of(&json!({
            "id": message.id,
            "role": message.role,
            "content": message.content,
            "pinned": message.pinned,
            "api_usage": message.api_usage,
        }))
    }

    fn digest_canonical_state(snapshot: &CanonicalStateSnapshot) -> Result<String> {
        Self::digest_of(&json!({
            "messages": snapshot.messages.iter().map(|message| json!({
                "id": message.id,
                "role": message.role,
                "content": message.content,
                "pinned": message.pinned,
                "api_usage": message.api_usage,
            })).collect::<Vec<_>>(),
            "input_content_parts": snapshot.input_content_parts,
            "open_tool_calls": snapshot.open_tool_calls,
            "completed_tool_results": snapshot
                .completed_tool_results
                .iter()
                .map(Self::tool_result_canonical_json)
                .collect::<Vec<_>>(),
        }))
    }

    fn input_content_parts(input: &InputEnvelope) -> Vec<kheish_types::InputContentPart> {
        match &input.payload {
            InputPayload::Rich { items, .. } => items.clone(),
            InputPayload::Text { content } => {
                let mut parts = Vec::new();
                if !content.trim().is_empty() {
                    parts.push(kheish_types::InputContentPart::Text {
                        text: content.clone(),
                    });
                }
                parts.extend(
                    input.attachments.iter().cloned().map(|attachment| {
                        kheish_types::InputContentPart::Attachment { attachment }
                    }),
                );
                parts
            }
            _ => Vec::new(),
        }
    }

    fn attachments_from_content_parts(
        parts: &[kheish_types::InputContentPart],
    ) -> Vec<kheish_types::AttachmentRef> {
        parts
            .iter()
            .filter_map(|part| match part {
                kheish_types::InputContentPart::Attachment { attachment } => {
                    Some(attachment.clone())
                }
                kheish_types::InputContentPart::Text { .. } => None,
            })
            .collect()
    }

    fn digest_tool_result_record(result: &ToolResultRecord) -> Result<String> {
        Self::digest_of(&Self::tool_result_canonical_json(result))
    }

    fn tool_result_canonical_json(result: &ToolResultRecord) -> serde_json::Value {
        json!({
            "call_id": result.call_id,
            "output": result.output,
            "is_error": result.is_error,
            "tool_name": result.tool_name,
            "context_updates": result.context_updates,
        })
    }

    fn digest_message_content(message: &MessageRecord) -> Result<String> {
        if message.role == Role::Tool {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&message.content) {
                return digest_json_value(&parsed);
            }
        }

        Ok(digest_text(&message.content))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::VecDeque;
    use std::sync::Arc;

    use kheish_types::{
        ApprovalRequest, ContextUpdate, HookPermissionUpdateBehavior, HookPermissionUpdateScope,
    };
    use parking_lot::Mutex;
    use serde_json::json;

    use super::*;

    struct ScriptedModel {
        turns: Mutex<VecDeque<ModelTurn>>,
        requests: Mutex<Vec<ModelRequest>>,
    }

    impl ScriptedModel {
        fn new(turns: Vec<ModelTurn>) -> Self {
            Self {
                turns: Mutex::new(VecDeque::from(turns)),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<ModelRequest> {
            self.requests.lock().clone()
        }
    }

    #[async_trait]
    impl ModelDriver for ScriptedModel {
        async fn next_turn(&self, request: ModelRequest) -> Result<ModelTurn> {
            self.requests.lock().push(request);
            self.turns
                .lock()
                .pop_front()
                .ok_or_else(|| anyhow!("no scripted turn remaining"))
        }
    }

    struct ScriptedResultModel {
        turns: Mutex<VecDeque<Result<ModelTurn>>>,
        requests: Mutex<Vec<ModelRequest>>,
    }

    impl ScriptedResultModel {
        fn new(turns: Vec<Result<ModelTurn>>) -> Self {
            Self {
                turns: Mutex::new(VecDeque::from(turns)),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<ModelRequest> {
            self.requests.lock().clone()
        }
    }

    #[async_trait]
    impl ModelDriver for ScriptedResultModel {
        async fn next_turn(&self, request: ModelRequest) -> Result<ModelTurn> {
            self.requests.lock().push(request);
            self.turns
                .lock()
                .pop_front()
                .ok_or_else(|| anyhow!("no scripted turn remaining"))?
        }
    }

    #[derive(Default)]
    struct RecordingJournalSink {
        batches: Mutex<Vec<Vec<LogEntry>>>,
    }

    #[async_trait]
    impl JournalSink for RecordingJournalSink {
        async fn persist_entries(
            &self,
            _conversation: &ConversationKey,
            entries: &[LogEntry],
        ) -> Result<()> {
            self.batches.lock().push(entries.to_vec());
            Ok(())
        }
    }

    struct FailingJournalSink;

    #[async_trait]
    impl JournalSink for FailingJournalSink {
        async fn persist_entries(
            &self,
            _conversation: &ConversationKey,
            _entries: &[LogEntry],
        ) -> Result<()> {
            bail!("scripted persistence failure")
        }
    }

    struct StaticRestorationProvider {
        restoration: PostCompactRestoration,
    }

    #[async_trait]
    impl PostCompactRestorationProvider for StaticRestorationProvider {
        async fn build_restoration(
            &self,
            _conversation: &ConversationKey,
            _snapshot: &CanonicalStateSnapshot,
            _compacted_until_offset: u64,
        ) -> Result<Option<PostCompactRestoration>> {
            Ok(Some(self.restoration.clone()))
        }
    }

    struct EchoToolExecutor;

    impl ToolCatalog for EchoToolExecutor {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![
                ToolDefinition {
                    name: "lookup_context".to_string(),
                    description: "Returns test context".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "query": {"type": "string"}
                        },
                        "required": ["query"],
                        "additionalProperties": false
                    }),
                    allows_parallel: true,
                },
                ToolDefinition {
                    name: "write_file".to_string(),
                    description: "Writes one file inside the workspace.".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "content": {"type": "string"}
                        },
                        "required": ["path", "content"],
                        "additionalProperties": false
                    }),
                    allows_parallel: false,
                },
            ]
        }
    }

    #[async_trait]
    impl ToolExecutor for EchoToolExecutor {
        async fn execute(&self, call: &ToolCallRecord) -> Result<ToolResultRecord> {
            Ok(ToolResultRecord {
                call_id: call.id.clone(),
                output: json!({
                    "tool": call.name,
                    "received": call.input,
                }),
                is_error: false,
                tool_name: Some(call.name.clone()),
                offset: None,
                timestamp_ms: None,
                context_updates: Vec::new(),
                hook_contexts: Vec::new(),
            })
        }
    }

    struct SelectivePermissionGate;

    #[async_trait]
    impl PermissionGate for SelectivePermissionGate {
        async fn check(&self, call: &ToolCallRecord) -> Result<PermissionDecision> {
            if call.name == "delete_file" {
                Ok(PermissionDecision::Deny {
                    reason: "delete_file is disabled in tests".to_string(),
                })
            } else {
                Ok(PermissionDecision::Allow)
            }
        }
    }

    struct AlwaysDenyPermissionGate;

    #[async_trait]
    impl PermissionGate for AlwaysDenyPermissionGate {
        async fn check(&self, call: &ToolCallRecord) -> Result<PermissionDecision> {
            Ok(PermissionDecision::Deny {
                reason: format!("{} denied in tests", call.name),
            })
        }
    }

    struct RecordingPermissionGate {
        allow_patterns: Mutex<Vec<String>>,
        updates: Mutex<Vec<HookPermissionUpdate>>,
        final_decisions: Mutex<Vec<(String, PermissionDecision, Option<String>)>>,
    }

    impl RecordingPermissionGate {
        fn new() -> Self {
            Self {
                allow_patterns: Mutex::new(Vec::new()),
                updates: Mutex::new(Vec::new()),
                final_decisions: Mutex::new(Vec::new()),
            }
        }

        fn updates(&self) -> Vec<HookPermissionUpdate> {
            self.updates.lock().clone()
        }

        fn final_decisions(&self) -> Vec<(String, PermissionDecision, Option<String>)> {
            self.final_decisions.lock().clone()
        }
    }

    #[async_trait]
    impl PermissionGate for RecordingPermissionGate {
        async fn check(&self, call: &ToolCallRecord) -> Result<PermissionDecision> {
            let allow_patterns = self.allow_patterns.lock().clone();
            if allow_patterns.iter().any(|pattern| pattern == &call.name) {
                return Ok(PermissionDecision::Allow);
            }
            Ok(PermissionDecision::Ask {
                request: ApprovalRequest {
                    id: format!("approval-{}", call.id),
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    input: call.input.clone(),
                    scope: "session".to_string(),
                    reason: "approval required in tests".to_string(),
                },
            })
        }

        async fn apply_hook_permission_updates(
            &self,
            _session_id: &str,
            updates: &[HookPermissionUpdate],
        ) -> Result<()> {
            self.updates.lock().extend_from_slice(updates);
            let mut allow_patterns = self.allow_patterns.lock();
            for update in updates {
                if matches!(update.behavior, HookPermissionUpdateBehavior::Allow) {
                    allow_patterns.push(update.tool_name_pattern.clone());
                }
            }
            Ok(())
        }

        async fn record_final_decision(
            &self,
            call: &ToolCallRecord,
            decision: &PermissionDecision,
            reason: Option<String>,
        ) -> Result<()> {
            self.final_decisions
                .lock()
                .push((call.id.clone(), decision.clone(), reason));
            Ok(())
        }
    }

    struct ScriptedHookDispatcher {
        outcomes: Mutex<BTreeMap<HookEventName, VecDeque<HookDispatchOutcome>>>,
        invocations: Mutex<Vec<HookInvocation>>,
    }

    impl ScriptedHookDispatcher {
        fn new(entries: Vec<(HookEventName, Vec<HookDispatchOutcome>)>) -> Self {
            Self {
                outcomes: Mutex::new(
                    entries
                        .into_iter()
                        .map(|(event, outcomes)| (event, VecDeque::from(outcomes)))
                        .collect(),
                ),
                invocations: Mutex::new(Vec::new()),
            }
        }

        fn invocations(&self) -> Vec<HookInvocation> {
            self.invocations.lock().clone()
        }
    }

    #[async_trait]
    impl HookDispatcher for ScriptedHookDispatcher {
        async fn dispatch(&self, invocation: HookInvocation) -> Result<HookDispatchOutcome> {
            self.invocations.lock().push(invocation.clone());
            Ok(self
                .outcomes
                .lock()
                .get_mut(&invocation.event)
                .and_then(|queue| queue.pop_front())
                .unwrap_or_default())
        }
    }

    #[tokio::test]
    async fn compaction_checkpoint_replays_to_the_same_canonical_state() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-1".to_string(),
            thread_id: Some("thread-a".to_string()),
        };
        let policy = LoopPolicy {
            max_turns: 4,
            keep_last_messages: 1,
            autocompact_threshold_tokens: 20,
            ..LoopPolicy::default()
        };
        let mut engine = AgentEngine::new(conversation, policy);

        let model = Arc::new(ScriptedModel::new(vec![
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-1",
                    Role::Assistant,
                    "Je vais appeler un tool pour enrichir le contexte avant de repondre.",
                ),
                tool_calls: vec![ToolCallRecord {
                    id: "call-1".to_string(),
                    name: "lookup_context".to_string(),
                    input: json!({"query": "important state"}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                }],
                finish_reason: ModelFinishReason::ToolCalls,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "compaction-1",
                    Role::Assistant,
                    r#"<analysis>done</analysis><summary>
1. Primary Request and Intent
- The user wants the task completed.
2. Current Work
- Waiting to continue after the tool result.
</summary>"#,
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-2",
                    Role::Assistant,
                    "Voila la reponse finale avec le contexte complete.",
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            },
        ]));

        let input = InputEnvelope::text(
            "memory",
            "test",
            "session-1",
            "user-1",
            "Bonjour, voici un contexte suffisamment long pour declencher une compaction dans la boucle agentique.",
        );
        let outcome = engine
            .run_input(input, model.as_ref(), &EchoToolExecutor)
            .await?;

        assert_eq!(outcome.turns, 2);
        assert_eq!(outcome.final_message_id, "assistant-2");
        assert_eq!(outcome.trace.turns.len(), 2);
        assert!(!engine.checkpoints().is_empty());
        assert!(!outcome.trace.checkpoints.is_empty());

        let from_journal = engine.replay_from_journal();
        let from_checkpoint = engine
            .replay_from_latest_checkpoint()
            .expect("checkpoint should exist");
        assert_eq!(from_journal, from_checkpoint);

        let requests = model.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].kind, ModelRequestKind::MainLoop);
        assert_eq!(requests[1].kind, ModelRequestKind::Compaction);
        assert_eq!(requests[2].kind, ModelRequestKind::MainLoop);
        assert_eq!(requests[1].available_tools.len(), 0);
        assert_eq!(requests[1].generation.tool_choice, ToolChoice::None);
        assert!(requests[2].prompt.summary.is_some());
        assert!(outcome.trace.turns[1].prompt.has_summary);

        let current_prompt = engine.current_prompt_projection();
        assert!(current_prompt.summary.is_some());
        assert!(current_prompt.messages.len() < from_journal.messages.len());

        Ok(())
    }

    #[tokio::test]
    async fn tool_calls_are_recorded_and_closed_in_the_canonical_state() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-2".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 2,
                keep_last_messages: 3,
                ..LoopPolicy::default()
            },
        );

        let model = ScriptedModel::new(vec![
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-tool",
                    Role::Assistant,
                    "Je dois utiliser un tool.",
                ),
                tool_calls: vec![ToolCallRecord {
                    id: "call-tool".to_string(),
                    name: "fetch_state".to_string(),
                    input: json!({"scope": "session"}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                }],
                finish_reason: ModelFinishReason::ToolCalls,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-final",
                    Role::Assistant,
                    "Le tool a repondu, je peux conclure.",
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            },
        ]);

        let input = InputEnvelope::text("memory", "test", "session-2", "user-1", "start");
        let outcome = engine.run_input(input, &model, &EchoToolExecutor).await?;

        let state = engine.replay_from_journal();
        assert!(state.open_tool_calls.is_empty());
        assert_eq!(state.completed_tool_results.len(), 1);
        assert_eq!(outcome.trace.turns[0].tool_executions.len(), 1);
        assert!(
            state
                .messages
                .iter()
                .any(|message| message.role == Role::Tool)
        );

        Ok(())
    }

    #[tokio::test]
    async fn journal_sink_makes_in_flight_entries_durable_at_turn_boundaries() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-journal-sink".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 4,
                ..LoopPolicy::default()
            },
        );
        let sink = Arc::new(RecordingJournalSink::default());
        engine.set_journal_sink(Some(sink.clone()));

        let model = ScriptedModel::new(vec![
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-tool",
                    Role::Assistant,
                    "Inspecting.",
                ),
                tool_calls: vec![ToolCallRecord {
                    id: "call-tool".to_string(),
                    name: "lookup_context".to_string(),
                    input: json!({"query": "one"}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                }],
                finish_reason: ModelFinishReason::ToolCalls,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new("assistant-final", Role::Assistant, "Done."),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            },
        ]);

        let input =
            InputEnvelope::text("memory", "test", "session-journal-sink", "user-1", "start");
        let outcome = engine.run_input(input, &model, &EchoToolExecutor).await?;
        assert!(matches!(outcome.status, RunStatus::Completed));

        let batches = sink.batches.lock().clone();
        assert!(
            batches.len() >= 3,
            "expected flushes at the input, tool-start, and tool-finish boundaries, got {}",
            batches.len()
        );
        let flushed = batches.iter().flatten().collect::<Vec<_>>();
        // Every entry flushed exactly once, in order.
        for pair in flushed.windows(2) {
            assert!(
                pair[0].offset < pair[1].offset,
                "flushed offsets must be strictly increasing"
            );
        }
        assert_eq!(
            flushed.len(),
            engine.journal().len(),
            "every journal entry must be durable by the end of the run"
        );
        assert_eq!(engine.journal_flushed_len(), engine.journal().len());
        // The input is durable before the first assistant message, and the
        // tool start is durable in an earlier batch than its result.
        assert!(matches!(
            batches[0][0].event,
            SessionEvent::InputReceived { .. }
        ));
        let batch_of = |predicate: fn(&SessionEvent) -> bool| {
            batches
                .iter()
                .position(|batch| batch.iter().any(|entry| predicate(&entry.event)))
                .expect("event should be flushed")
        };
        let started_batch = batch_of(|event| matches!(event, SessionEvent::ToolCallStarted { .. }));
        let finished_batch =
            batch_of(|event| matches!(event, SessionEvent::ToolCallFinished { .. }));
        assert!(
            started_batch < finished_batch,
            "tool start must be durable before its result"
        );
        Ok(())
    }

    #[tokio::test]
    async fn journal_sink_failure_fails_the_run_cleanly() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-journal-sink-failure".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 2,
                ..LoopPolicy::default()
            },
        );
        engine.set_journal_sink(Some(Arc::new(FailingJournalSink)));

        let model = ScriptedModel::new(vec![ModelTurn {
            assistant_message: MessageRecord::new("assistant-final", Role::Assistant, "Done."),
            tool_calls: Vec::new(),
            finish_reason: ModelFinishReason::Completed,
            usage: None,
        }]);

        let input = InputEnvelope::text(
            "memory",
            "test",
            "session-journal-sink-failure",
            "user-1",
            "start",
        );
        let error = engine
            .run_input(input, &model, &EchoToolExecutor)
            .await
            .expect_err("a persistence failure must fail the run");
        assert!(
            format!("{error:#}").contains("failed to persist in-flight journal entries"),
            "unexpected error: {error:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn zero_max_turns_allows_an_unbounded_agent_loop() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-unbounded".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 0,
                keep_last_messages: 3,
                ..LoopPolicy::default()
            },
        );

        let model = ScriptedModel::new(vec![
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-tool-1",
                    Role::Assistant,
                    "Je dois inspecter une premiere chose.",
                ),
                tool_calls: vec![ToolCallRecord {
                    id: "call-tool-1".to_string(),
                    name: "lookup_context".to_string(),
                    input: json!({"query": "one"}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                }],
                finish_reason: ModelFinishReason::ToolCalls,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-tool-2",
                    Role::Assistant,
                    "Je continue avec une deuxieme inspection.",
                ),
                tool_calls: vec![ToolCallRecord {
                    id: "call-tool-2".to_string(),
                    name: "lookup_context".to_string(),
                    input: json!({"query": "two"}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                }],
                finish_reason: ModelFinishReason::ToolCalls,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-tool-3",
                    Role::Assistant,
                    "Je termine la verification.",
                ),
                tool_calls: vec![ToolCallRecord {
                    id: "call-tool-3".to_string(),
                    name: "lookup_context".to_string(),
                    input: json!({"query": "three"}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                }],
                finish_reason: ModelFinishReason::ToolCalls,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-final-unbounded",
                    Role::Assistant,
                    "J'ai fini.",
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            },
        ]);

        let input = InputEnvelope::text("memory", "test", "session-unbounded", "user-1", "start");
        let outcome = engine.run_input(input, &model, &EchoToolExecutor).await?;

        assert_eq!(outcome.turns, 4);
        assert!(matches!(outcome.status, RunStatus::Completed));
        assert_eq!(outcome.trace.turns.len(), 4);
        assert_eq!(
            outcome.snapshot.run_meta.policy.max_turns, 0,
            "zero is the public sentinel for an unbounded turn policy"
        );

        Ok(())
    }

    #[tokio::test]
    async fn denied_tool_calls_are_observable_in_trace_and_state() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-3".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 2,
                keep_last_messages: 3,
                ..LoopPolicy::default()
            },
        );

        let model = ScriptedModel::new(vec![
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-deny-1",
                    Role::Assistant,
                    "Je tente une action sensible.",
                ),
                tool_calls: vec![ToolCallRecord {
                    id: "call-denied".to_string(),
                    name: "delete_file".to_string(),
                    input: json!({"path": "/tmp/file.txt"}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                }],
                finish_reason: ModelFinishReason::ToolCalls,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-deny-2",
                    Role::Assistant,
                    "Je ne peux pas executer cette action.",
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            },
        ]);

        let input = InputEnvelope::text("memory", "test", "session-3", "user-1", "delete");
        let outcome = engine
            .run_input_with_permissions(input, &model, &EchoToolExecutor, &SelectivePermissionGate)
            .await?;

        let tool_trace = &outcome.trace.turns[0].tool_executions[0];
        assert_eq!(tool_trace.call_id, "call-denied");
        assert!(tool_trace.result_is_error);
        assert_eq!(
            tool_trace.decision,
            PermissionDecision::Deny {
                reason: "delete_file is disabled in tests".to_string(),
            }
        );

        let state = engine.replay_from_journal();
        let denied_result = state
            .completed_tool_results
            .iter()
            .find(|result| result.call_id == "call-denied")
            .expect("denied result should exist");
        assert!(denied_result.is_error);
        assert_eq!(
            denied_result.output["permission_denied"],
            serde_json::Value::Bool(true)
        );

        Ok(())
    }

    #[tokio::test]
    async fn permission_request_hooks_can_apply_permission_updates_and_continue_without_approval()
    -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-hook-allow".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(conversation, LoopPolicy::default());
        let hooks = Arc::new(ScriptedHookDispatcher::new(vec![(
            HookEventName::PermissionRequest,
            vec![HookDispatchOutcome {
                permission: Some(HookPermissionBehavior::Allow),
                updated_permissions: vec![HookPermissionUpdate {
                    scope: HookPermissionUpdateScope::Session,
                    tool_name_pattern: "write_file".to_string(),
                    behavior: HookPermissionUpdateBehavior::Allow,
                    reason: Some("trusted by hook".to_string()),
                }],
                additional_contexts: vec!["permission hook ran".to_string()],
                ..HookDispatchOutcome::default()
            }],
        )]));
        engine.set_hook_dispatcher(Some(hooks.clone()));

        let model = ScriptedModel::new(vec![
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-hook-allow-1",
                    Role::Assistant,
                    "Writing the file now.",
                ),
                tool_calls: vec![ToolCallRecord {
                    id: "call-hook-allow".to_string(),
                    name: "write_file".to_string(),
                    input: json!({
                        "path": "reports/hook.txt",
                        "content": "hook body",
                    }),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                }],
                finish_reason: ModelFinishReason::ToolCalls,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-hook-allow-2",
                    Role::Assistant,
                    "Done.",
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            },
        ]);
        let permissions = RecordingPermissionGate::new();

        let outcome = engine
            .run_input_with_permissions(
                InputEnvelope::text("daemon", "api", "session-hook-allow", "user-1", "write"),
                &model,
                &EchoToolExecutor,
                &permissions,
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(permissions.updates().len(), 1);
        let final_decisions = permissions.final_decisions();
        assert_eq!(final_decisions.len(), 1);
        assert_eq!(final_decisions[0].0, "call-hook-allow");
        assert!(matches!(final_decisions[0].1, PermissionDecision::Allow));
        assert_eq!(
            final_decisions[0].2.as_deref(),
            Some("allowed by permission hook")
        );
        assert!(
            hooks
                .invocations()
                .iter()
                .any(|invocation| invocation.event == HookEventName::PermissionRequest)
        );
        let state = engine.replay_from_journal();
        assert!(
            state
                .completed_tool_results
                .iter()
                .any(|result| result.call_id == "call-hook-allow" && !result.is_error)
        );
        assert!(state.messages.iter().any(|message| {
            message.role == Role::System && message.content.contains("permission hook ran")
        }));
        Ok(())
    }

    #[tokio::test]
    async fn permission_request_hook_ask_overrides_approve_decision() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-hook-ask".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(conversation, LoopPolicy::default());
        let hooks = Arc::new(ScriptedHookDispatcher::new(vec![(
            HookEventName::PermissionRequest,
            vec![HookDispatchOutcome {
                decision: Some(HookDecision::Approve),
                permission: Some(HookPermissionBehavior::Ask),
                ..HookDispatchOutcome::default()
            }],
        )]));
        engine.set_hook_dispatcher(Some(hooks));

        let model = ScriptedModel::new(vec![ModelTurn {
            assistant_message: MessageRecord::new(
                "assistant-hook-ask-1",
                Role::Assistant,
                "Writing the file now.",
            ),
            tool_calls: vec![ToolCallRecord {
                id: "call-hook-ask".to_string(),
                name: "write_file".to_string(),
                input: json!({
                    "path": "reports/hook.txt",
                    "content": "hook body",
                }),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            }],
            finish_reason: ModelFinishReason::ToolCalls,
            usage: None,
        }]);
        let permissions = RecordingPermissionGate::new();

        let outcome = engine
            .run_input_with_permissions(
                InputEnvelope::text("daemon", "api", "session-hook-ask", "user-1", "write"),
                &model,
                &EchoToolExecutor,
                &permissions,
            )
            .await?;

        assert!(matches!(
            outcome.status,
            RunStatus::WaitingForApproval { .. }
        ));
        let final_decisions = permissions.final_decisions();
        assert_eq!(final_decisions.len(), 1);
        assert!(matches!(
            final_decisions[0].1,
            PermissionDecision::Ask { .. }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn permission_denied_hooks_can_request_one_retry_turn() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-hook-retry".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 3,
                ..LoopPolicy::default()
            },
        );
        let hooks = Arc::new(ScriptedHookDispatcher::new(vec![(
            HookEventName::PermissionDenied,
            vec![HookDispatchOutcome {
                retry: true,
                additional_contexts: vec!["permission denied hook ran".to_string()],
                ..HookDispatchOutcome::default()
            }],
        )]));
        engine.set_hook_dispatcher(Some(hooks));

        let model = ScriptedModel::new(vec![
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-hook-retry-1",
                    Role::Assistant,
                    "Trying the blocked action first.",
                ),
                tool_calls: vec![ToolCallRecord {
                    id: "call-hook-retry".to_string(),
                    name: "delete_file".to_string(),
                    input: json!({"path": "reports/hook.txt"}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                }],
                finish_reason: ModelFinishReason::ToolCalls,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-hook-retry-2",
                    Role::Assistant,
                    "I will not retry the blocked tool.",
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            },
        ]);

        let outcome = engine
            .run_input_with_permissions(
                InputEnvelope::text("daemon", "api", "session-hook-retry", "user-1", "retry"),
                &model,
                &EchoToolExecutor,
                &AlwaysDenyPermissionGate,
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Completed);
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]
                .provider_prompt
                .items
                .iter()
                .any(|item| matches!(
                    item,
                    ProviderInputItem::Message { role: Role::User, content, .. }
                        if content.contains("permission-denied hook requested one retry")
                ))
        );
        Ok(())
    }

    #[tokio::test]
    async fn completion_requirements_force_a_follow_up_until_a_file_is_written() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-3b".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 4,
                keep_last_messages: 3,
                ..LoopPolicy::default()
            },
        );

        let model = ScriptedModel::new(vec![
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-narrative",
                    Role::Assistant,
                    "Je crée le fichier maintenant.",
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-write",
                    Role::Assistant,
                    "J'écris le rapport.",
                ),
                tool_calls: vec![ToolCallRecord {
                    id: "call-write".to_string(),
                    name: "write_file".to_string(),
                    input: json!({
                        "path": "reports/summary.txt",
                        "content": "report body",
                    }),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                }],
                finish_reason: ModelFinishReason::ToolCalls,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-done",
                    Role::Assistant,
                    "Rapport écrit.",
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            },
        ]);

        let mut input = InputEnvelope::text(
            "daemon",
            "api",
            "session-3b",
            "user-1",
            "Analyse la machine et mets le rapport dans un fichier du workspace.",
        );
        input.metadata = kheish_types::metadata_with_completion_requirements(
            serde_json::Value::Null,
            &[CompletionRequirement::WorkspaceFile {
                path: Some("reports/summary.txt".to_string()),
            }],
        )?;

        let outcome = engine.run_input(input, &model, &EchoToolExecutor).await?;

        assert_eq!(outcome.status, RunStatus::Completed);
        let state = engine.replay_from_journal();
        assert!(
            state
                .completed_tool_results
                .iter()
                .any(|result| result.call_id == "call-write" && !result.is_error)
        );
        let requests = model.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[1].generation.tool_choice,
            ToolChoice::Specific {
                name: "write_file".to_string(),
            }
        );
        assert!(engine.journal().iter().any(|entry| matches!(
            &entry.event,
            SessionEvent::MessageAppended { message }
                if message.role == Role::User
                    && message.content.contains("The task is not complete yet.")
        )));

        Ok(())
    }

    fn status_contract(max_repair_attempts: Option<u8>) -> kheish_types::StructuredOutputContract {
        kheish_types::StructuredOutputContract {
            schema: kheish_types::StructuredFieldSchema::from_json_schema(&json!({
                "type": "object",
                "properties": {
                    "status": {"type": "string"},
                    "count": {"type": "number"},
                },
                "required": ["status", "count"],
                "additionalProperties": false,
            }))
            .expect("test schema uses the supported subset"),
            max_repair_attempts,
        }
    }

    fn contract_input(session_id: &str) -> Result<InputEnvelope> {
        let mut input = InputEnvelope::text(
            "daemon",
            "api",
            session_id,
            "user-1",
            "Summarize the queue state.",
        );
        input.metadata = kheish_types::metadata_with_structured_output_contract(
            serde_json::Value::Null,
            &status_contract(None),
        )?;
        Ok(input)
    }

    fn text_turn(id: &str, content: &str) -> ModelTurn {
        ModelTurn {
            assistant_message: MessageRecord::new(id, Role::Assistant, content),
            tool_calls: Vec::new(),
            finish_reason: ModelFinishReason::Completed,
            usage: None,
        }
    }

    #[tokio::test]
    async fn output_contract_repairs_a_nonconforming_final_answer() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-oc-1".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 4,
                keep_last_messages: 3,
                ..LoopPolicy::default()
            },
        );
        let model = ScriptedModel::new(vec![
            text_turn("assistant-prose", "Everything looks fine, 3 items pending."),
            text_turn("assistant-json", "{\"status\": \"ok\", \"count\": 3}"),
        ]);

        let outcome = engine
            .run_input(contract_input("session-oc-1")?, &model, &EchoToolExecutor)
            .await?;

        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(
            outcome.structured_output,
            Some(json!({"status": "ok", "count": 3}))
        );
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        // The repair turn withholds tools entirely so the model must answer.
        assert_eq!(requests[1].generation.tool_choice, ToolChoice::None);
        assert!(engine.journal().iter().any(|entry| matches!(
            &entry.event,
            SessionEvent::MessageAppended { message }
                if message.role == Role::User
                    && message.content.contains("Validation failed")
        )));
        Ok(())
    }

    #[tokio::test]
    async fn output_contract_fails_closed_after_exhausted_repairs() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-oc-2".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 6,
                keep_last_messages: 3,
                ..LoopPolicy::default()
            },
        );
        let model = ScriptedModel::new(vec![
            text_turn("assistant-bad-1", "not json"),
            text_turn("assistant-bad-2", "{\"status\": \"ok\"}"),
        ]);
        let mut input = InputEnvelope::text(
            "daemon",
            "api",
            "session-oc-2",
            "user-1",
            "Summarize the queue state.",
        );
        input.metadata = kheish_types::metadata_with_structured_output_contract(
            serde_json::Value::Null,
            &status_contract(Some(1)),
        )?;

        let error = engine
            .run_input(input, &model, &EchoToolExecutor)
            .await
            .expect_err("exhausted repairs must fail the run");
        let message = error.to_string();
        assert!(
            message.contains("structured output contract unsatisfied after 1 repair attempts"),
            "unexpected error: {message}"
        );
        assert!(message.contains("missing required field `count`"));
        Ok(())
    }

    #[tokio::test]
    async fn output_contract_allows_tool_call_turns_before_the_final_answer() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-oc-3".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 4,
                keep_last_messages: 3,
                ..LoopPolicy::default()
            },
        );
        let model = ScriptedModel::new(vec![
            ModelTurn {
                assistant_message: MessageRecord::new("assistant-tool", Role::Assistant, ""),
                tool_calls: vec![ToolCallRecord {
                    id: "call-echo".to_string(),
                    name: "echo".to_string(),
                    input: json!({"text": "ping"}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                }],
                finish_reason: ModelFinishReason::ToolCalls,
                usage: None,
            },
            text_turn(
                "assistant-json",
                "```json\n{\"status\": \"ok\", \"count\": 1}\n```",
            ),
        ]);

        let outcome = engine
            .run_input(contract_input("session-oc-3")?, &model, &EchoToolExecutor)
            .await?;

        assert_eq!(outcome.status, RunStatus::Completed);
        // Fenced JSON is extracted leniently, then validated strictly.
        assert_eq!(
            outcome.structured_output,
            Some(json!({"status": "ok", "count": 1}))
        );
        Ok(())
    }

    #[test]
    fn run_meta_snapshot_defaults_contract_fields_on_old_payloads() -> Result<()> {
        let contract = status_contract(Some(2));
        let mut meta = RunMetaSnapshot {
            session_id: "s".to_string(),
            thread_id: None,
            input_source_plugin: "daemon".to_string(),
            input_source_kind: "api".to_string(),
            input_event_offset: 0,
            completion_requirements: Vec::new(),
            completion_follow_up_count: 0,
            output_contract: Some(contract.clone()),
            output_contract_repair_count: 2,
            permission_denied_retry_count: 0,
            max_output_tokens_recovery_count: 0,
            recovered_memory: None,
            learned_context: None,
            visible_skills: None,
            autocompact: kheish_types::AutocompactTracking::default(),
            policy: RunPolicySnapshot {
                max_turns: 1,
                keep_last_messages: 1,
                snip_token_budget: 0,
                snip_keep_minimum_messages: 0,
                microcompact_keep_recent: 0,
                microcompact_idle_timeout_ms: None,
                session_memory_min_tokens: 0,
                session_memory_max_tokens: 0,
                autocompact_threshold_tokens: 0,
                autocompact_buffer_tokens: 0,
            },
            engine_version: "test".to_string(),
        };
        let round_tripped: RunMetaSnapshot = serde_json::from_str(&serde_json::to_string(&meta)?)?;
        assert_eq!(round_tripped.output_contract, Some(contract));
        assert_eq!(round_tripped.output_contract_repair_count, 2);

        // A payload persisted before the feature deserializes with defaults.
        meta.output_contract = None;
        meta.output_contract_repair_count = 0;
        let mut legacy = serde_json::to_value(&meta)?;
        legacy
            .as_object_mut()
            .expect("meta serializes as an object")
            .remove("output_contract_repair_count");
        let restored: RunMetaSnapshot = serde_json::from_value(legacy)?;
        assert_eq!(restored.output_contract, None);
        assert_eq!(restored.output_contract_repair_count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn specific_tool_choice_is_released_after_a_successful_tool_execution() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-3d".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 3,
                keep_last_messages: 3,
                ..LoopPolicy::default()
            },
        );

        let model = ScriptedModel::new(vec![
            ModelTurn {
                assistant_message: MessageRecord::new("assistant-write", Role::Assistant, ""),
                tool_calls: vec![ToolCallRecord {
                    id: "call-write".to_string(),
                    name: "write_file".to_string(),
                    input: json!({
                        "path": "reports/summary.txt",
                        "content": "report body",
                    }),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                }],
                finish_reason: ModelFinishReason::ToolCalls,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-done",
                    Role::Assistant,
                    "WRITE_DONE",
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            },
        ]);

        let input = InputEnvelope::text("daemon", "api", "session-3d", "user-1", "write");
        let outcome = engine
            .run_input_with_generation(
                input,
                ModelGenerationConfig {
                    tool_choice: ToolChoice::Specific {
                        name: "write_file".to_string(),
                    },
                    ..ModelGenerationConfig::default()
                },
                &model,
                &EchoToolExecutor,
                &AllowAllPermissions,
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Completed);
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].generation.tool_choice,
            ToolChoice::Specific {
                name: "write_file".to_string(),
            }
        );
        assert_eq!(requests[1].generation.tool_choice, ToolChoice::Auto);

        Ok(())
    }

    #[tokio::test]
    async fn specific_tool_choice_is_kept_after_a_failed_tool_execution() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-3e".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 3,
                keep_last_messages: 3,
                ..LoopPolicy::default()
            },
        );

        let model = ScriptedModel::new(vec![
            ModelTurn {
                assistant_message: MessageRecord::new("assistant-write-1", Role::Assistant, ""),
                tool_calls: vec![ToolCallRecord {
                    id: "call-write-1".to_string(),
                    name: "write_file".to_string(),
                    input: json!({
                        "path": "reports/summary.txt",
                        "content": "report body",
                    }),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                }],
                finish_reason: ModelFinishReason::ToolCalls,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new("assistant-write-2", Role::Assistant, ""),
                tool_calls: vec![ToolCallRecord {
                    id: "call-write-2".to_string(),
                    name: "write_file".to_string(),
                    input: json!({
                        "path": "reports/summary.txt",
                        "content": "report body",
                    }),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                }],
                finish_reason: ModelFinishReason::ToolCalls,
                usage: None,
            },
        ]);

        let input = InputEnvelope::text("daemon", "api", "session-3e", "user-1", "write");
        let outcome = engine
            .run_input_with_generation(
                input,
                ModelGenerationConfig {
                    tool_choice: ToolChoice::Specific {
                        name: "write_file".to_string(),
                    },
                    ..ModelGenerationConfig::default()
                },
                &model,
                &EchoToolExecutor,
                &AlwaysDenyPermissionGate,
            )
            .await;

        assert!(outcome.is_err());
        let requests = model.requests();
        assert!(requests.len() >= 2);
        assert!(requests.iter().take(2).all(|request| {
            request.generation.tool_choice
                == ToolChoice::Specific {
                    name: "write_file".to_string(),
                }
        }));

        Ok(())
    }

    #[tokio::test]
    async fn max_tokens_recovery_escalates_then_continues() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-3c".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 5,
                keep_last_messages: 3,
                ..LoopPolicy::default()
            },
        );

        let model = ScriptedModel::new(vec![
            ModelTurn {
                assistant_message: MessageRecord::new("assistant-max-1", Role::Assistant, ""),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::MaxTokens,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new("assistant-max-2", Role::Assistant, ""),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::MaxTokens,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-write-after-max",
                    Role::Assistant,
                    "Writing the report now.",
                ),
                tool_calls: vec![ToolCallRecord {
                    id: "call-write-after-max".to_string(),
                    name: "write_file".to_string(),
                    input: json!({
                        "path": "reports/summary.txt",
                        "content": "report body after recovery",
                    }),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                }],
                finish_reason: ModelFinishReason::ToolCalls,
                usage: None,
            },
            ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-max-done",
                    Role::Assistant,
                    "Done.",
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            },
        ]);

        let mut input = InputEnvelope::text(
            "daemon",
            "api",
            "session-3c",
            "user-1",
            "Write the report file in the workspace.",
        );
        input.metadata = kheish_types::metadata_with_completion_requirements(
            serde_json::Value::Null,
            &[CompletionRequirement::WorkspaceFile {
                path: Some("reports/summary.txt".to_string()),
            }],
        )?;

        let outcome = engine.run_input(input, &model, &EchoToolExecutor).await?;

        assert_eq!(outcome.status, RunStatus::Completed);
        let requests = model.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].generation.max_output_tokens, None);
        assert_eq!(
            requests[1].generation.max_output_tokens,
            Some(kheish_types::ESCALATED_MAX_OUTPUT_TOKENS)
        );
        assert_eq!(requests[2].generation.max_output_tokens, None);
        assert!(engine.journal().iter().any(|entry| matches!(
            &entry.event,
            SessionEvent::MessageAppended { message }
                if message.role == Role::User
                    && message.content.contains("Output token limit hit.")
        )));

        Ok(())
    }

    #[test]
    fn tool_result_digests_ignore_runtime_offsets_and_timestamps() -> Result<()> {
        let base = ToolResultRecord {
            call_id: "call-1".to_string(),
            output: json!({"stdout": "ok"}),
            is_error: false,
            tool_name: Some("bash".to_string()),
            offset: Some(11),
            timestamp_ms: Some(1000),
            context_updates: vec![ContextUpdate::FileRead {
                path: "README.md".to_string(),
            }],
            hook_contexts: Vec::new(),
        };
        let variant = ToolResultRecord {
            offset: Some(29),
            timestamp_ms: Some(2000),
            ..base.clone()
        };

        assert_eq!(
            AgentEngine::digest_tool_result_record(&base)?,
            AgentEngine::digest_tool_result_record(&variant)?
        );

        let snapshot_a = CanonicalStateSnapshot {
            completed_tool_results: vec![base],
            ..CanonicalStateSnapshot::default()
        };
        let snapshot_b = CanonicalStateSnapshot {
            completed_tool_results: vec![variant],
            ..CanonicalStateSnapshot::default()
        };
        assert_eq!(
            AgentEngine::digest_canonical_state(&snapshot_a)?,
            AgentEngine::digest_canonical_state(&snapshot_b)?
        );
        Ok(())
    }

    #[test]
    fn journal_digests_ignore_nested_tool_result_offsets_and_timestamps() -> Result<()> {
        let event_a = LogEntry {
            offset: 5,
            timestamp_ms: 101,
            event: SessionEvent::ToolCallFinished {
                result: ToolResultRecord {
                    call_id: "call-1".to_string(),
                    output: json!({"stdout": "ok"}),
                    is_error: false,
                    tool_name: Some("bash".to_string()),
                    offset: Some(5),
                    timestamp_ms: Some(101),
                    context_updates: Vec::new(),
                    hook_contexts: Vec::new(),
                },
            },
        };
        let event_b = LogEntry {
            offset: 5,
            timestamp_ms: 202,
            event: SessionEvent::ToolCallFinished {
                result: ToolResultRecord {
                    call_id: "call-1".to_string(),
                    output: json!({"stdout": "ok"}),
                    is_error: false,
                    tool_name: Some("bash".to_string()),
                    offset: Some(99),
                    timestamp_ms: Some(303),
                    context_updates: Vec::new(),
                    hook_contexts: Vec::new(),
                },
            },
        };

        assert_eq!(
            AgentEngine::digest_log_entries([&event_a])?,
            AgentEngine::digest_log_entries([&event_b])?
        );
        Ok(())
    }

    #[tokio::test]
    async fn prompt_too_long_recovery_can_switch_to_fallback_model() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-3d".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 4,
                keep_last_messages: 0,
                snip_keep_minimum: 100,
                autocompact_threshold_tokens: 100,
                ..LoopPolicy::default()
            },
        );

        let model = ScriptedResultModel::new(vec![
            Err(anyhow!("prompt is too long: 210000 > 200000")),
            Ok(ModelTurn {
                assistant_message: MessageRecord::new(
                    "compaction-1",
                    Role::Assistant,
                    "<summary>compact summary</summary>",
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            }),
            Err(anyhow!("prompt is too long: 205000 > 200000")),
            Ok(ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-fallback",
                    Role::Assistant,
                    "Done with fallback model.",
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            }),
        ]);

        let outcome = engine
            .run_input_with_generation(
                InputEnvelope::text(
                    "daemon",
                    "api",
                    "session-3d",
                    "user-1",
                    "Summarize the machine state.",
                ),
                ModelGenerationConfig {
                    fallback_model: Some("claude-sonnet-4-5".to_string()),
                    ..ModelGenerationConfig::default()
                },
                &model,
                &EchoToolExecutor,
                &AllowAllPermissions,
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Completed);
        let requests = model.requests();
        assert_eq!(requests[0].kind, ModelRequestKind::MainLoop);
        assert!(
            requests
                .iter()
                .any(|request| request.kind == ModelRequestKind::Compaction)
        );
        assert!(requests.iter().any(|request| {
            request.kind == ModelRequestKind::MainLoop
                && request.generation.model.as_deref() == Some("claude-sonnet-4-5")
        }));
        Ok(())
    }

    #[tokio::test]
    async fn provider_unavailable_recovery_can_switch_to_fallback_model() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-3e".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(conversation, LoopPolicy::default());
        let model = ScriptedResultModel::new(vec![
            Err(anyhow!("anthropic provider error (status 503): overloaded")),
            Ok(ModelTurn {
                assistant_message: MessageRecord::new(
                    "assistant-fallback-2",
                    Role::Assistant,
                    "Recovered on fallback model.",
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            }),
        ]);

        let outcome = engine
            .run_input_with_generation(
                InputEnvelope::text("daemon", "api", "session-3e", "user-1", "Answer briefly."),
                ModelGenerationConfig {
                    fallback_model: Some("claude-sonnet-4-5".to_string()),
                    ..ModelGenerationConfig::default()
                },
                &model,
                &EchoToolExecutor,
                &AllowAllPermissions,
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Completed);
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].generation.model.as_deref(),
            Some("claude-sonnet-4-5")
        );
        Ok(())
    }

    #[tokio::test]
    async fn microcompact_is_skipped_for_active_sessions() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-microcompact".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation.clone(),
            LoopPolicy {
                microcompact_stale_after_ms: Some(u64::MAX),
                autocompact_threshold_tokens: usize::MAX,
                ..LoopPolicy::default()
            },
        );
        let input = InputEnvelope::text(
            "daemon",
            "api",
            conversation.session_id.clone(),
            "user-1",
            "Inspect the machine and write a report.",
        );
        engine.accept_input(input)?;

        let assistant_message = MessageRecord::new("assistant-1", Role::Assistant, "");
        engine.append_message(assistant_message.clone());
        let call = ToolCallRecord {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            input: json!({"command": "hostname"}),
            assistant_message_id: Some(assistant_message.id.clone()),
            assistant_provider_response_id: None,
        };
        engine.start_tool_call(call.clone());
        let result = ToolResultRecord {
            call_id: call.id.clone(),
            output: json!({
                "command": "hostname",
                "stdout": "andromeda\n",
                "stderr": "",
                "exit_code": 0,
                "success": true,
            }),
            is_error: false,
            tool_name: Some(call.name.clone()),
            offset: None,
            timestamp_ms: None,
            context_updates: Vec::new(),
            hook_contexts: Vec::new(),
        };
        engine.finish_tool_call(result.clone());
        engine.append_tool_result_message(&result)?;

        let mut workspace = engine.build_prompt_workspace();
        let original_tool_message = workspace
            .prompt
            .messages
            .iter()
            .find(|message| message.role == Role::Tool)
            .map(|message| message.content.clone())
            .expect("tool message exists");
        let pipeline_start_offset = engine.next_offset();
        let model = ScriptedModel::new(Vec::new());

        let checkpoint = engine
            .compact_pipeline(
                &mut workspace,
                2,
                &ModelGenerationConfig::default(),
                &model,
                None,
            )
            .await?;

        assert!(checkpoint.is_none());
        assert!(
            engine
                .collect_compaction_boundaries_since(pipeline_start_offset)
                .is_empty(),
            "fresh sessions should not emit microcompact boundaries"
        );
        let current_tool_message = workspace
            .prompt
            .messages
            .iter()
            .find(|message| message.role == Role::Tool)
            .map(|message| message.content.clone())
            .expect("tool message exists");
        assert_eq!(current_tool_message, original_tool_message);
        Ok(())
    }

    #[tokio::test]
    async fn session_memory_compaction_is_projection_only() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-memory".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 4,
                keep_last_messages: 1,
                autocompact_threshold_tokens: 50,
                session_memory_min_tokens: 1,
                session_memory_max_tokens: 100,
                ..LoopPolicy::default()
            },
        );

        engine.append_message(MessageRecord::new("user-1", Role::User, &"A".repeat(500)));
        engine.append_message(MessageRecord::new(
            "assistant-1",
            Role::Assistant,
            "Initial assistant turn before the checkpoint.",
        ));

        let compaction_model = ScriptedResultModel::new(vec![Ok(ModelTurn {
            assistant_message: MessageRecord::new(
                "compaction-1",
                Role::Assistant,
                "<summary>Summary for the initial checkpoint.</summary>",
            ),
            tool_calls: Vec::new(),
            finish_reason: ModelFinishReason::Completed,
            usage: None,
        })]);
        let checkpoint = engine
            .compact_if_needed(1, &compaction_model)
            .await?
            .expect("initial model-driven compaction should create a checkpoint");
        assert_eq!(engine.checkpoints().len(), 1);
        assert_eq!(checkpoint.compacted_until_offset, 0);

        engine.append_message(MessageRecord::new("user-2", Role::User, &"B".repeat(220)));
        engine.append_message(MessageRecord::new(
            "assistant-2",
            Role::Assistant,
            "Recent assistant turn one.",
        ));
        engine.append_message(MessageRecord::new("user-3", Role::User, &"C".repeat(220)));
        engine.append_message(MessageRecord::new(
            "assistant-3",
            Role::Assistant,
            "Recent assistant turn two.",
        ));

        let mut workspace = engine.build_prompt_workspace();
        let original_len = workspace.prompt.messages.len();
        let pipeline_start_offset = engine.next_offset();
        let no_model = ScriptedModel::new(Vec::new());

        let checkpoint = engine
            .compact_pipeline(
                &mut workspace,
                2,
                &ModelGenerationConfig::default(),
                &no_model,
                None,
            )
            .await?;

        assert!(
            checkpoint.is_none(),
            "session-memory should not persist a checkpoint"
        );
        assert_eq!(
            engine.checkpoints().len(),
            1,
            "no new durable checkpoint expected"
        );
        assert!(
            workspace.prompt.messages.len() < original_len,
            "session-memory compaction should shrink the prompt projection"
        );
        assert!(
            engine
                .collect_compaction_boundaries_since(pipeline_start_offset)
                .iter()
                .any(|boundary| matches!(
                    boundary,
                    CompactionBoundary::Autocompact {
                        strategy: CompactionStrategy::SessionMemory,
                        ..
                    }
                )),
            "a session-memory boundary should be recorded"
        );
        assert!(
            no_model.requests().is_empty(),
            "session-memory compaction must not call the model"
        );
        Ok(())
    }

    #[tokio::test]
    async fn model_driven_compaction_attaches_restoration_to_checkpoint() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "restoration-session".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation.clone(),
            LoopPolicy {
                keep_last_messages: 1,
                autocompact_threshold_tokens: 100,
                ..LoopPolicy::default()
            },
        );
        engine.append_message(MessageRecord::new("user-1", Role::User, &"A".repeat(300)));
        engine.append_message(MessageRecord::new(
            "assistant-1",
            Role::Assistant,
            "Collected the first batch of machine details.",
        ));

        let model = ScriptedResultModel::new(vec![Ok(ModelTurn {
            assistant_message: MessageRecord::new(
                "compaction-1",
                Role::Assistant,
                "<summary>Compacted summary.</summary>",
            ),
            tool_calls: Vec::new(),
            finish_reason: ModelFinishReason::Completed,
            usage: None,
        })]);
        let restoration = PostCompactRestoration {
            modified_files: vec![kheish_types::FileSnapshot {
                path: "reports/machine.txt".to_string(),
                content: "report draft".to_string(),
            }],
            active_tools: vec![ToolDefinition {
                name: "write_file".to_string(),
                description: "Write one file".to_string(),
                input_schema: json!({"type": "object"}),
                allows_parallel: false,
            }],
            active_skills: vec![kheish_types::ActiveSkillSnapshot {
                name: "machine_report".to_string(),
                description: "Write and verify a machine report".to_string(),
                when_to_use: None,
                version: None,
                skill_path: "/tmp/machine_report/SKILL.md".to_string(),
                skill_root: "/tmp/machine_report".to_string(),
                digest: "digest-machine-report".to_string(),
                args: None,
                context: kheish_types::SkillExecutionContext::Inline,
                allowed_tools: vec!["write_file".to_string()],
                blocked_tools: Vec::new(),
                agent_profile: None,
                provider: None,
                model: None,
                fallback_model: None,
                activation_reason: None,
                instructions: "Write the report and verify the result.".to_string(),
            }],
            active_plugins: vec!["filesystem".to_string()],
            active_mcp_tools: vec!["fs::read".to_string()],
            mcp_server_instructions: Vec::new(),
            workspace_state: kheish_types::WorkspaceSnapshot {
                workspace_root: Some("/tmp".to_string()),
                git_branch: Some("main".to_string()),
                recent_read_files: vec!["README.md".to_string()],
                recent_modified_files: vec!["reports/machine.txt".to_string()],
            },
            retained_user_inputs: Vec::new(),
            session_control: kheish_types::SessionControlState::default(),
        };
        let provider = StaticRestorationProvider {
            restoration: restoration.clone(),
        };

        let token_count = AgentEngine::prompt_token_count(&engine.current_prompt_projection());
        let checkpoint = engine
            .compact_if_needed_with_tokens(
                1,
                token_count,
                &model,
                Some(&provider),
                false,
                CompactionTrigger::Auto,
            )
            .await?
            .expect("compaction should create one checkpoint");

        assert_eq!(checkpoint.compacted_until_offset, 0);
        assert_eq!(
            engine
                .checkpoints()
                .last()
                .expect("checkpoint should be persisted")
                .restoration,
            Some(restoration.clone())
        );
        let workspace = engine.build_prompt_workspace();
        assert_eq!(workspace.prompt.restoration, Some(restoration.clone()));
        assert!(workspace
            .provider_prompt
            .items
            .iter()
            .any(|item| matches!(item, ProviderInputItem::Restoration { restoration: value } if value == &restoration)));

        let updated = engine.update_latest_checkpoint_restoration(|restoration| {
            restoration.active_tools.clear();
            restoration.active_mcp_tools.clear();
            restoration.mcp_server_instructions.clear();
        });
        assert!(updated);
        let workspace = engine.build_prompt_workspace();
        let restoration = workspace
            .prompt
            .restoration
            .as_ref()
            .expect("restoration should remain present");
        assert!(restoration.active_tools.is_empty());
        assert!(restoration.active_mcp_tools.is_empty());
        assert!(restoration.mcp_server_instructions.is_empty());
        assert!(
            workspace
                .provider_prompt
                .items
                .iter()
                .any(|item| matches!(item, ProviderInputItem::Restoration { restoration } if restoration.active_mcp_tools.is_empty()))
        );
        Ok(())
    }

    #[test]
    fn restore_drops_invalid_latest_checkpoint_and_keeps_last_valid_one() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "restore-validation".to_string(),
            thread_id: None,
        };
        let journal = vec![
            LogEntry {
                offset: 0,
                timestamp_ms: 1,
                event: SessionEvent::MessageAppended {
                    message: MessageRecord::new("user-1", Role::User, "alpha"),
                },
            },
            LogEntry {
                offset: 1,
                timestamp_ms: 2,
                event: SessionEvent::MessageAppended {
                    message: MessageRecord::new("assistant-1", Role::Assistant, "beta"),
                },
            },
        ];

        let valid_canonical = AgentEngine::replay_log_entries(journal.iter().take(1));
        let valid_checkpoint = SessionCheckpoint {
            compacted_until_offset: 0,
            journal_digest: AgentEngine::digest_log_entries(journal.iter().take(1))?,
            prompt_summary: SummaryBlock {
                title: "compacted_history".to_string(),
                content: "alpha".to_string(),
            },
            prompt_window_id: "prompt-window-1".to_string(),
            prompt_window_generation: 1,
            prompt_window_started_after_offset: 0,
            prompt_window_created_by: "test".to_string(),
            compact_metadata: CompactBoundaryMetadata::default(),
            restoration: None,
            canonical: valid_canonical,
        };
        let invalid_checkpoint = SessionCheckpoint {
            compacted_until_offset: 1,
            journal_digest: "invalid-digest".to_string(),
            prompt_summary: SummaryBlock {
                title: "compacted_history".to_string(),
                content: "broken".to_string(),
            },
            prompt_window_id: "prompt-window-2".to_string(),
            prompt_window_generation: 2,
            prompt_window_started_after_offset: 1,
            prompt_window_created_by: "test".to_string(),
            compact_metadata: CompactBoundaryMetadata::default(),
            restoration: None,
            canonical: CanonicalStateSnapshot::default(),
        };

        let restored = AgentEngine::restore(
            conversation,
            LoopPolicy::default(),
            journal.clone(),
            vec![valid_checkpoint.clone(), invalid_checkpoint],
        );

        assert_eq!(restored.checkpoints(), &[valid_checkpoint]);
        assert_eq!(
            restored.replay_from_latest_checkpoint(),
            Some(AgentEngine::replay_log_entries(journal.iter()))
        );
        Ok(())
    }

    #[tokio::test]
    async fn compaction_keeps_tool_call_history_when_later_tool_results_cross_the_boundary()
    -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-4".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 2,
                keep_last_messages: 2,
                autocompact_threshold_tokens: 1,
                ..LoopPolicy::default()
            },
        );

        engine.append_message(MessageRecord::new(
            "user-1",
            Role::User,
            "A long user message to force compaction.",
        ));
        let assistant_offset = engine.append_message(MessageRecord::new(
            "assistant-1",
            Role::Assistant,
            "I will call two tools.",
        ));
        engine.start_tool_call(ToolCallRecord {
            id: "call-1".to_string(),
            name: "echo".to_string(),
            input: json!({"text": "first"}),
            assistant_message_id: Some("assistant-1".to_string()),
            assistant_provider_response_id: None,
        });
        engine.start_tool_call(ToolCallRecord {
            id: "call-2".to_string(),
            name: "echo".to_string(),
            input: json!({"text": "second"}),
            assistant_message_id: Some("assistant-1".to_string()),
            assistant_provider_response_id: None,
        });
        let result_1 = ToolResultRecord {
            call_id: "call-1".to_string(),
            output: json!({"echo": "first"}),
            is_error: false,
            tool_name: Some("echo".to_string()),
            offset: None,
            timestamp_ms: None,
            context_updates: Vec::new(),
            hook_contexts: Vec::new(),
        };
        engine.finish_tool_call(result_1.clone());
        engine.append_tool_result_message(&result_1)?;
        engine.append_message(MessageRecord::new(
            "assistant-2",
            Role::Assistant,
            "Waiting for the remaining tool result.",
        ));
        let result_2 = ToolResultRecord {
            call_id: "call-2".to_string(),
            output: json!({"echo": "second"}),
            is_error: false,
            tool_name: Some("echo".to_string()),
            offset: None,
            timestamp_ms: None,
            context_updates: Vec::new(),
            hook_contexts: Vec::new(),
        };
        engine.finish_tool_call(result_2.clone());
        engine.append_tool_result_message(&result_2)?;

        let initial_cutoff = engine
            .offset_for_message_index(2)
            .expect("third message offset should exist");
        let adjusted_cutoff = engine
            .adjust_compaction_cutoff(initial_cutoff)?
            .expect("compaction should remain possible");
        assert!(adjusted_cutoff < assistant_offset);

        let model = ScriptedModel::new(vec![ModelTurn {
            assistant_message: MessageRecord::new(
                "compaction-2",
                Role::Assistant,
                "<summary>Tool continuity preserved.</summary>",
            ),
            tool_calls: Vec::new(),
            finish_reason: ModelFinishReason::Completed,
            usage: None,
        }]);
        let checkpoint = engine
            .compact_if_needed(1, &model)
            .await?
            .expect("compaction should have produced a checkpoint");
        assert!(checkpoint.compacted_until_offset < assistant_offset);
        assert_eq!(
            engine.checkpoints()[0].compact_metadata.trigger,
            CompactionTrigger::Auto
        );

        let provider_prompt = engine.current_provider_prompt();
        let call_index = provider_prompt
            .items
            .iter()
            .position(|item| {
                matches!(
                    item,
                    ProviderInputItem::ToolCall { call, .. } if call.id == "call-2"
                )
            })
            .expect("tool call should remain visible after compaction");
        let result_index = provider_prompt
            .items
            .iter()
            .position(|item| {
                matches!(
                    item,
                    ProviderInputItem::ToolResult { result } if result.call_id == "call-2"
                )
            })
            .expect("tool result should remain visible after compaction");
        assert!(call_index < result_index);

        Ok(())
    }

    #[tokio::test]
    async fn compaction_invalidates_provider_continuation_until_new_window_response() -> Result<()>
    {
        let conversation = ConversationKey {
            session_id: "session-provider-window".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                keep_last_messages: 3,
                autocompact_threshold_tokens: 1,
                ..LoopPolicy::default()
            },
        );
        engine.append_message(MessageRecord::new("user-1", Role::User, "old user"));
        engine.append_message(
            MessageRecord::new("assistant-1", Role::Assistant, "old assistant")
                .with_provider_response_id("resp_older")
                .with_provider_context(json!({"anthropic": {"content_blocks": ["old"]}})),
        );
        engine.append_message(MessageRecord::new("user-2", Role::User, "tail user"));
        engine.append_message(
            MessageRecord::new("assistant-2", Role::Assistant, "tail assistant")
                .with_provider_response_id("resp_old")
                .with_provider_context(json!({"anthropic": {"content_blocks": ["tail"]}})),
        );
        engine.append_message(MessageRecord::new("user-3", Role::User, "latest user"));

        let model = ScriptedModel::new(vec![ModelTurn {
            assistant_message: MessageRecord::new(
                "compaction-window",
                Role::Assistant,
                "<summary>Window summary.</summary>",
            ),
            tool_calls: Vec::new(),
            finish_reason: ModelFinishReason::Completed,
            usage: None,
        }]);
        engine
            .compact_if_needed(1, &model)
            .await?
            .expect("compaction should create a checkpoint");

        let provider_prompt = engine.current_provider_prompt();
        let tail_assistant = provider_prompt
            .items
            .iter()
            .find_map(|item| match item {
                ProviderInputItem::Message {
                    id,
                    provider_response_id,
                    provider_context,
                    ..
                } if id == "assistant-2" => Some((provider_response_id, provider_context)),
                _ => None,
            })
            .expect("tail assistant should remain visible after compaction");
        assert!(tail_assistant.0.is_none());
        assert!(tail_assistant.1.is_none());

        engine.append_message(
            MessageRecord::new("assistant-new", Role::Assistant, "new-window assistant")
                .with_provider_response_id("resp_new")
                .with_provider_context(json!({"anthropic": {"content_blocks": ["new"]}})),
        );
        let provider_prompt = engine.current_provider_prompt();
        let new_assistant = provider_prompt
            .items
            .iter()
            .find_map(|item| match item {
                ProviderInputItem::Message {
                    id,
                    provider_response_id,
                    provider_context,
                    ..
                } if id == "assistant-new" => Some((provider_response_id, provider_context)),
                _ => None,
            })
            .expect("new-window assistant should be visible");
        assert_eq!(new_assistant.0.as_deref(), Some("resp_new"));
        assert!(new_assistant.1.is_some());

        Ok(())
    }

    #[test]
    fn snip_projection_disables_provider_resume_for_all_providers() {
        let conversation = ConversationKey {
            session_id: "session-snip-provider-window".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                snip_token_budget: 10,
                snip_keep_minimum: 1,
                autocompact_threshold_tokens: 20,
                autocompact_buffer_tokens: 5,
                ..LoopPolicy::default()
            },
        );
        engine.append_message(MessageRecord::new("user-1", Role::User, &"A".repeat(400)));
        engine.append_message(
            MessageRecord::new("assistant-1", Role::Assistant, "tail assistant")
                .with_provider_response_id("resp_old")
                .with_provider_context(json!({"anthropic": {"content_blocks": ["old"]}})),
        );

        let workspace = engine
            .aggressive_snip_workspace(1, &ModelGenerationConfig::default())
            .expect("snip should rebuild a smaller prompt");
        assert!(
            workspace
                .provider_prompt
                .items
                .iter()
                .all(|item| match item {
                    ProviderInputItem::Message {
                        provider_response_id,
                        provider_context,
                        ..
                    } => provider_response_id.is_none() && provider_context.is_none(),
                    ProviderInputItem::ToolCall { call, .. } => {
                        call.assistant_provider_response_id.is_none()
                    }
                    _ => true,
                })
        );

        let rebuilt = engine.build_prompt_workspace();
        assert!(
            rebuilt.provider_prompt.items.iter().all(|item| match item {
                ProviderInputItem::Message {
                    provider_response_id,
                    provider_context,
                    ..
                } => provider_response_id.is_none() && provider_context.is_none(),
                ProviderInputItem::ToolCall { call, .. } => {
                    call.assistant_provider_response_id.is_none()
                }
                _ => true,
            }),
            "provider resume must remain disabled after rebuilding from the journal"
        );
    }

    #[test]
    fn prompt_visible_tool_outputs_are_bounded_without_mutating_audit() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-tool-output-cap".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(conversation, LoopPolicy::default());
        engine.start_tool_call(ToolCallRecord {
            id: "call-large".to_string(),
            name: "custom_mcp_large_output".to_string(),
            input: json!({"query": "large"}),
            assistant_message_id: Some("assistant-large".to_string()),
            assistant_provider_response_id: Some("resp_large".to_string()),
        });
        let raw_output = "x".repeat(60_000);
        let result = ToolResultRecord {
            call_id: "call-large".to_string(),
            output: json!({"payload": raw_output}),
            is_error: false,
            tool_name: Some("custom_mcp_large_output".to_string()),
            offset: None,
            timestamp_ms: None,
            context_updates: Vec::new(),
            hook_contexts: Vec::new(),
        };
        engine.finish_tool_call(result.clone());
        engine.append_tool_result_message(&result)?;

        let audit = engine.replay_from_journal();
        assert_eq!(audit.completed_tool_results[0].output, result.output);

        let provider_prompt = engine.current_provider_prompt();
        let capped = provider_prompt
            .items
            .iter()
            .find_map(|item| match item {
                ProviderInputItem::ToolResult { result } if result.call_id == "call-large" => {
                    Some(&result.output)
                }
                _ => None,
            })
            .expect("tool result should be visible");
        let metadata = capped
            .get("_kheish_prompt_visible_tool_output")
            .expect("large output should be capped");
        assert_eq!(metadata["truncated"], true);
        assert_eq!(metadata["tool_name"], "custom_mcp_large_output");
        assert!(metadata["preview"].as_str().unwrap_or_default().len() < 60_000);

        Ok(())
    }

    #[test]
    fn compaction_prompt_projection_caps_outputs_and_strips_provider_resume() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-compaction-prompt-cap".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(conversation, LoopPolicy::default());
        engine.append_message(
            MessageRecord::new("assistant-large", Role::Assistant, "using tool")
                .with_provider_response_id("resp_old")
                .with_provider_context(json!({"anthropic": {"content_blocks": ["old"]}})),
        );
        engine.start_tool_call(ToolCallRecord {
            id: "call-large".to_string(),
            name: "custom_mcp_large_output".to_string(),
            input: json!({}),
            assistant_message_id: Some("assistant-large".to_string()),
            assistant_provider_response_id: Some("resp_old".to_string()),
        });
        let raw_output = "x".repeat(60_000);
        let result = ToolResultRecord {
            call_id: "call-large".to_string(),
            output: json!({"payload": raw_output}),
            is_error: false,
            tool_name: Some("custom_mcp_large_output".to_string()),
            offset: None,
            timestamp_ms: None,
            context_updates: Vec::new(),
            hook_contexts: Vec::new(),
        };
        engine.finish_tool_call(result.clone());
        engine.append_tool_result_message(&result)?;

        let groups = vec![engine.journal().to_vec()];
        let (projection, provider_prompt) = engine.compaction_prompt_projection(None, &groups, 1);

        assert!(projection.messages.iter().all(|message| {
            message.provider_response_id.is_none() && message.provider_context.is_none()
        }));
        assert!(provider_prompt.items.iter().all(|item| match item {
            ProviderInputItem::Message {
                provider_response_id,
                provider_context,
                ..
            } => provider_response_id.is_none() && provider_context.is_none(),
            ProviderInputItem::ToolCall { call, .. } => {
                call.assistant_provider_response_id.is_none()
            }
            _ => true,
        }));
        let capped = provider_prompt
            .items
            .iter()
            .find_map(|item| match item {
                ProviderInputItem::ToolResult { result } if result.call_id == "call-large" => {
                    Some(&result.output)
                }
                _ => None,
            })
            .expect("tool result should be visible to compaction");
        assert!(capped.get("_kheish_prompt_visible_tool_output").is_some());

        Ok(())
    }

    #[tokio::test]
    async fn compaction_retries_after_prompt_too_long_by_truncating_oldest_groups() -> Result<()> {
        let conversation = ConversationKey {
            session_id: "session-5".to_string(),
            thread_id: None,
        };
        let mut engine = AgentEngine::new(
            conversation,
            LoopPolicy {
                max_turns: 3,
                keep_last_messages: 2,
                autocompact_threshold_tokens: 20,
                ..LoopPolicy::default()
            },
        );

        let long_a = "A".repeat(160);
        let long_b = "B".repeat(160);
        let long_c = "C".repeat(160);
        engine.accept_input(InputEnvelope::text(
            "daemon",
            "api",
            "session-5",
            "user-1",
            long_a,
        ))?;
        engine.append_message(MessageRecord::new(
            "assistant-a",
            Role::Assistant,
            "phase a",
        ));
        engine.accept_input(InputEnvelope::text(
            "daemon",
            "api",
            "session-5",
            "user-2",
            long_b,
        ))?;
        engine.append_message(MessageRecord::new(
            "assistant-b",
            Role::Assistant,
            "phase b",
        ));
        engine.accept_input(InputEnvelope::text(
            "daemon",
            "api",
            "session-5",
            "user-3",
            long_c,
        ))?;
        engine.append_message(MessageRecord::new(
            "assistant-c",
            Role::Assistant,
            "phase c",
        ));

        let model = ScriptedResultModel::new(vec![
            Err(anyhow!("Anthropic request failed with status 413")),
            Ok(ModelTurn {
                assistant_message: MessageRecord::new(
                    "compaction-ptl",
                    Role::Assistant,
                    "<summary>Summary after retry.</summary>",
                ),
                tool_calls: Vec::new(),
                finish_reason: ModelFinishReason::Completed,
                usage: None,
            }),
        ]);

        let checkpoint = engine
            .compact_if_needed(1, &model)
            .await?
            .expect("compaction should succeed after retry");

        assert!(checkpoint.summary_text.contains("Summary after retry."));
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].kind, ModelRequestKind::Compaction);
        assert_eq!(requests[1].kind, ModelRequestKind::Compaction);
        assert!(
            requests[0].provider_prompt.items.len() > requests[1].provider_prompt.items.len(),
            "the retry should summarize fewer groups after truncation"
        );

        Ok(())
    }

    #[test]
    fn prompt_token_count_includes_system_sections_and_restoration() {
        let prompt = PromptProjection {
            summary: None,
            system_sections: vec![SystemPromptSection {
                name: "recovered_memory".to_string(),
                content: "Recovered memory".to_string(),
            }],
            messages: vec![MessageRecord::new("u1", Role::User, "hello")],
            open_tool_calls: Vec::new(),
            restoration: Some(PostCompactRestoration {
                modified_files: Vec::new(),
                active_tools: Vec::new(),
                active_skills: Vec::new(),
                active_plugins: Vec::new(),
                active_mcp_tools: Vec::new(),
                mcp_server_instructions: Vec::new(),
                workspace_state: kheish_types::WorkspaceSnapshot {
                    workspace_root: Some("/tmp".to_string()),
                    git_branch: None,
                    recent_read_files: Vec::new(),
                    recent_modified_files: Vec::new(),
                },
                retained_user_inputs: Vec::new(),
                session_control: kheish_types::SessionControlState::default(),
            }),
        };

        let token_count = AgentEngine::prompt_token_count(&prompt);
        assert!(token_count > crate::tokens::rough_token_estimate("hello"));
        assert!(token_count > crate::tokens::rough_token_estimate("Recovered memory"));
    }

    #[test]
    fn autocompact_threshold_respects_resolved_model_context_window() {
        let engine = AgentEngine::new(
            ConversationKey {
                session_id: "session-threshold".to_string(),
                thread_id: None,
            },
            LoopPolicy {
                autocompact_threshold_tokens: 167_000,
                autocompact_buffer_tokens: 13_000,
                ..LoopPolicy::default()
            },
        );
        let generation = ModelGenerationConfig {
            model: Some("gpt-4o".to_string()),
            max_output_tokens: Some(8_000),
            ..ModelGenerationConfig::default()
        };

        assert_eq!(
            engine.effective_autocompact_threshold_tokens(&generation),
            107_000
        );
    }

    #[test]
    fn accept_input_does_not_persist_recovered_memory_metadata() -> Result<()> {
        let mut engine = AgentEngine::new(
            ConversationKey {
                session_id: "session-memory".to_string(),
                thread_id: None,
            },
            LoopPolicy::default(),
        );
        let mut input = InputEnvelope::text("daemon", "api", "session-memory", "user-1", "hello");
        input.metadata = kheish_types::metadata_with_recovered_memory(
            serde_json::Value::Null,
            Some(&kheish_types::RecoveredMemoryBundle {
                entries: vec![kheish_types::RecoveredMemoryEntry {
                    run_id: "run-1".to_string(),
                    recorded_at_ms: 1,
                    status: "completed".to_string(),
                    request_preview: Some("hi".to_string()),
                    outcome_preview: None,
                    artifact_ids: Vec::new(),
                    failure_markers: Vec::new(),
                    summary: "Request: hi".to_string(),
                }],
                truncated: false,
            }),
        )?;

        engine.accept_input(input)?;

        let event = engine
            .journal()
            .iter()
            .find(|entry| matches!(entry.event, SessionEvent::InputReceived { .. }))
            .expect("journal should contain one input event");
        let SessionEvent::InputReceived { input } = &event.event else {
            panic!("expected input event");
        };
        assert!(
            input
                .metadata
                .get(kheish_types::RECOVERED_MEMORY_METADATA_KEY)
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn accept_input_does_not_persist_learned_context_metadata() -> Result<()> {
        let mut engine = AgentEngine::new(
            ConversationKey {
                session_id: "session-learning".to_string(),
                thread_id: None,
            },
            LoopPolicy::default(),
        );
        let mut input = InputEnvelope::text("daemon", "api", "session-learning", "user-1", "hello");
        input.metadata = kheish_types::metadata_with_learned_context(
            serde_json::Value::Null,
            Some(&kheish_types::LearnedContextBundle {
                entries: vec![kheish_types::LearnedContextEntry {
                    learning_id: "learning-1".to_string(),
                    kind: kheish_types::LearningKind::Fact,
                    published_at_ms: 1,
                    content: "The repo favors deterministic fixtures.".to_string(),
                }],
                truncated: false,
            }),
        )?;

        engine.accept_input(input)?;

        let event = engine
            .journal()
            .iter()
            .find(|entry| matches!(entry.event, SessionEvent::InputReceived { .. }))
            .expect("journal should contain one input event");
        let SessionEvent::InputReceived { input } = &event.event else {
            panic!("expected input event");
        };
        assert!(
            input
                .metadata
                .get(kheish_types::LEARNED_CONTEXT_METADATA_KEY)
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn prompt_too_long_gap_parser_extracts_token_difference() {
        let error = anyhow!("API Error: Prompt is too long: 137500 tokens > 135000 maximum");
        assert_eq!(AgentEngine::prompt_too_long_token_gap(&error), Some(2500));
    }

    #[test]
    fn typed_context_window_error_triggers_reactive_compaction_path() {
        let typed = anyhow!(ModelProviderError::new(
            ProviderErrorKind::ContextWindowExceeded,
            "OpenAI stream error: type=invalid_request_error, code=context_length_exceeded",
            false,
            None,
        ));
        assert!(AgentEngine::is_prompt_too_long_error(&typed));

        let fallback = anyhow!("provider error code=context_length_exceeded");
        assert!(AgentEngine::is_prompt_too_long_error(&fallback));
    }
}
