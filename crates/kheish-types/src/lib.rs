mod capabilities;
mod compaction;
mod hooks;
mod interaction;
mod learning;
mod memory;
mod model;
mod prompt;
mod routing;
mod session;
mod skills;
mod tools;

pub use capabilities::{
    CapabilityScope, CredentialScope, PersonaSkillAssignment, allow_list_allows_entry,
};
pub use compaction::{
    CompactBoundaryMetadata, CompactionBoundary, CompactionStrategy, CompactionTrigger,
    FileSnapshot, PostCompactRestoration, PreservedSegment, RetainedUserInput, SummaryBlock,
    WorkspaceSnapshot,
};
pub use hooks::{
    HOOK_CONTRACT_VERSION, HookDecision, HookDefinition, HookDispatchOutcome, HookEventName,
    HookExecutorConfig, HookFailureMode, HookFailurePolicy, HookInvocation, HookModelConfig,
    HookPermissionBehavior, HookPermissionUpdate, HookPermissionUpdateBehavior,
    HookPermissionUpdateScope, HookSettings,
};
pub use interaction::{
    ApprovalRequest, ApprovalResolution, ApprovalResolutionBehavior, PendingToolBatch,
    PendingToolDecision, PendingUserQuestion, PermissionDecision, RunStatus, UserQuestion,
    UserQuestionAnswer, UserQuestionOption, UserQuestionRequest, UserQuestionResolution,
};
pub use learning::{
    DEFAULT_WORKSPACE_LEARNING_SCOPE_ID, LEARNED_CONTEXT_METADATA_KEY, LearnedContextBundle,
    LearnedContextEntry, LearningEvidenceRef, LearningKind, LearningPolicyDecision,
    LearningPublishTier, LearningScope, LearningScopeKind, LearningSensitivity, LearningSourceRef,
    LearningStatus, LearningVerificationStatus, learned_context_from_metadata,
    metadata_with_learned_context,
};
pub use memory::{
    RECOVERED_MEMORY_METADATA_KEY, RecoveredMemoryBundle, RecoveredMemoryEntry,
    metadata_with_recovered_memory, recovered_memory_from_metadata,
};
pub use model::{
    ApiUsage, CAPPED_DEFAULT_MAX_OUTPUT_TOKENS, COMPLETION_REQUIREMENTS_METADATA_KEY,
    CompletionRequirement, DEFAULT_OUTPUT_CONTRACT_REPAIR_ATTEMPTS, ESCALATED_MAX_OUTPUT_TOKENS,
    MAX_OUTPUT_CONTRACT_REPAIR_ATTEMPTS, ModelFinishReason, ModelGenerationConfig,
    ModelMaxOutputTokens, ModelProviderError, ModelUsage, ProviderErrorKind, ReasoningConfig,
    ReasoningEffort, ReasoningSummary, ResponseFormat, STRUCTURED_OUTPUT_CONTRACT_METADATA_KEY,
    StructuredFieldSchema, StructuredInputContract, StructuredOutputContract, StructuredValueKind,
    ToolChoice, capped_default_max_output_tokens, classify_provider_error_message,
    completion_requirements_from_metadata, extract_json_text,
    metadata_with_completion_requirements, metadata_with_structured_output_contract,
    model_context_window, model_max_output_tokens, structured_output_contract_from_metadata,
};
pub use prompt::{PromptProjection, ProviderInputItem, ProviderPrompt, SystemPromptSection};
pub use routing::{
    ActorRef, AttachmentRef, ContentPart, ConversationKey,
    DEFAULT_DOCUMENT_ATTACHMENT_TEXT_CHAR_LIMIT, InputContentPart, InputEnvelope, InputPayload,
    ReplyHandle, RichOutput, SourceRef, asset_storage_uri, normalize_reply_targets,
    parse_asset_storage_uri, render_content_parts_text, render_document_attachment_text,
};
pub use session::{
    ArchivedTaskCounts, ArchivedTaskRecord, AutocompactTracking, CanonicalStateSnapshot,
    CheckpointSnapshot, CheckpointTrace, DEFAULT_AGENT_MAX_TURNS, FinalStateSnapshot,
    HOOK_RUNTIME_STATE_METADATA_KEY, HookRuntimeState, LogEntry, PlanArtifact,
    PromptMessageSnapshot, PromptSnapshot, PromptTrace, RunMetaSnapshot, RunPolicySnapshot,
    RunSnapshot, RunTrace, SESSION_CAPABILITY_SCOPE_METADATA_KEY,
    SESSION_CONTROL_STATE_METADATA_KEY, SESSION_CREDENTIAL_SCOPE_METADATA_KEY,
    SESSION_EXECUTION_IDENTITY_METADATA_KEY, SESSION_GOAL_METADATA_KEY,
    SESSION_INPUT_CONTRACT_METADATA_KEY, SESSION_OPERATOR_CONFIG_METADATA_KEY,
    SESSION_OUTPUT_CONTRACT_METADATA_KEY, SESSION_PERSONA_BINDING_METADATA_KEY,
    SESSION_REPLY_TARGETS_METADATA_KEY, SESSION_ROUTE_POLICY_METADATA_KEY,
    SESSION_TOOL_OVERRIDES_METADATA_KEY, SessionCheckpoint, SessionControlState, SessionEvent,
    SessionExecutionIdentity, SessionGoal, SessionGoalStatus, SessionGoalUsageAccount,
    SessionOperatorConfig, SessionPersonaBinding, SessionRoutePolicy, SessionToolOverrides,
    SystemPromptSectionSnapshot, TaskArchiveReason, TaskRecord, TaskStatus, TodoItem,
    ToolCallSnapshot, ToolExecutionTrace, ToolResultSnapshot, TurnSnapshot, TurnTrace,
    UNBOUNDED_AGENT_MAX_TURNS, hook_runtime_state_from_metadata, metadata_with_hook_runtime_state,
    metadata_with_session_capability_scope, metadata_with_session_control_state,
    metadata_with_session_credential_scope, metadata_with_session_execution_identity,
    metadata_with_session_goal, metadata_with_session_operator_config,
    metadata_with_session_persona_binding, metadata_with_session_reply_targets,
    metadata_with_session_route_policy, session_capability_scope_from_metadata,
    session_control_state_from_metadata, session_credential_scope_from_metadata,
    session_execution_identity_from_metadata, session_goal_from_metadata,
    session_input_contract_from_metadata, session_operator_config_from_metadata,
    session_output_contract_from_metadata, session_persona_binding_from_metadata,
    session_reply_targets_from_metadata, session_route_policy_from_metadata,
    session_tool_overrides_from_metadata,
};
pub use skills::{
    ActiveSkillSnapshot, SESSION_SKILLS_STATE_METADATA_KEY, SESSION_VISIBLE_SKILLS_METADATA_KEY,
    SessionSkillsState, SkillExecutionContext, metadata_with_session_skills_state,
    metadata_with_session_visible_skills, session_skills_state_from_metadata,
    session_visible_skills_from_metadata,
};
pub use tools::{
    ContextUpdate, DYNAMIC_MCP_TOOL_ALLOWLIST_SENTINEL, MessageRecord, Role, ToolCallRecord,
    ToolDefinition, ToolResultRecord, ToolSurfaceFilter, is_dynamic_mcp_tool_entry,
};

#[cfg(test)]
mod tests {
    use crate::{
        CAPPED_DEFAULT_MAX_OUTPUT_TOKENS, capped_default_max_output_tokens, model_context_window,
        model_max_output_tokens,
    };

    #[test]
    fn model_max_output_tokens_exposes_native_defaults() {
        assert_eq!(model_max_output_tokens("claude-opus-4-6").default, 64_000);
        assert_eq!(
            model_max_output_tokens("claude-opus-4-6").upper_limit,
            128_000
        );
        assert_eq!(model_max_output_tokens("claude-3-opus").default, 4_096);
        assert_eq!(model_max_output_tokens("claude-3-sonnet").default, 8_192);
        assert_eq!(model_max_output_tokens("unknown-model").default, 8_000);
    }

    #[test]
    fn capped_default_max_output_tokens_matches_claude_code_cap() {
        assert_eq!(
            capped_default_max_output_tokens("claude-opus-4-6"),
            CAPPED_DEFAULT_MAX_OUTPUT_TOKENS
        );
        assert_eq!(capped_default_max_output_tokens("claude-3-opus"), 4_096);
    }

    #[test]
    fn model_context_window_matches_supported_provider_families() {
        assert_eq!(model_context_window("claude-opus-4-6"), Some(200_000));
        assert_eq!(model_context_window("claude-sonnet-4-5"), Some(200_000));
        assert_eq!(model_context_window("gpt-5.4"), Some(400_000));
        assert_eq!(model_context_window("gpt-4.1"), Some(1_047_576));
        assert_eq!(model_context_window("gpt-4o"), Some(128_000));
        assert_eq!(model_context_window("unknown-model"), None);
    }
}
