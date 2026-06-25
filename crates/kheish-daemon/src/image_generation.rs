//! Daemon-owned image generation backed by provider-specific runtimes.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use image::GenericImageView;
use kheish_runtime::{
    GoogleImageEditInput, GoogleImageEditRequest, GoogleImageEditor, GoogleImageGenerationRequest,
    GoogleImageGenerator, GoogleImageProviderConfig, OpenAiImageEditInput, OpenAiImageEditRequest,
    OpenAiImageEditor, OpenAiImageGenerationRequest, OpenAiImageGenerator, OpenAiProviderConfig,
    OpenRouterImageEditInput, OpenRouterImageEditRequest, OpenRouterImageEditor,
    OpenRouterImageGenerationRequest, OpenRouterImageGenerator, OpenRouterProviderConfig,
    RuntimeObserver, XAiImageEditor, XAiImageGenerator, XAiProviderConfig,
    resolve_google_image_model, resolve_openai_image_model, resolve_openrouter_image_model,
    resolve_xai_image_model,
};
use sha2::{Digest, Sha256};

use crate::assets::{
    AssetProvenanceRecord, AssetProvenanceSourceRecord, FileAssetStore, decode_image_with_limits,
    validate_image_payload_dimensions,
};
use crate::control_tools::{
    EditImageToolRequest, EditImageToolResponse, GenerateImageToolRequest,
    GenerateImageToolResponse, ImageToolResponse, ImageToolRouteOverride,
};
use crate::model_routing::ModelRouteConfig;

const MAX_EDIT_IMAGE_INPUTS: usize = 8;
const MAX_EDIT_IMAGE_INPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_IMAGE_PROMPT_CHARS: usize = 8_000;
const MAX_IMAGE_OUTPUTS: u32 = 4;
const MAX_IMAGE_SIZE_HINT_CHARS: usize = 32;
const MAX_IMAGE_REQUEST_EDGE_PX: u32 = 4096;
const MAX_IMAGE_REQUEST_PIXELS: u64 =
    MAX_IMAGE_REQUEST_EDGE_PX as u64 * MAX_IMAGE_REQUEST_EDGE_PX as u64;
const MAX_IMAGE_REQUEST_DECODE_ALLOC_BYTES: u64 = 128 * 1024 * 1024;

/// One provider-specific batch of generated image bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GeneratedImageBatch {
    /// The provider that generated the images.
    pub provider: String,
    /// The concrete provider model that generated the images.
    pub model: String,
    /// The generated image payloads.
    pub images: Vec<GeneratedImagePayload>,
    /// Optional provider-revised prompt from the generation response.
    pub revised_prompt: Option<String>,
}

/// One generated image payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GeneratedImagePayload {
    /// The normalized MIME type returned by the provider.
    pub media_type: String,
    /// The encoded image bytes persisted by the daemon asset store.
    pub bytes: Vec<u8>,
}

/// One daemon-owned source image supplied for editing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EditImageSource {
    /// The daemon-owned source asset identifier.
    pub asset_id: String,
    /// The original file name persisted by the daemon asset store.
    pub file_name: String,
    /// The normalized MIME type persisted by the daemon asset store.
    pub media_type: String,
    /// The raw SHA-256 persisted by the daemon asset store.
    pub sha256: String,
    /// The raw image bytes loaded from the daemon asset store.
    pub bytes: Vec<u8>,
}

/// Run/session context used to stamp daemon-generated image assets with provenance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImageToolExecutionContext {
    /// The session that requested the image operation.
    pub session_id: Option<String>,
    /// The run that requested the image operation.
    pub run_id: Option<String>,
    /// The tool-call id that requested the image operation.
    pub tool_call_id: Option<String>,
}

/// One provider-neutral image-edit request after daemon asset resolution.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImageEditBackendRequest {
    /// The text instruction describing the requested edits.
    pub prompt: String,
    /// The ordered source images supplied to the provider.
    pub images: Vec<EditImageSource>,
    /// The number of edited images to return.
    pub count: Option<u32>,
    /// Optional size override passed to the provider.
    pub size: Option<String>,
}

/// Provider-neutral image-generation backend contract.
#[async_trait]
pub(crate) trait ImageGenerationBackend: Send + Sync {
    /// Returns the stable provider name exposed by this backend.
    fn provider(&self) -> &str;

    /// Generates one or more images for the provided request.
    async fn generate(
        &self,
        request: &GenerateImageToolRequest,
        model_override: Option<&str>,
    ) -> Result<GeneratedImageBatch>;

    /// Returns whether this backend supports daemon image editing.
    fn supports_edit(&self) -> bool {
        false
    }

    /// Edits one or more source images for the provided request.
    async fn edit(
        &self,
        _request: &ImageEditBackendRequest,
        _model_override: Option<&str>,
    ) -> Result<GeneratedImageBatch> {
        bail!(
            "image editing is not supported by provider {}",
            self.provider()
        )
    }
}

/// One daemon-owned image-generation service that persists generated bytes as assets.
pub(crate) struct ImageGenerationService {
    assets: Arc<FileAssetStore>,
    backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>>,
    default_route_id: String,
}

struct ImagePersistenceContext {
    kind: &'static str,
    tool_name: &'static str,
    route_id: Option<String>,
    prompt_sha256: String,
    source_assets: Vec<AssetProvenanceSourceRecord>,
    execution: ImageToolExecutionContext,
}

/// One additional daemon-owned image backend configured outside the text model route inventory.
#[derive(Clone)]
pub struct AdditionalImageBackendConfig {
    route_id: String,
    route: ModelRouteConfig,
}

impl AdditionalImageBackendConfig {
    /// Builds one additional image backend whose route identifier matches the provider family.
    pub fn route(route: ModelRouteConfig) -> Self {
        Self::named(route.provider_name(), route)
    }

    /// Builds one additional image backend from a fully resolved daemon route config.
    pub fn named(route_id: impl Into<String>, route: ModelRouteConfig) -> Self {
        Self {
            route_id: route_id.into(),
            route,
        }
    }

    /// Builds one additional OpenAI image backend.
    pub fn openai(config: OpenAiProviderConfig) -> Self {
        Self::route(ModelRouteConfig::OpenAi(config))
    }

    /// Builds one additional Google image backend.
    pub fn google(config: GoogleImageProviderConfig) -> Self {
        Self::route(ModelRouteConfig::Google(config))
    }

    /// Builds one additional OpenRouter image backend.
    pub fn openrouter(config: OpenRouterProviderConfig) -> Self {
        Self::route(ModelRouteConfig::OpenRouter(config))
    }

    /// Builds one additional xAI image backend.
    pub fn xai(config: XAiProviderConfig) -> Self {
        Self::route(ModelRouteConfig::XAi(config))
    }

    /// Returns the stable route identifier used by this additional image backend.
    pub fn route_id(&self) -> &str {
        &self.route_id
    }

    /// Returns the resolved provider route used by this additional image backend.
    pub fn route_config(&self) -> &ModelRouteConfig {
        &self.route
    }
}

/// Google-backed image-generation backend.
pub(crate) struct GoogleImageGenerationBackend {
    config: GoogleImageProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

impl GoogleImageGenerationBackend {
    /// Builds one Google-backed image backend.
    pub(crate) fn from_config(
        mut config: GoogleImageProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self> {
        config.model = resolve_google_image_model(&config.model);
        Ok(Self { config, observer })
    }
}

#[async_trait]
impl ImageGenerationBackend for GoogleImageGenerationBackend {
    fn provider(&self) -> &str {
        "google"
    }

    async fn generate(
        &self,
        request: &GenerateImageToolRequest,
        model_override: Option<&str>,
    ) -> Result<GeneratedImageBatch> {
        let generated = GoogleImageGenerator::new(
            google_config_for_image_model(&self.config, model_override),
            self.observer.clone(),
        )?
        .generate(GoogleImageGenerationRequest {
            prompt: request.prompt.clone(),
            count: request.count.unwrap_or(1),
            size: request.size.clone(),
        })
        .await
        .map_err(|error| anyhow!(error.message))?;
        Ok(GeneratedImageBatch {
            provider: self.provider().to_string(),
            model: generated.model,
            images: generated
                .images
                .into_iter()
                .map(|image| GeneratedImagePayload {
                    media_type: image.media_type,
                    bytes: image.bytes,
                })
                .collect(),
            revised_prompt: generated.text,
        })
    }

    fn supports_edit(&self) -> bool {
        true
    }

    async fn edit(
        &self,
        request: &ImageEditBackendRequest,
        model_override: Option<&str>,
    ) -> Result<GeneratedImageBatch> {
        let edited = GoogleImageEditor::new(
            google_config_for_image_model(&self.config, model_override),
            self.observer.clone(),
        )?
        .edit(GoogleImageEditRequest {
            prompt: request.prompt.clone(),
            images: request
                .images
                .iter()
                .map(|image| GoogleImageEditInput {
                    file_name: image.file_name.clone(),
                    media_type: image.media_type.clone(),
                    bytes: image.bytes.clone(),
                })
                .collect(),
            count: request.count.unwrap_or(1),
            size: request.size.clone(),
        })
        .await
        .map_err(|error| anyhow!(error.message))?;
        Ok(GeneratedImageBatch {
            provider: self.provider().to_string(),
            model: edited.model,
            images: edited
                .images
                .into_iter()
                .map(|image| GeneratedImagePayload {
                    media_type: image.media_type,
                    bytes: image.bytes,
                })
                .collect(),
            revised_prompt: edited.text,
        })
    }
}

impl ImageGenerationService {
    /// Builds one image-generation service from provider-neutral backends.
    pub(crate) fn new(
        backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>>,
        default_route_id: String,
        assets: Arc<FileAssetStore>,
    ) -> Result<Self> {
        if !backends.contains_key(&default_route_id) {
            bail!(
                "image-generation default route '{}' is not configured",
                default_route_id
            );
        }
        Ok(Self {
            assets,
            backends,
            default_route_id,
        })
    }

    /// Generates one or more daemon-owned image assets.
    #[allow(dead_code)]
    pub(crate) async fn generate(
        &self,
        request: GenerateImageToolRequest,
        preferred_route_id: Option<&str>,
        credential_scope: Option<&kheish_types::CredentialScope>,
    ) -> Result<GenerateImageToolResponse> {
        self.generate_with_context(
            request,
            preferred_route_id,
            credential_scope,
            ImageToolExecutionContext::default(),
        )
        .await
    }

    /// Generates one or more daemon-owned image assets with durable provenance context.
    pub(crate) async fn generate_with_context(
        &self,
        request: GenerateImageToolRequest,
        preferred_route_id: Option<&str>,
        credential_scope: Option<&kheish_types::CredentialScope>,
        context: ImageToolExecutionContext,
    ) -> Result<GenerateImageToolResponse> {
        validate_generate_request(&request)?;
        let route_ids = generation_candidate_route_ids(
            &self.backends,
            &request.route,
            preferred_route_id,
            &self.default_route_id,
            credential_scope,
        )?;
        let mut failures = Vec::new();
        for route_id in route_ids {
            let backend = self.backends.get(route_id).ok_or_else(|| {
                anyhow!("no image-generation backend is configured for route {route_id}")
            })?;
            match backend
                .generate(&request, request.route.model.as_deref())
                .await
                .and_then(|generated| {
                    validate_generated_batch(&generated)?;
                    Ok(generated)
                }) {
                Ok(generated) => {
                    return self.persist_generated_images(
                        generated,
                        ImagePersistenceContext {
                            kind: "image_generation",
                            tool_name: "generate_image",
                            route_id: Some(route_id.to_string()),
                            prompt_sha256: sha256_hex(request.prompt.as_bytes()),
                            source_assets: Vec::new(),
                            execution: context,
                        },
                    );
                }
                Err(error) => {
                    failures.push(format!("{route_id}: {error:#}"));
                    if request.route.provider.is_some() {
                        break;
                    }
                }
            }
        }
        bail!(
            "all image-generation routes failed: {}",
            failures.join("; ")
        )
    }

    /// Edits one or more daemon-owned image assets.
    #[allow(dead_code)]
    pub(crate) async fn edit(
        &self,
        request: EditImageToolRequest,
        preferred_route_id: Option<&str>,
        credential_scope: Option<&kheish_types::CredentialScope>,
    ) -> Result<EditImageToolResponse> {
        self.edit_with_context(
            request,
            preferred_route_id,
            credential_scope,
            ImageToolExecutionContext::default(),
        )
        .await
    }

    /// Edits one or more daemon-owned image assets with durable provenance context.
    pub(crate) async fn edit_with_context(
        &self,
        request: EditImageToolRequest,
        preferred_route_id: Option<&str>,
        credential_scope: Option<&kheish_types::CredentialScope>,
        context: ImageToolExecutionContext,
    ) -> Result<EditImageToolResponse> {
        validate_edit_request(&request)?;
        if request.image_asset_ids.is_empty() {
            bail!("edit_image requires at least one image_asset_id");
        }
        let sources = self.resolve_edit_sources(&request.image_asset_ids)?;
        let source_assets = sources
            .iter()
            .map(|source| AssetProvenanceSourceRecord {
                asset_id: source.asset_id.clone(),
                media_type: source.media_type.clone(),
                sha256: source.sha256.clone(),
            })
            .collect::<Vec<_>>();
        let backend_request = ImageEditBackendRequest {
            prompt: request.prompt.clone(),
            images: sources,
            count: request.count,
            size: request.size.clone(),
        };
        let route_ids =
            self.edit_candidate_route_ids(&request.route, preferred_route_id, credential_scope)?;
        let mut failures = Vec::new();
        for route_id in route_ids {
            let backend = self.backends.get(route_id).ok_or_else(|| {
                anyhow!("no image-edit backend is configured for route {route_id}")
            })?;
            match backend
                .edit(&backend_request, request.route.model.as_deref())
                .await
                .and_then(|generated| {
                    validate_generated_batch(&generated)?;
                    Ok(generated)
                }) {
                Ok(generated) => {
                    return self.persist_generated_images(
                        generated,
                        ImagePersistenceContext {
                            kind: "image_edit",
                            tool_name: "edit_image",
                            route_id: Some(route_id.to_string()),
                            prompt_sha256: sha256_hex(request.prompt.as_bytes()),
                            source_assets: source_assets.clone(),
                            execution: context,
                        },
                    );
                }
                Err(error) => {
                    failures.push(format!("{route_id}: {error:#}"));
                    if request.route.provider.is_some() {
                        break;
                    }
                }
            }
        }
        bail!("all image-edit routes failed: {}", failures.join("; "))
    }

    fn resolve_edit_sources(&self, image_asset_ids: &[String]) -> Result<Vec<EditImageSource>> {
        if image_asset_ids.len() > MAX_EDIT_IMAGE_INPUTS {
            bail!(
                "edit_image accepts at most {} source images per request",
                MAX_EDIT_IMAGE_INPUTS
            );
        }
        let mut sources = Vec::with_capacity(image_asset_ids.len());
        let mut total_bytes = 0usize;
        for asset_id in image_asset_ids {
            let (record, bytes) = self.assets.read_raw(asset_id)?;
            if !record.is_image() {
                bail!(
                    "edit_image only accepts image assets; asset {} has media type {}",
                    record.id,
                    record.media_type
                );
            }
            validate_stored_edit_source(&record, &bytes)?;
            total_bytes = total_bytes.saturating_add(bytes.len());
            if total_bytes > MAX_EDIT_IMAGE_INPUT_BYTES {
                bail!(
                    "edit_image source images exceed the {} byte limit",
                    MAX_EDIT_IMAGE_INPUT_BYTES
                );
            }
            sources.push(EditImageSource {
                asset_id: record.id,
                file_name: record.file_name,
                media_type: record.media_type,
                sha256: record.sha256,
                bytes,
            });
        }
        Ok(sources)
    }

    fn edit_candidate_route_ids<'a>(
        &'a self,
        route_override: &'a ImageToolRouteOverride,
        preferred_route_id: Option<&'a str>,
        credential_scope: Option<&kheish_types::CredentialScope>,
    ) -> Result<Vec<&'a str>> {
        if let Some(route_id) = route_override.provider.as_deref() {
            ensure_route_allowed(credential_scope, route_id)?;
            let backend = self.backends.get(route_id).ok_or_else(|| {
                anyhow!("no image-edit backend is configured for route {route_id}")
            })?;
            if backend.supports_edit() {
                return Ok(vec![route_id]);
            }
            bail!("image editing is not supported by route {route_id}");
        }
        let mut route_ids = Vec::new();
        if let Some(route_id) = preferred_route_id
            && route_is_allowed(credential_scope, route_id)
            && self
                .backends
                .get(route_id)
                .is_some_and(|backend| backend.supports_edit())
        {
            push_unique_route(&mut route_ids, route_id);
        }
        if route_is_allowed(credential_scope, &self.default_route_id)
            && self
                .backends
                .get(&self.default_route_id)
                .is_some_and(|backend| backend.supports_edit())
        {
            push_unique_route(&mut route_ids, &self.default_route_id);
        }
        for (route_id, backend) in &self.backends {
            if route_is_allowed(credential_scope, route_id) && backend.supports_edit() {
                push_unique_route(&mut route_ids, route_id);
            }
        }
        if !route_ids.is_empty() {
            return Ok(route_ids);
        }
        if credential_scope.is_some() {
            bail!("credential scope blocks all image-edit routes");
        }
        bail!("no image-edit backend is configured")
    }

    fn persist_generated_images(
        &self,
        generated: GeneratedImageBatch,
        context: ImagePersistenceContext,
    ) -> Result<ImageToolResponse> {
        validate_generated_batch(&generated)?;
        let mut attachments = Vec::with_capacity(generated.images.len());
        let output_count = generated.images.len() as u32;
        for (index, image) in generated.images.iter().enumerate() {
            let extension = preferred_extension_for_media_type(&image.media_type)?;
            let record = self.assets.import_bytes_with_provenance(
                &format!(
                    "generated-image-{}-{}.{}",
                    generated.model,
                    index + 1,
                    extension
                ),
                Some(&image.media_type),
                &image.bytes,
                Some(AssetProvenanceRecord {
                    kind: context.kind.to_string(),
                    tool_name: context.tool_name.to_string(),
                    session_id: context.execution.session_id.clone(),
                    run_id: context.execution.run_id.clone(),
                    tool_call_id: context.execution.tool_call_id.clone(),
                    route_id: context.route_id.clone(),
                    provider: generated.provider.clone(),
                    model: generated.model.clone(),
                    prompt_sha256: context.prompt_sha256.clone(),
                    source_assets: context.source_assets.clone(),
                    output_index: index as u32 + 1,
                    output_count,
                }),
            )?;
            attachments.push(record.attachment_ref());
        }
        Ok(ImageToolResponse {
            provider: generated.provider,
            model: generated.model,
            route_id: context.route_id,
            assets: attachments,
            revised_prompt: generated.revised_prompt,
        })
    }
}

fn validate_generate_request(request: &GenerateImageToolRequest) -> Result<()> {
    validate_prompt(&request.prompt, "generate_image")?;
    validate_count(request.count, "generate_image")?;
    validate_size_hint(request.size.as_deref())?;
    Ok(())
}

fn validate_edit_request(request: &EditImageToolRequest) -> Result<()> {
    validate_prompt(&request.prompt, "edit_image")?;
    validate_count(request.count, "edit_image")?;
    validate_size_hint(request.size.as_deref())?;
    Ok(())
}

fn validate_prompt(prompt: &str, tool_name: &str) -> Result<()> {
    let trimmed = prompt.trim();
    anyhow::ensure!(!trimmed.is_empty(), "{tool_name} prompt must not be empty");
    anyhow::ensure!(
        trimmed.chars().count() <= MAX_IMAGE_PROMPT_CHARS,
        "{tool_name} prompt exceeds the {} character limit",
        MAX_IMAGE_PROMPT_CHARS
    );
    Ok(())
}

fn validate_count(count: Option<u32>, tool_name: &str) -> Result<()> {
    let count = count.unwrap_or(1);
    anyhow::ensure!(count > 0, "{tool_name} count must be at least 1");
    anyhow::ensure!(
        count <= MAX_IMAGE_OUTPUTS,
        "{tool_name} count must be at most {}",
        MAX_IMAGE_OUTPUTS
    );
    Ok(())
}

fn validate_size_hint(size: Option<&str>) -> Result<()> {
    let Some(size) = size.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    anyhow::ensure!(
        size.chars().count() <= MAX_IMAGE_SIZE_HINT_CHARS,
        "image size hint exceeds the {} character limit",
        MAX_IMAGE_SIZE_HINT_CHARS
    );
    anyhow::ensure!(
        size.chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, 'x' | 'X' | ':' | '-' | '_')),
        "image size hint contains unsupported characters"
    );
    let normalized = size.to_ascii_lowercase();
    if let Some((width, height)) = normalized.split_once('x')
        && width.chars().all(|ch| ch.is_ascii_digit())
        && height.chars().all(|ch| ch.is_ascii_digit())
    {
        let width = width
            .parse::<u32>()
            .map_err(|_| anyhow!("invalid image size {size}; width is not a valid integer"))?;
        let height = height
            .parse::<u32>()
            .map_err(|_| anyhow!("invalid image size {size}; height is not a valid integer"))?;
        anyhow::ensure!(width > 0 && height > 0, "image dimensions must be positive");
        anyhow::ensure!(
            width.max(height) <= MAX_IMAGE_REQUEST_EDGE_PX,
            "image dimensions exceed the {}px edge limit",
            MAX_IMAGE_REQUEST_EDGE_PX
        );
    }
    Ok(())
}

fn validate_generated_image_payload(image: &GeneratedImagePayload) -> Result<()> {
    validate_image_bytes(&image.media_type, &image.bytes)
        .with_context(|| format!("invalid generated image payload ({})", image.media_type))
}

fn validate_stored_edit_source(
    record: &crate::assets::StoredAssetRecord,
    bytes: &[u8],
) -> Result<()> {
    let digest = hex::encode(Sha256::digest(bytes));
    anyhow::ensure!(
        digest == record.sha256,
        "asset {} raw payload checksum mismatch",
        record.id
    );
    validate_image_bytes(&record.media_type, bytes)
        .with_context(|| format!("asset {} is not a valid edit image", record.id))
}

fn validate_image_bytes(media_type: &str, bytes: &[u8]) -> Result<()> {
    anyhow::ensure!(!bytes.is_empty(), "image payload is empty");
    validate_image_payload_dimensions(
        media_type,
        bytes,
        MAX_IMAGE_REQUEST_EDGE_PX,
        MAX_IMAGE_REQUEST_PIXELS,
        "",
    )?;
    let decoded = decode_image_with_limits(
        media_type,
        bytes,
        MAX_IMAGE_REQUEST_EDGE_PX,
        MAX_IMAGE_REQUEST_DECODE_ALLOC_BYTES,
    )
    .with_context(|| format!("failed to decode {media_type} image"))?;
    let (width, height) = decoded.dimensions();
    anyhow::ensure!(width > 0 && height > 0, "image dimensions must be positive");
    anyhow::ensure!(
        width.max(height) <= MAX_IMAGE_REQUEST_EDGE_PX,
        "image dimensions exceed the {}px edge limit",
        MAX_IMAGE_REQUEST_EDGE_PX
    );
    Ok(())
}

fn validate_generated_batch(generated: &GeneratedImageBatch) -> Result<()> {
    if generated.images.is_empty() {
        bail!("image provider {} returned no images", generated.provider);
    }
    if generated.images.len() > MAX_IMAGE_OUTPUTS as usize {
        bail!(
            "image provider {} returned {} images, exceeding the {} image limit",
            generated.provider,
            generated.images.len(),
            MAX_IMAGE_OUTPUTS
        );
    }
    for image in &generated.images {
        validate_generated_image_payload(image)?;
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn generation_candidate_route_ids<'a>(
    backends: &'a BTreeMap<String, Arc<dyn ImageGenerationBackend>>,
    route_override: &'a ImageToolRouteOverride,
    preferred_route_id: Option<&'a str>,
    default_route_id: &'a str,
    credential_scope: Option<&kheish_types::CredentialScope>,
) -> Result<Vec<&'a str>> {
    if let Some(route_id) = route_override.provider.as_deref() {
        ensure_route_allowed(credential_scope, route_id)?;
        if backends.contains_key(route_id) {
            return Ok(vec![route_id]);
        }
        bail!("no image-generation backend is configured for route {route_id}");
    }
    let mut route_ids = Vec::new();
    if let Some(route_id) = preferred_route_id
        .filter(|route_id| backends.contains_key(*route_id))
        .filter(|route_id| route_is_allowed(credential_scope, route_id))
    {
        push_unique_route(&mut route_ids, route_id);
    }
    if backends.contains_key(default_route_id)
        && route_is_allowed(credential_scope, default_route_id)
    {
        push_unique_route(&mut route_ids, default_route_id);
    }
    for route_id in backends.keys() {
        if route_is_allowed(credential_scope, route_id) {
            push_unique_route(&mut route_ids, route_id);
        }
    }
    if !route_ids.is_empty() {
        return Ok(route_ids);
    }
    if credential_scope.is_some() {
        bail!("credential scope blocks all image-generation routes");
    }
    Ok(vec![default_route_id])
}

fn push_unique_route<'a>(route_ids: &mut Vec<&'a str>, route_id: &'a str) {
    if !route_ids.contains(&route_id) {
        route_ids.push(route_id);
    }
}

fn route_is_allowed(
    credential_scope: Option<&kheish_types::CredentialScope>,
    route_id: &str,
) -> bool {
    credential_scope.is_none_or(|scope| scope.is_empty() || scope.allows_route(route_id))
}

fn ensure_route_allowed(
    credential_scope: Option<&kheish_types::CredentialScope>,
    route_id: &str,
) -> Result<()> {
    anyhow::ensure!(
        route_is_allowed(credential_scope, route_id),
        "credential scope blocks route {route_id}"
    );
    Ok(())
}

/// OpenAI-backed image-generation backend.
pub(crate) struct OpenAiImageGenerationBackend {
    config: OpenAiProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

impl OpenAiImageGenerationBackend {
    /// Builds one OpenAI-backed image backend.
    pub(crate) fn from_config(
        mut config: OpenAiProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self> {
        config.model = resolve_openai_image_model(&config.model);
        Ok(Self { config, observer })
    }
}

#[async_trait]
impl ImageGenerationBackend for OpenAiImageGenerationBackend {
    fn provider(&self) -> &str {
        "openai"
    }

    async fn generate(
        &self,
        request: &GenerateImageToolRequest,
        model_override: Option<&str>,
    ) -> Result<GeneratedImageBatch> {
        let generated = OpenAiImageGenerator::new(
            openai_config_for_image_model(&self.config, model_override),
            self.observer.clone(),
        )?
        .generate(OpenAiImageGenerationRequest {
            prompt: request.prompt.clone(),
            count: request.count.unwrap_or(1),
            size: request.size.clone(),
        })
        .await
        .map_err(|error| anyhow!(error.message))?;
        let revised_prompt = generated
            .images
            .iter()
            .find_map(|image| image.revised_prompt.clone());
        Ok(GeneratedImageBatch {
            provider: self.provider().to_string(),
            model: generated.model,
            images: generated
                .images
                .into_iter()
                .map(|image| GeneratedImagePayload {
                    media_type: image.media_type,
                    bytes: image.bytes,
                })
                .collect(),
            revised_prompt,
        })
    }

    fn supports_edit(&self) -> bool {
        true
    }

    async fn edit(
        &self,
        request: &ImageEditBackendRequest,
        model_override: Option<&str>,
    ) -> Result<GeneratedImageBatch> {
        let edited = OpenAiImageEditor::new(
            openai_config_for_image_model(&self.config, model_override),
            self.observer.clone(),
        )?
        .edit(OpenAiImageEditRequest {
            prompt: request.prompt.clone(),
            images: request
                .images
                .iter()
                .map(|image| OpenAiImageEditInput {
                    file_name: image.file_name.clone(),
                    media_type: image.media_type.clone(),
                    bytes: image.bytes.clone(),
                })
                .collect(),
            count: request.count.unwrap_or(1),
            size: request.size.clone(),
        })
        .await
        .map_err(|error| anyhow!(error.message))?;
        let revised_prompt = edited
            .images
            .iter()
            .find_map(|image| image.revised_prompt.clone());
        Ok(GeneratedImageBatch {
            provider: self.provider().to_string(),
            model: edited.model,
            images: edited
                .images
                .into_iter()
                .map(|image| GeneratedImagePayload {
                    media_type: image.media_type,
                    bytes: image.bytes,
                })
                .collect(),
            revised_prompt,
        })
    }
}

/// OpenRouter-backed image-generation backend.
pub(crate) struct OpenRouterImageGenerationBackend {
    config: OpenRouterProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

impl OpenRouterImageGenerationBackend {
    /// Builds one OpenRouter-backed image backend.
    pub(crate) fn from_config(
        mut config: OpenRouterProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self> {
        config.model = resolve_openrouter_image_model(&config.model);
        Ok(Self { config, observer })
    }
}

#[async_trait]
impl ImageGenerationBackend for OpenRouterImageGenerationBackend {
    fn provider(&self) -> &str {
        "openrouter"
    }

    async fn generate(
        &self,
        request: &GenerateImageToolRequest,
        model_override: Option<&str>,
    ) -> Result<GeneratedImageBatch> {
        let generated = OpenRouterImageGenerator::new(
            openrouter_config_for_image_model(&self.config, model_override),
            self.observer.clone(),
        )?
        .generate(OpenRouterImageGenerationRequest {
            prompt: request.prompt.clone(),
            count: request.count.unwrap_or(1),
            size: request.size.clone(),
        })
        .await
        .map_err(|error| anyhow!(error.message))?;
        Ok(GeneratedImageBatch {
            provider: self.provider().to_string(),
            model: generated.model,
            images: generated
                .images
                .into_iter()
                .map(|image| GeneratedImagePayload {
                    media_type: image.media_type,
                    bytes: image.bytes,
                })
                .collect(),
            revised_prompt: generated.text,
        })
    }

    fn supports_edit(&self) -> bool {
        true
    }

    async fn edit(
        &self,
        request: &ImageEditBackendRequest,
        model_override: Option<&str>,
    ) -> Result<GeneratedImageBatch> {
        let edited = OpenRouterImageEditor::new(
            openrouter_config_for_image_model(&self.config, model_override),
            self.observer.clone(),
        )?
        .edit(OpenRouterImageEditRequest {
            prompt: request.prompt.clone(),
            images: request
                .images
                .iter()
                .map(|image| OpenRouterImageEditInput {
                    file_name: image.file_name.clone(),
                    media_type: image.media_type.clone(),
                    bytes: image.bytes.clone(),
                })
                .collect(),
            count: request.count.unwrap_or(1),
            size: request.size.clone(),
        })
        .await
        .map_err(|error| anyhow!(error.message))?;
        Ok(GeneratedImageBatch {
            provider: self.provider().to_string(),
            model: edited.model,
            images: edited
                .images
                .into_iter()
                .map(|image| GeneratedImagePayload {
                    media_type: image.media_type,
                    bytes: image.bytes,
                })
                .collect(),
            revised_prompt: edited.text,
        })
    }
}

/// xAI-backed image-generation backend.
pub(crate) struct XAiImageGenerationBackend {
    config: XAiProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

impl XAiImageGenerationBackend {
    /// Builds one xAI-backed image-generation backend.
    pub(crate) fn from_config(
        mut config: XAiProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self> {
        config.model = resolve_xai_image_model(&config.model);
        Ok(Self { config, observer })
    }
}

#[async_trait]
impl ImageGenerationBackend for XAiImageGenerationBackend {
    fn provider(&self) -> &str {
        "xai"
    }

    async fn generate(
        &self,
        request: &GenerateImageToolRequest,
        model_override: Option<&str>,
    ) -> Result<GeneratedImageBatch> {
        let generated = XAiImageGenerator::new(
            xai_config_for_image_model(&self.config, model_override),
            self.observer.clone(),
        )?
        .generate(OpenAiImageGenerationRequest {
            prompt: request.prompt.clone(),
            count: request.count.unwrap_or(1),
            size: request.size.clone(),
        })
        .await
        .map_err(|error| anyhow!(error.message))?;
        let revised_prompt = generated
            .images
            .iter()
            .find_map(|image| image.revised_prompt.clone());
        Ok(GeneratedImageBatch {
            provider: self.provider().to_string(),
            model: generated.model,
            images: generated
                .images
                .into_iter()
                .map(|image| GeneratedImagePayload {
                    media_type: image.media_type,
                    bytes: image.bytes,
                })
                .collect(),
            revised_prompt,
        })
    }

    fn supports_edit(&self) -> bool {
        true
    }

    async fn edit(
        &self,
        request: &ImageEditBackendRequest,
        model_override: Option<&str>,
    ) -> Result<GeneratedImageBatch> {
        let edited = XAiImageEditor::new(
            xai_config_for_image_model(&self.config, model_override),
            self.observer.clone(),
        )?
        .edit(OpenAiImageEditRequest {
            prompt: request.prompt.clone(),
            images: request
                .images
                .iter()
                .map(|image| OpenAiImageEditInput {
                    file_name: image.file_name.clone(),
                    media_type: image.media_type.clone(),
                    bytes: image.bytes.clone(),
                })
                .collect(),
            count: request.count.unwrap_or(1),
            size: request.size.clone(),
        })
        .await
        .map_err(|error| anyhow!(error.message))?;
        let revised_prompt = edited
            .images
            .iter()
            .find_map(|image| image.revised_prompt.clone());
        Ok(GeneratedImageBatch {
            provider: self.provider().to_string(),
            model: edited.model,
            images: edited
                .images
                .into_iter()
                .map(|image| GeneratedImagePayload {
                    media_type: image.media_type,
                    bytes: image.bytes,
                })
                .collect(),
            revised_prompt,
        })
    }
}

fn openai_config_for_image_model(
    config: &OpenAiProviderConfig,
    model_override: Option<&str>,
) -> OpenAiProviderConfig {
    let mut cloned = config.clone();
    cloned.model = resolve_openai_image_model(model_override.unwrap_or(cloned.model.as_str()));
    cloned
}

fn xai_config_for_image_model(
    config: &XAiProviderConfig,
    model_override: Option<&str>,
) -> XAiProviderConfig {
    let mut cloned = config.clone();
    cloned.model = resolve_xai_image_model(model_override.unwrap_or(cloned.model.as_str()));
    cloned
}

fn openrouter_config_for_image_model(
    config: &OpenRouterProviderConfig,
    model_override: Option<&str>,
) -> OpenRouterProviderConfig {
    let mut cloned = config.clone();
    cloned.model = resolve_openrouter_image_model(model_override.unwrap_or(cloned.model.as_str()));
    cloned
}

fn google_config_for_image_model(
    config: &GoogleImageProviderConfig,
    model_override: Option<&str>,
) -> GoogleImageProviderConfig {
    let mut cloned = config.clone();
    cloned.model = resolve_google_image_model(model_override.unwrap_or(cloned.model.as_str()));
    cloned
}

fn preferred_extension_for_media_type(media_type: &str) -> Result<&'static str> {
    match media_type {
        "image/jpeg" => Ok("jpg"),
        "image/png" => Ok("png"),
        other => bail!("unsupported generated image media type {other}"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, anyhow, bail};
    use async_trait::async_trait;
    use image::{ImageBuffer, Rgb};
    use kheish_types::{CredentialScope, parse_asset_storage_uri};

    use super::{
        AdditionalImageBackendConfig, GeneratedImageBatch, GeneratedImagePayload,
        ImageEditBackendRequest, ImageGenerationBackend, ImageGenerationService,
        ImageToolExecutionContext, MAX_EDIT_IMAGE_INPUTS, MAX_IMAGE_OUTPUTS,
        preferred_extension_for_media_type,
    };
    use crate::assets::FileAssetStore;
    use crate::control_tools::{
        EditImageToolRequest, GenerateImageToolRequest, ImageToolRouteOverride,
    };
    use crate::model_routing::ModelRouteConfig;
    use kheish_runtime::OpenAiProviderConfig;

    #[derive(Default)]
    struct FakeGenerationBackend;

    #[async_trait]
    impl ImageGenerationBackend for FakeGenerationBackend {
        fn provider(&self) -> &str {
            "xai"
        }

        async fn generate(
            &self,
            _request: &crate::control_tools::GenerateImageToolRequest,
            _model_override: Option<&str>,
        ) -> Result<GeneratedImageBatch> {
            Ok(GeneratedImageBatch::default())
        }
    }

    struct FakeEditBackend;

    #[async_trait]
    impl ImageGenerationBackend for FakeEditBackend {
        fn provider(&self) -> &str {
            "openai"
        }

        async fn generate(
            &self,
            _request: &crate::control_tools::GenerateImageToolRequest,
            _model_override: Option<&str>,
        ) -> Result<GeneratedImageBatch> {
            Ok(GeneratedImageBatch::default())
        }

        fn supports_edit(&self) -> bool {
            true
        }

        async fn edit(
            &self,
            request: &ImageEditBackendRequest,
            _model_override: Option<&str>,
        ) -> Result<GeneratedImageBatch> {
            assert_eq!(request.images.len(), 1);
            Ok(GeneratedImageBatch {
                provider: "openai".to_string(),
                model: "gpt-image-1.5".to_string(),
                images: vec![GeneratedImagePayload {
                    media_type: "image/png".to_string(),
                    bytes: valid_png_bytes(),
                }],
                revised_prompt: Some(request.prompt.clone()),
            })
        }
    }

    struct InvalidGenerationBackend {
        media_type: &'static str,
        bytes: Vec<u8>,
    }

    #[async_trait]
    impl ImageGenerationBackend for InvalidGenerationBackend {
        fn provider(&self) -> &str {
            "invalid"
        }

        async fn generate(
            &self,
            _request: &GenerateImageToolRequest,
            _model_override: Option<&str>,
        ) -> Result<GeneratedImageBatch> {
            Ok(GeneratedImageBatch {
                provider: "invalid".to_string(),
                model: "invalid-image-model".to_string(),
                images: vec![GeneratedImagePayload {
                    media_type: self.media_type.to_string(),
                    bytes: self.bytes.clone(),
                }],
                revised_prompt: None,
            })
        }
    }

    struct StaticBatchBackend {
        provider: &'static str,
        model: &'static str,
        supports_edit: bool,
        images: Vec<GeneratedImagePayload>,
    }

    #[async_trait]
    impl ImageGenerationBackend for StaticBatchBackend {
        fn provider(&self) -> &str {
            self.provider
        }

        async fn generate(
            &self,
            _request: &GenerateImageToolRequest,
            _model_override: Option<&str>,
        ) -> Result<GeneratedImageBatch> {
            Ok(GeneratedImageBatch {
                provider: self.provider.to_string(),
                model: self.model.to_string(),
                images: self.images.clone(),
                revised_prompt: None,
            })
        }

        fn supports_edit(&self) -> bool {
            self.supports_edit
        }

        async fn edit(
            &self,
            _request: &ImageEditBackendRequest,
            _model_override: Option<&str>,
        ) -> Result<GeneratedImageBatch> {
            Ok(GeneratedImageBatch {
                provider: self.provider.to_string(),
                model: self.model.to_string(),
                images: self.images.clone(),
                revised_prompt: None,
            })
        }
    }

    struct FailingImageBackend {
        provider: &'static str,
        supports_edit: bool,
        generate_calls: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl ImageGenerationBackend for FailingImageBackend {
        fn provider(&self) -> &str {
            self.provider
        }

        async fn generate(
            &self,
            _request: &GenerateImageToolRequest,
            _model_override: Option<&str>,
        ) -> Result<GeneratedImageBatch> {
            *self
                .generate_calls
                .lock()
                .expect("generate calls mutex poisoned") += 1;
            bail!("{} image backend unavailable", self.provider)
        }

        fn supports_edit(&self) -> bool {
            self.supports_edit
        }

        async fn edit(
            &self,
            _request: &ImageEditBackendRequest,
            _model_override: Option<&str>,
        ) -> Result<GeneratedImageBatch> {
            bail!("{} image backend unavailable", self.provider)
        }
    }

    #[derive(Default)]
    struct CapturedBackendCalls {
        generate_model_overrides: Vec<Option<String>>,
        edit_model_overrides: Vec<Option<String>>,
        last_edit_image_count: Option<usize>,
    }

    struct CapturingImageBackend {
        provider: &'static str,
        supports_edit: bool,
        calls: Arc<Mutex<CapturedBackendCalls>>,
    }

    #[async_trait]
    impl ImageGenerationBackend for CapturingImageBackend {
        fn provider(&self) -> &str {
            self.provider
        }

        async fn generate(
            &self,
            _request: &GenerateImageToolRequest,
            model_override: Option<&str>,
        ) -> Result<GeneratedImageBatch> {
            self.calls
                .lock()
                .expect("capture mutex poisoned")
                .generate_model_overrides
                .push(model_override.map(ToOwned::to_owned));
            Ok(GeneratedImageBatch {
                provider: self.provider.to_string(),
                model: model_override.unwrap_or(self.provider).to_string(),
                images: vec![GeneratedImagePayload {
                    media_type: "image/png".to_string(),
                    bytes: valid_png_bytes(),
                }],
                revised_prompt: None,
            })
        }

        fn supports_edit(&self) -> bool {
            self.supports_edit
        }

        async fn edit(
            &self,
            request: &ImageEditBackendRequest,
            model_override: Option<&str>,
        ) -> Result<GeneratedImageBatch> {
            let mut calls = self.calls.lock().expect("capture mutex poisoned");
            calls
                .edit_model_overrides
                .push(model_override.map(ToOwned::to_owned));
            calls.last_edit_image_count = Some(request.images.len());
            drop(calls);
            Ok(GeneratedImageBatch {
                provider: self.provider.to_string(),
                model: model_override.unwrap_or(self.provider).to_string(),
                images: vec![GeneratedImagePayload {
                    media_type: "image/png".to_string(),
                    bytes: valid_png_bytes(),
                }],
                revised_prompt: Some(request.prompt.clone()),
            })
        }
    }

    #[test]
    fn generated_image_extensions_follow_the_returned_media_type() -> Result<()> {
        assert_eq!(preferred_extension_for_media_type("image/png")?, "png");
        assert_eq!(preferred_extension_for_media_type("image/jpeg")?, "jpg");
        Ok(())
    }

    #[test]
    fn additional_image_backend_config_preserves_the_configured_route_id() {
        let named = AdditionalImageBackendConfig::named(
            "openrouter",
            ModelRouteConfig::OpenAi(OpenAiProviderConfig::new("gpt-image-1.5", "test-key")),
        );
        let implicit = AdditionalImageBackendConfig::openai(OpenAiProviderConfig::new(
            "gpt-image-1.5",
            "test-key",
        ));

        assert_eq!(named.route_id(), "openrouter");
        assert_eq!(implicit.route_id(), "openai");
    }

    #[tokio::test]
    async fn image_generation_service_records_generation_provenance_on_each_output() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let assets_for_assert = assets.clone();
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert(
            "openai-images".to_string(),
            Arc::new(StaticBatchBackend {
                provider: "openai",
                model: "gpt-image-1.5",
                supports_edit: true,
                images: vec![
                    GeneratedImagePayload {
                        media_type: "image/png".to_string(),
                        bytes: valid_png_bytes_for_color(Rgb([255, 0, 0])),
                    },
                    GeneratedImagePayload {
                        media_type: "image/png".to_string(),
                        bytes: valid_png_bytes_for_color(Rgb([0, 255, 0])),
                    },
                ],
            }),
        );
        let service = ImageGenerationService::new(backends, "openai-images".to_string(), assets)?;

        let response = service
            .generate_with_context(
                GenerateImageToolRequest {
                    prompt: "Render two architectural studies.".to_string(),
                    count: Some(2),
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                Some("openai-images"),
                None,
                ImageToolExecutionContext {
                    session_id: Some("session-image".to_string()),
                    run_id: Some("run-image".to_string()),
                    tool_call_id: Some("call-image".to_string()),
                },
            )
            .await?;

        assert_eq!(response.route_id.as_deref(), Some("openai-images"));
        assert_eq!(response.assets.len(), 2);
        let expected_prompt_sha256 = super::sha256_hex(b"Render two architectural studies.");
        for (index, attachment) in response.assets.iter().enumerate() {
            let record = assets_for_assert
                .get(&attachment.id)
                .ok_or_else(|| anyhow!("missing generated asset {}", attachment.id))?;
            assert_eq!(record.provenance.len(), 1);
            let provenance = &record.provenance[0];
            assert_eq!(provenance.kind, "image_generation");
            assert_eq!(provenance.tool_name, "generate_image");
            assert_eq!(provenance.session_id.as_deref(), Some("session-image"));
            assert_eq!(provenance.run_id.as_deref(), Some("run-image"));
            assert_eq!(provenance.tool_call_id.as_deref(), Some("call-image"));
            assert_eq!(provenance.route_id.as_deref(), Some("openai-images"));
            assert_eq!(provenance.provider, "openai");
            assert_eq!(provenance.model, "gpt-image-1.5");
            assert_eq!(provenance.prompt_sha256, expected_prompt_sha256);
            assert!(provenance.source_assets.is_empty());
            assert_eq!(provenance.output_index, index as u32 + 1);
            assert_eq!(provenance.output_count, 2);
        }
        Ok(())
    }

    #[tokio::test]
    async fn image_edit_service_records_source_asset_provenance_and_checksums() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let source = assets.import_bytes("plan.png", Some("image/png"), &valid_png_bytes())?;
        let assets_for_assert = assets.clone();
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert(
            "openai-images".to_string(),
            Arc::new(StaticBatchBackend {
                provider: "openai",
                model: "gpt-image-1.5",
                supports_edit: true,
                images: vec![GeneratedImagePayload {
                    media_type: "image/png".to_string(),
                    bytes: different_valid_png_bytes(),
                }],
            }),
        );
        let service = ImageGenerationService::new(backends, "openai-images".to_string(), assets)?;

        let response = service
            .edit_with_context(
                EditImageToolRequest {
                    prompt: "Apply the marked edit.".to_string(),
                    image_asset_ids: vec![source.id.clone()],
                    image_asset_ids_was_omitted: false,
                    count: Some(1),
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                Some("openai-images"),
                None,
                ImageToolExecutionContext {
                    session_id: Some("session-edit".to_string()),
                    run_id: Some("run-edit".to_string()),
                    tool_call_id: Some("call-edit".to_string()),
                },
            )
            .await?;

        let edited_id = &response.assets[0].id;
        let edited = assets_for_assert
            .get(edited_id)
            .ok_or_else(|| anyhow!("missing edited asset {edited_id}"))?;
        assert_eq!(edited.provenance.len(), 1);
        let provenance = &edited.provenance[0];
        assert_eq!(provenance.kind, "image_edit");
        assert_eq!(provenance.tool_name, "edit_image");
        assert_eq!(provenance.session_id.as_deref(), Some("session-edit"));
        assert_eq!(provenance.run_id.as_deref(), Some("run-edit"));
        assert_eq!(provenance.tool_call_id.as_deref(), Some("call-edit"));
        assert_eq!(provenance.route_id.as_deref(), Some("openai-images"));
        assert_eq!(
            provenance.prompt_sha256,
            super::sha256_hex(b"Apply the marked edit.")
        );
        assert_eq!(provenance.output_index, 1);
        assert_eq!(provenance.output_count, 1);
        assert_eq!(provenance.source_assets.len(), 1);
        assert_eq!(provenance.source_assets[0].asset_id, source.id);
        assert_eq!(provenance.source_assets[0].media_type, source.media_type);
        assert_eq!(provenance.source_assets[0].sha256, source.sha256);
        Ok(())
    }

    #[tokio::test]
    async fn image_generation_service_preserves_deduped_asset_provenance_events() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let assets_for_assert = assets.clone();
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert(
            "openai-images".to_string(),
            Arc::new(StaticBatchBackend {
                provider: "openai",
                model: "gpt-image-1.5",
                supports_edit: true,
                images: vec![GeneratedImagePayload {
                    media_type: "image/png".to_string(),
                    bytes: valid_png_bytes(),
                }],
            }),
        );
        let service = ImageGenerationService::new(backends, "openai-images".to_string(), assets)?;

        let first = service
            .generate_with_context(
                GenerateImageToolRequest {
                    prompt: "Render the first copy.".to_string(),
                    count: Some(1),
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                Some("openai-images"),
                None,
                ImageToolExecutionContext {
                    session_id: Some("session-dedup".to_string()),
                    run_id: Some("run-1".to_string()),
                    tool_call_id: Some("call-1".to_string()),
                },
            )
            .await?;
        let second = service
            .generate_with_context(
                GenerateImageToolRequest {
                    prompt: "Render the second copy.".to_string(),
                    count: Some(1),
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                Some("openai-images"),
                None,
                ImageToolExecutionContext {
                    session_id: Some("session-dedup".to_string()),
                    run_id: Some("run-2".to_string()),
                    tool_call_id: Some("call-2".to_string()),
                },
            )
            .await?;

        assert_eq!(first.assets[0].id, second.assets[0].id);
        let record = assets_for_assert
            .get(&first.assets[0].id)
            .ok_or_else(|| anyhow!("missing deduped asset"))?;
        assert_eq!(record.provenance.len(), 2);
        assert_eq!(record.provenance[0].run_id.as_deref(), Some("run-1"));
        assert_eq!(record.provenance[1].run_id.as_deref(), Some("run-2"));
        assert_eq!(
            record.provenance[0].prompt_sha256,
            super::sha256_hex(b"Render the first copy.")
        );
        assert_eq!(
            record.provenance[1].prompt_sha256,
            super::sha256_hex(b"Render the second copy.")
        );
        Ok(())
    }

    #[tokio::test]
    async fn image_generation_service_fails_over_after_preferred_backend_error() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let primary_calls = Arc::new(Mutex::new(0usize));
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert(
            "primary".to_string(),
            Arc::new(FailingImageBackend {
                provider: "primary",
                supports_edit: true,
                generate_calls: primary_calls.clone(),
            }),
        );
        backends.insert(
            "backup".to_string(),
            Arc::new(StaticBatchBackend {
                provider: "backup",
                model: "backup-image-model",
                supports_edit: true,
                images: vec![GeneratedImagePayload {
                    media_type: "image/png".to_string(),
                    bytes: valid_png_bytes(),
                }],
            }),
        );
        let service = ImageGenerationService::new(backends, "primary".to_string(), assets)?;

        let response = service
            .generate(
                GenerateImageToolRequest {
                    prompt: "Render with fallback.".to_string(),
                    count: Some(1),
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                Some("primary"),
                None,
            )
            .await?;

        assert_eq!(
            *primary_calls.lock().expect("primary calls mutex poisoned"),
            1
        );
        assert_eq!(response.route_id.as_deref(), Some("backup"));
        assert_eq!(response.provider, "backup");
        assert_eq!(response.model, "backup-image-model");
        Ok(())
    }

    #[tokio::test]
    async fn image_generation_service_does_not_fallback_for_explicit_route_override() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert(
            "primary".to_string(),
            Arc::new(FailingImageBackend {
                provider: "primary",
                supports_edit: true,
                generate_calls: Arc::new(Mutex::new(0)),
            }),
        );
        backends.insert(
            "backup".to_string(),
            Arc::new(StaticBatchBackend {
                provider: "backup",
                model: "backup-image-model",
                supports_edit: true,
                images: vec![GeneratedImagePayload {
                    media_type: "image/png".to_string(),
                    bytes: valid_png_bytes(),
                }],
            }),
        );
        let service = ImageGenerationService::new(backends, "primary".to_string(), assets)?;

        let error = service
            .generate(
                GenerateImageToolRequest {
                    prompt: "Render only on primary.".to_string(),
                    count: Some(1),
                    size: None,
                    route: ImageToolRouteOverride {
                        provider: Some("primary".to_string()),
                        model: None,
                    },
                },
                Some("primary"),
                None,
            )
            .await
            .expect_err("explicit route override should not fail over");

        assert!(
            error
                .to_string()
                .contains("all image-generation routes failed")
                && error
                    .to_string()
                    .contains("primary image backend unavailable"),
            "unexpected error: {error:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn image_generation_service_rejects_invalid_multi_output_without_persisting() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let assets_for_assert = assets.clone();
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert(
            "openai-images".to_string(),
            Arc::new(StaticBatchBackend {
                provider: "openai",
                model: "gpt-image-1.5",
                supports_edit: true,
                images: vec![
                    GeneratedImagePayload {
                        media_type: "image/png".to_string(),
                        bytes: valid_png_bytes(),
                    },
                    GeneratedImagePayload {
                        media_type: "image/png".to_string(),
                        bytes: b"not an image".to_vec(),
                    },
                ],
            }),
        );
        let service = ImageGenerationService::new(backends, "openai-images".to_string(), assets)?;

        let error = service
            .generate(
                GenerateImageToolRequest {
                    prompt: "Render a partial bad batch.".to_string(),
                    count: Some(2),
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                Some("openai-images"),
                None,
            )
            .await
            .expect_err("bad second image should reject the whole batch");

        assert!(
            error
                .to_string()
                .contains("all image-generation routes failed"),
            "unexpected error: {error:#}"
        );
        assert!(
            assets_for_assert.list(None).is_empty(),
            "invalid multi-output batches must leave no persisted asset behind"
        );
        Ok(())
    }

    #[tokio::test]
    async fn image_edit_service_falls_back_to_an_edit_capable_backend() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let source = assets.import_bytes("plan.png", Some("image/png"), &valid_png_bytes())?;

        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert("xai-route".to_string(), Arc::new(FakeGenerationBackend));
        backends.insert("openai-images".to_string(), Arc::new(FakeEditBackend));
        let service = ImageGenerationService::new(backends, "xai-route".to_string(), assets)?;

        let response = service
            .edit(
                EditImageToolRequest {
                    prompt: "Turn the square red.".to_string(),
                    image_asset_ids: vec![source.id],
                    image_asset_ids_was_omitted: false,
                    count: Some(1),
                    size: Some("1024x1024".to_string()),
                    route: ImageToolRouteOverride::default(),
                },
                Some("xai-route"),
                None,
            )
            .await?;

        assert_eq!(response.provider, "openai");
        assert_eq!(response.assets.len(), 1);
        assert_eq!(response.assets[0].media_type, "image/png");
        Ok(())
    }

    #[tokio::test]
    async fn image_edit_service_rejects_non_image_assets() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let source = assets.import_bytes("notes.txt", Some("text/plain"), b"hello")?;
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert("openai-images".to_string(), Arc::new(FakeEditBackend));
        let service = ImageGenerationService::new(backends, "openai-images".to_string(), assets)?;

        let error = service
            .edit(
                EditImageToolRequest {
                    prompt: "Turn this into a sketch.".to_string(),
                    image_asset_ids: vec![source.id],
                    image_asset_ids_was_omitted: false,
                    count: None,
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                Some("openai-images"),
                None,
            )
            .await
            .expect_err("non-image assets should be rejected");

        assert!(
            error
                .to_string()
                .contains("edit_image only accepts image assets"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn image_edit_service_rejects_too_many_source_images() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let mut image_asset_ids = Vec::new();
        for index in 0..=MAX_EDIT_IMAGE_INPUTS {
            let record = assets.import_bytes(
                &format!("plan-{index}.png"),
                Some("image/png"),
                &valid_png_bytes(),
            )?;
            image_asset_ids.push(record.id);
        }
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert("openai-images".to_string(), Arc::new(FakeEditBackend));
        let service = ImageGenerationService::new(backends, "openai-images".to_string(), assets)?;

        let error = service
            .edit(
                EditImageToolRequest {
                    prompt: "Combine these plans.".to_string(),
                    image_asset_ids,
                    image_asset_ids_was_omitted: false,
                    count: None,
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                Some("openai-images"),
                None,
            )
            .await
            .expect_err("too many source images should be rejected");

        assert!(
            error.to_string().contains("accepts at most"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn image_generation_service_honors_route_provider_and_model_overrides() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let openai_calls = Arc::new(Mutex::new(CapturedBackendCalls::default()));
        let google_calls = Arc::new(Mutex::new(CapturedBackendCalls::default()));
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert(
            "openai-route".to_string(),
            Arc::new(CapturingImageBackend {
                provider: "openai",
                supports_edit: true,
                calls: openai_calls.clone(),
            }),
        );
        backends.insert(
            "google-images".to_string(),
            Arc::new(CapturingImageBackend {
                provider: "google",
                supports_edit: true,
                calls: google_calls.clone(),
            }),
        );
        let service = ImageGenerationService::new(backends, "openai-route".to_string(), assets)?;

        let response = service
            .generate(
                GenerateImageToolRequest {
                    prompt: "Render the plan.".to_string(),
                    count: Some(1),
                    size: Some("1024x1024".to_string()),
                    route: ImageToolRouteOverride {
                        provider: Some("google-images".to_string()),
                        model: Some("gemini-3-pro-image-preview".to_string()),
                    },
                },
                Some("openai-route"),
                None,
            )
            .await?;

        assert_eq!(response.provider, "google");
        assert_eq!(response.model, "gemini-3-pro-image-preview");
        assert!(
            openai_calls
                .lock()
                .expect("capture mutex poisoned")
                .generate_model_overrides
                .is_empty()
        );
        assert_eq!(
            google_calls
                .lock()
                .expect("capture mutex poisoned")
                .generate_model_overrides,
            vec![Some("gemini-3-pro-image-preview".to_string())]
        );
        Ok(())
    }

    #[tokio::test]
    async fn image_generation_service_validates_prompt_count_and_size_before_dispatch() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let calls = Arc::new(Mutex::new(CapturedBackendCalls::default()));
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert(
            "openai-route".to_string(),
            Arc::new(CapturingImageBackend {
                provider: "openai",
                supports_edit: true,
                calls: calls.clone(),
            }),
        );
        let service = ImageGenerationService::new(backends, "openai-route".to_string(), assets)?;

        for (request, expected) in [
            (
                GenerateImageToolRequest {
                    prompt: "   ".to_string(),
                    count: Some(1),
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                "prompt must not be empty",
            ),
            (
                GenerateImageToolRequest {
                    prompt: "Render.".to_string(),
                    count: Some(0),
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                "count must be at least 1",
            ),
            (
                GenerateImageToolRequest {
                    prompt: "Render.".to_string(),
                    count: Some(MAX_IMAGE_OUTPUTS + 1),
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                "count must be at most",
            ),
            (
                GenerateImageToolRequest {
                    prompt: "Render.".to_string(),
                    count: Some(1),
                    size: Some("99999x1024".to_string()),
                    route: ImageToolRouteOverride::default(),
                },
                "image dimensions exceed",
            ),
        ] {
            let error = service
                .generate(request, Some("openai-route"), None)
                .await
                .expect_err("invalid image request should be rejected");
            assert!(
                error.to_string().contains(expected),
                "expected {expected}, got {error}"
            );
        }

        assert!(
            calls
                .lock()
                .expect("capture mutex poisoned")
                .generate_model_overrides
                .is_empty(),
            "invalid requests should be rejected before backend dispatch"
        );
        Ok(())
    }

    #[tokio::test]
    async fn image_generation_service_rejects_invalid_provider_image_payloads() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let assets_for_assert = assets.clone();
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert(
            "invalid-route".to_string(),
            Arc::new(InvalidGenerationBackend {
                media_type: "image/png",
                bytes: b"not a png".to_vec(),
            }),
        );
        let service = ImageGenerationService::new(backends, "invalid-route".to_string(), assets)?;

        let error = service
            .generate(
                GenerateImageToolRequest {
                    prompt: "Render.".to_string(),
                    count: Some(1),
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                Some("invalid-route"),
                None,
            )
            .await
            .expect_err("invalid provider image should be rejected");
        assert!(
            error
                .to_string()
                .contains("invalid generated image payload"),
            "unexpected error: {error}"
        );
        assert!(
            assets_for_assert.list(None).is_empty(),
            "invalid provider images must not be persisted as assets"
        );
        Ok(())
    }

    #[tokio::test]
    async fn image_generation_service_rejects_oversized_provider_image_before_decode() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let assets_for_assert = assets.clone();
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert(
            "oversized-route".to_string(),
            Arc::new(InvalidGenerationBackend {
                media_type: "image/png",
                bytes: png_header_with_dimensions(super::MAX_IMAGE_REQUEST_EDGE_PX + 1, 1),
            }),
        );
        let service = ImageGenerationService::new(backends, "oversized-route".to_string(), assets)?;

        let error = service
            .generate(
                GenerateImageToolRequest {
                    prompt: "Render.".to_string(),
                    count: Some(1),
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                Some("oversized-route"),
                None,
            )
            .await
            .expect_err("oversized provider image should be rejected before decode");
        let error_text = format!("{error:#}");
        assert!(
            error_text.contains("edge limit"),
            "unexpected error: {error_text}"
        );
        assert!(
            assets_for_assert.list(None).is_empty(),
            "oversized provider images must not be persisted as assets"
        );
        Ok(())
    }

    #[tokio::test]
    async fn image_edit_service_honors_route_provider_and_model_overrides() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let source = assets.import_bytes("plan.png", Some("image/png"), &valid_png_bytes())?;
        let reference =
            assets.import_bytes("reference.png", Some("image/png"), &valid_png_bytes())?;
        let openai_calls = Arc::new(Mutex::new(CapturedBackendCalls::default()));
        let google_calls = Arc::new(Mutex::new(CapturedBackendCalls::default()));
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert(
            "openai-route".to_string(),
            Arc::new(CapturingImageBackend {
                provider: "openai",
                supports_edit: true,
                calls: openai_calls.clone(),
            }),
        );
        backends.insert(
            "google-images".to_string(),
            Arc::new(CapturingImageBackend {
                provider: "google",
                supports_edit: true,
                calls: google_calls.clone(),
            }),
        );
        let service = ImageGenerationService::new(backends, "openai-route".to_string(), assets)?;

        let response = service
            .edit(
                EditImageToolRequest {
                    prompt: "Apply the reviewer corrections.".to_string(),
                    image_asset_ids: vec![source.id, reference.id],
                    image_asset_ids_was_omitted: false,
                    count: Some(1),
                    size: Some("1024x1024".to_string()),
                    route: ImageToolRouteOverride {
                        provider: Some("google-images".to_string()),
                        model: Some("gemini-3-pro-image-preview".to_string()),
                    },
                },
                Some("openai-route"),
                None,
            )
            .await?;

        assert_eq!(response.provider, "google");
        assert_eq!(response.model, "gemini-3-pro-image-preview");
        assert!(
            openai_calls
                .lock()
                .expect("capture mutex poisoned")
                .edit_model_overrides
                .is_empty()
        );
        let google_calls = google_calls.lock().expect("capture mutex poisoned");
        assert_eq!(
            google_calls.edit_model_overrides,
            vec![Some("gemini-3-pro-image-preview".to_string())]
        );
        assert_eq!(google_calls.last_edit_image_count, Some(2));
        Ok(())
    }

    #[tokio::test]
    async fn image_edit_service_verifies_source_image_checksum_before_dispatch() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let source = assets.import_bytes("plan.png", Some("image/png"), &valid_png_bytes())?;
        let (kind, relative_path) =
            parse_asset_storage_uri(&source.uri).expect("raw asset uri should parse");
        let raw_path = temp.path().join("assets").join(kind).join(relative_path);
        fs::write(&raw_path, different_valid_png_bytes())?;

        let calls = Arc::new(Mutex::new(CapturedBackendCalls::default()));
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert(
            "openai-route".to_string(),
            Arc::new(CapturingImageBackend {
                provider: "openai",
                supports_edit: true,
                calls: calls.clone(),
            }),
        );
        let service = ImageGenerationService::new(backends, "openai-route".to_string(), assets)?;

        let error = service
            .edit(
                EditImageToolRequest {
                    prompt: "Apply the reviewer corrections.".to_string(),
                    image_asset_ids: vec![source.id],
                    image_asset_ids_was_omitted: false,
                    count: Some(1),
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                Some("openai-route"),
                None,
            )
            .await
            .expect_err("tampered source image should be rejected");

        assert!(
            error.to_string().contains("raw payload checksum mismatch")
                || error.to_string().contains("asset integrity mismatch"),
            "unexpected error: {error}"
        );
        assert!(
            calls
                .lock()
                .expect("capture mutex poisoned")
                .edit_model_overrides
                .is_empty(),
            "tampered source image should be rejected before backend dispatch"
        );
        Ok(())
    }

    #[tokio::test]
    async fn image_generation_service_prefers_named_routes_on_shared_provider_drivers() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let default_calls = Arc::new(Mutex::new(CapturedBackendCalls::default()));
        let preferred_calls = Arc::new(Mutex::new(CapturedBackendCalls::default()));
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert(
            "openai".to_string(),
            Arc::new(CapturingImageBackend {
                provider: "openai",
                supports_edit: true,
                calls: default_calls.clone(),
            }),
        );
        backends.insert(
            "openrouter".to_string(),
            Arc::new(CapturingImageBackend {
                provider: "openai",
                supports_edit: true,
                calls: preferred_calls.clone(),
            }),
        );
        let service = ImageGenerationService::new(backends, "openai".to_string(), assets)?;

        let response = service
            .generate(
                GenerateImageToolRequest {
                    prompt: "Render the plan.".to_string(),
                    count: Some(1),
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                Some("openrouter"),
                None,
            )
            .await?;

        assert_eq!(response.provider, "openai");
        assert_eq!(response.model, "openai");
        assert!(
            default_calls
                .lock()
                .expect("capture mutex poisoned")
                .generate_model_overrides
                .is_empty()
        );
        assert_eq!(
            preferred_calls
                .lock()
                .expect("capture mutex poisoned")
                .generate_model_overrides,
            vec![None]
        );
        Ok(())
    }

    #[tokio::test]
    async fn image_generation_service_rejects_routes_blocked_by_credential_scope() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert("openai-route".to_string(), Arc::new(FakeGenerationBackend));
        let service = ImageGenerationService::new(backends, "openai-route".to_string(), assets)?;
        let scope = CredentialScope {
            route_deny: vec!["openai-route".to_string()],
            ..CredentialScope::default()
        };

        let error = service
            .generate(
                GenerateImageToolRequest {
                    prompt: "Render the blocked plan.".to_string(),
                    count: Some(1),
                    size: None,
                    route: ImageToolRouteOverride::default(),
                },
                Some("openai-route"),
                Some(&scope),
            )
            .await
            .expect_err("blocked image route should fail");

        assert!(
            error
                .to_string()
                .contains("credential scope blocks all image-generation routes"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn image_edit_service_rejects_explicit_routes_blocked_by_credential_scope() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let source = assets.import_bytes("plan.png", Some("image/png"), &valid_png_bytes())?;
        let mut backends: BTreeMap<String, Arc<dyn ImageGenerationBackend>> = BTreeMap::new();
        backends.insert("openai-images".to_string(), Arc::new(FakeEditBackend));
        let service = ImageGenerationService::new(backends, "openai-images".to_string(), assets)?;
        let scope = CredentialScope {
            route_deny: vec!["openai-images".to_string()],
            ..CredentialScope::default()
        };

        let error = service
            .edit(
                EditImageToolRequest {
                    prompt: "Apply the blocked edit.".to_string(),
                    image_asset_ids: vec![source.id],
                    image_asset_ids_was_omitted: false,
                    count: Some(1),
                    size: None,
                    route: ImageToolRouteOverride {
                        provider: Some("openai-images".to_string()),
                        model: None,
                    },
                },
                None,
                Some(&scope),
            )
            .await
            .expect_err("blocked explicit edit route should fail");

        assert!(
            error
                .to_string()
                .contains("credential scope blocks route openai-images"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    fn valid_png_bytes() -> Vec<u8> {
        valid_png_bytes_for_color(Rgb([255, 0, 0]))
    }

    fn different_valid_png_bytes() -> Vec<u8> {
        valid_png_bytes_for_color(Rgb([0, 255, 0]))
    }

    fn valid_png_bytes_for_color(color: Rgb<u8>) -> Vec<u8> {
        let image = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_pixel(8, 8, color);
        let mut cursor = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut cursor, image::ImageFormat::Png)
            .expect("fixture PNG should encode");
        cursor.into_inner()
    }

    fn png_header_with_dimensions(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 2, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes
    }
}
