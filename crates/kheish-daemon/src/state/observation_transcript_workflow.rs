//! Observation transcript job workflow methods implemented on `DaemonState`.

use super::*;
use crate::derivations::NormalizedDerivationTranscriptionOptions;
use crate::observation_transcripts::{
    AudioTranscriptChunk, ObservationTranscriptCreateRequest, ObservationTranscriptJobRecord,
    ObservationTranscriptPhase, ObservationTranscriptSegmentView, ObservationTranscriptStatus,
    audio_transcript_chunk_from_items, concatenate_pcm_wav_payloads, is_audio_observation,
    split_pcm_wav_payload, wav_duration_ms,
};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use kheish_runtime::AudioTranscriptionTimestamps;
use kheish_runtime::redact_text;
use std::collections::{BTreeMap, BTreeSet};
use tokio::task::JoinHandle;

const OBSERVATION_TRANSCRIPT_WORKER_RETRY_BACKOFF_MS: u64 = 2_500;
const OBSERVATION_TRANSCRIPT_ERROR_MAX_CHARS: usize = 2_000;
const OBSERVATION_TRANSCRIPT_SPEAKER_LABEL_MAX_CHARS: usize = 80;
const OBSERVATION_TRANSCRIPT_SPEAKER_KEY_MAX_CHARS: usize = 128;
const DEFAULT_OBSERVATION_TRANSCRIPT_CHUNK_CONCURRENCY: usize = 4;
const MAX_OBSERVATION_TRANSCRIPT_CHUNK_CONCURRENCY: usize = 16;

struct CanonicalTranscriptAsset {
    id: String,
    timestamps: Option<AudioTranscriptionTimestamps>,
}

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(crate) fn spawn_observation_transcript_worker(
        self: &Arc<Self>,
    ) -> tokio::task::JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            state.observation_transcript_worker_loop().await;
        })
    }

    pub(crate) async fn restore_observation_transcript_worker_on_boot(
        self: &Arc<Self>,
    ) -> Result<()> {
        let mut jobs = self.observation_transcript_jobs.lock().await;
        let mut changed = Vec::new();
        for record in jobs.values_mut() {
            if record.view.status == ObservationTranscriptStatus::Running {
                record.view.status = ObservationTranscriptStatus::Queued;
                record.view.phase = ObservationTranscriptPhase::Selecting;
                record.view.progress.completed_chunks = 0;
                record.view.progress.failed_chunks = 0;
                record.view.progress.skipped_chunks = 0;
                record.view.progress.completed_observations = 0;
                record.view.progress.failed_observations = 0;
                record.view.artifacts.clear();
                record.view.segments.clear();
                record.view.error = None;
                record.view.updated_at_ms = now_ms();
                record.view.started_at_ms = None;
                record.view.finished_at_ms = None;
                changed.push(record.clone());
            }
        }
        drop(jobs);
        for record in changed {
            self.observation_transcript_store.save_job(&record)?;
        }
        self.observation_transcript_notify.notify_waiters();
        Ok(())
    }

    pub(crate) async fn create_observation_transcript_job(
        self: &Arc<Self>,
        request: ObservationTranscriptCreateRequest,
    ) -> Result<ObservationTranscriptJobView> {
        let request = request.normalized();
        request.validate()?;
        if request.transcription.diarization {
            let Some(transcription_service) = self.transcription_service.as_ref() else {
                anyhow::bail!(
                    "transcription speaker diarization requires a configured OpenAI transcription backend"
                );
            };
            transcription_service.planned_backend_for_options(
                request.transcription.route_id.as_deref(),
                None,
                Some(&NormalizedDerivationTranscriptionOptions {
                    prompt: None,
                    language: None,
                    timestamp_granularities: Vec::new(),
                    diarization: true,
                }),
            )?;
        }
        let request_fingerprint = request.fingerprint()?;
        let mut idempotency = self.observation_transcript_idempotency.lock().await;
        if let Some(existing_job_id) = idempotency.get(&request.idempotency_key).cloned() {
            let jobs = self.observation_transcript_jobs.lock().await;
            let existing = jobs
                .get(&existing_job_id)
                .ok_or_else(|| anyhow!("unknown observation transcript job {existing_job_id}"))?;
            anyhow::ensure!(
                existing.request_fingerprint == request_fingerprint,
                "idempotency key is already bound to observation transcript job {} with different request payload",
                existing.view.transcript_job_id
            );
            return Ok(existing.view.clone());
        }

        let transcript_job_id = self.next_observation_transcript_job_id();
        let mut record = ObservationTranscriptJobRecord::queued(
            transcript_job_id.clone(),
            request,
            request_fingerprint,
        );
        let selected = self
            .observation_service
            .resolve_transcript_selection(&record.view.selection)
            .await;
        let selected_audio_count = selected
            .iter()
            .filter(|view| is_audio_observation(view))
            .count();
        record.selected_observation_ids = selected
            .iter()
            .map(|observation| observation.observation_id.clone())
            .collect();
        record.view.progress.total_observations = record.selected_observation_ids.len() as u64;
        record.view.progress.selected_audio_observations = selected_audio_count as u64;
        record.view.progress.skipped_observations = record
            .selected_observation_ids
            .len()
            .saturating_sub(selected_audio_count)
            as u64;
        self.observation_transcript_store.save_job(&record)?;
        idempotency.insert(
            record.view.idempotency_key.clone(),
            record.view.transcript_job_id.clone(),
        );
        drop(idempotency);
        self.observation_transcript_jobs
            .lock()
            .await
            .insert(transcript_job_id, record.clone());
        self.observation_transcript_notify.notify_waiters();
        Ok(record.view)
    }

    pub(crate) async fn list_observation_transcript_jobs(
        &self,
        capture_group_id: Option<&str>,
        recording_id: Option<&str>,
        status: Option<ObservationTranscriptStatus>,
    ) -> Vec<ObservationTranscriptJobView> {
        let mut jobs = self
            .observation_transcript_jobs
            .lock()
            .await
            .values()
            .filter(|record| {
                capture_group_id.is_none_or(|capture_group_id| {
                    record.view.selection.capture_group_id == capture_group_id
                }) && recording_id.is_none_or(|recording_id| {
                    record.view.selection.recording_id.as_deref() == Some(recording_id)
                }) && status
                    .as_ref()
                    .is_none_or(|status| &record.view.status == status)
            })
            .map(|record| record.view.clone())
            .collect::<Vec<_>>();
        jobs.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.transcript_job_id.cmp(&right.transcript_job_id))
        });
        jobs
    }

    pub(crate) async fn get_observation_transcript_job(
        &self,
        transcript_job_id: &str,
    ) -> Result<ObservationTranscriptJobView> {
        self.observation_transcript_jobs
            .lock()
            .await
            .get(transcript_job_id)
            .map(|record| {
                let mut view = record.view.clone();
                sort_observation_transcript_segments(&mut view.segments);
                view
            })
            .ok_or_else(|| anyhow!("unknown observation transcript job {transcript_job_id}"))
    }

    pub(crate) async fn list_observation_transcript_segments(
        &self,
        transcript_job_id: &str,
    ) -> Result<Vec<ObservationTranscriptSegmentView>> {
        let jobs = self.observation_transcript_jobs.lock().await;
        let record = jobs
            .get(transcript_job_id)
            .ok_or_else(|| anyhow!("unknown observation transcript job {transcript_job_id}"))?;
        let mut segments = record.view.segments.clone();
        sort_observation_transcript_segments(&mut segments);
        Ok(segments)
    }

    pub(crate) async fn retry_observation_transcript_job(
        self: &Arc<Self>,
        transcript_job_id: &str,
    ) -> Result<ObservationTranscriptJobView> {
        let mut jobs = self.observation_transcript_jobs.lock().await;
        let record = jobs
            .get_mut(transcript_job_id)
            .ok_or_else(|| anyhow!("unknown observation transcript job {transcript_job_id}"))?;
        if record.view.status == ObservationTranscriptStatus::Completed
            || record.view.status == ObservationTranscriptStatus::Queued
            || record.view.status == ObservationTranscriptStatus::Running
        {
            return Ok(record.view.clone());
        }
        record.view.status = ObservationTranscriptStatus::Queued;
        record.view.phase = ObservationTranscriptPhase::Selecting;
        record.attempt_id = record.attempt_id.saturating_add(1);
        record.view.progress.failed_chunks = 0;
        record.view.progress.skipped_chunks = 0;
        record.view.progress.completed_chunks = 0;
        record.view.progress.completed_observations = 0;
        record.view.progress.failed_observations = 0;
        record.view.segments.clear();
        record.view.artifacts.clear();
        record.view.error = None;
        record.view.started_at_ms = None;
        record.view.finished_at_ms = None;
        record.view.updated_at_ms = now_ms();
        let view = record.view.clone();
        let save = record.clone();
        drop(jobs);
        self.observation_transcript_store.save_job(&save)?;
        self.observation_transcript_notify.notify_waiters();
        Ok(view)
    }

    pub(crate) async fn cancel_observation_transcript_job(
        self: &Arc<Self>,
        transcript_job_id: &str,
    ) -> Result<ObservationTranscriptJobView> {
        let mut jobs = self.observation_transcript_jobs.lock().await;
        let record = jobs
            .get_mut(transcript_job_id)
            .ok_or_else(|| anyhow!("unknown observation transcript job {transcript_job_id}"))?;
        if record.view.status.is_terminal() {
            return Ok(record.view.clone());
        }
        let now = now_ms();
        record.view.status = ObservationTranscriptStatus::Cancelled;
        record.view.phase = ObservationTranscriptPhase::Cancelled;
        record.view.error = Some("cancelled".to_string());
        record.view.finished_at_ms = Some(now);
        record.view.updated_at_ms = now;
        let view = record.view.clone();
        let save = record.clone();
        drop(jobs);
        self.observation_transcript_store.save_job(&save)?;
        Ok(view)
    }

    pub(crate) async fn active_observation_transcript_asset_ids(&self) -> BTreeSet<String> {
        let jobs = self.observation_transcript_jobs.lock().await;
        let active_observation_ids = jobs
            .values()
            .filter(|record| !record.view.status.is_terminal())
            .flat_map(|record| record.selected_observation_ids.iter().cloned())
            .collect::<BTreeSet<_>>();
        drop(jobs);
        let mut asset_ids = BTreeSet::new();
        for observation_id in active_observation_ids {
            if let Ok(observation) = self
                .observation_service
                .get_observation(&observation_id)
                .await
            {
                asset_ids.insert(observation.asset_id);
                if let Some(asset_id) = observation.canonical_text_asset_id {
                    asset_ids.insert(asset_id);
                }
            }
        }
        asset_ids
    }

    fn next_observation_transcript_job_id(&self) -> String {
        format!(
            "transcript-job-{}",
            self.next_observation_transcript_id
                .fetch_add(1, Ordering::Relaxed)
        )
    }

    async fn observation_transcript_worker_loop(self: Arc<Self>) {
        loop {
            if let Some((transcript_job_id, attempt_id)) =
                self.claim_next_observation_transcript_job().await
            {
                if let Err(error) = self
                    .process_observation_transcript_job(&transcript_job_id, attempt_id)
                    .await
                {
                    error!(
                        transcript_job_id = %transcript_job_id,
                        error = ?error,
                        "observation transcript worker error"
                    );
                    if let Err(settle_error) = self
                        .settle_observation_transcript_job_failed(
                            &transcript_job_id,
                            attempt_id,
                            bounded_transcript_error(&error),
                        )
                        .await
                    {
                        error!(
                            transcript_job_id = %transcript_job_id,
                            error = ?settle_error,
                            "failed to persist observation transcript job failure"
                        );
                    }
                    let notify = &self.observation_transcript_notify;
                    tokio::select! {
                        _ = notify.notified() => {}
                        _ = sleep_until(Instant::now() + Duration::from_millis(OBSERVATION_TRANSCRIPT_WORKER_RETRY_BACKOFF_MS)) => {}
                    }
                }
                continue;
            }
            self.observation_transcript_notify.notified().await;
        }
    }

    async fn claim_next_observation_transcript_job(&self) -> Option<(String, u64)> {
        let mut jobs = self.observation_transcript_jobs.lock().await;
        let mut queued = jobs
            .values()
            .filter(|record| record.view.status == ObservationTranscriptStatus::Queued)
            .map(|record| {
                (
                    record.view.created_at_ms,
                    record.view.transcript_job_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        queued.sort();
        let transcript_job_id = queued.first()?.1.clone();
        let mut attempt_id = 0;
        if let Some(record) = jobs.get_mut(&transcript_job_id) {
            let now = now_ms();
            attempt_id = record.attempt_id;
            record.view.status = ObservationTranscriptStatus::Running;
            record.view.phase = ObservationTranscriptPhase::Selecting;
            record.view.started_at_ms = Some(now);
            record.view.updated_at_ms = now;
            let _ = self.observation_transcript_store.save_job(record);
        }
        Some((transcript_job_id, attempt_id))
    }

    async fn process_observation_transcript_job(
        self: &Arc<Self>,
        transcript_job_id: &str,
        attempt_id: u64,
    ) -> Result<()> {
        let record = self
            .observation_transcript_record(transcript_job_id)
            .await?;
        self.update_observation_transcript_job(transcript_job_id, |record| {
            if record.view.status == ObservationTranscriptStatus::Cancelled
                || record.attempt_id != attempt_id
            {
                return;
            }
            record.view.phase = ObservationTranscriptPhase::Selecting;
            record.view.progress.completed_chunks = 0;
            record.view.progress.failed_chunks = 0;
            record.view.progress.skipped_chunks = 0;
            record.view.progress.completed_observations = 0;
            record.view.progress.failed_observations = 0;
            record.view.artifacts.clear();
            record.view.segments.clear();
            record.view.error = None;
            record.view.finished_at_ms = None;
        })
        .await?;
        if !self
            .observation_transcript_attempt_is_current(transcript_job_id, attempt_id)
            .await
        {
            return Ok(());
        }

        let selected = if record.selected_observation_ids.is_empty() {
            self.observation_service
                .resolve_transcript_selection(&record.view.selection)
                .await
        } else {
            let mut selected = Vec::with_capacity(record.selected_observation_ids.len());
            for observation_id in &record.selected_observation_ids {
                selected.push(
                    self.observation_service
                        .get_observation(observation_id)
                        .await?,
                );
            }
            selected.sort_by(|left, right| {
                left.captured_at_ms
                    .cmp(&right.captured_at_ms)
                    .then_with(|| left.observation_id.cmp(&right.observation_id))
            });
            selected
        };
        let selected_observation_ids = selected
            .iter()
            .map(|observation| observation.observation_id.clone())
            .collect::<Vec<_>>();
        let audio = selected
            .into_iter()
            .filter(is_audio_observation)
            .collect::<Vec<_>>();
        let skipped = selected_observation_ids.len().saturating_sub(audio.len()) as u64;
        self.update_observation_transcript_job(transcript_job_id, |record| {
            if record.view.status == ObservationTranscriptStatus::Cancelled
                || record.attempt_id != attempt_id
            {
                return;
            }
            if record.selected_observation_ids.is_empty() {
                record.selected_observation_ids = selected_observation_ids.clone();
            }
            record.view.progress.total_observations = selected_observation_ids.len() as u64;
            record.view.progress.selected_audio_observations = audio.len() as u64;
            record.view.progress.skipped_observations = skipped;
            record.view.updated_at_ms = now_ms();
        })
        .await?;
        if !self
            .observation_transcript_attempt_is_current(transcript_job_id, attempt_id)
            .await
        {
            return Ok(());
        }

        anyhow::ensure!(
            !audio.is_empty(),
            "observation transcript selection did not resolve any audio observations"
        );

        self.update_observation_transcript_job(transcript_job_id, |record| {
            if record.view.status == ObservationTranscriptStatus::Cancelled
                || record.attempt_id != attempt_id
            {
                return;
            }
            record.view.phase = ObservationTranscriptPhase::Chunking;
        })
        .await?;
        if !self
            .observation_transcript_attempt_is_current(transcript_job_id, attempt_id)
            .await
        {
            return Ok(());
        }
        self.update_observation_transcript_job(transcript_job_id, |record| {
            if record.view.status == ObservationTranscriptStatus::Cancelled
                || record.attempt_id != attempt_id
            {
                return;
            }
            record.view.phase = ObservationTranscriptPhase::Transcribing;
            record.view.updated_at_ms = now_ms();
        })
        .await?;
        if !self
            .observation_transcript_attempt_is_current(transcript_job_id, attempt_id)
            .await
        {
            return Ok(());
        }

        self.process_observation_transcript_audio_stream(
            transcript_job_id,
            attempt_id,
            &record,
            audio,
        )
        .await?;

        self.finalize_observation_transcript_job(transcript_job_id, attempt_id)
            .await?;
        Ok(())
    }

    async fn process_observation_transcript_audio_stream(
        self: &Arc<Self>,
        transcript_job_id: &str,
        attempt_id: u64,
        record: &ObservationTranscriptJobRecord,
        audio: Vec<crate::ObservationView>,
    ) -> Result<()> {
        let chunk_concurrency = observation_transcript_chunk_concurrency();
        let mut in_flight =
            FuturesUnordered::<JoinHandle<Vec<ObservationTranscriptSegmentView>>>::new();
        let mut by_role = BTreeMap::<String, Vec<crate::ObservationView>>::new();
        for observation in audio {
            let role = crate::observation_transcripts::observation_role(&observation)
                .unwrap_or("audio")
                .to_string();
            by_role.entry(role).or_default().push(observation);
        }

        let mut segment_index = 1usize;
        for (role, mut observations) in by_role {
            observations.sort_by(|left, right| {
                left.captured_at_ms
                    .cmp(&right.captured_at_ms)
                    .then_with(|| left.observation_id.cmp(&right.observation_id))
            });
            let mut current = Vec::<(
                crate::ObservationView,
                crate::assets::StoredAssetRecord,
                Vec<u8>,
                u64,
            )>::new();
            let mut current_bytes = 0u64;
            let mut current_duration_ms = 0u64;

            for observation in observations {
                if !self
                    .observation_transcript_attempt_is_current(transcript_job_id, attempt_id)
                    .await
                {
                    return Ok(());
                }
                let (asset, bytes) = match self.assets.read_raw(&observation.asset_id) {
                    Ok(value) => value,
                    Err(error) => {
                        self.flush_observation_transcript_chunk(
                            transcript_job_id,
                            attempt_id,
                            record,
                            &role,
                            &mut segment_index,
                            &mut current,
                            &mut current_bytes,
                            &mut current_duration_ms,
                            &mut in_flight,
                            chunk_concurrency,
                        )
                        .await?;
                        let segment = failed_observation_transcript_segment(
                            &role,
                            &mut segment_index,
                            observation,
                            bounded_transcript_error(&error),
                        );
                        self.append_observation_transcript_segment(
                            transcript_job_id,
                            attempt_id,
                            segment,
                        )
                        .await?;
                        continue;
                    }
                };

                if asset.media_type == "audio/wav" {
                    match split_pcm_wav_payload(
                        &bytes,
                        record.view.transcription.target_chunk_seconds,
                        record.view.transcription.max_chunk_bytes,
                    ) {
                        Ok(payloads) => {
                            for payload in payloads {
                                let duration_ms = wav_duration_ms(&payload).unwrap_or_else(|| {
                                    audio_observation_duration_ms(&observation, &payload)
                                });
                                let projected_bytes =
                                    current_bytes.saturating_add(payload.len() as u64);
                                let projected_duration =
                                    current_duration_ms.saturating_add(duration_ms);
                                if !current.is_empty()
                                    && (projected_bytes > record.view.transcription.max_chunk_bytes
                                        || projected_duration
                                            > record
                                                .view
                                                .transcription
                                                .target_chunk_seconds
                                                .saturating_mul(1000))
                                {
                                    self.flush_observation_transcript_chunk(
                                        transcript_job_id,
                                        attempt_id,
                                        record,
                                        &role,
                                        &mut segment_index,
                                        &mut current,
                                        &mut current_bytes,
                                        &mut current_duration_ms,
                                        &mut in_flight,
                                        chunk_concurrency,
                                    )
                                    .await?;
                                }
                                current_bytes = current_bytes.saturating_add(payload.len() as u64);
                                current_duration_ms =
                                    current_duration_ms.saturating_add(duration_ms);
                                current.push((
                                    observation.clone(),
                                    asset.clone(),
                                    payload,
                                    duration_ms,
                                ));
                            }
                            continue;
                        }
                        Err(error)
                            if bytes.len() as u64 > record.view.transcription.max_chunk_bytes =>
                        {
                            self.flush_observation_transcript_chunk(
                                transcript_job_id,
                                attempt_id,
                                record,
                                &role,
                                &mut segment_index,
                                &mut current,
                                &mut current_bytes,
                                &mut current_duration_ms,
                                &mut in_flight,
                                chunk_concurrency,
                            )
                            .await?;
                            let segment = failed_observation_transcript_segment(
                                &role,
                                &mut segment_index,
                                observation,
                                format!(
                                    "audio observation exceeds max_chunk_bytes and cannot be split as PCM WAV: {}",
                                    bounded_transcript_error(&error)
                                ),
                            );
                            self.append_observation_transcript_segment(
                                transcript_job_id,
                                attempt_id,
                                segment,
                            )
                            .await?;
                            continue;
                        }
                        Err(_) => {}
                    }
                }

                self.flush_observation_transcript_chunk(
                    transcript_job_id,
                    attempt_id,
                    record,
                    &role,
                    &mut segment_index,
                    &mut current,
                    &mut current_bytes,
                    &mut current_duration_ms,
                    &mut in_flight,
                    chunk_concurrency,
                )
                .await?;
                if bytes.len() as u64 > record.view.transcription.max_chunk_bytes {
                    let segment = failed_observation_transcript_segment(
                        &role,
                        &mut segment_index,
                        observation,
                        "audio observation exceeds max_chunk_bytes and cannot be split".to_string(),
                    );
                    self.append_observation_transcript_segment(
                        transcript_job_id,
                        attempt_id,
                        segment,
                    )
                    .await?;
                    continue;
                }
                let duration_ms = audio_observation_duration_ms(&observation, &bytes);
                let chunk = audio_transcript_chunk_from_items(
                    &role,
                    segment_index,
                    vec![(observation, asset, bytes, duration_ms)],
                    duration_ms,
                );
                segment_index = segment_index.saturating_add(1);
                self.enqueue_observation_transcript_chunk(
                    record,
                    &mut in_flight,
                    chunk_concurrency,
                    chunk,
                    transcript_job_id,
                    attempt_id,
                )
                .await?;
            }
            self.flush_observation_transcript_chunk(
                transcript_job_id,
                attempt_id,
                record,
                &role,
                &mut segment_index,
                &mut current,
                &mut current_bytes,
                &mut current_duration_ms,
                &mut in_flight,
                chunk_concurrency,
            )
            .await?;
        }
        while !in_flight.is_empty() {
            self.drain_next_observation_transcript_chunk(
                transcript_job_id,
                attempt_id,
                &mut in_flight,
            )
            .await?;
        }
        Ok(())
    }

    async fn flush_observation_transcript_chunk(
        self: &Arc<Self>,
        transcript_job_id: &str,
        attempt_id: u64,
        record: &ObservationTranscriptJobRecord,
        role: &str,
        segment_index: &mut usize,
        current: &mut Vec<(
            crate::ObservationView,
            crate::assets::StoredAssetRecord,
            Vec<u8>,
            u64,
        )>,
        current_bytes: &mut u64,
        current_duration_ms: &mut u64,
        in_flight: &mut FuturesUnordered<JoinHandle<Vec<ObservationTranscriptSegmentView>>>,
        chunk_concurrency: usize,
    ) -> Result<()> {
        if current.is_empty() {
            return Ok(());
        }
        if !self
            .observation_transcript_attempt_is_current(transcript_job_id, attempt_id)
            .await
        {
            return Ok(());
        }
        let chunk = audio_transcript_chunk_from_items(
            role,
            *segment_index,
            std::mem::take(current),
            *current_duration_ms,
        );
        *segment_index = (*segment_index).saturating_add(1);
        *current_bytes = 0;
        *current_duration_ms = 0;
        self.enqueue_observation_transcript_chunk(
            record,
            in_flight,
            chunk_concurrency,
            chunk,
            transcript_job_id,
            attempt_id,
        )
        .await?;
        Ok(())
    }

    async fn enqueue_observation_transcript_chunk(
        self: &Arc<Self>,
        record: &ObservationTranscriptJobRecord,
        in_flight: &mut FuturesUnordered<JoinHandle<Vec<ObservationTranscriptSegmentView>>>,
        chunk_concurrency: usize,
        chunk: AudioTranscriptChunk,
        transcript_job_id: &str,
        attempt_id: u64,
    ) -> Result<()> {
        while in_flight.len() >= chunk_concurrency {
            self.drain_next_observation_transcript_chunk(transcript_job_id, attempt_id, in_flight)
                .await?;
        }
        let state = self.clone();
        let record = record.clone();
        in_flight.push(tokio::spawn(async move {
            state
                .transcribe_observation_transcript_chunk(&record, chunk)
                .await
        }));
        Ok(())
    }

    async fn drain_next_observation_transcript_chunk(
        &self,
        transcript_job_id: &str,
        attempt_id: u64,
        in_flight: &mut FuturesUnordered<JoinHandle<Vec<ObservationTranscriptSegmentView>>>,
    ) -> Result<()> {
        let Some(result) = in_flight.next().await else {
            return Ok(());
        };
        let segments =
            result.map_err(|error| anyhow!("observation transcript chunk task failed: {error}"))?;
        self.append_observation_transcript_segments(transcript_job_id, attempt_id, segments)
            .await?;
        Ok(())
    }

    async fn transcribe_observation_transcript_chunk(
        &self,
        record: &ObservationTranscriptJobRecord,
        chunk: AudioTranscriptChunk,
    ) -> Vec<ObservationTranscriptSegmentView> {
        let source_asset_ids = chunk.source_asset_ids.clone();
        let observation_ids = chunk
            .observations
            .iter()
            .map(|observation| observation.observation_id.clone())
            .collect::<Vec<_>>();
        let base_segment = ObservationTranscriptSegmentView {
            segment_id: chunk.segment_id.clone(),
            status: ObservationTranscriptStatus::Running,
            role: chunk.role.clone(),
            captured_at_ms: chunk.captured_at_ms,
            duration_ms: chunk.duration_ms,
            seq_no_start: chunk.seq_no_start,
            seq_no_end: chunk.seq_no_end,
            observation_ids,
            source_asset_ids,
            audio_asset_id: None,
            transcript_asset_id: None,
            speaker_key: None,
            speaker_label: None,
            text: None,
            error: None,
        };
        match self
            .transcribe_observation_transcript_chunk_result(record, chunk)
            .await
        {
            Ok((audio_asset_id, transcript_asset)) => {
                let mut segment = base_segment;
                segment.audio_asset_id = audio_asset_id;
                let Some(transcript_asset) = transcript_asset else {
                    segment.status = ObservationTranscriptStatus::Skipped;
                    segment.error = Some("no speech detected".to_string());
                    return vec![segment];
                };
                let text = self.assets.read_text(&transcript_asset.id).ok().flatten();
                if text.as_deref().map(str::trim).is_none_or(str::is_empty) {
                    segment.status = ObservationTranscriptStatus::Skipped;
                    segment.error = Some("no speech detected".to_string());
                    return vec![segment];
                }
                segment.status = ObservationTranscriptStatus::Completed;
                segment.text = text;
                segment.transcript_asset_id = Some(transcript_asset.id);
                if record.view.transcription.diarization
                    && let Some(timestamps) = transcript_asset.timestamps.as_ref()
                    && let Some(segments) = diarized_transcript_segments(&segment, timestamps)
                {
                    return segments;
                }
                vec![segment]
            }
            Err(error) => {
                let mut segment = base_segment;
                if is_no_speech_transcription_error(&error) {
                    segment.status = ObservationTranscriptStatus::Skipped;
                    segment.error = Some("no speech detected".to_string());
                    return vec![segment];
                }
                segment.status = ObservationTranscriptStatus::Failed;
                segment.error = Some(bounded_transcript_error(&error));
                vec![segment]
            }
        }
    }

    async fn transcribe_observation_transcript_chunk_result(
        &self,
        record: &ObservationTranscriptJobRecord,
        chunk: AudioTranscriptChunk,
    ) -> Result<(Option<String>, Option<CanonicalTranscriptAsset>)> {
        let asset = if chunk.payloads.len() == 1 {
            let asset_id = chunk
                .source_asset_ids
                .first()
                .ok_or_else(|| anyhow!("transcript chunk has no source asset"))?;
            let source_asset = self
                .assets
                .get(asset_id)
                .ok_or_else(|| anyhow!("unknown asset {asset_id}"))?;
            let payload = chunk
                .payloads
                .first()
                .ok_or_else(|| anyhow!("transcript chunk has no payload"))?;
            let source_payload_matches = self
                .assets
                .read_raw(&source_asset.id)
                .ok()
                .is_some_and(|(_, bytes)| bytes == *payload);
            if source_payload_matches {
                source_asset
            } else {
                let extension = transcript_audio_extension(&source_asset.media_type);
                self.assets.import_bytes(
                    &format!(
                        "{}-{}.{}",
                        record.view.transcript_job_id, chunk.segment_id, extension
                    ),
                    Some(&source_asset.media_type),
                    payload,
                )?
            }
        } else {
            let bytes = concatenate_pcm_wav_payloads(&chunk.payloads)?;
            self.assets.import_bytes(
                &format!("{}-{}.wav", record.view.transcript_job_id, chunk.segment_id),
                Some("audio/wav"),
                &bytes,
            )?
        };
        let transcription_options = record.view.transcription.diarization.then(|| {
            NormalizedDerivationTranscriptionOptions {
                prompt: None,
                language: None,
                timestamp_granularities: Vec::new(),
                diarization: true,
            }
        });
        let transcript_asset = self
            .ensure_asset_audio_canonical_text_with_provenance(
                &asset,
                record.view.transcription.route_id.as_deref(),
                None,
                transcription_options.as_ref(),
                None,
                record.view.transcription.diarization,
            )
            .await?
            .map(|result| CanonicalTranscriptAsset {
                id: result.asset.id,
                timestamps: result.timestamps,
            });
        Ok((Some(asset.id), transcript_asset))
    }

    async fn append_observation_transcript_segments(
        &self,
        transcript_job_id: &str,
        attempt_id: u64,
        segments: Vec<ObservationTranscriptSegmentView>,
    ) -> Result<()> {
        if segments.is_empty() {
            return Ok(());
        }
        self.update_observation_transcript_job(transcript_job_id, |record| {
            if record.view.status == ObservationTranscriptStatus::Cancelled
                || record.attempt_id != attempt_id
            {
                return;
            }
            let aggregate_status = aggregate_segment_status(&segments);
            let observation_count = segments
                .iter()
                .flat_map(|segment| segment.observation_ids.iter().cloned())
                .collect::<BTreeSet<_>>()
                .len() as u64;

            record.view.progress.total_chunks = record.view.progress.total_chunks.saturating_add(1);
            if aggregate_status == ObservationTranscriptStatus::Completed {
                record.view.progress.completed_chunks =
                    record.view.progress.completed_chunks.saturating_add(1);
                record.view.progress.completed_observations = record
                    .view
                    .progress
                    .completed_observations
                    .saturating_add(observation_count);
            } else if aggregate_status == ObservationTranscriptStatus::Failed {
                record.view.progress.failed_chunks =
                    record.view.progress.failed_chunks.saturating_add(1);
                record.view.progress.failed_observations = record
                    .view
                    .progress
                    .failed_observations
                    .saturating_add(observation_count);
            } else if aggregate_status == ObservationTranscriptStatus::Skipped {
                record.view.progress.skipped_chunks =
                    record.view.progress.skipped_chunks.saturating_add(1);
                record.view.progress.skipped_observations = record
                    .view
                    .progress
                    .skipped_observations
                    .saturating_add(observation_count);
            }
            record.view.segments.extend(segments.clone());
            record.view.updated_at_ms = now_ms();
        })
        .await?;
        Ok(())
    }

    async fn append_observation_transcript_segment(
        &self,
        transcript_job_id: &str,
        attempt_id: u64,
        segment: ObservationTranscriptSegmentView,
    ) -> Result<()> {
        self.update_observation_transcript_job(transcript_job_id, |record| {
            if record.view.status == ObservationTranscriptStatus::Cancelled
                || record.attempt_id != attempt_id
            {
                return;
            }
            record.view.progress.total_chunks = record.view.progress.total_chunks.saturating_add(1);
            if segment.status == ObservationTranscriptStatus::Completed {
                record.view.progress.completed_chunks =
                    record.view.progress.completed_chunks.saturating_add(1);
                record.view.progress.completed_observations = record
                    .view
                    .progress
                    .completed_observations
                    .saturating_add(segment.observation_ids.len() as u64);
            } else if segment.status == ObservationTranscriptStatus::Failed {
                record.view.progress.failed_chunks =
                    record.view.progress.failed_chunks.saturating_add(1);
                record.view.progress.failed_observations = record
                    .view
                    .progress
                    .failed_observations
                    .saturating_add(segment.observation_ids.len() as u64);
            } else if segment.status == ObservationTranscriptStatus::Skipped {
                record.view.progress.skipped_chunks =
                    record.view.progress.skipped_chunks.saturating_add(1);
                record.view.progress.skipped_observations = record
                    .view
                    .progress
                    .skipped_observations
                    .saturating_add(segment.observation_ids.len() as u64);
            }
            record.view.segments.push(segment.clone());
            record.view.updated_at_ms = now_ms();
        })
        .await?;
        Ok(())
    }

    async fn finalize_observation_transcript_job(
        &self,
        transcript_job_id: &str,
        attempt_id: u64,
    ) -> Result<()> {
        if !self
            .observation_transcript_attempt_is_current(transcript_job_id, attempt_id)
            .await
        {
            return Ok(());
        }
        self.update_observation_transcript_job(transcript_job_id, |record| {
            if record.view.status == ObservationTranscriptStatus::Cancelled
                || record.attempt_id != attempt_id
            {
                return;
            }
            record.view.phase = ObservationTranscriptPhase::Finalizing;
            record.view.updated_at_ms = now_ms();
        })
        .await?;
        let record = self
            .observation_transcript_record(transcript_job_id)
            .await?;
        if record.view.status == ObservationTranscriptStatus::Cancelled
            || record.attempt_id != attempt_id
        {
            return Ok(());
        }
        let transcript_segments = record
            .view
            .segments
            .iter()
            .filter(|segment| {
                matches!(
                    segment.status,
                    ObservationTranscriptStatus::Completed | ObservationTranscriptStatus::Skipped
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        anyhow::ensure!(
            !transcript_segments.is_empty(),
            "observation transcript job produced no transcript segments"
        );

        let markdown = render_transcript_markdown(&record, &transcript_segments);
        let segments_jsonl = render_transcript_segments_jsonl(&record.view.segments)?;
        let markdown_asset = self.assets.import_bytes(
            &format!("{}.transcript.md", transcript_job_id),
            Some("text/markdown"),
            markdown.as_bytes(),
        )?;
        let segments_asset = self.assets.import_bytes(
            &format!("{}.segments.jsonl", transcript_job_id),
            Some("text/plain"),
            segments_jsonl.as_bytes(),
        )?;
        let failed_chunks = record.view.progress.failed_chunks;
        let terminal_status = if failed_chunks == 0 {
            ObservationTranscriptStatus::Completed
        } else {
            ObservationTranscriptStatus::Failed
        };
        let terminal_phase = if failed_chunks == 0 {
            ObservationTranscriptPhase::Completed
        } else {
            ObservationTranscriptPhase::Failed
        };
        let terminal_now = now_ms();
        let mut manifest_record = record.clone();
        manifest_record.view.status = terminal_status.clone();
        manifest_record.view.phase = terminal_phase.clone();
        manifest_record.view.error = if failed_chunks == 0 {
            None
        } else {
            Some(format!("{failed_chunks} transcript chunk(s) failed"))
        };
        manifest_record.view.finished_at_ms = Some(terminal_now);
        manifest_record.view.updated_at_ms = terminal_now;
        let manifest_asset = self.assets.import_bytes(
            &format!("{}.manifest.json", transcript_job_id),
            Some("application/json"),
            &serde_json::to_vec_pretty(&render_transcript_manifest(
                &manifest_record,
                &markdown_asset,
                &segments_asset,
            ))?,
        )?;
        self.update_observation_transcript_job(transcript_job_id, |record| {
            let now = now_ms();
            if record.view.status == ObservationTranscriptStatus::Cancelled
                || record.attempt_id != attempt_id
            {
                return;
            }
            record.view.status = terminal_status.clone();
            record.view.phase = terminal_phase.clone();
            record.view.error = if failed_chunks == 0 {
                None
            } else {
                Some(format!("{failed_chunks} transcript chunk(s) failed"))
            };
            record.view.artifacts = vec![
                crate::ObservationTranscriptArtifactView::new("markdown", &markdown_asset),
                crate::ObservationTranscriptArtifactView::new("segments_jsonl", &segments_asset),
                crate::ObservationTranscriptArtifactView::new("manifest", &manifest_asset),
            ];
            record.view.finished_at_ms = Some(now);
            record.view.updated_at_ms = now;
        })
        .await?;
        Ok(())
    }

    async fn observation_transcript_attempt_is_current(
        &self,
        transcript_job_id: &str,
        attempt_id: u64,
    ) -> bool {
        self.observation_transcript_jobs
            .lock()
            .await
            .get(transcript_job_id)
            .map(|record| {
                record.view.status != ObservationTranscriptStatus::Cancelled
                    && record.attempt_id == attempt_id
            })
            .unwrap_or(false)
    }

    async fn settle_observation_transcript_job_failed(
        &self,
        transcript_job_id: &str,
        attempt_id: u64,
        error: String,
    ) -> Result<()> {
        if !self
            .observation_transcript_attempt_is_current(transcript_job_id, attempt_id)
            .await
        {
            return Ok(());
        }
        self.update_observation_transcript_job(transcript_job_id, |record| {
            if record.view.status == ObservationTranscriptStatus::Cancelled
                || record.attempt_id != attempt_id
            {
                return;
            }
            let now = now_ms();
            record.view.status = ObservationTranscriptStatus::Failed;
            record.view.phase = ObservationTranscriptPhase::Failed;
            record.view.error = Some(error.clone());
            record.view.finished_at_ms = Some(now);
            record.view.updated_at_ms = now;
        })
        .await?;
        Ok(())
    }

    async fn observation_transcript_record(
        &self,
        transcript_job_id: &str,
    ) -> Result<ObservationTranscriptJobRecord> {
        self.observation_transcript_jobs
            .lock()
            .await
            .get(transcript_job_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown observation transcript job {transcript_job_id}"))
    }

    async fn update_observation_transcript_job<F>(
        &self,
        transcript_job_id: &str,
        mut update: F,
    ) -> Result<ObservationTranscriptJobView>
    where
        F: FnMut(&mut ObservationTranscriptJobRecord),
    {
        let mut jobs = self.observation_transcript_jobs.lock().await;
        let record = jobs
            .get_mut(transcript_job_id)
            .ok_or_else(|| anyhow!("unknown observation transcript job {transcript_job_id}"))?;
        update(record);
        let view = record.view.clone();
        let save = record.clone();
        drop(jobs);
        self.observation_transcript_store.save_job(&save)?;
        Ok(view)
    }
}

fn failed_observation_transcript_segment(
    role: &str,
    segment_index: &mut usize,
    observation: crate::ObservationView,
    error: String,
) -> ObservationTranscriptSegmentView {
    let segment = ObservationTranscriptSegmentView {
        segment_id: format!("segment-{}", *segment_index),
        status: ObservationTranscriptStatus::Failed,
        role: role.to_string(),
        captured_at_ms: observation.captured_at_ms,
        duration_ms: 0,
        seq_no_start: observation.seq_no,
        seq_no_end: observation.seq_no,
        observation_ids: vec![observation.observation_id],
        source_asset_ids: vec![observation.asset_id],
        audio_asset_id: None,
        transcript_asset_id: None,
        speaker_key: None,
        speaker_label: None,
        text: None,
        error: Some(error),
    };
    *segment_index = (*segment_index).saturating_add(1);
    segment
}

fn aggregate_segment_status(
    segments: &[ObservationTranscriptSegmentView],
) -> ObservationTranscriptStatus {
    if segments
        .iter()
        .any(|segment| segment.status == ObservationTranscriptStatus::Failed)
    {
        ObservationTranscriptStatus::Failed
    } else if segments
        .iter()
        .any(|segment| segment.status == ObservationTranscriptStatus::Completed)
    {
        ObservationTranscriptStatus::Completed
    } else {
        ObservationTranscriptStatus::Skipped
    }
}

fn diarized_transcript_segments(
    base: &ObservationTranscriptSegmentView,
    timestamps: &AudioTranscriptionTimestamps,
) -> Option<Vec<ObservationTranscriptSegmentView>> {
    let mut output = Vec::new();
    for (index, provider_segment) in timestamps.segments.iter().enumerate() {
        let text = provider_segment.text.trim();
        if text.is_empty() {
            continue;
        }
        let start_ms = provider_segment.start_ms.min(base.duration_ms);
        let end_ms = provider_segment.end_ms.min(base.duration_ms);
        if end_ms < start_ms {
            continue;
        }
        let speaker_label = provider_segment
            .speaker
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(bounded_speaker_label);
        let speaker_key = speaker_label
            .as_deref()
            .map(|label| scoped_speaker_key(&base.segment_id, label));
        let mut segment = base.clone();
        segment.segment_id = format!("{}-utterance-{}", base.segment_id, index + 1);
        segment.captured_at_ms = base.captured_at_ms.saturating_add(start_ms);
        segment.duration_ms = end_ms.saturating_sub(start_ms);
        segment.text = Some(text.to_string());
        segment.speaker_key = speaker_key;
        segment.speaker_label = speaker_label;
        output.push(segment);
    }
    (!output.is_empty()).then_some(output)
}

fn scoped_speaker_key(scope: &str, speaker_label: &str) -> String {
    let scope = normalize_speaker_key_fragment(scope, "segment", 56);
    let speaker = normalize_speaker_key_fragment(speaker_label, "speaker", 64);
    truncate_chars(
        format!("{scope}-{speaker}"),
        OBSERVATION_TRANSCRIPT_SPEAKER_KEY_MAX_CHARS,
    )
}

fn normalize_speaker_key_fragment(value: &str, fallback: &str, max_chars: usize) -> String {
    let mut key = String::new();
    let mut last_was_separator = false;
    for character in value.trim().chars() {
        if character.is_ascii_alphanumeric() {
            key.push(character.to_ascii_lowercase());
            last_was_separator = false;
        } else if matches!(character, '_' | '-' | ' ' | ':' | '.') && !last_was_separator {
            key.push('-');
            last_was_separator = true;
        }
    }
    let key = truncate_chars(key.trim_matches('-').to_string(), max_chars);
    if key.is_empty() {
        fallback.to_string()
    } else {
        key
    }
}

fn bounded_speaker_label(value: &str) -> String {
    let normalized = value
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
    truncate_chars(normalized, OBSERVATION_TRANSCRIPT_SPEAKER_LABEL_MAX_CHARS)
}

fn truncate_chars(value: String, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn audio_observation_duration_ms(observation: &crate::ObservationView, payload: &[u8]) -> u64 {
    observation
        .metadata
        .get("duration_ms")
        .and_then(|value| value.as_u64())
        .or_else(|| wav_duration_ms(payload))
        .unwrap_or(5_000)
}

fn transcript_audio_extension(media_type: &str) -> &'static str {
    match media_type {
        "audio/wav" => "wav",
        "audio/webm" => "webm",
        _ => "audio",
    }
}

fn render_transcript_markdown(
    record: &ObservationTranscriptJobRecord,
    segments: &[ObservationTranscriptSegmentView],
) -> String {
    let offset_base_ms = record.view.selection.after_ms.unwrap_or_else(|| {
        segments
            .iter()
            .map(|segment| segment.captured_at_ms)
            .min()
            .unwrap_or(record.view.created_at_ms)
    });
    let mut output = format!(
        "# Recording Transcript\n\n- Transcript job: `{}`\n- Capture group: `{}`\n- Recording: `{}`\n- Segments: {}\n\n",
        record.view.transcript_job_id,
        record.view.selection.capture_group_id,
        record
            .view
            .selection
            .recording_id
            .as_deref()
            .unwrap_or("unknown"),
        segments.len()
    );
    let mut wrote_text = false;
    for segment in segments {
        if let Some(text) = segment
            .text
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            let label = segment
                .speaker_label
                .as_deref()
                .or(segment.speaker_key.as_deref())
                .unwrap_or(&segment.role);
            let label = markdown_heading_text(label);
            output.push_str(&format!(
                "## {} · +{}\n\n{}\n\n",
                label,
                format_duration_offset(segment.captured_at_ms, offset_base_ms),
                text
            ));
            wrote_text = true;
        }
    }
    if !wrote_text {
        output.push_str("No speech was detected in the selected audio observations.\n");
    }
    output
}

fn markdown_heading_text(value: &str) -> String {
    let mut output = String::new();
    let mut last_was_space = false;
    for character in value.trim().chars() {
        let next = match character {
            '\n' | '\r' | '\t' => ' ',
            '#' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|' => ' ',
            character if character.is_control() => ' ',
            character => character,
        };
        if next.is_whitespace() {
            if !last_was_space {
                output.push(' ');
                last_was_space = true;
            }
        } else {
            output.push(next);
            last_was_space = false;
        }
    }
    let output = output.trim();
    if output.is_empty() {
        "Speaker".to_string()
    } else {
        output.to_string()
    }
}

fn render_transcript_segments_jsonl(
    segments: &[ObservationTranscriptSegmentView],
) -> Result<String> {
    let mut output = String::new();
    for segment in segments {
        output.push_str(&serde_json::to_string(segment)?);
        output.push('\n');
    }
    Ok(output)
}

fn render_transcript_manifest(
    record: &ObservationTranscriptJobRecord,
    markdown_asset: &StoredAssetRecord,
    segments_asset: &StoredAssetRecord,
) -> Value {
    json!({
        "schema_version": 1,
        "kind": "observation_transcript_manifest",
        "transcript_job_id": &record.view.transcript_job_id,
        "capture_group_id": &record.view.selection.capture_group_id,
        "recording_id": &record.view.selection.recording_id,
        "status": &record.view.status,
        "progress": &record.view.progress,
        "markdown_asset_id": &markdown_asset.id,
        "segments_jsonl_asset_id": &segments_asset.id,
        "created_at_ms": record.view.created_at_ms,
        "updated_at_ms": record.view.updated_at_ms,
    })
}

fn format_duration_offset(captured_at_ms: u64, base_ms: u64) -> String {
    let seconds = captured_at_ms.saturating_sub(base_ms) / 1000;
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    format!("{minutes:02}:{seconds:02}")
}

fn observation_transcript_chunk_concurrency() -> usize {
    std::env::var("KHEISH_OBSERVATION_TRANSCRIPT_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.clamp(1, MAX_OBSERVATION_TRANSCRIPT_CHUNK_CONCURRENCY))
        .unwrap_or(DEFAULT_OBSERVATION_TRANSCRIPT_CHUNK_CONCURRENCY)
}

fn sort_observation_transcript_segments(segments: &mut [ObservationTranscriptSegmentView]) {
    segments.sort_by(|left, right| {
        left.captured_at_ms
            .cmp(&right.captured_at_ms)
            .then_with(|| left.role.cmp(&right.role))
            .then_with(|| {
                transcript_segment_index(&left.segment_id)
                    .cmp(&transcript_segment_index(&right.segment_id))
            })
            .then_with(|| left.segment_id.cmp(&right.segment_id))
    });
}

fn transcript_segment_index(segment_id: &str) -> u64 {
    segment_id
        .strip_prefix("segment-")
        .and_then(|value| value.split('-').next())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(u64::MAX)
}

fn is_no_speech_transcription_error(error: &anyhow::Error) -> bool {
    error
        .to_string()
        .to_ascii_lowercase()
        .contains("audio transcription json did not contain text")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diarized_transcript_segments_split_chunk_by_provider_turns() {
        let base = ObservationTranscriptSegmentView {
            segment_id: "segment-1".to_string(),
            status: ObservationTranscriptStatus::Completed,
            role: "microphone".to_string(),
            captured_at_ms: 1_000,
            duration_ms: 5_000,
            seq_no_start: Some(1),
            seq_no_end: Some(2),
            observation_ids: vec!["obs-1".to_string()],
            source_asset_ids: vec!["asset-1".to_string()],
            audio_asset_id: Some("audio-1".to_string()),
            transcript_asset_id: Some("text-1".to_string()),
            speaker_key: None,
            speaker_label: None,
            text: Some("hello there welcome back".to_string()),
            error: None,
        };
        let timestamps = AudioTranscriptionTimestamps {
            language: None,
            duration_ms: Some(2_500),
            words: Vec::new(),
            segments: vec![
                kheish_runtime::AudioTranscriptionSegmentTimestamp {
                    id: Some(0),
                    text: "hello there".to_string(),
                    start_ms: 0,
                    end_ms: 1_200,
                    speaker: Some("speaker_0".to_string()),
                },
                kheish_runtime::AudioTranscriptionSegmentTimestamp {
                    id: Some(1),
                    text: "welcome back".to_string(),
                    start_ms: 1_300,
                    end_ms: 2_500,
                    speaker: Some("Speaker 2".to_string()),
                },
            ],
        };

        let segments = diarized_transcript_segments(&base, &timestamps).expect("segments");

        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].segment_id, "segment-1-utterance-1");
        assert_eq!(segments[0].captured_at_ms, 1_000);
        assert_eq!(segments[0].duration_ms, 1_200);
        assert_eq!(
            segments[0].speaker_key.as_deref(),
            Some("segment-1-speaker-0")
        );
        assert_eq!(segments[0].speaker_label.as_deref(), Some("speaker_0"));
        assert_eq!(segments[1].captured_at_ms, 2_300);
        assert_eq!(
            segments[1].speaker_key.as_deref(),
            Some("segment-1-speaker-2")
        );
        assert_eq!(segments[1].text.as_deref(), Some("welcome back"));
    }
}

fn bounded_transcript_error(error: &anyhow::Error) -> String {
    let redacted = redact_text(&error.to_string());
    let mut output = String::new();
    for ch in redacted
        .chars()
        .take(OBSERVATION_TRANSCRIPT_ERROR_MAX_CHARS)
    {
        output.push(ch);
    }
    output
}
