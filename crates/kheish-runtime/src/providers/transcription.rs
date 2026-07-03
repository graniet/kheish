use std::collections::BTreeMap;
use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use reqwest::header::CONTENT_LENGTH;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::multipart::{Form, Part};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::ProviderError;
use crate::observability::{
    DebugArtifact, RuntimeObserver, external_action_trace_with_grant_id,
    failed_external_action_outcome, safe_url_audit_target, safe_url_debug_target,
};
use crate::{
    DebugArtifactFormat, NoopObserver, current_cancellation_token, headers_payload_for_level,
    interrupted_error, provider_payload_for_level,
};
use kheish_auth::ResolvedAuthMaterial;
use kheish_codec::{digest_bytes, digest_json_value, digest_text};

use super::OpenAiProviderConfig;
use super::errors::sanitize_upstream_error_message;

const DEFAULT_OPENAI_TRANSCRIPTION_MODEL: &str = "gpt-4o-transcribe";
const DEFAULT_OPENAI_DIARIZATION_MODEL: &str = "gpt-4o-transcribe-diarize";
const CODEX_RESPONSES_PATH_MARKER: &str = "/backend-api/codex/responses";
const OPENAI_AUDIO_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OPENAI_TRANSCRIPTION_REQUEST_BYTES: usize = 25 * 1024 * 1024;
const MAX_OPENAI_TRANSCRIPTION_RESPONSE_BYTES: usize = 1024 * 1024;

/// One provider-neutral speech-to-text request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioTranscriptionRequest {
    /// The original file name used for multipart metadata and display.
    pub file_name: String,
    /// The normalized MIME type of the uploaded audio payload.
    pub media_type: String,
    /// The raw audio bytes that should be transcribed.
    pub bytes: Vec<u8>,
    /// Optional contextual prompt supplied to the transcription backend.
    pub prompt: Option<String>,
    /// Optional language hint supplied to the transcription backend.
    pub language: Option<String>,
    /// Optional timestamp granularities requested from providers that support them.
    pub timestamp_granularities: Vec<String>,
    /// Requests speaker diarization from providers that support diarized transcription.
    pub diarization: bool,
}

/// One provider-neutral speech-to-text response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioTranscriptionResponse {
    /// The provider family that generated the transcript.
    pub provider: String,
    /// The concrete provider model that generated the transcript.
    pub model: String,
    /// The normalized plain-text transcript.
    pub text: String,
    /// Optional structured timestamp payload from the provider.
    pub timestamps: Option<AudioTranscriptionTimestamps>,
}

/// Structured timestamp payload normalized from provider-specific transcription JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioTranscriptionTimestamps {
    /// Provider-reported language when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Provider-reported audio duration rounded to milliseconds when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Word-level timestamps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<AudioTranscriptionWordTimestamp>,
    /// Segment-level timestamps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<AudioTranscriptionSegmentTimestamp>,
}

/// One word-level timestamp.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioTranscriptionWordTimestamp {
    pub word: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// One segment-level timestamp.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioTranscriptionSegmentTimestamp {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
}

/// Resolves the canonical OpenAI speech-to-text model identifier.
#[must_use]
pub fn resolve_openai_transcription_model(model: &str) -> String {
    let normalized = model.trim();
    let lower = normalized.to_ascii_lowercase();
    if lower.contains("transcribe") || lower == "whisper-1" {
        normalized.to_string()
    } else {
        DEFAULT_OPENAI_TRANSCRIPTION_MODEL.to_string()
    }
}

/// Resolves the effective OpenAI speech-to-text model identifier for one request.
#[must_use]
pub fn resolve_openai_transcription_request_model(model: &str, diarization: bool) -> String {
    if !diarization {
        return resolve_openai_transcription_model(model);
    }
    let normalized = model.trim();
    if normalized
        .to_ascii_lowercase()
        .contains("transcribe-diarize")
    {
        normalized.to_string()
    } else {
        DEFAULT_OPENAI_DIARIZATION_MODEL.to_string()
    }
}

/// One OpenAI-backed speech-to-text client used by daemon-owned services.
#[derive(Clone)]
pub struct OpenAiAudioTranscriber {
    client: Client,
    config: OpenAiProviderConfig,
    observer: Arc<dyn RuntimeObserver>,
}

impl OpenAiAudioTranscriber {
    /// Builds a new OpenAI-backed audio transcriber.
    pub fn new(config: OpenAiProviderConfig) -> Result<Self, ProviderError> {
        Self::with_observer(config, Arc::new(NoopObserver))
    }

    /// Builds a new OpenAI-backed audio transcriber with runtime observation hooks enabled.
    pub fn with_observer(
        mut config: OpenAiProviderConfig,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Result<Self, ProviderError> {
        config.model = resolve_openai_transcription_model(&config.model);
        let client = Client::builder().build().map_err(|error| ProviderError {
            message: format!("failed to build OpenAI transcription client: {error}"),
            retryable: false,
            retry_after_ms: None,
        })?;
        Ok(Self {
            client,
            config,
            observer,
        })
    }

    fn external_action_target(&self, endpoint: &str) -> String {
        format!("openai:{}", safe_url_audit_target(endpoint))
    }

    fn request_digest(&self, request: &AudioTranscriptionRequest, model: &str) -> Value {
        json!({
            "model": model,
            "file_name_sha256": digest_text(&request.file_name),
            "file_extension": request
                .file_name
                .rsplit_once('.')
                .map(|(_, extension)| extension.to_ascii_lowercase()),
            "media_type": request.media_type.clone(),
            "byte_len": request.bytes.len(),
            "sha256": digest_bytes(&request.bytes),
            "prompt_chars": request
                .prompt
                .as_deref()
                .map(|value| value.chars().count())
                .unwrap_or(0),
            "language": request.language.clone(),
            "timestamp_granularities": request.timestamp_granularities.clone(),
            "diarization": request.diarization,
        })
    }

    fn response_digest(&self, response: &AudioTranscriptionResponse) -> Value {
        json!({
            "model": response.model,
            "text_sha256": digest_text(&response.text),
            "text_chars": response.text.chars().count(),
            "word_timestamps": response
                .timestamps
                .as_ref()
                .map(|timestamps| timestamps.words.len())
                .unwrap_or(0),
            "segment_timestamps": response
                .timestamps
                .as_ref()
                .map(|timestamps| timestamps.segments.len())
                .unwrap_or(0),
            "diarized_segments": response
                .timestamps
                .as_ref()
                .map(|timestamps| timestamps.segments.iter().filter(|segment| segment.speaker.is_some()).count())
                .unwrap_or(0),
        })
    }

    fn record_request(
        &self,
        endpoint: &str,
        headers: &HeaderMap,
        request: &AudioTranscriptionRequest,
        model: &str,
        grant_id: Option<String>,
    ) -> Result<(), ProviderError> {
        let digest = self.request_digest(request, model);
        self.observer
            .record_external_action(external_action_trace_with_grant_id(
                "request",
                "model_provider",
                self.external_action_target(endpoint),
                Some(digest_json_value(&digest).unwrap_or_else(|_| "unknown".to_string())),
                None,
                None,
                grant_id,
            ))
            .map_err(provider_audit_error)?;
        let level = self.observer.debug_level();
        if level.is_enabled() {
            self.observer.record_debug_artifact(DebugArtifact::new(
                level,
                None,
                None,
                "openai-audio-transcription-provider-request",
                DebugArtifactFormat::Json,
                json!({
                    "provider": "openai",
                    "method": "POST",
                    "url": safe_url_debug_target(endpoint),
                    "headers": headers_payload_for_level(level, headers),
                    "body": provider_payload_for_level(level, &digest),
                }),
            ));
        }
        Ok(())
    }

    fn record_response(
        &self,
        target: &str,
        status: u16,
        headers: &HeaderMap,
        response: &AudioTranscriptionResponse,
        grant_id: Option<String>,
    ) -> Result<(), ProviderError> {
        let digest = self.response_digest(response);
        self.observer
            .record_external_action(external_action_trace_with_grant_id(
                "response",
                "model_provider",
                target.to_string(),
                None,
                Some(digest_json_value(&digest).unwrap_or_else(|_| "unknown".to_string())),
                Some(status.to_string()),
                grant_id,
            ))
            .map_err(provider_audit_error)?;
        let level = self.observer.debug_level();
        if level.is_enabled() {
            self.observer.record_debug_artifact(DebugArtifact::new(
                level,
                None,
                None,
                "openai-audio-transcription-provider-response",
                DebugArtifactFormat::Json,
                json!({
                    "provider": "openai",
                    "status": status,
                    "headers": headers_payload_for_level(level, headers),
                    "body": provider_payload_for_level(level, &digest),
                }),
            ));
        }
        Ok(())
    }

    fn record_failure(
        &self,
        target: impl Into<String>,
        message: &str,
        grant_id: Option<String>,
    ) -> Result<(), ProviderError> {
        self.observer
            .record_external_action(external_action_trace_with_grant_id(
                "response",
                "model_provider",
                target,
                None,
                None,
                Some(failed_external_action_outcome(message)),
                grant_id,
            ))
            .map_err(provider_audit_error)
    }

    async fn auth_material(
        &self,
        force_refresh: bool,
    ) -> Result<ResolvedAuthMaterial, ProviderError> {
        if let Some(provider) = &self.config.request_auth_provider {
            let result = if force_refresh {
                provider.refresh().await
            } else {
                provider.resolve().await
            };
            return result.map_err(|error| ProviderError {
                message: format!("failed to resolve OpenAI auth material: {error}"),
                retryable: false,
                retry_after_ms: None,
            });
        }
        let api_key = self.config.api_key.clone().ok_or_else(|| ProviderError {
            message: "missing OpenAI API key for audio transcription".to_string(),
            retryable: false,
            retry_after_ms: None,
        })?;
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {api_key}"));
        if let Some(organization) = &self.config.organization {
            headers.insert("OpenAI-Organization".to_string(), organization.clone());
        }
        if let Some(project) = &self.config.project {
            headers.insert("OpenAI-Project".to_string(), project.clone());
        }
        Ok(ResolvedAuthMaterial {
            headers,
            base_url_override: None,
            grant_id: None,
            lease_id: None,
        })
    }

    async fn ensure_auth_material_active(
        &self,
        material: &ResolvedAuthMaterial,
    ) -> Result<(), ProviderError> {
        if let Some(provider) = &self.config.request_auth_provider {
            provider
                .ensure_active(material)
                .await
                .map_err(|error| ProviderError {
                    message: format!("OpenAI auth material is no longer active: {error}"),
                    retryable: false,
                    retry_after_ms: None,
                })?;
        }
        Ok(())
    }

    fn headers_from_material(
        &self,
        material: &ResolvedAuthMaterial,
    ) -> Result<HeaderMap, ProviderError> {
        let mut headers = HeaderMap::new();
        for (name, value) in &material.headers {
            let header_name = if name.eq_ignore_ascii_case("authorization") {
                AUTHORIZATION
            } else {
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                    ProviderError {
                        message: format!("invalid OpenAI header name `{name}`: {error}"),
                        retryable: false,
                        retry_after_ms: None,
                    }
                })?
            };
            headers.insert(
                header_name,
                HeaderValue::from_str(value).map_err(|error| ProviderError {
                    message: format!("invalid OpenAI header `{name}`: {error}"),
                    retryable: false,
                    retry_after_ms: None,
                })?,
            );
        }
        if let Some(organization) = &self.config.organization {
            headers.insert(
                reqwest::header::HeaderName::from_static("openai-organization"),
                HeaderValue::from_str(organization).map_err(|error| ProviderError {
                    message: format!("invalid OpenAI organization header: {error}"),
                    retryable: false,
                    retry_after_ms: None,
                })?,
            );
        }
        if let Some(project) = &self.config.project {
            headers.insert(
                reqwest::header::HeaderName::from_static("openai-project"),
                HeaderValue::from_str(project).map_err(|error| ProviderError {
                    message: format!("invalid OpenAI project header: {error}"),
                    retryable: false,
                    retry_after_ms: None,
                })?,
            );
        }
        Ok(headers)
    }

    /// Sends one bounded audio payload to the OpenAI audio transcriptions endpoint.
    pub async fn transcribe(
        &self,
        request: &AudioTranscriptionRequest,
    ) -> Result<AudioTranscriptionResponse, ProviderError> {
        let request_model =
            resolve_openai_transcription_request_model(&self.config.model, request.diarization);
        validate_openai_transcription_request(request, &request_model)?;
        let cancellation = current_cancellation_token();
        if cancellation
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return Err(audio_transcription_interrupted_error());
        }
        let mut force_refresh = false;
        let (response, response_target, response_grant_id) = loop {
            let auth_material = self.auth_material(force_refresh).await?;
            let grant_id = auth_material.grant_id.clone();
            let endpoint = openai_audio_transcriptions_endpoint(
                auth_material
                    .base_url_override
                    .as_deref()
                    .unwrap_or(self.config.base_url.as_str()),
            )?;
            let headers = self.headers_from_material(&auth_material)?;
            let part = Part::bytes(request.bytes.clone())
                .file_name(request.file_name.clone())
                .mime_str(&request.media_type)
                .map_err(|error| ProviderError {
                    message: format!("invalid audio media type {}: {error}", request.media_type),
                    retryable: false,
                    retry_after_ms: None,
                })?;
            let response_format = if request.diarization {
                "diarized_json"
            } else if request.timestamp_granularities.is_empty() {
                "text"
            } else {
                "verbose_json"
            };
            let mut form = Form::new()
                .text("model", request_model.clone())
                .text("response_format", response_format)
                .part("file", part);
            if request.diarization {
                form = form.text("chunking_strategy", "auto");
            }
            for granularity in &request.timestamp_granularities {
                form = form.text("timestamp_granularities[]", granularity.clone());
            }
            if let Some(prompt) = request
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
            {
                form = form.text("prompt", prompt.to_string());
            }
            if let Some(language) = request
                .language
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                form = form.text("language", language.to_string());
            }

            self.ensure_auth_material_active(&auth_material).await?;
            self.record_request(
                &endpoint,
                &headers,
                request,
                &request_model,
                grant_id.clone(),
            )?;
            let send = self
                .client
                .post(&endpoint)
                .timeout(OPENAI_AUDIO_REQUEST_TIMEOUT)
                .headers(headers)
                .multipart(form)
                .send();
            let response = match maybe_cancel_provider_future(send, cancellation.clone()).await {
                Ok(response) => response,
                Err(error) => {
                    let mapped = if error.to_string() == interrupted_error().to_string() {
                        audio_transcription_interrupted_error()
                    } else {
                        ProviderError {
                            message: format!("OpenAI audio transcription request failed: {error}"),
                            retryable: true,
                            retry_after_ms: None,
                        }
                    };
                    self.record_failure(
                        self.external_action_target(&endpoint),
                        &mapped.message,
                        grant_id.clone(),
                    )?;
                    return Err(mapped);
                }
            };
            if response.status() == StatusCode::UNAUTHORIZED
                && self.config.request_auth_provider.is_some()
                && !force_refresh
            {
                self.record_failure(
                    self.external_action_target(&endpoint),
                    "401-refresh",
                    grant_id.clone(),
                )?;
                force_refresh = true;
                continue;
            }
            break (response, self.external_action_target(&endpoint), grant_id);
        };
        let status = response.status();
        let response_headers = response.headers().clone();
        let retry_after_ms = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_retry_after_ms);
        let body = match read_openai_transcription_response_body(response, cancellation).await {
            Ok(body) => body,
            Err(error) => {
                self.record_failure(&response_target, &error.message, response_grant_id.clone())?;
                return Err(error);
            }
        };
        if !status.is_success() {
            let error_payload = serde_json::from_str::<serde_json::Value>(&body).ok();
            let error = error_payload
                .as_ref()
                .and_then(|payload| payload.get("error"))
                .unwrap_or(&Value::Null);
            let error_code_string = error
                .get("code")
                .and_then(Value::as_i64)
                .map(|value| value.to_string());
            let message = sanitize_upstream_error_message(
                "OpenAI",
                "audio transcription error",
                Some(status),
                error
                    .get("type")
                    .and_then(Value::as_str)
                    .or_else(|| error.get("status").and_then(Value::as_str)),
                error
                    .get("code")
                    .and_then(Value::as_str)
                    .or(error_code_string.as_deref()),
                error.get("message").and_then(Value::as_str),
            );
            let mapped = ProviderError {
                message,
                retryable: status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error(),
                retry_after_ms,
            };
            self.record_failure(&response_target, &mapped.message, response_grant_id.clone())?;
            return Err(mapped);
        }
        let transcription = decode_openai_transcription_body(
            &body,
            !request.diarization && request.timestamp_granularities.is_empty(),
            &request_model,
        )?;
        self.record_response(
            &response_target,
            status.as_u16(),
            &response_headers,
            &transcription,
            response_grant_id,
        )?;
        Ok(transcription)
    }
}

impl Debug for OpenAiAudioTranscriber {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiAudioTranscriber")
            .field("config", &self.config)
            .finish()
    }
}

fn validate_openai_transcription_request(
    request: &AudioTranscriptionRequest,
    model: &str,
) -> Result<(), ProviderError> {
    if request.bytes.is_empty() {
        return Err(ProviderError {
            message: "OpenAI audio transcription requires audio bytes".to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if request.bytes.len() > MAX_OPENAI_TRANSCRIPTION_REQUEST_BYTES {
        return Err(ProviderError {
            message: format!(
                "OpenAI audio transcription request exceeds the {} byte limit",
                MAX_OPENAI_TRANSCRIPTION_REQUEST_BYTES
            ),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if !openai_transcription_media_type_supported(&request.media_type) {
        return Err(ProviderError {
            message: format!(
                "OpenAI audio transcription does not support media type {}",
                request.media_type
            ),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if request.diarization {
        if request
            .prompt
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Err(ProviderError {
                message: "OpenAI diarized audio transcription does not support prompts".to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
        if !request.timestamp_granularities.is_empty() {
            return Err(ProviderError {
                message:
                    "OpenAI diarized audio transcription does not support timestamp granularities"
                        .to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
    }
    if let Some(prompt) = request.prompt.as_deref()
        && prompt.chars().count() > 2_000
    {
        return Err(ProviderError {
            message: "OpenAI audio transcription prompt exceeds the 2000 character limit"
                .to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if let Some(language) = request.language.as_deref().map(str::trim)
        && !language.is_empty()
        && (language.chars().count() > 32
            || !language.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            }))
    {
        return Err(ProviderError {
            message:
                "OpenAI audio transcription language must be a short ASCII language identifier"
                    .to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if !request.timestamp_granularities.is_empty() {
        for granularity in &request.timestamp_granularities {
            if granularity != "word" && granularity != "segment" {
                return Err(ProviderError {
                    message:
                        "OpenAI audio transcription timestamp granularity must be `word` or `segment`"
                            .to_string(),
                    retryable: false,
                    retry_after_ms: None,
                });
            }
        }
        if request.timestamp_granularities.len() > 2 {
            return Err(ProviderError {
                message: "OpenAI audio transcription accepts at most word and segment timestamps"
                    .to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
        if request
            .timestamp_granularities
            .windows(2)
            .any(|pair| pair[0] == pair[1])
        {
            return Err(ProviderError {
                message: "OpenAI audio transcription timestamp granularities must be unique"
                    .to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
        if model.trim() != "whisper-1" {
            return Err(ProviderError {
                message:
                    "OpenAI audio transcription timestamps require transcription model `whisper-1`"
                        .to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
    }
    Ok(())
}

fn openai_transcription_media_type_supported(media_type: &str) -> bool {
    matches!(
        media_type
            .split_once(';')
            .map(|(value, _)| value)
            .unwrap_or(media_type)
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "audio/wav"
            | "audio/x-wav"
            | "audio/webm"
            | "audio/mpeg"
            | "audio/mp3"
            | "audio/mpga"
            | "audio/ogg"
            | "audio/opus"
            | "audio/aac"
            | "audio/x-aac"
            | "audio/flac"
            | "audio/x-flac"
            | "audio/mp4"
            | "audio/m4a"
            | "audio/x-m4a"
    )
}

async fn maybe_cancel_provider_future<F, T>(
    future: F,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<T, anyhow::Error>
where
    F: std::future::Future<Output = Result<T, reqwest::Error>>,
{
    if let Some(cancellation) = cancellation {
        tokio::select! {
            result = future => result.map_err(Into::into),
            _ = cancellation.cancelled() => Err(interrupted_error()),
        }
    } else {
        future.await.map_err(Into::into)
    }
}

fn audio_transcription_interrupted_error() -> ProviderError {
    ProviderError {
        message: interrupted_error().to_string(),
        retryable: false,
        retry_after_ms: None,
    }
}

fn decode_openai_transcription_body(
    body: &str,
    plain_text: bool,
    model: &str,
) -> Result<AudioTranscriptionResponse, ProviderError> {
    if plain_text {
        return Ok(AudioTranscriptionResponse {
            provider: "openai".to_string(),
            model: model.to_string(),
            text: body.trim().to_string(),
            timestamps: None,
        });
    }

    let payload = serde_json::from_str::<Value>(body).map_err(|error| ProviderError {
        message: format!("OpenAI audio transcription JSON was invalid: {error}"),
        retryable: false,
        retry_after_ms: None,
    })?;
    let text = payload
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ProviderError {
            message: "OpenAI audio transcription JSON did not contain text".to_string(),
            retryable: false,
            retry_after_ms: None,
        })?
        .to_string();
    Ok(AudioTranscriptionResponse {
        provider: "openai".to_string(),
        model: model.to_string(),
        text,
        timestamps: openai_transcription_timestamps(&payload),
    })
}

fn openai_transcription_timestamps(payload: &Value) -> Option<AudioTranscriptionTimestamps> {
    let words = payload
        .get("words")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let word = item.get("word").and_then(Value::as_str)?.trim();
                    let start_ms = seconds_value_to_ms(item.get("start")?)?;
                    let end_ms = seconds_value_to_ms(item.get("end")?)?;
                    (!word.is_empty() && end_ms >= start_ms).then(|| {
                        AudioTranscriptionWordTimestamp {
                            word: word.to_string(),
                            start_ms,
                            end_ms,
                        }
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let segments = payload
        .get("segments")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let start_ms = seconds_value_to_ms(item.get("start")?)?;
                    let end_ms = seconds_value_to_ms(item.get("end")?)?;
                    (end_ms >= start_ms).then(|| AudioTranscriptionSegmentTimestamp {
                        id: item.get("id").and_then(Value::as_i64),
                        text: item
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                        start_ms,
                        end_ms,
                        speaker: item
                            .get("speaker")
                            .and_then(Value::as_str)
                            .or_else(|| item.get("speaker_id").and_then(Value::as_str))
                            .or_else(|| item.get("speaker_label").and_then(Value::as_str))
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(ToString::to_string),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let language = payload
        .get("language")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let duration_ms = payload.get("duration").and_then(seconds_value_to_ms);

    (language.is_some() || duration_ms.is_some() || !words.is_empty() || !segments.is_empty())
        .then_some(AudioTranscriptionTimestamps {
            language,
            duration_ms,
            words,
            segments,
        })
}

fn seconds_value_to_ms(value: &Value) -> Option<u64> {
    let seconds = value.as_f64()?;
    seconds.is_finite().then_some(seconds)?;
    (seconds >= 0.0).then_some((seconds * 1_000.0).round() as u64)
}

async fn read_openai_transcription_response_body(
    response: reqwest::Response,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<String, ProviderError> {
    let retry_after_ms = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after_ms);
    let retryable = response.status().is_server_error();
    if let Some(content_length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        && content_length > MAX_OPENAI_TRANSCRIPTION_RESPONSE_BYTES
    {
        return Err(ProviderError {
            message: format!(
                "OpenAI audio transcription response exceeds the {} byte limit",
                MAX_OPENAI_TRANSCRIPTION_RESPONSE_BYTES
            ),
            retryable: false,
            retry_after_ms,
        });
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let chunk = if let Some(cancellation) = cancellation.as_ref() {
            tokio::select! {
                chunk = stream.next() => chunk,
                _ = cancellation.cancelled() => return Err(audio_transcription_interrupted_error()),
            }
        } else {
            stream.next().await
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(|error| ProviderError {
            message: format!("failed to read OpenAI audio transcription response body: {error}"),
            retryable,
            retry_after_ms,
        })?;
        if bytes.len().saturating_add(chunk.len()) > MAX_OPENAI_TRANSCRIPTION_RESPONSE_BYTES {
            return Err(ProviderError {
                message: format!(
                    "OpenAI audio transcription response exceeds the {} byte limit",
                    MAX_OPENAI_TRANSCRIPTION_RESPONSE_BYTES
                ),
                retryable: false,
                retry_after_ms,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|error| ProviderError {
        message: format!("OpenAI audio transcription response body was not UTF-8: {error}"),
        retryable: false,
        retry_after_ms,
    })
}

fn parse_retry_after_ms(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if let Ok(seconds) = trimmed.parse::<u64>() {
        return Some(seconds.saturating_mul(1_000));
    }
    let target = httpdate::parse_http_date(trimmed).ok()?;
    let duration = target.duration_since(SystemTime::now()).ok()?;
    Some(
        u64::try_from(duration.as_millis())
            .unwrap_or(u64::MAX)
            .max(1),
    )
}

fn openai_audio_transcriptions_endpoint(base_url: &str) -> Result<String, ProviderError> {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.contains(CODEX_RESPONSES_PATH_MARKER) {
        return Err(ProviderError {
            message: "OpenAI Codex account endpoints do not support audio transcription"
                .to_string(),
            retryable: false,
            retry_after_ms: None,
        });
    }
    if trimmed.ends_with("/audio/transcriptions") {
        return Ok(trimmed.to_string());
    }
    if let Some(prefix) = trimmed.strip_suffix("/responses") {
        return Ok(format!("{prefix}/audio/transcriptions"));
    }
    if trimmed.ends_with("/v1") {
        return Ok(format!("{trimmed}/audio/transcriptions"));
    }
    Err(ProviderError {
        message: format!("unsupported OpenAI base URL for audio transcription: {base_url}"),
        retryable: false,
        retry_after_ms: None,
    })
}

fn provider_audit_error(error: anyhow::Error) -> ProviderError {
    ProviderError {
        message: format!("external action audit failed: {error}"),
        retryable: false,
        retry_after_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::sync::Arc;

    use anyhow::Result;

    use super::*;
    use crate::providers::testsupport::spawn_mock_server;
    use crate::{
        DebugArtifact, DebugCaptureLevel, ExecutionScope, InMemoryObserver, TraceEvent,
        TraceEventKind, scope_execution,
    };

    struct FixedDebugObserver {
        artifacts: Mutex<Vec<DebugArtifact>>,
    }

    impl FixedDebugObserver {
        fn shared() -> Arc<Self> {
            Arc::new(Self {
                artifacts: Mutex::new(Vec::new()),
            })
        }

        fn debug_artifacts(&self) -> Vec<DebugArtifact> {
            self.artifacts.lock().clone()
        }
    }

    impl RuntimeObserver for FixedDebugObserver {
        fn debug_level(&self) -> DebugCaptureLevel {
            DebugCaptureLevel::Full
        }

        fn record(&self, _event: TraceEvent) {}

        fn record_debug_artifact(&self, artifact: DebugArtifact) {
            self.artifacts.lock().push(artifact);
        }

        fn increment_counter(&self, _name: &str, _delta: u64) {}
    }

    fn has_provider_external_action(
        traces: &[TraceEvent],
        phase: &str,
        target_prefix: &str,
    ) -> bool {
        traces.iter().any(|event| {
            matches!(
                &event.kind,
                TraceEventKind::ExternalAction {
                    phase: recorded_phase,
                    kind,
                    target,
                    ..
                } if recorded_phase == phase
                    && kind == "model_provider"
                    && target.starts_with(target_prefix)
            )
        })
    }

    #[test]
    fn resolves_openai_transcription_endpoint_from_responses_base_url() {
        assert_eq!(
            openai_audio_transcriptions_endpoint("https://api.openai.com/v1/responses")
                .expect("endpoint"),
            "https://api.openai.com/v1/audio/transcriptions"
        );
        assert!(
            openai_audio_transcriptions_endpoint("https://chatgpt.com/backend-api/codex/responses")
                .expect_err("Codex account endpoints must be explicit")
                .message
                .contains("do not support audio transcription")
        );
    }

    #[test]
    fn resolves_openai_transcription_model_from_text_routes() {
        assert_eq!(
            resolve_openai_transcription_model("gpt-5-mini"),
            "gpt-4o-transcribe"
        );
        assert_eq!(
            resolve_openai_transcription_model("gpt-4o-mini-transcribe"),
            "gpt-4o-mini-transcribe"
        );
        assert_eq!(resolve_openai_transcription_model("whisper-1"), "whisper-1");
    }

    #[tokio::test]
    async fn transcribes_audio_through_openai_audio_endpoint() -> Result<()> {
        let captured = Arc::new(Mutex::new(String::new()));
        let base_url = spawn_mock_server(
            200,
            &[("Content-Type", "text/plain")],
            "hello from transcription",
            captured.clone(),
        )
        .await?;
        let mut config = OpenAiProviderConfig::new("gpt-4o-transcribe", "test-key");
        config.base_url = format!("{base_url}/v1");
        let transcriber = OpenAiAudioTranscriber::new(config)?;
        let response = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sample.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: b"RIFFxxxxWAVEfmt ".to_vec(),
                prompt: Some("Prefer exact transcript".to_string()),
                language: Some("fr".to_string()),
                timestamp_granularities: Vec::new(),
                diarization: false,
            })
            .await?;
        assert_eq!(response.provider, "openai");
        assert_eq!(response.model, "gpt-4o-transcribe");
        assert_eq!(response.text, "hello from transcription");
        let body = captured.lock();
        assert!(body.contains("gpt-4o-transcribe"));
        assert!(body.contains("response_format"));
        assert!(body.contains("Prefer exact transcript"));
        assert!(body.contains("sample.wav"));
        Ok(())
    }

    #[tokio::test]
    async fn openai_audio_transcriber_rejects_oversized_audio_before_network() -> Result<()> {
        let config = OpenAiProviderConfig::new("gpt-4o-transcribe", "test-key");
        let transcriber = OpenAiAudioTranscriber::new(config)?;

        let error = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "huge.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: vec![0u8; MAX_OPENAI_TRANSCRIPTION_REQUEST_BYTES + 1],
                prompt: None,
                language: None,
                timestamp_granularities: Vec::new(),
                diarization: false,
            })
            .await
            .expect_err("oversized request should fail before network");

        assert!(!error.retryable);
        assert!(
            error.message.contains("request exceeds"),
            "unexpected error: {}",
            error.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn openai_audio_transcriber_rejects_unsupported_media_type_before_network() -> Result<()>
    {
        let config = OpenAiProviderConfig::new("gpt-4o-transcribe", "test-key");
        let transcriber = OpenAiAudioTranscriber::new(config)?;

        let error = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sample.wma".to_string(),
                media_type: "audio/x-ms-wma".to_string(),
                bytes: b"not-supported".to_vec(),
                prompt: None,
                language: None,
                timestamp_granularities: Vec::new(),
                diarization: false,
            })
            .await
            .expect_err("unsupported request should fail before network");

        assert!(!error.retryable);
        assert!(
            error
                .message
                .contains("does not support media type audio/x-ms-wma"),
            "unexpected error: {}",
            error.message
        );
        Ok(())
    }

    #[test]
    fn openai_audio_transcriber_accepts_documented_transcription_media_types() {
        for media_type in [
            "audio/wav",
            "audio/webm",
            "audio/mpeg",
            "audio/mpga",
            "audio/mp4",
            "audio/m4a",
            "audio/ogg",
            "audio/opus",
            "audio/aac",
            "audio/flac",
        ] {
            assert!(
                openai_transcription_media_type_supported(media_type),
                "expected {media_type} to be supported"
            );
        }
    }

    #[tokio::test]
    async fn openai_audio_transcriber_honors_cancelled_execution_scope() -> Result<()> {
        let config = OpenAiProviderConfig::new("gpt-4o-transcribe", "test-key");
        let transcriber = OpenAiAudioTranscriber::new(config)?;
        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();

        let error = scope_execution(ExecutionScope::default(), cancellation, async {
            transcriber
                .transcribe(&AudioTranscriptionRequest {
                    file_name: "sample.wav".to_string(),
                    media_type: "audio/wav".to_string(),
                    bytes: b"RIFFxxxxWAVEfmt ".to_vec(),
                    prompt: None,
                    language: None,
                    timestamp_granularities: Vec::new(),
                    diarization: false,
                })
                .await
        })
        .await
        .expect_err("cancelled transcription should stop before network");

        assert!(!error.retryable);
        assert_eq!(error.message, interrupted_error().to_string());
        Ok(())
    }

    #[tokio::test]
    async fn openai_audio_transcriber_records_external_action_traces() -> Result<()> {
        let base_url = spawn_mock_server(
            200,
            &[("Content-Type", "text/plain")],
            "hello from transcription",
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let observer = InMemoryObserver::shared();
        let mut config = OpenAiProviderConfig::new("gpt-4o-transcribe", "test-key");
        config.base_url = format!("{base_url}/v1");
        let transcriber = OpenAiAudioTranscriber::with_observer(config, observer.clone())?;
        let response = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sample.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: b"RIFFxxxxWAVEfmt ".to_vec(),
                prompt: Some("Prefer exact transcript".to_string()),
                language: Some("fr".to_string()),
                timestamp_granularities: Vec::new(),
                diarization: false,
            })
            .await?;

        assert_eq!(response.text, "hello from transcription");
        let traces = observer.traces();
        assert!(has_provider_external_action(
            &traces,
            "request",
            "openai:http://"
        ));
        assert!(has_provider_external_action(
            &traces,
            "response",
            "openai:http://"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn openai_audio_transcriber_records_debug_without_raw_audio() -> Result<()> {
        let base_url = spawn_mock_server(
            200,
            &[("Content-Type", "text/plain")],
            "secret transcript",
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let observer = FixedDebugObserver::shared();
        let mut config = OpenAiProviderConfig::new("gpt-4o-transcribe", "test-key");
        config.base_url = format!("{base_url}/v1");
        let transcriber = OpenAiAudioTranscriber::with_observer(config, observer.clone())?;
        let response = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sample.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: b"RAW-AUDIO-BYTES".to_vec(),
                prompt: Some("SECRET PROMPT".to_string()),
                language: None,
                timestamp_granularities: Vec::new(),
                diarization: false,
            })
            .await?;

        assert_eq!(response.text, "secret transcript");
        let artifacts = observer.debug_artifacts();
        assert!(artifacts.iter().any(|artifact| {
            artifact.name == "openai-audio-transcription-provider-request"
                && artifact.payload["body"]["byte_len"] == 15
        }));
        let rendered = serde_json::to_string(&artifacts)?;
        assert!(!rendered.contains("RAW-AUDIO-BYTES"));
        assert!(!rendered.contains("SECRET PROMPT"));
        assert!(!rendered.contains("secret transcript"));
        Ok(())
    }

    #[tokio::test]
    async fn openai_audio_transcriber_posts_verbose_json_for_timestamps() -> Result<()> {
        let captured = Arc::new(Mutex::new(String::new()));
        let body = serde_json::json!({
            "text": "hello world",
            "language": "en",
            "duration": 1.25,
            "words": [
                {"word": "hello", "start": 0.0, "end": 0.52},
                {"word": "world", "start": 0.55, "end": 1.25}
            ],
            "segments": [
                {"id": 0, "text": "hello world", "start": 0.0, "end": 1.25}
            ]
        })
        .to_string();
        let base_url = spawn_mock_server(
            200,
            &[("Content-Type", "application/json")],
            &body,
            captured.clone(),
        )
        .await?;
        let mut config = OpenAiProviderConfig::new("whisper-1", "test-key");
        config.base_url = format!("{base_url}/v1");
        let transcriber = OpenAiAudioTranscriber::new(config)?;
        let response = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sample.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: b"RIFFxxxxWAVEfmt ".to_vec(),
                prompt: None,
                language: Some("en".to_string()),
                timestamp_granularities: vec!["word".to_string(), "segment".to_string()],
                diarization: false,
            })
            .await?;

        assert_eq!(response.provider, "openai");
        assert_eq!(response.model, "whisper-1");
        assert_eq!(response.text, "hello world");
        let timestamps = response.timestamps.expect("timestamps should be parsed");
        assert_eq!(timestamps.language.as_deref(), Some("en"));
        assert_eq!(timestamps.duration_ms, Some(1_250));
        assert_eq!(timestamps.words.len(), 2);
        assert_eq!(timestamps.words[0].start_ms, 0);
        assert_eq!(timestamps.words[1].end_ms, 1_250);
        assert_eq!(timestamps.segments.len(), 1);
        assert_eq!(timestamps.segments[0].text, "hello world");

        let request_body = captured.lock();
        assert!(request_body.contains("response_format"));
        assert!(request_body.contains("verbose_json"));
        assert!(request_body.contains("timestamp_granularities[]"));
        assert!(request_body.contains("word"));
        assert!(request_body.contains("segment"));
        Ok(())
    }

    #[tokio::test]
    async fn openai_audio_transcriber_posts_diarized_json_and_parses_speakers() -> Result<()> {
        let captured = Arc::new(Mutex::new(String::new()));
        let body = serde_json::json!({
            "text": "hello there welcome back",
            "segments": [
                {"speaker": "speaker_0", "text": "hello there", "start": 0.0, "end": 1.2},
                {"speaker": "speaker_1", "text": "welcome back", "start": 1.3, "end": 2.4}
            ]
        })
        .to_string();
        let base_url = spawn_mock_server(
            200,
            &[("Content-Type", "application/json")],
            &body,
            captured.clone(),
        )
        .await?;
        let mut config = OpenAiProviderConfig::new("gpt-4o-transcribe", "test-key");
        config.base_url = format!("{base_url}/v1");
        let transcriber = OpenAiAudioTranscriber::new(config)?;
        let response = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sample.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: b"RIFFxxxxWAVEfmt ".to_vec(),
                prompt: None,
                language: Some("en".to_string()),
                timestamp_granularities: Vec::new(),
                diarization: true,
            })
            .await?;

        assert_eq!(response.provider, "openai");
        assert_eq!(response.model, "gpt-4o-transcribe-diarize");
        assert_eq!(response.text, "hello there welcome back");
        let segments = response
            .timestamps
            .expect("diarized response should expose segments")
            .segments;
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].speaker.as_deref(), Some("speaker_0"));
        assert_eq!(segments[1].start_ms, 1_300);

        let request_body = captured.lock();
        assert!(request_body.contains("gpt-4o-transcribe-diarize"));
        assert!(request_body.contains("diarized_json"));
        assert!(request_body.contains("chunking_strategy"));
        assert!(request_body.contains("auto"));
        Ok(())
    }

    #[tokio::test]
    async fn openai_audio_transcriber_rejects_timestamps_for_non_whisper_model() -> Result<()> {
        let config = OpenAiProviderConfig::new("gpt-4o-transcribe", "test-key");
        let transcriber = OpenAiAudioTranscriber::new(config)?;

        let error = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sample.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: b"RIFFxxxxWAVEfmt ".to_vec(),
                prompt: None,
                language: None,
                timestamp_granularities: vec!["word".to_string()],
                diarization: false,
            })
            .await
            .expect_err("timestamp requests should require whisper-1");

        assert!(!error.retryable);
        assert!(
            error.message.contains("whisper-1"),
            "unexpected error: {}",
            error.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn openai_audio_transcriber_sanitizes_upstream_error_messages() -> Result<()> {
        let retry_after =
            httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(60));
        let body = serde_json::json!({
            "error": {
                "type": "rate_limit_error",
                "code": "rate_limit_exceeded",
                "message": "raw upstream echo: SECRET_PROMPT sk-test-secret"
            }
        })
        .to_string();
        let base_url = spawn_mock_server(
            429,
            &[
                ("Content-Type", "application/json"),
                ("Retry-After", retry_after.as_str()),
            ],
            &body,
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let mut config = OpenAiProviderConfig::new("gpt-4o-transcribe", "test-key");
        config.base_url = format!("{base_url}/v1");
        let transcriber = OpenAiAudioTranscriber::new(config)?;

        let error = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sample.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: b"RIFFxxxxWAVEfmt ".to_vec(),
                prompt: Some("SECRET_PROMPT".to_string()),
                language: None,
                timestamp_granularities: Vec::new(),
                diarization: false,
            })
            .await
            .expect_err("429 should fail");

        assert!(error.retryable);
        assert!(error.retry_after_ms.unwrap_or_default() > 0);
        assert!(error.message.contains("status 429"));
        assert!(error.message.contains("type=rate_limit_error"));
        assert!(error.message.contains("code=rate_limit_exceeded"));
        assert!(!error.message.contains("SECRET_PROMPT"));
        assert!(!error.message.contains("sk-test-secret"));
        Ok(())
    }

    #[tokio::test]
    async fn openai_audio_transcriber_rejects_oversized_response_body() -> Result<()> {
        let oversized = "x".repeat(MAX_OPENAI_TRANSCRIPTION_RESPONSE_BYTES + 1);
        let base_url = spawn_mock_server(
            200,
            &[("Content-Type", "text/plain")],
            &oversized,
            Arc::new(Mutex::new(String::new())),
        )
        .await?;
        let mut config = OpenAiProviderConfig::new("gpt-4o-transcribe", "test-key");
        config.base_url = format!("{base_url}/v1");
        let transcriber = OpenAiAudioTranscriber::new(config)?;

        let error = transcriber
            .transcribe(&AudioTranscriptionRequest {
                file_name: "sample.wav".to_string(),
                media_type: "audio/wav".to_string(),
                bytes: b"RIFFxxxxWAVEfmt ".to_vec(),
                prompt: None,
                language: None,
                timestamp_granularities: Vec::new(),
                diarization: false,
            })
            .await
            .expect_err("oversized response should fail");

        assert!(
            error.message.contains("byte limit"),
            "unexpected error: {}",
            error.message
        );
        Ok(())
    }
}
