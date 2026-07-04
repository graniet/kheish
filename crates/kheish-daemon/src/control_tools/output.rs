//! Tools that let the model compose rich daemon outputs and generated media.

use anyhow::{Result, bail};
use async_trait::async_trait;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolInputKind,
    ToolSchema,
};
use kheish_types::{AttachmentRef, ContentPart, RichOutput};
use serde::Deserialize;
use serde_json::Value;

use super::helpers::{
    build_array_field, build_boolean_field, build_string_field, deserialize_tool_request,
    execution_run_id, execution_session_id,
};
use super::{
    DaemonToolControlHandle, EditImageToolRequest, GenerateAudioToolRequest,
    GenerateImageToolRequest,
};

#[derive(Clone)]
pub(crate) struct EmitOutputTool {
    control: DaemonToolControlHandle,
}

impl EmitOutputTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[derive(Clone)]
pub(crate) struct GenerateImageTool {
    control: DaemonToolControlHandle,
}

impl GenerateImageTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[derive(Clone)]
pub(crate) struct GenerateAudioTool {
    control: DaemonToolControlHandle,
}

impl GenerateAudioTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[derive(Clone)]
pub(crate) struct EditImageTool {
    control: DaemonToolControlHandle,
}

impl EditImageTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

/// Stores a session-workspace file as a daemon-owned asset — the bridge
/// between files an agent writes (reports, PDFs, exports) and the
/// operator-facing asset store.
pub(crate) struct StoreAssetTool {
    control: DaemonToolControlHandle,
}

impl StoreAssetTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[derive(Debug, Deserialize)]
struct StoreAssetRequest {
    path: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    media_type: Option<String>,
}

#[async_trait]
impl Tool for StoreAssetTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "store_asset".to_string(),
            description: "Store a file from the session workspace as a daemon-owned asset. Returns the asset_id; reference it in emit_output parts or artifact_ids to deliver the file to the user.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field(
                        "path",
                        "Workspace-relative path of the file to store (escaping the workspace is rejected).",
                        true,
                    ),
                    build_string_field(
                        "label",
                        "Optional display file name recorded on the asset; defaults to the file's own name.",
                        false,
                    ),
                    build_string_field(
                        "media_type",
                        "Optional declared MIME type; inferred from the file when omitted.",
                        false,
                    ),
                ],
            },
            timeout_ms: 30_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?.to_string();
        let run_id = execution_run_id(&ctx).map(str::to_string);
        let request = deserialize_tool_request::<StoreAssetRequest>(input)?;
        let control = self.control.resolve()?;
        let asset = control
            .store_workspace_asset(
                &session_id,
                run_id.as_deref(),
                Some(ctx.call_id.as_str()),
                &request.path,
                request.label.as_deref(),
                request.media_type.as_deref(),
            )
            .await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(asset)?))
    }
}

#[derive(Debug, Default, Deserialize)]
struct EmitOutputRequest {
    #[serde(default)]
    content: String,
    #[serde(default)]
    parts: Vec<EmitOutputPartRequest>,
    #[serde(default)]
    artifact_ids: Vec<String>,
    #[serde(default)]
    include_artifacts_inline: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EmitOutputPartRequest {
    Text { text: String },
    Asset { asset_id: String },
}

#[async_trait]
impl Tool for EmitOutputTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "emit_output".to_string(),
            description: "Compose the final daemon output shown to the user, including ordered text and daemon-owned assets.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field(
                        "content",
                        "Optional plain-text fallback and preview. When parts omit text, this string is prepended as visible text.",
                        false,
                    ),
                    build_array_field(
                        "parts",
                        "Optional ordered visible output parts. Each item must be an object with type `text` plus `text`, or type `asset` plus `asset_id`.",
                        false,
                        ToolInputKind::Object,
                    ),
                    build_array_field(
                        "artifact_ids",
                        "Optional daemon-owned asset IDs associated with the answer but not necessarily shown inline.",
                        false,
                        ToolInputKind::String,
                    ),
                    build_boolean_field(
                        "include_artifacts_inline",
                        "When true, artifact_ids are also appended as visible attachment parts.",
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
        let request = deserialize_tool_request::<EmitOutputRequest>(input)?;
        let control = self.control.resolve()?;

        let mut parts = Vec::with_capacity(request.parts.len());
        for part in request.parts {
            match part {
                EmitOutputPartRequest::Text { text } => {
                    if !text.trim().is_empty() {
                        parts.push(ContentPart::Text { text });
                    }
                }
                EmitOutputPartRequest::Asset { asset_id } => {
                    parts.push(ContentPart::Attachment {
                        attachment: control
                            .load_asset_attachment(&session_id, &asset_id)
                            .await?,
                    });
                }
            }
        }

        let mut artifacts =
            resolve_artifacts(control.as_ref(), &session_id, &request.artifact_ids).await?;
        if request.include_artifacts_inline {
            for attachment in &artifacts {
                if !parts.iter().any(|part| matches!(
                    part,
                    ContentPart::Attachment { attachment: existing } if existing.id == attachment.id
                )) {
                    parts.push(ContentPart::Attachment {
                        attachment: attachment.clone(),
                    });
                }
            }
        }

        let output = RichOutput {
            content: request.content,
            parts,
            artifacts: std::mem::take(&mut artifacts),
        }
        .normalized();
        if output.content.is_empty() && output.parts.is_empty() && output.artifacts.is_empty() {
            bail!("emit_output requires content, parts, or artifact_ids");
        }

        Ok(ToolExecutionOutput::json(serde_json::to_value(output)?))
    }
}

#[async_trait]
impl Tool for GenerateImageTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "generate_image".to_string(),
            description: "Generate one or more daemon-owned images from a text prompt and return their asset identifiers.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("prompt", "Text prompt describing the image to generate.", true),
                    build_string_field("size", "Optional provider image size such as 1024x1024.", false),
                    super::helpers::build_number_field("count", "Optional number of images to generate.", false),
                    build_string_field(
                        "provider",
                        "Optional configured image route identifier such as `openai`, `openrouter`, or `google-images`.",
                        false,
                    ),
                    build_string_field(
                        "model",
                        "Optional provider-specific image model override.",
                        false,
                    ),
                ],
            },
            timeout_ms: 180_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let run_id = execution_run_id(&ctx);
        let request = deserialize_tool_request::<GenerateImageToolRequest>(input)?;
        let control = self.control.resolve()?;
        let response = control
            .generate_image(session_id, run_id, Some(ctx.call_id.as_str()), request)
            .await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(response)?))
    }
}

#[async_trait]
impl Tool for GenerateAudioTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "generate_audio".to_string(),
            description:
                "Generate one daemon-owned audio asset from text and return its asset identifier."
                    .to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field(
                        "input",
                        "Text that should be synthesized into speech; max 4096 characters.",
                        true,
                    ),
                    build_string_field(
                        "instructions",
                        "Optional provider-specific style or tone instructions; max 4000 characters.",
                        false,
                    ),
                    build_string_field(
                        "voice",
                        "Optional provider-specific voice identifier such as `alloy`, `coral`, or a daemon-supported custom voice id.",
                        false,
                    ),
                    build_string_field(
                        "format",
                        "Optional response format: `mp3`, `wav`, `pcm`, `opus`, `aac`, or `flac` when supported by the selected provider.",
                        false,
                    ),
                    super::helpers::build_number_field(
                        "speed",
                        "Optional speech speed multiplier.",
                        false,
                    ),
                    build_string_field(
                        "provider",
                        "Optional configured audio route identifier such as `openrouter`.",
                        false,
                    ),
                    build_string_field(
                        "model",
                        "Optional provider-specific audio model override.",
                        false,
                    ),
                ],
            },
            timeout_ms: 180_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let run_id = execution_run_id(&ctx);
        let request = deserialize_tool_request::<GenerateAudioToolRequest>(input)?;
        let control = self.control.resolve()?;
        let response = control
            .generate_audio(session_id, run_id, Some(ctx.call_id.as_str()), request)
            .await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(response)?))
    }
}

#[async_trait]
impl Tool for EditImageTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "edit_image".to_string(),
            description: "Edit one or more daemon-owned images using a text instruction and return the new asset identifiers.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("prompt", "Text instruction describing how the provided images should be edited.", true),
                    build_array_field(
                        "image_asset_ids",
                        "Ordered daemon-owned image asset IDs. The first image is the primary image to edit; later images are passed as additional visual context when supported. When omitted, the daemon infers the source image only if the current user turn includes exactly one attached image. Passing an explicit empty array does not trigger inference.",
                        false,
                        ToolInputKind::String,
                    ),
                    build_string_field("size", "Optional provider image size such as 1024x1024.", false),
                    super::helpers::build_number_field("count", "Optional number of edited images to return.", false),
                    build_string_field(
                        "provider",
                        "Optional configured image route identifier such as `openai`, `openrouter`, or `google-images`.",
                        false,
                    ),
                    build_string_field(
                        "model",
                        "Optional provider-specific image model override.",
                        false,
                    ),
                ],
            },
            timeout_ms: 180_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let run_id = execution_run_id(&ctx);
        let request = deserialize_tool_request::<EditImageToolRequest>(input)?;
        let control = self.control.resolve()?;
        let response = control
            .edit_image(session_id, run_id, Some(ctx.call_id.as_str()), request)
            .await?;
        Ok(ToolExecutionOutput::json(serde_json::to_value(response)?))
    }
}

async fn resolve_artifacts(
    control: &dyn super::DaemonToolControl,
    session_id: &str,
    asset_ids: &[String],
) -> Result<Vec<AttachmentRef>> {
    let mut resolved: Vec<AttachmentRef> = Vec::with_capacity(asset_ids.len());
    for asset_id in asset_ids {
        let attachment = control.load_asset_attachment(session_id, asset_id).await?;
        if resolved.iter().any(|existing| existing.id == attachment.id) {
            continue;
        }
        resolved.push(attachment);
    }
    Ok(resolved)
}
