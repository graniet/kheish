//! Non-CLI runtime services for models, tools, permissions, sessions, and integrations.

mod debug;
mod execution;
mod model;
mod observability;
mod permissions;
mod providers;
mod runtime;
mod system_prompt;
mod tools;
mod workspace;

pub use debug::{
    DebugArtifactFormat, DebugCaptureLevel, DebugControl, DebugRedactionConfigStatus,
    debug_json_payload_for_level, debug_redaction_config_error, debug_redaction_config_status,
    headers_payload_for_level, provider_payload_for_level, redact_headers, redact_json_value,
    redact_text, summarize_json_value, summarize_text, text_payload_for_level,
};
pub use execution::{
    ExecutionScope, current_cancellation_token, current_execution_scope, interrupted_error,
    is_interrupted_error, scope_execution, tool_context_string_allowlist,
};
pub use kheish_auth::RequestAuthProvider;
pub use model::{
    ModelBudget, ModelBudgetSnapshot, ModelEventSink, ModelFinishReason, ModelGenerationConfig,
    ModelProvider, ModelRetryPolicy, ModelRuntime, ModelRuntimeRequest, ModelStreamEvent,
    ModelUsage, ProviderError, ReasoningConfig, ReasoningEffort, ReasoningSummary, ResponseFormat,
    StructuredFieldSchema, StructuredValueKind, ToolChoice,
};
pub use observability::{
    DebugArtifact, InMemoryObserver, LEARNED_CONTEXT_PROMPT_BUDGET_OMITTED_COUNTER,
    MetricsSnapshot, NoopObserver, RUN_MEMORY_PROMPT_BUDGET_OMITTED_COUNTER,
    RUN_MEMORY_PROMPT_INJECTED_COUNTER, RuntimeObserver, TraceDiff, TraceEvent, TraceEventKind,
    external_action_trace, external_action_trace_with_grant_id, failed_external_action_outcome,
    failed_reqwest_external_action_outcome, safe_url_audit_target, safe_url_debug_target,
};
pub use permissions::{
    PermissionBehavior, PermissionContext, PermissionEngine, PermissionExplanation, PermissionMode,
    PermissionOutcome, PermissionRule, PermissionScope, SessionPermissionUpdateStore,
};
pub use providers::{
    AnthropicPricing, AnthropicProvider, AnthropicProviderConfig, AudioTranscriptionRequest,
    AudioTranscriptionResponse, AudioTranscriptionSegmentTimestamp, AudioTranscriptionTimestamps,
    AudioTranscriptionWordTimestamp, GoogleGeneratedImage, GoogleImageEditInput,
    GoogleImageEditRequest, GoogleImageEditor, GoogleImageGenerationRequest,
    GoogleImageGenerationResponse, GoogleImageGenerator, GoogleImageProviderConfig, GoogleProvider,
    GoogleProviderConfig, OpenAiAudioTranscriber, OpenAiGeneratedImage, OpenAiImageEditInput,
    OpenAiImageEditRequest, OpenAiImageEditor, OpenAiImageGenerationRequest,
    OpenAiImageGenerationResponse, OpenAiImageGenerator, OpenAiPricing, OpenAiProvider,
    OpenAiProviderConfig, OpenAiSpeechRequest, OpenAiSpeechResponse, OpenAiSpeechSynthesizer,
    OpenRouterAudioTranscriber, OpenRouterGeneratedImage, OpenRouterImageEditInput,
    OpenRouterImageEditRequest, OpenRouterImageEditor, OpenRouterImageGenerationRequest,
    OpenRouterImageGenerationResponse, OpenRouterImageGenerator, OpenRouterModelCapabilities,
    OpenRouterProvider, OpenRouterProviderConfig, OpenRouterSpeechRequest,
    OpenRouterSpeechResponse, OpenRouterSpeechSynthesizer, XAiImageEditor, XAiImageGenerator,
    XAiProvider, XAiProviderConfig, fetch_openrouter_model_capabilities,
    parse_openrouter_model_capabilities, resolve_google_image_model, resolve_google_model,
    resolve_openai_image_model, resolve_openai_transcription_model,
    resolve_openai_transcription_request_model, resolve_openai_tts_model,
    resolve_openrouter_image_model, resolve_openrouter_model,
    resolve_openrouter_transcription_model, resolve_openrouter_tts_model, resolve_xai_image_model,
    resolve_xai_model,
};
pub use runtime::{
    AgentRuntime, AgentRuntimeDependencies, AgentRuntimeRestore, HookBlockedError,
    McpInstructionBlock, McpRuntimeSurface,
};
pub use system_prompt::{
    AgentPromptOverride, PromptMergeMode, SystemPromptBuilder, SystemPromptEnvironment,
    SystemPromptSettings,
};
pub use tools::{
    McpScopedHydrator, SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput,
    ToolHook, ToolInputKind, ToolRuntime, ToolRuntimeLimits, ToolSchema, ToolSchemaField,
    normalize_tool_input_numbers,
};
pub use workspace::bounded_workspace_root;
