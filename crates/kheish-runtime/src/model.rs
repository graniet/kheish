use parking_lot::Mutex;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use kheish_core::{ModelDriver, ModelRequest, ModelRequestKind, ModelTurn};
use kheish_types::{
    MessageRecord, ModelProviderError, ProviderErrorKind, ProviderPrompt, Role, ToolCallRecord,
    ToolDefinition,
};
pub use kheish_types::{
    ModelFinishReason, ModelGenerationConfig, ModelUsage, ReasoningConfig, ReasoningEffort,
    ReasoningSummary, ResponseFormat, StructuredFieldSchema, StructuredValueKind, ToolChoice,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

use crate::debug::{
    DebugArtifactFormat, DebugCaptureLevel, debug_json_payload_for_level, summarize_json_value,
};
use crate::execution::{current_cancellation_token, interrupted_error};
use crate::observability::{DebugArtifact, RuntimeObserver, TraceEvent, TraceEventKind};

/// Runtime-wide model budget limits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelBudget {
    /// The maximum cumulative output tokens.
    pub max_total_output_tokens: u64,
    /// The maximum cumulative model cost in USD.
    pub max_total_cost_usd: f64,
}

impl Default for ModelBudget {
    fn default() -> Self {
        Self {
            max_total_output_tokens: 50_000,
            max_total_cost_usd: 20.0,
        }
    }
}

/// A snapshot of the remaining and consumed model budget.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelBudgetSnapshot {
    /// The consumed usage.
    pub consumed: ModelUsage,
    /// The configured budget.
    pub budget: ModelBudget,
}

/// Retry behavior for provider failures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRetryPolicy {
    /// The maximum number of attempts.
    pub max_attempts: usize,
    /// The base backoff in milliseconds.
    pub base_backoff_ms: u64,
    /// The maximum wall-clock duration of a single streaming attempt.
    pub stream_timeout_ms: u64,
    /// The maximum time allowed between two provider events.
    pub inactivity_timeout_ms: u64,
}

impl Default for ModelRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_backoff_ms: 25,
            stream_timeout_ms: 10_000,
            inactivity_timeout_ms: 2_000,
        }
    }
}

/// A provider error that may or may not be retryable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderError {
    /// The provider error message.
    pub message: String,
    /// Whether the error is retryable.
    pub retryable: bool,
    /// An optional provider-supplied retry-after hint in milliseconds.
    pub retry_after_ms: Option<u64>,
}

impl Display for ProviderError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    /// Returns the shared engine-level error kind for this provider failure.
    pub fn kind(&self) -> ProviderErrorKind {
        kheish_types::classify_provider_error_message(&self.message)
    }

    fn into_model_provider_error(self) -> ModelProviderError {
        ModelProviderError::new(
            self.kind(),
            self.message,
            self.retryable,
            self.retry_after_ms,
        )
    }
}

/// A single model stream event emitted by a provider.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelStreamEvent {
    /// A stable message identifier.
    MessageId { value: String },
    /// A streamed text delta.
    TextDelta { text: String },
    /// A completed tool call block.
    ToolCall { call: ToolCallRecord },
    /// A usage report.
    Usage { usage: ModelUsage },
    /// A structured output payload.
    StructuredOutput { value: Value },
    /// Provider-native context that should be retained with the assistant
    /// message and replayed by the same provider on later turns.
    ProviderContext { value: Value },
    /// A terminal stop reason.
    Stop { reason: ModelFinishReason },
}

/// A runtime request passed to the model provider.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRuntimeRequest {
    /// The logical request kind.
    pub kind: ModelRequestKind,
    /// The session identifier.
    pub session_id: String,
    /// The optional thread identifier.
    pub thread_id: Option<String>,
    /// The turn number.
    pub turn: usize,
    /// The model runtime retry attempt for this provider request.
    pub attempt: usize,
    /// The provider-neutral prompt items for this turn.
    pub prompt: ProviderPrompt,
    /// The provider-facing tools available during the turn.
    pub available_tools: Vec<ToolDefinition>,
    /// Provider-neutral generation settings for the turn.
    pub generation: ModelGenerationConfig,
}

/// A sink used by providers to emit streaming model events.
#[derive(Clone)]
pub struct ModelEventSink {
    inner: ModelEventSinkInner,
}

#[derive(Clone)]
enum ModelEventSinkInner {
    #[cfg(test)]
    Events(mpsc::UnboundedSender<ModelStreamEvent>),
    Signals(mpsc::UnboundedSender<StreamSignal>),
}

#[derive(Clone, Debug)]
enum StreamSignal {
    Activity,
    Event(ModelStreamEvent),
}

impl ModelEventSink {
    /// Creates a sink backed by an unbounded model event channel.
    #[cfg(test)]
    pub(crate) fn new(sender: mpsc::UnboundedSender<ModelStreamEvent>) -> Self {
        Self {
            inner: ModelEventSinkInner::Events(sender),
        }
    }

    /// Creates a sink backed by an internal signal channel that can distinguish
    /// provider-side activity from semantic model events.
    fn new_with_activity(sender: mpsc::UnboundedSender<StreamSignal>) -> Self {
        Self {
            inner: ModelEventSinkInner::Signals(sender),
        }
    }

    /// Emits a model stream event.
    pub fn emit(&self, event: ModelStreamEvent) -> Result<()> {
        match &self.inner {
            #[cfg(test)]
            ModelEventSinkInner::Events(sender) => sender
                .send(event)
                .map_err(|_| anyhow!("model event sink is closed")),
            ModelEventSinkInner::Signals(sender) => sender
                .send(StreamSignal::Event(event))
                .map_err(|_| anyhow!("model event sink is closed")),
        }
    }

    /// Marks provider-side liveness without surfacing a semantic event.
    pub(crate) fn mark_activity(&self) -> Result<()> {
        match &self.inner {
            #[cfg(test)]
            ModelEventSinkInner::Events(_) => Ok(()),
            ModelEventSinkInner::Signals(sender) => sender
                .send(StreamSignal::Activity)
                .map_err(|_| anyhow!("model event sink is closed")),
        }
    }
}

/// A provider capable of streaming a single model turn.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Streams a model response into the provided sink.
    async fn stream(
        &self,
        request: ModelRuntimeRequest,
        sink: ModelEventSink,
    ) -> std::result::Result<(), ProviderError>;
}

/// A retrying, budget-aware, streaming model runtime.
pub struct ModelRuntime<P> {
    provider: P,
    retry: ModelRetryPolicy,
    budget: ModelBudget,
    consumed: Mutex<ModelUsage>,
    observer: Arc<dyn RuntimeObserver>,
}

impl<P> ModelRuntime<P> {
    /// Creates a new model runtime.
    pub fn new(
        provider: P,
        retry: ModelRetryPolicy,
        budget: ModelBudget,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Self {
        Self {
            provider,
            retry,
            budget,
            consumed: Mutex::new(ModelUsage::default()),
            observer,
        }
    }

    /// Returns a snapshot of consumed model budget.
    pub fn budget_snapshot(&self) -> ModelBudgetSnapshot {
        ModelBudgetSnapshot {
            consumed: self.consumed.lock().clone(),
            budget: self.budget.clone(),
        }
    }
}

#[async_trait]
impl<P> ModelDriver for ModelRuntime<P>
where
    P: ModelProvider,
{
    async fn next_turn(&self, request: ModelRequest) -> Result<ModelTurn> {
        let cancellation = current_cancellation_token();
        let mut current_generation = request.generation.clone();
        let mut last_retryable_error = None::<ProviderError>;
        for attempt in 1..=self.retry.max_attempts {
            let mut attempt_request = request.clone();
            attempt_request.generation = current_generation.clone();
            let debug_level = self.observer.debug_level();
            self.observer
                .record(TraceEvent::new(TraceEventKind::ModelAttemptStarted {
                    turn: attempt_request.turn,
                    attempt,
                }));
            self.record_model_request_debug(&attempt_request, attempt, debug_level);

            let (sender, mut receiver) = mpsc::unbounded_channel();
            let runtime_request = ModelRuntimeRequest {
                kind: attempt_request.kind,
                session_id: attempt_request.conversation.session_id.clone(),
                thread_id: attempt_request.conversation.thread_id.clone(),
                turn: attempt_request.turn,
                attempt,
                prompt: attempt_request.provider_prompt.clone(),
                available_tools: attempt_request.available_tools.clone(),
                generation: attempt_request.generation.clone(),
            };

            let stream = self
                .provider
                .stream(runtime_request, ModelEventSink::new_with_activity(sender));
            tokio::pin!(stream);

            let mut message_id = None;
            let mut text = String::new();
            let mut tool_calls = Vec::new();
            let mut usage = ModelUsage::default();
            let mut structured_output = None;
            let mut provider_context = None;
            let mut finish_reason = None;
            let deadline =
                tokio::time::Instant::now() + Duration::from_millis(self.retry.stream_timeout_ms);

            let stream_result = loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break Err(ProviderError {
                        message: "model stream timed out".to_string(),
                        retryable: true,
                        retry_after_ms: None,
                    });
                }

                tokio::select! {
                    event = receiver.recv() => {
                        match event {
                            Some(StreamSignal::Activity) => {}
                            Some(StreamSignal::Event(event)) => {
                                apply_stream_event(
                                    event,
                                    &mut message_id,
                                    &mut text,
                                    &mut tool_calls,
                                    &mut usage,
                                    &mut structured_output,
                                    &mut provider_context,
                                    &mut finish_reason,
                                );
                                if finish_reason.is_some() {
                                    break Ok(());
                                }
                            }
                            None => {
                                break stream.await;
                            }
                        }
                    }
                    result = &mut stream => {
                        break result;
                    }
                    _ = async {
                        if let Some(cancellation) = &cancellation {
                            cancellation.cancelled().await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    } => {
                        return Err(interrupted_error());
                    }
                    _ = sleep(Duration::from_millis(self.retry.inactivity_timeout_ms)) => {
                        break Err(ProviderError {
                            message: "model stream became inactive".to_string(),
                            retryable: true,
                            retry_after_ms: None,
                        });
                    }
                }
            };

            while let Ok(signal) = receiver.try_recv() {
                if let StreamSignal::Event(event) = signal {
                    apply_stream_event(
                        event,
                        &mut message_id,
                        &mut text,
                        &mut tool_calls,
                        &mut usage,
                        &mut structured_output,
                        &mut provider_context,
                        &mut finish_reason,
                    );
                }
            }

            self.consume_budget(&usage)?;
            match stream_result {
                Ok(()) => {
                    self.observer
                        .record(TraceEvent::new(TraceEventKind::ModelAttemptFinished {
                            turn: attempt_request.turn,
                            attempt,
                        }));
                    // Structured validation only applies to final text turns:
                    // a tool-call turn legitimately carries no answer text and
                    // must not be parsed as JSON.
                    if tool_calls.is_empty() {
                        if let kheish_types::ResponseFormat::StructuredJson { schema } =
                            &attempt_request.generation.response_format
                        {
                            let value = if let Some(value) = structured_output.as_ref() {
                                value
                            } else {
                                structured_output =
                                    Some(serde_json::from_str::<Value>(&text).map_err(
                                        |error| anyhow!("structured output parse error: {error}"),
                                    )?);
                                structured_output
                                    .as_ref()
                                    .expect("structured output must be available after parse")
                            };
                            schema.validate_value(value).map_err(|error| {
                                anyhow!("structured output validation error: {error}")
                            })?;
                            if text.is_empty() {
                                text = serde_json::to_string(&value)?;
                            }
                        }
                    }
                    let resolved_finish_reason = finish_reason.unwrap_or_else(|| {
                        if tool_calls.is_empty() {
                            ModelFinishReason::Completed
                        } else {
                            ModelFinishReason::ToolCalls
                        }
                    });
                    self.record_model_response_debug(
                        &attempt_request,
                        attempt,
                        debug_level,
                        &message_id,
                        &text,
                        &tool_calls,
                        &usage,
                        structured_output.as_ref(),
                        provider_context.as_ref(),
                        &resolved_finish_reason,
                    );
                    let provider_response_id = message_id.clone();
                    let assistant_message_id = message_id
                        .unwrap_or_else(|| format!("assistant-turn-{}", attempt_request.turn));
                    let mut assistant_message =
                        MessageRecord::new(assistant_message_id, Role::Assistant, text);
                    if let Some(provider_response_id) = provider_response_id {
                        assistant_message =
                            assistant_message.with_provider_response_id(provider_response_id);
                    }
                    if let Some(provider_context) = provider_context {
                        assistant_message =
                            assistant_message.with_provider_context(provider_context);
                    }
                    return Ok(ModelTurn {
                        assistant_message,
                        tool_calls,
                        finish_reason: resolved_finish_reason,
                        usage: Some(usage),
                    });
                }
                Err(error) if error.retryable && attempt < self.retry.max_attempts => {
                    last_retryable_error = Some(error.clone());
                    if let Some(adjusted_max_output_tokens) =
                        parse_max_tokens_context_overflow_adjustment(&error.message)
                    {
                        current_generation.max_output_tokens = Some(adjusted_max_output_tokens);
                        self.record_model_error_debug(
                            &attempt_request,
                            attempt,
                            debug_level,
                            &error.message,
                            true,
                        );
                        self.observer.record(TraceEvent::new(
                            TraceEventKind::ModelRetryScheduled {
                                turn: attempt_request.turn,
                                attempt,
                                reason: format!(
                                    "adjusted max_output_tokens to {adjusted_max_output_tokens}: {}",
                                    error.message
                                ),
                            },
                        ));
                        continue;
                    }
                    self.record_model_error_debug(
                        &attempt_request,
                        attempt,
                        debug_level,
                        &error.message,
                        true,
                    );
                    self.observer
                        .record(TraceEvent::new(TraceEventKind::ModelRetryScheduled {
                            turn: attempt_request.turn,
                            attempt,
                            reason: error.message.clone(),
                        }));
                    let backoff_ms = error
                        .retry_after_ms
                        .unwrap_or(self.retry.base_backoff_ms * attempt as u64);
                    tokio::select! {
                        _ = sleep(Duration::from_millis(backoff_ms)) => {}
                        _ = async {
                            if let Some(cancellation) = &cancellation {
                                cancellation.cancelled().await;
                            } else {
                                std::future::pending::<()>().await;
                            }
                        } => return Err(interrupted_error()),
                    }
                }
                Err(error) => {
                    self.record_model_error_debug(
                        &attempt_request,
                        attempt,
                        debug_level,
                        &error.message,
                        false,
                    );
                    return Err(anyhow!(error.into_model_provider_error()));
                }
            }
        }

        if let Some(reason) = last_retryable_error {
            return Err(anyhow!(ModelProviderError::new(
                reason.kind(),
                format!("model runtime exhausted retries: {}", reason.message),
                reason.retryable,
                reason.retry_after_ms,
            )));
        }
        bail!("model runtime exhausted retries")
    }
}

impl<P> ModelRuntime<P> {
    fn consume_budget(&self, usage: &ModelUsage) -> Result<()> {
        let mut consumed = self.consumed.lock();
        accumulate_usage(&mut consumed, usage);
        if consumed.output_tokens > self.budget.max_total_output_tokens {
            bail!("model output token budget exceeded");
        }
        if consumed.cost_usd > self.budget.max_total_cost_usd {
            bail!("model cost budget exceeded");
        }
        Ok(())
    }

    fn record_model_request_debug(
        &self,
        request: &ModelRequest,
        attempt: usize,
        level: DebugCaptureLevel,
    ) {
        if !level.is_enabled() {
            return;
        }
        let payload = match level {
            DebugCaptureLevel::On => serde_json::json!({
                "turn": request.turn,
                "provider_prompt": summarize_json_value(
                    &serde_json::to_value(&request.provider_prompt).unwrap_or(Value::Null)
                ),
                "prompt_projection": summarize_json_value(
                    &serde_json::to_value(&request.prompt).unwrap_or(Value::Null)
                ),
                "available_tools": summarize_json_value(
                    &serde_json::to_value(&request.available_tools).unwrap_or(Value::Null)
                ),
                "generation": summarize_json_value(
                    &serde_json::to_value(&request.generation).unwrap_or(Value::Null)
                ),
            }),
            DebugCaptureLevel::Redacted | DebugCaptureLevel::Full => debug_json_payload_for_level(
                level,
                &serde_json::json!({
                    "turn": request.turn,
                    "prompt_projection": request.prompt,
                    "provider_prompt": request.provider_prompt,
                    "available_tools": request.available_tools,
                    "generation": request.generation,
                }),
            ),
            DebugCaptureLevel::Off => Value::Null,
        };
        let name = format!("{}model-request", request.kind.artifact_prefix());
        self.observer.record_debug_artifact(DebugArtifact::new(
            level,
            Some(request.turn),
            Some(attempt),
            &name,
            DebugArtifactFormat::Json,
            payload,
        ));
    }

    fn record_model_response_debug(
        &self,
        request: &ModelRequest,
        attempt: usize,
        level: DebugCaptureLevel,
        message_id: &Option<String>,
        text: &str,
        tool_calls: &[ToolCallRecord],
        usage: &ModelUsage,
        structured_output: Option<&Value>,
        provider_context: Option<&Value>,
        finish_reason: &ModelFinishReason,
    ) {
        if !level.is_enabled() {
            return;
        }
        let payload = match level {
            DebugCaptureLevel::On => serde_json::json!({
            "message_id": message_id,
            "text": crate::summarize_text(text),
            "tool_calls": summarize_json_value(
                &serde_json::to_value(tool_calls).unwrap_or(Value::Null)
            ),
            "usage": usage,
                "finish_reason": finish_reason,
                "structured_output": structured_output.map(summarize_json_value),
                "provider_context": provider_context.map(summarize_json_value),
            }),
            DebugCaptureLevel::Redacted | DebugCaptureLevel::Full => debug_json_payload_for_level(
                level,
                &serde_json::json!({
                    "message_id": message_id,
                    "text": text,
                    "tool_calls": tool_calls,
                    "usage": usage,
                    "finish_reason": finish_reason,
                    "structured_output": structured_output,
                    "provider_context": provider_context,
                }),
            ),
            DebugCaptureLevel::Off => Value::Null,
        };
        let name = format!("{}model-response", request.kind.artifact_prefix());
        self.observer.record_debug_artifact(DebugArtifact::new(
            level,
            Some(request.turn),
            Some(attempt),
            &name,
            DebugArtifactFormat::Json,
            payload,
        ));
    }

    fn record_model_error_debug(
        &self,
        request: &ModelRequest,
        attempt: usize,
        level: DebugCaptureLevel,
        message: &str,
        retryable: bool,
    ) {
        if !level.is_enabled() {
            return;
        }
        let name = format!("{}model-error", request.kind.artifact_prefix());
        self.observer.record_debug_artifact(DebugArtifact::new(
            level,
            Some(request.turn),
            Some(attempt),
            &name,
            DebugArtifactFormat::Json,
            match level {
                DebugCaptureLevel::Redacted | DebugCaptureLevel::Full => {
                    debug_json_payload_for_level(
                        level,
                        &serde_json::json!({
                        "message": message,
                        "retryable": retryable,
                        }),
                    )
                }
                _ => serde_json::json!({
                    "message": message,
                    "retryable": retryable,
                }),
            },
        ));
    }
}

fn apply_stream_event(
    event: ModelStreamEvent,
    message_id: &mut Option<String>,
    text: &mut String,
    tool_calls: &mut Vec<ToolCallRecord>,
    usage: &mut ModelUsage,
    structured_output: &mut Option<Value>,
    provider_context: &mut Option<Value>,
    finish_reason: &mut Option<ModelFinishReason>,
) {
    match event {
        ModelStreamEvent::MessageId { value } => *message_id = Some(value),
        ModelStreamEvent::TextDelta { text: delta } => text.push_str(&delta),
        ModelStreamEvent::ToolCall { call } => tool_calls.push(call),
        ModelStreamEvent::Usage { usage: snapshot } => replace_usage(usage, &snapshot),
        ModelStreamEvent::StructuredOutput { value } => *structured_output = Some(value),
        ModelStreamEvent::ProviderContext { value } => {
            merge_provider_context(provider_context, value);
        }
        ModelStreamEvent::Stop { reason } => *finish_reason = Some(reason),
    }
}

fn merge_provider_context(target: &mut Option<Value>, update: Value) {
    match (target.as_mut(), update) {
        (Some(Value::Object(existing)), Value::Object(update)) => {
            for (key, value) in update {
                existing.insert(key, value);
            }
        }
        (_, value) => {
            *target = Some(value);
        }
    }
}

fn accumulate_usage(target: &mut ModelUsage, delta: &ModelUsage) {
    target.input_tokens += delta.input_tokens;
    target.output_tokens += delta.output_tokens;
    target.cost_usd += delta.cost_usd;
}

fn replace_usage(target: &mut ModelUsage, snapshot: &ModelUsage) {
    target.input_tokens = snapshot.input_tokens;
    target.output_tokens = snapshot.output_tokens;
    target.cost_usd = snapshot.cost_usd;
}

fn parse_max_tokens_context_overflow_adjustment(message: &str) -> Option<u32> {
    const MARKER: &str = "input length and `max_tokens` exceed context limit:";
    const SAFETY_BUFFER: u32 = 1_000;
    const FLOOR_OUTPUT_TOKENS: u32 = 3_000;

    let payload = message.split_once(MARKER)?.1.trim();
    let mut numbers = payload
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok());
    let input_tokens = numbers.next()?;
    let _requested_output_tokens = numbers.next()?;
    let context_limit = numbers.next()?;
    let available_context = context_limit
        .saturating_sub(input_tokens)
        .saturating_sub(SAFETY_BUFFER);
    if available_context < FLOOR_OUTPUT_TOKENS {
        return None;
    }
    Some(available_context.max(FLOOR_OUTPUT_TOKENS))
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use kheish_core::{ModelDriver, ModelRequest, ModelRequestKind};
    use kheish_types::ToolCallRecord;
    use kheish_types::{
        ConversationKey, ModelFinishReason, ModelGenerationConfig, ModelProviderError,
        PromptProjection, ProviderErrorKind, ProviderPrompt, ResponseFormat,
    };
    use serde_json::json;
    use tokio::time::{Duration, sleep};

    use super::{
        ModelBudget, ModelRetryPolicy, ModelRuntime, ModelStreamEvent, ModelUsage, ProviderError,
        StructuredFieldSchema, StructuredValueKind,
    };
    use crate::observability::{InMemoryObserver, TraceEventKind};

    struct ScriptedProvider {
        streams: Mutex<VecDeque<Result<Vec<ModelStreamEvent>, ProviderError>>>,
        attempts: Arc<Mutex<Vec<usize>>>,
    }

    enum TimedAction {
        Sleep(Duration),
        Activity,
        Event(ModelStreamEvent),
    }

    struct TimedProvider {
        actions: Mutex<VecDeque<TimedAction>>,
    }

    impl ScriptedProvider {
        fn new(streams: Vec<Result<Vec<ModelStreamEvent>, ProviderError>>) -> Self {
            Self {
                streams: Mutex::new(VecDeque::from(streams)),
                attempts: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl TimedProvider {
        fn new(actions: Vec<TimedAction>) -> Self {
            Self {
                actions: Mutex::new(VecDeque::from(actions)),
            }
        }
    }

    fn default_request(session_id: &str, response_format: ResponseFormat) -> ModelRequest {
        ModelRequest {
            kind: ModelRequestKind::MainLoop,
            conversation: ConversationKey {
                session_id: session_id.to_string(),
                thread_id: None,
            },
            turn: 1,
            prompt: PromptProjection::default(),
            provider_prompt: ProviderPrompt::default(),
            available_tools: Vec::new(),
            generation: ModelGenerationConfig {
                response_format,
                ..ModelGenerationConfig::default()
            },
        }
    }

    #[async_trait]
    impl super::ModelProvider for ScriptedProvider {
        async fn stream(
            &self,
            request: super::ModelRuntimeRequest,
            sink: super::ModelEventSink,
        ) -> std::result::Result<(), ProviderError> {
            self.attempts.lock().push(request.attempt);
            match self
                .streams
                .lock()
                .pop_front()
                .expect("missing scripted provider response")
            {
                Ok(events) => {
                    for event in events {
                        sink.emit(event).expect("sink should remain open");
                    }
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }
    }

    #[async_trait]
    impl super::ModelProvider for TimedProvider {
        async fn stream(
            &self,
            _request: super::ModelRuntimeRequest,
            sink: super::ModelEventSink,
        ) -> std::result::Result<(), ProviderError> {
            loop {
                let action = self.actions.lock().pop_front();
                let Some(action) = action else {
                    return Ok(());
                };
                match action {
                    TimedAction::Sleep(duration) => sleep(duration).await,
                    TimedAction::Activity => sink.mark_activity().expect("sink should remain open"),
                    TimedAction::Event(event) => sink.emit(event).expect("sink should remain open"),
                }
            }
        }
    }

    #[tokio::test]
    async fn model_runtime_retries_retryable_errors() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let provider = ScriptedProvider::new(vec![
            Err(ProviderError {
                message: "temporary".to_string(),
                retryable: true,
                retry_after_ms: None,
            }),
            Ok(vec![
                ModelStreamEvent::TextDelta {
                    text: "done".to_string(),
                },
                ModelStreamEvent::Usage {
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 2,
                        cost_usd: 0.1,
                    },
                },
            ]),
        ]);
        let attempts = provider.attempts.clone();
        let runtime = ModelRuntime::new(
            provider,
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            observer.clone(),
        );

        let turn = runtime
            .next_turn(default_request("session-1", ResponseFormat::Text))
            .await?;

        assert_eq!(turn.assistant_message.content, "done");
        assert_eq!(attempts.lock().as_slice(), &[1, 2]);
        assert!(
            observer
                .traces()
                .iter()
                .any(|event| matches!(event.kind, TraceEventKind::ModelRetryScheduled { .. }))
        );
        Ok(())
    }

    #[test]
    fn provider_error_kind_classifies_context_window_across_provider_messages() {
        let cases = [
            "OpenAI stream error: type=invalid_request_error, code=context_length_exceeded",
            "Anthropic request failed with status 400: prompt is too long",
            "Google request error with status 400: token count exceeds the model context length",
            "OpenRouter request error with status 413",
        ];
        for message in cases {
            let error = ProviderError {
                message: message.to_string(),
                retryable: false,
                retry_after_ms: None,
            };
            assert_eq!(error.kind(), ProviderErrorKind::ContextWindowExceeded);
        }
    }

    #[tokio::test]
    async fn model_runtime_preserves_typed_error_when_retries_exhaust() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let runtime = ModelRuntime::new(
            ScriptedProvider::new(vec![
                Err(ProviderError {
                    message: "OpenAI stream error: code=context_length_exceeded".to_string(),
                    retryable: true,
                    retry_after_ms: None,
                }),
                Err(ProviderError {
                    message: "OpenAI stream error: code=context_length_exceeded".to_string(),
                    retryable: true,
                    retry_after_ms: None,
                }),
            ]),
            ModelRetryPolicy {
                max_attempts: 2,
                base_backoff_ms: 0,
                stream_timeout_ms: 10_000,
                inactivity_timeout_ms: 10_000,
            },
            ModelBudget::default(),
            observer,
        );

        let error = runtime
            .next_turn(default_request("session-retry-kind", ResponseFormat::Text))
            .await
            .expect_err("exhausted retries should fail");
        let provider_error = error
            .downcast_ref::<ModelProviderError>()
            .expect("runtime should preserve typed provider error");
        assert_eq!(
            provider_error.kind,
            ProviderErrorKind::ContextWindowExceeded
        );
        Ok(())
    }

    #[tokio::test]
    async fn model_runtime_attaches_provider_context_to_assistant_message() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let provider = ScriptedProvider::new(vec![Ok(vec![
            ModelStreamEvent::MessageId {
                value: "msg-anthropic-thinking".to_string(),
            },
            ModelStreamEvent::ProviderContext {
                value: json!({
                    "anthropic": {
                        "content_blocks": [{
                            "type": "thinking",
                            "thinking": "I should use a tool.",
                            "signature": "sig-123"
                        }]
                    }
                }),
            },
            ModelStreamEvent::TextDelta {
                text: "Calling tool.".to_string(),
            },
            ModelStreamEvent::ToolCall {
                call: ToolCallRecord {
                    id: "call-1".to_string(),
                    name: "echo".to_string(),
                    input: json!({"text": "ping"}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                },
            },
            ModelStreamEvent::Stop {
                reason: ModelFinishReason::ToolCalls,
            },
        ])]);
        let runtime = ModelRuntime::new(
            provider,
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            observer,
        );

        let turn = runtime
            .next_turn(default_request(
                "session-provider-context",
                ResponseFormat::Text,
            ))
            .await?;

        assert_eq!(
            turn.assistant_message.provider_response_id.as_deref(),
            Some("msg-anthropic-thinking")
        );
        assert_eq!(
            turn.assistant_message
                .provider_context
                .as_ref()
                .and_then(|value| value.pointer("/anthropic/content_blocks/0/signature"))
                .and_then(serde_json::Value::as_str),
            Some("sig-123")
        );
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.finish_reason, ModelFinishReason::ToolCalls);
        Ok(())
    }

    #[tokio::test]
    async fn model_runtime_adjusts_max_output_tokens_after_context_overflow() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let runtime = ModelRuntime::new(
            ScriptedProvider::new(vec![
                Err(ProviderError {
                    message: "input length and `max_tokens` exceed context limit: 188059 + 20000 > 200000".to_string(),
                    retryable: true,
                    retry_after_ms: None,
                }),
                Ok(vec![
                    ModelStreamEvent::TextDelta {
                        text: "done".to_string(),
                    },
                    ModelStreamEvent::Usage {
                        usage: ModelUsage {
                            input_tokens: 1,
                            output_tokens: 2,
                            cost_usd: 0.1,
                        },
                    },
                ]),
            ]),
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            observer.clone(),
        );

        let mut request = default_request("session-1b", ResponseFormat::Text);
        request.generation.max_output_tokens = Some(20_000);
        let turn = runtime.next_turn(request).await?;

        assert_eq!(turn.assistant_message.content, "done");
        assert!(
            observer
                .traces()
                .iter()
                .any(|event| matches!(event.kind, TraceEventKind::ModelRetryScheduled { ref reason, .. } if reason.contains("adjusted max_output_tokens to 10941")))
        );
        Ok(())
    }

    #[tokio::test]
    async fn model_runtime_skips_structured_validation_on_tool_call_turns() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let runtime = ModelRuntime::new(
            ScriptedProvider::new(vec![Ok(vec![
                ModelStreamEvent::ToolCall {
                    call: ToolCallRecord {
                        id: "call-1".to_string(),
                        name: "echo".to_string(),
                        input: json!({"text": "ping"}),
                        assistant_message_id: None,
                        assistant_provider_response_id: None,
                    },
                },
                ModelStreamEvent::Stop {
                    reason: ModelFinishReason::ToolCalls,
                },
            ])]),
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            observer,
        );

        // A tool-call turn has no answer text; with a structured response
        // format it must not be parsed as JSON (this used to hard-fail).
        let turn = runtime
            .next_turn(default_request(
                "session-tools",
                ResponseFormat::StructuredJson {
                    schema: StructuredFieldSchema::new(StructuredValueKind::Object),
                },
            ))
            .await?;

        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.finish_reason, ModelFinishReason::ToolCalls);
        Ok(())
    }

    #[tokio::test]
    async fn model_runtime_validates_structured_output() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let runtime = ModelRuntime::new(
            ScriptedProvider::new(vec![Ok(vec![
                ModelStreamEvent::StructuredOutput {
                    value: json!({"answer": "ok"}),
                },
                ModelStreamEvent::Usage {
                    usage: ModelUsage::default(),
                },
            ])]),
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            observer,
        );

        let turn = runtime
            .next_turn(default_request(
                "session-2",
                ResponseFormat::StructuredJson {
                    schema: StructuredFieldSchema {
                        kind: StructuredValueKind::Object,
                        fields: BTreeMap::from([(
                            "answer".to_string(),
                            StructuredFieldSchema::new(StructuredValueKind::String),
                        )]),
                        optional_fields: BTreeMap::new(),
                        items: None,
                    },
                },
            ))
            .await?;

        assert_eq!(turn.assistant_message.content, "{\"answer\":\"ok\"}");
        Ok(())
    }

    #[tokio::test]
    async fn model_runtime_allows_null_optional_structured_fields() -> Result<()> {
        let runtime = ModelRuntime::new(
            ScriptedProvider::new(vec![Ok(vec![ModelStreamEvent::StructuredOutput {
                value: json!({"answer": "ok", "note": null}),
            }])]),
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            InMemoryObserver::shared(),
        );

        let turn = runtime
            .next_turn(default_request(
                "session-2b",
                ResponseFormat::StructuredJson {
                    schema: StructuredFieldSchema {
                        kind: StructuredValueKind::Object,
                        fields: BTreeMap::from([(
                            "answer".to_string(),
                            StructuredFieldSchema::new(StructuredValueKind::String),
                        )]),
                        optional_fields: BTreeMap::from([(
                            "note".to_string(),
                            StructuredFieldSchema::new(StructuredValueKind::String),
                        )]),
                        items: None,
                    },
                },
            ))
            .await?;

        assert_eq!(
            turn.assistant_message.content,
            "{\"answer\":\"ok\",\"note\":null}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn model_runtime_rejects_unknown_structured_fields() -> Result<()> {
        let runtime = ModelRuntime::new(
            ScriptedProvider::new(vec![Ok(vec![ModelStreamEvent::StructuredOutput {
                value: json!({"answer": "ok", "extra": true}),
            }])]),
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            InMemoryObserver::shared(),
        );

        let error = runtime
            .next_turn(default_request(
                "session-2c",
                ResponseFormat::StructuredJson {
                    schema: StructuredFieldSchema {
                        kind: StructuredValueKind::Object,
                        fields: BTreeMap::from([(
                            "answer".to_string(),
                            StructuredFieldSchema::new(StructuredValueKind::String),
                        )]),
                        optional_fields: BTreeMap::new(),
                        items: None,
                    },
                },
            ))
            .await
            .expect_err("unknown structured field must be rejected");

        assert!(error.to_string().contains("$: unknown field `extra`"));
        Ok(())
    }

    #[tokio::test]
    async fn model_runtime_uses_latest_usage_snapshot_per_attempt() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let runtime = ModelRuntime::new(
            ScriptedProvider::new(vec![Ok(vec![
                ModelStreamEvent::Usage {
                    usage: ModelUsage {
                        input_tokens: 2,
                        output_tokens: 3,
                        cost_usd: 0.1,
                    },
                },
                ModelStreamEvent::Usage {
                    usage: ModelUsage {
                        input_tokens: 2,
                        output_tokens: 5,
                        cost_usd: 0.2,
                    },
                },
                ModelStreamEvent::TextDelta {
                    text: "done".to_string(),
                },
            ])]),
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            observer,
        );

        runtime
            .next_turn(default_request("session-usage", ResponseFormat::Text))
            .await?;

        let snapshot = runtime.budget_snapshot();
        assert_eq!(snapshot.consumed.input_tokens, 2);
        assert_eq!(snapshot.consumed.output_tokens, 5);
        assert_eq!(snapshot.consumed.cost_usd, 0.2);
        Ok(())
    }

    #[tokio::test]
    async fn model_runtime_resets_inactivity_timeout_on_provider_activity() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let runtime = ModelRuntime::new(
            TimedProvider::new(vec![
                TimedAction::Event(ModelStreamEvent::MessageId {
                    value: "resp-activity".to_string(),
                }),
                TimedAction::Sleep(Duration::from_millis(40)),
                TimedAction::Activity,
                TimedAction::Sleep(Duration::from_millis(40)),
                TimedAction::Event(ModelStreamEvent::TextDelta {
                    text: "done".to_string(),
                }),
                TimedAction::Event(ModelStreamEvent::Stop {
                    reason: ModelFinishReason::Completed,
                }),
                TimedAction::Event(ModelStreamEvent::Usage {
                    usage: ModelUsage::default(),
                }),
            ]),
            ModelRetryPolicy {
                max_attempts: 1,
                base_backoff_ms: 1,
                stream_timeout_ms: 500,
                inactivity_timeout_ms: 50,
            },
            ModelBudget::default(),
            observer,
        );

        let turn = runtime
            .next_turn(default_request("session-activity", ResponseFormat::Text))
            .await?;

        assert_eq!(turn.assistant_message.content, "done");
        assert_eq!(turn.finish_reason, ModelFinishReason::Completed);
        Ok(())
    }

    #[tokio::test]
    async fn model_runtime_returns_after_stop_without_waiting_for_stream_close() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let runtime = ModelRuntime::new(
            TimedProvider::new(vec![
                TimedAction::Event(ModelStreamEvent::ToolCall {
                    call: ToolCallRecord {
                        id: "call-1".to_string(),
                        name: "security_repo_audit".to_string(),
                        input: json!({"repo":"demo"}),
                        assistant_message_id: None,
                        assistant_provider_response_id: None,
                    },
                }),
                TimedAction::Event(ModelStreamEvent::Stop {
                    reason: ModelFinishReason::ToolCalls,
                }),
                TimedAction::Sleep(Duration::from_millis(200)),
            ]),
            ModelRetryPolicy {
                max_attempts: 1,
                base_backoff_ms: 1,
                stream_timeout_ms: 500,
                inactivity_timeout_ms: 50,
            },
            ModelBudget::default(),
            observer,
        );

        let turn = tokio::time::timeout(
            Duration::from_millis(100),
            runtime.next_turn(default_request("session-stop", ResponseFormat::Text)),
        )
        .await
        .expect("runtime should return promptly after stop event")?;

        assert_eq!(turn.finish_reason, ModelFinishReason::ToolCalls);
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].name, "security_repo_audit");
        Ok(())
    }

    #[tokio::test]
    async fn model_runtime_times_out_without_provider_activity() {
        let observer = InMemoryObserver::shared();
        let runtime = ModelRuntime::new(
            TimedProvider::new(vec![TimedAction::Sleep(Duration::from_millis(80))]),
            ModelRetryPolicy {
                max_attempts: 1,
                base_backoff_ms: 1,
                stream_timeout_ms: 500,
                inactivity_timeout_ms: 50,
            },
            ModelBudget::default(),
            observer,
        );

        let error = runtime
            .next_turn(default_request("session-timeout", ResponseFormat::Text))
            .await
            .expect_err("provider should time out after sustained inactivity");
        assert!(
            error.to_string().contains("model stream became inactive"),
            "unexpected error: {error}"
        );
    }
}
