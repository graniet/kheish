use parking_lot::RwLock;
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use kheish_agent::{ChildRetentionPolicy, ManagedAgentSnapshot};
use kheish_runtime::{PermissionMode, PromptMergeMode, ToolExecutionOutput};
use kheish_skills::{SkillDefinition, SkillSummary};
use kheish_types::{
    AttachmentRef, CapabilityScope, CredentialScope, ModelGenerationConfig, SessionControlState,
    SessionGoal, SessionOperatorConfig, TaskRecord, TaskStatus, ToolSurfaceFilter,
    UserQuestionRequest,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::shell_tasks::{BackgroundShellTaskRequest, TaskOutputView};
use crate::{
    AgentSummaryView, PostMailboxResponse, ScheduleCreateRequest, ScheduleView,
    SubmitInputItemRequest,
};

/// One typed control request used to spawn a sub-agent from the tool layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpawnAgentToolRequest {
    /// Optional child session identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Optional child thread identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Optional child workspace root override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Optional team label for structured coordination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_name: Option<String>,
    /// Optional isolation strategy for the child workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<SpawnIsolation>,
    /// Human-readable subtask name.
    pub name: String,
    /// Human-readable subtask description.
    #[serde(default)]
    pub description: String,
    /// The legacy plain-text initial prompt used when `input_items` is empty.
    #[serde(default)]
    pub prompt: String,
    /// Ordered daemon-owned asset identifiers appended after `prompt`.
    ///
    /// Use this simpler field when the child should receive one text prompt plus existing
    /// daemon-owned assets. Use `input_items` only when the asset order must be interleaved
    /// with multiple text fragments or inline uploads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asset_ids: Vec<String>,
    /// The ordered multimodal initial input sequence for the child.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_items: Vec<SubmitInputItemRequest>,
    /// Optional built-in agent profile name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Optional prompt override for the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Optional prompt merge mode for the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_merge_mode: Option<PromptMergeMode>,
    /// Optional model override for the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Optional provider route override for the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Optional fallback model override for the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    /// Optional full generation override for the child. `model` and `fallback_model` remain shortcuts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<ModelGenerationConfig>,
    /// Optional permission mode override for the child session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Optional post-settlement retention strategy for the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<ChildRetentionPolicy>,
    /// Optional human-friendly nickname override for the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    /// Optional allow-list for the child tool surface.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
    /// Optional deny-list for the child tool surface.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_tools: Vec<String>,
    /// Optional child capability-scope restriction applied on top of the parent session scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_scope: Option<CapabilityScope>,
    /// Optional child credential-scope restriction applied on top of the parent session scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_scope: Option<CredentialScope>,
    /// When true, waits for the child to settle before returning.
    #[serde(default)]
    pub wait: bool,
    /// When false, the child run is launched synchronously and the tool waits for settlement.
    #[serde(default = "default_run_in_background")]
    pub run_in_background: bool,
    /// Optional wait timeout in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// The live parent assistant text propagated from the current turn.
    #[serde(skip)]
    pub parent_assistant_message: Option<String>,
    /// The inherited parent tool identifiers propagated from the current turn.
    #[serde(skip)]
    pub inherited_tool_call_ids: Vec<String>,
    /// The daemon run identifier propagated from the current turn.
    #[serde(skip)]
    pub spawned_by_run_id: Option<String>,
    /// The idempotency key propagated from the current tool call.
    #[serde(skip)]
    pub spawn_request_id: Option<String>,
}

/// The isolation strategy requested for a spawned child agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnIsolation {
    /// Reuse the parent workspace root directly.
    #[default]
    Shared,
    /// Run the child against a dedicated workspace root.
    Worktree,
}

fn default_run_in_background() -> bool {
    true
}

/// The mailbox message type used for daemon-routed parent clarification answers.
pub const PARENT_CLARIFICATION_ANSWER_MESSAGE_TYPE: &str = "parent_clarification_answer";
/// The mailbox subject used for daemon-routed parent clarification answers.
pub const PARENT_CLARIFICATION_ANSWER_SUBJECT: &str = "clarification_answer";

/// One durable response returned after spawning a sub-agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpawnAgentToolResponse {
    /// The spawned child agent identifier.
    pub agent_id: String,
    /// The child session identifier.
    pub session_id: String,
    /// The current child status.
    pub status: String,
    /// Whether the child keeps running after the tool returns.
    pub run_in_background: bool,
    /// The optional background run identifier created for the initial subtask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_run_id: Option<String>,
    /// The latest canonical output emitted by the launched child run when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_output: Option<String>,
    /// The child team label when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_name: Option<String>,
    /// The effective isolation strategy used for the child.
    #[serde(default)]
    pub isolation: SpawnIsolation,
    /// The allocated machine-friendly child name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The allocated hierarchy path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The human-friendly nickname, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    /// The effective retention policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<ChildRetentionPolicy>,
    /// The terminal snapshot when `wait` was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<ManagedAgentSnapshot>,
}

/// One durable response returned after leaving session plan mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitPlanModeOutcome {
    /// The persisted session control state after leaving plan mode.
    pub state: SessionControlState,
    /// The effective permission mode restored after leaving plan mode.
    pub restored_permission_mode: Option<PermissionMode>,
}

/// One image-generation request executed by the daemon on behalf of the model.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageToolRouteOverride {
    /// Optional configured image route identifier such as `openai`, `openrouter`, or `google-images`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Optional provider-specific image model override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// One image-generation request executed by the daemon on behalf of the model.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerateImageToolRequest {
    /// The text prompt used to generate the images.
    pub prompt: String,
    /// The number of images to generate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// Optional size override passed to the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// Optional route override used to target one specific image backend.
    #[serde(flatten)]
    pub route: ImageToolRouteOverride,
}

/// One image-edit request executed by the daemon on behalf of the model.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EditImageToolRequest {
    /// The text instruction that describes the requested image edits.
    pub prompt: String,
    /// The ordered daemon-owned image assets supplied to the provider.
    ///
    /// The first asset is treated as the primary image to edit. Additional assets are passed
    /// through in order as extra visual context when the provider supports multiple inputs.
    ///
    /// When omitted, the daemon infers the source image only when the current user turn includes
    /// exactly one eligible image attachment. Passing an explicit empty array does not trigger
    /// inference.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_asset_ids: Vec<String>,
    /// Tracks whether the caller omitted `image_asset_ids` entirely so the daemon can distinguish
    /// omission from an explicit empty array.
    #[serde(skip)]
    pub(crate) image_asset_ids_was_omitted: bool,
    /// The number of edited images to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// Optional size override passed to the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// Optional route override used to target one specific image backend.
    #[serde(flatten)]
    pub route: ImageToolRouteOverride,
}

#[derive(Debug, Deserialize)]
struct EditImageToolRequestWire {
    prompt: String,
    #[serde(default, deserialize_with = "deserialize_optional_vec_field")]
    image_asset_ids: OptionalVecField<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<String>,
    #[serde(flatten)]
    route: ImageToolRouteOverride,
}

#[derive(Debug, Default)]
enum OptionalVecField<T> {
    #[default]
    Missing,
    Null,
    Present(Vec<T>),
}

fn deserialize_optional_vec_field<'de, D, T>(
    deserializer: D,
) -> Result<OptionalVecField<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    match Option::<Option<Vec<T>>>::deserialize(deserializer)? {
        None => Ok(OptionalVecField::Null),
        Some(None) => Ok(OptionalVecField::Null),
        Some(Some(values)) => Ok(OptionalVecField::Present(values)),
    }
}

impl<'de> Deserialize<'de> for EditImageToolRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = EditImageToolRequestWire::deserialize(deserializer)?;
        let (image_asset_ids, image_asset_ids_was_omitted) = match wire.image_asset_ids {
            OptionalVecField::Missing => (Vec::new(), true),
            OptionalVecField::Null => (Vec::new(), false),
            OptionalVecField::Present(values) => (values, false),
        };
        Ok(Self {
            prompt: wire.prompt,
            image_asset_ids_was_omitted,
            image_asset_ids,
            count: wire.count,
            size: wire.size,
            route: wire.route,
        })
    }
}

/// One image-tool result returned to the model.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageToolResponse {
    /// The provider that generated the assets.
    pub provider: String,
    /// The concrete provider model that generated the assets.
    pub model: String,
    /// The daemon image route that produced the assets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
    /// The daemon-owned image assets created by the request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assets: Vec<AttachmentRef>,
    /// Optional provider-revised prompt captured from the first image result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revised_prompt: Option<String>,
}

/// One image-generation result returned to the model.
pub type GenerateImageToolResponse = ImageToolResponse;

/// One image-edit result returned to the model.
pub type EditImageToolResponse = ImageToolResponse;

/// One daemon-owned audio-generation request executed on behalf of the model.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GenerateAudioToolRequest {
    /// The text that should be synthesized into audio.
    pub input: String,
    /// Optional provider-specific style or tone instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Optional provider-specific voice identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// Optional provider-specific response format such as `mp3` or `pcm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Optional speech speed multiplier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,
    /// Optional route override used to target one specific audio backend.
    #[serde(flatten)]
    pub route: ImageToolRouteOverride,
}

/// One audio-generation result returned to the model.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioToolResponse {
    /// The provider that generated the assets.
    pub provider: String,
    /// The concrete provider model that generated the assets.
    pub model: String,
    /// The configured daemon route that generated the assets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
    /// The daemon-owned audio assets created by the request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assets: Vec<AttachmentRef>,
    /// Optional transcript associated with the generated audio asset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
}

/// One audio-generation result returned to the model.
pub type GenerateAudioToolResponse = AudioToolResponse;

/// One typed message request routed through the daemon mailbox.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageAgentToolRequest {
    /// The destination agent identifier.
    pub agent_id: String,
    /// A short message subject.
    pub subject: String,
    /// The legacy plain-text message body used when `input_items` is empty.
    #[serde(default)]
    pub message: String,
    /// Ordered daemon-owned asset identifiers appended after `message`.
    ///
    /// Use this simpler field when the recipient should receive one text message plus existing
    /// daemon-owned assets. Use `input_items` only when precise multimodal ordering or inline
    /// uploads are required.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asset_ids: Vec<String>,
    /// The ordered multimodal message sequence for the destination agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_items: Vec<SubmitInputItemRequest>,
    /// Optional structured mailbox message type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_type: Option<String>,
}

/// One durable response returned after requesting a parent clarification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParentClarificationToolResponse {
    /// The parent agent identifier that owns the surfaced question.
    pub parent_agent_id: String,
    /// The parent session identifier where the question is now pending.
    pub parent_session_id: String,
    /// The child run that emitted the clarification request, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_run_id: Option<String>,
    /// The child tool call that emitted the clarification request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_tool_call_id: Option<String>,
    /// The synthetic run identifier that carries the waiting question.
    pub run_id: String,
    /// The structured request identifier.
    pub request_id: String,
    /// The mailbox message type used when the answer returns to the child.
    pub response_message_type: String,
}

/// One non-blocking operator notification emitted by a model-facing tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OperatorNotificationRequest {
    /// Optional short subject rendered above or before the message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// The operator-facing message body.
    pub message: String,
    /// Optional urgency label such as `info`, `warning`, `blocker`, or `critical`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urgency: Option<String>,
    /// Internal idempotency key derived from the model tool-call id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// Durable response returned after queueing an operator notification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OperatorNotificationToolResponse {
    /// Whether the notification was accepted for delivery.
    pub queued: bool,
    /// Number of configured reply targets used for this notification.
    pub target_count: usize,
    /// Session that owns the operator contact policy.
    pub session_id: String,
    /// Run that emitted the notification, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Stable output kind recorded on delivery metadata.
    pub output_kind: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct AgentProfileTemplate {
    pub(super) prompt: Option<String>,
    pub(super) prompt_merge_mode: PromptMergeMode,
    pub(super) generation: ModelGenerationConfig,
    pub(super) tool_surface: ToolSurfaceFilter,
}

/// One task update payload used by the task tools.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskMutation {
    /// Optional title replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional description replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional task status replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    /// Optional owner override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_agent_id: Option<String>,
    /// Optional task output replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Optional metadata merge payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// Optional metadata keys to remove.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove_metadata_keys: Option<Vec<String>>,
    /// Optional dependency replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<Vec<String>>,
    /// Optional dependencies to append.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_blocked_by: Option<Vec<String>>,
    /// Optional dependencies to remove.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove_blocked_by: Option<Vec<String>>,
    /// Optional downstream dependency replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocks: Option<Vec<String>>,
    /// Optional downstream dependencies to append.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_blocks: Option<Vec<String>>,
    /// Optional downstream dependencies to remove.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove_blocks: Option<Vec<String>>,
}

/// Mutable daemon control surface used by orchestration tools.
#[async_trait]
pub trait DaemonToolControl: Send + Sync {
    /// Runs one daemon-managed shell task in foreground while still making it recoverable as a task.
    async fn run_foreground_shell_task(
        &self,
        session_id: &str,
        owner_agent_id: &str,
        request: BackgroundShellTaskRequest,
    ) -> Result<ToolExecutionOutput>;

    /// Starts one daemon-managed background shell task owned by the current agent.
    async fn start_background_shell_task(
        &self,
        session_id: &str,
        owner_agent_id: &str,
        request: BackgroundShellTaskRequest,
    ) -> Result<TaskRecord>;

    /// Reads one task output view, optionally waiting for settlement.
    async fn task_output_view(
        &self,
        session_id: &str,
        task_id: &str,
        wait: bool,
        timeout: Duration,
        tail_bytes: usize,
        include_full_output: bool,
    ) -> Result<TaskOutputView>;

    /// Stops one session task and returns the updated terminal snapshot when possible.
    async fn stop_task(
        &self,
        session_id: &str,
        task_id: &str,
        reason: Option<String>,
        actor_agent_id: Option<String>,
    ) -> Result<TaskRecord>;

    /// Spawns one child agent underneath the current agent.
    async fn spawn_agent(
        &self,
        parent_agent_id: &str,
        request: SpawnAgentToolRequest,
    ) -> Result<SpawnAgentToolResponse>;

    /// Returns the latest canonical output recorded for one daemon run.
    async fn latest_run_output(&self, run_id: &str) -> Result<Option<String>>;

    /// Sends one mailbox message to another agent.
    async fn message_agent(
        &self,
        from_agent_id: &str,
        request: MessageAgentToolRequest,
    ) -> Result<PostMailboxResponse>;

    /// Surfaces one structured clarification request through the parent session.
    async fn request_parent_clarification(
        &self,
        session_id: &str,
        requester_agent_id: &str,
        requester_run_id: Option<&str>,
        requester_tool_call_id: Option<&str>,
        request: UserQuestionRequest,
    ) -> Result<ParentClarificationToolResponse>;

    /// Loads the configured model-facing operator policy for one session.
    async fn load_session_operator_config(&self, session_id: &str)
    -> Result<SessionOperatorConfig>;

    /// Queues one non-blocking operator notification through configured session reply targets.
    async fn notify_operator(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        request: OperatorNotificationRequest,
    ) -> Result<OperatorNotificationToolResponse>;

    /// Waits for one agent to reach a terminal state or the timeout to expire.
    async fn wait_agent(
        &self,
        caller_agent_id: &str,
        agent_id: &str,
        timeout: Duration,
    ) -> Result<ManagedAgentSnapshot>;

    /// Lists full snapshots for the currently visible agents.
    async fn list_agents(&self, caller_agent_id: &str) -> Result<Vec<ManagedAgentSnapshot>>;

    /// Lists lightweight summaries for the currently visible agents.
    async fn list_agent_summaries(&self, caller_agent_id: &str) -> Result<Vec<AgentSummaryView>>;

    /// Loads one agent snapshot by identifier.
    async fn get_agent(
        &self,
        caller_agent_id: &str,
        agent_id: &str,
    ) -> Result<ManagedAgentSnapshot>;

    /// Loads one assistant message body for fork-context propagation.
    async fn load_assistant_message(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Option<String>>;

    /// Lists reusable skills visible to the current daemon runtime.
    async fn list_skills(&self, query: Option<&str>) -> Result<Vec<SkillSummary>>;

    /// Loads one reusable skill definition by name.
    async fn get_skill(&self, name: &str) -> Result<Option<SkillDefinition>>;

    /// Loads one daemon-owned promoted learning skill record by name when it exists.
    async fn get_learning_skill(&self, name: &str) -> Result<Option<crate::LearningSkillView>>;

    /// Loads one daemon-owned attachment reference by asset identifier.
    async fn load_asset_attachment(
        &self,
        session_id: &str,
        asset_id: &str,
    ) -> Result<AttachmentRef>;

    /// Reads one public channel thread visible to the current session.
    async fn read_channel_thread(
        &self,
        session_id: &str,
        channel_id: &str,
        thread_root_message_id: &str,
    ) -> Result<Vec<crate::ChannelMessageView>>;

    /// Applies one public channel reaction on behalf of the current actor.
    async fn set_channel_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        actor_id: &str,
        emoji: &str,
    ) -> Result<crate::ChannelMessageView>;

    /// Creates one durable autonomous channel stimulus on behalf of the current session.
    async fn create_channel_stimulus(
        &self,
        session_id: &str,
        channel_id: &str,
        request: crate::CreateChannelStimulusRequest,
    ) -> Result<crate::ChannelStimulusView>;

    /// Generates one or more daemon-owned image assets.
    async fn generate_image(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        tool_call_id: Option<&str>,
        request: GenerateImageToolRequest,
    ) -> Result<GenerateImageToolResponse>;

    /// Edits one or more daemon-owned image assets.
    async fn edit_image(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        tool_call_id: Option<&str>,
        request: EditImageToolRequest,
    ) -> Result<EditImageToolResponse>;

    /// Generates one daemon-owned audio asset.
    async fn generate_audio(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        tool_call_id: Option<&str>,
        request: GenerateAudioToolRequest,
    ) -> Result<GenerateAudioToolResponse>;

    /// Loads the current session control state.
    async fn load_session_control_state(&self, session_id: &str) -> Result<SessionControlState>;

    /// Persists the provided session control state.
    async fn save_session_control_state(
        &self,
        session_id: &str,
        state: SessionControlState,
    ) -> Result<SessionControlState>;

    /// Loads the current long-running session goal, when present.
    async fn load_session_goal(&self, session_id: &str) -> Result<Option<SessionGoal>>;

    /// Creates the current long-running session goal.
    async fn create_session_goal(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        objective: String,
        token_budget: Option<u64>,
        replace_if_inactive: bool,
    ) -> Result<SessionGoal>;

    /// Marks the current session goal complete from the current run.
    async fn complete_session_goal(&self, session_id: &str, run_id: &str) -> Result<SessionGoal>;

    /// Pauses the current session goal from the current run.
    async fn pause_session_goal(&self, session_id: &str, run_id: &str) -> Result<SessionGoal>;

    /// Enters durable session-scoped plan mode.
    async fn enter_session_plan_mode(&self, session_id: &str) -> Result<SessionControlState>;

    /// Leaves durable session-scoped plan mode and records the latest plan artifact.
    async fn exit_session_plan_mode(
        &self,
        session_id: &str,
        plan: String,
        summary: Option<String>,
    ) -> Result<ExitPlanModeOutcome>;

    /// Creates one durable daemon-owned schedule.
    async fn create_schedule(&self, request: ScheduleCreateRequest) -> Result<ScheduleView>;

    /// Lists schedules visible to the current caller.
    async fn list_schedules(&self, session_id: Option<&str>) -> Result<Vec<ScheduleView>>;

    /// Loads one schedule by identifier.
    async fn get_schedule(&self, schedule_id: &str) -> Result<ScheduleView>;

    /// Cancels one schedule.
    async fn cancel_schedule(&self, schedule_id: &str) -> Result<ScheduleView>;

    /// Pauses one schedule.
    async fn pause_schedule(&self, schedule_id: &str) -> Result<ScheduleView>;

    /// Resumes one paused schedule.
    async fn resume_schedule(&self, schedule_id: &str) -> Result<ScheduleView>;

    /// Triggers one schedule immediately while still respecting overlap policy.
    async fn trigger_schedule_now(&self, schedule_id: &str) -> Result<ScheduleView>;
}

/// Deferred daemon control handle used to break the build-time cycle.
#[derive(Clone, Default)]
pub struct DaemonToolControlHandle {
    inner: Arc<RwLock<Option<Weak<dyn DaemonToolControl>>>>,
}

impl DaemonToolControlHandle {
    /// Creates an empty deferred handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds the handle to the live daemon state.
    pub fn bind(&self, control: &Arc<dyn DaemonToolControl>) {
        *self.inner.write() = Some(Arc::downgrade(control));
    }

    pub(super) fn resolve(&self) -> Result<Arc<dyn DaemonToolControl>> {
        self.inner
            .read()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| anyhow!("daemon control is not available"))
    }
}
