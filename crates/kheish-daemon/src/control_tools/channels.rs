//! Tools that let agents inspect public channel threads and react without posting.

use anyhow::{Result, bail};
use async_trait::async_trait;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolSchema,
};
use serde::Deserialize;
use serde_json::Value;

use super::DaemonToolControlHandle;
use super::helpers::{
    build_array_field, build_number_field, build_string_field, deserialize_tool_request,
    execution_run_id, execution_session_id,
};

#[derive(Clone)]
pub(crate) struct ReadChannelThreadTool {
    control: DaemonToolControlHandle,
}

impl ReadChannelThreadTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[derive(Clone)]
pub(crate) struct SetChannelReactionTool {
    control: DaemonToolControlHandle,
}

impl SetChannelReactionTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[derive(Clone)]
pub(crate) struct CreateChannelStimulusTool {
    control: DaemonToolControlHandle,
}

impl CreateChannelStimulusTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[derive(Debug, Deserialize)]
struct ReadChannelThreadRequest {
    channel_id: String,
    thread_root_message_id: String,
}

#[derive(Debug, Deserialize)]
struct SetChannelReactionRequest {
    channel_id: String,
    message_id: String,
    emoji: String,
}

#[derive(Debug, Deserialize)]
struct CreateChannelStimulusRequest {
    channel_id: String,
    content: String,
    #[serde(default)]
    kind: Option<crate::ChannelStimulusKind>,
    #[serde(default)]
    scope: Option<crate::ChannelStimulusScope>,
    #[serde(default)]
    thread_root_message_id: Option<String>,
    #[serde(default)]
    visibility_hint: Option<crate::ChannelStimulusVisibilityHint>,
    #[serde(default)]
    addressed_member_ids: Vec<String>,
    #[serde(default)]
    source_kind: Option<String>,
    #[serde(default)]
    source_ref: Option<String>,
    #[serde(default)]
    dedupe_key: Option<String>,
    #[serde(default)]
    progress_key: Option<String>,
    #[serde(default)]
    expires_at_ms: Option<u64>,
}

#[async_trait]
impl Tool for ReadChannelThreadTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "read_channel_thread".to_string(),
            description: "Read the current public messages in one channel thread before replying."
                .to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("channel_id", "The daemon-owned channel identifier.", true),
                    build_string_field(
                        "thread_root_message_id",
                        "The public thread root message identifier.",
                        true,
                    ),
                ],
            },
            timeout_ms: 30_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let request = deserialize_tool_request::<ReadChannelThreadRequest>(input)?;
        let control = self.control.resolve()?;
        let messages = control
            .read_channel_thread(
                &session_id,
                &request.channel_id,
                &request.thread_root_message_id,
            )
            .await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(messages)?))
    }
}

#[async_trait]
impl Tool for SetChannelReactionTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "set_channel_reaction".to_string(),
            description:
                "Apply one lightweight public reaction to a channel message instead of posting a reply."
                    .to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("channel_id", "The daemon-owned channel identifier.", true),
                    build_string_field("message_id", "The public message identifier.", true),
                    build_string_field(
                        "emoji",
                        "The reaction emoji or token, such as 👍, 👀, or :thumbsup:.",
                        true,
                    ),
                ],
            },
            timeout_ms: 30_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let request = deserialize_tool_request::<SetChannelReactionRequest>(input)?;
        if request.emoji.trim().is_empty() {
            bail!("emoji is required");
        }
        let control = self.control.resolve()?;
        let message = control
            .set_channel_reaction(
                &request.channel_id,
                &request.message_id,
                &session_id,
                request.emoji.trim(),
            )
            .await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(message)?))
    }
}

#[async_trait]
impl Tool for CreateChannelStimulusTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "create_channel_stimulus".to_string(),
            description: "Queue one autonomous channel follow-up or new public subject without posting directly."
                .to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("channel_id", "The daemon-owned channel identifier.", true),
                    build_string_field(
                        "content",
                        "The public text that should appear in the channel before routing continues.",
                        true,
                    ),
                    build_string_field(
                        "kind",
                        "Optional stimulus kind such as agent_idea, result_summary, or thread_idle_followup.",
                        false,
                    ),
                    build_string_field(
                        "scope",
                        "Optional scope: channel opens a main-feed subject, thread continues one existing thread.",
                        false,
                    ),
                    build_string_field(
                        "thread_root_message_id",
                        "Optional canonical thread root required for thread-scoped stimuli.",
                        false,
                    ),
                    build_string_field(
                        "visibility_hint",
                        "Optional visibility hint: auto, main, or thread.",
                        false,
                    ),
                    build_array_field(
                        "addressed_member_ids",
                        "Optional member identifiers that should receive first-turn priority once the stimulus is materialized.",
                        false,
                        kheish_runtime::ToolInputKind::String,
                    ),
                    build_string_field(
                        "source_kind",
                        "Optional stable source kind such as agent_idea, schedule, reviewer, or task.",
                        false,
                    ),
                    build_string_field(
                        "source_ref",
                        "Optional stable source identifier used for dedupe and recovery.",
                        false,
                    ),
                    build_string_field(
                        "dedupe_key",
                        "Optional dedupe key used to coalesce equivalent queued stimuli.",
                        false,
                    ),
                    build_string_field(
                        "progress_key",
                        "Optional progress key used to supersede stale progress updates.",
                        false,
                    ),
                    build_number_field(
                        "expires_at_ms",
                        "Optional Unix timestamp in milliseconds after which the stimulus should be canceled instead of dispatched.",
                        false,
                    ),
                ],
            },
            timeout_ms: 30_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let request = deserialize_tool_request::<CreateChannelStimulusRequest>(input)?;
        if request.content.trim().is_empty() {
            bail!("content is required");
        }
        let kind = request
            .kind
            .unwrap_or(crate::ChannelStimulusKind::AgentIdea);
        let run_id = execution_run_id(&ctx).map(ToOwned::to_owned);
        let control = self.control.resolve()?;
        let stimulus = control
            .create_channel_stimulus(
                &session_id,
                &request.channel_id,
                crate::CreateChannelStimulusRequest {
                    scope: request.scope.unwrap_or_default(),
                    thread_root_message_id: request.thread_root_message_id,
                    kind: kind.clone(),
                    visibility_hint: request.visibility_hint,
                    content: request.content.trim().to_string(),
                    addressed_member_ids: request.addressed_member_ids,
                    sender_session_id: None,
                    sender_actor_id: None,
                    sender_display_name: None,
                    source_kind: request.source_kind.or_else(|| {
                        Some(match kind {
                            crate::ChannelStimulusKind::AgentIdea => "agent_idea".to_string(),
                            _ => "agent".to_string(),
                        })
                    }),
                    source_ref: request.source_ref.or_else(|| run_id.clone()),
                    dedupe_key: request.dedupe_key,
                    progress_key: request.progress_key,
                    available_at_ms: None,
                    expires_at_ms: request.expires_at_ms,
                    metadata: serde_json::json!({
                        "requested_by_session_id": session_id,
                        "requested_by_run_id": run_id,
                    }),
                },
            )
            .await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(stimulus)?))
    }
}
