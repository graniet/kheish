use parking_lot::Mutex;
use std::sync::{Arc, OnceLock};

use anyhow::Result;
use async_trait::async_trait;
use kheish_core::{
    AgentEngine, AllowAllPermissions, LoopPolicy, ModelDriver, ModelRequest, ToolCatalog,
    ToolExecutor,
};
use kheish_runtime::{
    AnthropicProvider, AnthropicProviderConfig, ModelBudget, ModelGenerationConfig,
    ModelRetryPolicy, ModelRuntime, NoopObserver, ReasoningConfig, ReasoningEffort, ResponseFormat,
    StructuredFieldSchema, StructuredValueKind, ToolChoice,
};
use kheish_types::{
    ConversationKey, MessageRecord, ModelFinishReason, PromptProjection, ProviderInputItem,
    ProviderPrompt, Role, ToolDefinition, ToolResultRecord,
};
use serde_json::json;

const DEFAULT_MODEL: &str = "claude-opus-4-6";

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_plain_text_response() -> Result<()> {
    let _guard = live_test_guard();
    let Some(runtime) = live_runtime()? else {
        return Ok(());
    };

    let turn = runtime
        .next_turn(build_request(
            "live-plain-text",
            ProviderPrompt {
                instructions: Vec::new(),
                force_synthetic_user_prefix: false,
                items: vec![ProviderInputItem::Message {
                    id: "user-1".to_string(),
                    role: Role::User,
                    content: "Reply with exactly KHEISH_OK and nothing else.".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: None,
                    provider_context: None,
                }],
            },
            Vec::new(),
            ModelGenerationConfig::default(),
        ))
        .await?;

    assert!(turn.tool_calls.is_empty());
    assert_eq!(turn.finish_reason, ModelFinishReason::Completed);
    assert!(turn.assistant_message.content.contains("KHEISH_OK"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_structured_json_response() -> Result<()> {
    let _guard = live_test_guard();
    let Some(runtime) = live_runtime()? else {
        return Ok(());
    };

    let turn = runtime
        .next_turn(build_request(
            "live-structured-json",
            ProviderPrompt {
                instructions: Vec::new(),
                force_synthetic_user_prefix: false,
                items: vec![ProviderInputItem::Message {
                    id: "user-1".to_string(),
                    role: Role::User,
                    content: "Return a JSON object with answer set to hello and status set to ok."
                        .to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: None,
                    provider_context: None,
                }],
            },
            Vec::new(),
            ModelGenerationConfig {
                response_format: ResponseFormat::StructuredJson {
                    schema: StructuredFieldSchema {
                        kind: StructuredValueKind::Object,
                        fields: [
                            (
                                "answer".to_string(),
                                StructuredFieldSchema::new(StructuredValueKind::String),
                            ),
                            (
                                "status".to_string(),
                                StructuredFieldSchema::new(StructuredValueKind::String),
                            ),
                        ]
                        .into_iter()
                        .collect(),
                        optional_fields: Default::default(),
                        items: None,
                    },
                },
                temperature: Some(0.0),
                ..ModelGenerationConfig::default()
            },
        ))
        .await?;

    let payload: serde_json::Value = serde_json::from_str(&turn.assistant_message.content)?;
    assert_eq!(payload["answer"], "hello");
    assert_eq!(payload["status"], "ok");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_forced_tool_call() -> Result<()> {
    let _guard = live_test_guard();
    let Some(runtime) = live_runtime()? else {
        return Ok(());
    };

    let turn = runtime
        .next_turn(build_request(
            "live-tool-call",
            ProviderPrompt {
                instructions: Vec::new(),
                force_synthetic_user_prefix: false,
                items: vec![ProviderInputItem::Message {
                    id: "user-1".to_string(),
                    role: Role::User,
                    content:
                        "Call the echo tool exactly once with text set to kheish-live. Do not answer normally."
                            .to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: None,
                                provider_context: None,
                }],
            },
            vec![echo_tool_definition()],
            ModelGenerationConfig {
                tool_choice: ToolChoice::Specific {
                    name: "echo".to_string(),
                },
                allow_parallel_tool_calls: false,
                temperature: Some(0.0),
                ..ModelGenerationConfig::default()
            },
        ))
        .await?;

    assert_eq!(turn.finish_reason, ModelFinishReason::ToolCalls);
    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(turn.tool_calls[0].name, "echo");
    assert_eq!(turn.tool_calls[0].input["text"], "kheish-live");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_thinking_tool_roundtrip_preserves_provider_context() -> Result<()> {
    let _guard = live_test_guard();
    let Some(runtime) = live_runtime()? else {
        return Ok(());
    };
    let prompt = "Call the echo tool exactly once with text set to kheish-thinking-tool. After the tool result is available, answer with exactly KHEISH_THINKING_TOOL_OK.".to_string();
    let reasoning = Some(ReasoningConfig {
        effort: Some(ReasoningEffort::High),
        summary: None,
        budget_tokens: Some(1024),
        interleaved: false,
    });

    let first_turn = runtime
        .next_turn(build_request(
            "live-thinking-tool-roundtrip",
            ProviderPrompt {
                instructions: Vec::new(),
                force_synthetic_user_prefix: false,
                items: vec![ProviderInputItem::Message {
                    id: "user-1".to_string(),
                    role: Role::User,
                    content: prompt.clone(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: None,
                    provider_context: None,
                }],
            },
            vec![echo_tool_definition()],
            ModelGenerationConfig {
                reasoning: reasoning.clone(),
                tool_choice: ToolChoice::Auto,
                allow_parallel_tool_calls: false,
                ..ModelGenerationConfig::default()
            },
        ))
        .await?;

    assert_eq!(first_turn.finish_reason, ModelFinishReason::ToolCalls);
    assert_eq!(first_turn.tool_calls.len(), 1);
    assert_eq!(first_turn.tool_calls[0].name, "echo");
    assert_eq!(
        first_turn.tool_calls[0].input["text"],
        "kheish-thinking-tool"
    );
    assert!(
        first_turn
            .assistant_message
            .provider_context
            .as_ref()
            .and_then(|value| value.pointer("/anthropic/content_blocks"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|blocks| !blocks.is_empty()),
        "extended thinking tool response should retain provider-native thinking blocks"
    );

    let tool_call = first_turn.tool_calls[0].clone();
    let mut second_request = build_request(
        "live-thinking-tool-roundtrip",
        ProviderPrompt {
            instructions: Vec::new(),
            force_synthetic_user_prefix: false,
            items: vec![
                ProviderInputItem::Message {
                    id: "user-1".to_string(),
                    role: Role::User,
                    content: prompt,
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: None,
                    provider_context: None,
                },
                ProviderInputItem::Message {
                    id: first_turn.assistant_message.id.clone(),
                    role: Role::Assistant,
                    content: first_turn.assistant_message.content.clone(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: first_turn.assistant_message.provider_response_id.clone(),
                    provider_context: first_turn.assistant_message.provider_context.clone(),
                },
                ProviderInputItem::ToolCall {
                    assistant_message_id: Some(first_turn.assistant_message.id.clone()),
                    call: tool_call.clone(),
                },
                ProviderInputItem::ToolResult {
                    result: ToolResultRecord {
                        call_id: tool_call.id.clone(),
                        output: json!({"echo": "kheish-thinking-tool"}),
                        is_error: false,
                        tool_name: Some(tool_call.name.clone()),
                        offset: None,
                        timestamp_ms: None,
                        context_updates: Vec::new(),
                        hook_contexts: Vec::new(),
                    },
                },
            ],
        },
        Vec::new(),
        ModelGenerationConfig {
            reasoning,
            tool_choice: ToolChoice::None,
            ..ModelGenerationConfig::default()
        },
    );
    second_request.turn = 2;
    let second_turn = runtime.next_turn(second_request).await?;

    assert!(second_turn.tool_calls.is_empty());
    assert_eq!(second_turn.finish_reason, ModelFinishReason::Completed);
    assert!(
        second_turn
            .assistant_message
            .content
            .contains("KHEISH_THINKING_TOOL_OK")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_reports_max_tokens_when_truncated() -> Result<()> {
    let _guard = live_test_guard();
    let Some(runtime) = live_runtime()? else {
        return Ok(());
    };

    let turn = runtime
        .next_turn(build_request(
            "live-max-tokens",
            ProviderPrompt {
                instructions: Vec::new(),
                force_synthetic_user_prefix: false,
                items: vec![ProviderInputItem::Message {
                    id: "user-1".to_string(),
                    role: Role::User,
                    content: "Write the word KHEISH fifty times separated by commas.".to_string(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: None,
                    provider_context: None,
                }],
            },
            Vec::new(),
            ModelGenerationConfig {
                max_output_tokens: Some(16),
                temperature: Some(0.0),
                ..ModelGenerationConfig::default()
            },
        ))
        .await?;

    assert!(!turn.assistant_message.content.trim().is_empty());
    assert_eq!(turn.finish_reason, ModelFinishReason::MaxTokens);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_continues_after_tool_result() -> Result<()> {
    let _guard = live_test_guard();
    let Some(runtime) = live_runtime()? else {
        return Ok(());
    };
    let prompt =
        "Use the echo tool exactly once with text set to kheish-live. After the tool result is available, answer with the echoed value only."
            .to_string();

    let first_turn = runtime
        .next_turn(build_request(
            "live-tool-roundtrip",
            ProviderPrompt {
                instructions: Vec::new(),
                force_synthetic_user_prefix: false,
                items: vec![ProviderInputItem::Message {
                    id: "user-1".to_string(),
                    role: Role::User,
                    content: prompt.clone(),
                    content_parts: Vec::new(),
                    attachments: Vec::new(),
                    provider_response_id: None,
                    provider_context: None,
                }],
            },
            vec![echo_tool_definition()],
            ModelGenerationConfig {
                tool_choice: ToolChoice::Specific {
                    name: "echo".to_string(),
                },
                allow_parallel_tool_calls: false,
                temperature: Some(0.0),
                ..ModelGenerationConfig::default()
            },
        ))
        .await?;

    assert_eq!(first_turn.tool_calls.len(), 1);
    let tool_call = first_turn.tool_calls[0].clone();
    let second_turn = runtime
        .next_turn(build_request(
            "live-tool-roundtrip",
            ProviderPrompt {
                instructions: Vec::new(),
                force_synthetic_user_prefix: false,
                items: vec![
                    ProviderInputItem::Message {
                        id: "user-1".to_string(),
                        role: Role::User,
                        content: prompt,
                        content_parts: Vec::new(),
                        attachments: Vec::new(),
                        provider_response_id: None,
                        provider_context: None,
                    },
                    ProviderInputItem::Message {
                        id: first_turn.assistant_message.id.clone(),
                        role: Role::Assistant,
                        content: first_turn.assistant_message.content.clone(),
                        content_parts: Vec::new(),
                        attachments: Vec::new(),
                        provider_response_id: first_turn
                            .assistant_message
                            .provider_response_id
                            .clone(),
                        provider_context: first_turn.assistant_message.provider_context.clone(),
                    },
                    ProviderInputItem::ToolCall {
                        assistant_message_id: Some(first_turn.assistant_message.id.clone()),
                        call: tool_call.clone(),
                    },
                    ProviderInputItem::ToolResult {
                        result: ToolResultRecord {
                            call_id: tool_call.id.clone(),
                            output: json!({"echo": "kheish-live"}),
                            is_error: false,
                            tool_name: Some(tool_call.name.clone()),
                            offset: None,
                            timestamp_ms: None,
                            context_updates: Vec::new(),
                            hook_contexts: Vec::new(),
                        },
                    },
                ],
            },
            Vec::new(),
            ModelGenerationConfig {
                tool_choice: ToolChoice::None,
                temperature: Some(0.0),
                ..ModelGenerationConfig::default()
            },
        ))
        .await?;

    assert!(second_turn.tool_calls.is_empty());
    assert_eq!(second_turn.finish_reason, ModelFinishReason::Completed);
    assert!(
        second_turn
            .assistant_message
            .content
            .to_lowercase()
            .contains("kheish-live")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_live_runs_model_driven_compaction_before_a_follow_up_turn() -> Result<()> {
    let _guard = live_test_guard();
    let Some(runtime) = live_runtime()? else {
        return Ok(());
    };

    let conversation = ConversationKey {
        session_id: "live-compaction".to_string(),
        thread_id: None,
    };
    let mut engine = AgentEngine::new(
        conversation,
        LoopPolicy {
            max_turns: 2,
            keep_last_messages: 2,
            autocompact_threshold_tokens: 100,
            ..LoopPolicy::default()
        },
    );
    let tools = NoTools;
    let permissions = AllowAllPermissions;
    let filler = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu ".repeat(20);

    let first_outcome = engine
        .run_input_with_permissions(
            kheish_types::InputEnvelope::text(
                "test",
                "cli",
                "live-compaction",
                "user-1",
                format!(
                    "Remember this long context and answer with exactly PHASE_ONE.\n\n{}",
                    filler
                ),
            ),
            &runtime,
            &tools,
            &permissions,
        )
        .await?;
    assert!(matches!(
        first_outcome.status,
        kheish_types::RunStatus::Completed
    ));
    assert!(
        engine
            .replay_from_journal()
            .messages
            .last()
            .expect("assistant message should exist after the first run")
            .content
            .contains("PHASE_ONE")
    );

    let second_outcome = engine
        .run_input_with_permissions(
            kheish_types::InputEnvelope::text(
                "test",
                "cli",
                "live-compaction",
                "user-1",
                "Using the previous context, answer with exactly PHASE_TWO.".to_string(),
            ),
            &runtime,
            &tools,
            &permissions,
        )
        .await?;

    assert!(matches!(
        second_outcome.status,
        kheish_types::RunStatus::Completed
    ));
    assert_eq!(engine.checkpoints().len(), 1);
    assert_eq!(second_outcome.checkpoints_created, 1);
    assert_eq!(second_outcome.trace.checkpoints.len(), 1);
    assert!(second_outcome.trace.turns[0].prompt.has_summary);
    assert!(
        engine
            .replay_from_journal()
            .messages
            .last()
            .expect("assistant message should exist after the second run")
            .content
            .contains("PHASE_TWO")
    );
    Ok(())
}

fn build_request(
    session_id: &str,
    provider_prompt: ProviderPrompt,
    available_tools: Vec<ToolDefinition>,
    generation: ModelGenerationConfig,
) -> ModelRequest {
    ModelRequest {
        kind: kheish_core::ModelRequestKind::MainLoop,
        conversation: ConversationKey {
            session_id: session_id.to_string(),
            thread_id: None,
        },
        turn: 1,
        prompt: prompt_projection_from_provider_prompt(&provider_prompt),
        provider_prompt,
        available_tools,
        generation,
    }
}

fn prompt_projection_from_provider_prompt(provider_prompt: &ProviderPrompt) -> PromptProjection {
    let mut messages = Vec::new();
    let mut open_tool_calls = Vec::new();
    let mut summary = None;
    let system_sections = provider_prompt
        .instructions
        .iter()
        .enumerate()
        .map(|(index, content)| kheish_types::SystemPromptSection {
            name: format!("instruction-{index}"),
            content: content.clone(),
        })
        .collect::<Vec<_>>();
    for item in &provider_prompt.items {
        match item {
            ProviderInputItem::Summary { summary: block } => summary = Some(block.clone()),
            ProviderInputItem::Restoration { .. } => {}
            ProviderInputItem::Message {
                id,
                role,
                content,
                provider_response_id,
                provider_context,
                ..
            } => {
                messages.push(MessageRecord {
                    id: id.clone(),
                    role: role.clone(),
                    content: content.clone(),
                    pinned: false,
                    provider_response_id: provider_response_id.clone(),
                    api_usage: None,
                    offset: None,
                    timestamp_ms: None,
                    provider_context: provider_context.clone(),
                });
            }
            ProviderInputItem::ToolCall { call, .. } => open_tool_calls.push(call.clone()),
            ProviderInputItem::ToolResult { .. } => {}
        }
    }
    PromptProjection {
        summary,
        system_sections,
        messages,
        open_tool_calls,
        restoration: None,
    }
}

fn echo_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "echo".to_string(),
        description: "Echoes the provided text argument exactly.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "text": {"type": "string"}
            },
            "required": ["text"],
            "additionalProperties": false
        }),
        allows_parallel: false,
    }
}

struct NoTools;

impl ToolCatalog for NoTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }
}

#[async_trait]
impl ToolExecutor for NoTools {
    async fn execute(&self, _call: &kheish_types::ToolCallRecord) -> Result<ToolResultRecord> {
        unreachable!("the compaction live test does not expose tools")
    }
}

fn live_runtime() -> Result<Option<ModelRuntime<AnthropicProvider>>> {
    let Some(api_key) = first_env(&["KHEISH_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"]) else {
        eprintln!("Skipping Anthropic live tests: no API key environment variable was set.");
        return Ok(None);
    };
    let model =
        std::env::var("KHEISH_ANTHROPIC_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let provider = AnthropicProvider::new(AnthropicProviderConfig::new(model, api_key))?;
    Ok(Some(ModelRuntime::new(
        provider,
        ModelRetryPolicy {
            max_attempts: 2,
            base_backoff_ms: 1_000,
            stream_timeout_ms: 240_000,
            inactivity_timeout_ms: 90_000,
        },
        ModelBudget {
            max_total_output_tokens: 16_000,
            max_total_cost_usd: 50.0,
        },
        Arc::new(NoopObserver),
    )))
}

fn first_env(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| std::env::var(name).ok())
}

fn live_test_guard() -> parking_lot::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock()
}
