use parking_lot::{Mutex, RwLock};
use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use kheish_core::{HookDispatcher, ToolCatalog, ToolExecutor};
use kheish_types::{
    ContextUpdate, HookDecision, HookDispatchOutcome, HookEventName, HookInvocation,
    StructuredFieldSchema, StructuredValueKind, ToolCallRecord, ToolDefinition, ToolResultRecord,
    ToolSurfaceFilter,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};

use crate::execution::{
    current_cancellation_token, current_execution_scope, interrupted_error, scope_execution,
};
use crate::observability::{
    RuntimeObserver, TraceEvent, TraceEventKind, external_action_trace,
    failed_external_action_outcome, safe_url_audit_target,
};
use crate::redact_text;

/// The supported primitive input kinds for tool schemas.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInputKind {
    /// Any JSON value.
    Any,
    /// A JSON string.
    String,
    /// A JSON number.
    Number,
    /// A JSON boolean.
    Boolean,
    /// A JSON object.
    Object,
    /// A JSON array.
    Array,
}

/// A single named field in a tool input schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSchemaField {
    /// The field name.
    pub name: String,
    /// The expected field kind.
    pub kind: ToolInputKind,
    /// The optional item kind for array fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_kind: Option<ToolInputKind>,
    /// Optional recursive schema used when primitive kinds are not descriptive enough.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_schema: Option<StructuredFieldSchema>,
    /// Whether the field is required.
    pub required: bool,
    /// The optional field description exposed to model providers.
    pub description: Option<String>,
}

/// A minimal object schema for tool inputs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSchema {
    /// The named fields validated on the input object.
    pub fields: Vec<ToolSchemaField>,
}

/// The sandbox profile requested by a tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProfile {
    /// No special restriction is required.
    Inherited,
    /// File reads only.
    ReadOnly,
    /// Workspace writes are allowed.
    WorkspaceWrite,
    /// Network access is allowed.
    NetworkEnabled,
}

/// Static metadata about a registered tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    /// The tool name.
    pub name: String,
    /// A short human-readable description of the tool behavior.
    pub description: String,
    /// The tool input schema.
    pub schema: ToolSchema,
    /// The execution timeout in milliseconds.
    pub timeout_ms: u64,
    /// The required sandbox profile.
    pub sandbox: SandboxProfile,
    /// Whether the tool may execute in parallel with other tools.
    pub allows_parallel: bool,
}

/// Context passed to a tool execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolContext {
    /// The tool call identifier.
    pub call_id: String,
    /// The required sandbox profile.
    pub sandbox: SandboxProfile,
    /// Additional execution metadata.
    pub metadata: Value,
}

/// One structured tool execution result together with normalized side effects.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolExecutionOutput {
    /// The JSON payload returned to the model.
    pub output: Value,
    /// The normalized context updates emitted by the tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_updates: Vec<ContextUpdate>,
    /// Additional hook-provided context injected after the tool result.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hook_contexts: Vec<String>,
}

impl ToolExecutionOutput {
    /// Creates one tool output without extra context updates.
    pub fn json(output: Value) -> Self {
        Self {
            output,
            context_updates: Vec::new(),
            hook_contexts: Vec::new(),
        }
    }

    /// Creates one tool output with explicit context updates.
    pub fn with_updates(output: Value, context_updates: Vec<ContextUpdate>) -> Self {
        Self {
            output,
            context_updates,
            hook_contexts: Vec::new(),
        }
    }
}

/// A tool implementation that can execute structured JSON input.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Returns the static descriptor for the tool.
    fn descriptor(&self) -> ToolDescriptor;

    /// Executes the tool.
    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput>;
}

/// Connects deferred, scope-bound MCP servers on demand for the current turn.
///
/// Some MCP servers (OAuth-backed HTTP servers) cannot be initialized at the
/// daemon's ambient boot: brokering their credentials requires a per-run
/// execution scope that only exists once a session run starts. Such servers are
/// registered but stay disconnected with no tools, so the model never sees
/// them. This hook lets the runtime ask the MCP layer — which the runtime crate
/// cannot depend on directly — to initialize those servers, enumerate their
/// tools, and register the tool adapters on `runtime` while the caller's
/// execution scope is active, so the tools become visible and callable this
/// same turn.
///
/// Implementations must be idempotent (a server already connected with tools is
/// left untouched) and best-effort (a server that fails to connect is skipped,
/// never surfaced as a run failure).
#[async_trait]
pub trait McpScopedHydrator: Send + Sync {
    /// Hydrates any deferred MCP servers among `allowed_servers` into `runtime`,
    /// running inside the caller's active execution scope.
    async fn hydrate_scoped_mcp_tools(
        &self,
        runtime: &ToolRuntime,
        allowed_servers: &std::collections::BTreeSet<String>,
    );
}

/// A lifecycle hook around tool execution.
#[async_trait]
pub trait ToolHook: Send + Sync {
    /// Runs before the tool executes.
    async fn before(&self, descriptor: &ToolDescriptor, call: &ToolCallRecord) -> Result<()>;

    /// Runs after the tool executes.
    async fn after(
        &self,
        descriptor: &ToolDescriptor,
        call: &ToolCallRecord,
        result: &ToolResultRecord,
    ) -> Result<()>;
}

/// A registry-backed tool runtime with validation, hooks, and timeouts.
pub struct ToolRuntime {
    /// Shared, lockable so MCP servers connected at runtime can register and
    /// unregister their tool adapters after the runtime is frozen in an `Arc`.
    registry: Arc<RwLock<BTreeMap<String, Arc<dyn Tool>>>>,
    hooks: Vec<Arc<dyn ToolHook>>,
    hook_dispatcher: Option<Arc<dyn HookDispatcher>>,
    observer: Arc<dyn RuntimeObserver>,
    limits: Arc<RwLock<ToolRuntimeLimits>>,
}

/// One filtered view over a shared tool runtime.
#[derive(Clone)]
pub struct ScopedToolRuntime {
    inner: Arc<ToolRuntime>,
    filter: ToolSurfaceFilter,
}

/// Runtime-enforced limits shared by every registered tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRuntimeLimits {
    /// Maximum serialized JSON bytes accepted for one tool input.
    pub max_input_bytes: usize,
    /// Maximum serialized JSON bytes returned to the model from one tool output.
    pub max_output_bytes: usize,
    /// Maximum serialized JSON bytes for one full tool result envelope.
    #[serde(default = "default_max_result_envelope_bytes")]
    pub max_result_envelope_bytes: usize,
    /// Maximum timeout any one tool call may consume.
    pub max_timeout_ms: u64,
    /// Maximum number of tool calls executed concurrently in one batch.
    pub max_parallel_tools: usize,
    /// Maximum number of tool calls accepted in one model tool-use batch.
    #[serde(default = "default_max_calls_per_turn")]
    pub max_calls_per_turn: usize,
    /// Maximum serialized JSON bytes returned cumulatively in one model tool-use batch.
    #[serde(default = "default_max_cumulative_output_bytes")]
    pub max_cumulative_output_bytes: usize,
    /// Maximum serialized JSON bytes across full tool result envelopes in one tool-use batch.
    #[serde(default = "default_max_cumulative_result_envelope_bytes")]
    pub max_cumulative_result_envelope_bytes: usize,
    /// Highest sandbox profile this runtime is allowed to execute.
    pub max_sandbox: SandboxProfile,
}

fn default_max_calls_per_turn() -> usize {
    256
}

fn default_max_cumulative_output_bytes() -> usize {
    64 * 1024 * 1024
}

fn default_max_result_envelope_bytes() -> usize {
    24 * 1024 * 1024
}

fn default_max_cumulative_result_envelope_bytes() -> usize {
    96 * 1024 * 1024
}

impl Default for ToolRuntimeLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * 1024 * 1024,
            max_output_bytes: 16 * 1024 * 1024,
            max_result_envelope_bytes: default_max_result_envelope_bytes(),
            max_timeout_ms: 180_000,
            max_parallel_tools: 16,
            max_calls_per_turn: default_max_calls_per_turn(),
            max_cumulative_output_bytes: default_max_cumulative_output_bytes(),
            max_cumulative_result_envelope_bytes: default_max_cumulative_result_envelope_bytes(),
            max_sandbox: SandboxProfile::NetworkEnabled,
        }
    }
}

impl ToolRuntimeLimits {
    /// Validates that every runtime limit is usable before applying it live.
    pub fn validate(&self) -> Result<()> {
        ensure_positive_limit("max_input_bytes", self.max_input_bytes)?;
        ensure_positive_limit("max_output_bytes", self.max_output_bytes)?;
        ensure_positive_limit("max_result_envelope_bytes", self.max_result_envelope_bytes)?;
        ensure_positive_limit("max_timeout_ms", self.max_timeout_ms)?;
        ensure_positive_limit("max_parallel_tools", self.max_parallel_tools)?;
        ensure_positive_limit("max_calls_per_turn", self.max_calls_per_turn)?;
        ensure_positive_limit(
            "max_cumulative_output_bytes",
            self.max_cumulative_output_bytes,
        )?;
        ensure_positive_limit(
            "max_cumulative_result_envelope_bytes",
            self.max_cumulative_result_envelope_bytes,
        )?;
        Ok(())
    }
}

fn ensure_positive_limit<T>(field: &'static str, value: T) -> Result<()>
where
    T: Copy + From<u8> + PartialEq,
{
    if value == T::from(0) {
        bail!("tool runtime limit {field} must be greater than zero");
    }
    Ok(())
}

#[derive(Clone)]
struct ToolTurnBudget {
    limits: ToolRuntimeLimits,
    state: Arc<Mutex<ToolTurnBudgetState>>,
}

#[derive(Debug, Default)]
struct ToolTurnBudgetState {
    accepted_calls: usize,
    cumulative_output_bytes: usize,
    cumulative_result_envelope_bytes: usize,
}

impl ToolTurnBudget {
    fn new(limits: ToolRuntimeLimits) -> Self {
        Self {
            limits,
            state: Arc::new(Mutex::new(ToolTurnBudgetState::default())),
        }
    }

    fn try_accept_call(&self) -> bool {
        let mut state = self.state.lock();
        if state.accepted_calls >= self.limits.max_calls_per_turn.max(1) {
            return false;
        }
        state.accepted_calls += 1;
        true
    }

    fn try_reserve_result(
        &self,
        output_bytes: usize,
        envelope_bytes: usize,
    ) -> Result<(), ToolTurnBudgetQuota> {
        let mut state = self.state.lock();
        let output_limit = self.limits.max_cumulative_output_bytes.max(1);
        let envelope_limit = self.limits.max_cumulative_result_envelope_bytes.max(1);
        let Some(next_output) = state.cumulative_output_bytes.checked_add(output_bytes) else {
            return Err(ToolTurnBudgetQuota::CumulativeOutputBytes);
        };
        if next_output > output_limit {
            return Err(ToolTurnBudgetQuota::CumulativeOutputBytes);
        };
        let Some(next_envelope) = state
            .cumulative_result_envelope_bytes
            .checked_add(envelope_bytes)
        else {
            return Err(ToolTurnBudgetQuota::CumulativeResultEnvelopeBytes);
        };
        if next_envelope > envelope_limit {
            return Err(ToolTurnBudgetQuota::CumulativeResultEnvelopeBytes);
        }
        state.cumulative_output_bytes = next_output;
        state.cumulative_result_envelope_bytes = next_envelope;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolTurnBudgetQuota {
    CumulativeOutputBytes,
    CumulativeResultEnvelopeBytes,
}

fn build_tool_result_record(
    call_id: String,
    output: Value,
    is_error: bool,
    tool_name: Option<String>,
    context_updates: Vec<ContextUpdate>,
    hook_contexts: Vec<String>,
) -> ToolResultRecord {
    ToolResultRecord {
        call_id,
        output,
        is_error,
        tool_name,
        offset: None,
        timestamp_ms: None,
        context_updates,
        hook_contexts,
    }
}

impl ToolRuntime {
    /// Creates an empty tool runtime.
    pub fn new(observer: Arc<dyn RuntimeObserver>) -> Self {
        Self::with_limits(observer, ToolRuntimeLimits::default())
    }

    /// Creates an empty tool runtime with explicit runtime limits.
    pub fn with_limits(observer: Arc<dyn RuntimeObserver>, limits: ToolRuntimeLimits) -> Self {
        Self {
            registry: Arc::new(RwLock::new(BTreeMap::new())),
            hooks: Vec::new(),
            hook_dispatcher: None,
            observer,
            limits: Arc::new(RwLock::new(limits)),
        }
    }

    /// Returns the currently configured runtime limits.
    pub fn limits(&self) -> ToolRuntimeLimits {
        self.limits.read().clone()
    }

    /// Replaces the runtime limits used by future tool batches.
    pub fn set_limits(&self, limits: ToolRuntimeLimits) -> Result<()> {
        limits.validate()?;
        *self.limits.write() = limits;
        Ok(())
    }

    /// Registers a tool instance.
    pub fn register<T>(&mut self, tool: T)
    where
        T: Tool + 'static,
    {
        self.registry
            .write()
            .insert(tool.descriptor().name.clone(), Arc::new(tool));
    }

    /// Registers a tool instance and fails if the name is already present.
    pub fn try_register_unique<T>(&mut self, tool: T) -> Result<()>
    where
        T: Tool + 'static,
    {
        let descriptor = tool.descriptor();
        let name = descriptor.name.clone();
        let mut registry = self.registry.write();
        if registry.contains_key(&name) {
            bail!("tool `{name}` is already registered");
        }
        registry.insert(name, Arc::new(tool));
        Ok(())
    }

    /// Registers a tool on a shared (frozen) runtime, failing on collisions.
    ///
    /// This is the hot-add path: MCP servers connected while the daemon runs
    /// register their adapters here, and every existing scoped view sees them
    /// on its next lookup.
    pub fn register_dynamic(&self, tool: Arc<dyn Tool>) -> Result<()> {
        let name = tool.descriptor().name.clone();
        let mut registry = self.registry.write();
        if registry.contains_key(&name) {
            bail!("tool `{name}` is already registered");
        }
        registry.insert(name, tool);
        Ok(())
    }

    /// Removes a dynamically registered tool; returns whether it existed.
    pub fn unregister_dynamic(&self, name: &str) -> bool {
        self.registry.write().remove(name).is_some()
    }

    /// Returns one registered tool by name.
    fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.registry.read().get(name).cloned()
    }

    /// Registers a hook executed around every tool invocation.
    pub fn add_hook<H>(&mut self, hook: H)
    where
        H: ToolHook + 'static,
    {
        self.hooks.push(Arc::new(hook));
    }

    /// Installs the shared lifecycle hook dispatcher used for configurable hook execution.
    pub fn set_hook_dispatcher(&mut self, dispatcher: Arc<dyn HookDispatcher>) {
        self.hook_dispatcher = Some(dispatcher);
    }

    /// Clones the registry-backed runtime while removing the configurable hook dispatcher.
    ///
    /// This is used for isolated internal agents or hook executors that must avoid
    /// recursively re-triggering the daemon-level hook pipeline.
    pub fn clone_without_hook_dispatcher(&self) -> Self {
        Self {
            // The registry Arc is shared: tools added at runtime stay visible
            // to hook executors and isolated internal agents too.
            registry: self.registry.clone(),
            hooks: self.hooks.clone(),
            hook_dispatcher: None,
            observer: self.observer.clone(),
            limits: self.limits.clone(),
        }
    }

    /// Returns all registered tool descriptors.
    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.registry
            .read()
            .values()
            .map(|tool| tool.descriptor())
            .collect()
    }

    /// Returns one filtered runtime view suitable for isolated agents.
    pub fn scoped(self: &Arc<Self>, filter: ToolSurfaceFilter) -> ScopedToolRuntime {
        ScopedToolRuntime {
            inner: self.clone(),
            filter,
        }
    }

    /// Executes a single tool call using the public runtime API.
    pub async fn execute(&self, call: &ToolCallRecord) -> Result<ToolResultRecord> {
        <Self as ToolExecutor>::execute(self, call).await
    }

    /// Executes a tool batch, using parallel execution when every tool allows it.
    pub async fn execute_batch(&self, calls: &[ToolCallRecord]) -> Result<Vec<ToolResultRecord>> {
        let Some(_) = calls.first() else {
            return Ok(Vec::new());
        };
        let execution_scope = current_execution_scope();
        let cancellation = current_cancellation_token();
        let limits = self.limits();
        let turn_budget = ToolTurnBudget::new(limits.clone());

        let all_parallel = {
            let registry = self.registry.read();
            calls.iter().all(|call| {
                registry
                    .get(&call.name)
                    .map(|tool| tool.descriptor().allows_parallel)
                    .unwrap_or(false)
            })
        };
        if calls.len() == 1 || !all_parallel {
            let mut results = Vec::with_capacity(calls.len());
            for call in calls {
                results.push(
                    self.execute_call_with_budget(call, turn_budget.clone())
                        .await?,
                );
            }
            return Ok(results);
        }

        let max_parallel_tools = limits.max_parallel_tools.max(1);
        let mut results = Vec::with_capacity(calls.len());
        for chunk in calls.chunks(max_parallel_tools) {
            results.extend(
                self.execute_parallel_chunk(
                    chunk,
                    execution_scope.clone(),
                    cancellation.clone(),
                    turn_budget.clone(),
                )
                .await?,
            );
        }
        Ok(results)
    }

    async fn execute_parallel_chunk(
        &self,
        calls: &[ToolCallRecord],
        execution_scope: Option<crate::ExecutionScope>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
        turn_budget: ToolTurnBudget,
    ) -> Result<Vec<ToolResultRecord>> {
        let mut tasks: Vec<(usize, JoinHandle<Result<ToolResultRecord>>)> =
            Vec::with_capacity(calls.len());
        for (index, call) in calls.iter().cloned().enumerate() {
            let descriptor = self
                .tool(&call.name)
                .map(|tool| tool.descriptor())
                .unwrap_or_else(|| unavailable_tool_descriptor(&call));
            if !turn_budget.try_accept_call() {
                let observer = self.observer.clone();
                let limits = turn_budget.limits.clone();
                tasks.push((
                    index,
                    tokio::spawn(async move {
                        runtime_quota_rejection_record(
                            observer,
                            &descriptor,
                            &call,
                            "max_calls_per_turn",
                            format!(
                                "tool call batch exceeded runtime limit of {} calls",
                                limits.max_calls_per_turn.max(1)
                            ),
                            json!({
                                "limit_calls": limits.max_calls_per_turn.max(1),
                            }),
                        )
                    }),
                ));
                continue;
            }
            let Some(tool) = self.tool(&call.name) else {
                tasks.push((
                    index,
                    tokio::spawn(async move {
                        Ok(build_tool_result_record(
                            call.id,
                            serde_json::json!({"error": "tool not found"}),
                            true,
                            Some(call.name),
                            Vec::new(),
                            Vec::new(),
                        ))
                    }),
                ));
                continue;
            };
            let descriptor = tool.descriptor();
            let hooks = self.hooks.clone();
            let hook_dispatcher = self.hook_dispatcher.clone();
            let observer = self.observer.clone();
            let limits = turn_budget.limits.clone();
            let execution_scope = execution_scope.clone();
            let cancellation = cancellation.clone();
            let turn_budget = turn_budget.clone();
            tasks.push((
                index,
                tokio::spawn(async move {
                    match (execution_scope, cancellation) {
                        (Some(mut scope), Some(cancellation)) => {
                            scope.tool_call_id = Some(call.id.clone());
                            scope_execution(scope, cancellation, async move {
                                execute_registered_tool(
                                    tool,
                                    descriptor,
                                    hooks,
                                    hook_dispatcher,
                                    observer,
                                    limits,
                                    turn_budget,
                                    call,
                                )
                                .await
                            })
                            .await
                        }
                        _ => {
                            execute_registered_tool(
                                tool,
                                descriptor,
                                hooks,
                                hook_dispatcher,
                                observer,
                                limits,
                                turn_budget,
                                call,
                            )
                            .await
                        }
                    }
                }),
            ));
        }

        let mut results = vec![None; calls.len()];
        for (index, task) in tasks {
            let record = task.await.map_err(|error| anyhow!(error))??;
            results[index] = Some(record);
        }
        Ok(results
            .into_iter()
            .map(|record| {
                record.unwrap_or_else(|| {
                    build_tool_result_record(
                        "missing-result".to_string(),
                        serde_json::json!({"error": "tool result missing"}),
                        true,
                        None,
                        Vec::new(),
                        Vec::new(),
                    )
                })
            })
            .collect())
    }

    async fn execute_call(&self, call: &ToolCallRecord) -> Result<ToolResultRecord> {
        self.execute_call_with_budget(call, ToolTurnBudget::new(self.limits()))
            .await
    }

    async fn execute_call_with_budget(
        &self,
        call: &ToolCallRecord,
        turn_budget: ToolTurnBudget,
    ) -> Result<ToolResultRecord> {
        let descriptor = self
            .tool(&call.name)
            .map(|tool| tool.descriptor())
            .unwrap_or_else(|| unavailable_tool_descriptor(call));
        if !turn_budget.try_accept_call() {
            return runtime_quota_rejection_record(
                self.observer.clone(),
                &descriptor,
                call,
                "max_calls_per_turn",
                format!(
                    "tool call batch exceeded runtime limit of {} calls",
                    turn_budget.limits.max_calls_per_turn.max(1)
                ),
                json!({
                    "limit_calls": turn_budget.limits.max_calls_per_turn.max(1),
                }),
            );
        }
        let Some(tool) = self.tool(&call.name) else {
            return Ok(build_tool_result_record(
                call.id.clone(),
                serde_json::json!({"error": "tool not found"}),
                true,
                Some(call.name.clone()),
                Vec::new(),
                Vec::new(),
            ));
        };
        let execution = async {
            execute_registered_tool(
                tool.clone(),
                tool.descriptor(),
                self.hooks.clone(),
                self.hook_dispatcher.clone(),
                self.observer.clone(),
                turn_budget.limits.clone(),
                turn_budget,
                call.clone(),
            )
            .await
        };
        match (current_execution_scope(), current_cancellation_token()) {
            (Some(mut scope), Some(cancellation)) => {
                scope.tool_call_id = Some(call.id.clone());
                scope_execution(scope, cancellation, execution).await
            }
            _ => execution.await,
        }
    }
}

impl ScopedToolRuntime {
    fn allows(&self, tool_name: &str) -> bool {
        self.filter.allows(tool_name)
    }

    fn unavailable_record(call: &ToolCallRecord) -> ToolResultRecord {
        build_tool_result_record(
            call.id.clone(),
            serde_json::json!({
                "error": format!("tool {} is not available for this agent", call.name),
            }),
            true,
            Some(call.name.clone()),
            Vec::new(),
            Vec::new(),
        )
    }

    fn unavailable_descriptor(&self, call: &ToolCallRecord) -> ToolDescriptor {
        self.inner
            .tool(&call.name)
            .map(|tool| tool.descriptor())
            .unwrap_or_else(|| ToolDescriptor {
                name: call.name.clone(),
                description: "Unavailable scoped tool.".to_string(),
                schema: ToolSchema::default(),
                timeout_ms: 1,
                sandbox: SandboxProfile::Inherited,
                allows_parallel: false,
            })
    }

    fn audited_unavailable_record(&self, call: &ToolCallRecord) -> Result<ToolResultRecord> {
        let descriptor = self.unavailable_descriptor(call);
        let mut started = TraceEvent::new(TraceEventKind::ToolStarted {
            tool_name: descriptor.name.clone(),
            call_id: call.id.clone(),
        });
        if started.tool_call_id.is_none() {
            started.tool_call_id = Some(call.id.clone());
        }
        self.inner.observer.record(started);
        let audit_target = external_tool_audit_target(&descriptor, call);
        let request_digest = kheish_codec::digest_serialize(&call.input)?;
        if let Some(target) = audit_target.as_deref() {
            let mut trace = external_action_trace(
                "request",
                "tool",
                target.to_string(),
                Some(request_digest.clone()),
                None,
                None,
            );
            if trace.tool_call_id.is_none() {
                trace.tool_call_id = Some(call.id.clone());
            }
            self.inner.observer.record_external_action(trace)?;
        }
        finish_registered_tool_result(
            self.inner.observer.clone(),
            &descriptor,
            audit_target.as_deref(),
            &request_digest,
            Self::unavailable_record(call),
        )
    }
}

impl ToolCatalog for ToolRuntime {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.descriptors()
            .into_iter()
            .map(|descriptor| descriptor.definition())
            .collect()
    }
}

impl ToolCatalog for ScopedToolRuntime {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner
            .definitions()
            .into_iter()
            .filter(|tool| self.allows(&tool.name))
            .collect()
    }
}

#[async_trait]
impl ToolExecutor for ToolRuntime {
    async fn execute(&self, call: &ToolCallRecord) -> Result<ToolResultRecord> {
        self.execute_call(call).await
    }

    async fn execute_batch(&self, calls: &[ToolCallRecord]) -> Result<Vec<ToolResultRecord>> {
        ToolRuntime::execute_batch(self, calls).await
    }
}

#[async_trait]
impl ToolExecutor for ScopedToolRuntime {
    async fn execute(&self, call: &ToolCallRecord) -> Result<ToolResultRecord> {
        if !self.allows(&call.name) {
            return self.audited_unavailable_record(call);
        }
        self.inner.execute(call).await
    }

    async fn execute_batch(&self, calls: &[ToolCallRecord]) -> Result<Vec<ToolResultRecord>> {
        let mut allowed = Vec::new();
        let mut denied = BTreeMap::new();
        for (index, call) in calls.iter().enumerate() {
            if self.allows(&call.name) {
                allowed.push((index, call.clone()));
            } else {
                denied.insert(index, self.audited_unavailable_record(call)?);
            }
        }
        if allowed.is_empty() {
            return Ok(denied.into_values().collect());
        }
        let allowed_calls: Vec<ToolCallRecord> =
            allowed.iter().map(|(_, call)| call.clone()).collect();
        let allowed_results = self.inner.execute_batch(&allowed_calls).await?;
        let mut ordered: Vec<Option<ToolResultRecord>> = vec![None; calls.len()];
        for (index, result) in denied {
            ordered[index] = Some(result);
        }
        for ((index, _), result) in allowed.into_iter().zip(allowed_results.into_iter()) {
            ordered[index] = Some(result);
        }
        Ok(ordered
            .into_iter()
            .map(|record| record.expect("scoped tool runtime should fill every result slot"))
            .collect())
    }
}

async fn execute_registered_tool(
    tool: Arc<dyn Tool>,
    descriptor: ToolDescriptor,
    hooks: Vec<Arc<dyn ToolHook>>,
    hook_dispatcher: Option<Arc<dyn HookDispatcher>>,
    observer: Arc<dyn RuntimeObserver>,
    limits: ToolRuntimeLimits,
    turn_budget: ToolTurnBudget,
    mut call: ToolCallRecord,
) -> Result<ToolResultRecord> {
    if let Some(cancellation) = current_cancellation_token() {
        if cancellation.is_cancelled() {
            return Err(interrupted_error());
        }
    }
    observer.record(TraceEvent::new(TraceEventKind::ToolStarted {
        tool_name: descriptor.name.clone(),
        call_id: call.id.clone(),
    }));

    let audit_target = external_tool_audit_target(&descriptor, &call);
    let request_digest = kheish_codec::digest_serialize(&call.input)?;
    if let Some(target) = audit_target.as_deref() {
        let mut trace = external_action_trace(
            "request",
            "tool",
            target.to_string(),
            Some(request_digest.clone()),
            None,
            None,
        );
        if trace.tool_call_id.is_none() {
            trace.tool_call_id = Some(call.id.clone());
        }
        observer.record_external_action(trace)?;
    }

    if !limits.max_sandbox.allows(&descriptor.sandbox) {
        let record = build_tool_result_record(
            call.id,
            json!({
                "error": format!(
                    "tool sandbox {:?} exceeds runtime sandbox limit {:?}",
                    descriptor.sandbox, limits.max_sandbox
                ),
                "tool": descriptor.name.clone(),
                "quota": "sandbox",
            }),
            true,
            Some(descriptor.name.clone()),
            Vec::new(),
            Vec::new(),
        );
        return finalize_registered_tool_result(
            observer,
            &descriptor,
            audit_target.as_deref(),
            &request_digest,
            &limits,
            &turn_budget,
            record,
        );
    }

    let input_bytes = serialized_json_len(&call.input)?;
    if input_bytes > limits.max_input_bytes {
        let record = build_tool_result_record(
            call.id,
            json!({
                "error": format!(
                    "tool input exceeded runtime limit of {} bytes",
                    limits.max_input_bytes
                ),
                "tool": descriptor.name.clone(),
                "quota": "input_bytes",
                "limit_bytes": limits.max_input_bytes,
                "actual_bytes": input_bytes,
            }),
            true,
            Some(descriptor.name.clone()),
            Vec::new(),
            Vec::new(),
        );
        return finalize_registered_tool_result(
            observer,
            &descriptor,
            audit_target.as_deref(),
            &request_digest,
            &limits,
            &turn_budget,
            record,
        );
    }

    call.input = normalize_tool_input_numbers(call.input);

    if let Err(error) = validate_tool_input(&call.input, &descriptor.schema) {
        let record = build_tool_result_record(
            call.id,
            serde_json::json!({"error": redact_text(&error.to_string())}),
            true,
            Some(descriptor.name.clone()),
            Vec::new(),
            Vec::new(),
        );
        return finalize_registered_tool_result(
            observer,
            &descriptor,
            audit_target.as_deref(),
            &request_digest,
            &limits,
            &turn_budget,
            record,
        );
    }

    let pre_hook = dispatch_tool_hook(
        hook_dispatcher.as_ref(),
        HookEventName::PreToolUse,
        &descriptor,
        &call,
        None,
    )
    .await?;
    if matches!(pre_hook.decision, Some(HookDecision::Block)) || !pre_hook.continue_execution {
        let record = build_tool_result_record(
            call.id.clone(),
            serde_json::json!({
                "error": redact_text(&pre_hook
                    .stop_reason
                    .clone()
                    .unwrap_or_else(|| "tool execution blocked by hook".to_string())),
                "tool": descriptor.name.clone(),
                "hook_blocked": true,
            }),
            true,
            Some(descriptor.name.clone()),
            Vec::new(),
            pre_hook.additional_contexts,
        );
        return finalize_registered_tool_result(
            observer,
            &descriptor,
            audit_target.as_deref(),
            &request_digest,
            &limits,
            &turn_budget,
            record,
        );
    }
    if pre_hook.updated_input.is_some() {
        let record = build_tool_result_record(
            call.id.clone(),
            serde_json::json!({
                "error": "pre_tool_use hooks cannot update tool input after permission evaluation; use permission_request hooks or approval updated_input instead",
                "tool": descriptor.name.clone(),
                "hook_blocked": true,
            }),
            true,
            Some(descriptor.name.clone()),
            Vec::new(),
            pre_hook.additional_contexts,
        );
        return finalize_registered_tool_result(
            observer,
            &descriptor,
            audit_target.as_deref(),
            &request_digest,
            &limits,
            &turn_budget,
            record,
        );
    }

    for hook in &hooks {
        if let Err(error) = hook.before(&descriptor, &call).await {
            let record = build_tool_result_record(
                call.id,
                serde_json::json!({"error": redact_text(&error.to_string())}),
                true,
                Some(descriptor.name.clone()),
                Vec::new(),
                Vec::new(),
            );
            return finalize_registered_tool_result(
                observer,
                &descriptor,
                audit_target.as_deref(),
                &request_digest,
                &limits,
                &turn_budget,
                record,
            );
        }
    }

    let result = tokio::select! {
        result = timeout(
            Duration::from_millis(descriptor.timeout_ms.min(limits.max_timeout_ms).max(1)),
            tool.execute(
                ToolContext {
                    call_id: call.id.clone(),
                    sandbox: descriptor.sandbox.clone(),
                    metadata: build_tool_context_metadata(current_execution_scope(), &call),
                },
                call.input.clone(),
            ),
        ) => result,
        _ = async {
            if let Some(cancellation) = current_cancellation_token() {
                cancellation.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => {
            let record = build_tool_result_record(
                call.id.clone(),
                serde_json::json!({
                    "error": interrupted_error().to_string(),
                    "tool": descriptor.name.clone(),
                    "interrupted": true,
                }),
                true,
                Some(descriptor.name.clone()),
                Vec::new(),
                Vec::new(),
            );
            finalize_registered_tool_result(
                observer,
                &descriptor,
                audit_target.as_deref(),
                &request_digest,
                &limits,
                &turn_budget,
                record,
            )?;
            return Err(interrupted_error());
        },
    };

    let mut record = match result {
        Ok(Ok(output)) => build_tool_result_record(
            call.id.clone(),
            output.output,
            false,
            Some(descriptor.name.clone()),
            output.context_updates,
            output.hook_contexts,
        ),
        Ok(Err(error)) => build_tool_result_record(
            call.id.clone(),
            serde_json::json!({"error": redact_text(&error.to_string())}),
            true,
            Some(descriptor.name.clone()),
            Vec::new(),
            Vec::new(),
        ),
        Err(_) => build_tool_result_record(
            call.id.clone(),
            serde_json::json!({"error": "tool execution timed out"}),
            true,
            Some(descriptor.name.clone()),
            Vec::new(),
            Vec::new(),
        ),
    };
    if let Some(dispatcher) = hook_dispatcher.as_ref() {
        if record.is_error {
            let failure_hook = dispatch_tool_hook(
                Some(dispatcher),
                HookEventName::PostToolUseFailure,
                &descriptor,
                &call,
                Some(&record),
            )
            .await?;
            record
                .hook_contexts
                .extend(failure_hook.additional_contexts);
            if matches!(failure_hook.decision, Some(HookDecision::Block))
                || !failure_hook.continue_execution
            {
                record.is_error = true;
                record.output = serde_json::json!({
                    "error": redact_text(&failure_hook
                        .stop_reason
                        .unwrap_or_else(|| "tool failure continuation blocked by hook".to_string())),
                    "tool": descriptor.name.clone(),
                    "hook_blocked": true,
                });
            }
        } else {
            let post_hook = dispatch_tool_hook(
                Some(dispatcher),
                HookEventName::PostToolUse,
                &descriptor,
                &call,
                Some(&record),
            )
            .await?;
            if let Some(updated_output) = post_hook.updated_output {
                record.output = updated_output;
            }
            record.hook_contexts.extend(post_hook.additional_contexts);
            if matches!(post_hook.decision, Some(HookDecision::Block))
                || !post_hook.continue_execution
            {
                record.is_error = true;
                record.output = serde_json::json!({
                    "error": redact_text(&post_hook
                        .stop_reason
                        .unwrap_or_else(|| "tool continuation blocked by hook".to_string())),
                    "tool": descriptor.name.clone(),
                    "hook_blocked": true,
                });
            }
        }
    }

    for hook in &hooks {
        if let Err(error) = hook.after(&descriptor, &call, &record).await {
            record = build_tool_result_record(
                call.id.clone(),
                serde_json::json!({"error": redact_text(&error.to_string())}),
                true,
                Some(descriptor.name.clone()),
                Vec::new(),
                Vec::new(),
            );
        }
    }

    finalize_registered_tool_result(
        observer,
        &descriptor,
        audit_target.as_deref(),
        &request_digest,
        &limits,
        &turn_budget,
        record,
    )
}

#[derive(Serialize)]
struct ToolResultEnvelope<'a> {
    call_id: &'a str,
    tool_name: Option<&'a str>,
    is_error: bool,
    output: &'a Value,
    context_updates: &'a [ContextUpdate],
    hook_contexts: &'a [String],
}

fn tool_result_envelope(record: &ToolResultRecord) -> ToolResultEnvelope<'_> {
    ToolResultEnvelope {
        call_id: &record.call_id,
        tool_name: record.tool_name.as_deref(),
        is_error: record.is_error,
        output: &record.output,
        context_updates: &record.context_updates,
        hook_contexts: &record.hook_contexts,
    }
}

fn unavailable_tool_descriptor(call: &ToolCallRecord) -> ToolDescriptor {
    ToolDescriptor {
        name: call.name.clone(),
        description: "Unavailable tool.".to_string(),
        schema: ToolSchema::default(),
        timeout_ms: 1,
        sandbox: SandboxProfile::Inherited,
        allows_parallel: false,
    }
}

fn runtime_quota_rejection_record(
    observer: Arc<dyn RuntimeObserver>,
    descriptor: &ToolDescriptor,
    call: &ToolCallRecord,
    quota: &'static str,
    error: String,
    mut extra: Value,
) -> Result<ToolResultRecord> {
    let mut started = TraceEvent::new(TraceEventKind::ToolStarted {
        tool_name: descriptor.name.clone(),
        call_id: call.id.clone(),
    });
    if started.tool_call_id.is_none() {
        started.tool_call_id = Some(call.id.clone());
    }
    observer.record(started);
    let audit_target = external_tool_audit_target(descriptor, call);
    let request_digest = kheish_codec::digest_serialize(&call.input)?;
    if let Some(target) = audit_target.as_deref() {
        let mut trace = external_action_trace(
            "request",
            "tool",
            target.to_string(),
            Some(request_digest.clone()),
            None,
            None,
        );
        if trace.tool_call_id.is_none() {
            trace.tool_call_id = Some(call.id.clone());
        }
        observer.record_external_action(trace)?;
    }
    let object = extra
        .as_object_mut()
        .expect("quota extra must be an object");
    object.insert("error".to_string(), Value::String(error));
    object.insert("tool".to_string(), Value::String(descriptor.name.clone()));
    object.insert("quota".to_string(), Value::String(quota.to_string()));
    let record = build_tool_result_record(
        call.id.clone(),
        extra,
        true,
        Some(descriptor.name.clone()),
        Vec::new(),
        Vec::new(),
    );
    finish_registered_tool_result(
        observer,
        descriptor,
        audit_target.as_deref(),
        &request_digest,
        record,
    )
}

fn finalize_registered_tool_result(
    observer: Arc<dyn RuntimeObserver>,
    descriptor: &ToolDescriptor,
    audit_target: Option<&str>,
    request_digest: &str,
    limits: &ToolRuntimeLimits,
    turn_budget: &ToolTurnBudget,
    mut record: ToolResultRecord,
) -> Result<ToolResultRecord> {
    enforce_output_limit(&mut record, descriptor, limits)?;
    enforce_result_envelope_limit(&mut record, descriptor, limits)?;
    enforce_cumulative_result_limits(&mut record, descriptor, limits, turn_budget)?;
    finish_registered_tool_result(observer, descriptor, audit_target, request_digest, record)
}

fn finish_registered_tool_result(
    observer: Arc<dyn RuntimeObserver>,
    descriptor: &ToolDescriptor,
    audit_target: Option<&str>,
    request_digest: &str,
    record: ToolResultRecord,
) -> Result<ToolResultRecord> {
    if let Some(target) = audit_target {
        let outcome = if record.is_error {
            Some(failed_external_action_outcome(record.output.to_string()))
        } else {
            Some("ok".to_string())
        };
        let mut trace = external_action_trace(
            "response",
            "tool",
            target.to_string(),
            Some(request_digest.to_string()),
            Some(tool_result_envelope_digest(&record)?),
            outcome,
        );
        if trace.tool_call_id.is_none() {
            trace.tool_call_id = Some(record.call_id.clone());
        }
        observer.record_external_action(trace)?;
    }

    let mut finished = TraceEvent::new(TraceEventKind::ToolFinished {
        tool_name: descriptor.name.clone(),
        call_id: record.call_id.clone(),
        is_error: record.is_error,
    });
    if finished.tool_call_id.is_none() {
        finished.tool_call_id = Some(record.call_id.clone());
    }
    observer.record(finished);
    Ok(record)
}

impl SandboxProfile {
    fn allows(&self, requested: &SandboxProfile) -> bool {
        matches!(
            (self, requested),
            (SandboxProfile::NetworkEnabled, _)
                | (
                    SandboxProfile::WorkspaceWrite,
                    SandboxProfile::WorkspaceWrite
                        | SandboxProfile::ReadOnly
                        | SandboxProfile::Inherited
                )
                | (
                    SandboxProfile::ReadOnly,
                    SandboxProfile::ReadOnly | SandboxProfile::Inherited
                )
                | (SandboxProfile::Inherited, SandboxProfile::Inherited)
        )
    }
}

fn serialized_json_len(value: &Value) -> Result<usize> {
    Ok(serde_json::to_vec(value)?.len())
}

fn serialized_tool_result_envelope_len(record: &ToolResultRecord) -> Result<usize> {
    Ok(serde_json::to_vec(&tool_result_envelope(record))?.len())
}

fn tool_result_envelope_digest(record: &ToolResultRecord) -> Result<String> {
    kheish_codec::digest_serialize(&tool_result_envelope(record))
}

fn enforce_output_limit(
    record: &mut ToolResultRecord,
    descriptor: &ToolDescriptor,
    limits: &ToolRuntimeLimits,
) -> Result<()> {
    if is_typed_quota_error(record) {
        return Ok(());
    }
    let output_bytes = serialized_json_len(&record.output)?;
    if output_bytes <= limits.max_output_bytes {
        return Ok(());
    }
    record.is_error = true;
    record.context_updates.clear();
    record.hook_contexts.clear();
    record.output = json!({
        "error": format!(
            "tool output exceeded runtime limit of {} bytes",
            limits.max_output_bytes
        ),
        "tool": descriptor.name.clone(),
        "quota": "output_bytes",
        "limit_bytes": limits.max_output_bytes,
        "actual_bytes": output_bytes,
    });
    Ok(())
}

fn is_typed_quota_error(record: &ToolResultRecord) -> bool {
    record.is_error && record.output.get("quota").and_then(Value::as_str).is_some()
}

fn enforce_result_envelope_limit(
    record: &mut ToolResultRecord,
    descriptor: &ToolDescriptor,
    limits: &ToolRuntimeLimits,
) -> Result<()> {
    let envelope_bytes = serialized_tool_result_envelope_len(record)?;
    if envelope_bytes <= limits.max_result_envelope_bytes.max(1) {
        return Ok(());
    }
    record.is_error = true;
    record.context_updates.clear();
    record.hook_contexts.clear();
    record.output = json!({
        "error": format!(
            "tool result envelope exceeded runtime limit of {} bytes",
            limits.max_result_envelope_bytes.max(1)
        ),
        "tool": descriptor.name.clone(),
        "quota": "result_envelope_bytes",
        "limit_bytes": limits.max_result_envelope_bytes.max(1),
        "actual_bytes": envelope_bytes,
    });
    Ok(())
}

fn enforce_cumulative_result_limits(
    record: &mut ToolResultRecord,
    descriptor: &ToolDescriptor,
    limits: &ToolRuntimeLimits,
    turn_budget: &ToolTurnBudget,
) -> Result<()> {
    let output_bytes = serialized_json_len(&record.output)?;
    let envelope_bytes = serialized_tool_result_envelope_len(record)?;
    match turn_budget.try_reserve_result(output_bytes, envelope_bytes) {
        Ok(()) => Ok(()),
        Err(ToolTurnBudgetQuota::CumulativeOutputBytes) => {
            record.is_error = true;
            record.context_updates.clear();
            record.hook_contexts.clear();
            record.output = json!({
                "error": format!(
                    "tool outputs exceeded runtime turn limit of {} bytes",
                    limits.max_cumulative_output_bytes.max(1)
                ),
                "tool": descriptor.name.clone(),
                "quota": "cumulative_output_bytes",
                "limit_bytes": limits.max_cumulative_output_bytes.max(1),
                "actual_bytes": output_bytes,
            });
            Ok(())
        }
        Err(ToolTurnBudgetQuota::CumulativeResultEnvelopeBytes) => {
            record.is_error = true;
            record.context_updates.clear();
            record.hook_contexts.clear();
            record.output = json!({
                "error": format!(
                    "tool result envelopes exceeded runtime turn limit of {} bytes",
                    limits.max_cumulative_result_envelope_bytes.max(1)
                ),
                "tool": descriptor.name.clone(),
                "quota": "cumulative_result_envelope_bytes",
                "limit_bytes": limits.max_cumulative_result_envelope_bytes.max(1),
                "actual_bytes": envelope_bytes,
            });
            Ok(())
        }
    }
}

fn external_tool_audit_target(
    descriptor: &ToolDescriptor,
    call: &ToolCallRecord,
) -> Option<String> {
    match descriptor.name.as_str() {
        "bash" => Some(format!(
            "bash:command_sha256:{}",
            call.input
                .get("command")
                .and_then(Value::as_str)
                .map(short_digest)
                .unwrap_or_else(|| "<unknown>".to_string())
        )),
        "web_search" => Some(format!(
            "web_search:query_sha256:{}",
            call.input
                .get("query")
                .and_then(Value::as_str)
                .map(short_digest)
                .unwrap_or_else(|| "<unknown>".to_string())
        )),
        "web_fetch" => Some(format!(
            "web_fetch:{}",
            call.input
                .get("url")
                .and_then(Value::as_str)
                .map(safe_url_audit_target)
                .unwrap_or_else(|| "<unknown>".to_string())
        )),
        name if name.starts_with("mcp__") => Some(format!("mcp_tool:{name}")),
        name if name.starts_with("mcp.") => Some(format!("mcp_tool:{name}")),
        name => Some(format!("tool:{name}")),
    }
}

fn short_digest(value: &str) -> String {
    let digest = kheish_codec::digest_text(value);
    digest.get(..16).unwrap_or(digest.as_str()).to_string()
}

/// Normalizes integer-like JSON numbers in tool-call arguments before schema
/// validation and typed deserialization.
///
/// Some providers emit `1.0` for arguments that are logically integers. Kheish
/// keeps tool schemas strongly typed, so this function rewrites only lossless
/// integer-like floats while leaving fractional values unchanged.
pub fn normalize_tool_input_numbers(input: Value) -> Value {
    match input {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(normalize_tool_input_numbers)
                .collect::<Vec<_>>(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, normalize_tool_input_numbers(value)))
                .collect(),
        ),
        Value::Number(number) => {
            normalize_tool_number(&number).map_or(Value::Number(number), Value::Number)
        }
        other => other,
    }
}

fn normalize_tool_number(number: &serde_json::Number) -> Option<serde_json::Number> {
    let value = number.as_f64()?;
    if !value.is_finite() || value.fract() != 0.0 {
        return None;
    }
    if value >= 0.0 && value <= u64::MAX as f64 {
        return Some(serde_json::Number::from(value as u64));
    }
    if value >= i64::MIN as f64 && value <= i64::MAX as f64 {
        return Some(serde_json::Number::from(value as i64));
    }
    None
}

async fn dispatch_tool_hook(
    dispatcher: Option<&Arc<dyn HookDispatcher>>,
    event: HookEventName,
    descriptor: &ToolDescriptor,
    call: &ToolCallRecord,
    result: Option<&ToolResultRecord>,
) -> Result<HookDispatchOutcome> {
    let Some(dispatcher) = dispatcher else {
        return Ok(HookDispatchOutcome::default());
    };
    dispatcher
        .dispatch(HookInvocation {
            event,
            subject: Some(descriptor.name.clone()),
            session_id: current_execution_scope()
                .as_ref()
                .map(|scope| scope.session_id.clone()),
            agent_id: current_execution_scope()
                .as_ref()
                .and_then(|scope| scope.agent_id.clone()),
            run_id: current_execution_scope().and_then(|scope| scope.run_id),
            payload: json!({
                "descriptor": descriptor,
                "tool_call": call,
                "tool_result": result,
            }),
        })
        .await
}

fn build_tool_context_metadata(
    scope: Option<crate::ExecutionScope>,
    call: &ToolCallRecord,
) -> Value {
    let mut metadata = scope
        .as_ref()
        .and_then(|scope| serde_json::to_value(scope).ok())
        .unwrap_or_else(|| json!({}));
    let Some(object) = metadata.as_object_mut() else {
        return metadata;
    };
    if let Some(scope) = scope {
        object.insert(
            "visible_skills".to_string(),
            Value::Array(
                scope
                    .visible_skills
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
        );
        object.insert(
            "visible_mcp_servers".to_string(),
            Value::Array(
                scope
                    .visible_mcp_servers
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
        );
        object.insert(
            "visible_mcp_tools".to_string(),
            Value::Array(
                scope
                    .visible_mcp_tools
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
        );
    }
    object.insert("tool_call_id".to_string(), Value::String(call.id.clone()));
    if let Some(assistant_message_id) = &call.assistant_message_id {
        object.insert(
            "assistant_message_id".to_string(),
            Value::String(assistant_message_id.clone()),
        );
    }
    metadata
}

impl ToolDescriptor {
    /// Converts the runtime descriptor into a provider-facing tool definition.
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.schema.json_schema(),
            allows_parallel: self.allows_parallel,
        }
    }
}

impl ToolSchema {
    fn json_schema(&self) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for field in &self.fields {
            let mut field_schema = field_json_schema(field);
            if let Some(description) = &field.description {
                field_schema.insert(
                    "description".to_string(),
                    Value::String(description.clone()),
                );
            }
            properties.insert(field.name.clone(), Value::Object(field_schema));
            if field.required {
                required.push(Value::String(field.name.clone()));
            }
        }

        let mut schema = serde_json::Map::new();
        schema.insert("type".to_string(), Value::String("object".to_string()));
        schema.insert("properties".to_string(), Value::Object(properties));
        schema.insert("additionalProperties".to_string(), Value::Bool(false));
        if !required.is_empty() {
            schema.insert("required".to_string(), Value::Array(required));
        }
        Value::Object(schema)
    }
}

/// The schema for "any JSON value" spelled out as an explicit type union.
/// Provider validators (OpenAI at least) reject a bare `{}` property schema,
/// so untyped fields must still carry a `type` key.
fn any_json_schema() -> Value {
    json!({ "type": ["object", "array", "string", "number", "boolean", "null"] })
}

fn field_json_schema(field: &ToolSchemaField) -> serde_json::Map<String, Value> {
    if let Some(schema) = &field.structured_schema {
        let Value::Object(object) = structured_tool_schema_json(schema) else {
            return serde_json::Map::new();
        };
        return object;
    }

    let mut field_schema = serde_json::Map::new();
    if let Some(type_name) = field.kind.json_schema_type() {
        field_schema.insert("type".to_string(), Value::String(type_name.to_string()));
    }
    match field.kind {
        ToolInputKind::Array => {
            // Arrays must always carry `items`: providers such as OpenAI
            // reject function parameters with a bare `{"type":"array"}`.
            // An unknown or `Any` item kind serializes as the explicit
            // any-value union.
            let items = field
                .item_kind
                .as_ref()
                .and_then(ToolInputKind::json_schema_type)
                .map(|item_type| json!({ "type": item_type }))
                .unwrap_or_else(any_json_schema);
            field_schema.insert("items".to_string(), items);
        }
        ToolInputKind::Object => {
            field_schema.insert("additionalProperties".to_string(), Value::Bool(true));
        }
        ToolInputKind::Any => {
            let Value::Object(any_schema) = any_json_schema() else {
                unreachable!("any_json_schema is an object");
            };
            field_schema.extend(any_schema);
        }
        ToolInputKind::String | ToolInputKind::Number | ToolInputKind::Boolean => {}
    }
    field_schema
}

fn structured_tool_schema_json(schema: &StructuredFieldSchema) -> Value {
    match schema.kind {
        StructuredValueKind::Any => any_json_schema(),
        StructuredValueKind::String => json!({"type": "string"}),
        StructuredValueKind::Number => json!({"type": "number"}),
        StructuredValueKind::Boolean => json!({"type": "boolean"}),
        StructuredValueKind::Object => {
            let mut properties = serde_json::Map::new();
            let mut required = Vec::new();
            for (name, field_schema) in &schema.fields {
                properties.insert(name.clone(), structured_tool_schema_json(field_schema));
                required.push(Value::String(name.clone()));
            }
            for (name, field_schema) in &schema.optional_fields {
                properties.insert(name.clone(), structured_tool_schema_json(field_schema));
            }
            json!({
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            })
        }
        StructuredValueKind::Array => json!({
            "type": "array",
            "items": schema
                .items
                .as_ref()
                .map(|items| structured_tool_schema_json(items))
                .unwrap_or_else(|| json!({})),
        }),
    }
}

fn validate_tool_input(input: &Value, schema: &ToolSchema) -> Result<()> {
    let object = input
        .as_object()
        .ok_or_else(|| anyhow!("tool input must be a JSON object"))?;
    for field in &schema.fields {
        let Some(value) = object.get(&field.name) else {
            if field.required {
                bail!("missing required tool input field {}", field.name);
            }
            continue;
        };
        if !field.required && value.is_null() {
            continue;
        }
        if !matches_field_schema(value, field) {
            bail!("tool input field {} has invalid type", field.name);
        }
    }
    for key in object.keys() {
        if !schema.fields.iter().any(|field| field.name == *key) {
            bail!("unexpected tool input field {key}");
        }
    }
    Ok(())
}

fn matches_field_schema(value: &Value, field: &ToolSchemaField) -> bool {
    if let Some(schema) = &field.structured_schema {
        return validate_structured_tool_value(value, schema).is_ok();
    }
    if !matches_kind(value, &field.kind) {
        return false;
    }
    match (&field.kind, &field.item_kind) {
        (ToolInputKind::Array, Some(item_kind)) => value
            .as_array()
            .map(|items| items.iter().all(|item| matches_kind(item, item_kind)))
            .unwrap_or(false),
        _ => true,
    }
}

fn validate_structured_tool_value(value: &Value, schema: &StructuredFieldSchema) -> Result<()> {
    match schema.kind {
        StructuredValueKind::Any => Ok(()),
        StructuredValueKind::String if value.is_string() => Ok(()),
        StructuredValueKind::Number if value.is_number() => Ok(()),
        StructuredValueKind::Boolean if value.is_boolean() => Ok(()),
        StructuredValueKind::Object => {
            let object = value
                .as_object()
                .ok_or_else(|| anyhow!("tool input value must be an object"))?;
            for (name, field_schema) in &schema.fields {
                let field_value = object
                    .get(name)
                    .ok_or_else(|| anyhow!("missing required tool input field {name}"))?;
                validate_structured_tool_value(field_value, field_schema)?;
            }
            for (name, field_schema) in &schema.optional_fields {
                if let Some(field_value) = object.get(name) {
                    if field_value.is_null() {
                        continue;
                    }
                    validate_structured_tool_value(field_value, field_schema)?;
                }
            }
            for key in object.keys() {
                if !schema.fields.contains_key(key) && !schema.optional_fields.contains_key(key) {
                    bail!("unexpected tool input field {key}");
                }
            }
            Ok(())
        }
        StructuredValueKind::Array => {
            let items = value
                .as_array()
                .ok_or_else(|| anyhow!("tool input value must be an array"))?;
            if let Some(item_schema) = &schema.items {
                for item in items {
                    validate_structured_tool_value(item, item_schema)?;
                }
            }
            Ok(())
        }
        _ => bail!("tool input value type mismatch"),
    }
}

fn matches_kind(value: &Value, kind: &ToolInputKind) -> bool {
    match kind {
        ToolInputKind::Any => true,
        ToolInputKind::String => value.is_string(),
        ToolInputKind::Number => value.is_number(),
        ToolInputKind::Boolean => value.is_boolean(),
        ToolInputKind::Object => value.is_object(),
        ToolInputKind::Array => value.is_array(),
    }
}

impl ToolInputKind {
    fn json_schema_type(&self) -> Option<&'static str> {
        match self {
            Self::Any => None,
            Self::String => Some("string"),
            Self::Number => Some("number"),
            Self::Boolean => Some("boolean"),
            Self::Object => Some("object"),
            Self::Array => Some("array"),
        }
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use kheish_core::{HookDispatcher, ToolCatalog, ToolExecutor};
    use serde_json::{Value, json};
    use tokio::time::sleep;

    use super::{
        SandboxProfile, Tool, ToolDescriptor, ToolExecutionOutput, ToolHook, ToolInputKind,
        ToolRuntime, ToolRuntimeLimits, ToolSchema, ToolSchemaField, any_json_schema,
    };
    use crate::{
        ExecutionScope, is_interrupted_error, observability::InMemoryObserver, scope_execution,
    };
    use kheish_types::{
        ContextUpdate, HookDecision, HookDispatchOutcome, HookEventName, HookInvocation,
        StructuredFieldSchema, StructuredValueKind, ToolSurfaceFilter,
    };

    struct EchoTool;

    struct NamedTool {
        name: &'static str,
    }

    struct FailingTool {
        secret: &'static str,
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "echo".to_string(),
                description: "Echoes back the provided text".to_string(),
                schema: ToolSchema {
                    fields: vec![ToolSchemaField {
                        name: "text".to_string(),
                        kind: ToolInputKind::String,
                        item_kind: None,
                        structured_schema: None,
                        required: true,
                        description: Some("Text to echo back".to_string()),
                    }],
                },
                timeout_ms: 100,
                sandbox: SandboxProfile::ReadOnly,
                allows_parallel: true,
            }
        }

        async fn execute(
            &self,
            _ctx: super::ToolContext,
            input: serde_json::Value,
        ) -> Result<ToolExecutionOutput> {
            Ok(ToolExecutionOutput::json(json!({"echo": input["text"]})))
        }
    }

    #[async_trait]
    impl Tool for NamedTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: self.name.to_string(),
                description: "Returns ok.".to_string(),
                schema: ToolSchema {
                    fields: vec![ToolSchemaField {
                        name: "text".to_string(),
                        kind: ToolInputKind::String,
                        item_kind: None,
                        structured_schema: None,
                        required: false,
                        description: None,
                    }],
                },
                timeout_ms: 100,
                sandbox: SandboxProfile::NetworkEnabled,
                allows_parallel: true,
            }
        }

        async fn execute(
            &self,
            _ctx: super::ToolContext,
            _input: serde_json::Value,
        ) -> Result<ToolExecutionOutput> {
            Ok(ToolExecutionOutput::json(json!({"ok": true})))
        }
    }

    #[async_trait]
    impl Tool for FailingTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "failing".to_string(),
                description: "Fails for redaction tests.".to_string(),
                schema: ToolSchema::default(),
                timeout_ms: 100,
                sandbox: SandboxProfile::ReadOnly,
                allows_parallel: true,
            }
        }

        async fn execute(
            &self,
            _ctx: super::ToolContext,
            _input: serde_json::Value,
        ) -> Result<ToolExecutionOutput> {
            Err(anyhow!("tool failed with {}", self.secret))
        }
    }

    struct AuditHook(Mutex<Vec<String>>);

    struct EventHookDispatcher {
        outcomes: BTreeMap<HookEventName, HookDispatchOutcome>,
    }

    #[async_trait]
    impl HookDispatcher for EventHookDispatcher {
        async fn dispatch(&self, invocation: HookInvocation) -> Result<HookDispatchOutcome> {
            Ok(self
                .outcomes
                .get(&invocation.event)
                .cloned()
                .unwrap_or_default())
        }
    }

    #[async_trait]
    impl ToolHook for AuditHook {
        async fn before(
            &self,
            descriptor: &ToolDescriptor,
            _call: &kheish_types::ToolCallRecord,
        ) -> Result<()> {
            self.0.lock().push(format!("before:{}", descriptor.name));
            Ok(())
        }

        async fn after(
            &self,
            descriptor: &ToolDescriptor,
            _call: &kheish_types::ToolCallRecord,
            _result: &kheish_types::ToolResultRecord,
        ) -> Result<()> {
            self.0.lock().push(format!("after:{}", descriptor.name));
            Ok(())
        }
    }

    #[tokio::test]
    async fn tool_runtime_validates_and_executes_calls() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::new(observer);
        runtime.register(EchoTool);
        runtime.add_hook(AuditHook(Mutex::new(Vec::new())));

        let result = runtime
            .execute(&kheish_types::ToolCallRecord {
                id: "call-1".to_string(),
                name: "echo".to_string(),
                input: json!({"text": "hello"}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;

        assert!(!result.is_error);
        assert_eq!(result.output, json!({"echo": "hello"}));
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_redacts_tool_error_messages() -> Result<()> {
        let secret = "tool-error-secret-canary";
        kheish_auth::register_ephemeral_debug_redaction_token(secret);
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::new(observer);
        runtime.register(FailingTool { secret });

        let result = runtime
            .execute(&kheish_types::ToolCallRecord {
                id: "call-1".to_string(),
                name: "failing".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;

        assert!(result.is_error);
        let rendered = result.output.to_string();
        assert!(!rendered.contains(secret), "tool result leaked: {rendered}");
        assert!(rendered.contains("<redacted>"));
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_records_external_action_traces_for_boundary_tools() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::new(observer.clone());
        runtime.register(EchoTool);
        runtime.register(NamedTool { name: "bash" });
        runtime.register(NamedTool { name: "web_fetch" });
        runtime.register(NamedTool {
            name: "mcp__github__get_issue",
        });

        let scope = ExecutionScope {
            session_id: "session-1".to_string(),
            agent_id: Some("agent-1".to_string()),
            run_id: Some("run-1".to_string()),
            principal_id: Some("agent:agent-1".to_string()),
            parent_principal_id: None,
            delegation_id: None,
            grant_id: None,
            tool_call_id: None,
            provider: None,
            model: None,
            credential_scope: Default::default(),
            workspace_root: None,
            visible_skills: Vec::new(),
            visible_mcp_servers: Vec::new(),
            visible_mcp_tools: Vec::new(),
        };
        scope_execution(scope, Default::default(), async {
            for (id, name, input) in [
                ("call-echo", "echo", json!({"text": "hello"})),
                ("call-bash", "bash", json!({"text": "echo hi"})),
                (
                    "call-fetch",
                    "web_fetch",
                    json!({"text": "https://example.com"}),
                ),
                (
                    "call-mcp",
                    "mcp__github__get_issue",
                    json!({"text": "owner/repo#1"}),
                ),
            ] {
                let result = runtime
                    .execute(&kheish_types::ToolCallRecord {
                        id: id.to_string(),
                        name: name.to_string(),
                        input,
                        assistant_message_id: None,
                        assistant_provider_response_id: None,
                    })
                    .await?;
                assert!(!result.is_error);
            }
            Ok::<(), anyhow::Error>(())
        })
        .await?;

        let external = observer
            .traces()
            .into_iter()
            .filter_map(|trace| match trace.kind {
                crate::TraceEventKind::ExternalAction {
                    phase,
                    kind,
                    target,
                    ..
                } => Some((phase, kind, target, trace.tool_call_id)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(external.len(), 8);
        assert!(external.iter().any(|(_, _, target, tool_call_id)| {
            target == "tool:echo" && tool_call_id.as_deref() == Some("call-echo")
        }));
        assert!(external.iter().any(|(_, _, target, tool_call_id)| {
            target.starts_with("bash:") && tool_call_id.as_deref() == Some("call-bash")
        }));
        assert!(external.iter().any(|(_, _, target, tool_call_id)| {
            target.starts_with("web_fetch:") && tool_call_id.as_deref() == Some("call-fetch")
        }));
        assert!(external.iter().any(|(_, _, target, tool_call_id)| {
            target == "mcp_tool:mcp__github__get_issue"
                && tool_call_id.as_deref() == Some("call-mcp")
        }));
        assert!(external.iter().all(|(_, kind, _, _)| kind == "tool"),);
        Ok(())
    }

    #[tokio::test]
    async fn pre_tool_use_updated_input_is_blocked_after_permission_evaluation() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::new(observer);
        runtime.register(EchoTool);
        runtime.set_hook_dispatcher(Arc::new(EventHookDispatcher {
            outcomes: BTreeMap::from([(
                HookEventName::PreToolUse,
                HookDispatchOutcome {
                    updated_input: Some(json!({"text": "rewritten"})),
                    ..HookDispatchOutcome::default()
                },
            )]),
        }));

        let result = runtime
            .execute(&kheish_types::ToolCallRecord {
                id: "call-pre-hook-rewrite".to_string(),
                name: "echo".to_string(),
                input: json!({"text": "original"}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;
        assert!(result.is_error);
        assert!(
            result
                .output
                .to_string()
                .contains("cannot update tool input after permission evaluation")
        );
        Ok(())
    }

    #[tokio::test]
    async fn post_tool_use_updated_output_is_reflected_in_external_audit_digest() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::new(observer.clone());
        runtime.register(NamedTool { name: "bash" });
        let final_output = json!({"final": true});
        runtime.set_hook_dispatcher(Arc::new(EventHookDispatcher {
            outcomes: BTreeMap::from([(
                HookEventName::PostToolUse,
                HookDispatchOutcome {
                    updated_output: Some(final_output.clone()),
                    ..HookDispatchOutcome::default()
                },
            )]),
        }));

        let result = runtime
            .execute(&kheish_types::ToolCallRecord {
                id: "call-final-audit".to_string(),
                name: "bash".to_string(),
                input: json!({"text": "echo hi"}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;
        assert!(!result.is_error);
        assert_eq!(result.output, final_output);

        let expected_digest = super::tool_result_envelope_digest(&result)?;
        let response_digest = observer
            .traces()
            .into_iter()
            .find_map(|trace| match trace.kind {
                crate::TraceEventKind::ExternalAction {
                    phase,
                    kind,
                    response_digest,
                    ..
                } if phase == "response" && kind == "tool" => response_digest,
                _ => None,
            })
            .expect("tool response audit should be recorded");
        assert_eq!(response_digest, expected_digest);
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_preserves_empty_visibility_lists_in_tool_context_metadata() -> Result<()>
    {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::new(observer);
        runtime.register(MetadataEchoTool);

        let result = scope_execution(
            ExecutionScope {
                session_id: "scoped-session".to_string(),
                visible_skills: Vec::new(),
                visible_mcp_servers: Vec::new(),
                visible_mcp_tools: Vec::new(),
                ..ExecutionScope::default()
            },
            tokio_util::sync::CancellationToken::new(),
            async {
                runtime
                    .execute(&kheish_types::ToolCallRecord {
                        id: "call-empty-scope".to_string(),
                        name: "metadata_echo".to_string(),
                        input: json!({}),
                        assistant_message_id: None,
                        assistant_provider_response_id: None,
                    })
                    .await
            },
        )
        .await?;

        assert_eq!(
            result.output.get("visible_skills"),
            Some(&Value::Array(Vec::new()))
        );
        assert_eq!(
            result.output.get("visible_mcp_servers"),
            Some(&Value::Array(Vec::new()))
        );
        assert_eq!(
            result.output.get("visible_mcp_tools"),
            Some(&Value::Array(Vec::new()))
        );
        Ok(())
    }

    struct MetadataEchoTool;

    #[async_trait]
    impl Tool for MetadataEchoTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "metadata_echo".to_string(),
                description: "Returns the tool context metadata for tests.".to_string(),
                schema: ToolSchema::default(),
                timeout_ms: 100,
                sandbox: SandboxProfile::ReadOnly,
                allows_parallel: true,
            }
        }

        async fn execute(
            &self,
            ctx: super::ToolContext,
            _input: serde_json::Value,
        ) -> Result<ToolExecutionOutput> {
            Ok(ToolExecutionOutput::json(ctx.metadata))
        }
    }

    struct MockMcpTool;

    #[async_trait]
    impl Tool for MockMcpTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "mcp__example__query".to_string(),
                description: "Mock MCP tool for allowlist tests.".to_string(),
                schema: ToolSchema::default(),
                timeout_ms: 100,
                sandbox: SandboxProfile::NetworkEnabled,
                allows_parallel: true,
            }
        }

        async fn execute(
            &self,
            _ctx: super::ToolContext,
            _input: serde_json::Value,
        ) -> Result<ToolExecutionOutput> {
            Ok(ToolExecutionOutput::json(json!({"ok": true})))
        }
    }

    #[tokio::test]
    async fn scoped_tool_runtime_allowlist_applies_to_mcp_tools() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::new(observer.clone());
        runtime.register(MockMcpTool);
        let runtime = Arc::new(runtime);

        let scoped = runtime.scoped(ToolSurfaceFilter {
            allowlist: vec!["echo".to_string()],
            denylist: Vec::new(),
        });
        assert!(
            !scoped
                .definitions()
                .iter()
                .any(|definition| definition.name == "mcp__example__query"),
            "MCP tools should honor the same allowlist contract as other tools"
        );

        let result = scope_execution(ExecutionScope::default(), Default::default(), async {
            scoped
                .execute(&kheish_types::ToolCallRecord {
                    id: "call-hidden-mcp".to_string(),
                    name: "mcp__example__query".to_string(),
                    input: json!({}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                })
                .await
        })
        .await?;
        assert!(result.is_error);
        assert_eq!(
            result.output,
            json!({"error": "tool mcp__example__query is not available for this agent"})
        );
        let traces = observer.traces();
        assert!(traces.iter().any(|trace| matches!(
            &trace.kind,
            crate::TraceEventKind::ToolStarted { call_id, .. } if call_id == "call-hidden-mcp"
        )));
        assert!(traces.iter().any(|trace| matches!(
            &trace.kind,
            crate::TraceEventKind::ToolFinished { call_id, is_error: true, .. }
                if call_id == "call-hidden-mcp"
        )));
        let hidden_mcp_audit = traces
            .iter()
            .filter_map(|trace| match &trace.kind {
                crate::TraceEventKind::ExternalAction {
                    phase,
                    kind,
                    target,
                    outcome,
                    ..
                } if kind == "tool" && trace.tool_call_id.as_deref() == Some("call-hidden-mcp") => {
                    Some((phase.as_str(), target.as_str(), outcome.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            hidden_mcp_audit.iter().any(|(phase, target, _)| {
                *phase == "request" && *target == "mcp_tool:mcp__example__query"
            }),
            "hidden MCP request audit missing: {hidden_mcp_audit:#?}"
        );
        assert!(
            hidden_mcp_audit.iter().any(|(phase, target, outcome)| {
                *phase == "response"
                    && *target == "mcp_tool:mcp__example__query"
                    && outcome
                        .as_deref()
                        .is_some_and(|value| value.starts_with("failed:"))
            }),
            "hidden MCP response audit missing: {hidden_mcp_audit:#?}"
        );
        Ok(())
    }

    struct DelayedEchoTool;

    #[async_trait]
    impl Tool for DelayedEchoTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "delayed_echo".to_string(),
                description: "Echoes text and sometimes sleeps for test scheduling".to_string(),
                schema: ToolSchema {
                    fields: vec![ToolSchemaField {
                        name: "text".to_string(),
                        kind: ToolInputKind::String,
                        item_kind: None,
                        structured_schema: None,
                        required: true,
                        description: Some("Text to echo back".to_string()),
                    }],
                },
                timeout_ms: 100,
                sandbox: SandboxProfile::ReadOnly,
                allows_parallel: true,
            }
        }

        async fn execute(
            &self,
            _ctx: super::ToolContext,
            input: serde_json::Value,
        ) -> Result<ToolExecutionOutput> {
            let text = input["text"].as_str().unwrap_or_default();
            if text == "slow" {
                sleep(Duration::from_millis(20)).await;
            }
            Ok(ToolExecutionOutput::json(json!({"echo": text})))
        }
    }

    struct LongOutputTool;

    #[async_trait]
    impl Tool for LongOutputTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "long_output".to_string(),
                description: "Returns a large payload for runtime quota tests.".to_string(),
                schema: ToolSchema::default(),
                timeout_ms: 100,
                sandbox: SandboxProfile::ReadOnly,
                allows_parallel: true,
            }
        }

        async fn execute(
            &self,
            _ctx: super::ToolContext,
            _input: serde_json::Value,
        ) -> Result<ToolExecutionOutput> {
            Ok(ToolExecutionOutput::json(json!({
                "text": "x".repeat(128),
            })))
        }
    }

    struct SerialLongOutputTool;

    #[async_trait]
    impl Tool for SerialLongOutputTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "serial_long_output".to_string(),
                description: "Returns a large payload without parallel execution.".to_string(),
                schema: ToolSchema::default(),
                timeout_ms: 100,
                sandbox: SandboxProfile::ReadOnly,
                allows_parallel: false,
            }
        }

        async fn execute(
            &self,
            _ctx: super::ToolContext,
            _input: serde_json::Value,
        ) -> Result<ToolExecutionOutput> {
            Ok(ToolExecutionOutput::json(json!({
                "text": "x".repeat(128),
            })))
        }
    }

    struct ContextEnvelopeTool {
        context_bytes: usize,
    }

    #[async_trait]
    impl Tool for ContextEnvelopeTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "context_envelope".to_string(),
                description: "Returns a small output with large context updates.".to_string(),
                schema: ToolSchema::default(),
                timeout_ms: 100,
                sandbox: SandboxProfile::ReadOnly,
                allows_parallel: false,
            }
        }

        async fn execute(
            &self,
            _ctx: super::ToolContext,
            _input: serde_json::Value,
        ) -> Result<ToolExecutionOutput> {
            Ok(ToolExecutionOutput::with_updates(
                json!({"ok": true}),
                vec![ContextUpdate::WebResourceVisited {
                    uri: format!("https://example.test/{}", "x".repeat(self.context_bytes)),
                }],
            ))
        }
    }

    struct BoundaryLongOutputTool {
        name: &'static str,
    }

    #[async_trait]
    impl Tool for BoundaryLongOutputTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: self.name.to_string(),
                description: "Returns a large payload for boundary audit quota tests.".to_string(),
                schema: ToolSchema::default(),
                timeout_ms: 100,
                sandbox: SandboxProfile::ReadOnly,
                allows_parallel: true,
            }
        }

        async fn execute(
            &self,
            _ctx: super::ToolContext,
            _input: serde_json::Value,
        ) -> Result<ToolExecutionOutput> {
            Ok(ToolExecutionOutput::json(json!({
                "text": "x".repeat(128),
            })))
        }
    }

    struct ConcurrencyProbeTool {
        active: Arc<AtomicUsize>,
        max_seen: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for ConcurrencyProbeTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "concurrency_probe".to_string(),
                description: "Measures concurrent execution for runtime quota tests.".to_string(),
                schema: ToolSchema::default(),
                timeout_ms: 1_000,
                sandbox: SandboxProfile::ReadOnly,
                allows_parallel: true,
            }
        }

        async fn execute(
            &self,
            _ctx: super::ToolContext,
            _input: serde_json::Value,
        ) -> Result<ToolExecutionOutput> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_seen.fetch_max(active, Ordering::SeqCst);
            sleep(Duration::from_millis(20)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(ToolExecutionOutput::json(json!({"ok": true})))
        }
    }

    struct IntegerEchoTool;

    #[async_trait]
    impl Tool for IntegerEchoTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "integer_echo".to_string(),
                description: "Echoes an integer field for numeric normalization tests.".to_string(),
                schema: ToolSchema {
                    fields: vec![ToolSchemaField {
                        name: "count".to_string(),
                        kind: ToolInputKind::Number,
                        item_kind: None,
                        structured_schema: None,
                        required: true,
                        description: Some("Count to echo back".to_string()),
                    }],
                },
                timeout_ms: 100,
                sandbox: SandboxProfile::ReadOnly,
                allows_parallel: true,
            }
        }

        async fn execute(
            &self,
            _ctx: super::ToolContext,
            input: serde_json::Value,
        ) -> Result<ToolExecutionOutput> {
            let count = serde_json::from_value::<u32>(input["count"].clone())?;
            Ok(ToolExecutionOutput::json(json!({ "count": count })))
        }
    }

    fn nested_json(depth: usize) -> Value {
        let mut value = json!({"leaf": true});
        for _ in 0..depth {
            value = json!({"nested": value});
        }
        value
    }

    #[test]
    fn tool_runtime_limits_validate_positive_numeric_limits() {
        let limits = ToolRuntimeLimits::default();
        assert!(limits.validate().is_ok());
        for (field, set_zero) in [
            (
                "max_input_bytes",
                Box::new(|limits: &mut ToolRuntimeLimits| limits.max_input_bytes = 0)
                    as Box<dyn Fn(&mut ToolRuntimeLimits)>,
            ),
            (
                "max_output_bytes",
                Box::new(|limits: &mut ToolRuntimeLimits| limits.max_output_bytes = 0),
            ),
            (
                "max_result_envelope_bytes",
                Box::new(|limits: &mut ToolRuntimeLimits| limits.max_result_envelope_bytes = 0),
            ),
            (
                "max_timeout_ms",
                Box::new(|limits: &mut ToolRuntimeLimits| limits.max_timeout_ms = 0),
            ),
            (
                "max_parallel_tools",
                Box::new(|limits: &mut ToolRuntimeLimits| limits.max_parallel_tools = 0),
            ),
            (
                "max_calls_per_turn",
                Box::new(|limits: &mut ToolRuntimeLimits| limits.max_calls_per_turn = 0),
            ),
            (
                "max_cumulative_output_bytes",
                Box::new(|limits: &mut ToolRuntimeLimits| limits.max_cumulative_output_bytes = 0),
            ),
            (
                "max_cumulative_result_envelope_bytes",
                Box::new(|limits: &mut ToolRuntimeLimits| {
                    limits.max_cumulative_result_envelope_bytes = 0
                }),
            ),
        ] {
            let mut limits = ToolRuntimeLimits::default();
            set_zero(&mut limits);
            let error = limits.validate().expect_err("zero limit should fail");
            assert!(
                error.to_string().contains(field),
                "unexpected validation error for {field}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn tool_runtime_set_limits_affects_future_calls_and_hookless_clones() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::new(observer);
        runtime.register(EchoTool);
        let runtime = Arc::new(runtime);
        let hookless = runtime.clone_without_hook_dispatcher();

        let mut limits = runtime.limits();
        limits.max_input_bytes = 12;
        runtime.set_limits(limits.clone())?;
        assert_eq!(hookless.limits().max_input_bytes, 12);

        let result = hookless
            .execute(&kheish_types::ToolCallRecord {
                id: "call-shared-limits".to_string(),
                name: "echo".to_string(),
                input: json!({"text": "this is too large"}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;

        assert!(result.is_error);
        assert_eq!(result.output["quota"], "input_bytes");
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_preserves_batch_order_when_parallel() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::new(observer);
        runtime.register(DelayedEchoTool);

        let results = runtime
            .execute_batch(&[
                kheish_types::ToolCallRecord {
                    id: "call-1".to_string(),
                    name: "delayed_echo".to_string(),
                    input: json!({"text": "slow"}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                },
                kheish_types::ToolCallRecord {
                    id: "call-2".to_string(),
                    name: "delayed_echo".to_string(),
                    input: json!({"text": "fast"}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                },
            ])
            .await?;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].call_id, "call-1");
        assert_eq!(results[1].call_id, "call-2");
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_enforces_runtime_input_and_output_size_limits() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::with_limits(
            observer,
            ToolRuntimeLimits {
                max_input_bytes: 24,
                max_output_bytes: 32,
                ..ToolRuntimeLimits::default()
            },
        );
        runtime.register(EchoTool);
        runtime.register(LongOutputTool);

        let oversized_input = runtime
            .execute(&kheish_types::ToolCallRecord {
                id: "call-input-quota".to_string(),
                name: "echo".to_string(),
                input: json!({"text": "this input is too large"}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;
        assert!(oversized_input.is_error);
        assert_eq!(oversized_input.output["quota"], "input_bytes");

        let oversized_output = runtime
            .execute(&kheish_types::ToolCallRecord {
                id: "call-output-quota".to_string(),
                name: "long_output".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;
        assert!(oversized_output.is_error);
        assert_eq!(oversized_output.output["quota"], "output_bytes");
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_enforces_calls_per_turn_in_sequential_batches() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::with_limits(
            observer.clone(),
            ToolRuntimeLimits {
                max_calls_per_turn: 2,
                ..ToolRuntimeLimits::default()
            },
        );
        runtime.register(EchoTool);
        runtime.register(SerialLongOutputTool);
        let calls = (0..4)
            .map(|index| kheish_types::ToolCallRecord {
                id: format!("call-turn-{index}"),
                name: "serial_long_output".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .collect::<Vec<_>>();

        let results = runtime.execute_batch(&calls).await?;

        assert_eq!(results.len(), 4);
        assert!(!results[0].is_error);
        assert!(!results[1].is_error);
        assert!(results[2].is_error);
        assert_eq!(results[2].output["quota"], "max_calls_per_turn");
        assert!(results[3].is_error);
        assert_eq!(results[3].output["quota"], "max_calls_per_turn");
        let traces = observer.traces();
        for call_id in ["call-turn-2", "call-turn-3"] {
            assert!(traces.iter().any(|trace| matches!(
                &trace.kind,
                crate::TraceEventKind::ToolStarted { call_id: started, .. }
                    if started == call_id
            )));
            assert!(traces.iter().any(|trace| matches!(
                &trace.kind,
                crate::TraceEventKind::ToolFinished { call_id: finished, is_error: true, .. }
                    if finished == call_id
            )));
        }
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_enforces_calls_per_turn_across_parallel_chunks() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut runtime = ToolRuntime::with_limits(
            observer,
            ToolRuntimeLimits {
                max_parallel_tools: 2,
                max_calls_per_turn: 3,
                ..ToolRuntimeLimits::default()
            },
        );
        runtime.register(ConcurrencyProbeTool {
            active: active.clone(),
            max_seen: max_seen.clone(),
        });
        let calls = (0..5)
            .map(|index| kheish_types::ToolCallRecord {
                id: format!("call-turn-parallel-{index}"),
                name: "concurrency_probe".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .collect::<Vec<_>>();

        let results = runtime.execute_batch(&calls).await?;

        assert_eq!(results.len(), 5);
        assert!(results.iter().take(3).all(|result| !result.is_error));
        assert!(
            results.iter().skip(3).all(|result| {
                result.is_error && result.output["quota"] == "max_calls_per_turn"
            })
        );
        assert!(
            max_seen.load(Ordering::SeqCst) <= 2,
            "parallel runtime exceeded configured concurrency limit"
        );
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_enforces_cumulative_output_bytes_per_turn() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::with_limits(
            observer.clone(),
            ToolRuntimeLimits {
                max_output_bytes: 512,
                max_cumulative_output_bytes: 180,
                ..ToolRuntimeLimits::default()
            },
        );
        runtime.register(LongOutputTool);
        let calls = (0..2)
            .map(|index| kheish_types::ToolCallRecord {
                id: format!("call-output-turn-{index}"),
                name: "long_output".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .collect::<Vec<_>>();

        let results = runtime.execute_batch(&calls).await?;

        assert_eq!(results.len(), 2);
        assert!(!results[0].is_error);
        assert!(results[1].is_error);
        assert_eq!(results[1].output["quota"], "cumulative_output_bytes");
        assert!(results[1].context_updates.is_empty());
        assert!(results[1].hook_contexts.is_empty());
        let traces = observer.traces();
        assert!(traces.iter().any(|trace| matches!(
            &trace.kind,
            crate::TraceEventKind::ToolFinished { call_id, is_error: true, .. }
                if call_id == "call-output-turn-1"
        )));
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_enforces_result_envelope_bytes_for_context_updates() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::with_limits(
            observer.clone(),
            ToolRuntimeLimits {
                max_output_bytes: 512,
                max_result_envelope_bytes: 220,
                ..ToolRuntimeLimits::default()
            },
        );
        runtime.register(ContextEnvelopeTool { context_bytes: 512 });

        let result = runtime
            .execute(&kheish_types::ToolCallRecord {
                id: "call-envelope-context".to_string(),
                name: "context_envelope".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;

        assert!(result.is_error);
        assert_eq!(result.output["quota"], "result_envelope_bytes");
        assert!(result.context_updates.is_empty());
        assert!(result.hook_contexts.is_empty());
        let traces = observer.traces();
        assert!(traces.iter().any(|trace| matches!(
            &trace.kind,
            crate::TraceEventKind::ToolFinished { call_id, is_error: true, .. }
                if call_id == "call-envelope-context"
        )));
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_enforces_result_envelope_bytes_for_hook_contexts() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::with_limits(
            observer,
            ToolRuntimeLimits {
                max_output_bytes: 512,
                max_result_envelope_bytes: 220,
                ..ToolRuntimeLimits::default()
            },
        );
        runtime.register(EchoTool);
        runtime.set_hook_dispatcher(Arc::new(EventHookDispatcher {
            outcomes: BTreeMap::from([(
                HookEventName::PostToolUse,
                HookDispatchOutcome {
                    additional_contexts: vec!["x".repeat(512)],
                    ..HookDispatchOutcome::default()
                },
            )]),
        }));

        let result = runtime
            .execute(&kheish_types::ToolCallRecord {
                id: "call-envelope-hook".to_string(),
                name: "echo".to_string(),
                input: json!({"text": "ok"}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;

        assert!(result.is_error);
        assert_eq!(result.output["quota"], "result_envelope_bytes");
        assert!(result.hook_contexts.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_enforces_result_envelope_bytes_for_pre_hook_blocks() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::with_limits(
            observer,
            ToolRuntimeLimits {
                max_output_bytes: 512,
                max_result_envelope_bytes: 220,
                ..ToolRuntimeLimits::default()
            },
        );
        runtime.register(EchoTool);
        runtime.set_hook_dispatcher(Arc::new(EventHookDispatcher {
            outcomes: BTreeMap::from([(
                HookEventName::PreToolUse,
                HookDispatchOutcome {
                    decision: Some(HookDecision::Block),
                    stop_reason: Some("blocked for test".to_string()),
                    additional_contexts: vec!["x".repeat(512)],
                    ..HookDispatchOutcome::default()
                },
            )]),
        }));

        let result = runtime
            .execute(&kheish_types::ToolCallRecord {
                id: "call-envelope-pre-hook".to_string(),
                name: "echo".to_string(),
                input: json!({"text": "ok"}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;

        assert!(result.is_error);
        assert_eq!(result.output["quota"], "result_envelope_bytes");
        assert!(result.hook_contexts.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_enforces_cumulative_result_envelope_bytes_per_turn() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::with_limits(
            observer.clone(),
            ToolRuntimeLimits {
                max_output_bytes: 512,
                max_result_envelope_bytes: 2_048,
                max_cumulative_output_bytes: 1_024,
                max_cumulative_result_envelope_bytes: 470,
                ..ToolRuntimeLimits::default()
            },
        );
        runtime.register(ContextEnvelopeTool { context_bytes: 96 });
        let calls = (0..2)
            .map(|index| kheish_types::ToolCallRecord {
                id: format!("call-envelope-turn-{index}"),
                name: "context_envelope".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .collect::<Vec<_>>();

        let results = runtime.execute_batch(&calls).await?;

        assert_eq!(results.len(), 2);
        assert!(
            !results[0].is_error,
            "first result should fit: {results:#?}"
        );
        assert!(results[1].is_error);
        assert_eq!(
            results[1].output["quota"],
            "cumulative_result_envelope_bytes"
        );
        assert!(results[1].context_updates.is_empty());
        assert!(results[1].hook_contexts.is_empty());
        let traces = observer.traces();
        assert!(traces.iter().any(|trace| matches!(
            &trace.kind,
            crate::TraceEventKind::ToolFinished { call_id, is_error: true, .. }
                if call_id == "call-envelope-turn-1"
        )));
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_audits_final_quota_envelope_for_boundary_tools() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::with_limits(
            observer.clone(),
            ToolRuntimeLimits {
                max_output_bytes: 32,
                max_result_envelope_bytes: 2_048,
                ..ToolRuntimeLimits::default()
            },
        );
        runtime.register(BoundaryLongOutputTool { name: "bash" });

        let result = runtime
            .execute(&kheish_types::ToolCallRecord {
                id: "call-bash-output-quota".to_string(),
                name: "bash".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;

        assert!(result.is_error);
        assert_eq!(result.output["quota"], "output_bytes");
        let expected_digest = super::tool_result_envelope_digest(&result)?;
        let response_digest = observer
            .traces()
            .into_iter()
            .find_map(|trace| match trace.kind {
                crate::TraceEventKind::ExternalAction {
                    phase,
                    kind,
                    response_digest,
                    ..
                } if phase == "response"
                    && kind == "tool"
                    && trace.tool_call_id.as_deref() == Some("call-bash-output-quota") =>
                {
                    response_digest
                }
                _ => None,
            })
            .expect("boundary tool response audit should be recorded");
        assert_eq!(response_digest, expected_digest);
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_audits_and_finishes_runtime_rejections() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::with_limits(
            observer.clone(),
            ToolRuntimeLimits {
                max_input_bytes: 24,
                max_sandbox: SandboxProfile::ReadOnly,
                ..ToolRuntimeLimits::default()
            },
        );
        runtime.register(EchoTool);
        runtime.register(NamedTool { name: "web_fetch" });

        scope_execution(ExecutionScope::default(), Default::default(), async {
            let oversized_input = runtime
                .execute(&kheish_types::ToolCallRecord {
                    id: "call-input-quota-audit".to_string(),
                    name: "echo".to_string(),
                    input: json!({"text": "this input is too large"}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                })
                .await?;
            assert!(oversized_input.is_error);

            let denied_sandbox = runtime
                .execute(&kheish_types::ToolCallRecord {
                    id: "call-sandbox-quota-audit".to_string(),
                    name: "web_fetch".to_string(),
                    input: json!({}),
                    assistant_message_id: None,
                    assistant_provider_response_id: None,
                })
                .await?;
            assert!(denied_sandbox.is_error);
            Ok::<(), anyhow::Error>(())
        })
        .await?;

        let traces = observer.traces();
        for call_id in ["call-input-quota-audit", "call-sandbox-quota-audit"] {
            assert!(
                traces.iter().any(|trace| matches!(
                    &trace.kind,
                    crate::TraceEventKind::ToolStarted { call_id: started, .. }
                        if started == call_id
                )),
                "missing ToolStarted for {call_id}: {traces:#?}"
            );
            assert!(
                traces.iter().any(|trace| matches!(
                    &trace.kind,
                    crate::TraceEventKind::ToolFinished { call_id: finished, is_error: true, .. }
                        if finished == call_id
                )),
                "missing error ToolFinished for {call_id}: {traces:#?}"
            );
            let phases = traces
                .iter()
                .filter_map(|trace| match &trace.kind {
                    crate::TraceEventKind::ExternalAction {
                        phase,
                        kind,
                        outcome,
                        ..
                    } if kind == "tool" && trace.tool_call_id.as_deref() == Some(call_id) => {
                        Some((phase.as_str(), outcome.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert!(
                phases.iter().any(|(phase, _)| *phase == "request"),
                "missing request audit for {call_id}: {phases:#?}"
            );
            let response = phases
                .iter()
                .find(|(phase, _)| *phase == "response")
                .unwrap_or_else(|| panic!("missing response audit for {call_id}: {phases:#?}"));
            assert!(
                response
                    .1
                    .as_deref()
                    .is_some_and(|outcome| outcome.starts_with("failed:")),
                "unexpected response outcome for {call_id}: {response:#?}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_enforces_timeout_sandbox_and_parallel_limits() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut runtime = ToolRuntime::with_limits(
            observer,
            ToolRuntimeLimits {
                max_timeout_ms: 5,
                max_parallel_tools: 2,
                max_sandbox: SandboxProfile::ReadOnly,
                ..ToolRuntimeLimits::default()
            },
        );
        runtime.register(DelayedEchoTool);
        runtime.register(NamedTool { name: "web_fetch" });
        runtime.register(ConcurrencyProbeTool {
            active: active.clone(),
            max_seen: max_seen.clone(),
        });

        let timeout = runtime
            .execute(&kheish_types::ToolCallRecord {
                id: "call-timeout-quota".to_string(),
                name: "delayed_echo".to_string(),
                input: json!({"text": "slow"}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;
        assert!(timeout.is_error);
        assert_eq!(timeout.output["error"], "tool execution timed out");

        let denied_sandbox = runtime
            .execute(&kheish_types::ToolCallRecord {
                id: "call-sandbox-quota".to_string(),
                name: "web_fetch".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;
        assert!(denied_sandbox.is_error);
        assert_eq!(denied_sandbox.output["quota"], "sandbox");

        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::with_limits(
            observer,
            ToolRuntimeLimits {
                max_parallel_tools: 2,
                ..ToolRuntimeLimits::default()
            },
        );
        runtime.register(ConcurrencyProbeTool {
            active: active.clone(),
            max_seen: max_seen.clone(),
        });
        let calls = (0..5)
            .map(|index| kheish_types::ToolCallRecord {
                id: format!("call-parallel-{index}"),
                name: "concurrency_probe".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .collect::<Vec<_>>();
        let results = runtime.execute_batch(&calls).await?;
        assert_eq!(results.len(), 5);
        assert!(results.iter().all(|result| !result.is_error));
        assert!(
            max_seen.load(Ordering::SeqCst) <= 2,
            "parallel runtime exceeded configured concurrency limit"
        );
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_cancels_parallel_batches_without_finishing_later_chunks() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut runtime = ToolRuntime::with_limits(
            observer,
            ToolRuntimeLimits {
                max_parallel_tools: 2,
                ..ToolRuntimeLimits::default()
            },
        );
        runtime.register(ConcurrencyProbeTool {
            active: active.clone(),
            max_seen: max_seen.clone(),
        });
        let calls = (0..6)
            .map(|index| kheish_types::ToolCallRecord {
                id: format!("call-cancel-{index}"),
                name: "concurrency_probe".to_string(),
                input: json!({}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .collect::<Vec<_>>();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let child_token = cancellation.clone();
        let handle = tokio::spawn(async move {
            scope_execution(ExecutionScope::default(), child_token, async {
                runtime.execute_batch(&calls).await
            })
            .await
        });
        sleep(Duration::from_millis(5)).await;
        cancellation.cancel();

        let error = handle
            .await
            .expect("runtime task should not panic")
            .expect_err("parallel batch should surface cancellation");
        assert!(is_interrupted_error(&error));
        assert!(
            max_seen.load(Ordering::SeqCst) <= 2,
            "cancelled parallel batch exceeded runtime concurrency limit"
        );
        Ok(())
    }

    #[tokio::test]
    async fn tool_runtime_audits_and_finishes_cancelled_execution() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::new(observer.clone());
        runtime.register(DelayedEchoTool);
        let cancellation = tokio_util::sync::CancellationToken::new();
        let child_token = cancellation.clone();
        let handle = tokio::spawn(async move {
            scope_execution(ExecutionScope::default(), child_token, async {
                runtime
                    .execute(&kheish_types::ToolCallRecord {
                        id: "call-cancel-audit".to_string(),
                        name: "delayed_echo".to_string(),
                        input: json!({"text": "slow"}),
                        assistant_message_id: None,
                        assistant_provider_response_id: None,
                    })
                    .await
            })
            .await
        });
        sleep(Duration::from_millis(5)).await;
        cancellation.cancel();

        let error = handle
            .await
            .expect("runtime task should not panic")
            .expect_err("cancelled tool should surface interruption");
        assert!(is_interrupted_error(&error));

        let traces = observer.traces();
        assert!(traces.iter().any(|trace| matches!(
            &trace.kind,
            crate::TraceEventKind::ToolStarted { call_id, .. }
                if call_id == "call-cancel-audit"
        )));
        assert!(traces.iter().any(|trace| matches!(
            &trace.kind,
            crate::TraceEventKind::ToolFinished { call_id, is_error: true, .. }
                if call_id == "call-cancel-audit"
        )));
        let phases = traces
            .iter()
            .filter_map(|trace| match &trace.kind {
                crate::TraceEventKind::ExternalAction {
                    phase,
                    kind,
                    outcome,
                    ..
                } if kind == "tool"
                    && trace.tool_call_id.as_deref() == Some("call-cancel-audit") =>
                {
                    Some((phase.as_str(), outcome.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            phases.iter().any(|(phase, _)| *phase == "request"),
            "missing request audit for cancelled tool: {phases:#?}"
        );
        let response = phases
            .iter()
            .find(|(phase, _)| *phase == "response")
            .unwrap_or_else(|| panic!("missing response audit for cancelled tool: {phases:#?}"));
        assert!(
            response
                .1
                .as_deref()
                .is_some_and(|outcome| outcome.starts_with("failed:")),
            "unexpected cancellation audit outcome: {response:#?}"
        );
        Ok(())
    }

    #[test]
    fn tool_schema_validation_fuzzes_random_json_without_panics() {
        let schema = ToolSchema {
            fields: vec![
                ToolSchemaField {
                    name: "text".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: true,
                    description: None,
                },
                ToolSchemaField {
                    name: "items".to_string(),
                    kind: ToolInputKind::Array,
                    item_kind: Some(ToolInputKind::Number),
                    structured_schema: None,
                    required: false,
                    description: None,
                },
                ToolSchemaField {
                    name: "config".to_string(),
                    kind: ToolInputKind::Object,
                    item_kind: None,
                    structured_schema: Some(StructuredFieldSchema {
                        kind: StructuredValueKind::Object,
                        fields: BTreeMap::from([(
                            "enabled".to_string(),
                            StructuredFieldSchema::new(StructuredValueKind::Boolean),
                        )]),
                        optional_fields: BTreeMap::from([(
                            "label".to_string(),
                            StructuredFieldSchema::new(StructuredValueKind::String),
                        )]),
                        items: None,
                    }),
                    required: false,
                    description: None,
                },
            ],
        };
        let corpus = vec![
            Value::Null,
            json!(true),
            json!(42),
            json!("not an object"),
            json!({}),
            json!({"text": "ok"}),
            json!({"text": 7}),
            json!({"text": "ok", "items": [1, 2, 3]}),
            json!({"text": "ok", "items": [1, "bad"]}),
            json!({"text": "ok", "config": {"enabled": true}}),
            json!({"text": "ok", "config": {"enabled": true, "extra": true}}),
            nested_json(32),
        ];

        for input in corpus {
            let _ = super::validate_tool_input(&input, &schema);
        }
    }

    #[tokio::test]
    async fn tool_runtime_normalizes_integer_like_float_arguments() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::new(observer);
        runtime.register(IntegerEchoTool);

        let result = runtime
            .execute(&kheish_types::ToolCallRecord {
                id: "call-1".to_string(),
                name: "integer_echo".to_string(),
                input: json!({"count": 1.0}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;

        assert!(!result.is_error);
        assert_eq!(result.output, json!({"count": 1}));
        Ok(())
    }

    #[test]
    fn normalize_tool_input_numbers_preserves_fractional_values() {
        let normalized = super::normalize_tool_input_numbers(json!({
            "count": 1.5,
            "nested": [2.0, 3.25]
        }));

        assert_eq!(normalized["count"], json!(1.5));
        assert_eq!(normalized["nested"][0], json!(2));
        assert_eq!(normalized["nested"][1], json!(3.25));
    }

    #[tokio::test]
    async fn scoped_tool_runtime_filters_definitions_and_execution() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let mut runtime = ToolRuntime::new(observer);
        runtime.register(EchoTool);
        runtime.register(DelayedEchoTool);
        let runtime = Arc::new(runtime);
        let scoped = runtime.scoped(ToolSurfaceFilter {
            allowlist: vec!["echo".to_string()],
            denylist: vec!["delayed_echo".to_string()],
        });

        let definitions = scoped.definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "echo");

        let denied = scoped
            .execute(&kheish_types::ToolCallRecord {
                id: "call-2".to_string(),
                name: "delayed_echo".to_string(),
                input: json!({"text": "blocked"}),
                assistant_message_id: None,
                assistant_provider_response_id: None,
            })
            .await?;
        assert!(denied.is_error);
        assert_eq!(denied.tool_name.as_deref(), Some("delayed_echo"));
        Ok(())
    }

    #[test]
    fn tool_definition_json_schema_disallows_unknown_properties() {
        let schema = ToolSchema {
            fields: vec![
                ToolSchemaField {
                    name: "text".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: true,
                    description: Some("Text to echo back".to_string()),
                },
                ToolSchemaField {
                    name: "workdir".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: Some("Optional working directory".to_string()),
                },
                ToolSchemaField {
                    name: "allowed_tools".to_string(),
                    kind: ToolInputKind::Array,
                    item_kind: Some(ToolInputKind::String),
                    structured_schema: None,
                    required: false,
                    description: Some("Optional tool allowlist".to_string()),
                },
            ],
        };

        let json = schema.json_schema();
        assert_eq!(json["type"], "object");
        assert_eq!(json["additionalProperties"], json!(false));
        assert_eq!(json["required"], json!(["text"]));
        assert_eq!(json["properties"]["text"]["type"], json!("string"));
        assert_eq!(json["properties"]["workdir"]["type"], json!("string"));
        assert_eq!(json["properties"]["allowed_tools"]["type"], json!("array"));
        assert_eq!(
            json["properties"]["allowed_tools"]["items"],
            json!({"type": "string"})
        );
    }

    #[test]
    fn array_fields_without_item_kind_still_emit_items() {
        let schema = ToolSchema {
            fields: vec![ToolSchemaField {
                name: "entries".to_string(),
                kind: ToolInputKind::Array,
                item_kind: None,
                structured_schema: None,
                required: true,
                description: None,
            }],
        };

        let json = schema.json_schema();
        assert_eq!(json["properties"]["entries"]["type"], json!("array"));
        assert_eq!(json["properties"]["entries"]["items"], any_json_schema());
    }

    #[test]
    fn any_fields_emit_an_explicit_type_union_instead_of_an_empty_schema() {
        let schema = ToolSchema {
            fields: vec![ToolSchemaField {
                name: "parent".to_string(),
                kind: ToolInputKind::Any,
                item_kind: None,
                structured_schema: None,
                required: true,
                description: None,
            }],
        };

        let json = schema.json_schema();
        assert_eq!(json["properties"]["parent"], any_json_schema());
    }

    #[test]
    fn structured_tool_fields_emit_nested_json_schema_and_validate_input() -> Result<()> {
        let mut option_fields = BTreeMap::new();
        option_fields.insert(
            "label".to_string(),
            StructuredFieldSchema::new(StructuredValueKind::String),
        );
        let mut option_optional_fields = BTreeMap::new();
        option_optional_fields.insert(
            "id".to_string(),
            StructuredFieldSchema::new(StructuredValueKind::String),
        );
        let option_schema = StructuredFieldSchema {
            kind: StructuredValueKind::Object,
            fields: option_fields,
            optional_fields: option_optional_fields,
            items: None,
        };

        let mut options_schema = StructuredFieldSchema::new(StructuredValueKind::Array);
        options_schema.items = Some(Box::new(option_schema));

        let mut question_fields = BTreeMap::new();
        question_fields.insert(
            "question".to_string(),
            StructuredFieldSchema::new(StructuredValueKind::String),
        );
        question_fields.insert("options".to_string(), options_schema);
        let mut question_optional_fields = BTreeMap::new();
        question_optional_fields.insert(
            "header".to_string(),
            StructuredFieldSchema::new(StructuredValueKind::String),
        );
        let question_schema = StructuredFieldSchema {
            kind: StructuredValueKind::Object,
            fields: question_fields,
            optional_fields: question_optional_fields,
            items: None,
        };

        let mut questions_schema = StructuredFieldSchema::new(StructuredValueKind::Array);
        questions_schema.items = Some(Box::new(question_schema));
        let schema = ToolSchema {
            fields: vec![ToolSchemaField {
                name: "questions".to_string(),
                kind: ToolInputKind::Array,
                item_kind: Some(ToolInputKind::Object),
                structured_schema: Some(questions_schema),
                required: true,
                description: None,
            }],
        };

        let json_schema = schema.json_schema();
        let questions = &json_schema["properties"]["questions"];
        assert_eq!(questions["type"], json!("array"));
        assert_eq!(questions["items"]["type"], json!("object"));
        assert_eq!(questions["items"]["additionalProperties"], json!(false));
        assert_eq!(
            questions["items"]["required"],
            json!(["options", "question"])
        );
        assert_eq!(
            questions["items"]["properties"]["options"]["items"]["properties"]["label"]["type"],
            json!("string")
        );
        assert_eq!(
            questions["items"]["properties"]["options"]["items"]["additionalProperties"],
            json!(false)
        );

        super::validate_tool_input(
            &json!({
                "questions": [{
                    "question": "Which focus should I use?",
                    "header": null,
                    "options": [{"label": "memory", "id": null}]
                }]
            }),
            &schema,
        )?;
        assert!(
            super::validate_tool_input(
                &json!({
                    "questions": [{
                        "question": "Which focus should I use?",
                        "options": [{"label": "memory", "extra": true}]
                    }]
                }),
                &schema,
            )
            .is_err()
        );
        assert!(
            super::validate_tool_input(
                &json!({
                    "questions": [{
                        "question": "Which focus should I use?",
                        "options": [{"label": 1}]
                    }]
                }),
                &schema,
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn optional_tool_fields_accept_null_values() -> Result<()> {
        let schema = ToolSchema {
            fields: vec![
                ToolSchemaField {
                    name: "command".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: true,
                    description: None,
                },
                ToolSchemaField {
                    name: "workdir".to_string(),
                    kind: ToolInputKind::String,
                    item_kind: None,
                    structured_schema: None,
                    required: false,
                    description: None,
                },
            ],
        };

        super::validate_tool_input(&json!({"command": "pwd", "workdir": null}), &schema)?;
        Ok(())
    }

    #[test]
    fn array_tool_fields_validate_item_types() {
        let schema = ToolSchema {
            fields: vec![ToolSchemaField {
                name: "allowed_tools".to_string(),
                kind: ToolInputKind::Array,
                item_kind: Some(ToolInputKind::String),
                structured_schema: None,
                required: true,
                description: None,
            }],
        };

        assert!(
            super::validate_tool_input(&json!({"allowed_tools": ["bash", "read_file"]}), &schema)
                .is_ok()
        );
        assert!(
            super::validate_tool_input(&json!({"allowed_tools": ["bash", 1]}), &schema).is_err()
        );
    }
}
