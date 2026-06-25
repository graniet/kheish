//! Asset reference inspection methods implemented on [`DaemonState`].

use super::*;
use kheish_types::ContentPart;

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(crate) async fn asset_references(
        &self,
        asset_id: &str,
    ) -> Result<crate::AssetReferencesView> {
        let asset_id = asset_id.trim();
        anyhow::ensure!(!asset_id.is_empty(), "asset_id is required");
        let _ = self
            .assets
            .get(asset_id)
            .ok_or_else(|| anyhow!("unknown asset {asset_id}"))?;

        let mut references = Vec::new();
        let mut seen = BTreeSet::new();

        self.collect_run_asset_references(asset_id, &mut references, &mut seen)
            .await?;
        self.collect_session_output_asset_references(asset_id, &mut references, &mut seen)
            .await?;
        self.collect_delivery_asset_references(asset_id, &mut references, &mut seen)
            .await?;
        self.collect_asset_to_asset_references(asset_id, &mut references, &mut seen);
        self.collect_observation_asset_references(asset_id, &mut references, &mut seen)
            .await?;
        self.collect_observation_transcript_asset_references(asset_id, &mut references, &mut seen)
            .await;
        self.collect_derivation_asset_references(asset_id, &mut references, &mut seen)
            .await;
        self.collect_board_asset_references(asset_id, &mut references, &mut seen)
            .await?;
        self.collect_channel_asset_references(asset_id, &mut references, &mut seen)
            .await?;

        sort_asset_references(&mut references);
        let hard_reference_count = references.iter().filter(|reference| reference.hard).count();
        let soft_reference_count = references.len().saturating_sub(hard_reference_count);
        Ok(crate::AssetReferencesView {
            asset_id: asset_id.to_string(),
            hard_reference_count,
            soft_reference_count,
            references,
        })
    }

    pub(crate) async fn delete_asset(
        &self,
        asset_id: &str,
        dry_run: bool,
    ) -> Result<crate::AssetDeletionPlanView> {
        let mut plan = self.asset_deletion_plan(asset_id, dry_run).await?;
        if dry_run {
            return Ok(plan);
        }
        anyhow::ensure!(
            !plan.blocked,
            "asset {} has hard references; delete blocked",
            plan.asset_id
        );
        plan.deleted = self
            .assets
            .delete_asset_with_reason(&plan.asset_id, "api_delete")?;
        Ok(plan)
    }

    pub(crate) async fn gc_assets(&self, dry_run: bool) -> Result<crate::AssetGcPlanView> {
        let mut records = self.assets.list(None);
        records.sort_by(|left, right| left.id.cmp(&right.id));
        let mut plans = Vec::new();
        let mut candidate_count = 0usize;
        let mut blocked_count = 0usize;
        let mut deleted_count = 0usize;
        let mut reclaimable_bytes = 0u64;

        for record in records {
            let mut plan = self.asset_deletion_plan(&record.id, dry_run).await?;
            if plan.blocked {
                blocked_count += 1;
            } else {
                candidate_count += 1;
                reclaimable_bytes = reclaimable_bytes.saturating_add(plan.reclaimable_bytes);
                if !dry_run {
                    plan.deleted = self.assets.delete_asset_with_reason(&plan.asset_id, "gc")?;
                    if plan.deleted {
                        deleted_count += 1;
                    }
                }
            }
            plans.push(plan);
        }
        let orphan_files = self
            .assets
            .orphan_payload_files()?
            .into_iter()
            .map(asset_deletion_file_view)
            .collect::<Vec<_>>();
        let orphan_reclaimable_bytes = orphan_files
            .iter()
            .filter(|file| file.exists)
            .map(|file| file.byte_length)
            .sum::<u64>();
        reclaimable_bytes = reclaimable_bytes.saturating_add(orphan_reclaimable_bytes);
        let mut orphan_deleted_count = 0usize;
        if !dry_run {
            for file in &orphan_files {
                let Some(uri) = file.uri.as_deref() else {
                    continue;
                };
                if self.assets.delete_orphan_payload_file(uri)? {
                    orphan_deleted_count += 1;
                }
            }
        }

        Ok(crate::AssetGcPlanView {
            dry_run,
            inspected_count: plans.len(),
            candidate_count,
            blocked_count,
            deleted_count,
            orphan_file_count: orphan_files.len(),
            orphan_deleted_count,
            reclaimable_bytes,
            orphan_reclaimable_bytes,
            plans,
            orphan_files,
        })
    }

    async fn asset_deletion_plan(
        &self,
        asset_id: &str,
        dry_run: bool,
    ) -> Result<crate::AssetDeletionPlanView> {
        let asset_id = asset_id.trim();
        anyhow::ensure!(!asset_id.is_empty(), "asset_id is required");
        let asset = self
            .assets
            .get(asset_id)
            .ok_or_else(|| anyhow!("unknown asset {asset_id}"))?;
        let references = self.asset_references(asset_id).await?;
        let files = self
            .assets
            .deletion_files(&asset)?
            .into_iter()
            .map(asset_deletion_file_view)
            .collect::<Vec<_>>();
        let reclaimable_bytes = files
            .iter()
            .filter(|file| file.exists)
            .map(|file| file.byte_length)
            .sum();
        let blocked = references.hard_reference_count > 0;
        Ok(crate::AssetDeletionPlanView {
            asset_id: asset_id.to_string(),
            dry_run,
            blocked,
            deleted: false,
            reclaimable_bytes,
            hard_reference_count: references.hard_reference_count,
            soft_reference_count: references.soft_reference_count,
            files,
            references: references.references,
        })
    }

    pub(crate) async fn asset_ids_protected_from_observation_retention(
        &self,
    ) -> Result<BTreeSet<String>> {
        let mut asset_ids = BTreeSet::new();

        for run in self.run_service.list_runs(None).await? {
            for attachment in &run.input_attachments {
                asset_ids.insert(attachment.id.clone());
            }
            collect_output_asset_ids(&run.outputs, &mut asset_ids);
        }

        for session_id in self.session_service.session_pairs().await.keys() {
            let outputs = self.delivery_service.session_outputs(session_id).await?;
            collect_output_asset_ids(&outputs, &mut asset_ids);
        }

        for (_, record) in self.delivery_service.reference_records().await? {
            collect_content_part_asset_ids(&record.parts, &mut asset_ids);
            asset_ids.extend(record.artifacts.into_iter().map(|attachment| attachment.id));
        }

        for asset in self.assets.list(None) {
            if let Some(text_uri) = asset.text_uri.as_deref()
                && let Some(text_asset) = self.assets.get_by_uri(text_uri)
                && text_asset.id != asset.id
            {
                asset_ids.insert(text_asset.id);
            }
        }

        asset_ids.extend(self.active_observation_transcript_asset_ids().await);

        for derivation in self.derivation_service.list(None).await {
            match &derivation.subject {
                DerivationSubject::Asset { asset_id } => {
                    asset_ids.insert(asset_id.clone());
                }
                DerivationSubject::Observation { observation_id } => {
                    if let Ok(observation) = self
                        .observation_service
                        .get_observation(observation_id)
                        .await
                    {
                        asset_ids.insert(observation.asset_id);
                        if let Some(canonical_text_asset_id) = observation.canonical_text_asset_id {
                            asset_ids.insert(canonical_text_asset_id);
                        }
                    }
                }
                DerivationSubject::SessionInput { .. } => {}
            }
            if !derivation.result_asset_id.trim().is_empty() {
                asset_ids.insert(derivation.result_asset_id);
            }
            if let Some(timestamp_asset_id) = derivation
                .backend
                .and_then(|backend| backend.timestamp_asset_id)
            {
                asset_ids.insert(timestamp_asset_id);
            }
        }

        for board in self.board_service.list_boards(None, None).await {
            for revision in self
                .board_service
                .list_revisions(&board.summary.board_id)
                .await?
            {
                asset_ids.insert(revision.render_asset_id);
                if let Some(state_asset_id) = revision.state_asset_id.as_deref() {
                    asset_ids.insert(state_asset_id.to_string());
                    if let Ok((_, state_bytes)) = self.assets.read_raw(state_asset_id)
                        && let Ok(embedded_asset_ids) =
                            crate::boards::board_state_payload_asset_ids(&state_bytes)
                    {
                        asset_ids.extend(embedded_asset_ids);
                    }
                }
            }
        }

        for channel in self.channel_service.list_channels(None).await {
            asset_ids.extend(channel.pinned_asset_ids.clone());
            let messages = self
                .channel_service
                .list_messages(&channel.summary.channel_id, None)
                .await?;
            for message in messages {
                collect_rich_output_asset_ids(&message.output, &mut asset_ids);
            }
        }

        Ok(asset_ids)
    }

    async fn collect_run_asset_references(
        &self,
        asset_id: &str,
        references: &mut Vec<crate::AssetReferenceView>,
        seen: &mut BTreeSet<AssetReferenceKey>,
    ) -> Result<()> {
        for run in self.run_service.list_runs(None).await? {
            for attachment in &run.input_attachments {
                if attachment.id == asset_id {
                    push_asset_reference(
                        references,
                        seen,
                        AssetReferenceParts {
                            domain: "runs",
                            owner_id: &run.run_id,
                            role: "input_attachment",
                            hard: true,
                            parent_id: Some(&run.session_id),
                            detail_id: None,
                        },
                    );
                }
            }
            for (output_index, output) in run.outputs.iter().enumerate() {
                collect_daemon_output_asset_references(
                    asset_id,
                    references,
                    seen,
                    "runs",
                    &run.run_id,
                    Some(&run.session_id),
                    output_index,
                    output,
                );
            }
        }
        Ok(())
    }

    async fn collect_session_output_asset_references(
        &self,
        asset_id: &str,
        references: &mut Vec<crate::AssetReferenceView>,
        seen: &mut BTreeSet<AssetReferenceKey>,
    ) -> Result<()> {
        for session_id in self.session_service.session_pairs().await.keys() {
            let outputs = self.delivery_service.session_outputs(session_id).await?;
            for (output_index, output) in outputs.iter().enumerate() {
                collect_daemon_output_asset_references(
                    asset_id,
                    references,
                    seen,
                    "session_outputs",
                    session_id,
                    Some(session_id),
                    output_index,
                    output,
                );
            }
        }
        Ok(())
    }

    async fn collect_delivery_asset_references(
        &self,
        asset_id: &str,
        references: &mut Vec<crate::AssetReferenceView>,
        seen: &mut BTreeSet<AssetReferenceKey>,
    ) -> Result<()> {
        for (status, record) in self.delivery_service.reference_records().await? {
            collect_content_part_asset_references(
                asset_id,
                references,
                seen,
                "deliveries",
                &record.id,
                Some(&record.conversation.session_id),
                status.as_str(),
                &record.parts,
            );
            for attachment in &record.artifacts {
                if attachment.id == asset_id {
                    push_asset_reference(
                        references,
                        seen,
                        AssetReferenceParts {
                            domain: "deliveries",
                            owner_id: &record.id,
                            role: "delivery_artifact",
                            hard: true,
                            parent_id: Some(&record.conversation.session_id),
                            detail_id: Some(status.as_str()),
                        },
                    );
                }
            }
        }
        Ok(())
    }

    fn collect_asset_to_asset_references(
        &self,
        asset_id: &str,
        references: &mut Vec<crate::AssetReferenceView>,
        seen: &mut BTreeSet<AssetReferenceKey>,
    ) {
        for asset in self.assets.list(None) {
            if let Some(text_uri) = asset.text_uri.as_deref()
                && let Some(text_asset) = self.assets.get_by_uri(text_uri)
                && text_asset.id == asset_id
                && asset.id != asset_id
            {
                push_asset_reference(
                    references,
                    seen,
                    AssetReferenceParts {
                        domain: "assets",
                        owner_id: &asset.id,
                        role: "attached_text_asset",
                        hard: true,
                        parent_id: None,
                        detail_id: None,
                    },
                );
            }
            for provenance in &asset.provenance {
                for source in &provenance.source_assets {
                    if source.asset_id == asset_id && asset.id != asset_id {
                        push_asset_reference(
                            references,
                            seen,
                            AssetReferenceParts {
                                domain: "assets",
                                owner_id: &asset.id,
                                role: "provenance_source_asset",
                                hard: false,
                                parent_id: None,
                                detail_id: Some(&provenance.kind),
                            },
                        );
                    }
                }
            }
        }
    }

    async fn collect_observation_asset_references(
        &self,
        asset_id: &str,
        references: &mut Vec<crate::AssetReferenceView>,
        seen: &mut BTreeSet<AssetReferenceKey>,
    ) -> Result<()> {
        for observation in self
            .observation_service
            .list_observations(None, None, None, None, true)
            .await
        {
            let hard = observation.retention_state == crate::ObservationRetentionState::Active;
            let retention_state = observation_retention_state_name(&observation.retention_state);
            if observation.asset_id == asset_id {
                push_asset_reference(
                    references,
                    seen,
                    AssetReferenceParts {
                        domain: "observations",
                        owner_id: &observation.observation_id,
                        role: "raw_asset",
                        hard,
                        parent_id: Some(&observation.source_id),
                        detail_id: Some(retention_state),
                    },
                );
            }
            if observation.canonical_text_asset_id.as_deref() == Some(asset_id) {
                push_asset_reference(
                    references,
                    seen,
                    AssetReferenceParts {
                        domain: "observations",
                        owner_id: &observation.observation_id,
                        role: "canonical_text_asset",
                        hard,
                        parent_id: Some(&observation.source_id),
                        detail_id: Some(retention_state),
                    },
                );
            }
        }
        Ok(())
    }

    async fn collect_observation_transcript_asset_references(
        &self,
        asset_id: &str,
        references: &mut Vec<crate::AssetReferenceView>,
        seen: &mut BTreeSet<AssetReferenceKey>,
    ) {
        let jobs = self.observation_transcript_jobs.lock().await;
        for record in jobs.values() {
            for artifact in &record.view.artifacts {
                if artifact.asset_id == asset_id {
                    push_asset_reference(
                        references,
                        seen,
                        AssetReferenceParts {
                            domain: "observation_transcripts",
                            owner_id: &record.view.transcript_job_id,
                            role: &artifact.kind,
                            hard: true,
                            parent_id: record.view.selection.recording_id.as_deref(),
                            detail_id: Some(record.view.status.as_str()),
                        },
                    );
                }
            }
            for segment in &record.view.segments {
                if segment.audio_asset_id.as_deref() == Some(asset_id) {
                    push_asset_reference(
                        references,
                        seen,
                        AssetReferenceParts {
                            domain: "observation_transcripts",
                            owner_id: &record.view.transcript_job_id,
                            role: "segment_audio",
                            hard: true,
                            parent_id: Some(&segment.segment_id),
                            detail_id: Some(record.view.status.as_str()),
                        },
                    );
                }
                if segment.transcript_asset_id.as_deref() == Some(asset_id) {
                    push_asset_reference(
                        references,
                        seen,
                        AssetReferenceParts {
                            domain: "observation_transcripts",
                            owner_id: &record.view.transcript_job_id,
                            role: "segment_transcript",
                            hard: true,
                            parent_id: Some(&segment.segment_id),
                            detail_id: Some(record.view.status.as_str()),
                        },
                    );
                }
            }
        }
    }

    async fn collect_derivation_asset_references(
        &self,
        asset_id: &str,
        references: &mut Vec<crate::AssetReferenceView>,
        seen: &mut BTreeSet<AssetReferenceKey>,
    ) {
        for derivation in self.derivation_service.list(None).await {
            if matches!(
                &derivation.subject,
                DerivationSubject::Asset {
                    asset_id: subject_asset_id
                } if subject_asset_id == asset_id
            ) {
                push_asset_reference(
                    references,
                    seen,
                    AssetReferenceParts {
                        domain: "derivations",
                        owner_id: &derivation.derivation_id,
                        role: "subject_asset",
                        hard: true,
                        parent_id: None,
                        detail_id: Some(derivation.status.as_str()),
                    },
                );
            }
            if derivation.result_asset_id == asset_id {
                push_asset_reference(
                    references,
                    seen,
                    AssetReferenceParts {
                        domain: "derivations",
                        owner_id: &derivation.derivation_id,
                        role: "result_asset",
                        hard: true,
                        parent_id: None,
                        detail_id: Some(derivation.status.as_str()),
                    },
                );
            }
            if derivation
                .backend
                .as_ref()
                .and_then(|backend| backend.timestamp_asset_id.as_deref())
                == Some(asset_id)
            {
                push_asset_reference(
                    references,
                    seen,
                    AssetReferenceParts {
                        domain: "derivations",
                        owner_id: &derivation.derivation_id,
                        role: "timestamp_asset",
                        hard: true,
                        parent_id: None,
                        detail_id: Some(derivation.status.as_str()),
                    },
                );
            }
        }
    }

    async fn collect_board_asset_references(
        &self,
        asset_id: &str,
        references: &mut Vec<crate::AssetReferenceView>,
        seen: &mut BTreeSet<AssetReferenceKey>,
    ) -> Result<()> {
        for board in self.board_service.list_boards(None, None).await {
            for revision in self
                .board_service
                .list_revisions(&board.summary.board_id)
                .await?
            {
                if revision.render_asset_id == asset_id {
                    push_asset_reference(
                        references,
                        seen,
                        AssetReferenceParts {
                            domain: "boards",
                            owner_id: &revision.revision_id,
                            role: "render_asset",
                            hard: true,
                            parent_id: Some(&revision.board_id),
                            detail_id: None,
                        },
                    );
                }
                if revision.state_asset_id.as_deref() == Some(asset_id) {
                    push_asset_reference(
                        references,
                        seen,
                        AssetReferenceParts {
                            domain: "boards",
                            owner_id: &revision.revision_id,
                            role: "state_asset",
                            hard: true,
                            parent_id: Some(&revision.board_id),
                            detail_id: None,
                        },
                    );
                }
                if let Some(state_asset_id) = revision.state_asset_id.as_deref()
                    && let Ok((_, state_bytes)) = self.assets.read_raw(state_asset_id)
                    && let Ok(embedded_asset_ids) =
                        crate::boards::board_state_payload_asset_ids(&state_bytes)
                    && embedded_asset_ids.contains(asset_id)
                {
                    push_asset_reference(
                        references,
                        seen,
                        AssetReferenceParts {
                            domain: "boards",
                            owner_id: &revision.revision_id,
                            role: "state_embedded_asset",
                            hard: true,
                            parent_id: Some(&revision.board_id),
                            detail_id: Some(state_asset_id),
                        },
                    );
                }
            }
        }
        Ok(())
    }

    async fn collect_channel_asset_references(
        &self,
        asset_id: &str,
        references: &mut Vec<crate::AssetReferenceView>,
        seen: &mut BTreeSet<AssetReferenceKey>,
    ) -> Result<()> {
        for channel in self.channel_service.list_channels(None).await {
            for pinned_asset_id in &channel.pinned_asset_ids {
                if pinned_asset_id == asset_id {
                    push_asset_reference(
                        references,
                        seen,
                        AssetReferenceParts {
                            domain: "channels",
                            owner_id: &channel.summary.channel_id,
                            role: "pinned_asset",
                            hard: true,
                            parent_id: None,
                            detail_id: None,
                        },
                    );
                }
            }
            let messages = self
                .channel_service
                .list_messages(&channel.summary.channel_id, None)
                .await?;
            for message in messages {
                collect_rich_output_asset_references(
                    asset_id,
                    references,
                    seen,
                    "channels",
                    &message.message_id,
                    Some(&message.channel_id),
                    &message.output,
                );
            }
        }
        Ok(())
    }
}

fn asset_deletion_file_view(
    file: crate::assets::AssetDeletionFileRecord,
) -> crate::AssetDeletionFileView {
    crate::AssetDeletionFileView {
        kind: file.kind,
        uri: file.uri,
        byte_length: file.byte_length,
        exists: file.exists,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct AssetReferenceKey {
    domain: String,
    owner_id: String,
    role: String,
    hard: bool,
    parent_id: Option<String>,
    detail_id: Option<String>,
}

struct AssetReferenceParts<'a> {
    domain: &'a str,
    owner_id: &'a str,
    role: &'a str,
    hard: bool,
    parent_id: Option<&'a str>,
    detail_id: Option<&'a str>,
}

fn push_asset_reference(
    references: &mut Vec<crate::AssetReferenceView>,
    seen: &mut BTreeSet<AssetReferenceKey>,
    parts: AssetReferenceParts<'_>,
) {
    let key = AssetReferenceKey {
        domain: parts.domain.to_string(),
        owner_id: parts.owner_id.to_string(),
        role: parts.role.to_string(),
        hard: parts.hard,
        parent_id: parts.parent_id.map(ToOwned::to_owned),
        detail_id: parts.detail_id.map(ToOwned::to_owned),
    };
    if !seen.insert(key.clone()) {
        return;
    }
    references.push(crate::AssetReferenceView {
        domain: key.domain,
        owner_id: key.owner_id,
        role: key.role,
        hard: key.hard,
        parent_id: key.parent_id,
        detail_id: key.detail_id,
    });
}

fn collect_daemon_output_asset_references(
    asset_id: &str,
    references: &mut Vec<crate::AssetReferenceView>,
    seen: &mut BTreeSet<AssetReferenceKey>,
    domain: &str,
    owner_id: &str,
    parent_id: Option<&str>,
    output_index: usize,
    output: &DaemonOutputRecord,
) {
    let output_detail = format!("output:{output_index}");
    for part in &output.parts {
        if let ContentPart::Attachment { attachment } = part
            && attachment.id == asset_id
        {
            push_asset_reference(
                references,
                seen,
                AssetReferenceParts {
                    domain,
                    owner_id,
                    role: "output_part",
                    hard: true,
                    parent_id,
                    detail_id: Some(&output_detail),
                },
            );
        }
    }
    for attachment in &output.artifacts {
        if attachment.id == asset_id {
            push_asset_reference(
                references,
                seen,
                AssetReferenceParts {
                    domain,
                    owner_id,
                    role: "output_artifact",
                    hard: true,
                    parent_id,
                    detail_id: Some(&output_detail),
                },
            );
        }
    }
}

fn collect_rich_output_asset_references(
    asset_id: &str,
    references: &mut Vec<crate::AssetReferenceView>,
    seen: &mut BTreeSet<AssetReferenceKey>,
    domain: &str,
    owner_id: &str,
    parent_id: Option<&str>,
    output: &RichOutput,
) {
    for part in &output.parts {
        if let ContentPart::Attachment { attachment } = part
            && attachment.id == asset_id
        {
            push_asset_reference(
                references,
                seen,
                AssetReferenceParts {
                    domain,
                    owner_id,
                    role: "message_part",
                    hard: true,
                    parent_id,
                    detail_id: None,
                },
            );
        }
    }
    for attachment in &output.artifacts {
        if attachment.id == asset_id {
            push_asset_reference(
                references,
                seen,
                AssetReferenceParts {
                    domain,
                    owner_id,
                    role: "message_artifact",
                    hard: true,
                    parent_id,
                    detail_id: None,
                },
            );
        }
    }
}

fn collect_content_part_asset_references(
    asset_id: &str,
    references: &mut Vec<crate::AssetReferenceView>,
    seen: &mut BTreeSet<AssetReferenceKey>,
    domain: &str,
    owner_id: &str,
    parent_id: Option<&str>,
    detail_id: &str,
    parts: &[ContentPart],
) {
    for part in parts {
        if let ContentPart::Attachment { attachment } = part
            && attachment.id == asset_id
        {
            push_asset_reference(
                references,
                seen,
                AssetReferenceParts {
                    domain,
                    owner_id,
                    role: "delivery_part",
                    hard: true,
                    parent_id,
                    detail_id: Some(detail_id),
                },
            );
        }
    }
}

fn collect_output_asset_ids(outputs: &[DaemonOutputRecord], asset_ids: &mut BTreeSet<String>) {
    for output in outputs {
        for part in &output.parts {
            if let ContentPart::Attachment { attachment } = part {
                asset_ids.insert(attachment.id.clone());
            }
        }
        for attachment in &output.artifacts {
            asset_ids.insert(attachment.id.clone());
        }
    }
}

fn collect_content_part_asset_ids(parts: &[ContentPart], asset_ids: &mut BTreeSet<String>) {
    for part in parts {
        if let ContentPart::Attachment { attachment } = part {
            asset_ids.insert(attachment.id.clone());
        }
    }
}

fn collect_rich_output_asset_ids(output: &RichOutput, asset_ids: &mut BTreeSet<String>) {
    for part in &output.parts {
        if let ContentPart::Attachment { attachment } = part {
            asset_ids.insert(attachment.id.clone());
        }
    }
    for attachment in &output.artifacts {
        asset_ids.insert(attachment.id.clone());
    }
}

fn sort_asset_references(references: &mut [crate::AssetReferenceView]) {
    references.sort_by(|left, right| {
        left.domain
            .cmp(&right.domain)
            .then_with(|| left.parent_id.cmp(&right.parent_id))
            .then_with(|| left.owner_id.cmp(&right.owner_id))
            .then_with(|| left.role.cmp(&right.role))
            .then_with(|| left.detail_id.cmp(&right.detail_id))
            .then_with(|| right.hard.cmp(&left.hard))
    });
}

fn observation_retention_state_name(state: &crate::ObservationRetentionState) -> &'static str {
    match state {
        crate::ObservationRetentionState::Active => "active",
        crate::ObservationRetentionState::Purged => "purged",
    }
}
