//! Daemon-owned audio transcription backed by provider-specific runtimes.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use kheish_runtime::{
    AudioTranscriptionRequest, AudioTranscriptionResponse, OpenAiAudioTranscriber,
    OpenAiProviderConfig, OpenRouterAudioTranscriber, OpenRouterModelCapabilities,
    OpenRouterProviderConfig, RuntimeObserver, resolve_openai_transcription_model,
    resolve_openai_transcription_request_model, resolve_openrouter_transcription_model,
};

use crate::assets::{StoredAssetRecord, validate_supported_audio_payload};
use crate::derivations::NormalizedDerivationTranscriptionOptions;
use crate::model_routing::ModelRouteConfig;

const MAX_AUDIO_TRANSCRIPTION_BYTES: usize = 25 * 1024 * 1024;
const MAX_TRANSCRIPTION_PROMPT_CHARS: usize = 2_000;
const MAX_TRANSCRIPTION_LANGUAGE_CHARS: usize = 32;
pub(crate) const TRANSCRIPTION_PIPELINE_VERSION: u32 = 1;
pub(crate) const TRANSCRIPTION_CACHE_VERSION: &str = "v1";
pub(crate) const TRANSCRIPTION_STITCHING_STRATEGY_SINGLE_PART: &str = "single_part";
pub(crate) const TRANSCRIPTION_SINGLE_PART_COUNT: u32 = 1;

/// One provider-neutral audio transcription request after daemon asset resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AudioTranscriptionBackendRequest {
    /// The original file name used for the transcription upload.
    pub file_name: String,
    /// The normalized MIME type of the source asset.
    pub media_type: String,
    /// The raw audio bytes that should be transcribed.
    pub bytes: Vec<u8>,
    /// Optional contextual prompt supplied to the backend.
    pub prompt: Option<String>,
    /// Optional language hint supplied to the backend.
    pub language: Option<String>,
    /// Optional timestamp granularities requested from the backend.
    pub timestamp_granularities: Vec<String>,
    /// Requests speaker diarization when supported by the selected backend.
    pub diarization: bool,
}

/// One transcription response together with the daemon route that produced it.
#[derive(Debug)]
pub(crate) struct AudioTranscriptionResult {
    pub response: AudioTranscriptionResponse,
    pub route_id: String,
    pub pipeline_version: u32,
    pub stitching_strategy: String,
    pub part_count: u32,
}

/// Provider/model identity planned before one transcription request is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AudioTranscriptionBackendIdentity {
    pub provider: String,
    pub model: String,
}

/// Provider-neutral audio transcription backend contract.
#[async_trait]
pub(crate) trait AudioTranscriptionBackend: Send + Sync {
    /// Returns the provider/model identity that would be used for a request.
    fn identity(&self, model_override: Option<&str>) -> AudioTranscriptionBackendIdentity;

    /// Transcribes one bounded audio payload into plain text.
    async fn transcribe(
        &self,
        request: &AudioTranscriptionBackendRequest,
        model_override: Option<&str>,
    ) -> Result<AudioTranscriptionResponse>;
}

/// One daemon-owned transcription service that resolves provider-neutral backends.
pub(crate) struct TranscriptionService {
    backends: BTreeMap<String, Arc<dyn AudioTranscriptionBackend>>,
    default_route_id: String,
}

/// One additional daemon-owned transcription backend configured outside the text model route inventory.
#[derive(Clone)]
pub struct AdditionalTranscriptionBackendConfig {
    route_id: String,
    route: ModelRouteConfig,
}

impl AdditionalTranscriptionBackendConfig {
    /// Builds one additional transcription backend whose route identifier matches the provider family.
    pub fn route(route: ModelRouteConfig) -> Self {
        Self::named(route.provider_name(), route)
    }

    /// Builds one additional transcription backend from a fully resolved daemon route config.
    pub fn named(route_id: impl Into<String>, route: ModelRouteConfig) -> Self {
        Self {
            route_id: route_id.into(),
            route,
        }
    }

    /// Builds one additional OpenAI transcription backend.
    pub fn openai(config: OpenAiProviderConfig) -> Self {
        Self::route(ModelRouteConfig::OpenAi(config))
    }

    /// Builds one additional OpenRouter transcription backend.
    pub fn openrouter(config: OpenRouterProviderConfig) -> Self {
        Self::route(ModelRouteConfig::OpenRouter(config))
    }

    /// Returns the stable route identifier used by this additional transcription backend.
    pub fn route_id(&self) -> &str {
        &self.route_id
    }

    /// Returns the resolved provider route used by this additional transcription backend.
    pub fn route_config(&self) -> &ModelRouteConfig {
        &self.route
    }
}

/// OpenAI-backed audio transcription backend.
pub(crate) struct OpenAiAudioTranscriptionBackend {
    config: OpenAiProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

impl OpenAiAudioTranscriptionBackend {
    /// Builds one OpenAI-backed audio transcription backend.
    pub(crate) fn from_config(
        mut config: OpenAiProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Self {
        config.model = resolve_openai_transcription_model(&config.model);
        Self { config, observer }
    }
}

#[async_trait]
impl AudioTranscriptionBackend for OpenAiAudioTranscriptionBackend {
    fn identity(&self, model_override: Option<&str>) -> AudioTranscriptionBackendIdentity {
        let model = model_override
            .map(resolve_openai_transcription_model)
            .unwrap_or_else(|| self.config.model.clone());
        AudioTranscriptionBackendIdentity {
            provider: "openai".to_string(),
            model,
        }
    }

    async fn transcribe(
        &self,
        request: &AudioTranscriptionBackendRequest,
        model_override: Option<&str>,
    ) -> Result<AudioTranscriptionResponse> {
        let mut config = self.config.clone();
        if let Some(model) = model_override {
            config.model = resolve_openai_transcription_model(model);
        }
        OpenAiAudioTranscriber::with_observer(config, self.observer.clone())?
            .transcribe(&AudioTranscriptionRequest {
                file_name: request.file_name.clone(),
                media_type: request.media_type.clone(),
                bytes: request.bytes.clone(),
                prompt: request.prompt.clone(),
                language: request.language.clone(),
                timestamp_granularities: request.timestamp_granularities.clone(),
                diarization: request.diarization,
            })
            .await
            .map_err(Into::into)
    }
}

/// OpenRouter-backed audio transcription backend.
pub(crate) struct OpenRouterAudioTranscriptionBackend {
    config: OpenRouterProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

impl OpenRouterAudioTranscriptionBackend {
    /// Builds one OpenRouter-backed audio transcription backend.
    pub(crate) fn from_config(
        mut config: OpenRouterProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Self {
        let route_model = config.model.clone();
        config.model = if openrouter_route_model_supports_transcription(&config, &route_model) {
            resolve_openrouter_transcription_model(&route_model)
        } else {
            resolve_openrouter_transcription_model("")
        };
        Self { config, observer }
    }
}

fn openrouter_route_model_supports_transcription(
    config: &OpenRouterProviderConfig,
    model: &str,
) -> bool {
    if config
        .capability_for_model(model)
        .is_some_and(OpenRouterModelCapabilities::transcription)
    {
        return true;
    }
    let model = model.trim().to_ascii_lowercase();
    model.contains("transcribe") || model.contains("whisper")
}

#[async_trait]
impl AudioTranscriptionBackend for OpenRouterAudioTranscriptionBackend {
    fn identity(&self, model_override: Option<&str>) -> AudioTranscriptionBackendIdentity {
        let model = model_override
            .map(resolve_openrouter_transcription_model)
            .unwrap_or_else(|| self.config.model.clone());
        AudioTranscriptionBackendIdentity {
            provider: "openrouter".to_string(),
            model,
        }
    }

    async fn transcribe(
        &self,
        request: &AudioTranscriptionBackendRequest,
        model_override: Option<&str>,
    ) -> Result<AudioTranscriptionResponse> {
        let mut config = self.config.clone();
        if let Some(model) = model_override {
            config.model = resolve_openrouter_transcription_model(model);
        }
        OpenRouterAudioTranscriber::with_observer(config, self.observer.clone())?
            .transcribe(&AudioTranscriptionRequest {
                file_name: request.file_name.clone(),
                media_type: request.media_type.clone(),
                bytes: request.bytes.clone(),
                prompt: request.prompt.clone(),
                language: request.language.clone(),
                timestamp_granularities: request.timestamp_granularities.clone(),
                diarization: request.diarization,
            })
            .await
            .map_err(Into::into)
    }
}

impl TranscriptionService {
    /// Builds one transcription service from provider-neutral backends.
    pub(crate) fn new(
        backends: BTreeMap<String, Arc<dyn AudioTranscriptionBackend>>,
        default_route_id: String,
    ) -> Result<Self> {
        anyhow::ensure!(
            backends.contains_key(&default_route_id),
            "default transcription route {default_route_id} is not configured"
        );
        Ok(Self {
            backends,
            default_route_id,
        })
    }

    /// Returns whether the service supports the provided media type.
    #[must_use]
    pub(crate) fn supports_media_type(&self, media_type: &str) -> bool {
        transcription_audio_media_type(media_type).is_some()
    }

    /// Transcribes one daemon-owned audio asset and returns the selected daemon route id.
    pub(crate) async fn transcribe_asset_with_route(
        &self,
        asset: &StoredAssetRecord,
        bytes: Vec<u8>,
        preferred_route_id: Option<&str>,
        credential_scope: Option<&kheish_types::CredentialScope>,
        options: Option<&NormalizedDerivationTranscriptionOptions>,
    ) -> Result<AudioTranscriptionResult> {
        anyhow::ensure!(
            self.supports_media_type(&asset.media_type),
            "asset {} media type {} is not supported for audio transcription",
            asset.id,
            asset.media_type
        );
        anyhow::ensure!(
            bytes.len() <= MAX_AUDIO_TRANSCRIPTION_BYTES,
            "asset {} is too large for audio transcription: {} bytes exceeds {} bytes",
            asset.id,
            bytes.len(),
            MAX_AUDIO_TRANSCRIPTION_BYTES
        );
        let validation_media_type =
            transcription_audio_media_type(&asset.media_type).ok_or_else(|| {
                anyhow::anyhow!(
                    "asset {} media type {} is not supported for audio transcription",
                    asset.id,
                    asset.media_type
                )
            })?;
        let audio_kind = match validation_media_type {
            "audio/webm" => "WebM",
            "audio/wav" => "WAV",
            "audio/mpeg" => "MP3",
            "audio/mp4" | "audio/m4a" => "MP4/M4A",
            _ => "audio",
        };
        validate_supported_audio_payload(validation_media_type, &bytes).map_err(|error| {
            anyhow::anyhow!("audio transcription {audio_kind} preflight failed: {error}")
        })?;
        let selected_route_id = select_transcription_route_id(
            &self.backends,
            preferred_route_id,
            &self.default_route_id,
            credential_scope,
        )?;
        let backend = self.backends.get(selected_route_id).ok_or_else(|| {
            anyhow::anyhow!("missing transcription backend for route {selected_route_id}")
        })?;
        let (prompt, language) = match options {
            Some(options) => (
                options.prompt().map(ToString::to_string),
                options.language().map(ToString::to_string),
            ),
            None => validate_transcription_hints(None, None)?,
        };
        let timestamp_granularities = options
            .map(|options| options.timestamp_granularities().to_vec())
            .unwrap_or_default();
        let diarization = options.is_some_and(|options| options.diarization());
        let selected_identity = backend.identity(None);
        if diarization {
            anyhow::ensure!(
                selected_identity.provider == "openai",
                "transcription speaker diarization requires an OpenAI transcription backend"
            );
        }
        if !timestamp_granularities.is_empty() {
            anyhow::ensure!(
                selected_identity.provider == "openai" && selected_identity.model == "whisper-1",
                "transcription timestamp granularities require OpenAI transcription model `whisper-1`"
            );
        }
        let response = backend
            .transcribe(
                &AudioTranscriptionBackendRequest {
                    file_name: asset.file_name.clone(),
                    media_type: asset.media_type.clone(),
                    bytes,
                    prompt,
                    language,
                    timestamp_granularities,
                    diarization,
                },
                None,
            )
            .await?;
        Ok(AudioTranscriptionResult {
            response,
            route_id: selected_route_id.to_string(),
            pipeline_version: TRANSCRIPTION_PIPELINE_VERSION,
            stitching_strategy: TRANSCRIPTION_STITCHING_STRATEGY_SINGLE_PART.to_string(),
            part_count: TRANSCRIPTION_SINGLE_PART_COUNT,
        })
    }

    /// Returns the selected route/backend identity that would serve one transcription.
    pub(crate) fn planned_backend(
        &self,
        preferred_route_id: Option<&str>,
        credential_scope: Option<&kheish_types::CredentialScope>,
    ) -> Result<(String, AudioTranscriptionBackendIdentity)> {
        let selected_route_id = select_transcription_route_id(
            &self.backends,
            preferred_route_id,
            &self.default_route_id,
            credential_scope,
        )?;
        let backend = self.backends.get(selected_route_id).ok_or_else(|| {
            anyhow::anyhow!("missing transcription backend for route {selected_route_id}")
        })?;
        Ok((selected_route_id.to_string(), backend.identity(None)))
    }

    /// Returns the provider/model identity used by cache fingerprints for one option set.
    pub(crate) fn planned_backend_for_options(
        &self,
        preferred_route_id: Option<&str>,
        credential_scope: Option<&kheish_types::CredentialScope>,
        options: Option<&NormalizedDerivationTranscriptionOptions>,
    ) -> Result<(String, AudioTranscriptionBackendIdentity)> {
        let (route_id, mut identity) =
            self.planned_backend(preferred_route_id, credential_scope)?;
        if options.is_some_and(NormalizedDerivationTranscriptionOptions::diarization) {
            anyhow::ensure!(
                identity.provider == "openai",
                "transcription speaker diarization requires an OpenAI transcription backend"
            );
            identity.model = resolve_openai_transcription_request_model(&identity.model, true);
        }
        Ok((route_id, identity))
    }
}

fn transcription_audio_media_type(media_type: &str) -> Option<&'static str> {
    match media_type {
        "audio/wav" => Some("audio/wav"),
        "audio/webm" => Some("audio/webm"),
        "audio/mpeg" | "audio/mp3" | "audio/mpga" => Some("audio/mpeg"),
        "audio/ogg" | "audio/opus" => Some("audio/opus"),
        "audio/aac" | "audio/x-aac" => Some("audio/aac"),
        "audio/flac" | "audio/x-flac" => Some("audio/flac"),
        "audio/mp4" => Some("audio/mp4"),
        "audio/x-m4a" | "audio/m4a" => Some("audio/m4a"),
        _ => None,
    }
}

pub(crate) fn validate_transcription_hints(
    prompt: Option<&str>,
    language: Option<&str>,
) -> Result<(Option<String>, Option<String>)> {
    let prompt = prompt
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            anyhow::ensure!(
                value.chars().count() <= MAX_TRANSCRIPTION_PROMPT_CHARS,
                "transcription prompt exceeds the {} character limit",
                MAX_TRANSCRIPTION_PROMPT_CHARS
            );
            Ok::<_, anyhow::Error>(value.to_string())
        })
        .transpose()?;
    let language = language
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            anyhow::ensure!(
                value.chars().count() <= MAX_TRANSCRIPTION_LANGUAGE_CHARS,
                "transcription language exceeds the {} character limit",
                MAX_TRANSCRIPTION_LANGUAGE_CHARS
            );
            anyhow::ensure!(
                value
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric()
                        || matches!(character, '-' | '_')),
                "transcription language must contain only ASCII letters, digits, '-' or '_'"
            );
            Ok::<_, anyhow::Error>(value.to_string())
        })
        .transpose()?;
    Ok((prompt, language))
}

fn select_transcription_route_id<'a>(
    backends: &'a BTreeMap<String, Arc<dyn AudioTranscriptionBackend>>,
    preferred_route_id: Option<&'a str>,
    default_route_id: &'a str,
    credential_scope: Option<&kheish_types::CredentialScope>,
) -> Result<&'a str> {
    if let Some(route_id) = preferred_route_id
        .filter(|route_id| backends.contains_key(*route_id))
        .filter(|route_id| route_is_allowed(credential_scope, route_id))
    {
        return Ok(route_id);
    }
    if route_is_allowed(credential_scope, default_route_id) {
        return Ok(default_route_id);
    }
    if let Some(route_id) = backends
        .keys()
        .find(|route_id| route_is_allowed(credential_scope, route_id))
    {
        return Ok(route_id.as_str());
    }
    if credential_scope.is_some() {
        bail!("credential scope blocks all transcription routes");
    }
    Ok(default_route_id)
}

fn route_is_allowed(
    credential_scope: Option<&kheish_types::CredentialScope>,
    route_id: &str,
) -> bool {
    credential_scope.is_none_or(|scope| scope.is_empty() || scope.allows_route(route_id))
}

pub(crate) fn transcription_backend_from_additional_config(
    config: &AdditionalTranscriptionBackendConfig,
    observer: Arc<dyn RuntimeObserver>,
) -> Result<(String, Arc<dyn AudioTranscriptionBackend>)> {
    transcription_backend_from_route(config.route_id(), config.route_config(), observer)?
        .ok_or_else(|| anyhow::anyhow!("additional transcription backend route is unsupported"))
}

pub(crate) fn transcription_backend_from_route(
    route_id: &str,
    route: &ModelRouteConfig,
    observer: Arc<dyn RuntimeObserver>,
) -> Result<Option<(String, Arc<dyn AudioTranscriptionBackend>)>> {
    match route {
        ModelRouteConfig::OpenAi(config) => Ok(Some((
            route_id.to_string(),
            Arc::new(OpenAiAudioTranscriptionBackend::from_config(
                config.clone(),
                observer,
            )),
        ))),
        ModelRouteConfig::OpenRouter(config) => Ok(Some((
            route_id.to_string(),
            Arc::new(OpenRouterAudioTranscriptionBackend::from_config(
                config.clone(),
                observer,
            )),
        ))),
        ModelRouteConfig::Google(_) | ModelRouteConfig::Anthropic(_) | ModelRouteConfig::XAi(_) => {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use kheish_runtime::{
        AudioTranscriptionResponse, NoopObserver, OpenRouterModelCapabilities,
        OpenRouterProviderConfig,
    };
    use kheish_types::CredentialScope;

    use super::{
        AudioTranscriptionBackend, AudioTranscriptionBackendIdentity,
        AudioTranscriptionBackendRequest, MAX_AUDIO_TRANSCRIPTION_BYTES, TranscriptionService,
    };
    use crate::assets::FileAssetStore;

    struct FakeTranscriptionBackend;

    #[async_trait]
    impl AudioTranscriptionBackend for FakeTranscriptionBackend {
        fn identity(&self, _model_override: Option<&str>) -> AudioTranscriptionBackendIdentity {
            AudioTranscriptionBackendIdentity {
                provider: "openai".to_string(),
                model: "gpt-4o-transcribe".to_string(),
            }
        }

        async fn transcribe(
            &self,
            _request: &AudioTranscriptionBackendRequest,
            _model_override: Option<&str>,
        ) -> Result<AudioTranscriptionResponse> {
            Ok(AudioTranscriptionResponse {
                text: "hello".to_string(),
                provider: "openai".to_string(),
                model: "gpt-4o-transcribe".to_string(),
                timestamps: None,
            })
        }
    }

    #[test]
    fn openrouter_transcription_backend_maps_chat_route_to_default_stt() {
        let config = OpenRouterProviderConfig::new("x-ai/grok-4.3", "test-key");
        let backend =
            super::OpenRouterAudioTranscriptionBackend::from_config(config, Arc::new(NoopObserver));

        assert_eq!(backend.config.model, "openai/gpt-4o-mini-transcribe");
    }

    #[test]
    fn openrouter_transcription_backend_preserves_explicit_stt_route_model() {
        let model = "openai/whisper-1";
        let mut capabilities = BTreeMap::new();
        capabilities.insert(
            model.to_string(),
            OpenRouterModelCapabilities {
                audio_input: true,
                text_output: true,
                ..OpenRouterModelCapabilities::default()
            },
        );
        let config =
            OpenRouterProviderConfig::new(model, "test-key").with_model_capabilities(capabilities);
        let backend =
            super::OpenRouterAudioTranscriptionBackend::from_config(config, Arc::new(NoopObserver));

        assert_eq!(backend.config.model, model);
    }

    #[tokio::test]
    async fn transcription_service_rejects_routes_blocked_by_credential_scope() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = FileAssetStore::new(temp.path())?;
        let wav_bytes = valid_wav_bytes();
        let asset = assets.import_bytes("call.wav", Some("audio/wav"), &wav_bytes)?;
        let mut backends: BTreeMap<String, Arc<dyn AudioTranscriptionBackend>> = BTreeMap::new();
        backends.insert("openai".to_string(), Arc::new(FakeTranscriptionBackend));
        let service = TranscriptionService::new(backends, "openai".to_string())?;
        let scope = CredentialScope {
            route_deny: vec!["openai".to_string()],
            ..CredentialScope::default()
        };

        let error = service
            .transcribe_asset_with_route(&asset, wav_bytes, Some("openai"), Some(&scope), None)
            .await
            .expect_err("blocked transcription route should fail");

        assert!(
            error
                .to_string()
                .contains("credential scope blocks all transcription routes"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn transcription_service_accepts_openai_documented_mpga_media_type() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = FileAssetStore::new(temp.path())?;
        let audio_bytes = valid_mp3_bytes();
        let asset = assets.import_bytes("call.mpga", Some("audio/mpga"), &audio_bytes)?;
        let mut backends: BTreeMap<String, Arc<dyn AudioTranscriptionBackend>> = BTreeMap::new();
        backends.insert("openai".to_string(), Arc::new(FakeTranscriptionBackend));
        let service = TranscriptionService::new(backends, "openai".to_string())?;

        let result = service
            .transcribe_asset_with_route(&asset, audio_bytes, Some("openai"), None, None)
            .await?;

        assert_eq!(result.route_id, "openai");
        assert_eq!(result.response.text, "hello");
        Ok(())
    }

    #[tokio::test]
    async fn transcription_service_accepts_flac_opus_and_aac_before_upload() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = FileAssetStore::new(temp.path())?;
        let mut backends: BTreeMap<String, Arc<dyn AudioTranscriptionBackend>> = BTreeMap::new();
        backends.insert("openai".to_string(), Arc::new(FakeTranscriptionBackend));
        let service = TranscriptionService::new(backends, "openai".to_string())?;

        for (file_name, media_type, bytes) in [
            ("call.flac", "audio/flac", valid_flac_bytes()),
            ("call.opus", "audio/opus", valid_ogg_opus_bytes()),
            ("call.aac", "audio/aac", valid_aac_bytes()),
        ] {
            let asset = assets.import_bytes(file_name, Some(media_type), &bytes)?;
            let result = service
                .transcribe_asset_with_route(&asset, bytes, Some("openai"), None, None)
                .await?;
            assert_eq!(result.route_id, "openai");
            assert_eq!(result.response.text, "hello");
        }
        Ok(())
    }

    #[tokio::test]
    async fn transcription_service_rejects_invalid_audio_payload_before_upload() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = FileAssetStore::new(temp.path())?;
        let audio_bytes = b"not-an-mp3".to_vec();
        let asset = assets.import_bytes("call.mp3", Some("audio/mpeg"), &valid_mp3_bytes())?;
        let mut backends: BTreeMap<String, Arc<dyn AudioTranscriptionBackend>> = BTreeMap::new();
        backends.insert("openai".to_string(), Arc::new(FakeTranscriptionBackend));
        let service = TranscriptionService::new(backends, "openai".to_string())?;

        let error = service
            .transcribe_asset_with_route(&asset, audio_bytes, Some("openai"), None, None)
            .await
            .expect_err("invalid audio should be rejected before provider upload");

        assert!(
            error.to_string().contains("valid MP3"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn transcription_service_rejects_truncated_wav_before_upload() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = FileAssetStore::new(temp.path())?;
        let audio_bytes = {
            let mut bytes = valid_wav_bytes();
            bytes.truncate(bytes.len() - 2);
            bytes
        };
        let asset = assets.import_bytes("truncated.wav", Some("audio/wav"), &valid_wav_bytes())?;
        let mut backends: BTreeMap<String, Arc<dyn AudioTranscriptionBackend>> = BTreeMap::new();
        backends.insert("openai".to_string(), Arc::new(FakeTranscriptionBackend));
        let service = TranscriptionService::new(backends, "openai".to_string())?;

        let error = service
            .transcribe_asset_with_route(&asset, audio_bytes, Some("openai"), None, None)
            .await
            .expect_err("truncated WAV should be rejected before provider upload");
        assert!(
            error.to_string().contains("declared length exceeds")
                || error.to_string().contains("chunk exceeds"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn transcription_service_rejects_incoherent_wav_before_upload() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = FileAssetStore::new(temp.path())?;
        let mut audio_bytes = valid_wav_bytes();
        audio_bytes[32..34].copy_from_slice(&4u16.to_le_bytes());
        let asset = assets.import_bytes("call.wav", Some("audio/wav"), &valid_wav_bytes())?;
        let mut backends: BTreeMap<String, Arc<dyn AudioTranscriptionBackend>> = BTreeMap::new();
        backends.insert("openai".to_string(), Arc::new(FakeTranscriptionBackend));
        let service = TranscriptionService::new(backends, "openai".to_string())?;

        let error = service
            .transcribe_asset_with_route(&asset, audio_bytes, Some("openai"), None, None)
            .await
            .expect_err("incoherent WAV should be rejected before provider upload");
        assert!(
            error.to_string().contains("block_align"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn transcription_service_rejects_id3_only_mp3_before_upload() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = FileAssetStore::new(temp.path())?;
        let audio_bytes = id3_tag_with_payload(b"metadata-only");
        let asset = assets.import_bytes("tag-only.mp3", Some("audio/mpeg"), &valid_mp3_bytes())?;
        let mut backends: BTreeMap<String, Arc<dyn AudioTranscriptionBackend>> = BTreeMap::new();
        backends.insert("openai".to_string(), Arc::new(FakeTranscriptionBackend));
        let service = TranscriptionService::new(backends, "openai".to_string())?;

        let error = service
            .transcribe_asset_with_route(&asset, audio_bytes, Some("openai"), None, None)
            .await
            .expect_err("ID3-only MP3 should be rejected before provider upload");
        assert!(
            error
                .to_string()
                .contains("does not contain an MP3 audio frame"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn transcription_service_rejects_webm_without_doctype_before_upload() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = FileAssetStore::new(temp.path())?;
        let audio_bytes = vec![0x1a, 0x45, 0xdf, 0xa3, 0x84, 0x42, 0x86, 0x81, 0x01];
        let asset = assets.import_bytes(
            "missing-doctype.webm",
            Some("audio/webm"),
            &valid_webm_bytes(),
        )?;
        let mut backends: BTreeMap<String, Arc<dyn AudioTranscriptionBackend>> = BTreeMap::new();
        backends.insert("openai".to_string(), Arc::new(FakeTranscriptionBackend));
        let service = TranscriptionService::new(backends, "openai".to_string())?;

        let error = service
            .transcribe_asset_with_route(&asset, audio_bytes, Some("openai"), None, None)
            .await
            .expect_err("WebM without DocType webm should be rejected before provider upload");
        assert!(
            error.to_string().contains("valid WebM"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn transcription_service_rejects_webm_without_audio_track_before_upload() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = FileAssetStore::new(temp.path())?;
        let mut audio_bytes = valid_webm_bytes();
        let track_type_value = audio_bytes
            .windows(3)
            .position(|window| window == [0x83, 0x81, 0x02])
            .expect("audio track type fixture");
        audio_bytes[track_type_value + 2] = 0x01;
        let asset = assets.import_bytes("video.webm", Some("audio/webm"), &valid_webm_bytes())?;
        let mut backends: BTreeMap<String, Arc<dyn AudioTranscriptionBackend>> = BTreeMap::new();
        backends.insert("openai".to_string(), Arc::new(FakeTranscriptionBackend));
        let service = TranscriptionService::new(backends, "openai".to_string())?;

        let error = service
            .transcribe_asset_with_route(&asset, audio_bytes, Some("openai"), None, None)
            .await
            .expect_err("WebM without audio track should be rejected before provider upload");
        assert!(
            error
                .to_string()
                .contains("supported Opus/Vorbis audio track"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn transcription_service_rejects_webm_without_media_data_before_upload() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = FileAssetStore::new(temp.path())?;
        let mut audio_bytes = valid_webm_bytes();
        let block_payload = audio_bytes
            .windows(7)
            .position(|window| window == [0xa3, 0x85, 0x81, 0x00, 0x00, 0x80, 0x00])
            .expect("simple block fixture");
        audio_bytes[block_payload] = 0xec;
        let asset = assets.import_bytes("empty.webm", Some("audio/webm"), &valid_webm_bytes())?;
        let mut backends: BTreeMap<String, Arc<dyn AudioTranscriptionBackend>> = BTreeMap::new();
        backends.insert("openai".to_string(), Arc::new(FakeTranscriptionBackend));
        let service = TranscriptionService::new(backends, "openai".to_string())?;

        let error = service
            .transcribe_asset_with_route(&asset, audio_bytes, Some("openai"), None, None)
            .await
            .expect_err(
                "WebM without media block payload should be rejected before provider upload",
            );
        assert!(
            error.to_string().contains("non-empty media data"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn transcription_service_rejects_header_only_mp3_before_upload() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = FileAssetStore::new(temp.path())?;
        let asset = assets.import_bytes("call.mp3", Some("audio/mpeg"), &valid_mp3_bytes())?;
        let mut backends: BTreeMap<String, Arc<dyn AudioTranscriptionBackend>> = BTreeMap::new();
        backends.insert("openai".to_string(), Arc::new(FakeTranscriptionBackend));
        let service = TranscriptionService::new(backends, "openai".to_string())?;

        let error = service
            .transcribe_asset_with_route(
                &asset,
                vec![0xff, 0xfb, 0x90, 0x64],
                Some("openai"),
                None,
                None,
            )
            .await
            .expect_err("header-only MP3 should be rejected before provider upload");
        assert!(
            error.to_string().contains("MP3 audio frame is truncated"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn transcription_service_rejects_video_only_mp4_before_upload() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = FileAssetStore::new(temp.path())?;
        let mut backends: BTreeMap<String, Arc<dyn AudioTranscriptionBackend>> = BTreeMap::new();
        backends.insert("openai".to_string(), Arc::new(FakeTranscriptionBackend));
        let service = TranscriptionService::new(backends, "openai".to_string())?;

        let audio_bytes = minimal_iso_bmff_bytes(*b"soun");
        let audio_asset = assets.import_bytes("call.m4a", Some("audio/m4a"), &audio_bytes)?;
        let result = service
            .transcribe_asset_with_route(&audio_asset, audio_bytes, Some("openai"), None, None)
            .await?;
        assert_eq!(result.response.text, "hello");

        let video_bytes = minimal_iso_bmff_bytes(*b"vide");
        let error = service
            .transcribe_asset_with_route(&audio_asset, video_bytes, Some("openai"), None, None)
            .await
            .expect_err("video-only MP4 should be rejected before provider upload");
        assert!(
            error.to_string().contains("without an audio track"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn transcription_service_rejects_audio_larger_than_openai_limit() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let assets = FileAssetStore::new(temp.path())?;
        let large_bytes = vec![0u8; MAX_AUDIO_TRANSCRIPTION_BYTES + 1];
        let asset = assets.import_bytes("large.mpga", Some("audio/mpga"), &valid_mp3_bytes())?;
        let mut backends: BTreeMap<String, Arc<dyn AudioTranscriptionBackend>> = BTreeMap::new();
        backends.insert("openai".to_string(), Arc::new(FakeTranscriptionBackend));
        let service = TranscriptionService::new(backends, "openai".to_string())?;

        let error = service
            .transcribe_asset_with_route(&asset, large_bytes, Some("openai"), None, None)
            .await
            .expect_err("oversized audio should be rejected before provider upload");

        assert!(
            error
                .to_string()
                .contains("too large for audio transcription"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    fn valid_wav_bytes() -> Vec<u8> {
        let data = [0i16, 512i16]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect::<Vec<_>>();
        let data_chunk_size = data.len() as u32;
        let riff_size = 4 + (8 + 16) + (8 + data_chunk_size);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&riff_size.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8_000u32.to_le_bytes());
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_chunk_size.to_le_bytes());
        bytes.extend_from_slice(&data);
        bytes
    }

    fn valid_mp3_bytes() -> Vec<u8> {
        let mut bytes = vec![0xff, 0xfb, 0x90, 0x64];
        bytes.resize(417, 0);
        bytes
    }

    fn valid_ogg_opus_bytes() -> Vec<u8> {
        let mut bytes = vec![0; 27];
        bytes[..4].copy_from_slice(b"OggS");
        bytes[26] = 1;
        bytes.push(19);
        bytes.extend_from_slice(b"OpusHead");
        bytes.extend_from_slice(&[1, 1, 0, 0, 0, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes
    }

    fn valid_aac_bytes() -> Vec<u8> {
        vec![0xff, 0xf1, 0x50, 0x80, 0x01, 0x7f, 0xfc, 0, 1, 2, 3]
    }

    fn valid_flac_bytes() -> Vec<u8> {
        let mut bytes = b"fLaC".to_vec();
        bytes.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
        let mut streaminfo = [0u8; 34];
        streaminfo[0..2].copy_from_slice(&4096u16.to_be_bytes());
        streaminfo[2..4].copy_from_slice(&4096u16.to_be_bytes());
        let packed = ((44_100u64 & 0x000f_ffff) << 44)
            | (0u64 << 41)
            | ((15u64 & 0x1f) << 36)
            | (44_100u64 & 0x0000_000f_ffff_ffff);
        streaminfo[10..18].copy_from_slice(&packed.to_be_bytes());
        bytes.extend_from_slice(&streaminfo);
        bytes
    }

    fn valid_webm_bytes() -> Vec<u8> {
        vec![
            0x1a, 0x45, 0xdf, 0xa3, 0x9f, 0x42, 0x86, 0x81, 0x01, 0x42, 0xf7, 0x81, 0x01, 0x42,
            0xf2, 0x81, 0x04, 0x42, 0xf3, 0x81, 0x08, 0x42, 0x82, 0x84, b'w', b'e', b'b', b'm',
            0x42, 0x87, 0x81, 0x04, 0x42, 0x85, 0x81, 0x02, 0x18, 0x53, 0x80, 0x67, 0xb3, 0x16,
            0x54, 0xae, 0x6b, 0x9f, 0xae, 0x9d, 0xd7, 0x81, 0x01, 0x73, 0xc5, 0x81, 0x01, 0x83,
            0x81, 0x02, 0x86, 0x86, b'A', b'_', b'O', b'P', b'U', b'S', 0xe1, 0x89, 0xb5, 0x84,
            0x47, 0x3b, 0x80, 0x00, 0x9f, 0x81, 0x01, 0x1f, 0x43, 0xb6, 0x75, 0x8a, 0xe7, 0x81,
            0x00, 0xa3, 0x85, 0x81, 0x00, 0x00, 0x80, 0x00,
        ]
    }

    fn id3_tag_with_payload(payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ID3");
        bytes.extend_from_slice(&[4, 0, 0]);
        bytes.extend_from_slice(&id3_syncsafe_size(payload.len()));
        bytes.extend_from_slice(payload);
        bytes
    }

    fn id3_syncsafe_size(size: usize) -> [u8; 4] {
        [
            ((size >> 21) & 0x7f) as u8,
            ((size >> 14) & 0x7f) as u8,
            ((size >> 7) & 0x7f) as u8,
            (size & 0x7f) as u8,
        ]
    }

    fn minimal_iso_bmff_bytes(handler_type: [u8; 4]) -> Vec<u8> {
        let ftyp = iso_box(*b"ftyp", b"M4A \0\0\0\0M4A ".to_vec());
        let hdlr = iso_box(
            *b"hdlr",
            [
                &[0, 0, 0, 0][..],
                &[0, 0, 0, 0][..],
                &handler_type[..],
                &[0; 12][..],
                &[0][..],
            ]
            .concat(),
        );
        let mdia = iso_box(*b"mdia", hdlr);
        let trak = iso_box(*b"trak", mdia);
        let moov = iso_box(*b"moov", trak);
        let mdat = iso_box(*b"mdat", vec![0, 1, 2, 3]);
        [ftyp, moov, mdat].concat()
    }

    fn iso_box(kind: [u8; 4], payload: Vec<u8>) -> Vec<u8> {
        let size = u32::try_from(payload.len() + 8).expect("test ISO box size");
        let mut bytes = Vec::with_capacity(payload.len() + 8);
        bytes.extend_from_slice(&size.to_be_bytes());
        bytes.extend_from_slice(&kind);
        bytes.extend_from_slice(&payload);
        bytes
    }
}
