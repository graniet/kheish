//! Derivation workflow methods on [`DaemonState`].

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use sha2::{Digest, Sha256};

use kheish_types::SessionEvent;

use super::*;
use crate::derivations::NormalizedDerivationTranscriptionOptions;
use crate::{DerivationCacheStatus, ObservationSourceStatus};

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(crate) async fn list_derivations(
        &self,
        query: Option<&str>,
    ) -> Result<Vec<DerivationView>> {
        Ok(self
            .derivation_service
            .list(query)
            .await
            .into_iter()
            .map(DerivationView::from)
            .collect())
    }

    pub(crate) async fn get_derivation(&self, derivation_id: &str) -> Result<DerivationView> {
        let derivation = self
            .derivation_service
            .get(derivation_id)
            .await
            .ok_or_else(|| anyhow!("unknown derivation {derivation_id}"))?;
        Ok(DerivationView::from(derivation))
    }

    pub(crate) async fn create_derivation(
        &self,
        request: DerivationCreateRequest,
        controls: DerivationCreateControls,
    ) -> Result<DerivationView> {
        request.validate()?;
        let profile = request.profile.clone();
        let subject = request.subject.clone();
        let transcription_options = request.normalized_transcription_options()?;

        if profile == DerivationProfile::CanonicalText {
            let subject_lock = self.canonical_text_subject_lock(&subject).await;
            let _subject_guard = subject_lock.lock().await;
            let resolved = self.resolve_derivation_subject(&subject).await?;
            let profile_version = profile.version();
            let source_fingerprint = self
                .source_fingerprint_with_transcription_options(
                    &resolved,
                    transcription_options.as_ref(),
                    None,
                )
                .await?;
            let cache_key =
                derivation_cache_key(&profile, profile_version, &subject, &source_fingerprint);
            let existing = self.derivation_service.get_by_cache_key(&cache_key).await;
            let bypass_existing_cache = controls.force_refresh
                || existing
                    .as_ref()
                    .is_some_and(|existing| should_retry_failed_derivation(existing, controls));
            if !bypass_existing_cache && let Some(existing) = existing {
                return Ok(
                    DerivationView::from(existing).with_cache_status(DerivationCacheStatus::Hit)
                );
            }
            let planned_backend = self.planned_transcription_backend(&resolved)?;

            let execution = self
                .execute_derivation_profile(
                    &profile,
                    &resolved,
                    transcription_options.as_ref(),
                    controls.force_refresh,
                )
                .await;
            let execution = match execution {
                Ok(execution) => execution,
                Err(error) if should_persist_derivation_failure(&error) => {
                    let detail = error.to_string();
                    let record = StoredDerivationRecord {
                        derivation_id: self.derivation_service.next_derivation_id(),
                        profile,
                        profile_version,
                        subject,
                        source_fingerprint,
                        status: DerivationStatus::Failed,
                        result_asset_id: String::new(),
                        reused_subject_asset: false,
                        error: Some(detail.clone()),
                        backend: planned_backend,
                        created_at_ms: now_ms(),
                    };
                    let _ = self.derivation_service.create(record).await?;
                    return Err(anyhow!(detail));
                }
                Err(error) => return Err(error),
            };

            let final_resolved = self.resolve_derivation_subject(&subject).await?;
            let final_source_fingerprint = self
                .source_fingerprint_with_transcription_options(
                    &final_resolved,
                    transcription_options.as_ref(),
                    execution.backend.as_ref(),
                )
                .await?;
            return self
                .record_completed_derivation_locked(
                    profile,
                    profile_version,
                    subject,
                    final_source_fingerprint,
                    execution.result_asset_id,
                    execution.reused_subject_asset,
                    execution.backend,
                    bypass_existing_cache,
                )
                .await;
        }

        let resolved = self.resolve_derivation_subject(&subject).await?;
        let profile_version = profile.version();
        let source_fingerprint = resolved.source_fingerprint().to_string();
        let cache_key =
            derivation_cache_key(&profile, profile_version, &subject, &source_fingerprint);
        let creation_lock = self
            .derivation_service
            .creation_lock_for_cache_key(&cache_key)
            .await;
        let _guard = creation_lock.lock().await;
        let existing = self.derivation_service.get_by_cache_key(&cache_key).await;
        let bypass_existing_cache = controls.force_refresh
            || existing
                .as_ref()
                .is_some_and(|existing| should_retry_failed_derivation(existing, controls));
        if !bypass_existing_cache && let Some(existing) = existing {
            return Ok(DerivationView::from(existing).with_cache_status(DerivationCacheStatus::Hit));
        }

        let execution = self
            .execute_derivation_profile(&profile, &resolved, None, controls.force_refresh)
            .await;
        let execution = match execution {
            Ok(execution) => execution,
            Err(error) if should_persist_derivation_failure(&error) => {
                let detail = error.to_string();
                let record = StoredDerivationRecord {
                    derivation_id: self.derivation_service.next_derivation_id(),
                    profile,
                    profile_version,
                    subject,
                    source_fingerprint,
                    status: DerivationStatus::Failed,
                    result_asset_id: String::new(),
                    reused_subject_asset: false,
                    error: Some(detail.clone()),
                    backend: None,
                    created_at_ms: now_ms(),
                };
                let _ = self.derivation_service.create(record).await?;
                return Err(anyhow!(detail));
            }
            Err(error) => return Err(error),
        };

        self.record_completed_derivation_locked(
            profile,
            profile_version,
            subject,
            source_fingerprint,
            execution.result_asset_id,
            execution.reused_subject_asset,
            execution.backend,
            bypass_existing_cache,
        )
        .await
    }

    pub(super) async fn canonical_text_subject_lock(
        &self,
        subject: &DerivationSubject,
    ) -> Arc<tokio::sync::Mutex<()>> {
        self.derivation_service
            .creation_lock_for_key(&format!("canonical_text_subject:{}", subject.stable_key()))
            .await
    }

    pub(super) async fn record_completed_canonical_text_derivation_locked(
        &self,
        subject: DerivationSubject,
        result_asset_id: String,
        reused_subject_asset: bool,
        backend: Option<crate::derivations::DerivationBackendProvenance>,
    ) -> Result<DerivationView> {
        anyhow::ensure!(
            !result_asset_id.trim().is_empty(),
            "result_asset_id is required for completed canonical_text derivations"
        );
        let request = DerivationCreateRequest {
            profile: DerivationProfile::CanonicalText,
            subject,
            transcription: None,
        };
        request.validate()?;
        let resolved = self.resolve_derivation_subject(&request.subject).await?;
        let profile_version = request.profile.version();
        let source_fingerprint = if backend.is_some() {
            self.source_fingerprint_with_transcription_options(&resolved, None, backend.as_ref())
                .await?
        } else {
            resolved.source_fingerprint().to_string()
        };
        self.record_completed_derivation_locked(
            request.profile,
            profile_version,
            request.subject,
            source_fingerprint,
            result_asset_id,
            reused_subject_asset,
            backend,
            false,
        )
        .await
    }

    async fn record_completed_derivation_locked(
        &self,
        profile: DerivationProfile,
        profile_version: u32,
        subject: DerivationSubject,
        source_fingerprint: String,
        result_asset_id: String,
        reused_subject_asset: bool,
        backend: Option<crate::derivations::DerivationBackendProvenance>,
        bypass_existing_cache: bool,
    ) -> Result<DerivationView> {
        let cache_key =
            derivation_cache_key(&profile, profile_version, &subject, &source_fingerprint);
        if !bypass_existing_cache
            && let Some(existing) = self.derivation_service.get_by_cache_key(&cache_key).await
        {
            if existing.status == DerivationStatus::Completed
                && !existing.result_asset_id.is_empty()
            {
                anyhow::ensure!(
                    existing.result_asset_id == result_asset_id,
                    "derivation cache key {cache_key} already points at result asset {}, not {result_asset_id}",
                    existing.result_asset_id
                );
                let _ = self
                    .assets
                    .attach_derivation(&existing.result_asset_id, &existing.derivation_id)?;
            }
            return Ok(DerivationView::from(existing).with_cache_status(DerivationCacheStatus::Hit));
        }

        let record = StoredDerivationRecord {
            derivation_id: self.derivation_service.next_derivation_id(),
            profile,
            profile_version,
            subject,
            source_fingerprint,
            status: DerivationStatus::Completed,
            result_asset_id,
            reused_subject_asset,
            error: None,
            backend,
            created_at_ms: now_ms(),
        };
        let created = self.derivation_service.create(record).await?;
        let _ = self
            .assets
            .attach_derivation(&created.result_asset_id, &created.derivation_id)?;
        if let Some(timestamp_asset_id) = created
            .backend
            .as_ref()
            .and_then(|backend| backend.timestamp_asset_id.as_deref())
        {
            let _ = self
                .assets
                .attach_derivation(timestamp_asset_id, &created.derivation_id)?;
        }
        Ok(DerivationView::from(created).with_cache_status(DerivationCacheStatus::Miss))
    }

    async fn execute_derivation_profile(
        &self,
        profile: &DerivationProfile,
        resolved: &ResolvedDerivationSubject,
        transcription_options: Option<&NormalizedDerivationTranscriptionOptions>,
        force_refresh: bool,
    ) -> Result<DerivationExecution> {
        let (result_asset_id, reused_subject_asset, backend) = match (profile, resolved) {
            (DerivationProfile::CanonicalText, ResolvedDerivationSubject::Asset { asset, .. }) => {
                if let Some(derived) = self
                    .ensure_asset_audio_canonical_text_with_provenance(
                        asset,
                        None,
                        None,
                        transcription_options,
                        Some(&DerivationSubject::Asset {
                            asset_id: asset.id.clone(),
                        }),
                        force_refresh,
                    )
                    .await?
                {
                    (derived.asset.id, false, derived.backend)
                } else {
                    let text = if let Some(text) = self.assets.read_text(&asset.id)? {
                        text
                    } else {
                        self.assets.render_asset_transcript_part(asset)?
                    };
                    let file_name = derived_text_file_name(&asset.file_name);
                    let derived = self.assets.import_bytes(
                        &file_name,
                        Some("text/plain"),
                        text.as_bytes(),
                    )?;
                    (derived.id, false, None)
                }
            }
            (
                DerivationProfile::CanonicalText,
                ResolvedDerivationSubject::Observation {
                    observation, asset, ..
                },
            ) => {
                if let Some(derived) = self
                    .ensure_observation_audio_canonical_text_with_provenance(
                        observation,
                        None,
                        None,
                        transcription_options,
                        force_refresh,
                    )
                    .await?
                {
                    (derived.asset.id, false, derived.backend)
                } else if let Some(canonical_text_asset_id) =
                    observation.canonical_text_asset_id.as_ref()
                    && transcription_options.is_none()
                {
                    let derived = self
                        .assets
                        .get(canonical_text_asset_id)
                        .ok_or_else(|| anyhow!("unknown asset {canonical_text_asset_id}"))?;
                    anyhow::ensure!(
                        derived.media_type == "text/plain",
                        "observation {} canonical text asset must be text/plain",
                        observation.observation_id
                    );
                    (derived.id, false, None)
                } else {
                    let text = if let Some(text) = self.assets.read_text(&asset.id)? {
                        text
                    } else {
                        self.assets.render_asset_transcript_part(asset)?
                    };
                    let file_name = format!(
                        "{}.canonical.txt",
                        slug_fragment(&observation.observation_id)
                    );
                    let derived = self.assets.import_bytes(
                        &file_name,
                        Some("text/plain"),
                        text.as_bytes(),
                    )?;
                    (derived.id, false, None)
                }
            }
            (
                DerivationProfile::CanonicalText,
                ResolvedDerivationSubject::SessionInput {
                    session_id,
                    offset,
                    input,
                    ..
                },
            ) => {
                anyhow::ensure!(
                    transcription_options.is_none(),
                    "transcription options are not supported for session input derivations"
                );
                let text = render_session_input_canonical_text(input)?;
                let file_name = format!(
                    "{}-input-{}.canonical.txt",
                    slug_fragment(session_id),
                    offset
                );
                let derived =
                    self.assets
                        .import_bytes(&file_name, Some("text/plain"), text.as_bytes())?;
                (derived.id, false, None)
            }
            (DerivationProfile::VisualPreview, ResolvedDerivationSubject::Asset { asset, .. }) => {
                if asset.is_image() {
                    (asset.id.clone(), true, None)
                } else if let Some((media_type, bytes)) =
                    self.assets.read_preview_image(&asset.id)?
                {
                    let file_name = derived_preview_file_name(&asset.file_name, &media_type);
                    let derived =
                        self.assets
                            .import_bytes(&file_name, Some(&media_type), &bytes)?;
                    (derived.id, false, None)
                } else {
                    bail!(
                        "asset {} does not expose a visual preview for profile {}",
                        asset.id,
                        DerivationProfile::VisualPreview.as_str()
                    );
                }
            }
            (DerivationProfile::VisualPreview, ResolvedDerivationSubject::SessionInput { .. }) => {
                bail!("session input subjects do not expose a visual preview")
            }
            (
                DerivationProfile::VisualPreview,
                ResolvedDerivationSubject::Observation {
                    observation, asset, ..
                },
            ) => {
                if asset.is_image() {
                    (asset.id.clone(), true, None)
                } else if let Some((media_type, bytes)) =
                    self.assets.read_preview_image(&asset.id)?
                {
                    let file_name = derived_preview_file_name(&asset.file_name, &media_type);
                    let derived =
                        self.assets
                            .import_bytes(&file_name, Some(&media_type), &bytes)?;
                    (derived.id, false, None)
                } else {
                    bail!(
                        "observation {} does not expose a visual preview for profile {}",
                        observation.observation_id,
                        DerivationProfile::VisualPreview.as_str()
                    );
                }
            }
        };

        Ok(DerivationExecution {
            result_asset_id,
            reused_subject_asset,
            backend,
        })
    }

    async fn resolve_derivation_subject(
        &self,
        subject: &DerivationSubject,
    ) -> Result<ResolvedDerivationSubject> {
        match subject {
            DerivationSubject::Asset { asset_id } => {
                let asset = self
                    .assets
                    .get(asset_id)
                    .ok_or_else(|| anyhow!("unknown asset {asset_id}"))?;
                let canonical_fingerprint = self.asset_canonical_text_fingerprint(&asset)?;
                Ok(ResolvedDerivationSubject::Asset {
                    source_fingerprint: format!(
                        "asset:v2:{}:{}{}",
                        asset.media_type, asset.sha256, canonical_fingerprint
                    ),
                    asset,
                })
            }
            DerivationSubject::Observation { observation_id } => {
                self.enforce_observation_retention().await?;
                let observation = self
                    .observation_service
                    .get_observation(observation_id)
                    .await?;
                anyhow::ensure!(
                    observation.is_active(),
                    "observation {observation_id} is no longer materializable"
                );
                let source = self
                    .observation_service
                    .get_source(&observation.source_id)
                    .await?;
                anyhow::ensure!(
                    source.allow_materialization,
                    "observation source {} does not allow materialization",
                    source.source_id
                );
                anyhow::ensure!(
                    source.status != ObservationSourceStatus::Disabled,
                    "observation source {} is disabled",
                    source.source_id
                );
                let asset = self
                    .assets
                    .get(&observation.asset_id)
                    .ok_or_else(|| anyhow!("unknown asset {}", observation.asset_id))?;
                let canonical_fingerprint = observation
                    .canonical_text_asset_id
                    .as_deref()
                    .and_then(|asset_id| self.assets.get(asset_id))
                    .map(|asset| format!(":canonical:{}:{}", asset.media_type, asset.sha256))
                    .unwrap_or_default();
                Ok(ResolvedDerivationSubject::Observation {
                    source_fingerprint: format!(
                        "observation:v2:{}:raw:{}:{}{}",
                        observation.request_fingerprint,
                        asset.media_type,
                        asset.sha256,
                        canonical_fingerprint
                    ),
                    observation,
                    asset,
                })
            }
            DerivationSubject::SessionInput { session_id, offset } => {
                if self
                    .session_service
                    .session_agent_id(session_id)
                    .await
                    .is_none()
                {
                    bail!("unknown session {session_id}");
                }
                let session = self.session_service.load_session(session_id).await?;
                let input = session
                    .journal
                    .iter()
                    .find_map(|entry| {
                        (entry.offset == *offset).then_some(&entry.event).and_then(|event| match event
                        {
                            SessionEvent::InputReceived { input } => Some(input.clone()),
                            _ => None,
                        })
                    })
                    .ok_or_else(|| {
                        anyhow!(
                            "session {session_id} does not contain an InputReceived event at offset {offset}"
                        )
                    })?;
                let canonical = render_session_input_canonical_text(&input)?;
                let source_fingerprint = hex::encode(Sha256::digest(canonical.as_bytes()));
                Ok(ResolvedDerivationSubject::SessionInput {
                    source_fingerprint,
                    session_id: session_id.clone(),
                    offset: *offset,
                    input,
                })
            }
        }
    }

    fn asset_canonical_text_fingerprint(&self, asset: &StoredAssetRecord) -> Result<String> {
        let Some(text) = self.assets.read_text(&asset.id)? else {
            return Ok(String::new());
        };
        Ok(format!(
            ":canonical:text/plain:{}",
            hex::encode(Sha256::digest(text.as_bytes()))
        ))
    }

    async fn source_fingerprint_with_transcription_options(
        &self,
        resolved: &ResolvedDerivationSubject,
        transcription_options: Option<&NormalizedDerivationTranscriptionOptions>,
        backend: Option<&crate::derivations::DerivationBackendProvenance>,
    ) -> Result<String> {
        let (asset, base_fingerprint) = match (resolved, transcription_options.is_some()) {
            (ResolvedDerivationSubject::Asset { asset, .. }, false) => {
                (asset, resolved.source_fingerprint().to_string())
            }
            (ResolvedDerivationSubject::Observation { asset, .. }, false) => {
                (asset, resolved.source_fingerprint().to_string())
            }
            (ResolvedDerivationSubject::Asset { asset, .. }, true) => {
                let base = format!("asset:v2:{}:{}", asset.media_type, asset.sha256);
                (asset, base)
            }
            (
                ResolvedDerivationSubject::Observation {
                    observation, asset, ..
                },
                true,
            ) => {
                let base = format!(
                    "observation:v2:{}:raw:{}:{}",
                    observation.request_fingerprint, asset.media_type, asset.sha256
                );
                (asset, base)
            }
            (ResolvedDerivationSubject::SessionInput { .. }, _) => {
                if transcription_options.is_some() {
                    bail!("transcription options are not supported for session input derivations");
                }
                return Ok(resolved.source_fingerprint().to_string());
            }
        };
        let Some(transcription_service) = self.transcription_service.as_ref() else {
            if transcription_options.is_none() {
                return Ok(resolved.source_fingerprint().to_string());
            }
            bail!("transcription options require a configured audio transcription backend");
        };
        if !transcription_service.supports_media_type(&asset.media_type) {
            if transcription_options.is_none() {
                return Ok(resolved.source_fingerprint().to_string());
            }
            bail!("transcription options can only be used with audio assets");
        }
        let default_options;
        let options = if let Some(options) = transcription_options {
            options
        } else {
            default_options = NormalizedDerivationTranscriptionOptions::default();
            &default_options
        };
        let (route_id, provider, model) = if let Some(backend) =
            backend.filter(|backend| backend.kind == "transcription")
        {
            let mut model = backend.model.clone();
            if options.diarization() {
                anyhow::ensure!(
                    backend.provider == "openai",
                    "transcription speaker diarization requires an OpenAI transcription backend"
                );
                model = kheish_runtime::resolve_openai_transcription_request_model(&model, true);
            }
            (backend.route_id.clone(), backend.provider.clone(), model)
        } else {
            let (route_id, identity) =
                transcription_service.planned_backend_for_options(None, None, Some(options))?;
            (route_id, identity.provider, identity.model)
        };
        let options_digest = options.cache_digest_with_backend(&route_id, &provider, &model);
        Ok(format!(
            "{base_fingerprint}:transcription:{}:{options_digest}",
            crate::transcription::TRANSCRIPTION_CACHE_VERSION
        ))
    }

    fn planned_transcription_backend(
        &self,
        resolved: &ResolvedDerivationSubject,
    ) -> Result<Option<crate::derivations::DerivationBackendProvenance>> {
        let asset = match resolved {
            ResolvedDerivationSubject::Asset { asset, .. }
            | ResolvedDerivationSubject::Observation { asset, .. } => asset,
            ResolvedDerivationSubject::SessionInput { .. } => return Ok(None),
        };
        let Some(transcription_service) = self.transcription_service.as_ref() else {
            return Ok(None);
        };
        if !transcription_service.supports_media_type(&asset.media_type) {
            return Ok(None);
        }
        let (route_id, identity) = transcription_service.planned_backend(None, None)?;
        Ok(Some(crate::derivations::DerivationBackendProvenance {
            kind: "transcription".to_string(),
            route_id,
            provider: identity.provider,
            model: identity.model,
            pipeline_version: crate::transcription::TRANSCRIPTION_PIPELINE_VERSION,
            stitching_strategy: crate::transcription::TRANSCRIPTION_STITCHING_STRATEGY_SINGLE_PART
                .to_string(),
            part_count: crate::transcription::TRANSCRIPTION_SINGLE_PART_COUNT,
            timestamp_asset_id: None,
        }))
    }
}

enum ResolvedDerivationSubject {
    Asset {
        source_fingerprint: String,
        asset: StoredAssetRecord,
    },
    Observation {
        source_fingerprint: String,
        observation: ObservationView,
        asset: StoredAssetRecord,
    },
    SessionInput {
        source_fingerprint: String,
        session_id: String,
        offset: u64,
        input: InputEnvelope,
    },
}

struct DerivationExecution {
    result_asset_id: String,
    reused_subject_asset: bool,
    backend: Option<crate::derivations::DerivationBackendProvenance>,
}

impl ResolvedDerivationSubject {
    fn source_fingerprint(&self) -> &str {
        match self {
            Self::Asset {
                source_fingerprint, ..
            }
            | Self::Observation {
                source_fingerprint, ..
            }
            | Self::SessionInput {
                source_fingerprint, ..
            } => source_fingerprint,
        }
    }
}

fn should_retry_failed_derivation(
    existing: &StoredDerivationRecord,
    controls: DerivationCreateControls,
) -> bool {
    controls.retry_failed && existing.status == DerivationStatus::Failed
}

fn should_persist_derivation_failure(error: &anyhow::Error) -> bool {
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<kheish_runtime::ProviderError>()
            .is_some_and(|provider_error| provider_error.retryable)
    }) {
        return false;
    }
    let message = error.to_string();
    ![
        "does not expose a visual preview",
        "do not expose a visual preview",
        "unknown asset",
        "unknown observation",
        "unknown session",
        "is no longer materializable",
        "does not allow materialization",
        "observation source",
        "is disabled",
        "is required",
        "does not contain an InputReceived event",
        "is too large for audio transcription",
        "is not supported for audio transcription",
        "audio transcription payload",
        "audio transcription WAV",
        "audio transcription WebM",
        "audio transcription ID3",
        "audio transcription MP3",
        "audio transcription MP4/M4A",
        "audio media type",
        "invalid audio media type",
        "transcription timestamp granularit",
        "credential scope blocks all transcription routes",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn render_session_input_canonical_text(input: &InputEnvelope) -> Result<String> {
    let rendered = match &input.payload {
        InputPayload::Text { content } => content.trim().to_string(),
        InputPayload::Rich {
            rendered_content, ..
        } => rendered_content.trim().to_string(),
        InputPayload::Json { value } => serde_json::to_string_pretty(value)?,
        InputPayload::Event { name, value } => {
            format!("Event: {name}\n{}", serde_json::to_string_pretty(value)?)
        }
        InputPayload::Command { name, arguments } => format!(
            "Command: {name}\n{}",
            serde_json::to_string_pretty(arguments)?
        ),
    };
    Ok(rendered.replace("\r\n", "\n").trim().to_string())
}

fn slug_fragment(value: &str) -> String {
    let mut slug = String::with_capacity(value.len());
    let mut previous_dash = false;
    for ch in value.chars() {
        let keep = ch.is_ascii_alphanumeric();
        if keep {
            previous_dash = false;
            slug.push(ch.to_ascii_lowercase());
        } else if !previous_dash {
            previous_dash = true;
            slug.push('-');
        }
    }
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "input".to_string()
    } else {
        trimmed.to_string()
    }
}

pub(super) fn derived_text_file_name(file_name: &str) -> String {
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .map(slug_fragment)
        .unwrap_or_else(|| "asset".to_string());
    format!("{stem}.canonical.txt")
}

fn derived_preview_file_name(file_name: &str, media_type: &str) -> String {
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .map(slug_fragment)
        .unwrap_or_else(|| "asset".to_string());
    let extension = match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        _ => "bin",
    };
    format!("{stem}.preview.{extension}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_session_input_canonical_text_supports_every_payload_shape() -> Result<()> {
        let session_id = "demo";
        assert_eq!(
            render_session_input_canonical_text(&InputEnvelope::text(
                "http", "request", session_id, "user", "hello"
            ))?,
            "hello"
        );
        assert!(
            render_session_input_canonical_text(&InputEnvelope {
                source: SourceRef {
                    plugin: "http".to_string(),
                    kind: "request".to_string(),
                },
                conversation: ConversationKey {
                    session_id: session_id.to_string(),
                    thread_id: None,
                },
                actor: ActorRef {
                    id: "user".to_string(),
                    display_name: None,
                },
                payload: InputPayload::Event {
                    name: "tick".to_string(),
                    value: json!({"ok": true}),
                },
                attachments: Vec::new(),
                metadata: Value::Null,
                reply_targets: Vec::new(),
                reply: None,
            })?
            .contains("Event: tick")
        );
        Ok(())
    }

    #[test]
    fn derivation_retryable_provider_failures_are_not_persisted() {
        let error = anyhow::Error::new(kheish_runtime::ProviderError {
            message: "provider rate limited".to_string(),
            retryable: true,
            retry_after_ms: Some(1_000),
        })
        .context("transcription backend failed");

        assert!(!should_persist_derivation_failure(&error));
    }

    #[test]
    fn derivation_audio_preflight_failures_are_not_persisted() {
        for message in [
            "audio transcription payload is not a valid MP3/MPEG audio stream",
            "audio transcription MP3 preflight failed: MP3/MPEG payload does not contain audio frames",
            "audio transcription MP4/M4A preflight failed: MP4/M4A payload does not contain an audio track",
            "audio transcription WebM payload is missing DocType webm",
            "asset asset-1 is too large for audio transcription",
            "credential scope blocks all transcription routes",
        ] {
            assert!(
                !should_persist_derivation_failure(&anyhow!(message)),
                "message should not be persisted: {message}"
            );
        }
    }
}
