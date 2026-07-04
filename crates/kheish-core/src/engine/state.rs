use anyhow::Result;
use async_trait::async_trait;
use kheish_types::{
    CanonicalStateSnapshot, ConversationKey, DEFAULT_AGENT_MAX_TURNS, HookPermissionUpdate,
    MessageRecord, ModelFinishReason, ModelGenerationConfig, ModelUsage, PendingToolBatch,
    PendingUserQuestion, PermissionDecision, PostCompactRestoration, PromptProjection,
    ProviderPrompt, RunSnapshot, RunStatus, RunTrace, ToolCallRecord, ToolDefinition,
    ToolExecutionTrace, ToolResultRecord, UserQuestionRequest,
};
use serde::{Deserialize, Serialize};

const MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES: u32 = 3;
const DEFAULT_MICROCOMPACT_STALE_AFTER_MS: u64 = 15 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoopPolicy {
    /// Maximum main-loop turn number. `0` means no hard turn ceiling.
    pub max_turns: usize,
    pub keep_last_messages: usize,
    pub snip_token_budget: usize,
    pub snip_keep_minimum: usize,
    pub microcompact_keep_recent: usize,
    pub microcompact_stale_after_ms: Option<u64>,
    pub session_memory_min_tokens: usize,
    pub session_memory_max_tokens: usize,
    pub autocompact_threshold_tokens: usize,
    pub autocompact_buffer_tokens: usize,
}

impl Default for LoopPolicy {
    fn default() -> Self {
        Self {
            max_turns: DEFAULT_AGENT_MAX_TURNS,
            keep_last_messages: 6,
            snip_token_budget: 120_000,
            snip_keep_minimum: 10,
            microcompact_keep_recent: 5,
            microcompact_stale_after_ms: Some(DEFAULT_MICROCOMPACT_STALE_AFTER_MS),
            session_memory_min_tokens: 10_000,
            session_memory_max_tokens: 40_000,
            autocompact_threshold_tokens: 167_000,
            autocompact_buffer_tokens: 13_000,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AutocompactTracking {
    pub(crate) consecutive_failures: u32,
    pub(crate) last_compacted_turn: Option<u64>,
    pub(crate) turn_counter: u64,
}

impl AutocompactTracking {
    pub(crate) fn should_skip(&self) -> bool {
        self.consecutive_failures >= MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES
    }

    pub(crate) fn record_success(&mut self, turn: usize) {
        self.consecutive_failures = 0;
        self.last_compacted_turn = Some(turn as u64);
    }

    pub(crate) fn record_failure(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PromptWorkspace {
    pub(crate) prompt: PromptProjection,
    pub(crate) provider_prompt: ProviderPrompt,
    pub(crate) message_offsets: Vec<Option<u64>>,
    pub(crate) item_offsets: Vec<Option<u64>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelRequest {
    pub kind: ModelRequestKind,
    pub conversation: ConversationKey,
    pub turn: usize,
    pub prompt: PromptProjection,
    pub provider_prompt: ProviderPrompt,
    pub available_tools: Vec<ToolDefinition>,
    pub generation: ModelGenerationConfig,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRequestKind {
    #[default]
    MainLoop,
    Compaction,
}

impl ModelRequestKind {
    pub fn artifact_prefix(self) -> &'static str {
        match self {
            Self::MainLoop => "",
            Self::Compaction => "compaction-",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelTurn {
    pub assistant_message: MessageRecord,
    pub tool_calls: Vec<ToolCallRecord>,
    pub finish_reason: ModelFinishReason,
    pub usage: Option<ModelUsage>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunOutcome {
    pub turns: usize,
    pub final_message_id: String,
    pub checkpoints_created: usize,
    pub status: RunStatus,
    pub pending_batch: Option<PendingToolBatch>,
    pub pending_question: Option<PendingUserQuestion>,
    /// The contract-validated final payload, when the run carried a
    /// structured output contract. Completed runs only.
    pub structured_output: Option<serde_json::Value>,
    pub trace: RunTrace,
    pub snapshot: RunSnapshot,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExecutedBatchOutcome {
    pub(crate) tool_executions: Vec<ToolExecutionTrace>,
    pub(crate) pending_question: Option<PendingQuestionExecution>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PendingQuestionExecution {
    pub(crate) call: ToolCallRecord,
    pub(crate) request: UserQuestionRequest,
}

#[async_trait]
pub trait ModelDriver: Send + Sync {
    async fn next_turn(&self, request: ModelRequest) -> Result<ModelTurn>;
}

pub trait ToolCatalog: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
}

#[async_trait]
pub trait ToolExecutor: ToolCatalog + Send + Sync {
    async fn execute(&self, call: &ToolCallRecord) -> Result<ToolResultRecord>;

    async fn execute_batch(&self, calls: &[ToolCallRecord]) -> Result<Vec<ToolResultRecord>> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            results.push(self.execute(call).await?);
        }
        Ok(results)
    }
}

#[async_trait]
pub trait PermissionGate: Send + Sync {
    async fn check(&self, call: &ToolCallRecord) -> Result<PermissionDecision>;

    /// Applies hook-emitted permission updates for the current session.
    async fn apply_hook_permission_updates(
        &self,
        _session_id: &str,
        _updates: &[HookPermissionUpdate],
    ) -> Result<()> {
        Ok(())
    }

    /// Records the final permission decision after hook arbitration.
    async fn record_final_decision(
        &self,
        _call: &ToolCallRecord,
        _decision: &PermissionDecision,
        _reason: Option<String>,
    ) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
pub trait PostCompactRestorationProvider: Send + Sync {
    async fn build_restoration(
        &self,
        conversation: &ConversationKey,
        snapshot: &CanonicalStateSnapshot,
        compacted_until_offset: u64,
    ) -> Result<Option<PostCompactRestoration>>;
}

#[derive(Debug, Default)]
pub struct AllowAllPermissions;

#[async_trait]
impl PermissionGate for AllowAllPermissions {
    async fn check(&self, _call: &ToolCallRecord) -> Result<PermissionDecision> {
        Ok(PermissionDecision::Allow)
    }
}
