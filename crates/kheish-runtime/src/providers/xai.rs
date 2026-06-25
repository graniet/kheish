use std::fmt::{Debug, Formatter};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use async_trait::async_trait;
use kheish_auth::RequestAuthProvider;

use crate::model::{ModelEventSink, ModelProvider, ModelRuntimeRequest, ProviderError};
use crate::observability::RuntimeObserver;

use super::openai::{
    OpenAiImageEditRequest, OpenAiImageEditor, OpenAiImageGenerationRequest,
    OpenAiImageGenerationResponse, OpenAiImageGenerator, OpenAiProvider, OpenAiProviderConfig,
    ResponsesProviderFlavor,
};

const DEFAULT_XAI_MODEL: &str = "grok-4-fast-reasoning";

/// Configuration for the xAI Responses provider adapter.
#[derive(Clone)]
pub struct XAiProviderConfig {
    inner: OpenAiProviderConfig,
}

impl XAiProviderConfig {
    /// Creates a configuration that targets the standard xAI Responses endpoint.
    pub fn new(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        let model = model.into();
        let mut inner =
            OpenAiProviderConfig::new(model, api_key).with_flavor(ResponsesProviderFlavor::XAi);
        inner.organization = None;
        inner.project = None;
        Self { inner }
    }

    /// Loads the API key from an environment variable.
    pub fn from_env(
        model: impl Into<String>,
        env_var: impl AsRef<str>,
    ) -> Result<Self, ProviderError> {
        let env_var = env_var.as_ref();
        let api_key = std::env::var(env_var).map_err(|_| ProviderError {
            message: format!("missing xAI API key in environment variable {env_var}"),
            retryable: false,
            retry_after_ms: None,
        })?;
        Ok(Self::new(model, api_key))
    }

    /// Creates one xAI configuration backed by a dynamic auth provider.
    pub fn with_request_auth_provider(
        model: impl Into<String>,
        request_auth_provider: Arc<dyn RequestAuthProvider>,
    ) -> Self {
        let mut inner =
            OpenAiProviderConfig::with_request_auth_provider(model, request_auth_provider)
                .with_flavor(ResponsesProviderFlavor::XAi);
        inner.organization = None;
        inner.project = None;
        Self { inner }
    }
}

impl Debug for XAiProviderConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XAiProviderConfig")
            .field("model", &self.inner.model)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.inner.base_url)
            .field(
                "default_max_output_tokens",
                &self.inner.default_max_output_tokens,
            )
            .field("pricing", &self.inner.pricing)
            .field("asset_root", &self.inner.asset_root)
            .finish()
    }
}

impl Deref for XAiProviderConfig {
    type Target = OpenAiProviderConfig;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for XAiProviderConfig {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl From<XAiProviderConfig> for OpenAiProviderConfig {
    fn from(value: XAiProviderConfig) -> Self {
        value.inner
    }
}

/// xAI Responses streaming provider.
pub struct XAiProvider {
    inner: OpenAiProvider,
}

impl XAiProvider {
    /// Builds a new xAI provider using a dedicated HTTP client.
    pub fn new(config: XAiProviderConfig) -> Result<Self, ProviderError> {
        Self::with_observer(config, Arc::new(crate::NoopObserver))
    }

    /// Builds a new xAI provider with runtime observation hooks enabled.
    pub fn with_observer(
        config: XAiProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            inner: OpenAiProvider::with_observer(config.into(), observer)?,
        })
    }
}

#[async_trait]
impl ModelProvider for XAiProvider {
    async fn stream(
        &self,
        request: ModelRuntimeRequest,
        sink: ModelEventSink,
    ) -> std::result::Result<(), ProviderError> {
        self.inner.stream(request, sink).await
    }
}

/// xAI-backed image generator that shares auth behavior with the text provider.
pub struct XAiImageGenerator {
    inner: OpenAiImageGenerator,
}

impl XAiImageGenerator {
    /// Creates a new xAI image generator.
    pub fn new(
        config: XAiProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            inner: OpenAiImageGenerator::new(config.into(), observer)?,
        })
    }

    /// Generates one or more images and returns normalized binary payloads.
    pub async fn generate(
        &self,
        request: OpenAiImageGenerationRequest,
    ) -> Result<OpenAiImageGenerationResponse, ProviderError> {
        self.inner.generate(request).await
    }
}

/// xAI-backed image editor that uses xAI's JSON image-editing API.
pub struct XAiImageEditor {
    inner: OpenAiImageEditor,
}

impl XAiImageEditor {
    /// Creates a new xAI image editor.
    pub fn new(
        config: XAiProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            inner: OpenAiImageEditor::new(config.into(), observer)?,
        })
    }

    /// Edits one or more images and returns normalized binary payloads.
    pub async fn edit(
        &self,
        request: OpenAiImageEditRequest,
    ) -> Result<OpenAiImageGenerationResponse, ProviderError> {
        self.inner.edit(request).await
    }
}

/// Normalizes one xAI text model name to a default route when the configured model is not xAI-native.
pub fn resolve_xai_model(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return DEFAULT_XAI_MODEL.to_string();
    }
    trimmed.to_string()
}

/// Normalizes one xAI image model name to an image-capable route.
pub fn resolve_xai_image_model(model: &str) -> String {
    let normalized = model.trim().to_ascii_lowercase();
    if normalized.contains("grok-imagine") {
        return model.to_string();
    }
    "grok-imagine-image".to_string()
}
