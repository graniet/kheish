//! Audio transcription helpers shared across daemon state workflows.

use anyhow::{Result, anyhow, bail};
use sha2::Digest;

use super::derivation_workflow::derived_text_file_name;
use super::*;
use crate::derivations::NormalizedDerivationTranscriptionOptions;

pub(super) struct CanonicalTextAssetResult {
    pub(super) asset: StoredAssetRecord,
    pub(super) backend: Option<crate::derivations::DerivationBackendProvenance>,
    pub(super) timestamps: Option<kheish_runtime::AudioTranscriptionTimestamps>,
}

enum ExistingTranscriptionTextReuse {
    Reuse {
        backend: Option<crate::derivations::DerivationBackendProvenance>,
    },
    Regenerate,
}

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(super) async fn ensure_audio_input_parts_canonical_text(
        &self,
        parts: Vec<ResolvedInputPart>,
        preferred_route_id: Option<&str>,
        credential_scope: Option<&kheish_types::CredentialScope>,
    ) -> Result<Vec<ResolvedInputPart>> {
        let mut normalized = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                ResolvedInputPart::Text(text) => normalized.push(ResolvedInputPart::Text(text)),
                ResolvedInputPart::Asset(asset) => {
                    let _ = self
                        .ensure_asset_audio_canonical_text_derivation(
                            &asset,
                            preferred_route_id,
                            credential_scope,
                        )
                        .await?;
                    let refreshed = self.assets.get(&asset.id).unwrap_or(asset);
                    normalized.push(ResolvedInputPart::Asset(refreshed));
                }
            }
        }
        Ok(normalized)
    }

    pub(super) async fn ensure_asset_audio_canonical_text_derivation(
        &self,
        asset: &StoredAssetRecord,
        preferred_route_id: Option<&str>,
        credential_scope: Option<&kheish_types::CredentialScope>,
    ) -> Result<Option<StoredAssetRecord>> {
        let subject = DerivationSubject::Asset {
            asset_id: asset.id.clone(),
        };
        let subject_lock = self.canonical_text_subject_lock(&subject).await;
        let _subject_guard = subject_lock.lock().await;
        let asset = self.assets.get(&asset.id).unwrap_or_else(|| asset.clone());
        let result = self
            .ensure_asset_audio_canonical_text_with_provenance(
                &asset,
                preferred_route_id,
                credential_scope,
                None,
                Some(&subject),
                false,
            )
            .await?;
        if let Some(result) = result {
            let _ = self
                .record_completed_canonical_text_derivation_locked(
                    subject,
                    result.asset.id.clone(),
                    false,
                    result.backend.clone(),
                )
                .await?;
            return Ok(Some(result.asset));
        }
        Ok(None)
    }

    pub(super) async fn ensure_asset_audio_canonical_text_with_provenance(
        &self,
        asset: &StoredAssetRecord,
        preferred_route_id: Option<&str>,
        credential_scope: Option<&kheish_types::CredentialScope>,
        transcription_options: Option<&NormalizedDerivationTranscriptionOptions>,
        reuse_subject: Option<&DerivationSubject>,
        force_refresh: bool,
    ) -> Result<Option<CanonicalTextAssetResult>> {
        if !matches!(
            asset.media_type.as_str(),
            "audio/wav"
                | "audio/webm"
                | "audio/mpeg"
                | "audio/mp3"
                | "audio/mpga"
                | "audio/mp4"
                | "audio/x-m4a"
                | "audio/m4a"
        ) {
            anyhow::ensure!(
                transcription_options.is_none(),
                "transcription options can only be used with audio assets"
            );
            return Ok(None);
        }
        if !force_refresh
            && transcription_options.is_none()
            && let Some(text_uri) = asset.text_uri.as_deref()
            && let Some(existing) = self.assets.get_by_uri(text_uri)
        {
            anyhow::ensure!(
                existing.media_type == "text/plain",
                "asset {} text_uri points to non-text asset {}",
                asset.id,
                existing.id
            );
            if let ExistingTranscriptionTextReuse::Reuse { backend } = self
                .existing_transcription_text_reuse(
                    reuse_subject,
                    asset,
                    &existing,
                    preferred_route_id,
                    credential_scope,
                )
                .await?
            {
                return Ok(Some(CanonicalTextAssetResult {
                    asset: existing,
                    backend,
                    timestamps: None,
                }));
            }
        }
        let Some(transcription_service) = self.transcription_service.as_ref() else {
            anyhow::ensure!(
                transcription_options.is_none(),
                "transcription options require a configured audio transcription backend"
            );
            return Ok(None);
        };
        let (_, bytes) = self.assets.read_raw(&asset.id)?;
        if !force_refresh
            && transcription_options.is_none()
            && asset.media_type == "audio/wav"
            && wav_audio_payload_is_digital_silence(&bytes)?
        {
            tracing::info!(
                asset_id = %asset.id,
                "skipping automatic transcription for digital-silent WAV asset"
            );
            return Ok(None);
        }
        let transcription = transcription_service
            .transcribe_asset_with_route(
                asset,
                bytes,
                preferred_route_id,
                credential_scope,
                transcription_options,
            )
            .await?;
        let text = transcription.response.text.trim();
        if text.is_empty() {
            bail!("transcription for asset {} returned empty text", asset.id);
        }
        let derived = self.assets.import_bytes(
            &derived_text_file_name(&asset.file_name),
            Some("text/plain"),
            text.as_bytes(),
        )?;
        let timestamps = transcription.response.timestamps.clone();
        let timestamp_asset_id = if let Some(timestamps) = timestamps.as_ref() {
            let payload = serde_json::json!({
                "schema_version": 1,
                "kind": "audio_transcription_timestamps",
                "provider": &transcription.response.provider,
                "model": &transcription.response.model,
                "pipeline_version": transcription.pipeline_version,
                "stitching_strategy": &transcription.stitching_strategy,
                "part_count": transcription.part_count,
                "source_asset_id": &asset.id,
                "text_sha256": hex::encode(sha2::Sha256::digest(text.as_bytes())),
                "timestamps": timestamps,
            });
            let bytes = serde_json::to_vec_pretty(&payload)?;
            let asset = self.assets.import_bytes(
                &format!(
                    "{}.timestamps.json",
                    derived_text_file_name(&asset.file_name)
                ),
                Some("application/json"),
                &bytes,
            )?;
            Some(asset.id)
        } else {
            None
        };
        if transcription_options.is_none() {
            let _ = self.assets.attach_text_asset(&asset.id, &derived)?;
        }
        Ok(Some(CanonicalTextAssetResult {
            asset: derived,
            backend: Some(crate::derivations::DerivationBackendProvenance {
                kind: "transcription".to_string(),
                route_id: transcription.route_id,
                provider: transcription.response.provider,
                model: transcription.response.model,
                pipeline_version: transcription.pipeline_version,
                stitching_strategy: transcription.stitching_strategy,
                part_count: transcription.part_count,
                timestamp_asset_id,
            }),
            timestamps,
        }))
    }

    pub(super) async fn ensure_observation_audio_canonical_text_derivation(
        &self,
        observation: &ObservationView,
        preferred_route_id: Option<&str>,
        credential_scope: Option<&kheish_types::CredentialScope>,
    ) -> Result<Option<StoredAssetRecord>> {
        let subject = DerivationSubject::Observation {
            observation_id: observation.observation_id.clone(),
        };
        let subject_lock = self.canonical_text_subject_lock(&subject).await;
        let _subject_guard = subject_lock.lock().await;
        let observation = self
            .observation_service
            .get_observation(&observation.observation_id)
            .await?;
        let result = self
            .ensure_observation_audio_canonical_text_with_provenance(
                &observation,
                preferred_route_id,
                credential_scope,
                None,
                false,
            )
            .await?;
        if let Some(result) = result {
            let _ = self
                .record_completed_canonical_text_derivation_locked(
                    subject,
                    result.asset.id.clone(),
                    false,
                    result.backend.clone(),
                )
                .await?;
            return Ok(Some(result.asset));
        }
        Ok(None)
    }

    pub(super) async fn ensure_observation_audio_canonical_text_with_provenance(
        &self,
        observation: &ObservationView,
        preferred_route_id: Option<&str>,
        credential_scope: Option<&kheish_types::CredentialScope>,
        transcription_options: Option<&NormalizedDerivationTranscriptionOptions>,
        force_refresh: bool,
    ) -> Result<Option<CanonicalTextAssetResult>> {
        if !force_refresh
            && transcription_options.is_none()
            && let Some(canonical_text_asset_id) = observation.canonical_text_asset_id.as_deref()
        {
            let Some(asset) = self.assets.get(canonical_text_asset_id) else {
                return Err(anyhow!("unknown asset {canonical_text_asset_id}"));
            };
            anyhow::ensure!(
                asset.media_type == "text/plain",
                "observation {} canonical text asset must be text/plain",
                observation.observation_id
            );
            let raw_asset = self
                .assets
                .get(&observation.asset_id)
                .ok_or_else(|| anyhow!("unknown asset {}", observation.asset_id))?;
            let subject = DerivationSubject::Observation {
                observation_id: observation.observation_id.clone(),
            };
            if let ExistingTranscriptionTextReuse::Reuse { backend } = self
                .existing_transcription_text_reuse(
                    Some(&subject),
                    &raw_asset,
                    &asset,
                    preferred_route_id,
                    credential_scope,
                )
                .await?
            {
                return Ok(Some(CanonicalTextAssetResult {
                    asset,
                    backend,
                    timestamps: None,
                }));
            }
        }
        let asset = self
            .assets
            .get(&observation.asset_id)
            .ok_or_else(|| anyhow!("unknown asset {}", observation.asset_id))?;
        let derived = self
            .ensure_asset_audio_canonical_text_with_provenance(
                &asset,
                preferred_route_id,
                credential_scope,
                transcription_options,
                Some(&DerivationSubject::Observation {
                    observation_id: observation.observation_id.clone(),
                }),
                force_refresh,
            )
            .await?;
        if transcription_options.is_none()
            && let Some(text_asset) = derived.as_ref()
        {
            let _ = self
                .observation_service
                .set_canonical_text_asset(&observation.observation_id, &text_asset.asset.id)
                .await?;
        }
        Ok(derived)
    }

    async fn existing_transcription_text_reuse(
        &self,
        subject: Option<&DerivationSubject>,
        raw_asset: &StoredAssetRecord,
        text_asset: &StoredAssetRecord,
        preferred_route_id: Option<&str>,
        credential_scope: Option<&kheish_types::CredentialScope>,
    ) -> Result<ExistingTranscriptionTextReuse> {
        let Some(subject) = subject else {
            return Ok(ExistingTranscriptionTextReuse::Reuse { backend: None });
        };
        let Some(transcription_service) = self.transcription_service.as_ref() else {
            return Ok(ExistingTranscriptionTextReuse::Reuse { backend: None });
        };
        if !transcription_service.supports_media_type(&raw_asset.media_type) {
            return Ok(ExistingTranscriptionTextReuse::Reuse { backend: None });
        }
        let (planned_route_id, planned_identity) =
            transcription_service.planned_backend(preferred_route_id, credential_scope)?;
        let mut saw_subject_transcription = false;
        for derivation_id in &text_asset.derivation_ids {
            let Some(record) = self.derivation_service.get(derivation_id).await else {
                continue;
            };
            if record.status != DerivationStatus::Completed || &record.subject != subject {
                continue;
            }
            let Some(backend) = record
                .backend
                .as_ref()
                .filter(|backend| backend.kind == "transcription")
            else {
                continue;
            };
            saw_subject_transcription = true;
            if backend.route_id == planned_route_id
                && backend.provider == planned_identity.provider
                && backend.model == planned_identity.model
            {
                return Ok(ExistingTranscriptionTextReuse::Reuse {
                    backend: Some(backend.clone()),
                });
            }
        }
        if saw_subject_transcription {
            Ok(ExistingTranscriptionTextReuse::Regenerate)
        } else {
            Ok(ExistingTranscriptionTextReuse::Reuse { backend: None })
        }
    }
}

fn wav_audio_payload_is_digital_silence(bytes: &[u8]) -> Result<bool> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Ok(false);
    }

    let mut cursor = 12usize;
    while cursor.saturating_add(8) <= bytes.len() {
        let chunk_id = &bytes[cursor..cursor + 4];
        let chunk_len = u32::from_le_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        let data_start = cursor + 8;
        let data_end = data_start
            .checked_add(chunk_len)
            .ok_or_else(|| anyhow!("WAV chunk length overflow while scanning audio payload"))?;
        if data_end > bytes.len() {
            return Ok(false);
        }
        if chunk_id == b"data" {
            return Ok(chunk_len > 0 && bytes[data_start..data_end].iter().all(|byte| *byte == 0));
        }
        cursor = data_end + (chunk_len % 2);
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_digital_silent_wav_payload() -> Result<()> {
        let mut wav = minimal_pcm_wav(vec![0; 16]);
        assert!(wav_audio_payload_is_digital_silence(&wav)?);

        let data_offset = wav.len() - 8;
        wav[data_offset] = 1;
        assert!(!wav_audio_payload_is_digital_silence(&wav)?);
        Ok(())
    }

    #[test]
    fn ignores_non_wav_or_malformed_payloads() -> Result<()> {
        assert!(!wav_audio_payload_is_digital_silence(b"not a wav")?);
        assert!(!wav_audio_payload_is_digital_silence(
            b"RIFF\xff\xff\xff\xffWAVE"
        )?);
        Ok(())
    }

    fn minimal_pcm_wav(data: Vec<u8>) -> Vec<u8> {
        let data_len = data.len() as u32;
        let riff_len = 36 + data_len;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&riff_len.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&48_000u32.to_le_bytes());
        wav.extend_from_slice(&96_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.extend_from_slice(&data);
        wav
    }
}
